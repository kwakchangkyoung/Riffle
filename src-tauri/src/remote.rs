// 원격(SFTP) 파일 열기 내부 로직과 원격 탐색 IPC 커맨드.
// 에이전트(서버 측 인덱싱·검색·필터) 위임 경로도 이 파일에 둔다.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use riffle_core::LogFile;
use riffle_remote::{AgentEvent, AgentSession, QueryJson, RemoteError, RemoteUri};
use serde::Serialize;
use serde_json::Value;
use tauri::Manager;

use crate::commands::{FileInfo, IndexProgressDto, OpenedFile, QueryDto, SearchEvent};
use crate::state::{AppState, FilterState};

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

    // 에이전트 사용이 켜진 호스트면 세션을 준비한다. gzip은 에이전트가
    // 지원하지 않으므로 시도하지 않는다. 어느 단계든 실패하면 사유(notice)만
    // 남기고 일반 SFTP 경로로 진행한다 (기능 손실 없음).
    let mut agent_session: Option<Arc<AgentSession>> = None;
    let mut agent_notice: Option<String> = None;
    if !compressed && state.agent_enabled_for(&uri.host) {
        match agent_dir(state) {
            Some(dir) => match state.remote_pool.start_agent(&uri.host, &uri.path, &dir) {
                Ok(session) => agent_session = Some(Arc::new(session)),
                Err(e) => {
                    agent_notice = Some(agent_notice_text(&agent_fallback_reason(&e)));
                }
            },
            None => {
                agent_notice = Some(agent_notice_text(
                    "이 빌드에는 에이전트가 포함되어 있지 않습니다",
                ));
            }
        }
    }

    let id = state.insert_file(
        file.clone(),
        cancel.clone(),
        PathBuf::from(path),
        compressed,
    );
    if let Some(session) = &agent_session {
        state.set_file_agent(id, session.clone());
    }

    Ok(OpenedFile {
        info: FileInfo {
            id,
            path: path.to_string(),
            size,
            compressed,
            remote: true,
            source: "sftp".to_string(),
            agent: agent_session.is_some(),
        },
        file,
        cancel,
        gz_writer,
        agent: agent_session,
        agent_notice,
    })
}

/// 에이전트 바이너리 디렉터리를 결정한다.
/// RIFFLE_AGENT_DIR 환경 변수가 있으면 우선하고, 없으면 앱 리소스의 agents/ 다.
fn agent_dir(state: &AppState) -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("RIFFLE_AGENT_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    state.agent_dir()
}

/// 에이전트 준비 실패 사유를 사용자 안내 문구로 정리한다.
/// 로컬에 해당 아키텍처 바이너리가 없는 경우는 "이 빌드에는 <arch> 에이전트가 없습니다"로 안내한다.
fn agent_fallback_reason(e: &RemoteError) -> String {
    let msg = e.to_string();
    if msg.contains("에이전트 바이너리가 없습니다") {
        for arch in ["x86_64", "aarch64"] {
            if msg.contains(&format!("riffle-agent-{arch}")) {
                return format!("이 빌드에는 {arch} 에이전트가 없습니다");
            }
        }
    }
    msg
}

/// 에이전트를 쓰지 못해 일반 방식으로 폴백할 때 안내 바에 표시할 문구를 만든다.
pub fn agent_notice_text(reason: &str) -> String {
    format!("에이전트를 사용할 수 없어 일반 방식으로 처리합니다 ({reason})")
}

/// 인덱싱을 에이전트에서 실행하고 완료 인덱스를 file 에 채택한다.
/// 진행 상황은 on_progress 에 IndexProgressDto 로 전달한다.
/// 통신·프로토콜·채택 실패 시 Err — 호출자는 일반 인덱싱으로 폴백한다.
pub fn agent_index(
    file: &LogFile,
    session: &AgentSession,
    on_progress: &mut dyn FnMut(IndexProgressDto),
) -> Result<(), String> {
    let total_bytes = session.size();
    let progress = |indexed_bytes: u64, line_count: u64, done: bool| IndexProgressDto {
        indexed_bytes,
        total_bytes,
        line_count,
        done,
        content_bytes: total_bytes,
        decompressing: false,
        error: None,
        notice: None,
    };
    let (total, lines, checkpoints) = session
        .index(&mut |e| {
            if let AgentEvent::Progress {
                indexed_bytes,
                line_count,
            } = e
            {
                on_progress(progress(indexed_bytes, line_count, false));
            }
        })
        .map_err(|e| e.to_string())?;
    file.adopt_index(riffle_core::LineIndex::from_checkpoints(
        total,
        lines,
        checkpoints,
    ))
    .map_err(|e| e.to_string())?;
    on_progress(progress(total, lines, true));
    Ok(())
}

