use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;

use crate::error::{Error, Result};
use crate::lines::LogFile;

/// 최대 검색 결과 건수 (1,000만 건)
pub const MAX_RESULTS: usize = 10_000_000;

/// 기본 검색 세그먼트 크기 (16MB)
pub const DEFAULT_SEARCH_SEGMENT_SIZE: usize = 16 * 1024 * 1024;

/// 첫 번째 검색 세그먼트 크기 (2MB - 첫 매치 지연 단축용)
pub(crate) const FIRST_SEARCH_SEGMENT_SIZE: usize = 2 * 1024 * 1024;

/// 배치 전송 최대 크기
const BATCH_SIZE: usize = 1000;

/// 검색 옵션
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    /// 정규식 사용 여부
    pub regex: bool,
    /// 대소문자 구분 여부
    pub case_sensitive: bool,
}

/// 검색기 내부 구현 종류
#[derive(Clone)]
enum MatcherKind {
    /// 리터럴 대소문자 구분 (memchr memmem) 및 바이트 길이
    Literal(memchr::memmem::Finder<'static>, usize),
    /// 정규식 또는 대소문자 무시 리터럴
    Regex(regex::bytes::Regex),
}

/// 컴파일된 검색기. Send + Sync + Clone을 만족한다.
#[derive(Clone)]
pub struct Matcher {
    kind: MatcherKind,
    query: String,
    options: SearchOptions,
    /// 널러블 정규식의 비어있지 않은 매치 첫 바이트 집합 (성능 필터, 없으면 미적용)
    first_bytes: Option<FirstByteSet>,
}

impl Matcher {
    /// 새로운 Matcher를 생성한다.
    ///
    /// - query가 비어 있으면 `Error::InvalidQuery` 반환.
    /// - 정규식 문법 오류 시 `Error::InvalidQuery` 반환.
    /// - 대소문자 구분 리터럴은 `memchr::memmem::Finder`를 사용하고,
    ///   대소문자 무시 리터럴은 `regex::escape` 후 `(?i)` 정규식으로 처리한다.
    pub fn new(query: &str, options: SearchOptions) -> Result<Matcher> {
        if query.is_empty() {
            return Err(Error::InvalidQuery("검색어가 비어 있습니다".to_string()));
        }

        let kind = if !options.regex {
            if options.case_sensitive {
                let finder = memchr::memmem::Finder::new(query.as_bytes()).into_owned();
                MatcherKind::Literal(finder, query.len())
            } else {
                let pattern = regex::escape(query);
                let re = regex::bytes::RegexBuilder::new(&pattern)
                    .case_insensitive(true)
                    .multi_line(true)
                    .crlf(true)
                    .build()
                    .map_err(|e| Error::InvalidQuery(e.to_string()))?;
                MatcherKind::Regex(re)
            }
        } else {
            let re = regex::bytes::RegexBuilder::new(query)
                .case_insensitive(!options.case_sensitive)
                .multi_line(true)
                .crlf(true)
                .build()
                .map_err(|e| Error::InvalidQuery(e.to_string()))?;
            MatcherKind::Regex(re)
        };

        // 널러블 정규식은 모든 줄이 빈 매치 후보가 되므로, 비어있지 않은 매치의
        // 첫 바이트 집합으로 줄 단위 재검증을 건너뛸 수 있게 한다 (A3 성능).
        let first_bytes = match &kind {
            MatcherKind::Regex(re) if re.is_match(b"") => {
                first_bytes_of(query, !options.case_sensitive)
            }
            _ => None,
        };

        Ok(Matcher {
            kind,
            query: query.to_string(),
            options,
            first_bytes,
        })
    }

    /// 원본 검색어 반환
    pub fn query(&self) -> &str {
        &self.query
    }

    /// 검색 옵션 반환
    pub fn options(&self) -> SearchOptions {
        self.options
    }

    /// 한 줄(개행 미포함) 안의 모든 매치 바이트 범위를 반환한다.
    /// 겹치지 않으며 오름차순으로 정렬되고 빈 매치(길이 0)는 제외된다.
    pub fn find_ranges(&self, line: &[u8]) -> Vec<(usize, usize)> {
        match &self.kind {
            MatcherKind::Literal(finder, needle_len) => {
                let mut ranges = Vec::new();
                for start in finder.find_iter(line) {
                    ranges.push((start, start + *needle_len));
                }
                ranges
            }
            MatcherKind::Regex(re) => {
                let mut ranges = Vec::new();
                for m in re.find_iter(line) {
                    if m.start() < m.end() {
                        ranges.push((m.start(), m.end()));
                    }
                }
                ranges
            }
        }
    }

