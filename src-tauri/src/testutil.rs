// 테스트 공용 임시 파일 경로 생성기.
// 프로세스 전역 카운터를 이름에 포함해 병렬 테스트 간 파일 이름 충돌을 방지한다.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 테스트 전용 임시 파일 경로를 반환한다 (실제 파일은 생성하지 않음).
/// 호출마다 항상 유일한 이름을 반환한다.
pub(crate) fn temp_log_path(prefix: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "riffle_{}_{}_{}.log",
        prefix,
        std::process::id(),
        id
    ))
}

/// 테스트용 gzip 파일을 시스템 gzip 명령으로 생성하고 경로를 반환한다.
/// 반환된 경로는 .gz 확장자를 가지며, 테스트 끝에서 remove_file로 삭제해야 한다.
pub(crate) fn create_gzip_file(prefix: &str, content: &[u8]) -> PathBuf {
    let src = temp_log_path(&format!("{prefix}_src"));
    std::fs::write(&src, content).expect("gzip 원본 쓰기 실패");
    let gz_path = src.with_extension("gz");
    let out = std::process::Command::new("gzip")
        .arg("-c")
        .arg(&src)
        .output()
        .expect("gzip 명령 실행 실패");
    assert!(out.status.success(), "gzip 압축 실패");
    std::fs::write(&gz_path, &out.stdout).expect("gzip 결과 쓰기 실패");
    let _ = std::fs::remove_file(&src);
    gz_path
}
