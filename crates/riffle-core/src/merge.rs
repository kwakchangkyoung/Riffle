//! 여러 로그 파일을 타임스탬프 기준으로 섞은 가상 병합 뷰.
//!
//! 병합 결과 K(=CHECKPOINT_INTERVAL)줄마다 체크포인트(각 원본의 "다음에 읽을 줄 번호"와
//! 상속용 마지막 유효 타임스탬프)를 저장하는 희소 병합 인덱스를 만든다.
//! 임의 위치 읽기는 가장 가까운 체크포인트부터 최대 K-1줄의 k-way 병합을 재실행한다.
//!
//! 병합 규칙:
//! - 각 원본은 줄 순서대로만 소비한다 (원본이 시간순이 아니어도 순서를 바꾸지 않는다).
//! - 타임스탬프가 없는 줄은 같은 원본의 바로 앞 줄의 유효 타임스탬프를 상속한다.
//!   원본 첫 줄들에 타임스탬프가 없으면 -∞(맨 앞)으로 본다.
//! - 동일 타임스탬프는 원본 순번이 작은 쪽이 먼저 온다 (안정 정렬).
//! - 병합 대상은 new() 이후 build() 시작 시점의 스냅샷(각 원본의 확인된 줄 수·길이)이다.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::Result;
use crate::index::CHECKPOINT_INTERVAL;
use crate::lines::LogFile;
use crate::timestamp::{NEG_INF_TS, TsContext, parse_timestamp, year_of_unix_secs};

/// 병합 구축 시 원본별 순차 읽기 버퍼 크기 (16MB)
const BUILD_BUF_SIZE: usize = 16 * 1024 * 1024;
/// resolve/위치 변환 시 커서 복원용 읽기 버퍼 크기 (256KB)
const READ_BUF_SIZE: usize = 256 * 1024;

/// 병합 결과 한 줄의 원본 위치
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergedLine {
    /// 원본 순번 (병합 선택 순서)
    pub source: u16,
    /// 원본 내 줄 번호 (0-based)
    pub line: u64,
}

/// 병합 인덱스 구축 진행 상태
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeProgress {
    /// 현재까지 병합 순서가 확정된 줄 수
    pub processed_lines: u64,
    /// 전체 원본 줄 수 합
    pub total_lines: u64,
    /// 구축 완료 여부
    pub done: bool,
}

/// 체크포인트의 원본별 상태.
/// last_ts는 타임스탬프가 없는 줄의 상속 기준이라
/// 체크포인트에서 병합을 정확히 재개하는 데 필요하다.
#[derive(Debug, Clone, Copy)]
struct CpEntry {
    /// 병합 위치 k*K 에서 해당 원본의 다음 소비 줄 번호
    next_line: u64,
    /// 해당 원본에서 마지막으로 소비한 줄의 유효 ts (없으면 NEG_INF_TS)
    last_ts: i64,
}

/// 원본 한 개를 순차 스트리밍하며 줄별 유효 타임스탬프를 산출하는 커서.
/// 각 줄의 첫 64바이트(head)만 보관하므로 줄 길이와 무관하게 메모리는 버퍼 수준이다.
struct SourceCursor {
    file: Arc<LogFile>,
    /// 파싱 컨텍스트 (syslog 기본 연도는 원본 파일 수정 시각의 연도)
    ctx: TsContext,
    /// 스냅샷 끝 오프셋 (이 이후는 읽지 않는다)
    end: u64,
    /// 스냅샷 줄 수
    line_cap: u64,
    /// 다음 후보 줄 번호
    next_line: u64,
    /// 후보 줄의 시작 오프셋
    line_off: u64,
    /// 후보 줄의 앞 최대 64바이트
    head: Vec<u8>,
    /// 직전에 소비한 줄의 유효 ts (타임스탬프 상속 기준)
    last_ts: i64,
    /// 후보 줄의 유효 ts (첫 peek 때 계산해 캐시)
    cur_ts: i64,
    /// 읽기 버퍼
    buf: Vec<u8>,
    /// buf[0]의 절대 오프셋
    buf_off: u64,
    /// buf 내 유효 바이트 수
    buf_len: usize,
    /// 소진 여부
    done: bool,
}

