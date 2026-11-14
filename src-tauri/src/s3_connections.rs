// S3 저장 연결(이름+액세스 키) 관리.
//
// - 연결 목록은 app_data_dir/s3-connections.json 에 저장한다 (비밀 키는 절대 쓰지 않는다).
// - 비밀 키는 macOS 키체인 generic password 에 둔다 (service `com.gajago.riffle.s3`,
//   account = 연결 id). 다른 OS 빌드에서는 키체인 호출이 "지원하지 않습니다" 오류가 된다.
// - `s3://<연결id>@버킷/키` URI 로 파일을 열 때 Static 인증을 사용한다.
// - 읽기-수정-쓰기는 CONNS_LOCK 으로 직렬화하고, 쓰기는 임시 파일 후 rename 으로 원자적이다.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use riffle_remote::{S3Auth, S3StaticCreds};
use serde::{Deserialize, Serialize};
use tauri::Manager;

use crate::state::AppState;

/// 키체인 generic password 의 service 이름 (account 는 연결 id).
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "com.gajago.riffle.s3";

/// SecItem 의 "항목 없음" 오류 코드 (errSecItemNotFound).
#[cfg(target_os = "macos")]
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// s3-connections.json 한 항목이자 IPC DTO.
/// 비밀 키는 이 구조체에 없다 — JSON·프론트 어느 쪽에도 비밀 키가 나가지 않는다.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct S3Connection {
    /// `[a-z0-9]` 만 허용하는 고유 id (`s3://<id>@버킷/키` 의 `@` 앞 부분).
    pub id: String,
    /// 표시 이름 (trim 필수, 중복 허용).
    pub name: String,
    pub access_key_id: String,
    pub region: Option<String>,
    /// 기본 버킷 (선택 사항).
    pub bucket: Option<String>,
    /// 기본 접두사 (선택 사항).
    pub prefix: Option<String>,
    /// 마지막 사용 시각 (unix 초, 한 번도 안 썼으면 None).
    pub last_used: Option<u64>,
}

/// save_s3_connection 의 입력 DTO (프론트가 보내는 편집 내용).
/// 비밀 키가 들어 있으므로 Debug 를 구현하지 않는다 (로그 유출 방지).
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveS3ConnectionInput {
    /// 편집 대상 id. 새 연결이면 null.
    pub id: Option<String>,
    pub name: String,
    pub access_key_id: String,
    /// 새 연결은 필수. 편집 시 null/빈 문자열이면 기존 비밀 키 유지.
    pub secret_key: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub prefix: Option<String>,
}

/// 연결 목록 파일 읽기-수정-쓰기를 직렬화하는 전용 락.
static CONNS_LOCK: Mutex<()> = Mutex::new(());

/// s3-connections.json 파일 경로. AppState 에 setup 시 기록된다.
fn connections_path(state: &AppState) -> Result<PathBuf, String> {
    state
        .s3_connections_path()
        .ok_or_else(|| "S3 연결 저장 경로가 설정되지 않았습니다".to_string())
}

/// s3-connections.json 을 읽어 연결 목록을 반환한다.
/// 파일이 없거나 파싱에 실패하면 빈 목록을 반환한다.
fn read_connections_file(path: &Path) -> Vec<S3Connection> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// 연결 목록을 s3-connections.json 에 원자적으로 저장한다.
/// 임시 파일에 쓴 뒤 rename으로 교체해 중간에 죽어도 파일이 깨지지 않게 한다.
fn write_connections_file(path: &Path, conns: &[S3Connection]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("S3 연결 디렉터리 생성 실패 ('{}'): {e}", dir.display()))?;
    }
    let json = serde_json::to_string(conns).map_err(|e| e.to_string())?;
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    std::fs::write(&tmp_path, json)
        .map_err(|e| format!("S3 연결 임시 저장 실패 ('{}'): {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("S3 연결 저장 실패 ('{}'): {e}", path.display())
    })
}

