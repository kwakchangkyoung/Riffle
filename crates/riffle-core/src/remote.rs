//! 원격(또는 임의의) 랜덤 액세스 바이트 소스 추상화.
//!
//! SFTP 등 네트워크 파일 시스템의 구현체는 riffle-remote가 제공한다.
//! 이 모듈은 Tauri·tokio를 모르는 동기 trait만 정의하며,
//! 구현체가 내부에서 비동기 I/O를 처리한다.

/// 원격(또는 임의의) 랜덤 액세스 바이트 소스.
///
/// 동기 API이며 구현체가 내부에서 비동기 처리를 한다.
/// 모든 메서드는 여러 스레드에서 동시에 호출될 수 있다 (Send + Sync).
pub trait RandomRead: Send + Sync {
    /// 현재 크기를 원격에 조회한다 (네트워크 호출 가능). refresh/tail 용.
    fn stat_len(&self) -> std::io::Result<u64>;

    /// offset부터 buf를 최대한 채워 반환한다.
    /// 짧은 읽기를 허용하며 EOF이면 0을 반환한다. 큰 buf(수 MB)도 올 수 있다.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize>;

    /// 표시·식별용 URI (예: "sftp://myhost/var/log/app.log").
    fn uri(&self) -> String;
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::RandomRead;
    use crate::search::{Matcher, SearchOptions, filter_scan, search};
    use crate::{
        Combine, DecompressProgress, FilterRule, FilterSet, FilterSpec, LogFile, RefreshOutcome,
        RuleKind, is_gzip_reader, open_gzip_remote,
    };

    /// 메모리 기반 가짜 RandomRead.
    /// data는 RwLock로 감싸 refresh 테스트에서 내용을 바꿀 수 있게 한다.
    /// chunk_limit을 두면 read_at이 요청보다 적게 반환하는 짧은 읽기를 흉내내고,
    /// delay를 두면 네트워크 지연을 흉내낸다.
    struct MemRemote {
        data: RwLock<Vec<u8>>,
        chunk_limit: usize,
        delay: Duration,
        uri: String,
    }

    impl MemRemote {
        fn new(data: &[u8]) -> Self {
            MemRemote {
                data: RwLock::new(data.to_vec()),
                chunk_limit: 0,
                delay: Duration::ZERO,
                uri: "sftp://test/var/log/app.log".to_string(),
            }
        }

        /// read_at 한 번에 chunk_limit 바이트까지만 반환하는 가짜 소스
        fn with_chunk(data: &[u8], chunk_limit: usize) -> Self {
            let mut r = Self::new(data);
            r.chunk_limit = chunk_limit;
            r
        }

        fn set_data(&self, data: &[u8]) {
            *self.data.write().unwrap() = data.to_vec();
        }
    }

    impl RandomRead for MemRemote {
        fn stat_len(&self) -> io::Result<u64> {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            Ok(self.data.read().unwrap().len() as u64)
        }

        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            let data = self.data.read().unwrap();
            let offset = offset as usize;
            if offset >= data.len() || buf.is_empty() {
                return Ok(0);
            }
            let want = buf.len().min(data.len() - offset);
            let n = if self.chunk_limit > 0 {
                want.min(self.chunk_limit)
            } else {
                want
            };
            buf[..n].copy_from_slice(&data[offset..offset + n]);
            Ok(n)
        }