    /// haystack 안 첫 매치의 시작 오프셋을 반환한다 (정규식은 빈 매치 포함).
    /// 검색 후보 위치 탐색용으로, 매치 여부 판정은 line_has_match로 재검증한다.
    pub(crate) fn first_match_from(&self, haystack: &[u8]) -> Option<usize> {
        match &self.kind {
            MatcherKind::Literal(finder, _) => finder.find(haystack),
            MatcherKind::Regex(re) => re.find_iter(haystack).next().map(|m| m.start()),
        }
    }

    /// 줄 안에 비어있지 않은 매치가 하나라도 있는지 반환한다.
    /// find_ranges가 비어있지 않은지와 동일한 의미이며, 할당 없이 조기 종료한다.
    pub(crate) fn line_has_match(&self, line: &[u8]) -> bool {
        match &self.kind {
            MatcherKind::Literal(finder, _) => finder.find(line).is_some(),
            MatcherKind::Regex(re) => re.find_iter(line).any(|m| m.start() < m.end()),
        }
    }
}

/// UTF-8 문자열 안의 바이트 범위를 UTF-16 코드 유닛 범위로 변환한다.
/// JavaScript의 문자열 인덱스(서로게이트 쌍 고려)와 일치하도록 변환한다.
pub fn utf16_ranges(text: &str, byte_ranges: &[(usize, usize)]) -> Vec<(u32, u32)> {
    if byte_ranges.is_empty() || text.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::with_capacity(byte_ranges.len());
    let mut range_idx = 0;
    let mut cur_u16 = 0u32;
    let mut in_match = false;
    let mut start_u16 = 0u32;

    for (b_idx, ch) in text.char_indices() {
        while range_idx < byte_ranges.len() {
            let (b_start, b_end) = byte_ranges[range_idx];
            if !in_match {
                if b_idx >= b_start {
                    start_u16 = cur_u16;
                    in_match = true;
                    if b_idx >= b_end {
                        if start_u16 < cur_u16 {
                            result.push((start_u16, cur_u16));
                        }
                        in_match = false;
                        range_idx += 1;
                        continue;
                    }
                }
                break;
            } else {
                if b_idx >= b_end {
                    if start_u16 < cur_u16 {
                        result.push((start_u16, cur_u16));
                    }
                    in_match = false;
                    range_idx += 1;
                    continue;
                }
                break;
            }
        }
        cur_u16 += ch.len_utf16() as u32;
    }

    while range_idx < byte_ranges.len() {
        let (b_start, _b_end) = byte_ranges[range_idx];
        if !in_match {
            start_u16 = cur_u16;
        }
        if start_u16 < cur_u16 || (!in_match && b_start <= text.len()) {
            result.push((start_u16, cur_u16));
        }
        in_match = false;
        range_idx += 1;
    }

    result
}

/// 검색 진행 상태
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchProgress {
    /// 현재까지 스캔 완료된 바이트 수
    pub scanned_bytes: u64,
    /// 파일 전체 바이트 크기
    pub total_bytes: u64,
    /// 현재까지 발견된 매치된 줄 수
    pub match_count: u64,
}

/// 검색 실행 결과
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchOutcome {
    /// 검색 완료 (전체 매치 줄 수)
    Completed { total: u64 },
    /// 최대 결과 상한에 도달하여 조기 중단됨
    Capped { total: u64 },
    /// 취소 플래그에 의해 중단됨
    Cancelled,
}

/// 검색 파라미터 구성 (테스트용 설정 주입 지원)
#[derive(Debug, Clone, Copy)]
pub(crate) struct SearchConfig {
    pub segment_size: usize,
    pub max_results: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig {
            segment_size: DEFAULT_SEARCH_SEGMENT_SIZE,
            max_results: MAX_RESULTS,
        }
    }
}

/// 블로킹 방식으로 파일 전체를 검색한다.
/// 매치된 줄 번호(0-based, 한 줄에 여러 매치여도 1회)를 오름차순으로
/// 배치(최대 1,000개)씩 `on_batch`로 전달한다.
/// 파일의 인덱싱 완료 여부와 무관하게 동작하며, 자체적으로 개행을 세어 줄 번호를 계산한다.
pub fn search(
    file: &LogFile,
    matcher: &Matcher,
    cancel: &AtomicBool,
    on_batch: &mut dyn FnMut(&[u64]),
    on_progress: &mut dyn FnMut(SearchProgress),
) -> Result<SearchOutcome> {
    search_with_config(
        file,
        matcher,
        cancel,
        on_batch,
        on_progress,
        SearchConfig::default(),
    )
}

