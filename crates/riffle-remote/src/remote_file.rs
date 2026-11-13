//! riffle_core::RandomRead 계약을 SFTP 연결 위에 구현한다.
//!
//! - 큰 읽기(1MiB 이상): 256KiB 단위 요청을 최대 32개까지 파이프라이닝한다.
//! - 작은 읽기: 256KiB 블록 단위 LRU 캐시(최대 64MiB)를 거친다.
//! - 캐시는 `stat_len` 이 바뀌면 경계 블록만 무효화한다 (append 시 tail 만 stale).
//! - 연결 단절이 감지되면 1회 재연결 후 재시도한다.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use riffle_core::RandomRead;
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;
use tokio::task::JoinSet;

use crate::block_cache::BlockCache;
use crate::conn::SftpConn;
use crate::error::{
    RemoteError, map_sftp_error, remote_to_io, sftp_is_conn_error, sftp_is_timeout, sftp_to_io,
};
use crate::pool::{PoolInner, open_remote_file};
use crate::uri::RemoteUri;

/// 이 크기 이상의 read_at 은 캐시를 거치지 않고 직접 파이프라이닝한다.
const LARGE_READ: usize = 1024 * 1024;
/// 캐시 블록 크기.
const BLOCK_SIZE: u64 = 256 * 1024;
/// 블록 캐시 최대 총량.
const CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// 동시에 진행하는 SFTP read 요청 상한.
const READ_PARALLEL: usize = 32;
/// 한 요청 범위를 채우기 위한 short read 후속 요청 상한.
const FILL_LOOP_MAX: u32 = 16;
/// 크기를 모를 때 쓰는 센티널.
const UNKNOWN_SIZE: u64 = u64::MAX;

/// 작업 범위를 블록 경계로 자른 조각.
struct Piece {
    /// 블록 시작 오프셋 (BLOCK_SIZE 정렬).
    bs: u64,
    /// 이 조각이 필요한 파일 범위 [ps, pe).
    ps: u64,
    pe: u64,
}

/// 캐시가 채우지 못한 범위. 파일이 캐시된 블록을 가져온 뒤 커진 경우 등에 생긴다.
struct Gap {
    /// 대응하는 pieces 인덱스.
    piece: usize,
    file_off: u64,
    len: u32,
    /// work 안에서의 쓰기 위치.
    buf_idx: usize,
}

struct FileState {
    conn: Arc<SftpConn>,
    handle: String,
}

pub(crate) struct RemoteFile {
    inner: Arc<PoolInner>,
    host: String,
    path: String,
    uri: String,
    /// 현재 연결 세대와 그 위의 SFTP 핸들.
    state: StdMutex<FileState>,
    /// 마지막으로 확인한 파일 크기. UNKNOWN_SIZE 면 모르는 상태.
    size: AtomicU64,
    cache: StdMutex<BlockCache>,
}

impl RemoteFile {
    pub(crate) fn new(
        inner: Arc<PoolInner>,
        host: String,
        path: String,
        conn: Arc<SftpConn>,
        handle: String,
        size: Option<u64>,
    ) -> Self {
        let uri = RemoteUri {
            host: host.clone(),
            path: path.clone(),
        }
        .to_string();
        Self {
            inner,
            host,
            path,
            uri,
            state: StdMutex::new(FileState { conn, handle }),
            size: AtomicU64::new(size.unwrap_or(UNKNOWN_SIZE)),
            cache: StdMutex::new(BlockCache::new(CACHE_MAX_BYTES)),
        }
    }

    fn current(&self) -> (Arc<SftpConn>, String) {
        let s = self.state.lock().unwrap();
        (s.conn.clone(), s.handle.clone())
    }

