//! 호스트별 SFTP 연결을 관리하는 풀.
//!
//! - `open_file` 로 연 파일 수(`users`)가 0이 되면 연결을 즉시 종료한다.
//! - 파일 없이 `list_dir`/`home_dir` 만 쓰인 연결은 60초 유휴 후 자동 종료한다.
//! - 모든 공개 메서드는 동기이며 내부 토키오 런타임(워커 2개)에서 실행한다.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use riffle_core::RandomRead;
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::runtime::Runtime;
use tokio::sync::Mutex as AsyncMutex;

use crate::conn::{SftpConn, Transport, connect};
use crate::error::{RemoteError, map_sftp_error, sftp_is_conn_error, sftp_is_timeout};
use crate::remote_file::RemoteFile;
use crate::uri::RemoteUri;

/// 디렉터리 전용 연결의 유휴 종료 시간.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// 유휴 연결을 검사하는 주기.
const REAPER_INTERVAL: Duration = Duration::from_secs(15);

/// 원격 디렉터리 항목.
#[derive(Debug, Clone)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// unix 시간(초). 서버가 주지 않으면 `None`.
    pub mtime: Option<u64>,
}

/// 호스트별 연결 상태.
struct HostInner {
    /// 현재 연결 세대. 죽은 연결은 `dead` 플래그가 세워진 뒤 교체된다.
    conn: Option<Arc<SftpConn>>,
    /// 이 호스트로 열린 파일 수. 0이 되면 연결을 종료한다.
    users: usize,
    /// 마지막으로 연결이 사용된 시각 (유휴 판정용).
    last_used: Instant,
}

/// 호스트 접속을 직렬화하기 위한 잠금 + 연결 상태.
/// connect 는 await 를 걸치는 시간이 길어 별도 잠금으로 보호한다.
struct HostSlot {
    inner: StdMutex<HostInner>,
    connect_lock: AsyncMutex<()>,
}

pub(crate) struct PoolInner {
    rt: Runtime,
    transport: Transport,
    hosts: StdMutex<HashMap<String, Arc<HostSlot>>>,
}