/// 테스트용 설정을 주입받아 검색을 실행하는 내부 함수
pub(crate) fn search_with_config(
    file: &LogFile,
    matcher: &Matcher,
    cancel: &AtomicBool,
    on_batch: &mut dyn FnMut(&[u64]),
    on_progress: &mut dyn FnMut(SearchProgress),
    config: SearchConfig,
) -> Result<SearchOutcome> {
    let source = file.source();
    let total_bytes = source.len();

    if cancel.load(Ordering::Relaxed) {
        return Ok(SearchOutcome::Cancelled);
    }

    if total_bytes == 0 {
        on_progress(SearchProgress {
            scanned_bytes: 0,
            total_bytes: 0,
            match_count: 0,
        });
        return Ok(SearchOutcome::Completed { total: 0 });
    }

    let normal_segment_size = config.segment_size;
    let first_segment_size = FIRST_SEARCH_SEGMENT_SIZE.min(normal_segment_size);

    // 더블 버퍼링 채널 (I/O 스레드와 검색 파이프라이닝)
    let (free_tx, free_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);
    let (data_tx, data_rx) = std::sync::mpsc::sync_channel::<Result<(u64, Vec<u8>, usize)>>(2);

    let _ = free_tx.send(vec![0u8; normal_segment_size]);
    let _ = free_tx.send(vec![0u8; normal_segment_size]);

    let mut io_error: Option<Error> = None;
    let mut outcome = SearchOutcome::Completed { total: 0 };

    std::thread::scope(|s| {
        // 백그라운드 I/O 스레드: read_at을 사용하여 디스크 한계 속도로 버퍼 읽기
        s.spawn(move || {
            let mut offset: u64 = 0;
            let mut is_first = true;

            while offset < total_bytes {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }

                let chunk_size = if is_first {
                    first_segment_size
                } else {
                    normal_segment_size
                };
                let to_read = (chunk_size as u64).min(total_bytes - offset) as usize;

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
                        is_first = false;
                    }
                    Err(e) => {
                        let _ = data_tx.send(Err(e));
                        break;
                    }
                }
            }
        });

        // 메인 스레드: 수신한 버퍼의 줄 경계를 맞추고 Rayon으로 병렬 검색 수행
        let mut carry_over: Vec<u8> = Vec::new();
        let mut combined_buf: Vec<u8> = Vec::new();
        let mut global_line_offset: u64 = 0;
        let mut total_matches: u64 = 0;
        let mut batch: Vec<u64> = Vec::with_capacity(BATCH_SIZE);
        let mut is_capped = false;
        // 버퍼보다 긴 단일 줄 스트리밍 상태와, 긴 줄 종료 후 남은 꼬리 청크
        let mut long_line: Option<LongLineScan> = None;
        let mut pending_item: Option<(u64, Vec<u8>, usize)> = None;

        loop {
            // 긴 줄이 끝난 자리의 나머지 바이트를 먼저 처리하고, 없으면 IO 스레드에서 수신
            let (segment_offset, buf, valid_len) = match pending_item.take() {
                Some(item) => item,
                None => match data_rx.recv() {
                    Ok(Ok(data)) => data,
                    Ok(Err(e)) => {
                        io_error = Some(e);
                        break;
                    }
                    Err(_) => break,
                },
            };

            if cancel.load(Ordering::Relaxed) {
                outcome = SearchOutcome::Cancelled;
                break;
            }

            let is_eof = (segment_offset + valid_len as u64) >= total_bytes;

            // 긴 단일 줄 스트리밍 중: 이 청크를 줄의 연속으로 검사
            if let Some(scan) = long_line.as_mut() {
                let line_finished = match memchr::memchr(b'\n', &buf[..valid_len]) {
                    Some(nl) => {
                        feed_long_line(scan, matcher, &buf[..nl], true);
                        true
                    }
                    None => {
                        // 개행 없음: 파일 끝이면 이 청크가 줄의 끝
                        feed_long_line(scan, matcher, &buf[..valid_len], is_eof);
                        is_eof
                    }
                };

                if line_finished {
                    if scan.matched {
                        batch.push(scan.line_no);
                        total_matches += 1;
                        if batch.len() >= BATCH_SIZE {
                            on_batch(&batch);
                            batch.clear();
                        }
                        if total_matches >= config.max_results as u64 {
                            is_capped = true;
                        }
                    }
                    global_line_offset += 1;
                    long_line = None;

                    // 개행 이후 남은 바이트는 다음 아이템으로 넘겨 일반 경로에서 처리
                    if let Some(nl) = memchr::memchr(b'\n', &buf[..valid_len]) {
                        let rest = &buf[nl + 1..valid_len];
                        if !rest.is_empty() {
                            pending_item =
                                Some((segment_offset + nl as u64 + 1, rest.to_vec(), rest.len()));
                        }
                    }
                }

                let _ = free_tx.send(buf);
                on_progress(SearchProgress {
                    scanned_bytes: (segment_offset + valid_len as u64).min(total_bytes),
                    total_bytes,
                    match_count: total_matches,
                });

                if is_capped {
                    outcome = SearchOutcome::Capped {
                        total: total_matches,
                    };
                    break;
                }
                continue;
            }

            // 이전 버퍼에서 넘어온 미완성 줄(carry_over)과 현재 버퍼를 연결
            combined_buf.clear();
            combined_buf.extend_from_slice(&carry_over);
            combined_buf.extend_from_slice(&buf[..valid_len]);

            // 버퍼를 즉시 I/O 스레드로 반환하여 다음 읽기 진행
            let _ = free_tx.send(buf);

            let search_slice: &[u8] = if is_eof {
                // 파일 끝인 경우 남아있는 모든 바이트를 마지막 줄로 처리
                carry_over.clear();
                &combined_buf[..]
            } else {
                match memchr::memrchr(b'\n', &combined_buf) {
                    Some(last_nl_pos) => {
                        let slice = &combined_buf[..=last_nl_pos];
                        let remainder = &combined_buf[last_nl_pos + 1..];
                        carry_over.clear();
                        carry_over.extend_from_slice(remainder);
                        slice
                    }
                    None => {
                        // 버퍼 안에 개행이 전혀 없음: 단일 줄이 버퍼보다 길다.
                        // carry_over에 누적하지 않고 스트리밍 스캔으로 전환한다.
                        // combined_buf 전체가 아직 검사되지 않은 줄의 앞부분이다.
                        let mut scan = LongLineScan::new(global_line_offset);
                        feed_long_line(&mut scan, matcher, &combined_buf, false);
                        long_line = Some(scan);
                        carry_over.clear();
                        on_progress(SearchProgress {
                            scanned_bytes: segment_offset + valid_len as u64,
                            total_bytes,
                            match_count: total_matches,
                        });
                        continue;
                    }
                }
            };

            if !search_slice.is_empty() {
                // 구간(segment)을 줄 경계에 맞춰 rayon 서브청크로 분할
                let num_threads = rayon::current_num_threads();
                let chunk_count = if num_threads <= 1 || search_slice.len() < 32 * 1024 {
                    1
                } else {
                    num_threads.min((search_slice.len() / (16 * 1024)).max(1))
                };
                let target_chunk_len = search_slice.len().div_ceil(chunk_count);

                let mut subchunks: Vec<&[u8]> = Vec::with_capacity(chunk_count);
                let mut cur = 0;
                while cur < search_slice.len() {
                    let ideal_end = (cur + target_chunk_len).min(search_slice.len());
                    let end = if ideal_end >= search_slice.len() {
                        search_slice.len()
                    } else {
                        // ideal_end 이후의 첫 번째 '\n' 다음 위치로 맞춰 줄 경계 정렬
                        match memchr::memchr(b'\n', &search_slice[ideal_end..]) {
                            Some(p) => ideal_end + p + 1,
                            None => search_slice.len(),
                        }
                    };
                    subchunks.push(&search_slice[cur..end]);
                    cur = end;
                }

                // Rayon 병렬 검색 수행
                let results: Vec<(u64, Vec<u64>)> = subchunks
                    .par_iter()
                    .map(|sub| search_subchunk(sub, matcher, cancel))
                    .collect();

                // 순차 prefix sum 으로 전역 줄 번호 계산 및 결과 배치 전송
                for (sub_lines, local_matches) in results {
                    for local_line in local_matches {
                        let global_line = global_line_offset + local_line;
                        batch.push(global_line);
                        total_matches += 1;

                        if batch.len() >= BATCH_SIZE {
                            on_batch(&batch);
                            batch.clear();
                        }

                        if total_matches >= config.max_results as u64 {
                            is_capped = true;
                            break;
                        }
                    }

                    global_line_offset += sub_lines;

                    if is_capped || cancel.load(Ordering::Relaxed) {
                        break;
                    }
                }

                // 세그먼트 끝에서 배치에 남은 항목 즉시 플러시
                if !batch.is_empty() {
                    on_batch(&batch);
                    batch.clear();
                }
            }

            let scanned = (segment_offset + valid_len as u64).min(total_bytes);
            on_progress(SearchProgress {
                scanned_bytes: scanned,
                total_bytes,
                match_count: total_matches,
            });

            if is_capped {
                outcome = SearchOutcome::Capped {
                    total: total_matches,
                };
                break;
            }

            if cancel.load(Ordering::Relaxed) {
                outcome = SearchOutcome::Cancelled;
                break;
            }
        }

        // 스트리밍 등 세그먼트 단위 플러시를 거치지 않은 잔여 배치 전송
        if !batch.is_empty() {
            on_batch(&batch);
            batch.clear();
        }

        if !is_capped && !cancel.load(Ordering::Relaxed) && io_error.is_none() {
            outcome = SearchOutcome::Completed {
                total: total_matches,
            };
        }

        drop(data_rx);
        drop(free_tx);
    });

    if let Some(err) = io_error {
        return Err(err);
    }

    Ok(outcome)
}