    /// 파일 크기가 바뀌었을 때 경계 블록만 캐시에서 무효화한다.
    /// append 면 예전 EOF 를 포함하던 마지막 블록이 stale 이므로 그것만 지운다.
    fn note_size(&self, new: u64) {
        let old = self.size.swap(new, Ordering::Relaxed);
        if old == new || old == UNKNOWN_SIZE {
            return;
        }
        let mut keys = Vec::new();
        for &b in &[old, new] {
            if b == 0 {
                continue;
            }
            let blk = b / BLOCK_SIZE * BLOCK_SIZE;
            keys.push(blk);
            // b 가 블록 경계에 정확히 걸리면 직전 블록도 경계 블록이다.
            if b % BLOCK_SIZE == 0 && blk >= BLOCK_SIZE {
                keys.push(blk - BLOCK_SIZE);
            }
        }
        self.cache.lock().unwrap().invalidate(keys);
    }

    /// 연결 단절 후 재연결하고 파일 핸들을 다시 연다.
    async fn reconnect(&self) -> Result<(), RemoteError> {
        let old = self.current().0;
        self.inner.drop_conn_if_current(&self.host, &old).await;
        let conn = self.inner.ensure_conn(&self.host).await?;
        let (handle, size) = open_remote_file(&conn, &self.path)
            .await
            .map_err(|e| map_sftp_error(e, &self.path))?;
        // 새 연결에서는 파일 내용이 달라졌을 수 있으므로 캐시를 비운다.
        self.cache.lock().unwrap().clear();
        self.size.store(UNKNOWN_SIZE, Ordering::Relaxed);
        *self.state.lock().unwrap() = FileState { conn, handle };
        if let Some(s) = size {
            self.note_size(s);
        }
        Ok(())
    }

    async fn stat_len_async(&self) -> io::Result<u64> {
        for attempt in 0..2 {
            let (conn, handle) = self.current();
            match conn.session.fstat(handle.as_str()).await {
                Ok(attrs) => {
                    let s = attrs.attrs.size.unwrap_or(0);
                    self.note_size(s);
                    return Ok(s);
                }
                Err(e) => {
                    if attempt == 0 && (sftp_is_conn_error(&e) || sftp_is_timeout(&e)) {
                        if sftp_is_conn_error(&e) {
                            self.reconnect().await.map_err(remote_to_io)?;
                        }
                        continue;
                    }
                    return Err(sftp_to_io(e));
                }
            }
        }
        unreachable!()
    }

    async fn read_at_async(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        // 크기 힌트로 클램프하지 않는다: 파일이 자란 뒤 stat_len 이 갱신되기 전에도
        // 새 데이터를 읽을 수 있어야 하므로 EOF 판정은 서버 응답에 맡긴다.
        let work = buf;
        for attempt in 0..2 {
            let (conn, handle) = self.current();
            match self.dispatch(&conn, &handle, work, offset).await {
                Ok(n) => return Ok(n),
                Err(e) => {
                    if attempt == 0 && (sftp_is_conn_error(&e) || sftp_is_timeout(&e)) {
                        if sftp_is_conn_error(&e) {
                            self.reconnect().await.map_err(remote_to_io)?;
                        }
                        continue;
                    }
                    return Err(sftp_to_io(e));
                }
            }
        }
        unreachable!()
    }

    async fn dispatch(
        &self,
        conn: &SftpConn,
        handle: &str,
        work: &mut [u8],
        offset: u64,
    ) -> Result<usize, SftpError> {
        if work.len() >= LARGE_READ {
            self.read_large(conn, handle, work, offset).await
        } else {
            self.read_small(conn, handle, work, offset).await
        }
    }

    /// 큰 읽기: work 를 read_chunk 단위로 나눠 최대 32개까지 파이프라이닝한다.
    async fn read_large(
        &self,
        conn: &SftpConn,
        handle: &str,
        work: &mut [u8],
        offset: u64,
    ) -> Result<usize, SftpError> {
        let chunk = conn.read_chunk as u64;
        let total = work.len() as u64;
        let nchunks = total.div_ceil(chunk) as usize;
        let mut ranges = Vec::with_capacity(nchunks);
        for i in 0..nchunks {
            let off = offset + i as u64 * chunk;
            let len = (total - i as u64 * chunk).min(chunk) as u32;
            ranges.push((off, len));
        }
        let results = fetch_ranges(conn, handle, &ranges).await;

        let mut filled = 0usize;
        let mut prefix_open = true;
        for (i, res) in results.into_iter().enumerate() {
            let data = res?;
            let pos = i * chunk as usize;
            let n = data.len().min(work.len() - pos);
            work[pos..pos + n].copy_from_slice(&data[..n]);
            if prefix_open {
                filled += n;
                if n < ranges[i].1 as usize {
                    prefix_open = false;
                }
            }
        }
        Ok(filled)
    }

