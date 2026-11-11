// Riffle Tauri 애플리케이션 진입점

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("Tauri 애플리케이션 실행 중 오류가 발생했습니다");
}
