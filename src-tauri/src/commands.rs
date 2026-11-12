use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use riffle_core::LogFile;
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

use crate::state::AppState;

/// 파일 정보 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub id: u32,
    pub path: String,
    pub size: u64,
}

/// 인덱싱 진행률 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgressDto {
    pub indexed_bytes: u64,
    pub total_bytes: u64,
    pub line_count: u64,
    pub done: bool,
}

impl From<riffle_core::IndexProgress> for IndexProgressDto {
    fn from(progress: riffle_core::IndexProgress) -> Self {
        Self {
            indexed_bytes: progress.indexed_bytes,
            total_bytes: progress.total_bytes,
            line_count: progress.line_count,
            done: progress.done,
        }
    }
}

/// 한 줄 정보 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineDto {
    pub no: u64,
    pub text: String,
    pub byte_len: u64,
    pub truncated: bool,
}

impl From<riffle_core::Line> for LineDto {
    fn from(line: riffle_core::Line) -> Self {
        Self {
            no: line.no,
            text: line.text,
            byte_len: line.byte_len,
            truncated: line.truncated,
        }
    }
}

/// get_lines 요청 시 한 번에 가져올 수 있는 최대 줄 수
pub const MAX_GET_LINES_COUNT: u32 = 1000;

/// get_lines의 count 인자를 상한선(1000)으로 제한한다.
#[inline]
pub fn clamp_get_lines_count(count: u32) -> u32 {
    count.min(MAX_GET_LINES_COUNT)
}

/// 파일 열기 내부 로직: LogFile을 열고 AppState에 등록한다.
pub fn open_file_internal(
    state: &AppState,
    path: &str,
) -> Result<(FileInfo, Arc<LogFile>, Arc<AtomicBool>), String> {
    let log_file = LogFile::open(Path::new(path))
        .map_err(|e| format!("파일을 열 수 없습니다 ('{path}'): {e}"))?;
    let file = Arc::new(log_file);
    let cancel = Arc::new(AtomicBool::new(false));
    let size = file.source().len();
    let id = state.insert_file(file.clone(), cancel.clone());

    Ok((
        FileInfo {
            id,
            path: path.to_string(),
            size,
        },
        file,
        cancel,
    ))
}

/// LogFile 인스턴스에서 지정한 범위의 줄들을 읽어 LineDto 벡터로 변환한다.
pub fn read_lines_from_file(
    file: &LogFile,
    start: u64,
    count: u32,
) -> Result<Vec<LineDto>, String> {
    let lines = file
        .read_lines(start, count as u64)
        .map_err(|e| format!("줄 읽기 실패: {e}"))?;
    Ok(lines.into_iter().map(LineDto::from).collect())
}

/// 상태에서 파일 ID를 조회하여 줄들을 읽어온다 (단위 테스트 및 내부 사용용).
pub fn get_lines_from_state(
    state: &AppState,
    id: u32,
    start: u64,
    count: u32,
) -> Result<Vec<LineDto>, String> {
    let file = state.get_file(id)?;
    let clamped_count = clamp_get_lines_count(count);
    read_lines_from_file(&file, start, clamped_count)
}

/// 백그라운드 인덱싱 스레드 실행 로직.
/// Channel 전송을 최대 약 20회/초로 스로틀링하되, 첫 구간 완료 이벤트와 done=true 이벤트는 반드시 전송한다.
pub fn run_indexing(
    file: Arc<LogFile>,
    cancel: Arc<AtomicBool>,
    on_progress: Channel<IndexProgressDto>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut is_first = true;
        let mut last_send = Instant::now();

        file.build_index(&cancel, &mut |progress| {
            let now = Instant::now();
            let is_done = progress.done;
            let should_send =
                is_first || is_done || now.duration_since(last_send) >= Duration::from_millis(50);

            if should_send {
                is_first = false;
                last_send = now;
                let dto = IndexProgressDto::from(progress);
                if let Err(e) = on_progress.send(dto) {
                    eprintln!("인덱싱 진행률 채널 전송 오류: {e}");
                }
            }
        })
    }));

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("인덱싱 오류 발생: {e}");
        }
        Err(panic_payload) => {
            eprintln!("인덱싱 스레드 패닉 발생: {panic_payload:?}");
        }
    }
}

/// 로그 파일을 열고 즉시 파일 정보를 반환하며, 백그라운드에서 인덱싱을 시작한다.
#[tauri::command]
pub async fn open_file(
    state: tauri::State<'_, AppState>,
    path: String,
    on_progress: Channel<IndexProgressDto>,
) -> Result<FileInfo, String> {
    let (info, file, cancel) = open_file_internal(&state, &path)?;

    std::thread::spawn(move || {
        run_indexing(file, cancel, on_progress);
    });

    Ok(info)
}

