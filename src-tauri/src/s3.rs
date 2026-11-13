// S3 객체 열기 내부 로직과 S3 탐색·즐겨찾기 IPC 커맨드

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use riffle_core::LogFile;
use riffle_remote::S3Uri;
use serde::Serialize;
use tauri::{AppHandle, Manager};

use crate::commands::{FileInfo, OpenedFile};
use crate::state::AppState;

/// S3 접두사 아래 항목 DTO (프론트 ipc.ts의 S3Entry와 1:1)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct S3EntryDto {
    /// 접두사 기준 상대 이름 (폴더는 끝에 '/' 없이)
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 수정 시각 (unix 초, 폴더는 None)
    pub mtime: Option<u64>,
}

/// s3:// URI 객체를 열어 OpenedFile을 반환한다.
/// 프로필은 URI가 아니라 AppState의 s3_profile(마지막 선택)을 사용한다.
/// close 시 별도 해제가 필요 없다 — S3Pool의 클라이언트는 캐시에서 재사용된다.
pub fn open_s3_internal(state: &AppState, path: &str) -> Result<OpenedFile, String> {
    let uri = S3Uri::parse(path).ok_or_else(|| {
        format!("잘못된 S3 경로입니다: '{path}' (s3://버킷/키 형식이어야 합니다)")
    })?;
    let profile = state.s3_profile();
    let reader = state
        .s3_pool
        .open_object(profile.as_deref(), &uri)
        .map_err(|e| format!("S3 객체를 열 수 없습니다 ('{path}'): {e}"))?;
    // FileInfo.size는 원격의 압축 크기(gz면 압축 바이트 수)를 표시한다
    let size = reader
        .stat_len()
        .map_err(|e| format!("S3 객체 크기를 확인할 수 없습니다 ('{path}'): {e}"))?;
    let gz = riffle_core::is_gzip_reader(reader.as_ref())
        .map_err(|e| format!("S3 객체를 열 수 없습니다 ('{path}'): {e}"))?;

    let (log_file, gz_writer) = if gz {
        let (file, writer) = riffle_core::open_gzip_remote(reader)
            .map_err(|e| format!("S3 gzip 객체를 열 수 없습니다 ('{path}'): {e}"))?;
        (file, Some(writer))
    } else {
        let file = LogFile::open_remote(reader)
            .map_err(|e| format!("S3 객체를 열 수 없습니다 ('{path}'): {e}"))?;
        (file, None)
    };

    let file = Arc::new(log_file);
    let cancel = Arc::new(AtomicBool::new(false));
    let compressed = gz_writer.is_some();
    let id = state.insert_file(
        file.clone(),
        cancel.clone(),
        PathBuf::from(path),
        compressed,
    );

    Ok(OpenedFile {
        info: FileInfo {
            id,
            path: path.to_string(),
            size,
            compressed,
            remote: true,
            source: "s3".to_string(),
        },
        file,
        cancel,
        gz_writer,
    })
}

/// ~/.aws/config(+ ~/.aws/credentials)의 AWS 프로필 이름 목록을 반환한다.
/// 프로필 이름(섹션 헤더)만 읽고 자격 증명 값은 읽지 않는다.
#[tauri::command]
pub async fn list_aws_profiles() -> Vec<String> {
    match tauri::async_runtime::spawn_blocking(riffle_remote::list_aws_profiles).await {
        Ok(list) => list,
        Err(e) => {
            eprintln!("AWS 프로필 목록 조회 비동기 태스크 실패: {e}");
            Vec::new()
        }
    }
}

/// ListBuckets로 계정의 버킷 이름 목록을 반환한다.
/// s3:ListAllMyBuckets 권한이 없으면 오류를 반환하며,
/// 프론트는 이 경우 버킷 이름 직접 입력을 안내한다.
#[tauri::command]
pub async fn s3_list_buckets(
    app: tauri::AppHandle,
    profile: Option<String>,
) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state
            .s3_pool
            .list_buckets(profile.as_deref())
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("S3 버킷 목록 조회 비동기 태스크 실패: {e}"))?
}