/// 검색을 에이전트에 위임한다.
/// 매치 배치는 results 에 누적하고 종료 이벤트(Done/Cancelled)는 on_event 로 전달한다.
/// 통신·프로토콜 오류 시 Err — 호출자는 results 를 비우고 로컬 스캔으로 폴백한다.
// IPC 경로의 상태 객체들을 그대로 채우므로 인자가 많다
#[allow(clippy::too_many_arguments)]
pub fn agent_search(
    session: &AgentSession,
    query: &QueryDto,
    filter: Option<&Value>,
    cancel: &AtomicBool,
    results: &RwLock<Vec<u64>>,
    done: &AtomicBool,
    capped: &AtomicBool,
    on_event: &mut dyn FnMut(SearchEvent),
) -> Result<(), String> {
    let q = QueryJson {
        query: query.query.clone(),
        regex: query.regex,
        case_sensitive: query.case_sensitive,
    };
    session
        .search(&q, filter, cancel, &mut |e| match e {
            AgentEvent::Batch(batch) => {
                if let Ok(mut r) = results.write() {
                    r.extend_from_slice(&batch);
                }
            }
            AgentEvent::Done { total, capped: c } => {
                done.store(true, Ordering::Relaxed);
                capped.store(c, Ordering::Relaxed);
                on_event(SearchEvent::Done { total, capped: c });
            }
            AgentEvent::Cancelled => {
                on_event(SearchEvent::Cancelled);
            }
            // Error 이벤트는 search() 가 Err 로 반환하므로 여기서는 무시한다.
            _ => {}
        })
        .map_err(|e| e.to_string())
}

