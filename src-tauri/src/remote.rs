// 원격(SFTP) 파일 열기 내부 로직과 원격 탐색 IPC 커맨드

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use riffle_core::LogFile;
use riffle_remote::RemoteUri;
use serde::Serialize;
use tauri::Manager;

use crate::commands::{FileInfo, OpenedFile};
use crate::state::AppState;

/// ~/.ssh/config의 Host 항목 DTO (프론트 ipc.ts의 SshHost와 1:1)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshHostDto {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
}

/// 원격 디렉터리 항목 DTO (프론트 ipc.ts의 RemoteEntry와 1:1)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteEntryDto {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: Option<u64>,
}

/// 경로가 sftp:// 원격 URI이면 호스트 부분을 반환한다 (로컬 경로면 None).
/// close_file에서 pool.release 대상 호스트를 얻는 데 사용한다.
pub fn remote_host_of_path(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    let rest = s.strip_prefix("sftp://")?;
    let end = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..end];
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// sftp:// URI 원격 파일을 열어 OpenedFile을 반환한다.
/// 열기 도중 실패하면 SftpPool 참조를 되돌려 연결 누수를 막는다.
pub fn open_remote_internal(state: &AppState, path: &str) -> Result<OpenedFile, String> {
    let uri = RemoteUri::parse(path).ok_or_else(|| {
        format!("잘못된 원격 경로입니다: '{path}' (sftp://호스트/경로 형식이어야 합니다)")
    })?;
    match open_remote_inner(state, path, &uri) {
        Ok(opened) => Ok(opened),
        Err(e) => {
            state.remote_pool.release(&uri.host);
            Err(e)
        }
    }
}

/// 원격 파일 열기 본체. 실패 시 Err를 반환하며 pool.release는 호출 측이 담당한다.
fn open_remote_inner(state: &AppState, path: &str, uri: &RemoteUri) -> Result<OpenedFile, String> {
    let reader = state
        .remote_pool
        .open_file(uri)
        .map_err(|e| format!("원격 파일을 열 수 없습니다 ('{path}'): {e}"))?;
    // FileInfo.size는 원격의 압축 크기(gz면 압축 바이트 수)를 표시한다
    let size = reader
        .stat_len()
        .map_err(|e| format!("원격 파일 크기를 확인할 수 없습니다 ('{path}'): {e}"))?;
    let gz = riffle_core::is_gzip_reader(reader.as_ref())
        .map_err(|e| format!("원격 파일을 열 수 없습니다 ('{path}'): {e}"))?;

    let (log_file, gz_writer) = if gz {
        let (file, writer) = riffle_core::open_gzip_remote(reader)
            .map_err(|e| format!("원격 gzip 파일을 열 수 없습니다 ('{path}'): {e}"))?;
        (file, Some(writer))
    } else {
        let file = LogFile::open_remote(reader)
            .map_err(|e| format!("원격 파일을 열 수 없습니다 ('{path}'): {e}"))?;
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
        },
        file,
        cancel,
        gz_writer,
    })
}

/// ~/.ssh/config의 Host 별칭 목록을 반환한다.
#[tauri::command]
pub async fn list_ssh_hosts() -> Result<Vec<SshHostDto>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        Ok(riffle_remote::list_ssh_hosts()
            .into_iter()
            .map(|h| SshHostDto {
                alias: h.alias,
                hostname: h.hostname,
                user: h.user,
            })
            .collect())
    })
    .await
    .map_err(|e| format!("호스트 목록 조회 비동기 태스크 실패: {e}"))?
}

/// 원격 디렉터리의 항목 목록을 반환한다 (이름순, 디렉터리 먼저).
#[tauri::command]
pub async fn list_remote_dir(
    app: tauri::AppHandle,
    host: String,
    path: String,
) -> Result<Vec<RemoteEntryDto>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state
            .remote_pool
            .list_dir(&host, &path)
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|e| RemoteEntryDto {
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
    .map_err(|e| format!("디렉터리 목록 조회 비동기 태스크 실패: {e}"))?
}

