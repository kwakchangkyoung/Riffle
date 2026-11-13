//! 전송 계층: ssh 서브프로세스(또는 테스트용 임의 명령)를 띄워
//! 그 stdin/stdout 을 SFTP 스트림으로 사용한다.

use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::rawsession::Limits;
use russh_sftp::client::{Config as SftpConfig, RawSftpSession};
use russh_sftp::extensions;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::error::{RemoteError, classify_ssh_failure};

/// SFTP 세션 수립(ssh 접속 + 프로토콜 초기화) 제한 시간.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 개별 SFTP 요청의 응답 제한 시간.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// 서버가 read 길이 제한을 알려주지 않을 때 사용하는 요청 크기.
/// russh-sftp 기본 max_packet_len(256KiB)에서 패킷 오버헤드를 뺀 값.
const DEFAULT_READ_CHUNK: u64 = 256 * 1024 - 13;
/// 서버가 write 길이 제한을 알려주지 않을 때 사용하는 데이터 크기.
/// OpenSSH sftp-server 는 수신 패킷 전체가 256KiB 를 넘으면 세션을 끊으므로
/// 한도를 모를 때는 보수적인 32KiB 를 쓴다.
const DEFAULT_WRITE_CHUNK: u64 = 32 * 1024;
/// limits 확장으로 받은 packet_len/read_len 에서 뺄 패킷 오버헤드.
const READ_PACKET_OVERHEAD: u64 = 13;
/// write 패킷에서 데이터 외의 프레임(핸들 문자열·오프셋·길이 필드) 여유분.
const WRITE_PACKET_OVERHEAD: u64 = 64;
/// stderr 에 보관하는 최대 바이트 수 (최근 내용만 유지).
const STDERR_CAP: usize = 32 * 1024;

/// 원격 호스트에 붙는 방법.
#[derive(Debug, Clone)]
pub enum Transport {
    /// 시스템 `ssh` 로 sftp 서브시스템에 접속한다.
    /// `~/.ssh/config`·키·agent·known_hosts 를 그대로 사용한다.
    Ssh,
    /// 테스트용: 이 명령의 stdin/stdout 을 SFTP 스트림으로 사용한다.
    /// `host` 는 무시된다.
    Command(Vec<String>),
}

/// 호스트 하나에 대한 살아있는 SFTP 연결.
/// `Arc` 로 공유되며, 마지막 참조가 사라지거나 `shutdown` 되면 ssh 프로세스가 종료된다.
pub(crate) struct SftpConn {
    pub session: Arc<RawSftpSession>,
    /// kill_on_drop 으로 띄운 자식 프로세스. shutdown 시 명시적으로 죽인다.
    pub child: Mutex<Child>,
    /// 서버 한도와 패킷 크기를 반영한 SSH_FXP_READ 1회 요청 최대 바이트.
    pub read_chunk: u32,
    /// 서버 한도와 패킷 크기를 반영한 SSH_FXP_WRITE 1회 데이터 최대 바이트.
    pub write_chunk: u32,
    /// shutdown 이 시작됐는지 여부.
    pub dead: AtomicBool,
}

