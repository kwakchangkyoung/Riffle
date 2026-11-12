// macOS 파일 연결: Finder "다음으로 열기"/더블클릭으로 전달된 file:// URL 처리

use tauri::{AppHandle, Emitter, Manager, RunEvent, Url};

use crate::state::AppState;

/// file:// URL을 로컬 파일 경로 문자열로 변환한다.
/// 퍼센트 인코딩(공백, 한글 등)을 디코딩하며, file 스킴이 아니거나
/// 경로로 변환할 수 없으면 None을 반환한다.
pub fn file_url_to_path(url: &Url) -> Option<String> {
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// RunEvent::Opened 수신 시 호출된다.
/// 여러 파일이 한 번에 오면 마지막 것만 열기 위해 경로를 보관(pending_open)하고
/// 프론트엔드에 "open-file" 이벤트를 emit한다.
/// 콜드 스타트에서는 프론트 리스너 등록 전에 이 이벤트가 올 수 있으므로
/// pending_open에 함께 저장해 두고 프론트가 take_pending_open으로 가져간다.
pub fn handle_run_event(app: &AppHandle, event: RunEvent) {
    if let RunEvent::Opened { urls } = event {
        let Some(path) = urls.iter().rev().find_map(file_url_to_path) else {
            return;
        };
        let state = app.state::<AppState>();
        if state.is_front_ready() {
            // 프론트 준비 완료: 리스너가 살아 있으므로 emit만 한다.
            // pending_open에 저장하면 이후 take_pending_open과 중복 열기가 생긴다.
            if let Err(e) = app.emit("open-file", path) {
                eprintln!("open-file 이벤트 전송 실패: {e}");
            }
        } else {
            // 콜드 스타트: 프론트 리스너 등록 전일 수 있으므로 저장 + emit 둘 다 한다.
            state.set_pending_open(path.clone());
            if let Err(e) = app.emit("open-file", path) {
                eprintln!("open-file 이벤트 전송 실패: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_url_to_path_basic() {
        let url = Url::parse("file:///tmp/app.log").unwrap();
        assert_eq!(file_url_to_path(&url).as_deref(), Some("/tmp/app.log"));
    }

    #[test]
    fn test_file_url_to_path_percent_encoded() {
        // 공백과 퍼센트 인코딩된 문자
        let url = Url::parse("file:///tmp/my%20log%20%ED%95%9C%EA%B8%80.log").unwrap();
        assert_eq!(
            file_url_to_path(&url).as_deref(),
            Some("/tmp/my log 한글.log")
        );
    }

    #[test]
    fn test_file_url_to_path_raw_utf8() {
        // 한글이 퍼센트 인코딩되지 않고 그대로 들어온 URL도 디코딩된다
        let url = Url::parse("file:///tmp/한글 파일.txt").unwrap();
        assert_eq!(
            file_url_to_path(&url).as_deref(),
            Some("/tmp/한글 파일.txt")
        );
    }

    #[test]
    fn test_file_url_to_path_non_file_scheme() {
        let url = Url::parse("https://example.com/a.log").unwrap();
        assert_eq!(file_url_to_path(&url), None);
    }
}
