// Riffle Tauri 애플리케이션 진입점

pub mod commands;
pub mod state;

use commands::{close_file, get_lines, open_file};
use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![open_file, get_lines, close_file])
        .run(tauri::generate_context!())
        .expect("Tauri 애플리케이션 실행 중 오류가 발생했습니다");
}
