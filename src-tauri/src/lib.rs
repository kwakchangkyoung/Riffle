// Riffle Tauri 애플리케이션 진입점

pub mod commands;
pub mod menu;
#[cfg(target_os = "macos")]
pub mod opened;
pub mod presets;
pub mod recent;
pub mod settings;
pub mod state;
pub mod tail;
#[cfg(test)]
mod testutil;

use commands::{
    cancel_filter, cancel_search, close_file, copy_lines, delete_filter_preset, filter_density,
    filter_index_of, get_filter, get_filter_lines, get_filtered_lines, get_full_line, get_lines,
    get_recent_files, list_filter_presets, open_file, save_filter_preset, search, search_density,
    search_hit_at, search_nav, start_filter, take_pending_open, validate_filter, validate_query,
};
use settings::{get_settings, set_auto_check_updates};
use state::AppState;
use tail::set_tail;
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
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
            take_pending_open,
            get_settings,
            set_auto_check_updates,
            validate_filter,
            start_filter,
            cancel_filter,
            get_filter,
            get_filter_lines,
            filter_index_of,
            filter_density,
            list_filter_presets,
            save_filter_preset,
            delete_filter_preset
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
