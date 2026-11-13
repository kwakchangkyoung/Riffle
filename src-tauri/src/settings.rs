// 앱 설정 관리 (app_data_dir/settings.json)

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 앱 설정. settings.json 저장과 IPC(get_settings) 양쪽에 같은 형태로 사용한다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// 시작 시 업데이트 자동 확인 여부 (기본 true)
    pub auto_check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_check_updates: true,
        }
    }
}

/// settings.json 파일 경로
fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|e| format!("앱 데이터 디렉터리 조회 실패: {e}"))
}

/// settings.json을 읽어 Settings로 반환한다.
/// 파일이 없거나 JSON 파싱에 실패하면 기본값을 반환한다.
fn read_settings_file(path: &Path) -> Settings {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Settings::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 설정을 settings.json에 원자적으로 저장한다.
/// 임시 파일에 쓴 뒤 rename으로 교체해 중간에 죽어도 파일이 깨지지 않게 한다.
fn write_settings_file(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("설정 디렉터리 생성 실패 ('{}'): {e}", dir.display()))?;
    }
    let json = serde_json::to_string(settings).map_err(|e| e.to_string())?;
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    std::fs::write(&tmp_path, json)
        .map_err(|e| format!("설정 임시 저장 실패 ('{}'): {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("설정 저장 실패 ('{}'): {e}", path.display())
    })
}

/// 저장된 설정을 읽는다. 읽기/파싱 실패 시 기본값.
pub fn load_settings(app: &AppHandle) -> Settings {
    match settings_path(app) {
        Ok(path) => read_settings_file(&path),
        Err(e) => {
            eprintln!("설정 경로 조회 실패, 기본값 사용: {e}");
            Settings::default()
        }
    }
}

/// 설정을 저장한다.
pub fn save_settings(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app)?;
    write_settings_file(&path, settings)
}

/// 자동 업데이트 확인 설정을 변경·저장하고 메뉴 체크 상태를 갱신한다.
/// 메뉴의 토글 항목과 set_auto_check_updates 커맨드가 공통으로 사용한다.
pub fn update_auto_check_setting(app: &AppHandle, enabled: bool) -> Result<(), String> {
    save_settings(
        app,
        &Settings {
            auto_check_updates: enabled,
        },
    )?;
    // 체크 항목 상태 반영을 위해 네이티브 메뉴를 재구성한다
    let recent = app.state::<crate::state::AppState>().recent_files();
    crate::menu::apply_menu(app, &recent).map_err(|e| e.to_string())
}

/// 현재 앱 설정을 반환한다.
#[tauri::command]
pub async fn get_settings(app: tauri::AppHandle) -> Result<Settings, String> {
    Ok(load_settings(&app))
}

/// 자동 업데이트 확인 설정을 변경한다.
#[tauri::command]
pub async fn set_auto_check_updates(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    update_auto_check_setting(&app, enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_default() {
        assert!(Settings::default().auto_check_updates);
    }

    #[test]
    fn test_settings_serde_roundtrip() {
        let settings = Settings {
            auto_check_updates: false,
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(json, r#"{"autoCheckUpdates":false}"#);
        let parsed: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, settings);
    }

    #[test]
    fn test_settings_missing_field_uses_default() {
        // 필드가 없는 JSON도 기본값(true)으로 채워진다
        let parsed: Settings = serde_json::from_str("{}").unwrap();
        assert!(parsed.auto_check_updates);
    }

    #[test]
    fn test_read_missing_file_returns_default() {
        let path = crate::testutil::temp_log_path("settings_missing");
        assert_eq!(read_settings_file(&path), Settings::default());
    }

    #[test]
    fn test_read_corrupted_file_returns_default() {
        let path = crate::testutil::temp_log_path("settings_corrupt");
        std::fs::write(&path, b"{ this is not json !!!").unwrap();
        assert_eq!(read_settings_file(&path), Settings::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_write_then_read_roundtrip() {
        let path = crate::testutil::temp_log_path("settings_rw");
        let settings = Settings {
            auto_check_updates: false,
        };
        write_settings_file(&path, &settings).unwrap();
        assert_eq!(read_settings_file(&path), settings);
        // 임시 파일이 남지 않는다
        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(".tmp");
        assert!(!Path::new(&tmp_name).exists());
        let _ = std::fs::remove_file(&path);
    }
}