        fn uri(&self) -> String {
            self.uri.clone()
        }
    }

    /// 테스트용 임시 파일 (drop 시 삭제)
    struct TmpFile {
        path: PathBuf,
    }

    impl TmpFile {
        fn new(name: &str, content: &[u8]) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "riffle_remote_{}_{}_{}.tmp",
                name,
                std::process::id(),
                id
            ));
            std::fs::write(&path, content).expect("임시 파일 쓰기 실패");
            TmpFile { path }
        }

        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// 원격 소스를 열고 인덱싱까지 완료한 LogFile을 반환한다.
    fn open_and_index(remote: Arc<MemRemote>) -> LogFile {
        let file = LogFile::open_remote(remote).expect("open_remote 실패");
        let cancel = AtomicBool::new(false);
        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");
        file
    }

    /// 검색 매치 줄 번호를 오름차순으로 수집한다.
    fn collect_search(file: &LogFile, query: &str) -> Vec<u64> {
        let matcher = Matcher::new(
            query,
            SearchOptions {
                regex: false,
                case_sensitive: true,
            },
        )
        .expect("Matcher 생성 실패");
        let cancel = AtomicBool::new(false);
        let mut hits = Vec::new();
        search(
            file,
            &matcher,
            &cancel,
            &mut |batch| hits.extend_from_slice(batch),
            &mut |_| {},
        )
        .expect("검색 실패");
        hits
    }

    /// 필터 통과 줄 번호를 오름차순으로 수집한다.
    fn collect_filter(file: &LogFile, spec: &FilterSpec) -> Vec<u64> {
        let set = FilterSet::new(spec, None).expect("FilterSet 생성 실패");
        let cancel = AtomicBool::new(false);
        let mut hits = Vec::new();
        filter_scan(
            file,
            &set,
            &cancel,
            &mut |batch| hits.extend_from_slice(batch),
            &mut |_| {},
        )
        .expect("필터 스캔 실패");
        hits
    }

    /// CRLF·잘못된 UTF-8·16KB 초과 긴 줄·개행 없는 마지막 줄이 섞인 테스트 내용
    fn mixed_content() -> Vec<u8> {
        let mut content = Vec::new();
        for i in 0..300 {
            content.extend_from_slice(format!("remote line {:04} 메시지 alpha\n", i).as_bytes());
        }
        content.extend_from_slice(b"crlf line\r\n");
        content.extend_from_slice(&[0xff, 0xfe, b'x', b'\n']);
        content.extend_from_slice(b"long line ");
        content.extend_from_slice(&vec![b'y'; 20 * 1024]);
        content.extend_from_slice(b"\nlast line no newline");
        content
    }

    #[test]
    fn test_remote_matches_local() {
        // 원격 소스로 열었을 때 줄 수·줄 내용·검색·필터 결과가
        // 같은 내용을 로컬 파일로 열었을 때와 동일해야 한다
        let content = mixed_content();
        let tmp = TmpFile::new("parity", &content);
        let local = LogFile::open(tmp.path()).expect("로컬 열기 실패");
        let cancel = AtomicBool::new(false);
        local
            .build_index(&cancel, &mut |_| {})
            .expect("로컬 인덱싱 실패");

        // 네트워크 지연을 흉내낸 원격 소스
        let mut remote = MemRemote::new(&content);
        remote.delay = Duration::from_millis(1);
        let remote_file = open_and_index(Arc::new(remote));

        assert_eq!(
            remote_file.uri().as_deref(),
            Some("sftp://test/var/log/app.log")
        );
        assert_eq!(local.uri(), None);
        assert_eq!(remote_file.index().line_count(), local.index().line_count());

        // 임의 범위의 줄 내용 비교 (앞/중간/끝)
        for (start, count) in [(0u64, 20u64), (150, 30), (290, 60)] {
            let r = remote_file
                .read_lines(start, count, None, None)
                .expect("원격 읽기 실패");
            let l = local
                .read_lines(start, count, None, None)
                .expect("로컬 읽기 실패");
            assert_eq!(r.len(), l.len(), "줄 수 불일치 ({start}+{count})");
            for (rl, ll) in r.iter().zip(l.iter()) {
                assert_eq!(rl.text, ll.text, "줄 {} 내용 불일치", rl.no);
                assert_eq!(rl.byte_len, ll.byte_len);
                assert_eq!(rl.truncated, ll.truncated);
            }
        }

        // 검색 결과 비교
        assert_eq!(
            collect_search(&remote_file, "alpha"),
            collect_search(&local, "alpha")
        );
        assert_eq!(
            collect_search(&remote_file, "crlf"),
            collect_search(&local, "crlf")
        );

        // 필터 결과 비교 (포함 + 제외 + 레벨 혼합)
        let spec = FilterSpec {
            rules: vec![
                FilterRule {
                    pattern: "remote line 0".to_string(),
                    options: SearchOptions {
                        regex: false,
                        case_sensitive: true,
                    },
                    kind: RuleKind::Include,
                    enabled: true,
                    field: None,
                },
                FilterRule {
                    pattern: "005|crlf".to_string(),
                    options: SearchOptions {
                        regex: true,
                        case_sensitive: true,
                    },
                    kind: RuleKind::Exclude,
                    enabled: true,
                    field: None,
                },
            ],
            combine: Combine::Or,
            levels: vec![],
        };
        assert_eq!(
            collect_filter(&remote_file, &spec),
            collect_filter(&local, &spec)
        );
    }

    #[test]
    fn test_remote_short_reads() {
        // read_at이 요청보다 적게 반환하는 짧은 읽기가 섞여도 결과가 동일해야 한다
        let mut content = Vec::new();
        for i in 0..2000 {
            content.extend_from_slice(format!("short line {:04} 데이터\n", i).as_bytes());
        }
        content.extend_from_slice("마지막 줄".as_bytes());

        let tmp = TmpFile::new("short", &content);
        let local = LogFile::open(tmp.path()).expect("로컬 열기 실패");
        let cancel = AtomicBool::new(false);
        local
            .build_index(&cancel, &mut |_| {})
            .expect("로컬 인덱싱 실패");

        // 1바이트씩만 반환하는 극단적인 짧은 읽기
        let remote_file = open_and_index(Arc::new(MemRemote::with_chunk(&content, 1)));
        assert_eq!(remote_file.index().line_count(), local.index().line_count());

        // 전체 줄을 읽어 로컬과 하나씩 비교
        let total = local.index().line_count();
        let r_lines = remote_file
            .read_lines(0, total, None, None)
            .expect("원격 읽기 실패");
        let l_lines = local
            .read_lines(0, total, None, None)
            .expect("로컬 읽기 실패");
        assert_eq!(r_lines.len(), l_lines.len());
        for (rl, ll) in r_lines.iter().zip(l_lines.iter()) {
            assert_eq!(rl.text, ll.text, "줄 {} 내용 불일치", rl.no);
        }

        assert_eq!(
            collect_search(&remote_file, "데이터"),
            collect_search(&local, "데이터")
        );
    }

    #[test]
    fn test_remote_refresh() {
        let remote = Arc::new(MemRemote::new(b"alpha\nbeta\n"));
        let file = open_and_index(remote.clone());
        assert_eq!(file.index().line_count(), 2);

        // 변화 없음 → Unchanged
        assert_eq!(
            file.refresh().expect("refresh 실패"),
            RefreshOutcome::Unchanged
        );

        // 크기 증가 → Appended, 추가 내용이 반영된다
        remote.set_data(b"alpha\nbeta\ngamma\n");
        match file.refresh().expect("refresh 실패") {
            RefreshOutcome::Appended {
                old_line_count,
                new_line_count,
                ..
            } => {
                assert_eq!(old_line_count, 2);
                assert_eq!(new_line_count, 3);
            }
            other => panic!("Appended 기대, got {:?}", other),
        }
        let lines = file.read_lines(2, 1, None, None).expect("읽기 실패");
        assert_eq!(lines[0].text, "gamma");

        // 크기 감소 → Truncated (인덱스는 건드리지 않는다)
        remote.set_data(b"alpha\n");
        assert_eq!(
            file.refresh().expect("refresh 실패"),
            RefreshOutcome::Truncated
        );

        // 같은 크기로 돌아와도 inode가 없어 Replaced 판정 없이 Unchanged
        remote.set_data(b"alpha\nbeta\ngamma\n");
        assert_eq!(
            file.refresh().expect("refresh 실패"),
            RefreshOutcome::Unchanged
        );
    }

    /// 내용을 gzip 압축한 바이트를 반환한다.
    fn gzip_bytes(content: &[u8]) -> Vec<u8> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(content).expect("gzip 압축 실패");
        enc.finish().expect("gzip 종료 실패")
    }

    #[test]
    fn test_remote_gzip() {
        let mut content = Vec::new();
        for i in 0..500 {
            content.extend_from_slice(format!("gz remote line {:04}\n", i).as_bytes());
        }
        content.extend_from_slice("gz 마지막 줄 개행 없음".as_bytes());
        let gz = gzip_bytes(&content);

        // is_gzip_reader: 일반 데이터는 false, gzip은 true (1바이트 짧은 읽기에서도)
        let plain = Arc::new(MemRemote::new(b"not a gzip file"));
        assert!(!is_gzip_reader(plain.as_ref()).expect("판별 실패"));
        let gz_probe = Arc::new(MemRemote::with_chunk(&gz, 1));
        assert!(is_gzip_reader(gz_probe.as_ref()).expect("판별 실패"));

        // 원격 gzip을 순차 해제해 줄 수·내용이 원본과 일치해야 한다
        let (file, writer) = open_gzip_remote(gz_probe).expect("open_gzip_remote 실패");
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
        assert_eq!(last.compressed_total as usize, gz.len());
        assert_eq!(last.compressed_read as usize, gz.len());
        assert_eq!(last.decompressed as usize, content.len());
        assert_eq!(file.index().line_count(), 501);

        let lines = file.read_lines(0, 3, None, None).expect("읽기 실패");
        assert_eq!(lines[0].text, "gz remote line 0000");
        assert_eq!(lines[2].text, "gz remote line 0002");
        let tail = file.read_lines(500, 1, None, None).expect("읽기 실패");
        assert_eq!(tail[0].text, "gz 마지막 줄 개행 없음");
    }
}
