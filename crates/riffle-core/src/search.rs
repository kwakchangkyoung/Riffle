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

        Ok(Matcher {
            kind,
            query: query.to_string(),
            options,
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

        while let Ok(msg) = data_rx.recv() {
            if cancel.load(Ordering::Relaxed) {
                outcome = SearchOutcome::Cancelled;
                break;
            }

            let (segment_offset, buf, valid_len) = match msg {
                Ok(data) => data,
                Err(e) => {
                    io_error = Some(e);
                    break;
                }
            };

            let is_eof = (segment_offset + valid_len as u64) >= total_bytes;

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
                        // 버퍼 안에 개행이 전혀 없는 경우: 단일 줄이 버퍼보다 길거나 잘린 상태
                        carry_over.clear();
                        carry_over.extend_from_slice(&combined_buf);
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

/// 단일 서브청크(줄 단위로 정렬된 슬라이스) 내에서 매치 검색 수행.
/// 반환값: (서브청크 내 전체 줄 수, 매치된 로컬 줄 번호 목록)
/// 한 줄에 여러 번 매치되더라도 줄 번호는 1회만 기록하며, 다음 줄 시작부터 검색을 이어간다.
/// 매치 범위가 개행을 넘는 정규식(예: `a\nb`)은 매치가 시작된 줄을 기준으로 기록한다.
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
    let mut cur_search_offset = 0;
    let mut cur_local_line = 0u64;
    let mut last_nl_offset = 0;

    while cur_search_offset < sub.len() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }

        let slice = &sub[cur_search_offset..];
        let found_rel = match &matcher.kind {
            MatcherKind::Literal(finder, _len) => finder.find(slice),
            MatcherKind::Regex(re) => {
                let it = re.find_iter(slice);
                let mut res = None;
                for m in it {
                    if m.start() < m.end() {
                        res = Some(m.start());
                        break;
                    }
                }
                res
            }
        };

        match found_rel {
            Some(rel_pos) => {
                let match_start = cur_search_offset + rel_pos;
                // last_nl_offset부터 match_start 사이의 개행 수를 더해 현재 줄 번호 산출
                let nls =
                    memchr::memchr_iter(b'\n', &sub[last_nl_offset..match_start]).count() as u64;
                cur_local_line += nls;
                matched_lines.push(cur_local_line);

                // 다음 줄 시작 위치로 이동 (한 줄 1회 매치 보장)
                match memchr::memchr(b'\n', &sub[match_start..]) {
                    Some(nl_pos) => {
                        let next_line_start = match_start + nl_pos + 1;
                        cur_search_offset = next_line_start;
                        last_nl_offset = next_line_start;
                        cur_local_line += 1;
                    }
                    None => {
                        // 남은 영역에 개행이 없으므로 이 매치가 마지막 줄임
                        break;
                    }
                }
            }
            None => {
                break;
            }
        }
    }

    (total_lines, matched_lines)
}
