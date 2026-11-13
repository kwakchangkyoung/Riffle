// 줄 북마크 저장 (app_data_dir/bookmarks.json)
// recent.json과 같은 방식: 전역 Mutex 직렬화 + 임시 파일 rename 원자적 저장.
// 파일이 없거나 JSON이 깨져 있으면 빈 상태로 처리한다.
//
// bookmarks.json 형식:
//   { "<경로/URI>": { "fingerprint": "hex", "size": 크기, "updatedAt": 초, "items": [...] } }
// 키는 파일을 식별하는 경로/URI 문자열이다 (로컬은 canonical 경로, 원격은 sftp://·s3://).
// fingerprint는 파일 앞 64KB의 FNV-1a 64비트 해시로, 저장 값과 다르면 stale=true를
// 반환해 "파일 내용이 바뀌어 북마크 위치가 다를 수 있다"를 프론트에 알린다
// (북마크 자체는 지우지 않는다).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::state::AppState;

/// 북마크를 보관하는 파일 항목의 최대 개수
pub const MAX_BOOKMARK_FILES: usize = 500;

/// 북마크 읽기-수정-쓰기를 직렬화하는 전역 락
static BOOKMARKS_LOCK: Mutex<()> = Mutex::new(());

/// 북마크 관련 읽기-수정-쓰기를 하나의 락으로 직렬화한다.
pub(crate) fn with_bookmarks_lock<R>(f: impl FnOnce() -> R) -> R {
    let _guard = BOOKMARKS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    f()
}

/// 북마크 항목 DTO (IPC와 저장 파일 양쪽에 사용)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkDto {
    /// 0부터 시작하는 줄 번호
    pub line: u64,
    /// 메모
    pub note: String,
    /// 표시 색상 ("#rrggbb")
    pub color: String,
    /// 생성 시각 (unix 초)
    pub created_at: u64,
}

/// 북마크 조회 결과 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarksDto {
    pub items: Vec<BookmarkDto>,
    /// 저장 이후 파일 내용이 바뀌어 북마크 위치가 어긋났을 수 있음
    pub stale: bool,
}

/// bookmarks.json에 저장되는 파일별 항목
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkEntry {
    /// 저장 당시 머리 지문 (FNV-1a 64비트, 16진수 문자열)
    pub fingerprint: String,
    /// 저장 당시 파일 크기 (바이트)
    pub size: u64,
    /// 마지막 수정 시각 (unix 초, 항목 수 상한 정리에 사용)
    pub updated_at: u64,
    /// 북마크 목록 (줄 번호 오름차순)
    pub items: Vec<BookmarkDto>,
}

/// bookmarks.json 파일 경로
fn bookmarks_file_path(dir: &Path) -> PathBuf {
    dir.join("bookmarks.json")
}

/// 북마크 저장 디렉터리 (app_data_dir)를 반환한다.
fn bookmarks_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("앱 데이터 디렉터리 조회 실패: {e}"))
}

/// 현재 unix 시각(초)을 반환한다.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 지문을 저장용 16진수 문자열로 변환한다.
fn fingerprint_hex(fp: u64) -> String {
    format!("{fp:016x}")
}

/// bookmarks.json을 읽어 경로→항목 맵으로 반환한다.
/// 파일이 없거나 JSON 파싱에 실패하면 빈 맵.
fn read_bookmarks_file(path: &Path) -> HashMap<String, BookmarkEntry> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 맵을 bookmarks.json에 원자적으로 저장한다.
/// 임시 파일에 쓴 뒤 rename으로 교체해 중간에 죽어도 파일이 깨지지 않게 한다.
fn write_bookmarks_file(path: &Path, map: &HashMap<String, BookmarkEntry>) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("북마크 디렉터리 생성 실패 ('{}'): {e}", dir.display()))?;
    }
    let json = serde_json::to_string(map).map_err(|e| format!("북마크 직렬화 실패: {e}"))?;
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    std::fs::write(&tmp_path, json)
        .map_err(|e| format!("북마크 임시 저장 실패 ('{}'): {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("북마크 저장 실패 ('{}'): {e}", path.display())
    })
}

/// 북마크 목록을 줄 번호 오름차순으로 정렬하고 같은 줄의 중복을 제거한다.
/// 같은 줄이 여러 개면 먼저 온 항목을 남긴다.
pub(crate) fn normalize_items(mut items: Vec<BookmarkDto>) -> Vec<BookmarkDto> {
    items.sort_by_key(|b| b.line);
    items.dedup_by_key(|b| b.line);
    items
}

