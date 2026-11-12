use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;

use riffle_core::LogFile;

use crate::tail::TailLoopMsg;

/// tail 모드 감시 스레드 핸들
pub struct TailHandle {
    /// 감시 중지 플래그
    pub stop: Arc<AtomicBool>,
    /// 감시 루프 깨우기 채널 (Stop 전송 시 대기 없이 즉시 종료)
    pub wake: std::sync::mpsc::Sender<TailLoopMsg>,
    /// 감시 스레드
    pub thread: JoinHandle<()>,
}

/// 열려 있는 단일 로그 파일의 상태 정보
pub struct OpenFile {
    /// 로그 파일 코어 핸들
    pub file: Arc<LogFile>,
    /// 인덱싱/작업 취소 플래그
    pub cancel: Arc<AtomicBool>,
    /// 파일 경로 (tail 감시 대상 디렉터리 결정용)
    pub path: PathBuf,
    /// tail 모드 감시 핸들 (켜져 있을 때만 존재)
    pub tail: Option<TailHandle>,
}

/// 단일 검색 세션의 상태 정보
pub struct SearchState {
    /// 검색 고유 ID
    pub id: u32,
    /// 대상 파일 ID
    pub file_id: u32,
    /// 검색 작업 취소 플래그
    pub cancel: Arc<AtomicBool>,
    /// 매치된 줄 번호 목록 (오름차순)
    pub results: Arc<RwLock<Vec<u64>>>,
    /// 검색 완료 여부 플래그
    pub done: Arc<AtomicBool>,
    /// 결과 상한(1,000만 건) 도달 여부 플래그
    pub capped: Arc<AtomicBool>,
}

/// 애플리케이션 전역 상태.
/// 열린 파일 목록과 파일 ID 발급 카운터, 검색 상태 목록을 관리한다.
pub struct AppState {
    /// ID별 열린 파일 맵
    pub files: Mutex<HashMap<u32, OpenFile>>,
    /// ID별 활성 검색 상태 맵
    pub searches: Mutex<HashMap<u32, Arc<SearchState>>>,
    /// 다음에 발급할 파일 ID
    pub next_id: AtomicU32,
    /// 다음에 발급할 검색 ID
    pub next_search_id: AtomicU32,
    /// 최근 파일 목록 (최근 순, 네이티브 메뉴 표시용)
    pub recent_files: Mutex<Vec<String>>,
    /// Finder 파일 연결로 열린 경로 (콜드 스타트 시 프론트 리스너 등록 전 도착분)
    pub pending_open: Mutex<Option<String>>,
    /// 프론트가 take_pending_open을 호출했는지 여부 (호출 = 프론트 준비 완료)
    pub front_ready: AtomicBool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            searches: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            next_search_id: AtomicU32::new(1),
            recent_files: Mutex::new(Vec::new()),
            pending_open: Mutex::new(None),
            front_ready: AtomicBool::new(false),
        }
    }
}

impl AppState {
    /// 새 열린 파일을 상태에 등록하고 고유 ID를 반환한다.
    pub fn insert_file(&self, file: Arc<LogFile>, cancel: Arc<AtomicBool>, path: PathBuf) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut files = self.files.lock().unwrap_or_else(|p| p.into_inner());
        files.insert(
            id,
            OpenFile {
                file,
                cancel,
                path,
                tail: None,
            },
        );
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

    /// 파일을 닫고 취소 플래그를 설정한 뒤 맵에서 제거한다.
    /// 해당 파일에 연결된 모든 검색 세션도 취소하고 제거한다.
    pub fn close_file(&self, id: u32) {
        // 1. 파일 닫기, 인덱싱 취소, tail 핸들 회수 (join은 lock 밖에서 수행)
        let tail_handle = {
            let mut files = self.files.lock().unwrap_or_else(|p| p.into_inner());
            let mut tail = None;
            if let Some(open_file) = files.remove(&id) {
                open_file.cancel.store(true, Ordering::Relaxed);
                tail = open_file.tail;
            }
            tail
        };
        if let Some(handle) = tail_handle {
            handle.stop.store(true, Ordering::Relaxed);
            let _ = handle.wake.send(TailLoopMsg::Stop);
            let _ = handle.thread.join();
        }

        // 2. 해당 파일의 활성 검색 세션 모두 취소 및 제거
        let mut searches = self.searches.lock().unwrap_or_else(|p| p.into_inner());
        searches.retain(|_, s| {
            if s.file_id == id {
                s.cancel.store(true, Ordering::Relaxed);
                false
            } else {
                true
            }
        });
    }

