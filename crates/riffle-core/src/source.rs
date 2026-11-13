use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::Result;
use crate::remote::RandomRead;

/// 개행 탐색 스캔 시 사용하는 읽기 청크 크기 (64KB)
const SCAN_CHUNK_SIZE: usize = 64 * 1024;

/// refresh()의 길이/교체 판정에 사용하는 소스 종류
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// 경로가 있는 로컬 파일: inode/dev 교체·삭제를 추적한다
    LocalPath,
    /// 경로 없는 로컬 파일(gzip 해제 임시 파일): 길이 증가만 반영한다
    LocalAnonymous,
    /// 원격 RandomRead 소스: inode가 없어 길이 감소만 Truncated로 판정한다
    Remote,
}

/// 로컬 파일 디스크립터 기반 읽기 소스의 내부 상태
enum SourceInner {
    /// 로컬 파일 (경로 있음/없음)
    Local(LocalSource),
    /// 원격(또는 임의의) 랜덤 액세스 소스
    Remote(Arc<dyn RandomRead>),
}

/// 로컬 파일 소스의 내부 상태.
/// mmap을 사용하지 않고 pread(read_at)로만 읽으므로,
/// 외부에서 파일이 잘려도(truncate/로테이션) SIGBUS 없이 짧은 읽기로 안전하게 처리된다.
struct LocalSource {
    file: File,
    ino: u64,
    dev: u64,
    /// 경로 기반 소스만 Some. None이면 익명 소스(경로 없음)로
    /// refresh()의 Replaced/Truncated 판정을 건너뛴다.
    path: Option<PathBuf>,
}

/// 파일 디스크립터 또는 원격 RandomRead 기반 파일 읽기 소스.
/// mmap을 사용하지 않고 read_at으로만 읽으므로,
/// 외부에서 파일이 잘려도(truncate/로테이션) SIGBUS 없이 짧은 읽기로 안전하게 처리된다.
/// `len`은 "현재까지 확인된" 파일 길이로, tail 모드의 refresh()에서만 갱신된다.
pub struct Source {
    inner: SourceInner,
    len: AtomicU64,
}

