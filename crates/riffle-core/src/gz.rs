// .gz (gzip) 압축 파일 지원.
//
// gzip은 랜덤 접근이 불가하므로 백그라운드에서 임시 파일로 스트리밍 해제하고,
// 그 임시 파일을 "점점 커지는 파일"로 열어 Phase 4의 refresh()/extend로
// 인덱스를 늘린다. 임시 파일은 생성 직후 unlink하고 열린 핸들로만 읽고 쓰므로
// 앱이 크래시해도 디스크에 파일이 남지 않는다.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flate2::bufread::MultiGzDecoder;

use crate::error::{Error, Result};
use crate::lines::LogFile;
use crate::source::Source;

/// 해제 입/출력 버퍼 크기 (1MB)
const IO_BUF_SIZE: usize = 1024 * 1024;
/// 진행률 통지 최소 간격 (바이트)
const PROGRESS_BYTES: u64 = 16 * 1024 * 1024;
/// 진행률 통지 최소 간격 (시간)
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// gzip 해제 임시 파일 전용 디렉터리 이름 (temp_dir() 아래).
/// WKWebView 등이 temp_dir()/riffle/을 쓰므로 충돌을 피해 분리한다.
const TEMP_DIR_NAME: &str = "riffle-gz";

/// gzip 해제 진행 상태
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecompressProgress {
    /// 원본 .gz에서 읽은 바이트 수
    pub compressed_read: u64,
    /// 원본 .gz 크기
    pub compressed_total: u64,
    /// 임시 파일에 기록한(=읽기 가능한) 바이트 수
    pub decompressed: u64,
    /// 현재까지 확인된 줄 수 (LogFile 인덱스 기준)
    pub line_count: u64,
    /// 해제와 인덱싱이 모두 완료되었는지
    pub done: bool,
}

/// 파일 앞 2바이트가 gzip 매직(1f 8b)인지 확인한다 (확장자 무관).
pub fn is_gzip(path: &Path) -> Result<bool> {
    let mut file = File::open(path)?;
    let mut magic = [0u8; 2];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == [0x1f, 0x8b]),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// 원본 .gz 파일에서 실제로 읽어들인 바이트 수를 세는 래퍼.
/// 진행률의 compressed_read를 구하기 위해 디코더 입력 측에 둔다.
struct ByteCounter {
    inner: File,
    count: Arc<AtomicU64>,
}

impl Read for ByteCounter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.count.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// gzip 해제 작업 핸들.
/// 원본 .gz의 MultiGzDecoder(multi-member 지원)와 임시 파일 쓰기 핸들을 소유한다.
pub struct GzWriter {
    decoder: MultiGzDecoder<BufReader<ByteCounter>>,
    out: BufWriter<File>,
    /// 원본 .gz 크기 (진행률 분모)
    compressed_total: u64,
    /// 원본 .gz에서 읽은 바이트 수 (ByteCounter와 공유)
    compressed_read: Arc<AtomicU64>,
    /// 임시 파일에 기록한 바이트 수
    decompressed: u64,
    /// 임시 파일 경로. 생성 직후 unlink되므로 존재하지 않으며 테스트용이다.
    #[cfg(test)]
    temp_path: PathBuf,
}

/// .gz 파일을 열어 해제 대상 LogFile과 GzWriter를 만든다.
///
/// 임시 파일을 temp_dir()/riffle-gz/ 아래에 만들고 즉시 unlink한다.
/// 반환된 LogFile은 길이 0, 인덱스 완료 상태로 시작하며 이후
/// GzWriter::run 안의 refresh() 호출로 커진다.
/// `display_path` 개념은 호출 측(IPC)이 원본 경로를 그대로 쓰므로 여기서는 다루지 않는다.
pub fn open_gzip(path: &Path) -> Result<(LogFile, GzWriter)> {
    let src = File::open(path)?;
    let compressed_total = src.metadata()?.len();

    let dir = std::env::temp_dir().join(TEMP_DIR_NAME);
    std::fs::create_dir_all(&dir)?;

    // 고유 이름의 임시 파일을 만들고 즉시 unlink 한다.
    // 읽기/쓰기 핸들은 unlink 전에 열어 두므로 이후에도 유효하다.
    let temp_path = unique_temp_path(&dir);
    let write_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)?;
    let read_file = match File::open(&temp_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e.into());
        }
    };
    std::fs::remove_file(&temp_path)?;

    let log_file = LogFile::from_source(Source::from_file(read_file)?);

    let compressed_read = Arc::new(AtomicU64::new(0));
    let counted = ByteCounter {
        inner: src,
        count: Arc::clone(&compressed_read),
    };
    let decoder = MultiGzDecoder::new(BufReader::with_capacity(IO_BUF_SIZE, counted));
    let out = BufWriter::with_capacity(IO_BUF_SIZE, write_file);

    Ok((
        log_file,
        GzWriter {
            decoder,
            out,
            compressed_total,
            compressed_read,
            decompressed: 0,
            #[cfg(test)]
            temp_path,
        },
    ))
}

