// 파일 머리 지문: 파일 앞 64KB의 FNV-1a 64비트 해시.
// 북마크처럼 파일 내용과 함께 보관해야 하는 식별자에 사용한다.
// 로컬/원격/gz 해제본 모두 Source를 통해 읽으므로,
// 같은 내용을 어떤 방식으로 열어도 같은 값을 반환한다.

use crate::Result;
use crate::lines::LogFile;

/// 머리 지문에 사용하는 파일 앞부분 크기 (64KB)
pub const HEAD_FINGERPRINT_BYTES: u64 = 64 * 1024;

/// FNV-1a 64비트 상수
const FNV1A_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV1A_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 파일 앞 HEAD_FINGERPRINT_BYTES 바이트(파일이 더 작으면 전체)의
/// FNV-1a 64비트 해시를 반환한다. 빈 파일은 오프셋 기본값을 반환한다.
/// 원격 소스는 앞부분 읽기에 네트워크 I/O가 발생할 수 있다 (블로킹).
pub fn head_fingerprint(file: &LogFile) -> Result<u64> {
    let source = file.source();
    let end = source.len().min(HEAD_FINGERPRINT_BYTES);
    let mut hash = FNV1A_OFFSET;
    let mut buf = [0u8; 8192];
    let mut offset = 0u64;
    while offset < end {
        let want = ((end - offset) as usize).min(buf.len());
        let n = source.read_into(&mut buf[..want], offset)?;
        if n == 0 {
            break; // 확인된 길이 이전에 EOF에 도달 (외부 truncate 등)
        }
        for &b in &buf[..n] {
            hash ^= b as u64;
            hash = hash.wrapping_mul(FNV1A_PRIME);
        }
        offset += n as u64;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// 테스트용 임시 파일 (drop 시 삭제)
    struct TmpFile {
        path: PathBuf,
    }

    impl TmpFile {
        fn new(content: &[u8]) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "riffle_fp_{}_{}_{}.tmp",
                std::process::id(),
                id,
                content.len()
            ));
            std::fs::write(&path, content).expect("임시 파일 쓰기 실패");
            TmpFile { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// 메모리 기반 가짜 RandomRead.
    /// chunk_limit을 두면 read_at이 요청보다 적게 반환하는 짧은 읽기를 흉내낸다.
    struct MemRemote {
        data: Vec<u8>,
        chunk_limit: usize,
    }

    impl crate::RandomRead for MemRemote {
        fn stat_len(&self) -> io::Result<u64> {
            Ok(self.data.len() as u64)
        }

        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            let offset = offset as usize;
            if offset >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            let want = buf.len().min(self.data.len() - offset);
            let n = if self.chunk_limit > 0 {
                want.min(self.chunk_limit)
            } else {
                want
            };
            buf[..n].copy_from_slice(&self.data[offset..offset + n]);
            Ok(n)
        }

        fn uri(&self) -> String {
            "sftp://test/fp.log".to_string()
        }
    }

    #[test]
    fn test_fingerprint_empty_file() {
        let tmp = TmpFile::new(b"");
        let file = LogFile::open(tmp.path()).expect("열기 실패");
        // 빈 파일은 FNV-1a 오프셋 기본값 그대로다
        assert_eq!(head_fingerprint(&file).expect("지문 실패"), FNV1A_OFFSET);
    }

    #[test]
    fn test_fingerprint_small_file() {
        let tmp = TmpFile::new(b"hello world\n");
        let file = LogFile::open(tmp.path()).expect("열기 실패");
        // 참조 구현과 같은 값을 반환한다
        let mut expect = FNV1A_OFFSET;
        for &b in b"hello world\n" {
            expect ^= b as u64;
            expect = expect.wrapping_mul(FNV1A_PRIME);
        }
        assert_eq!(head_fingerprint(&file).expect("지문 실패"), expect);
        // 내용이 다르면 지문도 다르다
        let tmp2 = TmpFile::new(b"hello worle\n");
        let file2 = LogFile::open(tmp2.path()).expect("열기 실패");
        assert_ne!(
            head_fingerprint(&file).expect("지문 실패"),
            head_fingerprint(&file2).expect("지문 실패")
        );
    }

    #[test]
    fn test_fingerprint_only_head_64k() {
        // 64KB를 넘는 파일: 앞 64KB만 지문에 반영된다
        let mut head = Vec::new();
        for i in 0..(HEAD_FINGERPRINT_BYTES as usize) {
            head.push((i % 251) as u8);
        }
        let mut content = head.clone();
        content.extend_from_slice(&vec![b'x'; 100_000]);

        let tmp = TmpFile::new(&content);
        let file = LogFile::open(tmp.path()).expect("열기 실패");
        let fp1 = head_fingerprint(&file).expect("지문 실패");

        // 뒤쪽 내용을 바꿔도 지문은 같다
        let mut tail_changed = head.clone();
        tail_changed.extend_from_slice(&vec![b'y'; 100_000]);
        let tmp2 = TmpFile::new(&tail_changed);
        let file2 = LogFile::open(tmp2.path()).expect("열기 실패");
        assert_eq!(fp1, head_fingerprint(&file2).expect("지문 실패"));

        // 머리 한 바이트를 바꾸면 지문이 달라진다
        let mut head_changed = content.clone();
        head_changed[123] ^= 0xff;
        let tmp3 = TmpFile::new(&head_changed);
        let file3 = LogFile::open(tmp3.path()).expect("열기 실패");
        assert_ne!(fp1, head_fingerprint(&file3).expect("지문 실패"));
    }

    #[test]
    fn test_fingerprint_remote_matches_local() {
        // 같은 내용의 원격 소스(짧은 읽기 포함)도 로컬과 같은 지문을 반환한다
        let mut content = Vec::new();
        for i in 0..1000 {
            content.extend_from_slice(format!("line {i} 내용\n").as_bytes());
        }
        let tmp = TmpFile::new(&content);
        let local = LogFile::open(tmp.path()).expect("열기 실패");
        let local_fp = head_fingerprint(&local).expect("지문 실패");

        let remote = LogFile::open_remote(Arc::new(MemRemote {
            data: content.clone(),
            chunk_limit: 0,
        }))
        .expect("원격 열기 실패");
        assert_eq!(local_fp, head_fingerprint(&remote).expect("지문 실패"));

        // 1바이트씩만 반환하는 극단적인 짧은 읽기에서도 같은 값
        let chunked = LogFile::open_remote(Arc::new(MemRemote {
            data: content,
            chunk_limit: 1,
        }))
        .expect("원격 열기 실패");
        assert_eq!(local_fp, head_fingerprint(&chunked).expect("지문 실패"));
    }
}