impl SourceCursor {
    /// line_no 줄을 후보로 하는 커서를 만든다.
    /// last_ts는 상속 기준 유효 타임스탬프다 (체크포인트에 저장된 값).
    /// line_no >= line_cap이거나 줄 시작 오프셋을 알 수 없으면 소진 상태로 만든다.
    fn at_line(
        file: Arc<LogFile>,
        entry: CpEntry,
        ctx: TsContext,
        end: u64,
        line_cap: u64,
        buf_size: usize,
    ) -> Result<SourceCursor> {
        let mut cur = SourceCursor {
            file,
            ctx,
            end,
            line_cap,
            next_line: entry.next_line,
            line_off: 0,
            head: Vec::with_capacity(64),
            last_ts: entry.last_ts,
            cur_ts: NEG_INF_TS,
            buf: vec![0u8; buf_size],
            buf_off: 0,
            buf_len: 0,
            done: entry.next_line >= line_cap,
        };
        if cur.done {
            return Ok(cur);
        }
        let off = if entry.next_line == 0 {
            Some(0)
        } else {
            cur.file
                .index()
                .line_start(cur.file.source(), entry.next_line)
        };
        match off {
            Some(o) if o < end => {
                cur.line_off = o;
                cur.load_head()?;
                if cur.head.is_empty() {
                    cur.done = true;
                    return Ok(cur);
                }
                cur.cur_ts = parse_timestamp(&cur.head, &cur.ctx).unwrap_or(cur.last_ts);
            }
            _ => cur.done = true,
        }
        Ok(cur)
    }

    /// 후보 줄의 유효 타임스탬프를 반환한다 (소진 시 None).
    #[inline]
    fn peek(&self) -> Option<i64> {
        if self.done { None } else { Some(self.cur_ts) }
    }

    /// 후보 줄을 소비하고 그 유효 ts를 반환한다. 커서는 다음 줄로 전진한다.
    /// 호출 전 peek()이 Some이어야 한다.
    fn pop(&mut self) -> Result<i64> {
        let ts = self.cur_ts;
        self.last_ts = ts;
        let next_off = self.find_newline_end()?;
        self.next_line += 1;
        if self.next_line >= self.line_cap || next_off >= self.end {
            self.done = true;
            return Ok(ts);
        }
        self.line_off = next_off;
        self.load_head()?;
        if self.head.is_empty() {
            // 스냅샷 이후 파일이 외부에서 잘린 경우 소진 처리
            self.done = true;
            return Ok(ts);
        }
        self.cur_ts = parse_timestamp(&self.head, &self.ctx).unwrap_or(ts);
        Ok(ts)
    }

    /// line_off 이후 첫 '\n'의 다음 위치(절대 오프셋)를 반환한다. 없으면 end.
    fn find_newline_end(&mut self) -> Result<u64> {
        let mut pos = self.line_off;
        loop {
            if pos >= self.end {
                return Ok(self.end);
            }
            if pos < self.buf_off || pos >= self.buf_off + self.buf_len as u64 {
                let want = (self.end - pos).min(self.buf.len() as u64) as usize;
                self.buf_len = self.file.source().read_at_raw(&mut self.buf[..want], pos)?;
                self.buf_off = pos;
                if self.buf_len == 0 {
                    return Ok(self.end);
                }
            }
            let rel = (pos - self.buf_off) as usize;
            match memchr::memchr(b'\n', &self.buf[rel..self.buf_len]) {
                Some(nl) => return Ok(self.buf_off + (rel + nl) as u64 + 1),
                None => pos = self.buf_off + self.buf_len as u64,
            }
        }
    }

    /// line_off의 최대 64바이트를 head에 채운다.
    /// buf 창에 있는 부분은 재사용하고 부족분은 직접 읽는다.
    fn load_head(&mut self) -> Result<()> {
        self.head.clear();
        let want = (self.end - self.line_off).min(64) as usize;
        if want == 0 {
            return Ok(());
        }
        if self.line_off >= self.buf_off && self.line_off < self.buf_off + self.buf_len as u64 {
            let rel = (self.line_off - self.buf_off) as usize;
            let avail = (self.buf_len - rel).min(want);
            self.head.extend_from_slice(&self.buf[rel..rel + avail]);
        }
        if self.head.len() < want {
            let off = self.line_off + self.head.len() as u64;
            let mut tail = [0u8; 64];
            let n = self
                .file
                .source()
                .read_at_raw(&mut tail[..want - self.head.len()], off)?;
            self.head.extend_from_slice(&tail[..n]);
        }
        // head는 현재 줄까지만: 다음 줄의 타임스탬프가 섞이면 안 된다
        if let Some(nl) = self.head.iter().position(|&b| b == b'\n') {
            self.head.truncate(nl);
        }
        Ok(())
    }
}

/// 후보 중 (유효 ts, 원본 순번)이 가장 작은 커서의 인덱스를 반환한다.
/// ts가 같으면 순번이 작은 원본이 우선한다 (안정 정렬).
fn pick_min(cursors: &[SourceCursor]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, c) in cursors.iter().enumerate() {
        let Some(ts) = c.peek() else {
            continue;
        };
        match best {
            Some(b) if ts >= cursors[b].cur_ts => {}
            _ => best = Some(i),
        }
    }
    best
}