/// abs 위치의 매치 후보가 속한 줄의 (줄 시작, 내용 끝(개행·\r 제외), 다음 줄 시작)을 구한다.
/// sub[pos..]는 pos가 항상 줄 시작이라는 전제에서 호출한다.
/// 개행 문자 자체가 매치 시작이면 그 개행으로 끝나는 이전 줄을 대상으로 본다.
fn line_bounds_around(sub: &[u8], pos: usize, abs: usize) -> (usize, usize, usize) {
    let (content_end, next_pos) = if sub[abs] == b'\n' {
        // 매치가 개행에서 시작: 그 개행으로 끝나는 이전 줄
        (abs, abs + 1)
    } else {
        match memchr::memchr(b'\n', &sub[abs..]) {
            Some(p) => (abs + p, abs + p + 1),
            None => (sub.len(), sub.len()),
        }
    };
    // abs 이전 마지막 개행 다음이 줄 시작 (pos는 이미 줄 시작이므로 그 이후만 탐색)
    let line_start = match memchr::memrchr(b'\n', &sub[pos..abs]) {
        Some(r) => pos + r + 1,
        None => pos,
    };
    (line_start, content_end, next_pos)
}

/// 단일 서브청크(줄 단위로 정렬된 슬라이스) 내에서 매치 검색 수행.
/// 반환값: (서브청크 내 전체 줄 수, 매치된 로컬 줄 번호 목록)
///
/// 줄 단위 의미로 통일한다: 버퍼에서 정규식/리터럴 후보 위치를 찾은 뒤,
/// 후보 시작 위치가 속한 줄만 잘라(개행 제외, 끝 \r 제거 — 하이라이트와 동일 규칙)
/// line_has_match로 재검증한다. 정규식이 개행을 넘나들며 매치해도
/// (예: `foo\sbar`) 어느 한 줄의 텍스트 안에 매치가 없으면 기록하지 않는다.
/// 결과와 관계없이 다음 줄 시작부터 재탐색하므로 교차 매치가 앞 줄에서 시작해도
/// 뒤 줄의 매치를 놓치지 않는다. 한 줄에 여러 매치가 있어도 1회만 기록한다.
fn search_subchunk(sub: &[u8], matcher: &Matcher, cancel: &AtomicBool) -> (u64, Vec<u64>) {
    if sub.is_empty() {
        return (0, Vec::new());
    }

    let newline_count = memchr::memchr_iter(b'\n', sub).count() as u64;
    let total_lines = if !sub.ends_with(b"\n") {
        newline_count + 1
    } else {
        newline_count
    };

    let mut matched_lines = Vec::new();
    let mut pos = 0usize; // 항상 줄 시작 위치
    let mut line_no = 0u64; // pos에 해당하는 줄 번호

    while pos < sub.len() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let Some(rel) = matcher.first_match_from(&sub[pos..]) else {
            break;
        };
        let abs = pos + rel;

        let (line_start, content_end, next_pos) = line_bounds_around(sub, pos, abs);
        // pos~line_start 사이 개행 수만큼 줄 번호 전진
        let line_idx = line_no + memchr::memchr_iter(b'\n', &sub[pos..line_start]).count() as u64;

        // 줄 텍스트: 개행 제외, 끝의 '\r' 하나 제거 (find_ranges와 동일 규칙)
        let mut line_end = content_end;
        if line_end > line_start && sub[line_end - 1] == b'\r' {
            line_end -= 1;
        }
        let line = &sub[line_start..line_end];

        // 널러블 정규식: 첫 바이트 집합이 없는 줄은 재검증 없이 건너뛴다.
        // 집합은 비어있지 않은 매치 첫 바이트의 과대근사이므로 안전하다.
        let has_match = match &matcher.first_bytes {
            Some(fb) => fb.may_contain(line) && matcher.line_has_match(line),
            None => matcher.line_has_match(line),
        };
        if has_match {
            matched_lines.push(line_idx);
        }

        pos = next_pos;
        line_no = line_idx + 1;
    }

    (total_lines, matched_lines)
}

