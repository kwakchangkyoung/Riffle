//! 파일 개요 스캔: 인덱싱이 끝난 파일을 한 번 훑어
//! 스크롤바 분포 지도(위치별 ERROR/WARN 수)와 시간 히스토그램용
//! 초 단위 집계를 만든다.
//!
//! 스캔 파이프라인은 search.rs 와 같은 구조다: I/O 스레드가 read_at 으로
//! 16MB 세그먼트를 읽어 메인 스레드에 넘기고, 메인 스레드가 줄 경계에 맞춰
//! rayon 서브청크로 나눠 병렬 평가한 뒤 순서대로 합친다.
//! 판정에는 각 줄의 앞부분만 필요하므로(레벨 200B, 타임스탬프 64B)
//! 세그먼트를 넘는 긴 줄도 머리 최대 200B 만 보관하면 된다.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::filter::{Level, line_level};
use crate::lines::LogFile;
use crate::timestamp::{TsContext, parse_timestamp, year_of_unix_secs};

/// 위치 분포 구간 수. 줄 번호를 이 개수의 구간으로 나눠 ERROR/WARN 수를 센다.
pub const POS_BINS: usize = 1000;

/// 개요 스캔 세그먼트 크기 (16MB, 검색과 동일)
const SEGMENT_SIZE: usize = 16 * 1024 * 1024;

/// 줄 판정에 필요한 머리 바이트 수 상한 (레벨 스캔 200B 기준)
const HEAD_CAP: usize = 200;

/// 초 단위 버킷. 같은 초에 귀속된 줄들의 집계다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecBucket {
    /// epoch 초
    pub sec: i64,
    /// 이 초에 귀속된 줄 수
    pub total: u32,
    /// 그 중 ERROR(ERROR|FATAL) 줄 수
    pub error: u32,
    /// 그 중 WARN(WARN|WARNING) 줄 수
    pub warn: u32,
    /// 이 초에 귀속된 첫 줄 번호 (0-based)
    pub first_line: u64,
}

/// 파일 개요: 위치 분포 + 시간 분포.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    /// 스캔으로 확인한 전체 줄 수
    pub line_count: u64,
    /// pos 구간별 ERROR 줄 수 (len = min(POS_BINS, 인덱스 줄 수))
    pub pos_error: Vec<u32>,
    /// pos 구간별 WARN 줄 수 (pos_error 와 같은 길이)
    pub pos_warn: Vec<u32>,
    /// 시간 버킷 목록 (sec 오름차순). 타임스탬프가 하나도 없으면 비어 있다.
    pub seconds: Vec<SecBucket>,
    /// seconds 버킷 하나가 덮는 초 수. 스캔 직후는 1(초 단위),
    /// coarsen 으로 묶으면 묶음 간격 g 가 된다.
    pub granularity_secs: i64,
}

/// timeline() 이 만드는 막대 하나.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeBucket {
    /// 이 막대가 덮는 구간의 시작 epoch 초
    pub start_sec: i64,
    pub total: u32,
    pub error: u32,
    pub warn: u32,
    /// 이 구간에 귀속된 첫 줄 번호. 빈 구간이면 0.
    pub first_line: u64,
}

/// 시간 히스토그램: start_sec 부터 bucket_secs 간격의 연속 막대 목록.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    /// 막대 하나가 덮는 초 수 ("보기 좋은" 간격 또는 방어용 임의 값)
    pub bucket_secs: i64,
    /// 첫 막대의 시작 epoch 초 (첫 데이터 시각을 bucket_secs 로 내림 정렬)
    pub start_sec: i64,
    /// 연속된 막대 목록. 데이터가 없는 구간도 total=0 으로 채운다.
    pub buckets: Vec<TimeBucket>,
}

/// "보기 좋은" 구간 크기 후보 (초)
const NICE_BUCKET_SECS: &[i64] = &[
    1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200, 10800, 21600, 43200, 86400,
];