/// path에 저장된 북마크를 반환한다.
/// 저장된 지문이 다르거나 현재 크기가 저장된 크기보다 작으면 stale=true
/// (북마크는 지우지 않는다). 로그는 대부분 뒤에 계속 추가되므로
/// 크기가 커진 것만으로는 stale로 보지 않는다 (앞 64KB 지문이 같으면 같은 파일의 연속).
/// 항목이 없으면 빈 목록과 stale=false.
pub fn load_bookmarks(
    dir: &Path,
    path: &str,
    fingerprint: u64,
    size: u64,
) -> Result<BookmarksDto, String> {
    let map = read_bookmarks_file(&bookmarks_file_path(dir));
    let Some(entry) = map.get(path) else {
        return Ok(BookmarksDto {
            items: Vec::new(),
            stale: false,
        });
    };
    let stale = entry.fingerprint != fingerprint_hex(fingerprint) || size < entry.size;
    Ok(BookmarksDto {
        items: entry.items.clone(),
        stale,
    })
}

/// path의 북마크 목록을 저장하고 지문·크기를 현재 값으로 갱신한다.
/// items가 비어 있으면 해당 항목을 삭제한다.
/// 파일 항목이 MAX_BOOKMARK_FILES개를 넘으면 마지막 수정 시각이 오래된 것부터 제거한다.
pub fn save_bookmarks(
    dir: &Path,
    path: &str,
    fingerprint: u64,
    size: u64,
    items: Vec<BookmarkDto>,
) -> Result<(), String> {
    let file = bookmarks_file_path(dir);
    let mut map = read_bookmarks_file(&file);
    if items.is_empty() {
        map.remove(path);
    } else {
        map.insert(
            path.to_string(),
            BookmarkEntry {
                fingerprint: fingerprint_hex(fingerprint),
                size,
                updated_at: now_secs(),
                items: normalize_items(items),
            },
        );
        while map.len() > MAX_BOOKMARK_FILES {
            let Some(oldest) = map
                .iter()
                .min_by_key(|(_, e)| e.updated_at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            map.remove(&oldest);
        }
    }
    write_bookmarks_file(&file, &map)
}

/// 열린 파일의 북마크를 조회한다.
/// 파일 앞 64KB의 지문을 계산해 저장된 값과 비교하므로,
/// 원격 파일은 네트워크 읽기가 발생한다 (spawn_blocking에서 수행).
#[tauri::command]
pub async fn get_bookmarks(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: u32,
) -> Result<BookmarksDto, String> {
    let (file, key) = state.get_file_with_path(id)?;
    let dir = bookmarks_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        let fingerprint = riffle_core::head_fingerprint(&file)
            .map_err(|e| format!("파일 지문 계산 실패: {e}"))?;
        let size = file.source().len();
        with_bookmarks_lock(|| load_bookmarks(&dir, &key, fingerprint, size))
    })
    .await
    .map_err(|e| format!("북마크 조회 비동기 태스크 실패: {e}"))?
}

