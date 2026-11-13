// 최근 파일 목록 관리 (app_data_dir/recent.json)

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tauri::{AppHandle, Manager};

/// recent.json 읽기-수정-쓰기와 메뉴 재구성을 직렬화하는 전역 락
static RECENT_LOCK: Mutex<()> = Mutex::new(());

/// recent 관련 읽기-수정-쓰기(와 메뉴 재구성)를 하나의 락으로 직렬화한다.
pub(crate) fn with_recent_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = RECENT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    f()
}

/// 최근 파일 목록 최대 개수
pub const MAX_RECENT_FILES: usize = 10;

/// recent.json 파일 경로
fn recent_file_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("recent.json"))
        .map_err(|e| format!("앱 데이터 디렉터리 조회 실패: {e}"))
}

/// 목록 맨 앞에 path를 추가한다 (중복 제거, 최대 MAX_RECENT_FILES개). 순수 함수.
pub(crate) fn push_recent(list: &mut Vec<String>, path: &str) {
    list.retain(|p| p != path);
    list.insert(0, path.to_string());
    list.truncate(MAX_RECENT_FILES);
}

/// 존재하는 경로만 남긴다. 순수 함수 (파일 시스템 접근은 exists 클로저에 위임).
pub(crate) fn filter_existing(list: Vec<String>, exists: impl Fn(&str) -> bool) -> Vec<String> {
    list.into_iter().filter(|p| exists(p)).collect()
}

/// recent.json을 읽어 문자열 목록으로 반환한다.
/// 파일이 없거나 JSON 파싱에 실패하면 빈 목록.
fn read_recent_file(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 목록을 recent.json에 원자적으로 저장한다.
/// 임시 파일에 쓴 뒤 rename으로 교체해 중간에 죽어도 파일이 깨지지 않게 한다.
fn write_recent_file(path: &Path, list: &[String]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("최근 파일 디렉터리 생성 실패 ('{}'): {e}", dir.display()))?;
    }
    let json = serde_json::to_string(list).map_err(|e| e.to_string())?;
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    std::fs::write(&tmp_path, json)
        .map_err(|e| format!("최근 파일 임시 저장 실패 ('{}'): {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("최근 파일 저장 실패 ('{}'): {e}", path.display())
    })
}

/// 파일 존재 여부 확인 클로저 (없는 경로는 목록에서 제외하기 위해 사용)
fn path_exists(path: &str) -> bool {
    // 원격(sftp://) 경로는 로컬에서 존재 여부를 확인할 수 없으므로 유지한다
    if path.starts_with("sftp://") {
        return true;
    }
    Path::new(path).try_exists().unwrap_or(false)
}

/// 저장된 최근 파일 목록을 읽고 존재하지 않는 경로를 걸러 반환한다.
pub fn load_recent(app: &AppHandle) -> Result<Vec<String>, String> {
    let file = recent_file_path(app)?;
    Ok(filter_existing(read_recent_file(&file), path_exists))
}

/// path를 최근 파일 목록 맨 앞에 추가(중복 제거, 최대 10개)하고 저장한다.
/// 반환값은 갱신된 목록 (메뉴 재구성용).
/// 읽기-수정-쓰기는 with_recent_lock 안에서 호출해야 직렬화가 보장된다.
pub fn record_open(app: &AppHandle, path: &str) -> Result<Vec<String>, String> {
    let file = recent_file_path(app)?;
    let mut list = read_recent_file(&file);
    push_recent(&mut list, path);
    let list = filter_existing(list, path_exists);
    write_recent_file(&file, &list)?;
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_recent_adds_to_front() {
        let mut list = vec!["/a".to_string(), "/b".to_string()];
        push_recent(&mut list, "/c");
        assert_eq!(list, vec!["/c", "/a", "/b"]);
    }

    #[test]
    fn test_push_recent_removes_duplicate() {
        let mut list = vec!["/a".to_string(), "/b".to_string(), "/a".to_string()];
        push_recent(&mut list, "/a");
        // 중복 제거 후 맨 앞에 1개만 존재
        assert_eq!(list, vec!["/a", "/b"]);
    }

    #[test]
    fn test_push_recent_max_10() {
        let mut list: Vec<String> = (0..15).map(|i| format!("/f{}", i)).collect();
        push_recent(&mut list, "/new");
        assert_eq!(list.len(), MAX_RECENT_FILES);
        assert_eq!(list[0], "/new");
        // 가장 오래된 항목들이 잘려나간다
        assert!(!list.contains(&"/f14".to_string()));
        assert!(list.contains(&"/f5".to_string()));
    }

    #[test]
    fn test_filter_existing() {
        let list = vec![
            "/exists1".to_string(),
            "/gone".to_string(),
            "/exists2".to_string(),
        ];
        let filtered = filter_existing(list, |p| p != "/gone");
        assert_eq!(filtered, vec!["/exists1", "/exists2"]);
    }

    #[test]
    fn test_push_recent_then_filter() {
        // 추가 후 존재하지 않는 경로가 걸러지는 흐름
        let mut list = vec!["/old".to_string()];
        push_recent(&mut list, "/new");
        let filtered = filter_existing(list, |p| p == "/new");
        assert_eq!(filtered, vec!["/new"]);
    }
}