/// 표시 순서 정렬: lastUsed 내림차순(없으면 뒤), 같으면 이름순.
fn sort_connections(conns: &mut [S3Connection]) {
    conns.sort_by(|a, b| {
        b.last_used
            .cmp(&a.last_used)
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// 목록 안에서 id 의 last_used 를 갱신한다. 대상이 있으면 true.
fn touch_last_used_in(conns: &mut [S3Connection], id: &str, now: u64) -> bool {
    if let Some(c) = conns.iter_mut().find(|c| c.id == id) {
        c.last_used = Some(now);
        true
    } else {
        false
    }
}

/// 연결 id 허용 문자 검사: `[a-z0-9]` 만 (s3:// URI 의 `@` 앞에 들어가므로 `@`·`/` 금지).
fn valid_connection_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// `c` + 소문자 16진 12자리 형태의 새 연결 id 를 만든다.
/// 시각·pid·카운터를 splitmix64 로 섞어 프로세스 안팎에서 충돌하지 않게 하고,
/// 기존 목록과 겹치면 다시 뽑는다.
fn new_connection_id(existing: &[S3Connection]) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut x = (nanos as u64)
            ^ ((nanos >> 64) as u64)
            ^ (std::process::id() as u64).rotate_left(17)
            ^ SEQ
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^= x >> 31;
        let id = format!("c{:012x}", x & 0x0000_FFFF_FFFF_FFFF);
        if !existing.iter().any(|c| c.id == id) {
            return id;
        }
    }
}

/// 현재 시각 (unix 초).
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 키체인 (macOS 만 실제 구현, 다른 OS 는 "지원하지 않습니다" 오류)
// ---------------------------------------------------------------------------

/// 비밀 키를 키체인에 저장한다 (같은 account 가 있으면 덮어쓴다).
#[cfg(target_os = "macos")]
fn keychain_set(id: &str, secret: &str) -> Result<(), String> {
    security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, id, secret.as_bytes())
        .map_err(|e| format!("비밀 키를 키체인에 저장하지 못했습니다: {e}"))
}

/// 키체인의 비밀 키를 읽는다. 항목이 없으면 Ok(None).
#[cfg(target_os = "macos")]
fn keychain_get(id: &str) -> Result<Option<String>, String> {
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, id) {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| "키체인의 비밀 키가 손상되었습니다".to_string()),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
        Err(e) => Err(format!("키체인에서 비밀 키를 읽지 못했습니다: {e}")),
    }
}

/// 키체인의 비밀 키를 지운다. 항목이 없어도 성공으로 본다.
#[cfg(target_os = "macos")]
fn keychain_delete(id: &str) -> Result<(), String> {
    match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, id) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
        Err(e) => Err(format!("키체인에서 비밀 키를 지우지 못했습니다: {e}")),
    }
}

#[cfg(not(target_os = "macos"))]
fn keychain_set(_id: &str, _secret: &str) -> Result<(), String> {
    Err("이 플랫폼에서는 S3 연결 비밀 키 저장을 지원하지 않습니다".to_string())
}

#[cfg(not(target_os = "macos"))]
fn keychain_get(_id: &str) -> Result<Option<String>, String> {
    Err("이 플랫폼에서는 S3 연결 비밀 키 저장을 지원하지 않습니다".to_string())
}

#[cfg(not(target_os = "macos"))]
fn keychain_delete(_id: &str) -> Result<(), String> {
    // 이 플랫폼에서는 저장할 수 없으므로 지울 것도 없다 (항목 없음 = 성공).
    Ok(())
}

// ---------------------------------------------------------------------------
// 연결 조회·자격 증명
// ---------------------------------------------------------------------------

/// 목록 파일에서 연결을 찾는다 (없으면 삭제 안내 오류).
fn find_connection(state: &AppState, id: &str) -> Result<S3Connection, String> {
    let path = connections_path(state)?;
    let conns = read_connections_file(&path);
    conns
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| format!("S3 연결을 찾을 수 없습니다 (삭제되었을 수 있습니다): {id}"))
}