impl SftpConn {
    /// 연결을 끊는다: SFTP 채널을 닫고 자식 프로세스를 죽여 회수한다.
    /// 여러 번 호출해도 안전하다.
    pub async fn shutdown(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        // 채널 EOF 를 보내 ssh 가 정상 종료하도록 유도한 뒤 확실히 죽인다.
        let _ = self.session.close_session();
        let mut child = self.child.lock().await;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// ssh 공통 옵션. SFTP 접속뿐 아니라 에이전트 실행/uname 확인에도 그대로 쓴다.
pub(crate) const SSH_ARGS: &[&str] = &[
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    // 데이터 채널 용도이므로 포워딩·로컬 명령 실행을 모두 끈다.
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "PermitLocalCommand=no",
];

/// 전송 계층의 연결 수립 로직. `PoolInner` 가 호스트별로 호출한다.
pub(crate) async fn connect(
    transport: &Transport,
    host: &str,
    ssh_program: &str,
) -> Result<SftpConn, RemoteError> {
    let mut cmd = match transport {
        Transport::Ssh => {
            let mut c = Command::new(ssh_program);
            c.args(SSH_ARGS).arg("-s").arg(host).arg("sftp");
            c
        }
        Transport::Command(argv) => {
            let Some((prog, args)) = argv.split_first() else {
                return Err(RemoteError::Connection("빈 전송 명령".to_string()));
            };
            let mut c = Command::new(prog);
            c.args(args);
            c
        }
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(RemoteError::Io)?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill().await;
        return Err(RemoteError::Io(io::Error::other("stdio 파이프 생성 실패")));
    };

    // ssh 의 stderr 는 접속 실패 분류와 진단에만 쓰므로 꼬리만 유지하며 계속 비워준다.
    let stderr_tail = Arc::new(StdMutex::new(Vec::<u8>::new()));
    {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut buf = [0u8; 8192];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut g = tail.lock().unwrap();
                        g.extend_from_slice(&buf[..n]);
                        if g.len() > STDERR_CAP {
                            let excess = g.len() - STDERR_CAP;
                            g.drain(..excess);
                        }
                    }
                }
            }
        });
    }

    // 자식의 stdout 을 읽기, stdin 을 쓰기 쪽으로 합성한 양방향 스트림.
    let stream = tokio::io::join(stdout, stdin);
    let session = Arc::new(RawSftpSession::new_with_config(
        stream,
        SftpConfig {
            request_timeout_secs: REQUEST_TIMEOUT_SECS,
            ..Default::default()
        },
    ));

    let init = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let version = session.init().await?;
        let (mut read_chunk, mut write_chunk) = (DEFAULT_READ_CHUNK, DEFAULT_WRITE_CHUNK);
        if version
            .extensions
            .get(extensions::LIMITS)
            .map(String::as_str)
            == Some("1")
        {
            // 서버가 광고하는 한도를 읽어 요청 크기를 줄인다.
            // (set_limits 는 &mut 이 필요해 Arc 뒤에서는 쓸 수 없으므로
            //  클라이언트 측 사전 검증 없이 read/write_chunk 계산에만 반영한다.)
            if let Ok(lim_ext) = session.limits().await {
                let lim = Limits::from(lim_ext);
                if let Some(read_len) = lim.read_len {
                    read_chunk = read_chunk.min(read_len);
                }
                if let Some(write_len) = lim.write_len {
                    write_chunk = write_chunk.min(write_len);
                }
                if let Some(packet_len) = lim.packet_len {
                    read_chunk = read_chunk.min(packet_len.saturating_sub(READ_PACKET_OVERHEAD));
                    write_chunk = write_chunk.min(packet_len.saturating_sub(WRITE_PACKET_OVERHEAD));
                }
            }
        }
        Ok::<(u64, u64), SftpError>((read_chunk, write_chunk))
    })
    .await;

    let (read_chunk, write_chunk) = match init {
        Err(_) => {
            let _ = child.kill().await;
            return Err(RemoteError::Timeout(CONNECT_TIMEOUT.as_secs()));
        }
        Ok(Err(e)) => {
            let _ = child.kill().await;
            let stderr_text = stderr_text(&stderr_tail);
            return Err(match transport {
                Transport::Ssh => classify_ssh_failure(host, &stderr_text),
                Transport::Command(_) => RemoteError::Sftp(format!(
                    "SFTP 초기화 실패: {e}{}",
                    if stderr_text.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", stderr_text)
                    }
                )),
            });
        }
        Ok(Ok(chunks)) => chunks,
    };

    Ok(SftpConn {
        session,
        child: Mutex::new(child),
        read_chunk: read_chunk.clamp(1, u32::MAX as u64) as u32,
        write_chunk: write_chunk.clamp(1, u32::MAX as u64) as u32,
        dead: AtomicBool::new(false),
    })
}

fn stderr_text(tail: &StdMutex<Vec<u8>>) -> String {
    let g = tail.lock().unwrap();
    String::from_utf8_lossy(&g).trim().to_string()
}