impl PoolInner {
    fn new(transport: Transport) -> Arc<PoolInner> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio 런타임 생성 실패");
        let inner = Arc::new(PoolInner {
            rt,
            transport,
            hosts: StdMutex::new(HashMap::new()),
        });
        // 유휴 연결 회수 태스크. 풀이 죽으면 Weak 업그레이드에 실패해 스스로 종료된다.
        let weak = Arc::downgrade(&inner);
        inner.rt.spawn(async move { reaper_loop(weak).await });
        inner
    }

    /// 동기 API 구현체들이 내부 런타임 위에서 비동기 작업을 돌리기 위한 진입점.
    pub(crate) fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    fn entry(&self, host: &str) -> Arc<HostSlot> {
        let mut map = self.hosts.lock().unwrap();
        map.entry(host.to_string())
            .or_insert_with(|| {
                Arc::new(HostSlot {
                    inner: StdMutex::new(HostInner {
                        conn: None,
                        users: 0,
                        last_used: Instant::now(),
                    }),
                    connect_lock: AsyncMutex::new(()),
                })
            })
            .clone()
    }

    fn entry_opt(&self, host: &str) -> Option<Arc<HostSlot>> {
        self.hosts.lock().unwrap().get(host).cloned()
    }

    /// 살아있는 연결을 반환한다. 없거나 dead 면 `None`.
    fn live_conn(slot: &HostSlot) -> Option<Arc<SftpConn>> {
        let mut inner = slot.inner.lock().unwrap();
        let live = match &inner.conn {
            Some(c) if !c.dead.load(Ordering::Relaxed) => Some(c.clone()),
            _ => None,
        };
        if live.is_some() {
            inner.last_used = Instant::now();
        }
        live
    }

    /// 연결을 확보한다. 없으면 새로 만든다 (15초 타임아웃).
    pub(crate) async fn ensure_conn(&self, host: &str) -> Result<Arc<SftpConn>, RemoteError> {
        let slot = self.entry(host);
        if let Some(c) = Self::live_conn(&slot) {
            return Ok(c);
        }
        // 동시 접속 시도를 직렬화한다.
        let _guard = slot.connect_lock.lock().await;
        if let Some(c) = Self::live_conn(&slot) {
            return Ok(c);
        }
        // 여기 도달하면 conn 은 없거나 dead 뿐이다. dead 연결 참조를 정리한다.
        let _ = slot.inner.lock().unwrap().conn.take();
        let conn = Arc::new(connect(&self.transport, host).await?);
        {
            let mut inner = slot.inner.lock().unwrap();
            inner.conn = Some(conn.clone());
            inner.last_used = Instant::now();
        }
        Ok(conn)
    }

    /// 현재 연결이 `conn` 과 같은 포인터일 때만 제거하고 종료한다.
    /// 이미 다른 세대의 연결로 교체됐다면 건드리지 않는다.
    pub(crate) async fn drop_conn_if_current(&self, host: &str, conn: &Arc<SftpConn>) {
        let Some(slot) = self.entry_opt(host) else {
            return;
        };
        let taken = {
            let mut inner = slot.inner.lock().unwrap();
            if inner.conn.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
                inner.conn.take()
            } else {
                None
            }
        };
        if let Some(c) = taken {
            c.shutdown().await;
        }
    }

    /// 연결을 얻어 작업 `f` 를 실행한다. 연결 단절성 오류면 1회 재연결 후 재시도한다.
    /// `path` 는 NotFound/PermissionDenied 메시지에 사용한다.
    async fn with_conn<F, Fut, T>(&self, host: &str, path: &str, f: F) -> Result<T, RemoteError>
    where
        F: Fn(Arc<SftpConn>) -> Fut,
        Fut: Future<Output = Result<T, SftpError>>,
    {
        for attempt in 0..2 {
            let conn = self.ensure_conn(host).await?;
            match f(conn.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if attempt == 0 && (sftp_is_conn_error(&e) || sftp_is_timeout(&e)) {
                        if sftp_is_conn_error(&e) {
                            self.drop_conn_if_current(host, &conn).await;
                        }
                        continue;
                    }
                    return Err(map_sftp_error(e, path));
                }
            }
        }
        unreachable!()
    }

    async fn open_file_async(
        self: &Arc<Self>,
        uri: &RemoteUri,
    ) -> Result<Arc<dyn RandomRead>, RemoteError> {
        let host = uri.host.clone();
        let path = uri.path.clone();
        let (conn, handle, size) = self
            .with_conn(&host, &path, |conn| {
                let path = path.clone();
                async move {
                    let (handle, size) = open_remote_file(&conn, &path).await?;
                    Ok::<_, SftpError>((conn, handle, size))
                }
            })
            .await?;
        self.add_file_ref(&host);
        Ok(Arc::new(RemoteFile::new(
            self.clone(),
            host,
            path,
            conn,
            handle,
            size,
        )))
    }

    fn add_file_ref(&self, host: &str) {
        if let Some(slot) = self.entry_opt(host) {
            let mut inner = slot.inner.lock().unwrap();
            inner.users += 1;
            inner.last_used = Instant::now();
        }
    }

    async fn release_async(&self, host: &str) {
        let Some(slot) = self.entry_opt(host) else {
            return;
        };
        let taken = {
            let mut inner = slot.inner.lock().unwrap();
            inner.users = inner.users.saturating_sub(1);
            inner.last_used = Instant::now();
            if inner.users == 0 {
                inner.conn.take()
            } else {
                None
            }
        };
        if let Some(c) = taken {
            c.shutdown().await;
        }
    }

    async fn list_dir_async(
        &self,
        host: &str,
        path: &str,
    ) -> Result<Vec<RemoteEntry>, RemoteError> {
        self.with_conn(host, path, |conn| {
            let path = path.to_string();
            async move { sftp_list_dir(&conn.session, path.as_str()).await }
        })
        .await
    }

    async fn home_dir_async(&self, host: &str) -> Result<String, RemoteError> {
        self.with_conn(host, ".", |conn| async move {
            sftp_real_path(&conn.session, ".").await
        })
        .await
    }
}

/// 원격 파일을 열고 크기를 조회한다. 반환: (SFTP 핸들, 크기 힌트).
pub(crate) async fn open_remote_file(
    conn: &SftpConn,
    path: &str,
) -> Result<(String, Option<u64>), SftpError> {
    let handle = conn
        .session
        .open(path, OpenFlags::READ, FileAttributes::empty())
        .await?
        .handle;
    // fstat 실패는 크기 불명으로만 처리하고 open 자체는 성공으로 본다.
    let size = match conn.session.fstat(handle.as_str()).await {
        Ok(attrs) => attrs.attrs.size,
        Err(_) => None,
    };
    Ok((handle, size))
}