/// 버킷의 접두사 아래 항목을 '/' 구분자로 나열한다 (폴더 먼저, 이름순).
#[tauri::command]
pub async fn s3_list_prefix(
    app: tauri::AppHandle,
    profile: Option<String>,
    bucket: String,
    prefix: String,
) -> Result<Vec<S3EntryDto>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state
            .s3_pool
            .list_prefix(profile.as_deref(), &bucket, &prefix)
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|e| S3EntryDto {
                        name: e.name,
                        is_dir: e.is_dir,
                        size: e.size,
                        mtime: e.mtime,
                    })
                    .collect()
            })
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("S3 접두사 목록 조회 비동기 태스크 실패: {e}"))?
}

/// HeadObject로 S3 객체의 크기(바이트)를 반환한다.
#[tauri::command]
pub async fn s3_head(
    app: tauri::AppHandle,
    profile: Option<String>,
    uri: String,
) -> Result<u64, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let parsed = S3Uri::parse(&uri).ok_or_else(|| {
            format!("잘못된 S3 경로입니다: '{uri}' (s3://버킷/키 형식이어야 합니다)")
        })?;
        let state = app.state::<AppState>();
        state
            .s3_pool
            .head(profile.as_deref(), &parsed)
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("S3 객체 정보 조회 비동기 태스크 실패: {e}"))?
}

/// 마지막으로 선택한 AWS 프로필을 설정에 저장하고 AppState에 반영한다.
/// None이면 기본 자격 증명 체인을 사용한다.
#[tauri::command]
pub async fn set_s3_profile(app: tauri::AppHandle, profile: Option<String>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut settings = crate::settings::load_settings(&app);
        settings.s3_profile = profile.clone();
        crate::settings::save_settings(&app, &settings)?;
        app.state::<AppState>().set_s3_profile(profile);
        Ok(())
    })
    .await
    .map_err(|e| format!("S3 프로필 저장 비동기 태스크 실패: {e}"))?
}

/// s3-bookmarks.json 파일 경로
fn bookmarks_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("s3-bookmarks.json"))
        .map_err(|e| format!("앱 데이터 디렉터리 조회 실패: {e}"))
}

/// s3-bookmarks.json을 읽어 즐겨찾기 목록을 반환한다.
/// 항목은 "bucket" 또는 "bucket/prefix/" 형태이며,
/// 파일이 없거나 JSON 파싱에 실패하면 빈 목록을 반환한다.
fn read_bookmarks_file(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 즐겨찾기 목록을 s3-bookmarks.json에 원자적으로 저장한다.
/// 임시 파일에 쓴 뒤 rename으로 교체해 중간에 죽어도 파일이 깨지지 않게 한다.
fn write_bookmarks_file(path: &Path, items: &[String]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("즐겨찾기 디렉터리 생성 실패 ('{}'): {e}", dir.display()))?;
    }
    let json = serde_json::to_string(items).map_err(|e| e.to_string())?;
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    std::fs::write(&tmp_path, json)
        .map_err(|e| format!("즐겨찾기 임시 저장 실패 ('{}'): {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("즐겨찾기 저장 실패 ('{}'): {e}", path.display())
    })
}

/// 저장된 S3 버킷/접두사 즐겨찾기 목록을 반환한다.
#[tauri::command]
pub async fn get_s3_bookmarks(app: tauri::AppHandle) -> Vec<String> {
    match tauri::async_runtime::spawn_blocking(move || {
        let Ok(path) = bookmarks_path(&app) else {
            return Vec::new();
        };
        read_bookmarks_file(&path)
    })
    .await
    {
        Ok(list) => list,
        Err(e) => {
            eprintln!("S3 즐겨찾기 조회 비동기 태스크 실패: {e}");
            Vec::new()
        }
    }
}