/// temp_dir()/riffle-gz/ 안에 남은 임시 파일을 삭제한다. 오류는 무시한다.
/// 정상 경로에서는 임시 파일을 생성 직후 unlink하므로 여기 남는 파일은 드물다
/// (unlink 실패, create와 unlink 사이의 크래시 등).
/// 우리 패턴(riffle_*.tmp)에 맞는 일반 파일만 삭제해 다른 구성요소의 파일을 건드리지 않는다.
pub fn cleanup_temp_dir() {
    let dir = std::env::temp_dir().join(TEMP_DIR_NAME);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let is_ours = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("riffle_") && name.ends_with(".tmp"));
        if is_ours {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// 임시 파일용 고유 경로를 만든다. create_new가 충돌을 막으므로
/// 프로세스 내 카운터만으로 충분하다.
fn unique_temp_path(dir: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!("riffle_{}_{}.tmp", std::process::id(), id))
}

/// gzip 디코더 오류를 코어 에러로 변환한다.
/// 손상/잘림 등 형식 오류는 Gzip으로, 그 외(디스크 읽기 오류 등)는 Io로 분류한다.
fn decode_error(e: io::Error) -> Error {
    match e.kind() {
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => Error::Gzip(e.to_string()),
        _ => Error::Io(e),
    }
}

impl GzWriter {
    /// 블로킹. gzip을 해제하며 임시 파일에 쓰고, 일정 간격(16MB 또는 100ms)마다
    /// flush 후 file.refresh()로 인덱스를 늘리고 on_progress를 호출한다.
    /// 끝나면 done=true로 한 번 더 호출한다.
    ///
    /// cancel이 true가 되면 Ok(())로 즉시 중단한다 (이미 해제된 부분은 유효).
    /// 손상된 gzip이나 쓰기 오류(디스크 부족 등)는 Err를 반환하며,
    /// 그 전까지 해제·인덱싱된 부분은 그대로 읽을 수 있다.
    pub fn run(
        mut self,
        file: &LogFile,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(DecompressProgress),
    ) -> Result<()> {
        on_progress(self.progress(file, false));

        let mut buf = vec![0u8; IO_BUF_SIZE];
        let mut err: Option<Error> = None;
        let mut since_report = 0u64;
        let mut last_report = Instant::now();

        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            match self.decoder.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = self.out.write_all(&buf[..n]) {
                        err = Some(Error::Io(e));
                        break;
                    }
                    self.decompressed += n as u64;
                    since_report += n as u64;

                    if since_report >= PROGRESS_BYTES || last_report.elapsed() >= PROGRESS_INTERVAL
                    {
                        match self.report(file, on_progress) {
                            Ok(()) => {
                                since_report = 0;
                                last_report = Instant::now();
                            }
                            Err(e) => {
                                err = Some(e);
                                break;
                            }
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    err = Some(decode_error(e));
                    break;
                }
            }
        }

        // 남은 데이터를 디스크에 반영하고 마지막까지 인덱싱한다.
        // 이미 오류가 있으면 첫 오류를 유지한다.
        if let Err(e) = self.out.flush()
            && err.is_none()
        {
            err = Some(Error::Io(e));
        }
        if let Err(e) = file.refresh()
            && err.is_none()
        {
            err = Some(e);
        }

        let done = err.is_none() && !cancel.load(Ordering::Relaxed);
        on_progress(self.progress(file, done));

        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 버퍼를 디스크에 반영하고 인덱스를 늘린 뒤 진행률을 한 번 통지한다.
    fn report(
        &mut self,
        file: &LogFile,
        on_progress: &mut dyn FnMut(DecompressProgress),
    ) -> Result<()> {
        self.out.flush()?;
        file.refresh()?;
        on_progress(self.progress(file, false));
        Ok(())
    }

