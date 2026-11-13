// macOS 네이티브 메뉴바 구성 및 메뉴 이벤트를 프론트엔드로 전달

use std::path::Path;

use serde::Serialize;
use tauri::menu::{
    AboutMetadata, CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu,
};
use tauri::{AppHandle, Emitter, Manager, Wry};

use crate::state::AppState;

/// 프론트엔드 "menu" 이벤트 페이로드 (프론트 ipc.ts의 MenuEvent와 1:1)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MenuEventPayload {
    /// 메뉴 항목 id (최근 파일 선택 시 "open_recent")
    pub id: String,
    /// recent:<index> 선택 시 해당 파일 경로
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// 최근 파일 서브메뉴 아이템 id 접두사 ("recent:<index>")
const RECENT_ID_PREFIX: &str = "recent:";

/// recent 목록으로 네이티브 메뉴를 구성해 앱에 설정한다.
/// 최근 파일이 바뀔 때마다 다시 호출해 "최근 파일" 서브메뉴를 재구성한다.
pub fn apply_menu(app: &AppHandle, recent: &[String]) -> tauri::Result<()> {
    app.set_menu(build_menu(app, recent)?).map(|_| ())
}

/// 앱 메뉴바 전체를 구성한다.
fn build_menu(app: &AppHandle, recent: &[String]) -> tauri::Result<Menu<Wry>> {
    // 파일 메뉴: 열기, 최근 파일, 탭 닫기, 창 닫기
    let open_item = MenuItem::with_id(app, "open", "열기…", true, Some("CmdOrCtrl+O"))?;
    let close_tab_item = MenuItem::with_id(app, "close_tab", "탭 닫기", true, Some("CmdOrCtrl+W"))?;
    // PredefinedMenuItem::close_window는 macOS에서 단축키가 ⌘W로 고정되어
    // 탭 닫기(⌘W)와 충돌하므로, 일반 항목 + 네이티브 close로 ⌘⇧W를 지원한다.
    let close_window_item = MenuItem::with_id(
        app,
        "close_window",
        "창 닫기",
        true,
        Some("CmdOrCtrl+Shift+W"),
    )?;

    // 최근 파일 서브메뉴: 비어 있으면 비활성 "(없음)" 항목 하나
    let recent_entries: Vec<MenuItem<Wry>> = if recent.is_empty() {
        vec![MenuItem::with_id(
            app,
            "recent:none",
            "(없음)",
            false,
            None::<&str>,
        )?]
    } else {
        recent
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let label = Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                MenuItem::with_id(
                    app,
                    format!("{RECENT_ID_PREFIX}{i}"),
                    label,
                    true,
                    None::<&str>,
                )
            })
            .collect::<tauri::Result<Vec<_>>>()?
    };
    let mut recent_refs: Vec<&dyn IsMenuItem<Wry>> = Vec::with_capacity(recent_entries.len());
    for item in &recent_entries {
        recent_refs.push(item);
    }
    let recent_menu = Submenu::with_items(app, "최근 파일", true, &recent_refs)?;

    let file_menu = Submenu::with_items(
        app,
        "파일",
        true,
        &[
            &open_item,
            &recent_menu,
            &PredefinedMenuItem::separator(app)?,
            &close_tab_item,
            &close_window_item,
        ],
    )?;

    // 편집 메뉴: 복사, 찾기 (모두 선택은 의도적으로 제외)
    let edit_menu = Submenu::with_items(
        app,
        "편집",
        true,
        &[
            &MenuItem::with_id(app, "copy", "복사", true, Some("CmdOrCtrl+C"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "find", "찾기…", true, Some("CmdOrCtrl+F"))?,
        ],
    )?;

    // 이동 메뉴
    let move_menu = Submenu::with_items(
        app,
        "이동",
        true,
        &[
            &MenuItem::with_id(app, "goto_line", "줄로 이동…", true, Some("CmdOrCtrl+L"))?,
            &MenuItem::with_id(app, "go_top", "처음으로", true, Some("CmdOrCtrl+Up"))?,
            &MenuItem::with_id(app, "go_bottom", "끝으로", true, Some("CmdOrCtrl+Down"))?,
        ],
    )?;

    // 보기 메뉴
    let view_menu = Submenu::with_items(
        app,
        "보기",
        true,
        &[
            &MenuItem::with_id(app, "toggle_tail", "Tail 모드", true, Some("CmdOrCtrl+T"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "font_inc", "글자 크게", true, Some("CmdOrCtrl+="))?,
            &MenuItem::with_id(app, "font_dec", "글자 작게", true, Some("CmdOrCtrl+-"))?,
            &MenuItem::with_id(app, "font_reset", "기본 크기", true, Some("CmdOrCtrl+0"))?,
        ],
    )?;

    // 윈도우 메뉴: 탭 이동
    let window_menu = Submenu::with_items(
        app,
        "윈도우",
        true,
        &[
            &MenuItem::with_id(app, "next_tab", "다음 탭", true, Some("CmdOrCtrl+Shift+]"))?,
            &MenuItem::with_id(app, "prev_tab", "이전 탭", true, Some("CmdOrCtrl+Shift+["))?,
        ],
    )?;

    let mut items: Vec<&dyn IsMenuItem<Wry>> =
        vec![&file_menu, &edit_menu, &move_menu, &view_menu, &window_menu];

    // macOS 앱 메뉴 (About, Services, Hide, Quit 등)
    #[cfg(target_os = "macos")]
    let app_menu = {
        let about_metadata = AboutMetadata {
            name: Some(app.package_info().name.clone()),
            version: Some(app.package_info().version.to_string()),
            copyright: app.config().bundle.copyright.clone(),
            authors: app.config().bundle.publisher.clone().map(|p| vec![p]),
            ..Default::default()
        };
        // 업데이트 확인 항목과 시작 시 자동 확인 체크 항목 (About 바로 아래)
        let check_updates_item =
            MenuItem::with_id(app, "check_updates", "업데이트 확인…", true, None::<&str>)?;
        let settings = crate::settings::load_settings(app);
        let auto_update_item = CheckMenuItem::with_id(
            app,
            "toggle_auto_update",
            "시작 시 업데이트 자동 확인",
            true,
            settings.auto_check_updates,
            None::<&str>,
        )?;
        let restore_session_item = CheckMenuItem::with_id(
            app,
            "toggle_restore_session",
            "시작 시 이전 탭 복원",
            true,
            settings.restore_session,
            None::<&str>,
        )?;
        Submenu::with_items(
            app,
            app.package_info().name.clone(),
            true,
            &[
                &PredefinedMenuItem::about(app, None, Some(about_metadata))?,
                &PredefinedMenuItem::separator(app)?,
                &check_updates_item,
                &auto_update_item,
                &restore_session_item,
                &PredefinedMenuItem::separator(app)?,
                &PredefinedMenuItem::services(app, None)?,
                &PredefinedMenuItem::separator(app)?,
                &PredefinedMenuItem::hide(app, None)?,
                &PredefinedMenuItem::hide_others(app, None)?,
                &PredefinedMenuItem::show_all(app, None)?,
                &PredefinedMenuItem::separator(app)?,
                &PredefinedMenuItem::quit(app, None)?,
            ],
        )?
    };
    #[cfg(target_os = "macos")]
    items.insert(0, &app_menu);

    Menu::with_items(app, &items)
}

/// 메뉴 아이템 클릭을 프론트엔드 "menu" 이벤트로 전달한다.
/// recent:<index>는 AppState의 최근 파일 목록에서 경로를 찾아 id="open_recent"로 보낸다.
/// (PredefinedMenuItem은 네이티브 동작만 수행하고 이벤트를 발생시키지 않는다.)
pub fn handle_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    let id = event.id().0.as_str();

    // 자동 업데이트 확인 토글은 설정 저장과 메뉴 갱신까지 Rust에서 완결한다
    // (프론트엔드에는 이벤트를 보내지 않는다)
    if id == "toggle_auto_update" {
        let current = crate::settings::load_settings(app).auto_check_updates;
        if let Err(e) = crate::settings::update_auto_check_setting(app, !current) {
            eprintln!("업데이트 설정 변경 실패: {e}");
        }
        return;
    }

    // 탭 세션 복원 토글도 설정 저장과 메뉴 갱신까지 Rust에서 완결한다
    if id == "toggle_restore_session" {
        let current = crate::settings::load_settings(app).restore_session;
        if let Err(e) = crate::settings::update_restore_session_setting(app, !current) {
            eprintln!("세션 복원 설정 변경 실패: {e}");
        }
        return;
    }

    // 창 닫기는 네이티브로 처리한다 (프론트엔드 이벤트를 보내지 않는다)
    if id == "close_window" {
        if let Some(window) = app.get_webview_window("main")
            && let Err(e) = window.close()
        {
            eprintln!("창 닫기 실패: {e}");
        }
        return;
    }

    let payload = match id.strip_prefix(RECENT_ID_PREFIX) {
        Some(index_str) => {
            let Ok(index) = index_str.parse::<usize>() else {
                return;
            };
            // 메뉴 구성 이후 목록이 바뀐 경우 stale 항목이므로 무시한다
            let Some(path) = app.state::<AppState>().recent_files().get(index).cloned() else {
                return;
            };
            MenuEventPayload {
                id: "open_recent".to_string(),
                path: Some(path),
            }
        }
        None => MenuEventPayload {
            id: id.to_string(),
            path: None,
        },
    };

    if let Err(e) = app.emit("menu", payload) {
        eprintln!("메뉴 이벤트 전송 실패: {e}");
    }
}
