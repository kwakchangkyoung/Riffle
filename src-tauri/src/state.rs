use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use riffle_core::LogFile;

/// 열려 있는 단일 로그 파일의 상태 정보
pub struct OpenFile {
    /// 로그 파일 코어 핸들
    pub file: Arc<LogFile>,
    /// 인덱싱/작업 취소 플래그
    pub cancel: Arc<AtomicBool>,
}

/// 애플리케이션 전역 상태.
/// 열린 파일 목록과 파일 ID 발급 카운터를 관리한다.
pub struct AppState {
    /// ID별 열린 파일 맵
    pub files: Mutex<HashMap<u32, OpenFile>>,
    /// 다음에 발급할 파일 ID
    pub next_id: AtomicU32,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
        }
    }
}

impl AppState {
    /// 새 열린 파일을 상태에 등록하고 고유 ID를 반환한다.
    pub fn insert_file(&self, file: Arc<LogFile>, cancel: Arc<AtomicBool>) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut files = self.files.lock().unwrap_or_else(|p| p.into_inner());
        files.insert(id, OpenFile { file, cancel });
        id
    }

    /// ID에 해당하는 파일 핸들을 조회한다.
    pub fn get_file(&self, id: u32) -> Result<Arc<LogFile>, String> {
        let files = self.files.lock().unwrap_or_else(|p| p.into_inner());
        files
            .get(&id)
            .map(|f| f.file.clone())
            .ok_or_else(|| format!("ID {}에 해당하는 열린 파일을 찾을 수 없습니다.", id))
    }

    /// 파일을 닫고 취소 플래그를 설정한 뒤 맵에서 제거한다. 없는 ID인 경우 무시한다.
    pub fn close_file(&self, id: u32) {
        let mut files = self.files.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(open_file) = files.remove(&id) {
            open_file.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 현재 열려 있는 파일 수를 반환한다.
    pub fn file_count(&self) -> usize {
        let files = self.files.lock().unwrap_or_else(|p| p.into_inner());
        files.len()
    }
}
