// Riffle Tauri 애플리케이션 진입점

pub mod bookmarks;
pub mod commands;
pub mod menu;
#[cfg(target_os = "macos")]
pub mod opened;
pub mod presets;
pub mod recent;
pub mod remote;
pub mod s3;
pub mod session;
pub mod settings;
pub mod state;
pub mod tail;
#[cfg(test)]
mod testutil;

use bookmarks::{get_bookmarks, set_bookmarks};
use commands::{
    cancel_filter, cancel_search, close_file, copy_lines, delete_filter_preset, detect_columns,
    filter_density, filter_index_of, get_filter, get_filter_lines, get_filtered_lines,
    get_full_line, get_lines, get_recent_files, list_filter_presets, open_file, save_filter_preset,
    search, search_density, search_hit_at, search_nav, start_filter, take_pending_open,
    validate_filter, validate_query,
};
use remote::{list_remote_dir, list_ssh_hosts, remote_home};
use s3::{
    get_s3_bookmarks, list_aws_profiles, s3_head, s3_list_buckets, s3_list_prefix,
    set_s3_bookmarks, set_s3_profile,
};
use session::{load_session, save_session};
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
            detect_columns,
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
            save_session,
            load_session,
            validate_filter,
            start_filter,
            cancel_filter,
            get_filter,
            get_filter_lines,
            filter_index_of,
            filter_density,
            list_filter_presets,
            save_filter_preset,
            delete_filter_preset,
            list_ssh_hosts,
            list_remote_dir,
            remote_home,
            list_aws_profiles,
            s3_list_buckets,
            s3_list_prefix,
            s3_head,
            set_s3_profile,
            get_s3_bookmarks,
            set_s3_bookmarks,
            get_bookmarks,
            set_bookmarks
        ])
        .setup(|app| {
            // 이전 실행이 남긴 gzip 해제 임시 파일을 정리한다 (오류 무시)
            riffle_core::cleanup_temp_dir();
            // 최근 파일 목록을 상태에 로드하고 네이티브 메뉴를 구성한다
            let handle = app.handle().clone();
            let recent = recent::load_recent(&handle).unwrap_or_default();
            app.state::<AppState>().set_recent_files(recent.clone());
            // 마지막으로 선택한 AWS 프로필을 설정에서 복원한다 (없으면 기본 체인)
            let s3_profile = settings::load_settings(&handle).s3_profile;
            app.state::<AppState>().set_s3_profile(s3_profile);
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
