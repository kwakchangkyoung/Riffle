// Riffle Tauri 애플리케이션 진입점

pub mod commands;
pub mod menu;
#[cfg(target_os = "macos")]
pub mod opened;
pub mod recent;
pub mod state;
pub mod tail;
#[cfg(test)]
mod testutil;

use commands::{
    cancel_search, close_file, copy_lines, get_filtered_lines, get_full_line, get_lines,
    get_recent_files, open_file, search, search_density, search_hit_at, search_nav,
    take_pending_open, validate_query,
};
use state::AppState;
use tail::set_tail;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            open_file,
            close_file,
            get_lines,
            get_filtered_lines,
            validate_query,
            search,
            cancel_search,
            search_nav,
            search_hit_at,
            search_density,
            set_tail,
            copy_lines,
            get_full_line,
            get_recent_files,
            take_pending_open
        ])
        .setup(|app| {
            // 이전 실행이 남긴 gzip 해제 임시 파일을 정리한다 (오류 무시)
            riffle_core::cleanup_temp_dir();
            // 최근 파일 목록을 상태에 로드하고 네이티브 메뉴를 구성한다
            let handle = app.handle().clone();
            let recent = recent::load_recent(&handle).unwrap_or_default();
            app.state::<AppState>().set_recent_files(recent.clone());
            menu::apply_menu(&handle, &recent)?;
            Ok(())
        })
        .on_menu_event(menu::handle_menu_event)
        .build(tauri::generate_context!())
        .expect("Tauri 애플리케이션 빌드 중 오류가 발생했습니다");

    // macOS Finder 파일 연결(RunEvent::Opened)을 처리한다
    app.run(|handle, event| {
        #[cfg(target_os = "macos")]
        opened::handle_run_event(handle, event);
        #[cfg(not(target_os = "macos"))]
        let _ = (handle, event);
    });
}