    /// 작은 읽기: 블록 캐시를 거쳐 필요한 블록만 채운다.
    async fn read_small(
        &self,
        conn: &SftpConn,
        handle: &str,
        work: &mut [u8],
        offset: u64,
    ) -> Result<usize, SftpError> {
        let end = offset + work.len() as u64;
        let mut pieces = Vec::new();
        let mut bs = offset / BLOCK_SIZE * BLOCK_SIZE;
        while bs < end {
            let ps = offset.max(bs);
            let pe = end.min(bs + BLOCK_SIZE);
            pieces.push(Piece { bs, ps, pe });
            bs += BLOCK_SIZE;
        }
        let size = self.size.load(Ordering::Relaxed);

        // 1) 캐시 조회: 히트는 즉시 복사하고, 미스 블록은 모아 둔다.
        let mut covered = vec![0u32; pieces.len()];
        let mut missing: Vec<u64> = Vec::new();
        let mut gaps: Vec<Gap> = Vec::new();
        {
            let mut cache = self.cache.lock().unwrap();
            for (i, p) in pieces.iter().enumerate() {
                if let Some(block) = cache.get(p.bs) {
                    apply_block(&mut covered, &mut gaps, i, p, block, work, offset);
                } else {
                    missing.push(p.bs);
                }
            }
        }

        // 2) 미스 블록을 파이프라이닝으로 가져와 캐시에 넣고 복사한다.
        if !missing.is_empty() {
            let ranges: Vec<(u64, u32)> = missing
                .iter()
                .map(|&b| {
                    let flen = if size == UNKNOWN_SIZE {
                        BLOCK_SIZE
                    } else {
                        BLOCK_SIZE.min(size.saturating_sub(b))
                    };
                    (b, flen as u32)
                })
                .collect();
            let results = fetch_ranges(conn, handle, &ranges).await;
            let mut iter = results.into_iter();
            for &b in &missing {
                let res = iter.next().expect("missing 과 results 길이가 다르다");
                let data = res?;
                if let Some(i) = pieces.iter().position(|p| p.bs == b) {
                    apply_block(&mut covered, &mut gaps, i, &pieces[i], &data, work, offset);
                }
                if !data.is_empty() {
                    self.cache.lock().unwrap().insert(b, data);
                }
            }
        }

        // 3) 캐시가 채우지 못한 꼬리(파일이 캐시 이후 커진 경우)를 직접 읽는다.
        if !gaps.is_empty() {
            let ranges: Vec<(u64, u32)> = gaps.iter().map(|g| (g.file_off, g.len)).collect();
            let results = fetch_ranges(conn, handle, &ranges).await;
            for (g, res) in gaps.iter().zip(results) {
                let data = res?;
                let n = data.len().min(g.len as usize);
                work[g.buf_idx..g.buf_idx + n].copy_from_slice(&data[..n]);
                covered[g.piece] += n as u32;
            }
        }

        // 4) 앞에서부터 연속으로 채워진 길이를 반환한다.
        let mut filled = 0usize;
        for (i, p) in pieces.iter().enumerate() {
            filled += covered[i] as usize;
            if covered[i] < (p.pe - p.ps) as u32 {
                break;
            }
        }
        Ok(filled)
    }
}