/// 연결의 비밀 키를 키체인에서 읽는다 (없으면 재입력 안내 오류).
fn secret_for(conn: &S3Connection) -> Result<String, String> {
    keychain_get(&conn.id)?.ok_or_else(|| {
        format!(
            "S3 연결 '{}'의 비밀 키를 키체인에서 찾을 수 없습니다. 연결을 편집해 다시 입력하세요",
            conn.name
        )
    })
}

/// 연결 id 로 Static 자격 증명을 반환한다.
/// AppState 캐시를 우선 쓰고, 미스면 JSON+키체인에서 읽어 캐시한다
/// (열기·탐색마다 키체인을 읽지 않기 위함. save/delete 시 무효화된다).
pub fn creds_for(state: &AppState, id: &str) -> Result<S3StaticCreds, String> {
    if let Some(c) = state.s3_creds_get(id) {
        return Ok(c);
    }
    let conn = find_connection(state, id)?;
    let secret = secret_for(&conn)?;
    let creds = S3StaticCreds {
        id: conn.id.clone(),
        access_key_id: conn.access_key_id.clone(),
        secret_access_key: secret,
        region: conn.region.clone(),
    };
    state.s3_creds_put(creds.clone());
    Ok(creds)
}

/// 파일 열기용: 연결의 자격 증명(캐시 우선)과 표시 이름을 함께 반환한다.
pub fn resolve_for_open(state: &AppState, id: &str) -> Result<(S3StaticCreds, String), String> {
    if let Some(creds) = state.s3_creds_get(id) {
        // 자격 증명이 캐시됐으면 이름만 목록에서 읽는다 (로컬 파일이라 저렴).
        // 연결은 지워졌는데 캐시만 남은 경우도 있으므로 이름 조회 실패는 오류로 돌린다.
        let name = find_connection(state, id)?.name;
        return Ok((creds, name));
    }
    let conn = find_connection(state, id)?;
    let secret = secret_for(&conn)?;
    let creds = S3StaticCreds {
        id: conn.id.clone(),
        access_key_id: conn.access_key_id.clone(),
        secret_access_key: secret,
        region: conn.region.clone(),
    };
    state.s3_creds_put(creds.clone());
    Ok((creds, conn.name))
}

/// connection_id 지정 시 그 연결의 Static 인증, 아니면 주어진 프로필 인증을 돌려준다.
pub fn auth_for(
    state: &AppState,
    connection_id: Option<&str>,
    profile: Option<String>,
) -> Result<S3Auth, String> {
    match connection_id {
        Some(id) => Ok(S3Auth::Static(creds_for(state, id)?)),
        None => Ok(S3Auth::Profile(profile)),
    }
}

/// 연결의 lastUsed 를 지금으로 갱신한다.
/// 실패(파일 쓰기 오류 등)해도 호출 작업을 실패시키지 않고 로그만 남긴다.
pub fn touch_last_used(state: &AppState, id: &str) {
    let _guard = CONNS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let Ok(path) = connections_path(state) else {
        return;
    };
    let mut conns = read_connections_file(&path);
    if !touch_last_used_in(&mut conns, id, now_secs()) {
        return;
    }
    if let Err(e) = write_connections_file(&path, &conns) {
        eprintln!("S3 연결 lastUsed 갱신 실패: {e}");
    }
}

// ---------------------------------------------------------------------------
// IPC 커맨드
// ---------------------------------------------------------------------------