/// 정규식 패턴에서 "비어있지 않은 매치가 시작할 수 있는 첫 바이트" 집합.
/// 계산이 불확실한 구문(클래스, 와일드카드, 복잡한 이스케이프 등)이 있으면 None이며
/// 이 경우 필터를 적용하지 않는다. 집합은 항상 실제 첫 바이트 집합의 과대근사여야 한다.
#[derive(Clone)]
pub(crate) struct FirstByteSet {
    /// 바이트 값 -> 집합 포함 여부
    set: [bool; 256],
    /// 집합에 속한 바이트 목록 (memchr 최적화용, 오름차순)
    bytes: Vec<u8>,
}

impl FirstByteSet {
    /// haystack에 집합의 바이트가 하나라도 있으면 true.
    /// 집합이 작으면 memchr 계열로, 크면 단순 스캔으로 처리한다.
    fn may_contain(&self, haystack: &[u8]) -> bool {
        match self.bytes.len() {
            0 => false,
            1 => memchr::memchr(self.bytes[0], haystack).is_some(),
            2 => memchr::memchr2(self.bytes[0], self.bytes[1], haystack).is_some(),
            3 => memchr::memchr3(self.bytes[0], self.bytes[1], self.bytes[2], haystack).is_some(),
            _ => haystack.iter().any(|b| self.set[*b as usize]),
        }
    }
}

