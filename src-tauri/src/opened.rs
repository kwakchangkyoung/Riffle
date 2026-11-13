// macOS 파일 연결: Finder "다음으로 열기"/더블클릭/다중 선택으로 전달된 file:// URL 처리

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

/// file:// URL 목록을 로컬 파일 경로 문자열 목록으로 변환한다.
/// 순서를 유지하고 file 스킴이 아닌 URL은 제외한다.
pub fn file_urls_to_paths(urls: &[Url]) -> Vec<String> {
    urls.iter().filter_map(file_url_to_path).collect()
}

/// RunEvent::Opened 수신 시 호출된다.
/// 전달된 모든 파일 경로를 순서대로 처리한다.
/// 프론트 준비 전에는 pending_open에 누적해 두고 프론트가 take_pending_open으로
/// 가져가며, 준비 후에는 "open-files" 이벤트(payload: Vec<String>)를 emit한다.
pub fn handle_run_event(app: &AppHandle, event: RunEvent) {
    if let RunEvent::Opened { urls } = event {
        let paths = file_urls_to_paths(&urls);
        if paths.is_empty() {
            return;
        }
        let state = app.state::<AppState>();
        if state.is_front_ready() {
            // 프론트 준비 완료: 리스너가 살아 있으므로 emit만 한다.
            // pending_open에 저장하면 이후 take_pending_open과 중복 열기가 생긴다.
            if let Err(e) = app.emit("open-files", paths) {
                eprintln!("open-files 이벤트 전송 실패: {e}");
            }
        } else {
            // 콜드 스타트: 프론트 리스너 등록 전일 수 있으므로 pending_open에 누적한다.
            state.push_pending_open(paths);
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

    #[test]
    fn test_file_urls_to_paths_multiple() {
        // 여러 file URL이 순서대로 변환되고 file 스킴이 아닌 것은 제외된다
        let urls = vec![
            Url::parse("file:///tmp/a.log").unwrap(),
            Url::parse("https://example.com/x").unwrap(),
            Url::parse("file:///tmp/b%20c.log").unwrap(),
            Url::parse("file:///tmp/한글.log").unwrap(),
        ];
        assert_eq!(
            file_urls_to_paths(&urls),
            vec![
                "/tmp/a.log".to_string(),
                "/tmp/b c.log".to_string(),
                "/tmp/한글.log".to_string(),
            ]
        );
    }

    #[test]
    fn test_file_urls_to_paths_empty_and_non_file() {
        assert!(file_urls_to_paths(&[]).is_empty());
        let urls = vec![Url::parse("https://example.com/x").unwrap()];
        assert!(file_urls_to_paths(&urls).is_empty());
    }
}