    /// 현재 상태로 진행률 정보를 만든다.
    fn progress(&self, file: &LogFile, done: bool) -> DecompressProgress {
        DecompressProgress {
            compressed_read: self.compressed_read.load(Ordering::Relaxed),
            compressed_total: self.compressed_total,
            decompressed: self.decompressed,
            line_count: file.index().line_count(),
            done,
        }
    }

    /// 임시 파일 경로를 반환한다 (테스트/디버깅용).
    /// 생성 직후 unlink되므로 이 경로는 존재하지 않아야 한다.
    #[cfg(test)]
    fn temp_path(&self) -> &Path {
        &self.temp_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RefreshOutcome;
    use flate2::Compression;
    use flate2::write::GzEncoder;

    /// 테스트용 임시 파일 (drop 시 삭제). 기존 tests.rs의 TempTestFile과 같은 카운터 방식.
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(prefix: &str, content: &[u8]) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "riffle_gz_{}_{}_{}.tmp",
                prefix,
                std::process::id(),
                id
            ));
            std::fs::write(&path, content).expect("임시 파일 생성 실패");
            TempFile { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// 내용을 gzip 압축한 바이트를 반환한다.
    fn gzip_bytes(content: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(content).expect("gzip 압축 실패");
        enc.finish().expect("gzip 종료 실패")
    }

    /// 주어진 바이트를 .gz 임시 파일로 만들고 (LogFile, GzWriter)를 반환한다.
    /// writer는 호출자가 run()으로 소비해야 한다.
    fn open_gz_bytes(gz: &[u8]) -> (TempFile, LogFile, GzWriter) {
        let tmp = TempFile::new("open", gz);
        let (file, writer) = open_gzip(tmp.path()).expect("open_gzip 실패");
        (tmp, file, writer)
    }

    #[test]
    fn test_is_gzip() {
        // gzip 파일
        let gz = TempFile::new("is_gz", &gzip_bytes(b"hello\n"));
        assert!(is_gzip(gz.path()).unwrap());

        // 일반 파일
        let plain = TempFile::new("is_plain", b"hello\n");
        assert!(!is_gzip(plain.path()).unwrap());

        // 빈 파일
        let empty = TempFile::new("is_empty", b"");
        assert!(!is_gzip(empty.path()).unwrap());

        // 1바이트 파일
        let one = TempFile::new("is_one", &[0x1f]);
        assert!(!is_gzip(one.path()).unwrap());

        // 매직만 있고 나머지가 없는 2바이트 파일
        let magic_only = TempFile::new("is_magic", &[0x1f, 0x8b]);
        assert!(is_gzip(magic_only.path()).unwrap());
    }

    #[test]
    fn test_open_gzip_roundtrip_lines() {
        // CRLF와 개행 없는 마지막 줄이 섞인 내용
        let mut content = Vec::new();
        for i in 0..500 {
            content.extend_from_slice(format!("log line {:04} 메시지\r\n", i).as_bytes());
        }
        content.extend_from_slice("마지막 줄 개행 없음".as_bytes());

        let gz = gzip_bytes(&content);
        let (_tmp, file, writer) = open_gz_bytes(&gz);

        // 임시 파일은 생성 직후 unlink되어 경로에 존재하지 않아야 한다
        assert!(!writer.temp_path().exists());
        let listed = std::fs::read_dir(std::env::temp_dir().join(TEMP_DIR_NAME))
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .any(|e| e.path() == writer.temp_path())
            })
            .unwrap_or(false);
        assert!(!listed, "임시 파일이 디렉터리 목록에 남아 있음");

        let cancel = AtomicBool::new(false);
        let mut last = DecompressProgress {
            compressed_read: 0,
            compressed_total: 0,
            decompressed: 0,
            line_count: 0,
            done: false,
        };
        writer
            .run(&file, &cancel, &mut |p| last = p)
            .expect("해제 실패");

        assert!(last.done);
        assert_eq!(last.decompressed as usize, content.len());
        assert_eq!(last.compressed_total as usize, gz.len());
        assert_eq!(last.compressed_read as usize, gz.len());
        assert_eq!(file.index().line_count(), 501);

        // 임의 줄 내용이 원본과 일치하는지 확인
        let lines = file.read_lines(0, 10, None).expect("읽기 실패");
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0].text, "log line 0000 메시지");
        assert_eq!(lines[9].text, "log line 0009 메시지");
        let last_lines = file.read_lines(500, 1, None).expect("읽기 실패");
        assert_eq!(last_lines.len(), 1);
        assert_eq!(last_lines[0].text, "마지막 줄 개행 없음");
    }

    #[test]
    fn test_open_gzip_multi_member() {
        // 두 개의 gzip 스트림을 이어붙인다 (logrotate/cat a.gz b.gz 형태)
        let part1 = b"first part line1\nfirst part line2\n".to_vec();
        let part2 = b"second part line1\nsecond part line2\n".to_vec();
        let mut combined = gzip_bytes(&part1);
        combined.extend_from_slice(&gzip_bytes(&part2));

        let (_tmp, file, writer) = open_gz_bytes(&combined);
        let cancel = AtomicBool::new(false);
        writer
            .run(&file, &cancel, &mut |_| {})
            .expect("multi-member 해제 실패");

        assert_eq!(file.index().line_count(), 4);
        let lines = file.read_lines(0, 10, None).expect("읽기 실패");
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].text, "first part line1");
        assert_eq!(lines[1].text, "first part line2");
        assert_eq!(lines[2].text, "second part line1");
        assert_eq!(lines[3].text, "second part line2");
    }

    #[test]
    fn test_read_lines_during_decompression() {
        // 내용이 16MB를 넘도록 크게 만든다 (중간 진행 이벤트가 반드시 발생하도록)
        let mut content = Vec::new();
        for i in 0..1_000_000 {
            content.extend_from_slice(format!("decompress visible line {:06}\n", i).as_bytes());
        }
        let gz = gzip_bytes(&content);
        let (_tmp, file, writer) = open_gz_bytes(&gz);

        let cancel = AtomicBool::new(false);
        let mut saw_lines = false;
        writer
            .run(&file, &cancel, &mut |p| {
                // 진행률 콜백 안에서 이미 해제된 부분을 읽을 수 있어야 한다
                if p.decompressed > 0 && !saw_lines {
                    let lines = file.read_lines(0, 5, None).expect("읽기 실패");
                    if !lines.is_empty() {
                        assert_eq!(lines[0].text, "decompress visible line 000000");
                        saw_lines = true;
                    }
                }
            })
            .expect("해제 실패");

        assert!(saw_lines, "해제 중간에 줄을 읽지 못함");
        assert_eq!(file.index().line_count(), 1_000_000);
    }

    #[test]
    fn test_cancel_keeps_partial_content() {
        // 반복 데이터라 압축률이 높아 해제 도중 취소해도 충분한 양이 남는다
        let mut content = Vec::new();
        for _ in 0..1_000_000 {
            content.extend_from_slice(b"cancel test line with repetitive payload\n");
        }
        let gz = gzip_bytes(&content);
        let (_tmp, file, writer) = open_gz_bytes(&gz);

        let cancel = AtomicBool::new(false);
        let mut last = DecompressProgress {
            compressed_read: 0,
            compressed_total: 0,
            decompressed: 0,
            line_count: 0,
            done: false,
        };
        writer
            .run(&file, &cancel, &mut |p| {
                last = p;
                // 첫 중간 진행 이벤트(16MB 도달)에서 취소
                if p.decompressed >= 16 * 1024 * 1024 {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
            .expect("취소는 Ok를 반환해야 함");

        assert!(!last.done);
        assert!(last.decompressed < content.len() as u64);
        assert!(last.decompressed > 0);
        assert!(file.index().line_count() > 0);

        // 해제된 부분은 읽을 수 있다
        let lines = file.read_lines(0, 5, None).expect("읽기 실패");
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].text, "cancel test line with repetitive payload");
    }

    #[test]
    fn test_corrupt_gzip_error_keeps_partial() {
        let mut content = Vec::new();
        for i in 0..50_000 {
            content.extend_from_slice(format!("corrupt test line {:05}\n", i).as_bytes());
        }
        let mut gz = gzip_bytes(&content);

        // 중간 바이트를 변조한다 (압축 데이터 영역)
        let mid = gz.len() / 2;
        gz[mid] ^= 0xFF;

        let (_tmp, file, writer) = open_gz_bytes(&gz);
        let cancel = AtomicBool::new(false);
        let result = writer.run(&file, &cancel, &mut |_| {});

        assert!(result.is_err(), "손상된 gzip은 Err를 반환해야 함");

        // 오류 전까지 해제·인덱싱된 부분은 유효하다
        let lines = file.read_lines(0, 5, None).expect("읽기 실패");
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].text, "corrupt test line 00000");
    }

    #[test]
    fn test_truncated_gzip_error() {
        let mut content = Vec::new();
        for i in 0..10_000 {
            content.extend_from_slice(format!("truncate test line {:05}\n", i).as_bytes());
        }
        let gz = gzip_bytes(&content);
        let truncated = &gz[..gz.len() * 3 / 5];

        let (_tmp, file, writer) = open_gz_bytes(truncated);
        let cancel = AtomicBool::new(false);
        let result = writer.run(&file, &cancel, &mut |_| {});

        assert!(result.is_err(), "잘린 gzip은 Err를 반환해야 함");
        // 그 전까지의 내용은 읽을 수 있다
        let _ = file.read_lines(0, 5, None).expect("읽기 실패");
    }

    #[test]
    fn test_anonymous_source_refresh_no_replaced_truncated() {
        // 익명 소스는 경로가 없으므로 Replaced/Truncated 판정을 하지 않는다
        let dir = std::env::temp_dir();
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("riffle_anon_{}_{}.tmp", std::process::id(), id));
        // 빈 파일로 시작해야 인덱스가 완료 상태(done)로 시작한다
        std::fs::write(&path, b"").expect("쓰기 실패");

        let read_file = File::open(&path).expect("열기 실패");
        let source = Source::from_file(read_file).expect("from_file 실패");
        let file = LogFile::from_source(source);

        // 외부에서 파일을 추가하고 refresh → Appended (Replaced/Truncated 아님)
        {
            let mut f = OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("append 실패");
            f.write_all(b"line0\nline1\nline2\n").expect("쓰기 실패");
        }
        let outcome = file.refresh().expect("refresh 실패");
        assert!(matches!(outcome, RefreshOutcome::Appended { .. }));
        assert_eq!(file.index().line_count(), 3);

        // 파일을 줄여도(truncate) 익명 소스는 Truncated를 반환하지 않는다
        {
            let f = OpenOptions::new()
                .write(true)
                .open(&path)
                .expect("truncate 실패");
            f.set_len(6).expect("set_len 실패"); // "line0\n" 만 남김
        }
        let outcome = file.refresh().expect("refresh 실패");
        assert_ne!(outcome, RefreshOutcome::Truncated);
        assert_eq!(outcome, RefreshOutcome::Unchanged);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_cleanup_temp_dir_only_removes_our_files() {
        // riffle-gz 디렉터리에 우리 것/아닌 것을 섞어 놓고 cleanup을 실행한다
        let dir = std::env::temp_dir().join(TEMP_DIR_NAME);
        std::fs::create_dir_all(&dir).expect("디렉터리 생성 실패");

        let ours = dir.join("riffle_cleanup_del.tmp");
        let other_ext = dir.join("riffle_cleanup_keep.txt");
        let other_prefix = dir.join("other_cleanup.tmp");
        let subdir = dir.join("riffle_cleanup_dir.tmp");

        std::fs::write(&ours, b"x").expect("쓰기 실패");
        std::fs::write(&other_ext, b"x").expect("쓰기 실패");
        std::fs::write(&other_prefix, b"x").expect("쓰기 실패");
        std::fs::create_dir(&subdir).expect("디렉터리 생성 실패");

        cleanup_temp_dir();

        // riffle_*.tmp 일반 파일만 삭제되고 나머지는 남아 있어야 한다
        assert!(!ours.exists(), "우리 임시 파일이 삭제되지 않음");
        assert!(other_ext.exists(), "패턴 불일치 파일이 삭제됨");
        assert!(other_prefix.exists(), "접두사 불일치 파일이 삭제됨");
        assert!(subdir.is_dir(), "디렉터리가 삭제됨");

        let _ = std::fs::remove_file(&other_ext);
        let _ = std::fs::remove_file(&other_prefix);
        let _ = std::fs::remove_dir(&subdir);
    }
}