    /// 현재 열려 있는 파일 수를 반환한다.
    pub fn file_count(&self) -> usize {
        let files = self.files.lock().unwrap_or_else(|p| p.into_inner());
        files.len()
    }

    /// 새 검색 상태를 등록한다.
    /// 대상 파일이 아직 열려 있어야 하며, 같은 file_id를 가진 이전 검색 세션은
    /// 취소하고 맵에서 제거한다. 파일이 없으면 Err를 반환하고 아무것도 등록하지 않는다.
    pub fn insert_search(
        &self,
        file_id: u32,
        cancel: Arc<AtomicBool>,
        results: Arc<RwLock<Vec<u64>>>,
        done: Arc<AtomicBool>,
        capped: Arc<AtomicBool>,
    ) -> Result<Arc<SearchState>, String> {
        // files를 먼저 확인해 파일이 닫힌 뒤의 검색 등록(스레드 누수)을 막는다.
        // 락은 중첩하지 않고 순차 획득하므로 데드락이 없다.
        {
            let files = self.files.lock().unwrap_or_else(|p| p.into_inner());
            if !files.contains_key(&file_id) {
                return Err(format!(
                    "ID {file_id}에 해당하는 열린 파일을 찾을 수 없습니다."
                ));
            }
        }

        let mut searches = self.searches.lock().unwrap_or_else(|p| p.into_inner());

        // 동일 파일의 기존 검색 자동 취소 및 제거
        searches.retain(|_, s| {
            if s.file_id == file_id {
                s.cancel.store(true, Ordering::Relaxed);
                false
            } else {
                true
            }
        });

        let id = self.next_search_id.fetch_add(1, Ordering::Relaxed);
        let search_state = Arc::new(SearchState {
            id,
            file_id,
            cancel,
            results,
            done,
            capped,
        });

        searches.insert(id, search_state.clone());
        Ok(search_state)
    }

    /// ID에 해당하는 검색 상태를 조회한다.
    pub fn get_search(&self, search_id: u32) -> Result<Arc<SearchState>, String> {
        let searches = self.searches.lock().unwrap_or_else(|p| p.into_inner());
        searches
            .get(&search_id)
            .cloned()
            .ok_or_else(|| format!("ID {}에 해당하는 검색 상태를 찾을 수 없습니다.", search_id))
    }