/// 희소 병합 인덱스.
/// 병합 결과 K줄마다 각 원본의 커서 상태(다음 줄 번호, 상속용 last_ts)를 저장하고,
/// 임의 위치 읽기는 가장 가까운 체크포인트에서 k-way 병합을 재실행한다.
/// 구축 중에도 확정된 앞부분은 resolve로 읽을 수 있다 (체크포인트 점진 공개).
pub struct MergeIndex {
    sources: Vec<Arc<LogFile>>,
    /// 원본별 파싱 컨텍스트 (syslog 기본 연도는 원본 수정 시각의 연도)
    ctxs: Vec<TsContext>,
    /// 원본별 스냅샷 줄 수. new()에서는 표시용 추정값, build() 시작 시 확정된다.
    line_caps: Vec<AtomicU64>,
    /// 원본별 스냅샷 끝 오프셋
    ends: Vec<AtomicU64>,
    total_lines: AtomicU64,
    /// checkpoints[k * nsrc + s] = 병합 위치 k*K에서 원본 s의 커서 상태
    checkpoints: RwLock<Vec<CpEntry>>,
    /// block_ts[k] = 병합 위치 k*K에 놓인 줄의 유효 타임스탬프
    block_ts: RwLock<Vec<i64>>,
    /// 확정된 병합 줄 수
    line_count: AtomicU64,
    /// build()가 스냅샷을 확정했는지 여부
    started: AtomicBool,
    done: AtomicBool,
}

impl MergeIndex {
    /// 원본 목록으로 병합 인덱스를 만든다. 인덱스 구축은 build()에서 수행한다.
    /// syslog 기본 연도는 원본 파일 수정 시각의 연도를 쓰고, 알 수 없는
    /// 원본(원격/익명)은 ctx.default_year를 쓴다.
    pub fn new(sources: Vec<Arc<LogFile>>, ctx: TsContext) -> MergeIndex {
        let ctxs = sources
            .iter()
            .map(|f| {
                let mut c = ctx;
                if let Some(secs) = f.source().mtime_secs() {
                    c.default_year = year_of_unix_secs(secs);
                }
                c
            })
            .collect();
        let line_caps = sources
            .iter()
            .map(|f| AtomicU64::new(f.index().line_count()))
            .collect();
        let ends = sources
            .iter()
            .map(|f| AtomicU64::new(f.source().len()))
            .collect();
        let nsrc = sources.len();
        // 체크포인트 0: 병합 시작 시점 = 모든 원본이 처음 상태
        let initial = vec![
            CpEntry {
                next_line: 0,
                last_ts: NEG_INF_TS,
            };
            nsrc
        ];
        MergeIndex {
            sources,
            ctxs,
            line_caps,
            ends,
            total_lines: AtomicU64::new(0),
            checkpoints: RwLock::new(initial),
            block_ts: RwLock::new(Vec::new()),
            line_count: AtomicU64::new(0),
            started: AtomicBool::new(false),
            done: AtomicBool::new(false),
        }
    }

    /// 원본 목록을 반환한다.
    pub fn sources(&self) -> &[Arc<LogFile>] {
        &self.sources
    }

    /// 원본 수를 반환한다.
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    /// 원본 s의 스냅샷 줄 수를 반환한다 (build() 전이면 추정값).
    pub fn line_cap(&self, s: usize) -> u64 {
        self.line_caps
            .get(s)
            .map_or(0, |c| c.load(Ordering::Acquire))
    }

    /// 블로킹으로 병합 인덱스를 구축한다. 호출자가 별도 스레드에서 실행해야 한다.
    /// 원본 인덱싱이 끝난 뒤 호출하는 것을 전제로 하며, 그 전이면 각 원본의
    /// 확인된 줄 수를 스냅샷으로 사용한다.
    /// cancel이 true가 되면 그 시점까지 확정된 앞부분만 남기고 Ok(())로 중단한다.
    pub fn build(
        &self,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(MergeProgress),
    ) -> Result<()> {
        // 스냅샷 확정: 각 원본의 확인된 줄 수와 길이를 기록한다
        let mut total = 0u64;
        for (i, f) in self.sources.iter().enumerate() {
            let cap = f.index().line_count();
            self.line_caps[i].store(cap, Ordering::Release);
            self.ends[i].store(f.source().len(), Ordering::Release);
            total += cap;
        }
        self.total_lines.store(total, Ordering::Release);
        self.started.store(true, Ordering::Release);
        on_progress(self.progress());

        let mut cursors = self.make_cursors(0, BUILD_BUF_SIZE)?;
        let mut merged: u64 = 0;
        while let Some(src) = pick_min(&cursors) {
            // 체크포인트 0은 new()에서 이미 기록되어 있다
            if merged.is_multiple_of(CHECKPOINT_INTERVAL) && merged > 0 {
                self.push_checkpoint(&cursors);
            }
            let ts = cursors[src].pop()?;
            if merged.is_multiple_of(CHECKPOINT_INTERVAL) {
                self.block_ts
                    .write()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(ts);
            }
            merged += 1;
            self.line_count.store(merged, Ordering::Release);
            if merged.is_multiple_of(8192) && cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            if merged.is_multiple_of(65536) {
                on_progress(self.progress());
            }
        }
        self.done.store(true, Ordering::Release);
        on_progress(self.progress());
        Ok(())
    }