/// 지정된 범위의 로그 줄들을 읽어 반환한다.
#[tauri::command]
pub async fn get_lines(
    state: tauri::State<'_, AppState>,
    id: u32,
    start: u64,
    count: u32,
    query: Option<String>,
) -> Result<Vec<LineDto>, String> {
    let _ = query; // Phase 3에서 검색 연동 예정
    let file = state.get_file(id)?;
    let clamped_count = clamp_get_lines_count(count);

    tauri::async_runtime::spawn_blocking(move || read_lines_from_file(&file, start, clamped_count))
        .await
        .map_err(|e| format!("줄 읽기 비동기 태스크 실패: {e}"))?
}

/// 열린 파일을 닫고 취소 플래그를 설정하여 자원을 정리한다.
#[tauri::command]
pub async fn close_file(state: tauri::State<'_, AppState>, id: u32) -> Result<(), String> {
    state.close_file(id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::Ordering;

    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn create(content: &[u8]) -> Self {
            let unique_name = format!(
                "riffle_test_{}_{}.log",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique_name);
            let mut file = std::fs::File::create(&path).expect("임시 파일 생성 실패");
            file.write_all(content).expect("임시 파일 쓰기 실패");
            file.sync_all().expect("임시 파일 플러시 실패");
            Self { path }
        }

        fn path_str(&self) -> &str {
            self.path.to_str().unwrap()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn test_clamp_get_lines_count() {
        assert_eq!(clamp_get_lines_count(0), 0);
        assert_eq!(clamp_get_lines_count(1), 1);
        assert_eq!(clamp_get_lines_count(500), 500);
        assert_eq!(clamp_get_lines_count(1000), 1000);
        assert_eq!(clamp_get_lines_count(1001), 1000);
        assert_eq!(clamp_get_lines_count(50_000), 1000);
        assert_eq!(clamp_get_lines_count(u32::MAX), 1000);
    }

    #[test]
    fn test_open_get_lines_close_flow() {
        let content = b"line 0\nline 1\nline 2\nline 3\nline 4\n";
        let temp_file = TempFile::create(content);
        let state = AppState::default();

        // 1. open_file_internal 검증
        let (info, file, cancel) =
            open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        assert_eq!(info.id, 1);
        assert_eq!(info.path, temp_file.path_str());
        assert_eq!(info.size, content.len() as u64);
        assert_eq!(state.file_count(), 1);

        // 인덱싱 수행
        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");

        // 2. get_lines_from_state 검증 (정상 범위)
        let lines = get_lines_from_state(&state, info.id, 0, 3).expect("줄 읽기 실패");
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            LineDto {
                no: 0,
                text: "line 0".to_string(),
                byte_len: 6,
                truncated: false,
            }
        );
        assert_eq!(
            lines[1],
            LineDto {
                no: 1,
                text: "line 1".to_string(),
                byte_len: 6,
                truncated: false,
            }
        );
        assert_eq!(
            lines[2],
            LineDto {
                no: 2,
                text: "line 2".to_string(),
                byte_len: 6,
                truncated: false,
            }
        );

        // 3. get_lines_from_state 검증 (count 상한 1000 초과 요청 시 clamp 동작)
        let all_lines = get_lines_from_state(&state, info.id, 0, 5000).expect("줄 읽기 실패");
        assert_eq!(all_lines.len(), 5);

        // 4. close_file 검증
        assert!(!cancel.load(Ordering::Relaxed));
        state.close_file(info.id);
        assert!(cancel.load(Ordering::Relaxed));
        assert_eq!(state.file_count(), 0);

        // 닫힌 파일에 get_lines 시 에러
        let err = get_lines_from_state(&state, info.id, 0, 10);
        assert!(err.is_err());
    }

    #[test]
    fn test_nonexistent_file_id() {
        let state = AppState::default();
        let result = get_lines_from_state(&state, 9999, 0, 10);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("9999"));
    }

    #[test]
    fn test_close_nonexistent_id() {
        let state = AppState::default();
        // 없는 ID를 닫아도 패닉 없이 정상 동작해야 함
        state.close_file(9999);
        assert_eq!(state.file_count(), 0);
    }

    #[test]
    fn test_open_nonexistent_file() {
        let state = AppState::default();
        let result = open_file_internal(&state, "/nonexistent/path/to/file.log");
        assert!(result.is_err());
        let err_msg = result.err().unwrap();
        assert!(err_msg.contains("파일을 열 수 없습니다"));
    }
}