impl Overview {
    /// 시간 분포를 target_buckets 이하의 막대로 묶은 히스토그램을 반환한다.
    /// 구간 크기는 NICE_BUCKET_SECS 중 막대 수가 target 이하가 되는 가장 작은 값이다.
    /// 단, granularity_secs 보다 작거나 그 배수가 아닌 간격은 제외한다
    /// (coarsen 된 데이터에서 막대 경계가 버킷 경계와 어긋나지 않게).
    /// 어떤 후보로도 target 이하가 안 되면 target*2 이하가 되는 가장 작은 후보를 쓰고,
    /// 그것도 없으면 span/(target*2) 를 granularity 의 배수로 올림한 임의 간격으로 방어한다.
    /// seconds 가 비어 있거나 target_buckets 가 0 이면 빈 결과를 반환한다.
    pub fn timeline(&self, target_buckets: usize) -> Timeline {
        if self.seconds.is_empty() || target_buckets == 0 {
            return Timeline {
                bucket_secs: 1,
                start_sec: 0,
                buckets: Vec::new(),
            };
        }
        let first = self.seconds[0].sec;
        let last = self.seconds[self.seconds.len() - 1].sec;
        let cap = (target_buckets * 2) as i64;
        let g = self.granularity_secs.max(1);
        // 간격 s 로 만들어지는 실제 막대 수 (시작을 s 단위로 내림 정렬한 값 기준)
        let bars = |s: i64| -> i64 {
            let start = first - first.rem_euclid(s);
            (last - start) / s + 1
        };
        let nice = |limit: i64| {
            NICE_BUCKET_SECS
                .iter()
                .copied()
                .filter(|&s| s >= g && s % g == 0)
                .find(|&s| bars(s) <= limit)
        };
        let bucket_secs = nice(target_buckets as i64)
            .or_else(|| nice(cap))
            .unwrap_or_else(|| {
                // g 의 배수로 올림 (올림하면 막대 수는 줄어들 뿐 늘지 않는다)
                let raw = (last - first + cap.max(1)) / cap.max(1);
                (raw + g - 1) / g * g
            });

        let start_sec = first - first.rem_euclid(bucket_secs);
        let count = ((last - start_sec) / bucket_secs + 1) as usize;
        let mut buckets = Vec::with_capacity(count);
        for i in 0..count {
            buckets.push(TimeBucket {
                start_sec: start_sec + i as i64 * bucket_secs,
                total: 0,
                error: 0,
                warn: 0,
                first_line: u64::MAX,
            });
        }
        for s in &self.seconds {
            let idx = ((s.sec - start_sec) / bucket_secs) as usize;
            let b = &mut buckets[idx];
            b.total = b.total.saturating_add(s.total);
            b.error = b.error.saturating_add(s.error);
            b.warn = b.warn.saturating_add(s.warn);
            b.first_line = b.first_line.min(s.first_line);
        }
        for b in &mut buckets {
            if b.first_line == u64::MAX {
                b.first_line = 0;
            }
        }
        Timeline {
            bucket_secs,
            start_sec,
            buckets,
        }
    }

    /// seconds 를 NICE_BUCKET_SECS 의 간격 g 로 묶어 항목 수를 max_entries 이하로
    /// 줄인 새 개요를 반환한다. g 는 묶은 항목 수가 max_entries 이하가 되는
    /// 가장 작은 후보이며, 모든 후보가 넘으면 최대 후보(1일)를 쓴다.
    /// sec 는 g 단위 내림 정렬, total/error/warn 은 합산, first_line 은 최솟값.
    /// line_count 와 pos 분포는 그대로이고 granularity_secs 에 g 를 기록한다.
    /// 이미 max_entries 이하이면 g = 1 로 사실상 복사본이다.
    pub fn coarsen(&self, max_entries: usize) -> Overview {
        // seconds 는 sec 오름차순이므로 정렬된 구간 시작이 바뀔 때만 새 그룹이다.
        // limit 를 넘는 순간 조기 종료한다 (앞부분만 보고 탈락 판정 가능).
        let groups = |g: i64| -> usize {
            let mut count = 0usize;
            let mut prev = i64::MIN;
            for b in &self.seconds {
                let a = b.sec - b.sec.rem_euclid(g);
                if a != prev {
                    prev = a;
                    count += 1;
                    if count > max_entries {
                        break;
                    }
                }
            }
            count
        };
        let g = NICE_BUCKET_SECS
            .iter()
            .copied()
            .find(|&s| groups(s) <= max_entries)
            .unwrap_or(86400);

        let mut seconds: Vec<SecBucket> = Vec::with_capacity(groups(g).min(self.seconds.len()));
        for b in &self.seconds {
            let a = b.sec - b.sec.rem_euclid(g);
            match seconds.last_mut() {
                Some(last) if last.sec == a => {
                    last.total = last.total.saturating_add(b.total);
                    last.error = last.error.saturating_add(b.error);
                    last.warn = last.warn.saturating_add(b.warn);
                    last.first_line = last.first_line.min(b.first_line);
                }
                _ => seconds.push(SecBucket { sec: a, ..*b }),
            }
        }
        Overview {
            line_count: self.line_count,
            pos_error: self.pos_error.clone(),
            pos_warn: self.pos_warn.clone(),
            seconds,
            granularity_secs: g,
        }
    }
}

/// 서브청크 평가 결과. 병합은 청크 순서대로 순차 수행한다.
struct SubResult {
    /// 이 청크의 줄 수
    line_count: u64,
    /// pos 구간별 ERROR 수 (len = bins)
    pos_error: Vec<u32>,
    /// pos 구간별 WARN 수
    pos_warn: Vec<u32>,
    /// 초 → 집계. first_line 은 전역 줄 번호.
    seconds: HashMap<i64, SecAccum>,
    /// 첫 타임스탬프 이전의 줄들 집계 (귀속 시각은 병합 단계에서 결정).
    /// first_line 은 이 청크의 첫 줄(전역 번호).
    lead: SecAccum,
    /// 이 청크의 첫 유효 타임스탬프의 초 (lead 귀속 후보)
    first_ts_sec: Option<i64>,
    /// 이 청크의 마지막 유효 타임스탬프 밀리초 (다음 청크의 상속 시각)
    last_ts_ms: Option<i64>,
}

/// 초 버킷 집계 (누적용)
#[derive(Clone, Copy, Default)]
struct SecAccum {
    total: u32,
    error: u32,
    warn: u32,
    first_line: u64,
}

impl SecAccum {
    fn add_line(&mut self, is_error: bool, is_warn: bool, line: u64) {
        if self.total == 0 {
            self.first_line = line;
        }
        self.total += 1;
        if is_error {
            self.error += 1;
        }
        if is_warn {
            self.warn += 1;
        }
    }