    /// 체크포인트 cp_idx 시점의 커서 상태로 각 원본 커서를 복원한다.
    fn make_cursors(&self, cp_idx: usize, buf_size: usize) -> Result<Vec<SourceCursor>> {
        let cps = self.checkpoints.read().unwrap_or_else(|p| p.into_inner());
        let nsrc = self.sources.len();
        self.sources
            .iter()
            .enumerate()
            .map(|(s, f)| {
                SourceCursor::at_line(
                    f.clone(),
                    cps[cp_idx * nsrc + s],
                    self.ctxs[s],
                    self.ends[s].load(Ordering::Acquire),
                    self.line_caps[s].load(Ordering::Acquire),
                    buf_size,
                )
            })
            .collect()
    }

    /// 현재 커서 상태(각 원본의 다음 줄 번호·상속용 last_ts)를 체크포인트로 공개한다.
    fn push_checkpoint(&self, cursors: &[SourceCursor]) {
        let mut cps = self.checkpoints.write().unwrap_or_else(|p| p.into_inner());
        for c in cursors {
            cps.push(CpEntry {
                next_line: c.next_line,
                last_ts: c.last_ts,
            });
        }
    }

    /// 현재 병합 진행 상태를 반환한다.
    pub fn progress(&self) -> MergeProgress {
        MergeProgress {
            processed_lines: self.line_count.load(Ordering::Acquire),
            total_lines: self.total_lines.load(Ordering::Acquire),
            done: self.done.load(Ordering::Acquire),
        }
    }

    /// 확정된 병합 줄 수를 반환한다 (구축 중에는 앞부분만).
    #[inline]
    pub fn line_count(&self) -> u64 {
        self.line_count.load(Ordering::Acquire)
    }

    /// 구축 완료 여부를 반환한다.
    #[inline]
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// 병합 위치 [start, start+count)의 (원본, 줄) 목록을 반환한다.
    /// 확인된 범위(구축 중이면 현재까지 확정된 앞부분)까지만 반환한다.
    pub fn resolve(&self, start: u64, count: u64) -> Result<Vec<MergedLine>> {
        let confirmed = self.line_count();
        if start >= confirmed || count == 0 {
            return Ok(Vec::new());
        }
        let want = (confirmed - start).min(count);
        let mut cursors =
            self.make_cursors((start / CHECKPOINT_INTERVAL) as usize, READ_BUF_SIZE)?;

        // start까지 전진
        let mut pos = (start / CHECKPOINT_INTERVAL) * CHECKPOINT_INTERVAL;
        while pos < start {
            let Some(src) = pick_min(&cursors) else {
                return Ok(Vec::new()); // 확정 범위와 상태가 어긋남 (외부 truncate 등)
            };
            cursors[src].pop()?;
            pos += 1;
        }

        let mut out = Vec::with_capacity(want as usize);
        while (out.len() as u64) < want {
            let Some(src) = pick_min(&cursors) else {
                break;
            };
            let line = cursors[src].next_line;
            cursors[src].pop()?;
            out.push(MergedLine {
                source: src as u16,
                line,
            });
        }
        Ok(out)
    }

    /// 원본별 오름차순 줄 번호 목록을 한 번의 병합 재실행으로
    /// 병합 위치 목록(오름차순)으로 변환한다 (k-way 변환).
    /// 스냅샷 밖 줄이나 build() 전에는 해당 줄을 건너뛴다.
    /// 검색/필터 결과 변환용: 원본별 결과가 이미 정렬되어 있으므로
    /// 변환 결과도 자연히 병합 위치 오름차순이 된다.
    pub fn map_positions(&self, per_source_lines: &[Vec<u64>]) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        let nsrc = self.sources.len();
        if per_source_lines.len() != nsrc || !self.started.load(Ordering::Acquire) {
            return Ok(out);
        }
        // 스냅샷 범위 안의 질의만 유효하다
        let mut eff_len = vec![0usize; nsrc];
        let mut any = false;
        for (s, lines) in per_source_lines.iter().enumerate() {
            let cap = self.line_caps[s].load(Ordering::Acquire);
            eff_len[s] = lines.partition_point(|&l| l < cap);
            any |= eff_len[s] > 0;
        }
        if !any {
            return Ok(out);
        }

        // 재실행 시작 체크포인트: 모든 원본에서 첫 질의 줄이 아직 소비되지 않은
        // 가장 늦은 k (cp[k][s].next_line <= hits[s][0]).
        let k = {
            let cps = self.checkpoints.read().unwrap_or_else(|p| p.into_inner());
            let ncps = cps.len() / nsrc;
            let mut lo = 0usize;
            let mut hi = ncps;
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let ok = per_source_lines
                    .iter()
                    .enumerate()
                    .all(|(s, lines)| eff_len[s] == 0 || cps[mid * nsrc + s].next_line <= lines[0]);
                if ok {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            lo.saturating_sub(1)
        };

        let mut cursors = self.make_cursors(k, READ_BUF_SIZE)?;
        let mut pos = k as u64 * CHECKPOINT_INTERVAL;
        let mut idx = vec![0usize; nsrc];
        while let Some(src) = pick_min(&cursors) {
            let line = cursors[src].next_line;
            cursors[src].pop()?;
            if idx[src] < eff_len[src] {
                let hits = &per_source_lines[src];
                // 커서가 소비한 줄과 질의 목록을 같은 순서로 맞춘다
                while idx[src] < eff_len[src] && hits[idx[src]] < line {
                    idx[src] += 1;
                }
                if idx[src] < eff_len[src] && hits[idx[src]] == line {
                    out.push(pos);
                    idx[src] += 1;
                }
            }
            pos += 1;
        }
        Ok(out)
    }