async fn sftp_list_dir(
    session: &Arc<RawSftpSession>,
    path: &str,
) -> Result<Vec<RemoteEntry>, SftpError> {
    let dir_handle = session.opendir(path).await?.handle;
    let mut entries = Vec::new();
    let result = loop {
        match session.readdir(dir_handle.as_str()).await {
            Ok(name) => {
                for f in name.files {
                    if f.filename == "." || f.filename == ".." || f.filename.is_empty() {
                        continue;
                    }
                    entries.push(RemoteEntry {
                        name: f.filename,
                        is_dir: f.attrs.is_dir(),
                        size: f.attrs.size.unwrap_or(0),
                        mtime: f.attrs.mtime.map(u64::from),
                    });
                }
            }
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break Ok(()),
            Err(e) => break Err(e),
        }
    };
    let _ = session.close(dir_handle.as_str()).await;
    result?;

    // 디렉터리 우선, 그다음 이름순.
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

async fn sftp_real_path(session: &Arc<RawSftpSession>, path: &str) -> Result<String, SftpError> {
    let name = session.realpath(path).await?;
    match name.files.first() {
        Some(file) => Ok(file.filename.clone()),
        None => Err(SftpError::UnexpectedBehavior(
            "realpath 응답이 비어 있습니다".to_string(),
        )),
    }
}

/// 유휴 연결을 주기적으로 회수하는 백그라운드 태스크.
async fn reaper_loop(weak: Weak<PoolInner>) {
    loop {
        tokio::time::sleep(REAPER_INTERVAL).await;
        let Some(inner) = weak.upgrade() else {
            break;
        };
        let slots: Vec<Arc<HostSlot>> = inner.hosts.lock().unwrap().values().cloned().collect();
        for slot in slots {
            let taken = {
                let mut host_inner = slot.inner.lock().unwrap();
                let kill = match &host_inner.conn {
                    Some(c) if c.dead.load(Ordering::Relaxed) => true,
                    Some(_) if host_inner.users == 0 => {
                        host_inner.last_used.elapsed() >= IDLE_TIMEOUT
                    }
                    _ => false,
                };
                if kill { host_inner.conn.take() } else { None }
            };
            if let Some(c) = taken {
                c.shutdown().await;
            }
        }
    }
}

/// SFTP 로 원격 파일을 여는 풀.
///
/// 내부에 멀티스레드 토키오 런타임(워커 2개)을 소유한다.
/// 모든 메서드는 블로킹이며, 무거운 호출은 호출 측에서 `spawn_blocking` 등으로 감싸는 것을 권장한다.
pub struct SftpPool {
    inner: Arc<PoolInner>,
}

impl SftpPool {
    pub fn new(transport: Transport) -> SftpPool {
        SftpPool {
            inner: PoolInner::new(transport),
        }
    }

    /// URI 의 원격 파일을 `RandomRead` 로 연다.
    /// 연결이 없으면 새로 만들고(15초 타임아웃), 성공 시 호스트 참조가 1 증가한다.
    pub fn open_file(&self, uri: &RemoteUri) -> Result<Arc<dyn RandomRead>, RemoteError> {
        let inner = self.inner.clone();
        inner.block_on(inner.open_file_async(uri))
    }

    /// `open_file` 로 얻은 파일을 닫을 때 호출한다.
    /// 호스트의 파일 참조가 0이 되면 ssh 프로세스를 종료한다.
    pub fn release(&self, host: &str) {
        let inner = self.inner.clone();
        inner.block_on(inner.release_async(host));
    }

    /// 원격 디렉터리 목록. 이름순, 디렉터리 먼저, `.`/`..` 제외.
    pub fn list_dir(&self, host: &str, path: &str) -> Result<Vec<RemoteEntry>, RemoteError> {
        let inner = self.inner.clone();
        inner.block_on(inner.list_dir_async(host, path))
    }

    /// 로그인 셸의 현재 디렉터리(`realpath "."`).
    pub fn home_dir(&self, host: &str) -> Result<String, RemoteError> {
        let inner = self.inner.clone();
        inner.block_on(inner.home_dir_async(host))
    }
}
