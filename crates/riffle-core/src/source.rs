use std::fs::File;
use std::path::Path;

use crate::Result;

/// 메모리 맵 및 파일 디스크립터 기반 파일 읽기 소스 추상화.
/// 0바이트 파일은 mmap을 생성하지 않고 빈 슬라이스를 반환한다.
pub struct Source {
    mmap: Option<memmap2::Mmap>,
    file: Option<File>,
    len: u64,
}

impl Source {
    /// 지정된 경로의 파일을 열고 메모리 맵과 파일 핸들을 생성한다.
    /// 파일 크기가 0인 경우 mmap을 생성하지 않는다.
    pub fn open(path: &Path) -> Result<Source> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        let len = metadata.len();

        if len == 0 {
            Ok(Source {
                mmap: None,
                file: None,
                len: 0,
            })
        } else {
            // SAFETY: 파일을 읽기 전용으로 매핑하며, 프로세스 수명 동안 불변으로 가정한다.
            // 외부 프로세스에 의한 truncate 시 SIGBUS 위험은 Phase 4(tail 모드)에서 read_at으로 대응한다.
            let mmap = unsafe { memmap2::Mmap::map(&file)? };
            Ok(Source {
                mmap: Some(mmap),
                file: Some(file),
                len,
            })
        }
    }

    /// 파일의 전체 바이트 크기를 반환한다.
    #[inline]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// 파일이 비어있는지 여부를 반환한다.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// start부터 end까지의 바이트 슬라이스를 반환한다.
    /// end는 파일 길이(len)로 자동 clamp된다.
    /// start >= len 이거나 start >= end인 경우 빈 슬라이스를 반환한다.
    pub fn bytes(&self, start: u64, end: u64) -> &[u8] {
        if start >= self.len {
            return &[];
        }

        let clamped_end = end.min(self.len);
        if start >= clamped_end {
            return &[];
        }

        match &self.mmap {
            Some(mmap) => &mmap[start as usize..clamped_end as usize],
            None => &[],
        }
    }

    /// 파일 핸들에 대한 참조를 반환한다 (내부용).
    #[allow(dead_code)]
    pub(crate) fn file(&self) -> Option<&File> {
        self.file.as_ref()
    }

    /// 지정된 오프셋에서 버퍼 크기만큼 원시 데이터를 읽어온다.
    /// Unix에서는 pread(read_at)를 사용하여 mmap 페이지 폴트 없이 고속 순차 읽기를 수행한다.
    pub(crate) fn read_at_or_mmap(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            if let Some(file) = &self.file {
                let mut total_read = 0;
                let mut slice = buf;
                let mut cur_offset = offset;
                while !slice.is_empty() {
                    let n = file.read_at(slice, cur_offset)?;
                    if n == 0 {
                        break;
                    }
                    total_read += n;
                    cur_offset += n as u64;
                    slice = &mut slice[n..];
                }
                return Ok(total_read);
            }
        }
        let slice = self.bytes(offset, offset + buf.len() as u64);
        buf[..slice.len()].copy_from_slice(slice);
        Ok(slice.len())
    }
}