/// 패턴 바이트를 순서대로 해석하는 최소 파서 상태.
struct FirstByteParser<'a> {
    b: &'a [u8],
    pos: usize,
    case_insensitive: bool,
    /// 보수적 포기: true가 되면 전체 계산을 None으로 버린다
    bail: bool,
}

/// firsts: 이 요소가 매치의 맨 앞에 올 수 있는 바이트 집합
/// nullable: 이 요소가 빈 문자열과 매치될 수 있는지 (오직 참일 때만 다음 요소의 firsts를 합산)
struct FirstSet {
    bytes: [bool; 256],
    nullable: bool,
}

impl FirstSet {
    fn empty(nullable: bool) -> Self {
        FirstSet {
            bytes: [false; 256],
            nullable,
        }
    }
}

impl<'a> FirstByteParser<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn bail_out(&mut self) -> FirstSet {
        self.bail = true;
        FirstSet::empty(false)
    }

    /// 리터럴 바이트 하나를 firsts로 갖는 비널러블 요소
    fn literal(&mut self, byte: u8) -> FirstSet {
        let mut fs = FirstSet::empty(false);
        fs.bytes[byte as usize] = true;
        if self.case_insensitive && byte.is_ascii_alphabetic() {
            fs.bytes[byte.to_ascii_lowercase() as usize] = true;
            fs.bytes[byte.to_ascii_uppercase() as usize] = true;
        }
        fs
    }

    /// 최상위: alternation ('|' 로 구분)
    fn parse_alt(&mut self) -> FirstSet {
        let mut acc = self.parse_concat();
        while self.peek() == Some(b'|') && !self.bail {
            self.bump();
            let rhs = self.parse_concat();
            for i in 0..256 {
                acc.bytes[i] |= rhs.bytes[i];
            }
            acc.nullable |= rhs.nullable;
        }
        acc
    }

    /// 연결: ')' '|' 또는 끝까지의 요소 나열
    fn parse_concat(&mut self) -> FirstSet {
        let mut bytes = [false; 256];
        let mut nullable = true;
        loop {
            match self.peek() {
                None | Some(b')') | Some(b'|') => break,
                _ => {}
            }
            let el = self.parse_element();
            if self.bail {
                break;
            }
            // 지금까지의 모든 요소가 nullable일 때만 이 요소의 firsts를 합산한다
            if nullable {
                for (b, e) in bytes.iter_mut().zip(el.bytes.iter()) {
                    *b |= *e;
                }
            }
            nullable &= el.nullable;
        }
        FirstSet { bytes, nullable }
    }

    /// atom + 선택적 수량자 (* + ? {m,n} 및 lazy/possessive 꼬리)
    fn parse_element(&mut self) -> FirstSet {
        let mut fs = self.parse_atom();
        if self.bail {
            return fs;
        }
        match self.peek() {
            Some(b'*') | Some(b'?') => {
                self.bump();
                fs.nullable = true;
                self.eat_quantifier_tail();
            }
            Some(b'+') => {
                self.bump();
                // A+ = A A* : 첫 바이트는 A의 firsts, nullable은 A를 따름
                self.eat_quantifier_tail();
            }
            Some(b'{') => {
                if let Some(min) = self.try_parse_repetition() {
                    if min == 0 {
                        fs.nullable = true;
                    }
                    self.eat_quantifier_tail();
                }
                // 유효한 반복이 아니면 '{'는 다음 요소에서 리터럴로 처리된다
            }
            _ => {}
        }
        fs
    }

    /// 수량자 뒤의 lazy(?) 또는 possessive(+) 꼬리 하나를 소비한다
    fn eat_quantifier_tail(&mut self) {
        if matches!(self.peek(), Some(b'?') | Some(b'+')) {
            self.bump();
        }
    }

    /// '{m}', '{m,}', '{m,n}' 형태를 파싱해 min을 반환한다. 실패 시 위치를 되돌리고 None.
    fn try_parse_repetition(&mut self) -> Option<u64> {
        let save = self.pos;
        self.bump(); // '{'
        let min = self.parse_digits()?;
        if self.peek() == Some(b',') {
            self.bump();
            if self.peek() != Some(b'}') {
                self.parse_digits()?;
            }
        }
        if self.peek() != Some(b'}') {
            self.pos = save;
            return None;
        }
        self.bump(); // '}'
        Some(min)
    }

    fn parse_digits(&mut self) -> Option<u64> {
        let start = self.pos;
        let mut val: u64 = 0;
        while let Some(c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            self.bump();
            val = val.saturating_mul(10).saturating_add((c - b'0') as u64);
        }
        if self.pos == start {
            return None;
        }
        Some(val)
    }

    /// atom: 그룹, 앵커, 이스케이프, 리터럴 문자
    fn parse_atom(&mut self) -> FirstSet {
        match self.bump() {
            None => self.bail_out(),
            Some(b'(') => {
                // (?:...) 비캡처 그룹만 지원. (?P<, (?<, (?flags 등은 보수적으로 포기
                if self.peek() == Some(b'?') {
                    self.bump();
                    if self.peek() == Some(b':') {
                        self.bump();
                    } else {
                        return self.bail_out();
                    }
                }
                let inner = self.parse_alt();
                if self.peek() == Some(b')') {
                    self.bump();
                } else {
                    return self.bail_out();
                }
                inner
            }
            // '.' 와 '[...]' 클래스는 임의 바이트를 매치할 수 있어 보수적으로 포기
            Some(b'[') | Some(b'.') => self.bail_out(),
            // ^, $ 는 너비 0 앵커: firsts 없음, nullable
            Some(b'^') | Some(b'$') => FirstSet::empty(true),
            Some(b'\\') => self.parse_escape(),
            Some(c) => {
                // 메타문자가 아닌 모든 문자는 리터럴.
                // 멀티바이트 문자는 UTF-8 리드 바이트만 firsts에 넣는다
                // (리드 바이트는 해당 문자가 있는 줄에 반드시 존재하므로 안전).
                if c >= 0x80 {
                    if self.case_insensitive {
                        return self.bail_out();
                    }
                    // self.pos는 이미 다음 바이트를 가리키므로 리드 바이트만 취한다
                    let mut fs = FirstSet::empty(false);
                    fs.bytes[c as usize] = true;
                    fs
                } else {
                    self.literal(c)
                }
            }
        }
    }

    /// 이스케이프: 확실한 리터럴(\n \r \t, 구두점)만 허용, 나머지는 보수적으로 포기
    fn parse_escape(&mut self) -> FirstSet {
        match self.bump() {
            Some(b'n') => self.literal(0x0A),
            Some(b'r') => self.literal(0x0D),
            Some(b't') => self.literal(0x09),
            Some(c) if c.is_ascii_punctuation() => {
                let mut fs = FirstSet::empty(false);
                fs.bytes[c as usize] = true;
                fs
            }
            _ => self.bail_out(),
        }
    }
}