    fn add(&mut self, other: &SecAccum) {
        if other.total == 0 {
            return;
        }
        if self.total == 0 {
            self.first_line = other.first_line;
        } else {
            self.first_line = self.first_line.min(other.first_line);
        }
        self.total += other.total;
        self.error += other.error;
        self.warn += other.warn;
    }
}

/// 파일 전체를 한 번 스캔해 개요를 만든다.
/// 취소되면 Ok(None), 완료되면 Ok(Some(Overview)) 를 반환한다.
///
/// 전제: 인덱싱이 끝난 파일. pos 구간 수는 인덱스의 줄 수로 정하므로
/// 인덱스 완료 후 호출해야 한다 (미완료면 인덱스 기준 구간 수가 실제보다
/// 작아져 스캔에서 넘치는 줄은 마지막 구간에 몰린다).
///
/// progress 는 0.0~1.0 의 스캔 진행률 콜백이다.
pub fn compute_overview(
    file: &LogFile,
    ctx: &TsContext,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<Option<Overview>> {
    let source = file.source();
    let total_bytes = source.len();

    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    if total_bytes == 0 {
        progress(1.0);
        return Ok(Some(Overview {
            line_count: 0,
            pos_error: Vec::new(),
            pos_warn: Vec::new(),
            seconds: Vec::new(),
            granularity_secs: 1,
        }));
    }

    // syslog 기본 연도는 파일 수정 시각의 연도로 한다 (merge.rs 와 같은 규칙).
    let mut ctx = *ctx;
    if let Some(secs) = source.mtime_secs() {
        ctx.default_year = year_of_unix_secs(secs);
    }

    let index_lines = file.index().line_count();
    let bins = (index_lines as usize).clamp(1, POS_BINS);
    // bin 계산용 총 줄 수: 인덱스 기준. 스캔 중 발견한 초과분은 마지막 bin 에 몰린다.
    let bin_total = index_lines.max(1);

    let (free_tx, free_rx) = sync_channel::<Vec<u8>>(2);
    let (data_tx, data_rx) = sync_channel::<Result<(u64, Vec<u8>, usize)>>(2);
    let _ = free_tx.send(vec![0u8; SEGMENT_SIZE]);
    let _ = free_tx.send(vec![0u8; SEGMENT_SIZE]);

    let mut io_error: Option<Error> = None;
    // Some 이면 스캔 결과
    let mut overview: Option<Overview> = None;

    std::thread::scope(|s| {
        // I/O 스레드: 디스크 한계 속도로 순차 읽기 (search.rs 와 동일 구조)
        s.spawn(move || {
            let mut offset: u64 = 0;
            while offset < total_bytes {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let to_read = (SEGMENT_SIZE as u64).min(total_bytes - offset) as usize;
                let mut buf = match free_rx.recv() {
                    Ok(b) => b,
                    Err(_) => break,
                };
                if buf.len() < to_read {
                    buf.resize(to_read, 0);
                }
                match source.read_into(&mut buf[..to_read], offset) {
                    Ok(n) => {
                        if n == 0 {
                            break;
                        }
                        if data_tx.send(Ok((offset, buf, n))).is_err() {
                            break;
                        }
                        offset += n as u64;
                    }
                    Err(e) => {
                        let _ = data_tx.send(Err(e));
                        break;
                    }
                }
            }
        });

        // 메인 스레드: 줄 경계 정렬 → rayon 서브청크 평가 → 순차 병합
        let mut pos_error = vec![0u32; bins];
        let mut pos_warn = vec![0u32; bins];
        let mut seconds: BTreeMap<i64, SecAccum> = BTreeMap::new();
        // 아직 아무 타임스탬프도 보지 못한 상태의 누적 (파일 첫 타임스탬프에 귀속)
        let mut orphan = SecAccum::default();
        // 직전까지의 유효 타임스탬프 초 (타임스탬프 없는 줄의 귀속처)
        let mut cur_sec: Option<i64> = None;
        let mut global_line: u64 = 0;
        let mut cancelled = false;

        // 세그먼트 경계를 넘는 줄의 머리 (≤HEAD_CAP). 판정은 머리만으로
        // 충분하므로 줄이 아무리 길어도 이 양만 보관한다.
        let mut carry: Vec<u8> = Vec::new();
        let mut combined: Vec<u8> = Vec::new();

        loop {
            let (seg_off, buf, valid) = match data_rx.recv() {
                Ok(Ok(d)) => d,
                Ok(Err(e)) => {
                    io_error = Some(e);
                    break;
                }
                Err(_) => break,
            };
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let is_eof = seg_off + valid as u64 >= total_bytes;

            combined.clear();
            combined.extend_from_slice(&carry);
            combined.extend_from_slice(&buf[..valid]);
            // 버퍼는 즉시 I/O 스레드로 돌려보내 다음 읽기를 진행한다
            let _ = free_tx.send(buf);

            // processable: 줄 경계까지. rem: 미완성 마지막 줄의 머리.
            let processable: &[u8] = if is_eof {
                carry.clear();
                &combined
            } else {
                match memchr::memrchr(b'\n', &combined) {
                    Some(nl) => {
                        let rem = &combined[nl + 1..];
                        carry.clear();
                        carry.extend_from_slice(&rem[..rem.len().min(HEAD_CAP)]);
                        &combined[..=nl]
                    }
                    None => {
                        // 세그먼트 전체가 한 줄의 일부 → 머리만 보관하고 건너뛴다
                        carry.clear();
                        carry.extend_from_slice(&combined[..combined.len().min(HEAD_CAP)]);
                        &combined[..0]
                    }
                }
            };

            if !processable.is_empty() {
                // 서브청크 분할 (search.rs 와 같은 방식)
                let num_threads = rayon::current_num_threads();
                let chunk_count = if num_threads <= 1 || processable.len() < 32 * 1024 {
                    1
                } else {
                    num_threads.min((processable.len() / (16 * 1024)).max(1))
                };
                let target_len = processable.len().div_ceil(chunk_count);
                let mut subchunks: Vec<&[u8]> = Vec::with_capacity(chunk_count);
                let mut cur = 0;
                while cur < processable.len() {
                    let ideal_end = (cur + target_len).min(processable.len());
                    let end = if ideal_end >= processable.len() {
                        processable.len()
                    } else {
                        match memchr::memchr(b'\n', &processable[ideal_end..]) {
                            Some(p) => ideal_end + p + 1,
                            None => processable.len(),
                        }
                    };
                    subchunks.push(&processable[cur..end]);
                    cur = end;
                }

                // 서브청크별 줄 수를 먼저 세서 전역 시작 줄(base)을 정한다
                let counts: Vec<u64> = subchunks.par_iter().map(|sub| count_lines(sub)).collect();
                let results: Vec<SubResult> = subchunks
                    .par_iter()
                    .enumerate()
                    .map(|(i, sub)| {
                        let base = global_line + counts[..i].iter().sum::<u64>();
                        eval_subchunk(sub, base, bins, bin_total, &ctx, cancel)
                    })
                    .collect();

                for r in results {
                    // 위치 분포 합산
                    for (b, v) in pos_error.iter_mut().zip(r.pos_error.iter()) {
                        *b = b.saturating_add(*v);
                    }
                    for (b, v) in pos_warn.iter_mut().zip(r.pos_warn.iter()) {
                        *b = b.saturating_add(*v);
                    }
                    // lead(첫 타임스탬프 이전 줄들) 귀속:
                    // 청크에 타임스탬프가 있으면 그 첫 시각에, 없으면 직전 시각에,
                    // 둘 다 없으면 파일 첫 타임스탬프로 미룬다 (근사, orphan 누적).
                    if let Some(sec) = r.first_ts_sec {
                        seconds.entry(sec).or_default().add(&r.lead);
                        if cur_sec.is_none() && orphan.total > 0 {
                            seconds.entry(sec).or_default().add(&orphan);
                            orphan = SecAccum::default();
                        }
                    } else if let Some(sec) = cur_sec {
                        seconds.entry(sec).or_default().add(&r.lead);
                    } else {
                        orphan.add(&r.lead);
                    }
                    for (sec, acc) in &r.seconds {
                        seconds.entry(*sec).or_default().add(acc);
                    }
                    if let Some(ms) = r.last_ts_ms {
                        cur_sec = Some(ms.div_euclid(1000));
                    }
                    global_line += r.line_count;
                }
            }

            progress(((seg_off + valid as u64).min(total_bytes)) as f64 / total_bytes as f64);

            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
        }

        // 파일 첫 타임스탬프 이전의 orphan 잔여분을 그 시각에 귀속한다.
        // 파일 전체에 타임스탬프가 없으면 orphan 은 버려지고 seconds 도 비어 있다.
        if orphan.total > 0
            && !cancelled
            && io_error.is_none()
            && let Some(sec) = seconds.keys().next().copied()
        {
            seconds.entry(sec).or_default().add(&orphan);
        }

        if !cancelled && io_error.is_none() && !cancel.load(Ordering::Relaxed) {
            overview = Some(Overview {
                line_count: global_line,
                pos_error,
                pos_warn,
                granularity_secs: 1,
                seconds: seconds
                    .into_iter()
                    .map(|(sec, a)| SecBucket {
                        sec,
                        total: a.total,
                        error: a.error,
                        warn: a.warn,
                        first_line: a.first_line,
                    })
                    .collect(),
            });
            progress(1.0);
        }

        drop(data_rx);
        drop(free_tx);
    });

    if let Some(e) = io_error {
        return Err(e);
    }
    Ok(overview)
}

/// 줄 경계 정렬된 슬라이스의 줄 수를 센다
/// (개행 수 + 끝이 개행이 아니면 1).
fn count_lines(sub: &[u8]) -> u64 {
    if sub.is_empty() {
        return 0;
    }
    let nl = memchr::memchr_iter(b'\n', sub).count() as u64;
    if sub.ends_with(b"\n") { nl } else { nl + 1 }
}

/// 서브청크(줄 경계 정렬 슬라이스)를 평가한다.
/// base 는 첫 줄의 전역 줄 번호(0-based), bin_total 은 pos bin 계산용 총 줄 수.
fn eval_subchunk(
    sub: &[u8],
    base: u64,
    bins: usize,
    bin_total: u64,
    ctx: &TsContext,
    cancel: &AtomicBool,
) -> SubResult {
    let mut r = SubResult {
        line_count: 0,
        pos_error: vec![0u32; bins],
        pos_warn: vec![0u32; bins],
        seconds: HashMap::new(),
        lead: SecAccum::default(),
        first_ts_sec: None,
        last_ts_ms: None,
    };
    // 이 청크 안의 직전 유효 타임스탬프 초 (타임스탬프 없는 줄의 귀속처)
    let mut cur_sec: Option<i64> = None;

    let mut pos = 0usize;
    while pos < sub.len() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let (content_end, next) = match memchr::memchr(b'\n', &sub[pos..]) {
            Some(p) => (pos + p, pos + p + 1),
            None => (sub.len(), sub.len()),
        };
        // 개행 제외, 끝의 '\r' 하나 제거 (search.rs 와 같은 규칙)
        let mut end = content_end;
        if end > pos && sub[end - 1] == b'\r' {
            end -= 1;
        }
        let line = &sub[pos..end];
        let head = &line[..line.len().min(HEAD_CAP)];

        let global = base + r.line_count;
        let level = line_level(head);
        let is_error = level == Some(Level::Error);
        let is_warn = level == Some(Level::Warn);
        if is_error || is_warn {
            let bin = ((global * bins as u64) / bin_total).min(bins as u64 - 1) as usize;
            if is_error {
                r.pos_error[bin] += 1;
            } else {
                r.pos_warn[bin] += 1;
            }
        }

        match parse_timestamp(head, ctx) {
            Some(ms) => {
                let sec = ms.div_euclid(1000);
                if r.first_ts_sec.is_none() {
                    r.first_ts_sec = Some(sec);
                }
                r.last_ts_ms = Some(ms);
                cur_sec = Some(sec);
                r.seconds
                    .entry(sec)
                    .or_default()
                    .add_line(is_error, is_warn, global);
            }
            None => match cur_sec {
                Some(sec) => {
                    r.seconds
                        .entry(sec)
                        .or_default()
                        .add_line(is_error, is_warn, global);
                }
                None => r.lead.add_line(is_error, is_warn, global),
            },
        }

        r.line_count += 1;
        pos = next;
    }

    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    const UTC: TsContext = TsContext {
        local_offset_minutes: 0,
        default_year: 2026,
    };

    /// 테스트용 임시 파일 (drop 시 삭제)
    struct TmpFile {
        path: PathBuf,
    }

    impl TmpFile {
        fn new(name: &str, content: &[u8]) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "riffle_ov_{}_{}_{}.log",
                name,
                std::process::id(),
                id
            ));
            std::fs::write(&path, content).expect("임시 파일 쓰기 실패");
            TmpFile { path }
        }
    }

    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn open_indexed(path: &std::path::Path) -> LogFile {
        let f = LogFile::open(path).expect("열기 실패");
        f.build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("인덱싱 실패");
        f
    }

    fn compute(file: &LogFile) -> Overview {
        compute_overview(file, &UTC, &AtomicBool::new(false), &mut |_| {})
            .expect("개요 스캔 실패")
            .expect("취소되지 않아야 함")
    }

    /// 알려진 패턴의 로그 생성.
    /// 각 줄: "2026-11-14 10:MM:SS LVL msg i".
    /// with_continuation 이면 i%10==0 마다 타임스탬프 없는 연속 줄 추가.
    fn gen_lines(lines: usize, with_continuation: bool) -> Vec<u8> {
        let mut buf = Vec::new();
        for i in 0..lines {
            let lvl = match i % 20 {
                0 => "ERROR",
                5 => "FATAL",
                10 => "WARN",
                15 => "WARNING",
                _ => "INFO",
            };
            buf.extend_from_slice(
                format!(
                    "2026-11-14 10:{:02}:{:02} {} msg {}\n",
                    (i / 60) % 60,
                    i % 60,
                    lvl,
                    i
                )
                .as_bytes(),
            );
            // 타임스탬프 없는 연속 줄 (스택 트레이스 흉내)
            if with_continuation && i % 10 == 0 {
                buf.extend_from_slice(b"    at com.example.Frame.run(Frame.java:1)\n");
            }
        }
        buf
    }

    /// 순차 단순 계산으로 기대값을 만든다 (검증 기준).
    fn expected(content: &[u8], ctx: &TsContext) -> (u64, u64, u64, BTreeMap<i64, SecAccum>) {
        let mut error = 0u64;
        let mut warn = 0u64;
        let mut secs: BTreeMap<i64, SecAccum> = BTreeMap::new();
        let mut cur: Option<i64> = None;
        let mut pending: Vec<(bool, bool, u64)> = Vec::new();
        let mut lines: Vec<&[u8]> = content.split(|&b| b == b'\n').collect();
        // split은 마지막 개행 뒤에 빈 조각을 만들므로 버린다
        if lines.last() == Some(&b"".as_slice()) {
            lines.pop();
        }
        for (i, raw) in lines.iter().enumerate() {
            let line = raw.strip_suffix(b"\r").unwrap_or(raw);
            let lvl = line_level(line);
            let e = lvl == Some(Level::Error);
            let w = lvl == Some(Level::Warn);
            error += e as u64;
            warn += w as u64;
            match parse_timestamp(line, ctx) {
                Some(ms) => {
                    let sec = ms.div_euclid(1000);
                    cur = Some(sec);
                    // 앞 시각을 모르는 줄들을 첫 타임스탬프에 귀속
                    for (pe, pw, l) in pending.drain(..) {
                        secs.entry(sec).or_default().add_line(pe, pw, l);
                    }
                    secs.entry(sec).or_default().add_line(e, w, i as u64);
                }
                None => match cur {
                    Some(sec) => secs.entry(sec).or_default().add_line(e, w, i as u64),
                    None => pending.push((e, w, i as u64)),
                },
            }
        }
        (lines.len() as u64, error, warn, secs)
    }

    fn assert_secs_match(ov: &Overview, secs: &BTreeMap<i64, SecAccum>) {
        for s in &ov.seconds {
            let e = &secs[&s.sec];
            assert_eq!(s.total, e.total, "sec {}", s.sec);
            assert_eq!(s.error, e.error);
            assert_eq!(s.warn, e.warn);
            assert_eq!(s.first_line, e.first_line);
        }
        assert_eq!(ov.seconds.len(), secs.len());
    }

    #[test]
    fn counts_match_sequential() {
        // 모든 줄에 타임스탬프가 있으면 청크 분할과 무관하게 순차 결과와 정확히 같다
        let content = gen_lines(20_000, false);
        let tmp = TmpFile::new("basic", &content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);

        let (total, error, warn, secs) = expected(&content, &UTC);
        assert_eq!(ov.line_count, total);
        assert_eq!(ov.pos_error.iter().map(|&v| v as u64).sum::<u64>(), error);
        assert_eq!(ov.pos_warn.iter().map(|&v| v as u64).sum::<u64>(), warn);
        assert_eq!(
            ov.seconds.iter().map(|s| s.total as u64).sum::<u64>(),
            total
        );
        assert_secs_match(&ov, &secs);
    }

    #[test]
    fn continuation_lines_single_chunk() {
        // 단일 청크(32KB 미만)에서는 타임스탬프 없는 줄도 순차 결과와 정확히 같다
        let content = gen_lines(400, true);
        assert!(content.len() < 32 * 1024);
        let tmp = TmpFile::new("cont1", &content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);

        let (total, error, warn, secs) = expected(&content, &UTC);
        assert_eq!(ov.line_count, total);
        assert_eq!(
            ov.seconds.iter().map(|s| s.total as u64).sum::<u64>(),
            total
        );
        assert_eq!(ov.pos_error.iter().map(|&v| v as u64).sum::<u64>(), error);
        assert_eq!(ov.pos_warn.iter().map(|&v| v as u64).sum::<u64>(), warn);
        assert_secs_match(&ov, &secs);
    }

    #[test]
    fn continuation_lines_multi_chunk() {
        // 여러 청크에서는 청크 앞부분의 귀속이 근사(청크 첫 타임스탬프)라서
        // 초별 분포는 다를 수 있지만 합계는 보존되어야 한다
        let content = gen_lines(20_000, true);
        let tmp = TmpFile::new("cont2", &content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);

        let (total, error, warn, _secs) = expected(&content, &UTC);
        assert_eq!(ov.line_count, total);
        // 초 단위 total 합 == 줄 수
        assert_eq!(
            ov.seconds.iter().map(|s| s.total as u64).sum::<u64>(),
            total
        );
        assert_eq!(
            ov.seconds.iter().map(|s| s.error as u64).sum::<u64>(),
            error
        );
        assert_eq!(ov.seconds.iter().map(|s| s.warn as u64).sum::<u64>(), warn);
        // pos 분포 합계도 전체와 같다
        assert_eq!(ov.pos_error.iter().map(|&v| v as u64).sum::<u64>(), error);
        assert_eq!(ov.pos_warn.iter().map(|&v| v as u64).sum::<u64>(), warn);
    }

    #[test]
    fn pos_bins_distribution() {
        // 500줄: 줄 수 < POS_BINS 이므로 구간 수 = 줄 수 (또는 인덱스 기준 min)
        let content = gen_lines(500, true);
        let tmp = TmpFile::new("pos", &content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);
        let indexed = file.index().line_count();
        assert_eq!(ov.pos_error.len(), (indexed as usize).min(POS_BINS));
        // 각 줄이 정확히 하나의 bin 에만 들어갔는지: 합계가 전체와 같아야 한다
        let (_, error, warn, _) = expected(&content, &UTC);
        assert_eq!(ov.pos_error.iter().map(|&v| v as u64).sum::<u64>(), error);
        assert_eq!(ov.pos_warn.iter().map(|&v| v as u64).sum::<u64>(), warn);
        // 첫 줄(i=0)은 ERROR + 연속 줄. bin 0 에 에러가 있어야 한다
        assert!(ov.pos_error[0] >= 1);
    }

    #[test]
    fn first_line_of_second() {
        // 특정 초의 첫 줄 번호가 정확한지
        let content = b"2026-11-14 10:00:05 INFO a\nno-ts line\n2026-11-14 10:00:07 INFO b\n";
        let tmp = TmpFile::new("fl", content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);
        // 10:00:05 에 줄 0,1 귀속 / 10:00:07 에 줄 2
        assert_eq!(ov.seconds.len(), 2);
        assert_eq!(ov.seconds[0].total, 2);
        assert_eq!(ov.seconds[0].first_line, 0);
        assert_eq!(ov.seconds[1].total, 1);
        assert_eq!(ov.seconds[1].first_line, 2);
    }

    #[test]
    fn no_timestamps() {
        // 타임스탬프가 하나도 없으면 seconds 가 비어 있어야 한다
        let content = b"INFO a\nWARN b\nERROR c\n";
        let tmp = TmpFile::new("nots", content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);
        assert_eq!(ov.line_count, 3);
        assert!(ov.seconds.is_empty());
        assert_eq!(ov.pos_error.iter().map(|&v| v as u64).sum::<u64>(), 1);
        assert_eq!(ov.pos_warn.iter().map(|&v| v as u64).sum::<u64>(), 1);
    }

    #[test]
    fn leading_lines_attach_to_first_ts() {
        // 파일 처음의 타임스탬프 없는 줄들은 첫 타임스탬프에 귀속한다
        let content = b"continuation 1\ncontinuation 2\n2026-11-14 10:00:00 INFO x\n";
        let tmp = TmpFile::new("lead", content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);
        assert_eq!(ov.seconds.len(), 1);
        assert_eq!(ov.seconds[0].total, 3);
        assert_eq!(ov.seconds[0].first_line, 0);
    }

    #[test]
    fn long_lines_only_head() {
        // 세그먼트보다 긴 줄도 머리만으로 판정한다
        let mut content = Vec::new();
        content.extend_from_slice(b"2026-11-14 10:00:00 INFO start\n");
        // 1MB 길이의 ERROR 줄 (개행 없음)
        content.extend_from_slice(b"2026-11-14 10:00:01 ERROR ");
        content.extend_from_slice(&vec![b'x'; 1024 * 1024]);
        content.extend_from_slice(b"\n2026-11-14 10:00:02 INFO end\n");
        let tmp = TmpFile::new("long", &content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);
        assert_eq!(ov.line_count, 3);
        assert_eq!(ov.pos_error.iter().map(|&v| v as u64).sum::<u64>(), 1);
        assert_eq!(ov.seconds.len(), 3);
        assert_eq!(ov.seconds[1].error, 1);
    }

    #[test]
    fn cancel_returns_none() {
        let content = gen_lines(10_000, true);
        let tmp = TmpFile::new("cancel", &content);
        let file = open_indexed(&tmp.path);
        let cancel = AtomicBool::new(true);
        let r = compute_overview(&file, &UTC, &cancel, &mut |_| {}).expect("스캔 실패");
        assert!(r.is_none());
    }

    #[test]
    fn empty_file() {
        let tmp = TmpFile::new("empty", b"");
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);
        assert_eq!(ov.line_count, 0);
        assert!(ov.seconds.is_empty());
        assert!(ov.pos_error.is_empty());
    }

    #[test]
    fn timeline_bucket_size_and_sum() {
        // 1시간에 걸친 초 단위 데이터 3600개
        let mut ov = Overview {
            line_count: 3600,
            pos_error: vec![],
            pos_warn: vec![],
            seconds: Vec::new(),
            granularity_secs: 1,
        };
        for i in 0..3600i64 {
            ov.seconds.push(SecBucket {
                // 60초 정렬 경계에서 시작해 60s 간격이 정확히 60개 막대가 되게 한다
                sec: 1_699_999_980 + i,
                total: 2,
                error: u32::from(i % 60 == 0),
                warn: 0,
                first_line: i as u64,
            });
        }
        // target 60: 3600초 → 가장 작은 간격은 60s(1m)
        let tl = ov.timeline(60);
        assert_eq!(tl.bucket_secs, 60);
        assert!(tl.buckets.len() <= 60);
        assert_eq!(tl.buckets.iter().map(|b| b.total as u64).sum::<u64>(), 7200);
        assert_eq!(tl.buckets.iter().map(|b| b.error as u64).sum::<u64>(), 60);
        // 첫 막대 first_line 보존
        assert_eq!(tl.buckets[0].first_line, 0);
        // 빈 구간 포함 연속: start_sec 간격 확인
        assert_eq!(tl.buckets[1].start_sec - tl.buckets[0].start_sec, 60);
    }

    #[test]
    fn timeline_nice_size_selection() {
        let mk = |span_secs: i64| Overview {
            line_count: 2,
            pos_error: vec![],
            pos_warn: vec![],
            granularity_secs: 1,
            seconds: vec![
                SecBucket {
                    sec: 0,
                    total: 1,
                    error: 0,
                    warn: 0,
                    first_line: 0,
                },
                SecBucket {
                    sec: span_secs,
                    total: 1,
                    error: 0,
                    warn: 0,
                    first_line: 1,
                },
            ],
        };
        // 40초 span, target 30 → 5s 간격이면 9개 ≤ 30, 2s면 21개 ≤ 30 → 2s
        assert_eq!(mk(40).timeline(30).bucket_secs, 2);
        // 100초, target 10 → 15s 간격이면 7개 ≤ 10 → 15s
        assert_eq!(mk(100).timeline(10).bucket_secs, 15);
        // 50일 span, target 100 → 12h면 101개 > 100, 1d면 51개 ≤ 100 → 86400
        assert_eq!(mk(50 * 86400).timeline(100).bucket_secs, 86400);
        // 200일 span, target 100 → 후보 간격으로도 cap 이하가 안 되어 임의 간격
        let tl = mk(200 * 86400).timeline(100);
        assert!(tl.buckets.len() <= 200);
        assert_eq!(tl.buckets.iter().map(|b| b.total as u64).sum::<u64>(), 2);
    }

    #[test]
    fn timeline_empty() {
        let ov = Overview {
            line_count: 0,
            pos_error: vec![],
            pos_warn: vec![],
            seconds: vec![],
            granularity_secs: 1,
        };
        let tl = ov.timeline(50);
        assert!(tl.buckets.is_empty());
    }

    #[test]
    fn crlf_and_no_trailing_newline() {
        let content = b"2026-11-14 10:00:00 ERROR a\r\nno-ts\r\n2026-11-14 10:00:03 WARN c";
        let tmp = TmpFile::new("crlf", content);
        let file = open_indexed(&tmp.path);
        let ov = compute(&file);
        assert_eq!(ov.line_count, 3);
        assert_eq!(ov.seconds.len(), 2);
        assert_eq!(ov.seconds[0].total, 2);
        assert_eq!(ov.seconds[0].error, 1);
        assert_eq!(ov.seconds[1].warn, 1);
    }

    /// sec = base + i, total/error/warn 패턴이 있는 밀집 초 버킷 n개의 개요.
    fn dense_overview(base: i64, n: i64) -> Overview {
        let seconds = (0..n)
            .map(|i| SecBucket {
                sec: base + i,
                total: 2,
                error: u32::from(i % 7 == 0),
                warn: u32::from(i % 5 == 0),
                first_line: (i * 2) as u64,
            })
            .collect();
        Overview {
            line_count: (n * 2) as u64,
            pos_error: vec![1, 2],
            pos_warn: vec![3],
            seconds,
            granularity_secs: 1,
        }
    }

    #[test]
    fn coarsen_merges_to_nice_granularity() {
        // 0..99 초 (100개) 를 max 10 이하로: g=10 → 10그룹
        let ov = dense_overview(0, 100);
        let c = ov.coarsen(10);
        assert_eq!(c.granularity_secs, 10);
        assert_eq!(c.seconds.len(), 10);
        // 합계 보존
        for f in [
            |b: &SecBucket| b.total as u64,
            |b: &SecBucket| b.error as u64,
            |b: &SecBucket| b.warn as u64,
        ] {
            assert_eq!(
                ov.seconds.iter().map(&f).sum::<u64>(),
                c.seconds.iter().map(&f).sum::<u64>()
            );
        }
        // sec 는 g 단위 정렬, first_line 은 그룹 최솟값
        for (i, b) in c.seconds.iter().enumerate() {
            assert_eq!(b.sec, (i as i64) * 10);
            assert_eq!(b.first_line, (i as u64) * 20);
            assert_eq!(b.total, 20);
        }
        // pos 분포와 line_count 는 그대로
        assert_eq!(c.pos_error, ov.pos_error);
        assert_eq!(c.pos_warn, ov.pos_warn);
        assert_eq!(c.line_count, ov.line_count);
    }

    #[test]
    fn coarsen_aligns_unaligned_start() {
        // 정렬되지 않은 시작(7)도 g 단위로 내림된다
        let ov = dense_overview(7, 100);
        let c = ov.coarsen(20);
        assert_eq!(c.granularity_secs, 10);
        assert_eq!(c.seconds[0].sec, 0);
        assert_eq!(c.seconds[0].first_line, 0);
        // 마지막 그룹: 7..106 중 100..106 → sec 100
        assert_eq!(c.seconds.last().unwrap().sec, 100);
    }

    #[test]
    fn coarsen_noop_when_under_limit() {
        let ov = dense_overview(0, 5);
        let c = ov.coarsen(50_000);
        assert_eq!(c.granularity_secs, 1);
        assert_eq!(c.seconds, ov.seconds);
    }

    #[test]
    fn timeline_same_after_coarsen() {
        // 3600개 밀집 초: 원본 timeline(60) 은 60s 간격을 고른다.
        // coarsen(100) → g=60. 60 은 g 의 배수이므로 같은 간격을 골라 결과가 동일해야 한다.
        let ov = dense_overview(0, 3600);
        let want = ov.timeline(60);
        assert_eq!(want.bucket_secs, 60);
        let c = ov.coarsen(100);
        assert_eq!(c.granularity_secs, 60);
        assert_eq!(c.timeline(60), want);
    }

    #[test]
    fn timeline_respects_granularity() {
        // g=60 으로 묶인 개요는 60 미만·비배수 간격을 고르지 않는다.
        // 원본이면 5s(720개 ≤ 1000)를 골랐을 target 에서도 60s 를 골라야 한다.
        let ov = dense_overview(0, 3600);
        assert_eq!(ov.timeline(1000).bucket_secs, 5);
        let c = ov.coarsen(100);
        let tl = c.timeline(1000);
        assert_eq!(tl.bucket_secs, 60);
        assert_eq!(tl.bucket_secs % c.granularity_secs, 0);
        // 막대 경계가 버킷 경계와 일치: 합계 보존
        assert_eq!(
            tl.buckets.iter().map(|b| b.total as u64).sum::<u64>(),
            ov.seconds.iter().map(|b| b.total as u64).sum::<u64>()
        );
    }
}
