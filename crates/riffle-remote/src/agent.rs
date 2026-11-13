//! 원격 에이전트 세션 클라이언트.
//!
//! 원격 호스트에 업로드된 `riffle-agent` 를 ssh 로 실행하고,
//! stdin/stdout 줄 단위 JSON 프로토콜로 인덱싱·검색·필터를 위임한다.
//! 화면에 보이는 줄 읽기는 이 경로가 아니라 기존 SFTP(RandomRead)를 그대로 쓴다.
//!
//! 준비(uname → SFTP 업로드 → 실행 → hello/open)나 통신 중 어느 단계든
//! 실패하면 `RemoteError::Agent` 를 돌려주며, 호출자는 그때 기존 SFTP
//! 경로로 폴백하면 된다 (기능 손실 없음).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh_sftp::protocol::{FileAttributes, OpenFlags};
use serde_json::{Value, json};

use crate::conn::{SSH_ARGS, SftpConn, Transport};
use crate::error::{RemoteError, classify_ssh_failure, map_sftp_error};
use crate::pool::SftpPool;

/// 에이전트 실행 파일 버전 (= 워크스페이스 버전).
/// riffle-agent crate 도 같은 `version.workspace` 를 쓰므로 hello 응답의
/// version 과 항상 일치한다. 원격 설치 파일명(`riffle-agent-<버전>-<arch>`)에도
/// 들어가므로 버전을 올리면 원격에 새 파일이 설치된다.
pub const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 지원하는 프로토콜 버전.
const AGENT_PROTOCOL: u64 = 1;
/// 원격 홈 디렉터리 아래 에이전트 설치 위치.
const REMOTE_AGENT_DIR: &str = ".cache/riffle";
/// `Transport::Command`(로컬 테스트) 경로에서 쓰는 아키텍처 이름.
/// agent_dir 안의 `riffle-agent-local` 바이너리를 직접 실행한다.
const LOCAL_ARCH: &str = "local";
/// hello/open 응답 제한 시간.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// 작업 중 취소 플래그 폴링 간격.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// stderr 꼬리 보관 최대 바이트.
const STDERR_CAP: usize = 8 * 1024;

/// search 요청의 query 객체 (앱의 QueryDto 와 같은 JSON 형식).
#[derive(Debug, Clone)]
pub struct QueryJson {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
}

/// 에이전트가 보내는 스트리밍 이벤트.
#[derive(Debug)]
pub enum AgentEvent {
    /// index 진행률.
    Progress { indexed_bytes: u64, line_count: u64 },
    /// search 매치 줄 번호 배치.
    Batch(Vec<u64>),
    /// filter 의 보이는 줄 연속 구간 (반열린 [start, end)).
    Ranges(Vec<(u64, u64)>),
    /// 작업 완료. capped=true 이면 상한에 걸려 일부만 반환된 것.
    Done { total: u64, capped: bool },
    /// cancel 요청에 의해 중단됨.
    Cancelled,
    /// 에이전트 측 오류 (해당 작업만 실패).
    Error(String),
}

/// 살아있는 에이전트 프로세스와의 세션 (한 세션 = 파일 하나).
///
/// 응답 디스패치 스레드가 자식 stdout 을 줄 단위로 읽어 요청 id 별
/// 채널로 전달한다. 작업(index/search/filter)은 동시에 여러 개 가능하다.
/// Drop 시 자식 프로세스를 죽인다.
pub struct AgentSession {
    stdin: Mutex<ChildStdin>,
    /// 요청 id → 응답 채널 송신 측 (디스패치 스레드와 공유)
    requests: Arc<Mutex<HashMap<u64, Sender<Value>>>>,
    next_id: AtomicU64,
    child: Mutex<Child>,
    /// 에이전트 stderr 의 꼬리 (종료 원인 진단용)
    stderr_tail: Arc<Mutex<Vec<u8>>>,
    /// open 응답이 알려 준 파일 크기
    size: AtomicU64,
}