/// 열린 파일의 북마크 목록을 저장한다.
/// 줄 번호 오름차순 정렬·중복 제거 후 현재 지문/크기와 함께 기록해 stale 상태를 해소한다.
/// 빈 목록이면 해당 파일의 북마크 항목을 삭제한다.
#[tauri::command]
pub async fn set_bookmarks(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: u32,
    items: Vec<BookmarkDto>,
) -> Result<(), String> {
    let (file, key) = state.get_file_with_path(id)?;
    let dir = bookmarks_dir(&app)?;
    tauri::async_runtime::spawn_blocking(move || {
        // 빈 목록은 항목 삭제뿐이므로 지문을 계산하지 않는다 (원격 네트워크 읽기 절약)
        let (fingerprint, size) = if items.is_empty() {
            (0, 0)
        } else {
            let fp = riffle_core::head_fingerprint(&file)
                .map_err(|e| format!("파일 지문 계산 실패: {e}"))?;
            (fp, file.source().len())
        };
        with_bookmarks_lock(|| save_bookmarks(&dir, &key, fingerprint, size, items))
    })
    .await
    .map_err(|e| format!("북마크 저장 비동기 태스크 실패: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "riffle_bookmark_test_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn item(line: u64) -> BookmarkDto {
        BookmarkDto {
            line,
            note: format!("note {line}"),
            color: "#ffaa00".to_string(),
            created_at: 1_700_000_000 + line,
        }
    }

    #[test]
    fn test_bookmark_save_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = "/var/log/app.log";

        // 항목이 없으면 빈 목록 + stale=false
        let got = load_bookmarks(&dir, path, 1234, 100).unwrap();
        assert!(got.items.is_empty());
        assert!(!got.stale);

        let items = vec![item(10), item(2), item(42)];
        save_bookmarks(&dir, path, 1234, 100, items.clone()).unwrap();

        let got = load_bookmarks(&dir, path, 1234, 100).unwrap();
        assert!(!got.stale);
        // 줄 번호 오름차순으로 정렬되어 저장된다
        let lines: Vec<u64> = got.items.iter().map(|b| b.line).collect();
        assert_eq!(lines, vec![2, 10, 42]);
        assert_eq!(got.items[1].note, "note 10");

        // 지문이 다르면 stale=true (북마크는 지워지지 않는다)
        let got = load_bookmarks(&dir, path, 5678, 100).unwrap();
        assert!(got.stale);
        assert_eq!(got.items.len(), 3);
        // 같은 지문 + 크기 증가(append)는 stale이 아니다
        let got = load_bookmarks(&dir, path, 1234, 200).unwrap();
        assert!(!got.stale);
        assert_eq!(got.items.len(), 3);
        // 같은 지문 + 크기 감소는 내용이 바뀐 것이므로 stale=true
        let got = load_bookmarks(&dir, path, 1234, 50).unwrap();
        assert!(got.stale);

        // 새 지문으로 다시 저장하면 stale이 해소된다
        save_bookmarks(&dir, path, 5678, 200, vec![item(5)]).unwrap();
        let got = load_bookmarks(&dir, path, 5678, 200).unwrap();
        assert!(!got.stale);
        assert_eq!(got.items.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_bookmark_normalize_dedup() {
        let items = vec![item(5), item(1), item(5), item(3), item(1)];
        let norm = normalize_items(items);
        let lines: Vec<u64> = norm.iter().map(|b| b.line).collect();
        assert_eq!(lines, vec![1, 3, 5]);
    }

    #[test]
    fn test_bookmark_empty_items_removes_entry() {
        let dir = temp_dir("remove");
        let path = "/a.log";

        save_bookmarks(&dir, path, 1, 10, vec![item(1)]).unwrap();
        assert_eq!(load_bookmarks(&dir, path, 1, 10).unwrap().items.len(), 1);

        // 빈 목록 저장 → 항목 삭제
        save_bookmarks(&dir, path, 0, 0, Vec::new()).unwrap();
        let got = load_bookmarks(&dir, path, 1, 10).unwrap();
        assert!(got.items.is_empty());
        assert!(!got.stale);
        // 파일에도 항목이 남지 않는다
        let map = read_bookmarks_file(&bookmarks_file_path(&dir));
        assert!(!map.contains_key(path));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_bookmark_max_files_eviction() {
        let dir = temp_dir("evict");
        let file = bookmarks_file_path(&dir);

        // 서로 다른 수정 시각으로 상한 개수만큼 직접 채운다
        let mut map = HashMap::new();
        for i in 0..MAX_BOOKMARK_FILES {
            map.insert(
                format!("/f{i}.log"),
                BookmarkEntry {
                    fingerprint: fingerprint_hex(i as u64),
                    size: i as u64,
                    updated_at: i as u64,
                    items: vec![item(0)],
                },
            );
        }
        write_bookmarks_file(&file, &map).unwrap();

        // 새 항목 저장 → 가장 오래된 /f0.log가 제거되고 개수는 상한을 유지한다
        save_bookmarks(&dir, "/new.log", 9, 9, vec![item(1)]).unwrap();
        let map = read_bookmarks_file(&file);
        assert_eq!(map.len(), MAX_BOOKMARK_FILES);
        assert!(!map.contains_key("/f0.log"));
        assert!(map.contains_key("/f1.log"));
        assert!(map.contains_key("/new.log"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_bookmark_corrupt_file_recovers() {
        let dir = temp_dir("corrupt");
        let file = bookmarks_file_path(&dir);
        std::fs::write(&file, b"{not valid json!!!").unwrap();

        // 손상된 파일은 빈 상태로 처리된다
        let got = load_bookmarks(&dir, "/a.log", 1, 1).unwrap();
        assert!(got.items.is_empty());
        assert!(!got.stale);

        // 손상 파일 위에 저장하면 정상 상태로 복구된다
        save_bookmarks(&dir, "/a.log", 1, 1, vec![item(7)]).unwrap();
        let got = load_bookmarks(&dir, "/a.log", 1, 1).unwrap();
        assert_eq!(got.items.len(), 1);
        assert!(!got.stale);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_bookmark_dto_serde_camel_case() {
        let b = BookmarkDto {
            line: 3,
            note: "n".to_string(),
            color: "#112233".to_string(),
            created_at: 42,
        };
        let json = serde_json::to_string(&b).unwrap();
        assert_eq!(
            json,
            r##"{"line":3,"note":"n","color":"#112233","createdAt":42}"##
        );
        let parsed: BookmarkDto = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, b);
    }
}
