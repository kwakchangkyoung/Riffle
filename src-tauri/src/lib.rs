// Riffle Tauri 애플리케이션 진입점

pub mod commands;
pub mod state;

use commands::{
    cancel_search, close_file, get_filtered_lines, get_lines, open_file, search, search_density,
    search_hit_at, search_nav, validate_query,
};
use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
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
            search_density
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 애플리케이션 실행 중 오류가 발생했습니다");
}