/// 패턴에서 비어있지 않은 매치의 첫 바이트 집합을 계산한다.
/// 계산 불가 시 None (필터 미적용). 결과는 항상 과대근사다.
fn first_bytes_of(pattern: &str, case_insensitive: bool) -> Option<FirstByteSet> {
    let mut p = FirstByteParser {
        b: pattern.as_bytes(),
        pos: 0,
        case_insensitive,
        bail: false,
    };
    let fs = p.parse_alt();
    if p.bail || p.pos != p.b.len() {
        return None;
    }
    // 비어있지 않은 매치가 시작할 수 없는 첫 바이트만 모은다.
    // fs.bytes는 "매치가 시작되거나(널러블 접두 이후) 이어질 수 있는" 위치의 바이트다.
    let mut set = [false; 256];
    let mut bytes = Vec::new();
    for (i, &v) in fs.bytes.iter().enumerate() {
        if v {
            set[i] = true;
            bytes.push(i as u8);
        }
    }
    Some(FirstByteSet { set, bytes })
}

/// 정규식 긴 줄 스트리밍 시 청크 경계에 겹쳐 두는 바이트 수.
/// 이 크기보다 긴 매치가 청크 경계에 걸치면 놓칠 수 있다 (SPEC 3.4 참조).
const REGEX_STREAM_OVERLAP: usize = 64 * 1024;

/// 버퍼보다 긴 단일 줄을 메모리 버퍼 크기 수준으로 스트리밍 검사하는 상태.
/// 개행이 나올 때까지 청크를 이어서 검사하며, 매치되면 한 번만 기록하고
/// 줄 끝까지는 개행 탐색만 한다.
struct LongLineScan {
    /// 이 긴 줄의 전역 줄 번호 (0-based)
    line_no: u64,
    /// 이미 매치를 찾았는지 (true면 이후 청크는 개행 탐색만 하면 됨)
    matched: bool,
    /// 지금까지 스캔한 줄 바이트 수 (개행 제외 원본 기준)
    scanned: u64,
    /// 리터럴: 이전 데이터 끝 needle_len-1 바이트 / 정규식: 이전 데이터 끝 64KB
    overlap: Vec<u8>,
    /// 정규식 haystack(overlap ++ portion) 조립용 재사용 버퍼
    scratch: Vec<u8>,
}