    /// 검색 작업을 취소한다.
    pub fn cancel_search(&self, search_id: u32) {
        let searches = self.searches.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = searches.get(&search_id) {
            s.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 현재 관리 중인 검색 세션 수를 반환한다.
    pub fn search_count(&self) -> usize {
        let searches = self.searches.lock().unwrap_or_else(|p| p.into_inner());
        searches.len()
    }

    /// 최근 파일 목록을 설정한다 (파일 열기 성공 시 갱신).
    pub fn set_recent_files(&self, list: Vec<String>) {
        *self.recent_files.lock().unwrap_or_else(|p| p.into_inner()) = list;
    }

    /// 최근 파일 목록을 반환한다 (최근 순).
    pub fn recent_files(&self) -> Vec<String> {
        self.recent_files
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Finder에서 연 파일 경로를 임시 보관한다 (여러 개면 마지막 것만 유지).
    pub fn set_pending_open(&self, path: String) {
        *self.pending_open.lock().unwrap_or_else(|p| p.into_inner()) = Some(path);
    }

    /// 보관된 대기 경로를 꺼내고 비운다. 한 번만 값을 반환한다.
    /// 호출 시점에 프론트 준비가 완료된 것으로 표시한다.
    pub fn take_pending_open(&self) -> Option<String> {
        self.front_ready.store(true, Ordering::Relaxed);
        self.pending_open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// 프론트가 준비 완료(take_pending_open 호출)했는지 반환한다.
    pub fn is_front_ready(&self) -> bool {
        self.front_ready.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// insert_search의 파일 존재 확인을 통과시키기 위해 임시 LogFile을 등록한다.
    /// 반환된 경로는 테스트 끝에서 삭제해야 한다.
    fn insert_test_file(state: &AppState, id: u32) -> PathBuf {
        let path = crate::testutil::temp_log_path("state_test");
        std::fs::write(&path, b"x\n").expect("임시 파일 생성 실패");
        let log = Arc::new(LogFile::open(&path).expect("LogFile::open 실패"));
        state.files.lock().unwrap().insert(
            id,
            OpenFile {
                file: log,
                cancel: Arc::new(AtomicBool::new(false)),
                path: path.clone(),
                tail: None,
            },
        );
        path
    }

    fn remove_test_file(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_new_search_cancels_and_removes_previous_search() {
        let state = AppState::default();
        let p1 = insert_test_file(&state, 1);
        let p2 = insert_test_file(&state, 2);

        let cancel1 = Arc::new(AtomicBool::new(false));
        let results1 = Arc::new(RwLock::new(Vec::new()));
        let done1 = Arc::new(AtomicBool::new(false));
        let capped1 = Arc::new(AtomicBool::new(false));

        // 파일 1번에 대한 검색 1 등록
        let s1 = state
            .insert_search(
                1,
                cancel1.clone(),
                results1.clone(),
                done1.clone(),
                capped1.clone(),
            )
            .unwrap();
        assert_eq!(s1.id, 1);
        assert!(!cancel1.load(Ordering::Relaxed));
        assert!(state.get_search(1).is_ok());

        // 같은 파일(1번)에 대해 새 검색 2 등록 -> 이전 검색 1 취소 및 제거
        let cancel2 = Arc::new(AtomicBool::new(false));
        let results2 = Arc::new(RwLock::new(Vec::new()));
        let done2 = Arc::new(AtomicBool::new(false));
        let capped2 = Arc::new(AtomicBool::new(false));

        let s2 = state
            .insert_search(
                1,
                cancel2.clone(),
                results2.clone(),
                done2.clone(),
                capped2.clone(),
            )
            .unwrap();
        assert_eq!(s2.id, 2);
        assert!(cancel1.load(Ordering::Relaxed)); // 이전 검색 1 취소됨!
        assert!(!cancel2.load(Ordering::Relaxed));
        assert!(state.get_search(1).is_err()); // 이전 검색 1 제거됨!
        assert!(state.get_search(2).is_ok());

        // 다른 파일(2번)에 대해 새 검색 3 등록 -> 파일 1번의 검색 2는 영향 없음
        let cancel3 = Arc::new(AtomicBool::new(false));
        let results3 = Arc::new(RwLock::new(Vec::new()));
        let done3 = Arc::new(AtomicBool::new(false));
        let capped3 = Arc::new(AtomicBool::new(false));

        let s3 = state
            .insert_search(
                2,
                cancel3.clone(),
                results3.clone(),
                done3.clone(),
                capped3.clone(),
            )
            .unwrap();
        assert_eq!(s3.id, 3);
        assert!(!cancel2.load(Ordering::Relaxed));
        assert!(!cancel3.load(Ordering::Relaxed));
        assert_eq!(state.search_count(), 2);
        remove_test_file(&p1);
        remove_test_file(&p2);
    }

    #[test]
    fn test_close_file_cleans_up_searches() {
        let state = AppState::default();
        let p10 = insert_test_file(&state, 10);
        let p20 = insert_test_file(&state, 20);

        let cancel1 = Arc::new(AtomicBool::new(false));
        let cancel2 = Arc::new(AtomicBool::new(false));

        // 파일 10번에 검색 1 등록
        let s1 = state
            .insert_search(
                10,
                cancel1.clone(),
                Arc::new(RwLock::new(Vec::new())),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();

        // 파일 20번에 검색 2 등록
        let s2 = state
            .insert_search(
                20,
                cancel2.clone(),
                Arc::new(RwLock::new(Vec::new())),
                Arc::new(AtomicBool::new(false)),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();

        assert_eq!(state.search_count(), 2);

        // 파일 10번을 닫음 -> 검색 1만 취소되고 제거되어야 함
        state.close_file(10);

        assert!(cancel1.load(Ordering::Relaxed)); // 검색 1 취소됨
        assert!(!cancel2.load(Ordering::Relaxed)); // 검색 2 유지
        assert!(state.get_search(s1.id).is_err());
        assert!(state.get_search(s2.id).is_ok());
        assert_eq!(state.search_count(), 1);
        remove_test_file(&p10);
        remove_test_file(&p20);
    }

    #[test]
    fn test_insert_search_rejects_closed_file() {
        let state = AppState::default();
        // 파일이 등록되지 않은 ID로는 검색을 등록할 수 없다
        let result = state.insert_search(
            999,
            Arc::new(AtomicBool::new(false)),
            Arc::new(RwLock::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(result.is_err());
        assert_eq!(state.search_count(), 0);
    }

    #[test]
    fn test_search_state_lifecycle() {
        let state = AppState::default();
        let p1 = insert_test_file(&state, 1);
        let p2 = insert_test_file(&state, 2);

        let cancel1 = Arc::new(AtomicBool::new(false));
        let results1 = Arc::new(RwLock::new(Vec::new()));
        let done1 = Arc::new(AtomicBool::new(false));
        let capped1 = Arc::new(AtomicBool::new(false));

        // 1. 파일 1번에 대한 검색 1 등록
        let s1 = state
            .insert_search(
                1,
                cancel1.clone(),
                results1.clone(),
                done1.clone(),
                capped1.clone(),
            )
            .unwrap();
        assert_eq!(s1.id, 1);
        assert_eq!(s1.file_id, 1);
        assert_eq!(state.search_count(), 1);
        assert!(!cancel1.load(Ordering::Relaxed));

        // 2. 다른 파일(2번)에 대한 검색 2 등록 -> 검색 1은 취소되지 않아야 함
        let cancel2 = Arc::new(AtomicBool::new(false));
        let results2 = Arc::new(RwLock::new(Vec::new()));
        let done2 = Arc::new(AtomicBool::new(false));
        let capped2 = Arc::new(AtomicBool::new(false));

        let s2 = state
            .insert_search(
                2,
                cancel2.clone(),
                results2.clone(),
                done2.clone(),
                capped2.clone(),
            )
            .unwrap();
        assert_eq!(s2.id, 2);
        assert_eq!(state.search_count(), 2);
        assert!(!cancel1.load(Ordering::Relaxed));
        assert!(!cancel2.load(Ordering::Relaxed));

        // 3. 파일 1번에 대한 새 검색 3 등록 -> 파일 1번의 검색 1은 자동 취소 및 제거
        let cancel3 = Arc::new(AtomicBool::new(false));
        let results3 = Arc::new(RwLock::new(Vec::new()));
        let done3 = Arc::new(AtomicBool::new(false));
        let capped3 = Arc::new(AtomicBool::new(false));

        let s3 = state
            .insert_search(
                1,
                cancel3.clone(),
                results3.clone(),
                done3.clone(),
                capped3.clone(),
            )
            .unwrap();
        assert_eq!(s3.id, 3);
        assert_eq!(state.search_count(), 2); // s1 제거, s2와 s3 존재
        assert!(cancel1.load(Ordering::Relaxed)); // s1 취소됨!
        assert!(!cancel2.load(Ordering::Relaxed));
        assert!(!cancel3.load(Ordering::Relaxed));

        // 검색 조회 검증
        assert!(state.get_search(s1.id).is_err());
        assert!(state.get_search(s2.id).is_ok());
        assert!(state.get_search(s3.id).is_ok());

        // 4. cancel_search 직접 호출 검증
        state.cancel_search(s2.id);
        assert!(cancel2.load(Ordering::Relaxed));

        // 5. 파일 1번 close_file 호출 시 연결된 s3 정리 검증
        state.close_file(1);
        assert!(cancel3.load(Ordering::Relaxed)); // s3 취소됨!
        assert_eq!(state.search_count(), 1); // s2만 남음
        assert!(state.get_search(s3.id).is_err());
        remove_test_file(&p1);
        remove_test_file(&p2);
    }

    #[test]
    fn test_pending_open_take_once() {
        let state = AppState::default();

        // 비어 있으면 None
        assert_eq!(state.take_pending_open(), None);

        // 설정 후 take하면 경로가 반환되고 비워진다
        state.set_pending_open("/tmp/a.log".to_string());
        assert_eq!(state.take_pending_open().as_deref(), Some("/tmp/a.log"));
        assert_eq!(state.take_pending_open(), None);

        // 여러 번 설정하면 마지막 경로만 남는다
        state.set_pending_open("/tmp/a.log".to_string());
        state.set_pending_open("/tmp/b.log".to_string());
        assert_eq!(state.take_pending_open().as_deref(), Some("/tmp/b.log"));
    }
}