/// 원격 호스트의 홈 디렉터리 절대 경로를 반환한다.
#[tauri::command]
pub async fn remote_home(app: tauri::AppHandle, host: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        state.remote_pool.home_dir(&host).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("홈 디렉터리 조회 비동기 태스크 실패: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::open_file_internal;
    use crate::testutil::temp_log_path;
    use riffle_remote::{SftpPool, Transport};
    use std::sync::atomic::Ordering;

    /// macOS 기본 sftp-server를 Command transport로 사용하는 테스트용 풀.
    /// Command transport는 호스트를 무시하고 로컬 sftp-server에 연결하므로
    /// URI의 path 부분이 실제 로컬 파일시스템 경로가 된다.
    fn command_pool() -> SftpPool {
        SftpPool::new(Transport::Command(vec![
            "/usr/libexec/sftp-server".to_string(),
        ]))
    }

    fn sftp_server_exists() -> bool {
        Path::new("/usr/libexec/sftp-server").exists()
    }

    #[test]
    fn test_open_file_routes_sftp_uri_to_remote() {
        if !sftp_server_exists() {
            return;
        }
        let state = AppState {
            remote_pool: command_pool(),
            ..Default::default()
        };

        let local = temp_log_path("remote_route");
        std::fs::write(&local, b"remote line1\nremote line2\n").expect("임시 파일 쓰기 실패");
        let uri = format!("sftp://testhost{}", local.display());

        // open_file_internal이 sftp:// URI를 원격 경로로 라우팅해야 한다
        let opened = open_file_internal(&state, &uri).expect("원격 열기 실패");
        assert!(opened.info.remote);
        assert!(!opened.info.compressed);
        assert_eq!(opened.info.path, uri);
        assert_eq!(opened.info.size, 26);

        opened
            .file
            .build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("인덱싱 실패");
        let lines = opened
            .file
            .read_lines(0, 2, None, None)
            .expect("줄 읽기 실패");
        assert_eq!(lines[0].text, "remote line1");
        assert_eq!(lines[1].text, "remote line2");

        state.close_file(opened.info.id);
        assert_eq!(state.file_count(), 0);
        let _ = std::fs::remove_file(&local);
    }

    #[test]
    fn test_close_file_releases_remote_pool() {
        if !sftp_server_exists() {
            return;
        }
        let state = AppState {
            remote_pool: command_pool(),
            ..Default::default()
        };

        let local = temp_log_path("remote_release");
        std::fs::write(&local, b"a\nb\n").expect("임시 파일 쓰기 실패");
        let uri = format!("sftp://testhost{}", local.display());

        // 같은 호스트의 원격 파일 두 개를 열고 하나씩 닫는다.
        // close_file이 pool.release를 호출하지 않으면 참조 카운트가 남아
        // 재오픈/종료 시 연결이 종료되지 않는다.
        let o1 = open_file_internal(&state, &uri).expect("첫 번째 열기 실패");
        let o2 = open_file_internal(&state, &uri).expect("두 번째 열기 실패");
        assert_ne!(o1.info.id, o2.info.id);

        state.close_file(o1.info.id);
        // 나머지 파일은 계속 읽을 수 있어야 한다 (공유 연결이 살아 있음)
        o2.file
            .build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("인덱싱 실패");
        assert_eq!(o2.file.index().line_count(), 2);

        state.close_file(o2.info.id);
        assert_eq!(state.file_count(), 0);

        // 닫은 뒤 같은 URI를 다시 열 수 있어야 한다 (release 후 재연결)
        let o3 = open_file_internal(&state, &uri).expect("재오픈 실패");
        assert!(o3.info.remote);
        state.close_file(o3.info.id);

        let _ = std::fs::remove_file(&local);
    }

    #[test]
    fn test_remote_host_of_path() {
        assert_eq!(
            remote_host_of_path(Path::new("sftp://myhost/var/log/app.log")),
            Some("myhost".to_string())
        );
        assert_eq!(
            remote_host_of_path(Path::new("sftp://myhost")),
            Some("myhost".to_string())
        );
        assert_eq!(remote_host_of_path(Path::new("/local/file.log")), None);
        assert_eq!(remote_host_of_path(Path::new("sftp:///nohost")), None);
    }

    #[test]
    fn test_open_file_rejects_bad_remote_uri() {
        let state = AppState::default();
        // sftp:// 뒤에 호스트가 없는 경로는 RemoteUri::parse가 거부해야 한다
        let result = open_file_internal(&state, "sftp://");
        assert!(result.is_err());
        assert_eq!(state.file_count(), 0);
    }

    #[test]
    fn test_close_file_cancels_remote_indexing() {
        if !sftp_server_exists() {
            return;
        }
        let state = AppState {
            remote_pool: command_pool(),
            ..Default::default()
        };

        let local = temp_log_path("remote_cancel");
        // 인덱싱이 오래 걸리도록 큰 파일 생성
        let mut content = Vec::new();
        for i in 0..200_000u32 {
            content.extend_from_slice(format!("cancel test line {i} padding padding\n").as_bytes());
        }
        std::fs::write(&local, &content).expect("임시 파일 쓰기 실패");
        let uri = format!("sftp://testhost{}", local.display());

        let opened = open_file_internal(&state, &uri).expect("원격 열기 실패");
        let id = opened.info.id;
        // 인덱싱을 시작한 뒤 바로 닫는다
        let cancel = opened.cancel.clone();
        let file = opened.file.clone();
        let thread_cancel = cancel.clone();
        let handle = std::thread::spawn(move || file.build_index(&thread_cancel, &mut |_| {}));
        state.close_file(id);
        assert!(cancel.load(Ordering::Relaxed));
        let _ = handle.join();
        assert_eq!(state.file_count(), 0);

        let _ = std::fs::remove_file(&local);
    }
}