impl AgentSession {
    /// 로컬 에이전트 바이너리를 ssh 없이 직접 실행해 세션을 연다 (테스트용).
    /// `agent_bin` 은 `riffle-agent` 실행 파일, `file_path` 는 열 대상.
    pub fn spawn_local(agent_bin: &Path, file_path: &Path) -> Result<AgentSession, RemoteError> {
        let child = Command::new(agent_bin)
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                RemoteError::Agent(format!("에이전트 실행 실패({}): {e}", agent_bin.display()))
            })?;
        Self::attach(child, file_path.to_string_lossy().as_ref())
    }

    /// ssh 로 원격 명령을 실행해 세션을 연다.
    /// `remote_cmd` 는 원격 셸이 해석할 명령 문자열이다 ($HOME 확장 포함).
    fn spawn_ssh(
        ssh: &str,
        host: &str,
        remote_cmd: &str,
        path: &str,
    ) -> Result<AgentSession, RemoteError> {
        let child = Command::new(ssh)
            .args(SSH_ARGS)
            .arg(host)
            .arg(remote_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| RemoteError::Agent(format!("ssh 실행 실패: {e}")))?;
        Self::attach(child, path)
    }

    /// 자식 프로세스의 파이프를 연결하고 hello/open 핸드셰이크를 수행한다.
    fn attach(mut child: Child, path: &str) -> Result<AgentSession, RemoteError> {
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            return Err(RemoteError::Agent(
                "에이전트 stdio 파이프 생성 실패".to_string(),
            ));
        };

        // stderr 는 로그 용도 — 꼬리만 유지하며 계속 비워 둔다.
        let stderr_tail = Arc::new(Mutex::new(Vec::<u8>::new()));
        {
            let tail = stderr_tail.clone();
            std::thread::spawn(move || {
                let mut stderr = stderr;
                let mut buf = [0u8; 4096];
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut g = tail.lock().unwrap();
                    g.extend_from_slice(&buf[..n]);
                    let excess = g.len().saturating_sub(STDERR_CAP);
                    if excess > 0 {
                        g.drain(..excess);
                    }
                }
            });
        }

        // 디스패치 스레드: stdout 을 줄 단위로 읽어 요청 id 별 채널로 전달한다.
        let requests: Arc<Mutex<HashMap<u64, Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        {
            let requests = requests.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match r.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                                continue;
                            };
                            let Some(id) = v.get("id").and_then(Value::as_u64) else {
                                continue;
                            };
                            let tx = requests.lock().unwrap().get(&id).cloned();
                            if let Some(tx) = tx {
                                let _ = tx.send(v);
                            }
                        }
                    }
                }
                // 프로세스 종료: 대기 채널을 모두 닫아 recv() 가 깨어나게 한다.
                requests.lock().unwrap().clear();
            });
        }

        let session = AgentSession {
            stdin: Mutex::new(stdin),
            requests,
            next_id: AtomicU64::new(1),
            child: Mutex::new(child),
            stderr_tail,
            size: AtomicU64::new(0),
        };
        session
            .size
            .store(session.handshake(path)?, Ordering::Relaxed);
        Ok(session)
    }

    /// hello → open 핸드셰이크. 열린 파일 크기를 반환한다.
    /// 버전/프로토콜 불일치와 gzip 은 Err(→ 호출자가 SFTP 폴백)다.
    fn handshake(&self, path: &str) -> Result<u64, RemoteError> {
        let hello = self.roundtrip(json!({"op": "hello"}), HANDSHAKE_TIMEOUT)?;
        let h = hello.get("hello").ok_or_else(|| self.proto_err(&hello))?;
        let version = h.get("version").and_then(Value::as_str).unwrap_or("");
        let protocol = h.get("protocol").and_then(Value::as_u64).unwrap_or(0);
        if version != AGENT_VERSION || protocol != AGENT_PROTOCOL {
            return Err(RemoteError::Agent(format!(
                "버전이 다릅니다 (에이전트 {version} protocol {protocol}, 기대 {AGENT_VERSION} protocol {AGENT_PROTOCOL})"
            )));
        }

        let opened = self.roundtrip(json!({"op": "open", "path": path}), HANDSHAKE_TIMEOUT)?;
        if let Some(e) = opened.get("error") {
            return Err(RemoteError::Agent(err_msg(e)));
        }
        let o = opened
            .get("opened")
            .ok_or_else(|| self.proto_err(&opened))?;
        if o.get("gzip").and_then(Value::as_bool) == Some(true) {
            return Err(RemoteError::Agent(
                "gzip 파일은 에이전트를 지원하지 않습니다".to_string(),
            ));
        }
        Ok(u64_field(o, "size"))
    }

    /// open 응답이 알려 준 파일 크기.
    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Relaxed)
    }

    /// 요청 한 줄을 쓴다.
    fn send_line(&self, v: &Value) -> Result<(), RemoteError> {
        let mut s = serde_json::to_string(v)
            .map_err(|e| RemoteError::Agent(format!("요청 직렬화 실패: {e}")))?;
        s.push('\n');
        let mut w = self.stdin.lock().unwrap();
        w.write_all(s.as_bytes())
            .and_then(|()| w.flush())
            .map_err(|e| RemoteError::Agent(format!("에이전트 쓰기 실패: {e}")))
    }

    /// id 를 붙여 요청을 보내고 응답 채널을 반환한다.
    fn request(&self, mut v: Value) -> Result<(u64, Receiver<Value>), RemoteError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        v["id"] = Value::from(id);
        let (tx, rx) = mpsc::channel();
        self.requests.lock().unwrap().insert(id, tx);
        if let Err(e) = self.send_line(&v) {
            self.requests.lock().unwrap().remove(&id);
            return Err(e);
        }
        Ok((id, rx))
    }

    /// 요청 처리가 끝났으면 등록을 지운다.
    fn finish(&self, id: u64) {
        self.requests.lock().unwrap().remove(&id);
    }

    /// 응답을 하나 보내고 하나 받는다 (hello/open 전용, 타임아웃 있음).
    fn roundtrip(&self, req: Value, timeout: Duration) -> Result<Value, RemoteError> {
        let (id, rx) = self.request(req)?;
        let result = rx.recv_timeout(timeout);
        self.finish(id);
        match result {
            Ok(v) => Ok(v),
            Err(RecvTimeoutError::Timeout) => {
                Err(RemoteError::Agent("에이전트 응답 시간 초과".to_string()))
            }
            Err(RecvTimeoutError::Disconnected) => Err(self.dead_err()),
        }
    }

    /// 대상 작업에 cancel 요청을 보낸다 (응답 없음).
    fn send_cancel(&self, target: u64) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.send_line(&json!({"id": id, "op": "cancel", "target": target}));
    }

    fn proto_err(&self, v: &Value) -> RemoteError {
        RemoteError::Agent(format!("프로토콜 오류: {v}"))
    }

    /// 에이전트 프로세스 종료를 나타내는 오류 (stderr 꼬리를 붙인다).
    fn dead_err(&self) -> RemoteError {
        let tail = String::from_utf8_lossy(&self.stderr_tail.lock().unwrap())
            .trim()
            .to_string();
        if tail.is_empty() {
            RemoteError::Agent("에이전트 프로세스가 종료됐습니다".to_string())
        } else {
            RemoteError::Agent(format!("에이전트 프로세스가 종료됐습니다: {tail}"))
        }
    }

    /// 인덱싱을 에이전트에서 실행한다.
    /// 반환: (total_bytes, line_count, checkpoints) — `LineIndex::from_checkpoints` 용.
    pub fn index(
        &self,
        on: &mut dyn FnMut(AgentEvent),
    ) -> Result<(u64, u64, Vec<u64>), RemoteError> {
        let (id, rx) = self.request(json!({"op": "index"}))?;
        let result = (|| loop {
            let v = rx.recv().map_err(|_| self.dead_err())?;
            if let Some(p) = v.get("progress") {
                on(AgentEvent::Progress {
                    indexed_bytes: u64_field(p, "indexedBytes"),
                    line_count: u64_field(p, "lineCount"),
                });
            } else if let Some(ix) = v.get("index") {
                let cps = ix
                    .get("checkpoints")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                return Ok((u64_field(ix, "totalBytes"), u64_field(ix, "lineCount"), cps));
            } else if let Some(e) = v.get("error") {
                return Err(RemoteError::Agent(err_msg(e)));
            } else if v.get("cancelled").is_some() {
                return Err(RemoteError::Agent("인덱싱이 취소됐습니다".to_string()));
            }
        })();
        self.finish(id);
        result
    }

    /// 검색을 에이전트에서 실행한다 (필터 교집합 포함).
    /// `cancel` 이 세워지면 에이전트에 cancel 요청을 보내고,
    /// 종료 이벤트(Done/Cancelled/Error)가 올 때까지 이벤트를 `on` 에 전달한다.
    pub fn search(
        &self,
        query: &QueryJson,
        filter: Option<&Value>,
        cancel: &AtomicBool,
        on: &mut dyn FnMut(AgentEvent),
    ) -> Result<(), RemoteError> {
        let (id, rx) = self.request(json!({
            "op": "search",
            "query": {
                "query": query.query,
                "regex": query.regex,
                "caseSensitive": query.case_sensitive,
            },
            "filter": filter.cloned().unwrap_or(Value::Null),
        }))?;
        let result = self.stream_job(id, &rx, cancel, on);
        self.finish(id);
        result
    }

    /// 필터 스캔을 에이전트에서 실행한다.
    /// `spec` 은 FilterSpecDto 형식의 JSON (필드 규칙·컬럼 감지는 에이전트가 수행).
    pub fn filter(
        &self,
        spec: &Value,
        cancel: &AtomicBool,
        on: &mut dyn FnMut(AgentEvent),
    ) -> Result<(), RemoteError> {
        let (id, rx) = self.request(json!({"op": "filter", "spec": spec}))?;
        let result = self.stream_job(id, &rx, cancel, on);
        self.finish(id);
        result
    }

    /// 스트리밍 작업(search/filter)의 이벤트를 받아 `on` 에 전달한다.
    /// 50ms 마다 `cancel` 을 확인해 한 번만 cancel 요청을 보낸다.
    fn stream_job(
        &self,
        id: u64,
        rx: &Receiver<Value>,
        cancel: &AtomicBool,
        on: &mut dyn FnMut(AgentEvent),
    ) -> Result<(), RemoteError> {
        let mut cancel_sent = cancel.load(Ordering::Relaxed);
        if cancel_sent {
            self.send_cancel(id);
        }
        loop {
            match rx.recv_timeout(POLL_INTERVAL) {
                Ok(v) => {
                    if let Some(b) = v.get("batch") {
                        on(AgentEvent::Batch(u64_vec(b)));
                    } else if let Some(r) = v.get("ranges") {
                        on(AgentEvent::Ranges(range_vec(r)));
                    } else if let Some(d) = v.get("done") {
                        on(AgentEvent::Done {
                            total: u64_field(d, "total"),
                            capped: d.get("capped").and_then(Value::as_bool).unwrap_or(false),
                        });
                        return Ok(());
                    } else if v.get("cancelled").is_some() {
                        on(AgentEvent::Cancelled);
                        return Ok(());
                    } else if let Some(e) = v.get("error") {
                        let m = err_msg(e);
                        on(AgentEvent::Error(m.clone()));
                        return Err(RemoteError::Agent(m));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !cancel_sent && cancel.load(Ordering::Relaxed) {
                        cancel_sent = true;
                        self.send_cancel(id);
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return Err(self.dead_err()),
            }
        }
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn u64_field(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn err_msg(e: &Value) -> String {
    e.as_str().unwrap_or("알 수 없는 에이전트 오류").to_string()
}

fn u64_vec(v: &Value) -> Vec<u64> {
    v.as_array()
        .map(|a| a.iter().filter_map(Value::as_u64).collect())
        .unwrap_or_default()
}

fn range_vec(v: &Value) -> Vec<(u64, u64)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    let s = r.get(0).and_then(Value::as_u64)?;
                    let e = r.get(1).and_then(Value::as_u64)?;
                    Some((s, e))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `ssh <host> uname -sm` 으로 원격 아키텍처를 확인한다.
/// Linux x86_64 / aarch64 만 지원하며 그 외는 Err(→ SFTP 폴백)다.
fn remote_arch(ssh: &str, host: &str) -> Result<&'static str, RemoteError> {
    let out = Command::new(ssh)
        .args(SSH_ARGS)
        .arg(host)
        .arg("uname -sm")
        .output()
        .map_err(|e| RemoteError::Agent(format!("ssh uname 실행 실패: {e}")))?;
    if !out.status.success() {
        return Err(classify_ssh_failure(
            host,
            &String::from_utf8_lossy(&out.stderr),
        ));
    }
    let uname = String::from_utf8_lossy(&out.stdout).trim().to_string();
    match uname.as_str() {
        "Linux x86_64" => Ok("x86_64"),
        "Linux aarch64" | "Linux arm64" => Ok("aarch64"),
        other => Err(RemoteError::Agent(format!(
            "지원하지 않는 원격 환경입니다: {other}"
        ))),
    }
}

/// 원격 디렉터리에 바이너리를 업로드하고 0755 권한의 최종 이름으로 rename 한다.
/// 같은 크기의 파일이 이미 있으면 업로드를 생략한다.
/// 임시 이름으로 쓴 뒤 rename 해서 부분 파일이 노출되지 않게 한다.
pub(crate) async fn upload_agent_conn(
    conn: &Arc<SftpConn>,
    remote_dir: &str,
    remote_path: &str,
    data: &[u8],
) -> Result<(), RemoteError> {
    let session = &conn.session;
    if let Ok(attrs) = session.stat(remote_path).await
        && attrs.attrs.size == Some(data.len() as u64)
    {
        return Ok(());
    }

    // 설치 디렉터리를 경로 접두사 단위로 만든다 (mkdir -p, 이미 있으면 오류 무시).
    for (i, &b) in remote_dir.as_bytes().iter().enumerate() {
        if b == b'/' && i > 0 {
            let _ = session
                .mkdir(&remote_dir[..i], FileAttributes::empty())
                .await;
        }
    }
    let _ = session.mkdir(remote_dir, FileAttributes::empty()).await;

    // 임시 이름에 pid 와 프로세스 내 일련번호를 넣어 동시 업로드와
    // 충돌하지 않게 한다 (다른 프로세스는 pid 가 다르다).
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = format!(
        "{remote_path}.tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let _ = session.remove(tmp.as_str()).await;
    let handle = session
        .open(
            tmp.as_str(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            FileAttributes::empty(),
        )
        .await
        .map_err(|e| map_sftp_error(e, &tmp))?;

    let mut write_result = Ok(());
    let mut off = 0u64;
    for chunk in data.chunks(conn.write_chunk as usize) {
        if let Err(e) = session
            .write(handle.handle.clone(), off, chunk.to_vec())
            .await
        {
            write_result = Err(map_sftp_error(e, &tmp));
            break;
        }
        off += chunk.len() as u64;
    }
    if write_result.is_ok()
        && let Err(e) = session
            .setstat(
                tmp.as_str(),
                FileAttributes {
                    permissions: Some(0o755),
                    ..FileAttributes::empty()
                },
            )
            .await
    {
        write_result = Err(map_sftp_error(e, &tmp));
    }
    let _ = session.close(handle.handle).await;
    write_result?;

    // rename 이 덮어쓰기를 거부하는 서버를 위해 실패 시 기존 파일을 지우고 재시도한다.
    if session.rename(tmp.as_str(), remote_path).await.is_err() {
        // 다른 설치가 먼저 끝낸 경우: 목표가 같은 크기면 성공으로 본다.
        if let Ok(attrs) = session.stat(remote_path).await
            && attrs.attrs.size == Some(data.len() as u64)
        {
            let _ = session.remove(tmp.as_str()).await;
            return Ok(());
        }
        let _ = session.remove(remote_path).await;
        session
            .rename(tmp.as_str(), remote_path)
            .await
            .map_err(|e| map_sftp_error(e, remote_path))?;
    }
    Ok(())
}

impl SftpPool {
    /// 원격 호스트에서 riffle-agent 를 준비해 `AgentSession` 을 연다.
    ///
    /// 절차: `ssh <host> uname -sm` 아키텍처 확인 → 원격
    /// `~/.cache/riffle/riffle-agent-<버전>-<arch>` 에 같은 크기의 바이너리가
    /// 없으면 SFTP 업로드(임시 이름 → 0755 → rename) → `ssh <host> … --stdio`
    /// 실행 → hello/open 핸드셰이크.
    ///
    /// `agent_dir`: 로컬 에이전트 바이너리 디렉터리. 파일명은
    /// `riffle-agent-<arch>` (arch = x86_64 | aarch64, 테스트용 "local").
    /// 어느 단계든 실패하면 Err 를 반환하며 호출자는 SFTP 방식으로 폴백한다.
    pub fn start_agent(
        &self,
        host: &str,
        path: &str,
        agent_dir: &Path,
    ) -> Result<AgentSession, RemoteError> {
        match self.inner.transport() {
            // 테스트 경로: ssh 없이 로컬 에이전트 바이너리를 직접 실행한다.
            Transport::Command(_) => AgentSession::spawn_local(
                &agent_dir.join(format!("riffle-agent-{LOCAL_ARCH}")),
                Path::new(path),
            ),
            Transport::Ssh => self.start_agent_ssh(host, path, agent_dir),
        }
    }

    fn start_agent_ssh(
        &self,
        host: &str,
        path: &str,
        agent_dir: &Path,
    ) -> Result<AgentSession, RemoteError> {
        let ssh = self.inner.ssh_program().to_string();
        // 같은 호스트로의 동시 설치(uname 확인 + 업로드)를 직렬화한다.
        // 두 번째 호출은 캐시된 arch 와 "같은 크기 존재" 경로로 통과한다.
        let lock = self.inner.agent_install_lock(host);
        let remote_name = {
            let mut arch_cache = lock.lock().unwrap();
            let arch = match *arch_cache {
                Some(a) => a,
                None => {
                    let a = remote_arch(&ssh, host)?;
                    *arch_cache = Some(a);
                    a
                }
            };
            let local_bin: PathBuf = agent_dir.join(format!("riffle-agent-{arch}"));
            if !local_bin.is_file() {
                return Err(RemoteError::Agent(format!(
                    "에이전트 바이너리가 없습니다: {}",
                    local_bin.display()
                )));
            }
            let home = self.home_dir(host)?;
            let remote_dir = format!("{}/{REMOTE_AGENT_DIR}", home.trim_end_matches('/'));
            let remote_name = format!("riffle-agent-{AGENT_VERSION}-{arch}");
            let remote_path = format!("{remote_dir}/{remote_name}");
            self.upload_agent(host, &remote_dir, &remote_path, &local_bin)?;
            remote_name
        };
        // ~ 확장은 원격 셸에 맡긴다 ($HOME/… 형태로 전달).
        AgentSession::spawn_ssh(
            &ssh,
            host,
            &format!("\"$HOME/{REMOTE_AGENT_DIR}/{remote_name}\" --stdio"),
            path,
        )
    }

    /// 원격 경로에 같은 크기의 바이너리가 없으면 로컬 바이너리를 업로드한다.
    fn upload_agent(
        &self,
        host: &str,
        remote_dir: &str,
        remote_path: &str,
        local_bin: &Path,
    ) -> Result<(), RemoteError> {
        let data = std::fs::read(local_bin)
            .map_err(|e| RemoteError::Agent(format!("에이전트 바이너리 읽기 실패: {e}")))?;
        let inner = self.inner.clone();
        let inner2 = inner.clone();
        let remote_dir = remote_dir.to_string();
        let remote_path = remote_path.to_string();
        inner.block_on(async move {
            let conn = inner2.ensure_conn(host).await?;
            upload_agent_conn(&conn, &remote_dir, &remote_path, &data).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicU64;

    use riffle_core::{
        Combine, FilterRule, FilterSet, FilterSpec, Level, LineIndex, LineSet, LogFile, Matcher,
        RuleKind, SearchOptions, detect_columns, filter_scan, search_filtered,
    };

    fn tempdir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "riffle-agent-test-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `cargo build -p riffle-agent` 로 만든 로컬 에이전트 바이너리 경로.
    /// `cargo test` 만으로는 bin 이 빌드되지 않을 수 있으므로 없으면 빌드한다.
    /// RIFFLE_AGENT_BIN 환경 변수로 경로를 직접 지정할 수도 있다.
    fn agent_bin() -> PathBuf {
        if let Ok(b) = std::env::var("RIFFLE_AGENT_BIN") {
            return PathBuf::from(b);
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
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

    fn gen_log(path: &Path, lines: usize) {
        let mut buf = Vec::with_capacity(lines * 48);
        for i in 0..lines {
            buf.extend_from_slice(
                format!(
                    "2026-11-14 10:{:02}:{:02} {} worker-{} 메시지 alpha {}\n",
                    (i / 60) % 60,
                    i % 60,
                    if i % 7 == 0 { "WARN" } else { "INFO" },
                    i % 8,
                    i
                )
                .as_bytes(),
            );
        }
        fs::write(path, buf).unwrap();
    }

    /// 세션 search 의 매치 줄 번호와 done 결과를 수집한다.
    fn run_search(
        session: &AgentSession,
        q: &QueryJson,
        filter: Option<&Value>,
        cancel: &AtomicBool,
    ) -> (Vec<u64>, u64, bool) {
        let mut hits = Vec::new();
        let mut done = None;
        session
            .search(q, filter, cancel, &mut |e| match e {
                AgentEvent::Batch(b) => hits.extend_from_slice(&b),
                AgentEvent::Done { total, capped } => done = Some((total, capped)),
                AgentEvent::Error(m) => panic!("에이전트 오류: {m}"),
                _ => {}
            })
            .expect("에이전트 검색 실패");
        let (total, capped) = done.expect("done 응답 없음");
        (hits, total, capped)
    }

    /// 로컬 search_filtered 와 같은 조건의 기대값.
    fn local_search(file: &LogFile, m: &Matcher, f: Option<&FilterSet>) -> (Vec<u64>, u64, bool) {
        let cancel = AtomicBool::new(false);
        let mut hits = Vec::new();
        let outcome = search_filtered(
            file,
            m,
            f,
            &cancel,
            &mut |b| hits.extend_from_slice(b),
            &mut |_| {},
        )
        .expect("로컬 검색 실패");
        match outcome {
            riffle_core::SearchOutcome::Completed { total } => (hits, total, false),
            riffle_core::SearchOutcome::Capped { total } => (hits, total, true),
            riffle_core::SearchOutcome::Cancelled => (hits, 0, false),
        }
    }

    fn spec_json(rules: Value, combine: &str, levels: &[&str]) -> Value {
        json!({
            "rules": rules,
            "combine": combine,
            "levels": levels,
        })
    }

    /// FilterSpecDto 형식 JSON 에 대응하는 코어 FilterSpec.
    fn core_spec() -> FilterSpec {
        FilterSpec {
            rules: vec![FilterRule {
                pattern: "alpha".to_string(),
                options: SearchOptions {
                    regex: false,
                    case_sensitive: true,
                },
                kind: RuleKind::Include,
                enabled: true,
                field: None,
            }],
            combine: Combine::Or,
            levels: vec![Level::Info],
        }
    }

    #[test]
    fn test_version_output_matches() {
        // --version 출력이 AGENT_VERSION/프로토콜 상수와 일치해야 한다.
        let out = Command::new(agent_bin())
            .arg("--version")
            .output()
            .expect("에이전트 실행 실패");
        assert!(out.status.success());
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(
            text,
            format!("riffle-agent {AGENT_VERSION} protocol {AGENT_PROTOCOL}")
        );
    }

    #[test]
    fn test_spawn_local_roundtrip() {
        let dir = tempdir("rt");
        let file = dir.join("app.log");
        gen_log(&file, 100_000);
        let cancel = AtomicBool::new(false);

        let session = AgentSession::spawn_local(&agent_bin(), &file).expect("세션 실패");
        assert_eq!(
            session.size() as usize,
            fs::metadata(&file).unwrap().len() as usize
        );

        // 로컬 기대값
        let local = LogFile::open(&file).unwrap();
        local.build_index(&cancel, &mut |_| {}).unwrap();

        // index: 체크포인트·줄 수가 로컬 LineIndex 와 동일해야 한다
        let (total, lines, cps) = session.index(&mut |_| {}).expect("index 실패");
        assert_eq!(total, local.source().len());
        assert_eq!(lines, local.index().line_count());
        assert_eq!(cps, local.index().checkpoints());

        // adopt_index + read_lines == 로컬
        let adopted = LogFile::open(&file).unwrap();
        adopted
            .adopt_index(LineIndex::from_checkpoints(total, lines, cps))
            .expect("adopt_index 실패");
        for (start, count) in [(0u64, 50u64), (50_000, 100), (99_990, 20)] {
            let a = adopted.read_lines(start, count, None, None).unwrap();
            let l = local.read_lines(start, count, None, None).unwrap();
            assert_eq!(a.len(), l.len());
            for (x, y) in a.iter().zip(l.iter()) {
                assert_eq!(x.text, y.text, "줄 {} 내용 불일치", x.no);
            }
        }

        // search: 리터럴 / 정규식 / 대소문자 무시
        for (query, regex, case) in [
            ("alpha 12345", false, true),
            (r"alpha\s+999\d", true, true),
            ("ALPHA", false, false),
        ] {
            let q = QueryJson {
                query: query.to_string(),
                regex,
                case_sensitive: case,
            };
            let m = Matcher::new(
                query,
                SearchOptions {
                    regex,
                    case_sensitive: case,
                },
            )
            .unwrap();
            let got = run_search(&session, &q, None, &cancel);
            let want = local_search(&local, &m, None);
            assert_eq!(got, want, "query {query}");
        }

        // search + 필터 동반 (레벨 INFO만)
        let spec = core_spec();
        let spec_json = spec_json(
            json!([{"pattern":"alpha","regex":false,"caseSensitive":true,
                    "kind":"include","enabled":true,"color":"#fff"}]),
            "or",
            &["info"],
        );
        let q = QueryJson {
            query: "worker".to_string(),
            regex: false,
            case_sensitive: true,
        };
        let m = Matcher::new(
            "worker",
            SearchOptions {
                regex: false,
                case_sensitive: true,
            },
        )
        .unwrap();
        let cols = detect_columns(&local).unwrap();
        let set = FilterSet::new(&spec, Some(&cols)).unwrap();
        let got = run_search(&session, &q, Some(&spec_json), &cancel);
        let want = local_search(&local, &m, Some(&set));
        assert_eq!(got, want);
    }

    #[test]
    fn test_spawn_local_filter() {
        let dir = tempdir("flt");
        let file = dir.join("app.log");
        gen_log(&file, 60_000);
        let cancel = AtomicBool::new(false);
        let session = AgentSession::spawn_local(&agent_bin(), &file).expect("세션 실패");

        // 필드 규칙 포함 CSV 샘플로 필터 검증
        let csv = dir.join("data.csv");
        let mut content = String::from("name,city,age\n");
        for i in 0..30_000 {
            content.push_str(&format!(
                "user{i},{},{}\n",
                if i % 3 == 0 { "seoul" } else { "busan" },
                20 + i % 40
            ));
        }
        fs::write(&csv, content).unwrap();
        let csv_session = AgentSession::spawn_local(&agent_bin(), &csv).expect("csv 세션 실패");

        let spec_v = spec_json(
            json!([{"pattern":"seoul","regex":false,"caseSensitive":true,
                    "kind":"include","enabled":true,"color":"#fff","field":"city"}]),
            "and",
            &[],
        );
        let mut ranges = Vec::new();
        let mut done = None;
        csv_session
            .filter(&spec_v, &cancel, &mut |e| match e {
                AgentEvent::Ranges(r) => ranges.extend_from_slice(&r),
                AgentEvent::Done { total, capped } => done = Some((total, capped)),
                AgentEvent::Error(m) => panic!("에이전트 오류: {m}"),
                _ => {}
            })
            .expect("필터 실패");

        // 로컬 기대값
        let local = LogFile::open(&csv).unwrap();
        let cols = detect_columns(&local).unwrap();
        let spec = FilterSpec {
            rules: vec![FilterRule {
                pattern: "seoul".to_string(),
                options: SearchOptions {
                    regex: false,
                    case_sensitive: true,
                },
                kind: RuleKind::Include,
                enabled: true,
                field: Some("city".to_string()),
            }],
            combine: Combine::And,
            levels: vec![],
        };
        let set = FilterSet::new(&spec, Some(&cols)).unwrap();
        let mut expected = LineSet::new();
        filter_scan(
            &local,
            &set,
            &cancel,
            &mut |b| {
                for &l in b {
                    expected.push_line(l);
                }
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(ranges.as_slice(), expected.ranges());
        assert_eq!(done, Some((expected.len(), false)));

        // 일반 로그 파일에 대한 필터(레벨 조건)도 동작해야 한다
        let spec_v = spec_json(json!([]), "and", &["warn"]);
        let mut ranges = Vec::new();
        session
            .filter(&spec_v, &cancel, &mut |e| {
                if let AgentEvent::Ranges(r) = e {
                    ranges.extend_from_slice(&r);
                }
            })
            .expect("필터 실패");
        let warn_lines: u64 = (0..60_000u64).filter(|i| i % 7 == 0).count() as u64;
        let total: u64 = ranges.iter().map(|(s, e)| e - s).sum();
        assert_eq!(total, warn_lines);
    }

    #[test]
    fn test_cancel() {
        // 검색이 진행 중인 동안 cancel 플래그가 세워지면 Cancelled 이벤트가 온다.
        // 스캔이 끝나기 전에 취소가 도착하도록 충분히 큰 파일을 쓴다.
        let dir = tempdir("cancel");
        let file = dir.join("big.log");
        gen_log(&file, 1_500_000); // ~60MB
        let session = AgentSession::spawn_local(&agent_bin(), &file).expect("세션 실패");

        let cancel = AtomicBool::new(true); // 요청과 동시에 취소 전송
        let q = QueryJson {
            query: "alpha".to_string(),
            regex: false,
            case_sensitive: true,
        };
        let mut saw_cancelled = false;
        let mut saw_done = false;
        session
            .search(&q, None, &cancel, &mut |e| match e {
                AgentEvent::Cancelled => saw_cancelled = true,
                AgentEvent::Done { .. } => saw_done = true,
                AgentEvent::Error(m) => panic!("에이전트 오류: {m}"),
                _ => {}
            })
            .expect("검색 실패");
        // 대부분 Cancelled, 아주 빠르면 Done 일 수 있으나 어느 쪽이든 정상 종료여야 한다
        assert!(saw_cancelled || saw_done);
        if saw_done {
            eprintln!("참고: 취소 전에 검색이 끝나 Done 을 받았습니다");
        } else {
            assert!(saw_cancelled);
        }
    }

    #[test]
    fn test_concurrent_ops() {
        // 검색과 필터를 동시에 실행해도 각각 정상 완료돼야 한다.
        let dir = tempdir("conc");
        let file = dir.join("app.log");
        gen_log(&file, 80_000);
        let session = AgentSession::spawn_local(&agent_bin(), &file).expect("세션 실패");
        let session = &session;

        let spec = spec_json(json!([]), "and", &["warn"]);
        std::thread::scope(|s| {
            s.spawn(|| {
                let cancel = AtomicBool::new(false);
                let q = QueryJson {
                    query: "worker-3".to_string(),
                    regex: false,
                    case_sensitive: true,
                };
                let mut done = false;
                session
                    .search(&q, None, &cancel, &mut |e| {
                        if let AgentEvent::Done { total, .. } = e {
                            done = total == 10_000;
                        }
                    })
                    .unwrap();
                assert!(done);
            });
            s.spawn(|| {
                let cancel = AtomicBool::new(false);
                let mut done_total = None;
                session
                    .filter(&spec, &cancel, &mut |e| {
                        if let AgentEvent::Done { total, .. } = e {
                            done_total = Some(total);
                        }
                    })
                    .unwrap();
                let want = (0..80_000u64).filter(|i| i % 7 == 0).count() as u64;
                assert_eq!(done_total, Some(want));
            });
        });
    }

    #[test]
    fn test_start_agent_command_transport() {
        // Transport::Command 경로는 ssh 없이 로컬 바이너리(riffle-agent-local)를 실행한다.
        let dir = tempdir("cmd");
        let file = dir.join("app.log");
        gen_log(&file, 50_000);
        let agent_dir = dir.join("agents");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::copy(agent_bin(), agent_dir.join("riffle-agent-local")).unwrap();

        let pool = SftpPool::new(Transport::Command(vec!["/usr/libexec/sftp-server".into()]));
        let session = pool
            .start_agent("ignored", file.to_str().unwrap(), &agent_dir)
            .expect("start_agent 실패");
        let (total, lines, cps) = session.index(&mut |_| {}).expect("index 실패");
        assert_eq!(lines, 50_000);
        assert_eq!(total as usize, fs::metadata(&file).unwrap().len() as usize);
        assert_eq!(cps.first(), Some(&0));
    }

    #[test]
    fn test_upload_agent() {
        // 로컬 sftp-server(Transport::Command) 대상 업로드·권한·rename·재업로드 생략 검증.
        let dir = tempdir("upload");
        let remote_dir = dir.join("home/.cache/riffle");
        let remote_path = remote_dir.join("riffle-agent-test-x86_64");
        let remote_dir_s = remote_dir.to_string_lossy().to_string();
        let remote_path_s = remote_path.to_string_lossy().to_string();

        let pool = SftpPool::new(Transport::Command(vec!["/usr/libexec/sftp-server".into()]));
        let inner = pool.inner.clone();
        let conn = inner
            .block_on(inner.ensure_conn("local"))
            .expect("연결 실패");

        let data = vec![7u8; 10_000];
        inner
            .block_on(upload_agent_conn(
                &conn,
                &remote_dir_s,
                &remote_path_s,
                &data,
            ))
            .expect("업로드 실패");

        // 내용·권한 0755 확인
        assert_eq!(fs::read(&remote_path).unwrap(), data);
        let mode = fs::metadata(&remote_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);

        // 같은 크기면 재업로드 생략: 같은 크기의 다른 내용을 심어 두고 호출해도 유지돼야 한다
        fs::write(&remote_path, vec![9u8; 10_000]).unwrap();
        inner
            .block_on(upload_agent_conn(
                &conn,
                &remote_dir_s,
                &remote_path_s,
                &data,
            ))
            .expect("재확인 실패");
        assert_eq!(fs::read(&remote_path).unwrap(), vec![9u8; 10_000]);

        // 크기가 다르면 다시 업로드된다
        let other = vec![1u8; 12_000];
        inner
            .block_on(upload_agent_conn(
                &conn,
                &remote_dir_s,
                &remote_path_s,
                &other,
            ))
            .expect("재업로드 실패");
        assert_eq!(fs::read(&remote_path).unwrap(), other);

        // 실제 에이전트 바이너리(수 MB) 업로드 — 쓰기 청크가 서버 패킷
        // 한도를 넘어 세션이 죽는 회귀를 검증한다.
        let bin = fs::read(agent_bin()).expect("에이전트 바이너리 읽기 실패");
        inner
            .block_on(upload_agent_conn(
                &conn,
                &remote_dir_s,
                &remote_path_s,
                &bin,
            ))
            .expect("대형 업로드 실패");
        assert_eq!(fs::read(&remote_path).unwrap(), bin);
    }

    /// sftp-server 로 가는 가짜 ssh 를 작성한다.
    /// `-s` 호출은 sftp-server, `uname -sm` 은 "Linux x86_64",
    /// 그 외 마지막 인자는 원격 셸처럼 `sh -c` 로 실행한다.
    /// HOME 을 `fake_home` 으로 바꿔 업로드·실행 경로를 맞추고,
    /// 매 호출의 인자를 `log` 에 한 줄씩 기록한다.
    fn write_fake_ssh(dir: &Path, fake_home: &Path, log: &Path) -> PathBuf {
        let ssh = dir.join("fake-ssh");
        fs::write(
            &ssh,
            format!(
                "#!/bin/sh\n\
                 echo \"$*\" >> '{l}'\n\
                 export HOME='{h}'\n\
                 cd \"$HOME\" || exit 1\n\
                 for last; do :; done\n\
                 case \" $* \" in *\" -s \"*) exec /usr/libexec/sftp-server ;; esac\n\
                 case \"$last\" in\n\
                 \"uname -sm\") echo \"Linux x86_64\" ;;\n\
                 *) exec sh -c \"$last\" ;;\n\
                 esac\n",
                h = fake_home.display(),
                l = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
        ssh
    }

    #[test]
    fn test_start_agent_ssh_path() {
        // 앱의 start_agent_ssh 와 같은 순서를 가짜 ssh 로 재현한다:
        // uname 확인 → SFTP 업로드(실제 수 MB 바이너리) → 원격 exec → index.
        let dir = tempdir("sshpath");
        let fake_home = dir.join("home");
        fs::create_dir_all(&fake_home).unwrap();
        let file = dir.join("app.log");
        gen_log(&file, 50_000);
        let agent_dir = dir.join("agents");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::copy(agent_bin(), agent_dir.join("riffle-agent-x86_64")).unwrap();
        let ssh = write_fake_ssh(&dir, &fake_home, &dir.join("ssh.log"));
        let ssh_s = ssh.to_string_lossy().to_string();

        // 풀 연결은 가짜 ssh 의 -s 경로로 연다 (Command argv 를 그대로 실행).
        let pool = SftpPool::new(Transport::Command(vec![
            ssh_s.clone(),
            "-s".into(),
            "fakehost".into(),
            "sftp".into(),
        ]));

        let arch = remote_arch(&ssh_s, "fakehost").expect("uname 실패");
        assert_eq!(arch, "x86_64");
        let home = pool.home_dir("fakehost").expect("home_dir 실패");
        let remote_dir = format!("{}/{REMOTE_AGENT_DIR}", home.trim_end_matches('/'));
        let remote_name = format!("riffle-agent-{AGENT_VERSION}-{arch}");
        let remote_path = format!("{remote_dir}/{remote_name}");
        let local_bin = agent_dir.join(format!("riffle-agent-{arch}"));
        pool.upload_agent("fakehost", &remote_dir, &remote_path, &local_bin)
            .expect("업로드 실패");

        let session = AgentSession::spawn_ssh(
            &ssh_s,
            "fakehost",
            &format!("\"$HOME/{REMOTE_AGENT_DIR}/{remote_name}\" --stdio"),
            file.to_str().unwrap(),
        )
        .expect("세션 실패");
        let (total, lines, cps) = session.index(&mut |_| {}).expect("index 실패");
        assert_eq!(lines, 50_000);
        assert_eq!(total as usize, fs::metadata(&file).unwrap().len() as usize);
        assert_eq!(cps.first(), Some(&0));

        // 업로드된 파일이 같은 크기·0755 권한으로 존재해야 한다.
        let uploaded = fake_home.join(format!("{REMOTE_AGENT_DIR}/{remote_name}"));
        let meta = fs::metadata(&uploaded).expect("업로드 파일 없음");
        assert_eq!(meta.len(), fs::metadata(&local_bin).unwrap().len());
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
    }

    #[test]
    fn test_start_agent_concurrent() {
        // 같은 호스트에 대한 동시 start_agent 는 설치를 한 번만 수행하고
        // 모든 호출이 각자의 세션을 얻어야 한다 (동시 설치 경합 회귀).
        let dir = tempdir("sshconc");
        let fake_home = dir.join("home");
        fs::create_dir_all(&fake_home).unwrap();
        let file = dir.join("app.log");
        gen_log(&file, 50_000);
        let agent_dir = dir.join("agents");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::copy(agent_bin(), agent_dir.join("riffle-agent-x86_64")).unwrap();
        let log = dir.join("ssh.log");
        let ssh = write_fake_ssh(&dir, &fake_home, &log);
        let ssh_s = ssh.to_string_lossy().to_string();

        // Transport::Ssh 경로를 가짜 ssh 로 탄다 (SFTP 접속·uname·실행 모두).
        let pool = SftpPool::new_with_ssh(Transport::Ssh, &ssh_s);
        let path = file.to_str().unwrap().to_string();
        let sessions: Vec<AgentSession> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..4)
                .map(|_| s.spawn(|| pool.start_agent("fakehost", &path, &agent_dir)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap().expect("start_agent 실패"))
                .collect()
        });
        assert_eq!(sessions.len(), 4);
        for session in &sessions {
            let (_, lines, _) = session.index(&mut |_| {}).expect("index 실패");
            assert_eq!(lines, 50_000);
        }
        drop(sessions);

        // 설치가 직렬화됐다면 uname 확인은 1회다 (arch 캐시).
        let log_text = fs::read_to_string(&log).unwrap();
        assert_eq!(
            log_text.matches("uname -sm").count(),
            1,
            "설치 중복 수행: {log_text}"
        );

        // 설치 파일이 0755 로 존재하고 임시 파일은 남지 않아야 한다.
        let remote_dir = fake_home.join(REMOTE_AGENT_DIR);
        let installed = remote_dir.join(format!("riffle-agent-{AGENT_VERSION}-x86_64"));
        let meta = fs::metadata(&installed).expect("설치 파일 없음");
        assert_eq!(meta.permissions().mode() & 0o777, 0o755);
        let leftovers: Vec<_> = fs::read_dir(&remote_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "임시 파일 잔존: {leftovers:?}");
    }
}