impl LongLineScan {
    fn new(line_no: u64) -> Self {
        LongLineScan {
            line_no,
            matched: false,
            scanned: 0,
            overlap: Vec::new(),
            scratch: Vec::new(),
        }
    }
}

/// overlap ++ portion 의 끝 keep 바이트만 남기도록 overlap을 갱신한다.
fn update_overlap(overlap: &mut Vec<u8>, portion: &[u8], keep: usize) {
    if keep == 0 {
        overlap.clear();
        return;
    }
    if portion.len() >= keep {
        overlap.clear();
        overlap.extend_from_slice(&portion[portion.len() - keep..]);
    } else {
        overlap.extend_from_slice(portion);
        if overlap.len() > keep {
            let excess = overlap.len() - keep;
            overlap.drain(..excess);
        }
    }
}

/// 긴 줄의 한 청크(portion: 개행 미포함 줄 조각)를 스트리밍 검사한다.
/// line_ended는 이 portion이 줄의 끝(개행 발견 또는 파일 끝)에 도달했는지 여부.
/// 호출 전에 portion 끝의 '\r' 제거는 이 함수가 line_ended일 때 처리한다.
fn feed_long_line(scan: &mut LongLineScan, matcher: &Matcher, portion: &[u8], line_ended: bool) {
    if scan.matched {
        scan.scanned += portion.len() as u64;
        return;
    }

    // 줄 끝에 도달한 경우 끝의 '\r' 하나는 매치 대상에서 제외 (find_ranges와 동일 규칙)
    let mut portion_eff = portion;
    let mut overlap_eff: &[u8] = &scan.overlap;
    if line_ended {
        if portion_eff.last() == Some(&b'\r') {
            portion_eff = &portion_eff[..portion_eff.len() - 1];
        } else if portion_eff.is_empty() && overlap_eff.last() == Some(&b'\r') {
            overlap_eff = &overlap_eff[..overlap_eff.len() - 1];
        }
    }

    match &matcher.kind {
        MatcherKind::Literal(finder, needle_len) => {
            let keep = needle_len.saturating_sub(1);
            // 1) portion 내부의 매치
            if finder.find_iter(portion_eff).next().is_some() {
                scan.matched = true;
            } else if keep > 0 && !overlap_eff.is_empty() {
                // 2) 경계 매치: overlap 끝과 portion 앞이 걸치는 경우
                //    (overlap 안에서 시작해 portion 안으로 끝나는 매치만 신규)
                let k = keep.min(portion_eff.len());
                scan.scratch.clear();
                scan.scratch.extend_from_slice(overlap_eff);
                scan.scratch.extend_from_slice(&portion_eff[..k]);
                let boundary = overlap_eff.len();
                for start in finder.find_iter(&scan.scratch) {
                    let end = start + *needle_len;
                    if start < boundary && end > boundary {
                        scan.matched = true;
                        break;
                    }
                }
            }
            update_overlap(&mut scan.overlap, portion, keep);
        }
        MatcherKind::Regex(re) => {
            // haystack = overlap ++ portion_eff (연속 버퍼 필요)
            scan.scratch.clear();
            scan.scratch.extend_from_slice(overlap_eff);
            scan.scratch.extend_from_slice(portion_eff);
            let haystack_len = scan.scratch.len();
            let mid_line = scan.scanned > scan.overlap.len() as u64;
            let continues = !line_ended;

            // 널러블 정규식: 첫 바이트 집합이 haystack에 없으면 매치 시작이
            // 불가능하므로 정규식 스캔을 건너뛴다 (64KB보다 긴 매치는 원래 놓칠 수 있음)
            let may_match = matcher
                .first_bytes
                .as_ref()
                .is_none_or(|fb| fb.may_contain(&scan.scratch));

            if may_match {
                for m in re.find_iter(&scan.scratch) {
                    if m.start() == m.end() {
                        continue; // 빈 매치 무시
                    }
                    // 왼쪽 경계: haystack 시작이 줄 중간이면 offset 0 매치는
                    // 이전 창에서 이미 평가됐으므로(^ 등의 경계 아티팩트) 건너뛴다
                    if m.start() == 0 && mid_line {
                        continue;
                    }
                    // 오른쪽 경계: haystack 끝이 줄 중간이면 끝에 닿는 매치는
                    // 다음 창의 overlap 안에서 더 많은 문맥과 함께 재평가한다
                    if m.end() == haystack_len && continues {
                        continue;
                    }
                    scan.matched = true;
                    break;
                }
            }
            update_overlap(&mut scan.overlap, portion, REGEX_STREAM_OVERLAP);
        }
    }

    scan.scanned += portion.len() as u64;
}
