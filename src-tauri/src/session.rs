// 탭 세션 복원: 열린 탭 목록과 활성 탭을 session.json에 저장 (app_data_dir)

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 탭 세션 DTO. session.json 저장과 IPC(save_session/load_session) 양쪽에 사용한다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDto {
    /// 열린 탭의 파일 경로 목록 (탭 순서)
    pub paths: Vec<String>,
    /// 활성 탭 인덱스 (paths 내 위치, 없으면 None)
    pub active_index: Option<u32>,
}

/// session.json 파일 경로
fn session_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("session.json"))
        .map_err(|e| format!("앱 데이터 디렉터리 조회 실패: {e}"))
}

/// session.json을 읽어 SessionDto로 반환한다.
/// 파일이 없거나 JSON 파싱에 실패하면 None을 반환한다.
fn read_session_file(path: &Path) -> Option<SessionDto> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// 세션을 session.json에 원자적으로 저장한다.
/// 임시 파일에 쓴 뒤 rename으로 교체해 중간에 죽어도 파일이 깨지지 않게 한다.
fn write_session_file(path: &Path, session: &SessionDto) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("세션 디렉터리 생성 실패 ('{}'): {e}", dir.display()))?;
    }
    let json = serde_json::to_string(session).map_err(|e| e.to_string())?;
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    std::fs::write(&tmp_path, json)
        .map_err(|e| format!("세션 임시 저장 실패 ('{}'): {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("세션 저장 실패 ('{}'): {e}", path.display())
    })
}

/// 존재하는 경로만 남기고 활성 탭 인덱스를 재계산한다.
/// 경로가 모두 사라지면 복원할 세션이 없으므로 None을 반환한다.
/// exists 클로저에 파일 시스템 접근을 위임해 순수 함수로 유지한다.
pub(crate) fn filter_session(
    session: SessionDto,
    exists: impl Fn(&str) -> bool,
) -> Option<SessionDto> {
    // 필터링으로 인덱스가 밀리므로, 먼저 활성 탭의 경로를 기억해 둔다
    let active_path = session
        .active_index
        .and_then(|i| session.paths.get(i as usize))
        .cloned();
    let paths: Vec<String> = session.paths.into_iter().filter(|p| exists(p)).collect();
    if paths.is_empty() {
        return None;
    }
    let active_index = active_path
        .and_then(|ap| paths.iter().position(|p| p == &ap))
        .map(|i| i as u32);
    Some(SessionDto {
        paths,
        active_index,
    })
}

/// 파일 존재 여부 확인 클로저 (없는 경로는 세션에서 제외하기 위해 사용)
fn path_exists(path: &str) -> bool {
    // 원격(sftp://) 경로는 로컬에서 존재 여부를 확인할 수 없으므로 유지한다
    if path.starts_with("sftp://") {
        return true;
    }
    Path::new(path).try_exists().unwrap_or(false)
}

/// 탭 세션을 session.json에 저장한다. 프론트가 탭 변경 시 디바운스로 호출한다.
#[tauri::command]
pub async fn save_session(app: tauri::AppHandle, session: SessionDto) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = session_path(&app)?;
        write_session_file(&path, &session)
    })
    .await
    .map_err(|e| format!("세션 저장 비동기 태스크 실패: {e}"))?
}

/// 저장된 탭 세션을 반환한다. restore_session 설정이 꺼져 있거나
/// 파일이 없거나 경로가 모두 사라졌으면 None을 반환한다.
/// 존재하지 않는 경로는 걸러서 반환하며 활성 탭 인덱스도 재계산한다.
#[tauri::command]
pub async fn load_session(app: tauri::AppHandle) -> Result<Option<SessionDto>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        if !crate::settings::load_settings(&app).restore_session {
            return Ok(None);
        }
        let path = session_path(&app)?;
        let Some(session) = read_session_file(&path) else {
            return Ok(None);
        };
        Ok(filter_session(session, path_exists))
    })
    .await
    .map_err(|e| format!("세션 로드 비동기 태스크 실패: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_serde_camel_case() {
        let session = SessionDto {
            paths: vec!["/a.log".to_string(), "/b.log".to_string()],
            active_index: Some(1),
        };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(json, r#"{"paths":["/a.log","/b.log"],"activeIndex":1}"#);
        let parsed: SessionDto = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, session);
    }

    #[test]
    fn test_read_missing_file_returns_none() {
        let path = crate::testutil::temp_log_path("session_missing");
        assert_eq!(read_session_file(&path), None);
    }

    #[test]
    fn test_read_corrupted_file_returns_none() {
        let path = crate::testutil::temp_log_path("session_corrupt");
        std::fs::write(&path, b"{ this is not json !!!").unwrap();
        assert_eq!(read_session_file(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_write_then_read_roundtrip() {
        let path = crate::testutil::temp_log_path("session_rw");
        let session = SessionDto {
            paths: vec!["/a.log".to_string()],
            active_index: Some(0),
        };
        write_session_file(&path, &session).unwrap();
        assert_eq!(read_session_file(&path), Some(session));
        // 임시 파일이 남지 않는다
        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(".tmp");
        assert!(!Path::new(&tmp_name).exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_filter_session_removes_missing_paths() {
        let session = SessionDto {
            paths: vec!["/gone".to_string(), "/a".to_string(), "/b".to_string()],
            active_index: Some(2),
        };
        let filtered = filter_session(session, |p| p != "/gone").unwrap();
        // "/gone"이 빠지면서 "/b"의 인덱스가 2에서 1로 당겨진다
        assert_eq!(filtered.paths, vec!["/a".to_string(), "/b".to_string()]);
        assert_eq!(filtered.active_index, Some(1));
    }

    #[test]
    fn test_filter_session_active_path_removed() {
        let session = SessionDto {
            paths: vec!["/a".to_string(), "/gone".to_string()],
            active_index: Some(1),
        };
        let filtered = filter_session(session, |p| p != "/gone").unwrap();
        // 활성 탭이 사라졌으면 인덱스는 None (프론트가 기본 탭을 고른다)
        assert_eq!(filtered.paths, vec!["/a".to_string()]);
        assert_eq!(filtered.active_index, None);
    }

    #[test]
    fn test_filter_session_all_gone_returns_none() {
        let session = SessionDto {
            paths: vec!["/x".to_string(), "/y".to_string()],
            active_index: Some(0),
        };
        assert_eq!(filter_session(session, |_| false), None);
    }

    #[test]
    fn test_filter_session_out_of_range_active_index() {
        let session = SessionDto {
            paths: vec!["/a".to_string()],
            active_index: Some(9),
        };
        let filtered = filter_session(session, |_| true).unwrap();
        assert_eq!(filtered.active_index, None);
    }
}