    /// 원본 줄 → 병합 위치 역변환 (체크포인트 이진 탐색 후 전진).
    /// 스냅샷 밖이거나 오류면 None.
    pub fn position_of(&self, source: u16, line: u64) -> Option<u64> {
        let nsrc = self.sources.len();
        if source as usize >= nsrc {
            return None;
        }
        let mut lists = vec![Vec::new(); nsrc];
        lists[source as usize].push(line);
        self.map_positions(&lists)
            .ok()
            .and_then(|v| v.first().copied())
    }

    /// 오름차순 병합 위치 목록을 (원본, 줄)로 변환한다.
    /// 반환값은 positions와 같은 길이이며, 확인 범위 밖 위치는 None이다.
    /// 같은 K 블록의 위치는 커서를 공유해 한 번만 재실행한다.
    pub fn resolve_many(&self, positions: &[u64]) -> Result<Vec<Option<MergedLine>>> {
        let mut out = vec![None; positions.len()];
        let confirmed = self.line_count();
        let mut i = 0usize;
        while i < positions.len() {
            let first = positions[i];
            if first >= confirmed {
                break;
            }
            let block = first / CHECKPOINT_INTERVAL;
            let block_end = (block + 1) * CHECKPOINT_INTERVAL;
            // 같은 블록에 속한 위치 구간 [i, j)
            let j = i + positions[i..].partition_point(|&p| p < block_end);
            let last = positions[j - 1];

            let mut cursors = self.make_cursors(block as usize, READ_BUF_SIZE)?;
            let mut pos = block * CHECKPOINT_INTERVAL;
            let mut target = i;
            while target < j && pos <= last {
                let Some(src) = pick_min(&cursors) else {
                    break;
                };
                let line = cursors[src].next_line;
                cursors[src].pop()?;
                if pos == positions[target] {
                    out[target] = Some(MergedLine {
                        source: src as u16,
                        line,
                    });
                    target += 1;
                }
                pos += 1;
            }
            i = j;
        }
        Ok(out)
    }

    /// 유효 타임스탬프가 ts_ms 이상인 첫 병합 위치를 반환한다.
    /// 각 원본이 내부적으로 시간순이면 정확하고, 아니면 근사치다.
    /// 없으면 확인된 줄 수(끝 위치)를 반환한다.
    pub fn position_at_time(&self, ts_ms: i64) -> u64 {
        self.position_at_time_inner(ts_ms)
            .unwrap_or_else(|_| self.line_count())
    }

    fn position_at_time_inner(&self, ts_ms: i64) -> Result<u64> {
        let confirmed = self.line_count();
        if confirmed == 0 {
            return Ok(0);
        }
        // block_ts[k] >= ts_ms 인 첫 블록 kb. 답은 블록 kb-1 안 또는 kb*K에 있다.
        let kb = {
            let bts = self.block_ts.read().unwrap_or_else(|p| p.into_inner());
            bts.partition_point(|&t| t < ts_ms)
        };
        if kb == 0 {
            return Ok(0);
        }
        let mut cursors = self.make_cursors(kb - 1, READ_BUF_SIZE)?;
        let mut pos = (kb - 1) as u64 * CHECKPOINT_INTERVAL;
        let bound = (kb as u64 * CHECKPOINT_INTERVAL).min(confirmed);
        while pos < bound {
            let Some(src) = pick_min(&cursors) else {
                break;
            };
            let ts = cursors[src].pop()?;
            if ts >= ts_ms {
                return Ok(pos);
            }
            pos += 1;
        }
        Ok(bound)
    }

