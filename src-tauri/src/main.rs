// 릴리스 빌드 시 Windows 콘솔 창 방지
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    riffle_lib::run();
}