/// 입력 문자열을 trim 하고 비어 있으면 None 으로 정규화한다.
fn trim_opt(v: &Option<String>) -> Option<String> {
    v.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 검증 + 목록 반영 + JSON·키체인 저장. CONNS_LOCK 안에서 호출한다.
/// 비밀 키가 있으면 키체인을 먼저 쓰고 JSON 을 저장한다
/// (키체인 거부 시 아무것도 저장되지 않아 재시도가 깨끗하다).
fn save_connection_locked(
    state: &AppState,
    input: SaveS3ConnectionInput,
) -> Result<S3Connection, String> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err("연결 이름을 입력하세요".to_string());
    }
    let access_key_id = input.access_key_id.trim();
    if access_key_id.is_empty() {
        return Err("액세스 키를 입력하세요".to_string());
    }
    let secret = trim_opt(&input.secret_key);
    let region = trim_opt(&input.region);
    let bucket = trim_opt(&input.bucket);
    let prefix = trim_opt(&input.prefix);

    let path = connections_path(state)?;
    let mut conns = read_connections_file(&path);
    let conn = match input.id.as_deref() {
        Some(id) => {
            if !valid_connection_id(id) {
                return Err(format!("잘못된 S3 연결 id 입니다: {id}"));
            }
            let Some(cur) = conns.iter_mut().find(|c| c.id == id) else {
                return Err(format!(
                    "S3 연결을 찾을 수 없습니다 (삭제되었을 수 있습니다): {id}"
                ));
            };
            // 비밀 키가 새로 오면 키체인을 갱신한다 (없으면 기존 키 유지).
            if let Some(secret) = secret.as_deref() {
                keychain_set(id, secret)?;
            }
            cur.name = name.to_string();
            cur.access_key_id = access_key_id.to_string();
            cur.region = region;
            cur.bucket = bucket;
            cur.prefix = prefix;
            cur.clone()
        }
        None => {
            let Some(secret) = secret.as_deref() else {
                return Err("비밀 키를 입력하세요".to_string());
            };
            let id = new_connection_id(&conns);
            keychain_set(&id, secret)?;
            let conn = S3Connection {
                id,
                name: name.to_string(),
                access_key_id: access_key_id.to_string(),
                region,
                bucket,
                prefix,
                last_used: None,
            };
            conns.push(conn.clone());
            conn
        }
    };
    write_connections_file(&path, &conns)?;
    // 자격 증명이 바뀌었을 수 있으므로 캐시를 무효화한다
    state.s3_creds_remove(&conn.id);
    Ok(conn)
}

/// 저장된 S3 연결 목록을 반환한다 (lastUsed 내림차순, 같으면 이름순).
#[tauri::command]
pub async fn list_s3_connections(app: tauri::AppHandle) -> Result<Vec<S3Connection>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let mut conns = match state.s3_connections_path() {
            Some(p) => read_connections_file(&p),
            None => Vec::new(),
        };
        sort_connections(&mut conns);
        Ok(conns)
    })
    .await
    .map_err(|e| format!("S3 연결 목록 조회 비동기 태스크 실패: {e}"))?
}

/// S3 연결을 저장한다 (id 가 있으면 편집, 없으면 새 연결).
/// 새 연결은 secretKey 가 필수이고, 편집 시 비우면 기존 비밀 키를 유지한다.
/// 저장된 연결(비밀 키 제외)을 반환한다.
#[tauri::command]
pub async fn save_s3_connection(
    app: tauri::AppHandle,
    input: SaveS3ConnectionInput,
) -> Result<S3Connection, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _guard = CONNS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        save_connection_locked(state.inner(), input)
    })
    .await
    .map_err(|e| format!("S3 연결 저장 비동기 태스크 실패: {e}"))?
}