/// S3 버킷/접두사 즐겨찾기 목록을 저장한다.
#[tauri::command]
pub async fn set_s3_bookmarks(app: tauri::AppHandle, items: Vec<String>) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let path = bookmarks_path(&app)?;
        write_bookmarks_file(&path, &items)
    })
    .await
    .map_err(|e| format!("S3 즐겨찾기 저장 비동기 태스크 실패: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::OpenFile;
    use std::sync::Mutex;

    #[test]
    fn test_bookmarks_roundtrip() {
        let path = crate::testutil::temp_log_path("s3_bookmarks_rw");
        let items = vec!["my-bucket".to_string(), "my-bucket/logs/prod/".to_string()];
        write_bookmarks_file(&path, &items).unwrap();
        assert_eq!(read_bookmarks_file(&path), items);
        // 임시 파일이 남지 않는다
        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(".tmp");
        assert!(!Path::new(&tmp_name).exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_bookmarks_missing_returns_empty() {
        let path = crate::testutil::temp_log_path("s3_bookmarks_missing");
        assert_eq!(read_bookmarks_file(&path), Vec::<String>::new());
    }

    #[test]
    fn test_bookmarks_corrupted_returns_empty() {
        let path = crate::testutil::temp_log_path("s3_bookmarks_corrupt");
        std::fs::write(&path, b"{ this is not json !!!").unwrap();
        assert_eq!(read_bookmarks_file(&path), Vec::<String>::new());
        let _ = std::fs::remove_file(&path);
    }

    /// 임시 로컬 LogFile로 OpenFile을 등록하되 경로는 s3:// URI로 둔다.
    /// enable_tail은 파일 내용과 무관하게 경로 접두사로 S3를 판별한다.
    fn insert_s3_path_file(state: &AppState, id: u32) -> PathBuf {
        let path = crate::testutil::temp_log_path("s3_tail");
        std::fs::write(&path, b"x\n").expect("임시 파일 쓰기 실패");
        let log = Arc::new(LogFile::open(&path).expect("LogFile::open 실패"));
        state.files.lock().unwrap().insert(
            id,
            OpenFile {
                doc: crate::state::Doc::File(log),
                cancel: Arc::new(AtomicBool::new(false)),
                path: PathBuf::from("s3://bucket/app.log"),
                tail: None,
                compressed: false,
                columns: Arc::new(Mutex::new(None)),
            },
        );
        path
    }

    #[test]
    fn test_enable_tail_rejects_s3_file() {
        let state = AppState::default();
        let local = insert_s3_path_file(&state, 1);

        // S3 객체는 불변이므로 tail 감시를 시작할 수 없다
        let result = crate::tail::enable_tail(&state, 1, |_| {});
        assert!(matches!(&result, Err(e) if e.contains("S3 객체는 tail")));

        let _ = std::fs::remove_file(&local);
    }

    #[test]
    fn test_open_file_routes_s3_uri_to_s3_pool() {
        // 닫힌 포트의 가짜 endpoint를 지정해 실제 AWS 접속 없이 라우팅만 확인한다.
        let state = AppState {
            s3_pool: riffle_remote::S3Pool::with_endpoint("http://127.0.0.1:1"),
            ..Default::default()
        };

        // s3:// URI는 로컬 경로가 아닌 S3Pool로 라우팅된다.
        // 연결은 실패하지만 에러가 S3 경유임을 확인한다.
        let result = crate::commands::open_file_internal(&state, "s3://bucket/app.log");
        let Err(err) = result else {
            panic!("닫힌 포트의 가짜 endpoint라 열기가 실패해야 합니다");
        };
        assert!(err.contains("S3"), "에러가 S3 라우팅을 가리켜야 함: {err}");
        assert_eq!(state.file_count(), 0);
    }

    #[test]
    fn test_is_remote_uri() {
        assert!(crate::commands::is_remote_uri("sftp://host/path"));
        assert!(crate::commands::is_remote_uri("s3://bucket/key"));
        assert!(!crate::commands::is_remote_uri("/local/path.log"));
        assert!(!crate::commands::is_remote_uri("s3x://not-s3"));
    }
}
