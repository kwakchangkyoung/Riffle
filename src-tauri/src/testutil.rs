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