    /// 검증용: 병합 결과의 유효 타임스탬프가 감소하는 위치의 수를 센다.
    /// 각 원본이 시간순이면 0이어야 한다.
    pub fn verify_order(&self) -> Result<u64> {
        if self.line_count() == 0 {
            return Ok(0);
        }
        let mut cursors = self.make_cursors(0, READ_BUF_SIZE)?;
        let mut violations = 0u64;
        let mut prev = NEG_INF_TS;
        while let Some(src) = pick_min(&cursors) {
            let ts = cursors[src].pop()?;
            if ts < prev {
                violations += 1;
            }
            prev = ts;
        }
        Ok(violations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64 as StdAtomicU64;

    /// 테스트용 RAII 임시 파일 도우미.
    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(prefix: &str, content: &[u8]) -> Self {
            static COUNTER: StdAtomicU64 = StdAtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "riffle_merge_test_{}_{}_{}.tmp",
                prefix,
                std::process::id(),
                id
            ));
            let mut f = File::create(&path).expect("임시 파일 생성 실패");
            f.write_all(content).expect("임시 파일 쓰기 실패");
            f.flush().expect("임시 파일 플러시 실패");
            TempFile { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn ctx() -> TsContext {
        TsContext {
            local_offset_minutes: 0,
            default_year: 2026,
        }
    }

    /// 내용으로 임시 파일을 만들고 인덱싱된 LogFile을 반환한다.
    fn open_indexed(prefix: &str, content: &str) -> (TempFile, Arc<LogFile>) {
        let tmp = TempFile::new(prefix, content.as_bytes());
        let file = Arc::new(LogFile::open(tmp.path()).expect("열기 실패"));
        file.build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("인덱싱 실패");
        (tmp, file)
    }

    /// 순진한 병합 시뮬레이션: 각 원본의 모든 줄을 읽어 유효 ts를 계산하고,
    /// (ts, 원본 순번) 기준 k-way 병합을 그대로 재현한다.
    fn naive_merge(files: &[Arc<LogFile>], ctx: &TsContext) -> Vec<MergedLine> {
        // 원본별 (줄 번호 → 유효 ts) 계산
        let mut ts_lists: Vec<Vec<i64>> = Vec::with_capacity(files.len());
        for f in files {
            let n = f.index().line_count();
            let lines = f.read_lines(0, n, None, None).expect("줄 읽기 실패");
            let mut last = NEG_INF_TS;
            let mut v = Vec::with_capacity(lines.len());
            for line in &lines {
                let ts = parse_timestamp(line.text.as_bytes(), ctx).unwrap_or(last);
                last = ts;
                v.push(ts);
            }
            ts_lists.push(v);
        }
        let mut heads = vec![0usize; files.len()];
        let mut out = Vec::new();
        loop {
            let mut best: Option<usize> = None;
            for (i, ts_list) in ts_lists.iter().enumerate() {
                if heads[i] >= ts_list.len() {
                    continue;
                }
                let ts = ts_list[heads[i]];
                match best {
                    Some(b) if ts >= ts_lists[b][heads[b]] => {}
                    _ => best = Some(i),
                }
            }
            let Some(s) = best else { break };
            out.push(MergedLine {
                source: s as u16,
                line: heads[s] as u64,
            });
            heads[s] += 1;
        }
        out
    }

    /// resolve 전체 결과가 순진 시뮬레이션과 완전히 같은지 검사한다.
    fn assert_full_match(index: &MergeIndex, expected: &[MergedLine]) {
        assert_eq!(index.line_count(), expected.len() as u64);
        // 앞/중간/끝 구간으로 나눠 resolve 검증
        let mut got = Vec::new();
        let mut pos = 0u64;
        while pos < expected.len() as u64 {
            let chunk = index.resolve(pos, 257).expect("resolve 실패");
            assert!(!chunk.is_empty());
            got.extend_from_slice(&chunk);
            pos += chunk.len() as u64;
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn merge_two_sorted_files() {
        let a = "2026-11-12T00:00:01Z a1\n2026-11-12T00:00:03Z a2\n2026-11-12T00:00:05Z a3\n";
        let b = "2026-11-12T00:00:02Z b1\n2026-11-12T00:00:04Z b2\n";
        let (_t1, fa) = open_indexed("m2a", a);
        let (_t2, fb) = open_indexed("m2b", b);

        let index = MergeIndex::new(vec![fa.clone(), fb.clone()], ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");

        let expected = naive_merge(&[fa, fb], &ctx());
        assert_eq!(expected.len(), 5);
        assert_full_match(&index, &expected);
        // 실제 순서: a1 b1 a2 b2 a3
        let got = index.resolve(0, 5).unwrap();
        assert_eq!(
            got,
            vec![
                MergedLine { source: 0, line: 0 },
                MergedLine { source: 1, line: 0 },
                MergedLine { source: 0, line: 1 },
                MergedLine { source: 1, line: 1 },
                MergedLine { source: 0, line: 2 },
            ]
        );
    }

    #[test]
    fn merge_no_ts_lines_inherit() {
        // 타임스탬프 없는 줄(스택 트레이스)은 앞 줄에 붙어 다닌다
        let a = "2026-11-12T00:00:01Z a1\n    at stack frame\n    more trace\n2026-11-12T00:00:04Z a2\n";
        let b = "2026-11-12T00:00:02Z b1\n2026-11-12T00:00:03Z b2\n";
        let (_t1, fa) = open_indexed("mi_a", a);
        let (_t2, fb) = open_indexed("mi_b", b);

        let index = MergeIndex::new(vec![fa.clone(), fb.clone()], ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");

        let got = index.resolve(0, 10).unwrap();
        // a1(1s) b1(2s) b2(3s) 다음에 스택줄이 오면 안 되고,
        // a1 바로 뒤에 스택 트레이스가 붙어야 한다
        assert_eq!(
            got,
            vec![
                MergedLine { source: 0, line: 0 },
                MergedLine { source: 0, line: 1 },
                MergedLine { source: 0, line: 2 },
                MergedLine { source: 1, line: 0 },
                MergedLine { source: 1, line: 1 },
                MergedLine { source: 0, line: 3 },
            ]
        );
    }

    #[test]
    fn merge_leading_no_ts_is_neg_inf() {
        // 원본 첫 줄들에 타임스탬프가 없으면 맨 앞(-∞)으로 간다
        let a = "header no ts\n2026-11-12T00:00:05Z a2\n";
        let b = "2026-11-12T00:00:01Z b1\n";
        let (_t1, fa) = open_indexed("mn_a", a);
        let (_t2, fb) = open_indexed("mn_b", b);

        let index = MergeIndex::new(vec![fa.clone(), fb.clone()], ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");

        let got = index.resolve(0, 10).unwrap();
        assert_eq!(
            got,
            vec![
                MergedLine { source: 0, line: 0 },
                MergedLine { source: 1, line: 0 },
                MergedLine { source: 0, line: 1 },
            ]
        );
    }

    #[test]
    fn merge_same_ts_stable_by_source() {
        // 동일 타임스탬프는 원본 순서가 먼저다
        let a = "2026-11-12T00:00:01Z a1\n2026-11-12T00:00:01Z a2\n";
        let b = "2026-11-12T00:00:01Z b1\n";
        let c = "2026-11-12T00:00:01Z c1\n";
        let (_t1, fa) = open_indexed("ms_a", a);
        let (_t2, fb) = open_indexed("ms_b", b);
        let (_t3, fc) = open_indexed("ms_c", c);

        let index = MergeIndex::new(vec![fa.clone(), fb.clone(), fc.clone()], ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");

        let got = index.resolve(0, 10).unwrap();
        assert_eq!(
            got,
            vec![
                MergedLine { source: 0, line: 0 },
                MergedLine { source: 0, line: 1 },
                MergedLine { source: 1, line: 0 },
                MergedLine { source: 2, line: 0 },
            ]
        );
    }

    #[test]
    fn merge_unsorted_source_keeps_order() {
        // 원본이 시간 역행해도 원본 내부 순서는 바꾸지 않는다
        let a = "2026-11-12T00:00:05Z a1\n2026-11-12T00:00:01Z a2\n";
        let b = "2026-11-12T00:00:03Z b1\n";
        let (_t1, fa) = open_indexed("mu_a", a);
        let (_t2, fb) = open_indexed("mu_b", b);

        let index = MergeIndex::new(vec![fa.clone(), fb.clone()], ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");

        let expected = naive_merge(&[fa, fb], &ctx());
        assert_full_match(&index, &expected);
        // k-way 병합은 각 원본의 다음 줄만 비교한다: b1(3s) < a1(5s)이 먼저 나오고,
        // 시간 역행 줄 a2(1s)는 원본 순서를 지켜 a1 뒤에 나온다
        let got = index.resolve(0, 10).unwrap();
        assert_eq!(got[0], MergedLine { source: 1, line: 0 }); // b1(3s)
        assert_eq!(got[1], MergedLine { source: 0, line: 0 }); // a1(5s)
        assert_eq!(got[2], MergedLine { source: 0, line: 1 }); // a2(1s) — 원본 순서 유지
    }

    #[test]
    fn merge_checkpoint_boundary() {
        // K 배수 경계: 3개 파일, 각 1500줄 (합 4500줄 > 4*K)
        let mk = |base_sec: u64, tag: &str| {
            let mut s = String::new();
            for i in 0..1500u64 {
                s.push_str(&format!(
                    "2026-11-12T00:00:{:02}.000Z {tag}{i}\n",
                    base_sec + i % 60
                ));
            }
            s
        };
        let (_t1, fa) = open_indexed("mk_a", &mk(0, "a"));
        let (_t2, fb) = open_indexed("mk_b", &mk(1, "b"));
        let (_t3, fc) = open_indexed("mk_c", &mk(2, "c"));

        let files = vec![fa, fb, fc];
        let index = MergeIndex::new(files.clone(), ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");

        let expected = naive_merge(&files, &ctx());
        assert_eq!(expected.len(), 4500);
        assert_full_match(&index, &expected);

        // position_of 왕복: resolve(i) == (s,l) → position_of(s,l) == i
        for i in [0u64, 1, 1023, 1024, 1025, 2048, 4095, 4499] {
            let ml = index.resolve(i, 1).unwrap()[0];
            assert_eq!(index.position_of(ml.source, ml.line), Some(i), "i={i}");
        }
        // 전체 왕복 샘플
        for i in (0..expected.len() as u64).step_by(137) {
            let ml = expected[i as usize];
            assert_eq!(index.position_of(ml.source, ml.line), Some(i));
        }
    }

    #[test]
    fn merge_empty_and_single_source() {
        // 빈 원본 포함
        let (_t0, fe) = open_indexed("me_0", "");
        let a = "2026-11-12T00:00:01Z a1\n";
        let (_t1, fa) = open_indexed("me_1", a);

        let index = MergeIndex::new(vec![fe.clone(), fa.clone()], ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");
        assert_eq!(index.line_count(), 1);
        assert_eq!(
            index.resolve(0, 10).unwrap(),
            vec![MergedLine { source: 1, line: 0 }]
        );

        // 원본 1개
        let index1 = MergeIndex::new(vec![fa.clone()], ctx());
        index1
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");
        assert_eq!(index1.line_count(), 1);

        // 전부 빈 원본
        let (_t2, fe2) = open_indexed("me_2", "");
        let index2 = MergeIndex::new(vec![fe2, fe], ctx());
        index2
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");
        assert_eq!(index2.line_count(), 0);
        assert!(index2.resolve(0, 10).unwrap().is_empty());
    }

    #[test]
    fn merge_resolve_during_build() {
        // 인덱싱 중에도 확정된 앞부분은 resolve할 수 있다.
        // build를 중간에 멈추게 하는 대신, 커서를 직접 부분 공개해 시뮬레이션한다.
        let mut content = String::new();
        for i in 0..3000u64 {
            content.push_str(&format!("2026-11-12T00:00:{:02}.000Z l{i}\n", i % 60));
        }
        let (_t, f) = open_indexed("mp_0", &content);

        let index = Arc::new(MergeIndex::new(vec![f], ctx()));
        let idx = index.clone();
        let cancel = AtomicBool::new(false);
        let handle = std::thread::spawn(move || idx.build(&cancel, &mut |_| {}));
        // 첫 체크포인트(1024줄)가 공개될 때까지 대기 후 앞부분 읽기
        while index.line_count() < 1025 && !index.is_done() {
            std::thread::yield_now();
        }
        let got = index.resolve(0, 10).unwrap();
        assert_eq!(got.len(), 10);
        assert_eq!(got[0], MergedLine { source: 0, line: 0 });
        handle.join().unwrap().expect("병합 실패");
        assert_eq!(index.line_count(), 3000);
    }

    #[test]
    fn merge_position_at_time() {
        let a = "2026-11-12T00:00:01Z a1\n2026-11-12T00:00:03Z a2\n2026-11-12T00:00:05Z a3\n";
        let b = "2026-11-12T00:00:02Z b1\n2026-11-12T00:00:04Z b2\n";
        let (_t1, fa) = open_indexed("mt_a", a);
        let (_t2, fb) = open_indexed("mt_b", b);

        let index = MergeIndex::new(vec![fa, fb], ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");

        // 2026-11-12T00:00:00Z = 1_794_441_600_000
        // 1s → 0, 2s → 1, 2.5s → 2(a2 3s), 4s → 3(b2 4s), 5s → 4, 6s → 5(끝)
        assert_eq!(index.position_at_time(1_794_441_601_000), 0);
        assert_eq!(index.position_at_time(1_794_441_602_000), 1);
        assert_eq!(index.position_at_time(1_794_441_602_500), 2);
        assert_eq!(index.position_at_time(1_794_441_604_000), 3);
        assert_eq!(index.position_at_time(1_794_441_605_000), 4);
        assert_eq!(index.position_at_time(1_794_441_606_000), 5);
    }

    #[test]
    fn merge_map_positions_and_resolve_many() {
        let a = "2026-11-12T00:00:01Z a1\n2026-11-12T00:00:03Z a2\n";
        let b = "2026-11-12T00:00:02Z b1\n";
        let (_t1, fa) = open_indexed("mm_a", a);
        let (_t2, fb) = open_indexed("mm_b", b);

        let index = MergeIndex::new(vec![fa.clone(), fb.clone()], ctx());
        index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 실패");

        // a 줄 0,1 + b 줄 0 → 병합 위치 0,1,2 (k-way 변환 결과도 오름차순)
        let got = index.map_positions(&[vec![0, 1], vec![0]]).unwrap();
        assert_eq!(got, vec![0, 1, 2]);
        // 스냅샷 밖 줄은 무시된다
        let got2 = index.map_positions(&[vec![0, 99], vec![]]).unwrap();
        assert_eq!(got2, vec![0]);
        // b 줄 0 → 위치 1
        assert_eq!(index.position_of(1, 0), Some(1));

        // resolve_many: 확인 범위 밖 위치는 None
        let resolved = index.resolve_many(&[0, 2, 99]).unwrap();
        assert_eq!(
            resolved,
            vec![
                Some(MergedLine { source: 0, line: 0 }),
                Some(MergedLine { source: 0, line: 1 }),
                None,
            ]
        );
    }
}