/// 필터 스캔을 에이전트에 위임한다.
/// 보이는 줄 구간은 filter_state.lines 에 누적하고 종료 이벤트는 on_event 로 전달한다.
/// 통신·프로토콜 오류 시 Err — 호출자는 lines 를 비우고 로컬 스캔으로 폴백한다.
pub fn agent_filter(
    session: &AgentSession,
    spec_json: &Value,
    filter_state: &Arc<FilterState>,
    on_event: &mut dyn FnMut(SearchEvent),
) -> Result<(), String> {
    session
        .filter(spec_json, &filter_state.cancel, &mut |e| match e {
            AgentEvent::Ranges(ranges) => {
                let mut lines = filter_state
                    .lines
                    .write()
                    .unwrap_or_else(|p| p.into_inner());
                for (start, end) in ranges {
                    for n in start..end {
                        lines.push_line(n);
                    }
                }
            }
            AgentEvent::Done { total, capped } => {
                filter_state.done.store(true, Ordering::Relaxed);
                filter_state.capped.store(capped, Ordering::Relaxed);
                on_event(SearchEvent::Done { total, capped });
            }
            AgentEvent::Cancelled => {
                on_event(SearchEvent::Cancelled);
            }
            _ => {}
        })
        .map_err(|e| e.to_string())
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
    use crate::commands::{FilterSpecDto, open_file_internal};
    use crate::testutil::temp_log_path;
    use riffle_core::{FilterSet, LineSet, Matcher, SearchOptions};
    use riffle_remote::{SftpPool, Transport};
    use std::process::Command;
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

    /// `cargo build -p riffle-agent` 로 만든 로컬 에이전트 바이너리 경로.
    /// `cargo test` 만으로는 bin 이 빌드되지 않을 수 있으므로 없으면 빌드한다.
    /// RIFFLE_AGENT_BIN 환경 변수로 경로를 직접 지정할 수도 있다.
    fn agent_bin() -> PathBuf {
        if let Ok(b) = std::env::var("RIFFLE_AGENT_BIN") {
            return PathBuf::from(b);
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let target_dir = std::env::var("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| root.join("target"));
        let bin = target_dir.join("debug/riffle-agent");
        if !bin.is_file() {
            let status = Command::new(env!("CARGO"))
                .args(["build", "-p", "riffle-agent"])
                .current_dir(&root)
                .status()
                .expect("cargo build 실행 실패");
            assert!(status.success(), "riffle-agent 빌드 실패");
        }
        bin
    }

    /// `riffle-agent-local` 바이너리가 든 임시 agent_dir 를 만든다.
    /// Command transport 경로의 start_agent 는 이 이름의 로컬 바이너리를 직접 실행한다.
    fn local_agent_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("riffle-agentdir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(agent_bin(), dir.join("riffle-agent-local")).unwrap();
        dir
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

    /// Command transport + 로컬 에이전트로 SFTP 파일을 에이전트 모드로 열어
    /// 인덱스·검색·필터 결과가 일반 SFTP 모드와 같은지 검증한다.
    #[test]
    fn test_agent_mode_open_index_search_filter() {
        // RIFFLE_AGENT_DIR 이 있으면 agent_dir 결정이 환경 변수를 우선해 비결정적이므로 건너뛴다
        if !sftp_server_exists() || std::env::var_os("RIFFLE_AGENT_DIR").is_some() {
            return;
        }
        let state = AppState {
            remote_pool: command_pool(),
            ..Default::default()
        };
        state.set_agent_hosts(vec!["testhost".to_string()]);
        let agent_dir = local_agent_dir("mode");
        state.set_agent_dir(Some(agent_dir.clone()));

        // i % 7 == 0 번째 줄은 WARN, 나머지는 INFO
        let local = temp_log_path("agent_mode");
        let mut content = String::new();
        for i in 0..50_000u32 {
            content.push_str(&format!(
                "2026-11-14 10:{:02}:{:02} {} worker-{} 메시지\n",
                (i / 60) % 60,
                i % 60,
                if i % 7 == 0 { "WARN" } else { "INFO" },
                i % 8
            ));
        }
        std::fs::write(&local, &content).expect("임시 파일 쓰기 실패");
        let uri = format!("sftp://testhost{}", local.display());

        let opened = open_file_internal(&state, &uri).expect("원격 열기 실패");
        assert!(opened.info.agent, "에이전트 모드로 열려야 한다");
        assert!(opened.agent_notice.is_none());
        let session = state
            .get_agent(opened.info.id)
            .expect("파일에 에이전트 세션이 없다");
        let id = opened.info.id;

        // 인덱싱: 에이전트 체크포인트를 채택하고 로컬 인덱스와 비교한다
        let mut saw_done = false;
        agent_index(&opened.file, &session, &mut |p| {
            if p.done {
                saw_done = true;
            }
        })
        .expect("에이전트 인덱싱 실패");
        assert!(saw_done);
        assert_eq!(opened.file.index().line_count(), 50_000);

        let expect = LogFile::open(&local).expect("로컬 열기 실패");
        expect
            .build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("로컬 인덱싱 실패");
        assert_eq!(
            opened.file.index().checkpoints(),
            expect.index().checkpoints()
        );

        // 줄 읽기는 SFTP(RandomRead) 경로 그대로 동작해야 한다
        let remote_lines = opened.file.read_lines(0, 5, None, None).unwrap();
        let local_lines = expect.read_lines(0, 5, None, None).unwrap();
        for (a, b) in remote_lines.iter().zip(local_lines.iter()) {
            assert_eq!(a.text, b.text);
        }

        // 검색: 에이전트 결과 == 로컬 search_filtered 결과
        let query = QueryDto {
            query: "worker-3".to_string(),
            regex: false,
            case_sensitive: true,
        };
        let results = RwLock::new(Vec::new());
        let done = AtomicBool::new(false);
        let capped = AtomicBool::new(false);
        let cancel = AtomicBool::new(false);
        let mut done_total = None;
        agent_search(
            &session,
            &query,
            None,
            &cancel,
            &results,
            &done,
            &capped,
            &mut |e| {
                if let SearchEvent::Done { total, .. } = e {
                    done_total = Some(total);
                }
            },
        )
        .expect("에이전트 검색 실패");
        let matcher = Matcher::new("worker-3", SearchOptions::from(&query)).unwrap();
        let mut want = Vec::new();
        riffle_core::search_filtered(
            &expect,
            &matcher,
            None,
            &cancel,
            &mut |b| want.extend_from_slice(b),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(results.read().unwrap().as_slice(), want.as_slice());
        assert_eq!(done_total, Some(want.len() as u64));
        assert!(done.load(Ordering::Relaxed));

        // 필터: 에이전트 구간 == 로컬 filter_scan 의 LineSet
        let spec_dto = FilterSpecDto {
            rules: Vec::new(),
            combine: "and".to_string(),
            levels: vec!["warn".to_string()],
        };
        let core_spec = crate::commands::filter_spec_of(&spec_dto).unwrap();
        let set = Arc::new(FilterSet::new(&core_spec, None).unwrap());
        let fstate = state
            .insert_filter(id, spec_dto.clone(), set.clone())
            .expect("필터 등록 실패");
        let spec_json = serde_json::to_value(&spec_dto).unwrap();
        agent_filter(&session, &spec_json, &fstate, &mut |_| {}).expect("에이전트 필터 실패");
        assert!(fstate.done.load(Ordering::Relaxed));

        let mut want_set = LineSet::new();
        riffle_core::filter_scan(
            &expect,
            &set,
            &cancel,
            &mut |b| {
                for &n in b {
                    want_set.push_line(n);
                }
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fstate.lines.read().unwrap().ranges(), want_set.ranges());

        state.close_file(id);
        let _ = std::fs::remove_file(&local);
        let _ = std::fs::remove_dir_all(&agent_dir);
    }

    /// 에이전트 바이너리가 없으면 조용히 일반 SFTP 경로로 폴백해야 한다.
    #[test]
    fn test_agent_fallback_missing_binary() {
        if !sftp_server_exists() || std::env::var_os("RIFFLE_AGENT_DIR").is_some() {
            return;
        }
        let state = AppState {
            remote_pool: command_pool(),
            ..Default::default()
        };
        state.set_agent_hosts(vec!["testhost".to_string()]);
        // 존재하지 않는 디렉터리 → 에이전트 준비 실패 → 폴백
        state.set_agent_dir(Some(PathBuf::from("/nonexistent/riffle-agent-dir")));

        let local = temp_log_path("agent_fallback");
        std::fs::write(&local, b"a\nb\n").expect("임시 파일 쓰기 실패");
        let uri = format!("sftp://testhost{}", local.display());

        let opened = open_file_internal(&state, &uri).expect("원격 열기 실패");
        assert!(!opened.info.agent);
        assert!(state.get_agent(opened.info.id).is_none());
        let notice = opened.agent_notice.expect("폴백 사유가 있어야 한다");
        assert!(notice.contains("에이전트를 사용할 수 없어"));

        // 일반 인덱싱·읽기는 그대로 동작해야 한다
        opened
            .file
            .build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("인덱싱 실패");
        assert_eq!(opened.file.index().line_count(), 2);
        let lines = opened.file.read_lines(0, 2, None, None).unwrap();
        assert_eq!(lines[0].text, "a");

        state.close_file(opened.info.id);
        let _ = std::fs::remove_file(&local);
    }

    /// agentHosts 에 없는 호스트는 에이전트를 시도하지 않는다.
    #[test]
    fn test_agent_disabled_host_uses_sftp() {
        if !sftp_server_exists() {
            return;
        }
        let state = AppState {
            remote_pool: command_pool(),
            ..Default::default()
        };
        // set_agent_hosts 를 호출하지 않으면 기본 꺼짐이다
        let local = temp_log_path("agent_off");
        std::fs::write(&local, b"x\n").expect("임시 파일 쓰기 실패");
        let uri = format!("sftp://testhost{}", local.display());

        let opened = open_file_internal(&state, &uri).expect("원격 열기 실패");
        assert!(!opened.info.agent);
        assert!(opened.agent_notice.is_none());

        state.close_file(opened.info.id);
        let _ = std::fs::remove_file(&local);
    }
}