impl Source {
    /// 지정된 경로의 파일을 열고 inode/dev 및 초기 길이를 기록한다.
    pub fn open(path: &Path) -> Result<Source> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        Ok(Source {
            inner: SourceInner::Local(LocalSource {
                file,
                ino: metadata.ino(),
                dev: metadata.dev(),
                path: Some(path.to_path_buf()),
            }),
            len: AtomicU64::new(metadata.len()),
        })
    }

    /// 이미 열린 파일 핸들로부터 익명(경로 없음) 소스를 만든다.
    /// gzip 해제 임시 파일처럼 경로로 추적할 수 없는 파일에 사용한다.
    pub fn from_file(file: File) -> Result<Source> {
        let metadata = file.metadata()?;
        Ok(Source {
            inner: SourceInner::Local(LocalSource {
                file,
                ino: metadata.ino(),
                dev: metadata.dev(),
                path: None,
            }),
            len: AtomicU64::new(metadata.len()),
        })
    }

    /// 원격(또는 임의의) 랜덤 액세스 소스를 만든다.
    /// 초기 길이는 stat_len()으로 조회한다 (네트워크 호출 가능).
    /// 이후 refresh()는 stat_len 증가분만 Appended로 반영하고,
    /// inode가 없어 Replaced 판정은 하지 않는다 (길이 감소는 Truncated).
    pub fn from_remote(r: Arc<dyn RandomRead>) -> Result<Source> {
        let len = r.stat_len()?;
        Ok(Source {
            inner: SourceInner::Remote(r),
            len: AtomicU64::new(len),
        })
    }

    /// refresh() 판정에 사용하는 소스 종류를 반환한다.
    pub(crate) fn kind(&self) -> SourceKind {
        match &self.inner {
            SourceInner::Local(local) if local.path.is_some() => SourceKind::LocalPath,
            SourceInner::Local(_) => SourceKind::LocalAnonymous,
            SourceInner::Remote(_) => SourceKind::Remote,
        }
    }

    /// 원격 소스의 URI를 반환한다. 로컬 소스는 None.
    pub(crate) fn uri(&self) -> Option<String> {
        match &self.inner {
            SourceInner::Remote(r) => Some(r.uri()),
            SourceInner::Local(_) => None,
        }
    }

    /// 현재까지 확인된 파일 길이(바이트)를 반환한다.
    #[inline]
    pub fn len(&self) -> u64 {
        self.len.load(Ordering::Acquire)
    }

    /// 파일이 비어있는지 여부를 반환한다.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 확인된 길이를 갱신한다 (내부용, refresh 경로에서만 호출).
    pub(crate) fn set_len(&self, new_len: u64) {
        self.len.store(new_len, Ordering::Release);
    }

    /// [start, min(end, len)) 구간을 읽어 반환한다.
    /// 파일이 외부에서 잘려 실제로는 더 적게 읽히면 읽힌 만큼만 반환한다.
    pub fn read_range(&self, start: u64, end: u64) -> Result<Vec<u8>> {
        let clamped_end = end.min(self.len());
        if start >= clamped_end {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; (clamped_end - start) as usize];
        let n = self.read_into(&mut buf, start)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// buf를 offset부터 최대한 채운다. offset+buf.len()이 확인된 길이를 넘으면
    /// 길이까지만 읽는다. 파일이 외부에서 잘려 있으면 EOF에서 멈추고 읽은 바이트 수를 반환한다.
    pub fn read_into(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let len = self.len();
        if offset >= len || buf.is_empty() {
            return Ok(0);
        }
        let max = (len - offset).min(buf.len() as u64) as usize;
        self.read_at_raw(&mut buf[..max], offset)
    }

    /// 확인된 길이(len) 클램프 없이 소스에서 직접 읽는다.
    /// LineIndex::extend가 아직 인덱스에 반영되지 않은 추가분을 읽을 때 사용한다.
    /// EOF(외부 truncate 포함)에서는 요청보다 적게 읽힌다.
    /// 원격 소스의 짧은 읽기는 buf가 찰 때까지 반복 호출해 메운다.
    pub(crate) fn read_at_raw(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let mut read = 0;
        while read < buf.len() {
            let n = match &self.inner {
                SourceInner::Local(local) => {
                    local.file.read_at(&mut buf[read..], offset + read as u64)?
                }
                SourceInner::Remote(remote) => {
                    remote.read_at(&mut buf[read..], offset + read as u64)?
                }
            };
            if n == 0 {
                break;
            }
            read += n;
        }
        Ok(read)
    }

    /// 소스의 현재 크기를 반환한다 (로컬은 fstat, 원격은 stat_len 네트워크 호출).
    /// append를 즉시 반영한다.
    pub(crate) fn file_len(&self) -> Result<u64> {
        match &self.inner {
            SourceInner::Local(local) => Ok(local.file.metadata()?.len()),
            SourceInner::Remote(remote) => Ok(remote.stat_len()?),
        }
    }

    /// 경로가 현재 열린 파일과 같은 inode/dev를 가리키는지 확인한다.
    /// 경로의 파일이 없거나(삭제) 다른 파일로 교체됐으면(rename 로테이션) false.
    /// 익명 소스(경로 없음)와 원격 소스는 비교할 경로가 없으므로 항상 true를 반환한다.
    pub(crate) fn path_matches(&self) -> Result<bool> {
        let SourceInner::Local(local) = &self.inner else {
            return Ok(true);
        };
        let Some(path) = local.path.as_ref() else {
            return Ok(true);
        };
        match std::fs::metadata(path) {
            Ok(meta) => Ok(meta.ino() == local.ino && meta.dev() == local.dev),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// start 이상에서 n번째(0-based) '\n'의 절대 오프셋을 반환한다.
    /// 확인된 길이까지 64KB 청크로 읽으며, I/O 오류 시 Err, 끝까지 없으면 Ok(None).
    /// 줄 시작 탐색처럼 개행을 찾아야 하는 곳에서 사용한다.
    pub(crate) fn nth_newline(&self, start: u64, n: usize) -> Result<Option<u64>> {
        let end = self.len();
        if start >= end {
            return Ok(None);
        }
        let mut buf = [0u8; SCAN_CHUNK_SIZE];
        let mut offset = start;
        let mut seen = 0usize;
        while offset < end {
            let want = (end - offset).min(SCAN_CHUNK_SIZE as u64) as usize;
            let nread = self.read_into(&mut buf[..want], offset)?;
            if nread == 0 {
                break;
            }
            for pos in memchr::memchr_iter(b'\n', &buf[..nread]) {
                if seen == n {
                    return Ok(Some(offset + pos as u64));
                }
                seen += 1;
            }
            offset += nread as u64;
        }
        Ok(None)
    }

    /// [start, end) 구간(확인된 길이로 클램프) 안의 '\n' 개수를 반환한다.
    /// 64KB 청크로 읽으며 I/O 오류 시 Err.
    pub(crate) fn count_newlines(&self, start: u64, end: u64) -> Result<u64> {
        let end = end.min(self.len());
        if start >= end {
            return Ok(0);
        }
        let mut buf = [0u8; SCAN_CHUNK_SIZE];
        let mut offset = start;
        let mut count = 0u64;
        while offset < end {
            let want = (end - offset).min(SCAN_CHUNK_SIZE as u64) as usize;
            let nread = self.read_into(&mut buf[..want], offset)?;
            if nread == 0 {
                break;
            }
            count += memchr::memchr_iter(b'\n', &buf[..nread]).count() as u64;
            offset += nread as u64;
        }
        Ok(count)
    }
}