/// 블록 데이터 `[bs, bs+data.len())` 와 피스 `[ps,pe)` 의 교집합을 work 에 복사하고
/// covered/gaps 를 갱신한다. 데이터는 항상 블록 시작부터 연속이라고 가정한다.
fn apply_block(
    covered: &mut [u32],
    gaps: &mut Vec<Gap>,
    piece_idx: usize,
    p: &Piece,
    data: &[u8],
    work: &mut [u8],
    offset: u64,
) {
    let avail_end = p.bs + data.len() as u64;
    let cov_end = p.pe.min(avail_end);
    if cov_end > p.ps {
        let buf_idx = (p.ps - offset) as usize;
        let src_idx = (p.ps - p.bs) as usize;
        let n = (cov_end - p.ps) as usize;
        work[buf_idx..buf_idx + n].copy_from_slice(&data[src_idx..src_idx + n]);
        covered[piece_idx] = n as u32;
    }
    // 피스 안에서 아직 못 채운 꼬리. cov_end < ps 이면 피스 전체가 미충족이다.
    let gap_start = cov_end.max(p.ps);
    if gap_start < p.pe {
        gaps.push(Gap {
            piece: piece_idx,
            file_off: gap_start,
            len: (p.pe - gap_start) as u32,
            buf_idx: (gap_start - offset) as usize,
        });
    }
}

/// 범위 목록을 읽어 각 결과를 순서대로 돌려준다.
/// 최대 READ_PARALLEL 개까지 동시 요청하고, 블록 내 short read 는 이어서 채운다.
async fn fetch_ranges(
    conn: &SftpConn,
    handle: &str,
    ranges: &[(u64, u32)],
) -> Vec<Result<Vec<u8>, SftpError>> {
    let n = ranges.len();
    let mut out: Vec<Option<Result<Vec<u8>, SftpError>>> = Vec::new();
    out.resize_with(n, || None);
    if n == 0 {
        return Vec::new();
    }

    let session = conn.session.clone();
    let handle = handle.to_string();
    let chunk = conn.read_chunk;
    let mut set = JoinSet::new();
    let mut next = 0usize;
    let mut finished = 0usize;
    while finished < n {
        while next < n && next - finished < READ_PARALLEL {
            let idx = next;
            let (off, len) = ranges[next];
            let s = session.clone();
            let h = handle.clone();
            set.spawn(async move { (idx, fetch_one(&s, &h, off, len, chunk).await) });
            next += 1;
        }
        match set.join_next().await {
            Some(Ok((i, res))) => {
                out[i] = Some(res);
                finished += 1;
            }
            // 태스크 패닉: 해당 슬롯은 None 으로 남겨 최종 변환에서 오류가 된다.
            Some(Err(_)) => finished += 1,
            None => break,
        }
    }

    out.into_iter()
        .map(|o| {
            o.unwrap_or_else(|| {
                Err(SftpError::UnexpectedBehavior(
                    "읽기 작업이 중단되었습니다".to_string(),
                ))
            })
        })
        .collect()
}

/// 한 범위를 채울 때까지 read 를 반복한다. EOF/빈 응답이면 거기까지의 데이터를 반환한다.
async fn fetch_one(
    session: &RawSftpSession,
    handle: &str,
    off: u64,
    len: u32,
    chunk: u32,
) -> Result<Vec<u8>, SftpError> {
    let mut out = Vec::with_capacity(len as usize);
    let mut cur = off;
    let mut remain = len as u64;
    let mut iters = 0u32;
    while remain > 0 && iters < FILL_LOOP_MAX {
        iters += 1;
        let req = remain.min(chunk as u64) as u32;
        match session.read(handle, cur, req).await {
            Ok(data) => {
                if data.data.is_empty() {
                    break;
                }
                let n = data.data.len() as u64;
                out.extend_from_slice(&data.data);
                cur += n;
                remain = remain.saturating_sub(n);
            }
            Err(SftpError::Status(status)) if status.status_code == StatusCode::Eof => break,
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

impl RandomRead for RemoteFile {
    fn stat_len(&self) -> io::Result<u64> {
        self.inner.block_on(self.stat_len_async())
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.inner.block_on(self.read_at_async(buf, offset))
    }

    fn uri(&self) -> String {
        self.uri.clone()
    }
}