/// S3 연결을 삭제한다. JSON 항목과 키체인 비밀 키를 모두 지운다
/// (키체인 항목이 없어도 성공으로 본다).
#[tauri::command]
pub async fn delete_s3_connection(app: tauri::AppHandle, id: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _guard = CONNS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let path = connections_path(state.inner())?;
        let mut conns = read_connections_file(&path);
        conns.retain(|c| c.id != id);
        write_connections_file(&path, &conns)?;
        state.s3_creds_remove(&id);
        keychain_delete(&id)
    })
    .await
    .map_err(|e| format!("S3 연결 삭제 비동기 태스크 실패: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn(id: &str, name: &str, last_used: Option<u64>) -> S3Connection {
        S3Connection {
            id: id.to_string(),
            name: name.to_string(),
            last_used,
            ..Default::default()
        }
    }

    #[test]
    fn test_connections_file_roundtrip() {
        let path = crate::testutil::temp_log_path("s3conns_rw");
        let items = vec![
            S3Connection {
                id: "c11111111111".to_string(),
                name: "회사 버킷".to_string(),
                access_key_id: "AKIATEST".to_string(),
                region: Some("ap-northeast-2".to_string()),
                bucket: Some("b".to_string()),
                prefix: None,
                last_used: Some(1_700_000_000),
            },
            conn("c22222222222", "개인", None),
        ];
        write_connections_file(&path, &items).unwrap();
        assert_eq!(read_connections_file(&path), items);
        // 비밀 키는 JSON 에 절대 쓰지 않는다 (필드 자체가 없음을 직렬화로 확인).
        let json = std::fs::read_to_string(&path).unwrap();
        assert!(!json.contains("secret"), "비밀 키 필드 유출: {json}");
        // 임시 파일이 남지 않는다
        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(".tmp");
        assert!(!Path::new(&tmp_name).exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_connections_missing_and_corrupt() {
        let path = crate::testutil::temp_log_path("s3conns_missing");
        assert_eq!(read_connections_file(&path), Vec::<S3Connection>::new());
        std::fs::write(&path, b"{ not json !!!").unwrap();
        assert_eq!(read_connections_file(&path), Vec::<S3Connection>::new());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sort_connections() {
        let mut v = vec![
            conn("a", "이름2", None),
            conn("b", "브이", Some(10)),
            conn("c", "가나", Some(10)),
            conn("d", "이름1", Some(30)),
            conn("e", "아", None),
        ];
        sort_connections(&mut v);
        let order: Vec<&str> = v.iter().map(|c| c.id.as_str()).collect();
        // lastUsed 내림차순 → 30, 10, 10(이름순 가나 < 브이), 없음(이름순 아 < 이름2)
        assert_eq!(order, vec!["d", "c", "b", "e", "a"]);
    }

    #[test]
    fn test_touch_last_used_in() {
        let mut v = vec![conn("a", "x", None), conn("b", "y", Some(5))];
        assert!(touch_last_used_in(&mut v, "a", 100));
        assert_eq!(v[0].last_used, Some(100));
        assert_eq!(v[1].last_used, Some(5));
        assert!(!touch_last_used_in(&mut v, "ghost", 100));
    }

    #[test]
    fn test_valid_connection_id() {
        assert!(valid_connection_id("c1a2b3c4d5e6"));
        assert!(valid_connection_id("a"));
        assert!(!valid_connection_id(""));
        assert!(!valid_connection_id("UPPER"));
        assert!(!valid_connection_id("has@at"));
        assert!(!valid_connection_id("has/slash"));
        assert!(!valid_connection_id("has_underscore"));
        assert!(!valid_connection_id("공백 id"));
        assert!(!valid_connection_id(&"a".repeat(65)));
    }

    #[test]
    fn test_new_connection_id_format_and_unique() {
        let existing = vec![conn("c00000000000", "x", None)];
        let mut ids = std::collections::HashSet::new();
        for _ in 0..50 {
            let id = new_connection_id(&existing);
            assert!(id.len() == 13, "길이 불일치: {id}");
            assert!(id.starts_with('c'), "접두사 불일치: {id}");
            assert!(valid_connection_id(&id), "허용 문자 위반: {id}");
            assert!(ids.insert(id), "id 중복");
        }
        assert!(!ids.contains("c00000000000"));
    }

    #[test]
    fn test_save_edit_keeps_secret_and_last_used() {
        // secretKey 없는 편집은 키체인을 건드리지 않으므로 단위 테스트 가능.
        let state = AppState::default();
        let path = crate::testutil::temp_log_path("s3conns_edit");
        state.set_s3_connections_path(Some(path.clone()));
        write_connections_file(
            &path,
            &[S3Connection {
                id: "cabc123abc12".to_string(),
                name: "옛이름".to_string(),
                access_key_id: "AKIAOLD".to_string(),
                region: None,
                bucket: Some("b1".to_string()),
                prefix: None,
                last_used: Some(42),
            }],
        )
        .unwrap();

        let saved = save_connection_locked(
            &state,
            SaveS3ConnectionInput {
                id: Some("cabc123abc12".to_string()),
                name: "  새이름  ".to_string(),
                access_key_id: " AKIANEW ".to_string(),
                secret_key: None,
                region: Some("us-west-2".to_string()),
                bucket: None,
                prefix: Some("logs/".to_string()),
            },
        )
        .unwrap();
        assert_eq!(saved.name, "새이름");
        assert_eq!(saved.access_key_id, "AKIANEW");
        assert_eq!(saved.region.as_deref(), Some("us-west-2"));
        assert_eq!(saved.bucket, None);
        assert_eq!(saved.prefix.as_deref(), Some("logs/"));
        // last_used는 유지된다
        assert_eq!(saved.last_used, Some(42));
        assert_eq!(read_connections_file(&path), vec![saved]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_save_validation_errors() {
        let state = AppState::default();
        let path = crate::testutil::temp_log_path("s3conns_err");
        state.set_s3_connections_path(Some(path.clone()));
        let base = SaveS3ConnectionInput {
            id: None,
            name: "n".to_string(),
            access_key_id: "k".to_string(),
            secret_key: Some("s".to_string()),
            region: None,
            bucket: None,
            prefix: None,
        };
        // 이름 공백
        let e = save_connection_locked(
            &state,
            SaveS3ConnectionInput {
                name: "   ".to_string(),
                ..base.clone()
            },
        )
        .unwrap_err();
        assert!(e.contains("이름"));
        // 액세스 키 공백
        let e = save_connection_locked(
            &state,
            SaveS3ConnectionInput {
                access_key_id: " ".to_string(),
                ..base.clone()
            },
        )
        .unwrap_err();
        assert!(e.contains("액세스 키"));
        // 새 연결인데 비밀 키 없음
        let e = save_connection_locked(
            &state,
            SaveS3ConnectionInput {
                secret_key: Some("  ".to_string()),
                ..base
            },
        )
        .unwrap_err();
        assert!(e.contains("비밀 키"));
        // 편집 대상 부재 / 잘못된 id
        let e = save_connection_locked(
            &state,
            SaveS3ConnectionInput {
                id: Some("cghost".to_string()),
                name: "n".to_string(),
                access_key_id: "k".to_string(),
                secret_key: None,
                region: None,
                bucket: None,
                prefix: None,
            },
        )
        .unwrap_err();
        assert!(e.contains("찾을 수 없습니다"));
        let e = save_connection_locked(
            &state,
            SaveS3ConnectionInput {
                id: Some("bad@id".to_string()),
                name: "n".to_string(),
                access_key_id: "k".to_string(),
                secret_key: None,
                region: None,
                bucket: None,
                prefix: None,
            },
        )
        .unwrap_err();
        assert!(e.contains("잘못된"));
        let _ = std::fs::remove_file(&path);
    }

    /// 실제 macOS 키체인을 읽고 쓰는 수동 검증.
    /// `cargo test -p riffle --lib keychain -- --ignored` 로 실행하고 끝에서 항목을 지운다.
    #[test]
    #[ignore = "로컬 수동 실행: 실제 macOS 키체인을 읽고 쓴다"]
    fn keychain_roundtrip() {
        let id = "c9f8e7d6a5b4";
        keychain_delete(id).unwrap();
        assert_eq!(keychain_get(id).unwrap(), None);
        keychain_set(id, "riffle-test-secret").unwrap();
        assert_eq!(
            keychain_get(id).unwrap().as_deref(),
            Some("riffle-test-secret")
        );
        // 덮어쓰기도 된다
        keychain_set(id, "riffle-test-secret-2").unwrap();
        assert_eq!(
            keychain_get(id).unwrap().as_deref(),
            Some("riffle-test-secret-2")
        );
        keychain_delete(id).unwrap();
        assert_eq!(keychain_get(id).unwrap(), None);
    }
}
