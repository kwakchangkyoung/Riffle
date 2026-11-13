use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::filter::FilterSet;
use crate::index::{IndexProgress, LineIndex};
use crate::remote::RandomRead;
use crate::search::{Matcher, utf16_ranges};
use crate::source::{Source, SourceKind};
use crate::{Error, Result};

/// 표시용 한 줄 최대 바이트 크기 (16KB)
pub const MAX_DISPLAY_BYTES: usize = 16 * 1024;

/// 읽어온 한 줄의 정보
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// 0부터 시작하는 줄 번호
    pub no: u64,
    /// 표시용 텍스트 (\n 미포함, 끝의 \r 제거, 16KB 초과 시 잘림)
    pub text: String,
    /// 원본 줄의 바이트 길이 (\n 제외, \r 포함)
    pub byte_len: u64,
    /// 16KB 초과로 인해 텍스트가 잘렸는지 여부
    pub truncated: bool,
    /// UTF-16 [start, end) 하이라이트 범위 목록 (표시 text 기준, matcher 없으면 빈 Vec)
    pub highlights: Vec<(u32, u32)>,
    /// 규칙별 UTF-16 [start, end, rule_idx] 목록 (표시 text 기준, filter 없으면 빈 Vec)
    /// rule_idx는 필터 spec.rules의 인덱스이다.
    pub rule_highlights: Vec<(u32, u32, u32)>,
}

/// LogFile::refresh()의 판정 결과
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// 파일 상태에 변화가 없음
    Unchanged,
    /// 최초 인덱싱이 미완료라 아직 추가분을 반영하지 않음 (나중에 다시 호출)
    Pending,
    /// 추가분 반영 완료. old_line_count는 반영 전 줄 수.
    /// 반영 전 마지막 줄이 개행 없이 끝나 있었다면 그 줄의 내용이 바뀌었을 수 있다.
    Appended {
        old_line_count: u64,
        new_line_count: u64,
        new_len: u64,
    },
    /// 열린 파일 핸들의 크기가 줄어듦 (truncate)
    Truncated,
    /// 경로가 가리키는 파일이 다른 inode/dev로 바뀜(rename 로테이션) 또는 경로의 파일이 사라짐
    Replaced,
}

/// UTF-8 문자 경계를 깨지 않도록 최대 max_bytes 이하의 유효한 경계까지 후퇴하여 슬라이스를 반환한다.
pub(crate) fn truncate_utf8_boundary(bytes: &[u8]) -> &[u8] {
    if bytes.is_empty() {
        return bytes;
    }

    let n = bytes.len();
    // 마지막 바이트가 ASCII인 경우 경계가 완전함
    if (bytes[n - 1] & 0x80) == 0 {
        return bytes;
    }

    // 뒤에서부터 최대 4바이트 내에서 UTF-8 선두 바이트를 탐색
    let start_search = n.saturating_sub(4);
    for pos in (start_search..n).rev() {
        let b = bytes[pos];
        // 선두 바이트 판별: (b & 0xC0) != 0x80
        if (b & 0xC0) != 0x80 {
            let expected_len = if (b & 0xE0) == 0xC0 {
                2
            } else if (b & 0xF0) == 0xE0 {
                3
            } else if (b & 0xF8) == 0xF0 {
                4
            } else {
                1
            };

            // 선두 바이트부터 필요한 바이트가 현재 슬라이스 안에 모두 포함되는지 확인
            if pos + expected_len <= n {
                return bytes;
            } else {
                // 문자가 잘려 불완전하므로 선두 바이트 이전까지만 반환
                return &bytes[..pos];
            }
        }
    }

    // 선두 바이트를 찾지 못한 경우 그대로 반환
    bytes
}

/// 표시용 앞부분 읽기 크기 (16KB + UTF-8 문자 경계 여유)
const HEAD_READ_BYTES: usize = MAX_DISPLAY_BYTES + 8;

/// 원문 범위/전체 줄 읽기 시 한 번에 읽는 청크 크기 (1MB)
const RANGE_READ_CHUNK: usize = 1024 * 1024;

/// raw(개행 제외 원본 바이트) 한 줄을 out에 이어 붙인다. need_sep이면 앞에 '\n'을 넣는다.
/// 줄 끝의 '\r' 하나를 제거하고 lossy UTF-8로 디코딩한다 (잘림 없음).
/// 누적 바이트가 max_bytes를 넘으면 Err(Error::TooLarge)를 반환한다.
fn push_raw_line(out: &mut String, raw: &[u8], need_sep: bool, max_bytes: u64) -> Result<()> {
    let line = if raw.ends_with(b"\r") {
        &raw[..raw.len() - 1]
    } else {
        raw
    };
    if need_sep {
        out.push('\n');
    }
    out.push_str(&String::from_utf8_lossy(line));
    if out.len() as u64 > max_bytes {
        return Err(Error::TooLarge { limit: max_bytes });
    }
    Ok(())
}

/// 매치가 없는 빈 줄을 만든다 (확인된 길이 밖 오프셋 등 예외 상황용)
fn empty_line(line_no: u64) -> Line {
    Line {
        no: line_no,
        text: String::new(),
        byte_len: 0,
        truncated: false,
        highlights: Vec::new(),
        rule_highlights: Vec::new(),
    }
}

/// raw_prefix(줄의 앞부분 바이트, 개행 제외)와 실제 줄 바이트 길이(byte_len)로 Line을 만든다.
/// raw_prefix가 byte_len보다 짧으면(긴 줄의 표시분만 읽은 경우) 읽은 범위만 사용한다.
/// filter가 제공되면 규칙별 rule_highlights(UTF-16, rule_idx 포함)도 계산한다.
fn make_line(
    raw_prefix: &[u8],
    byte_len: u64,
    line_no: u64,
    matcher: Option<&Matcher>,
    filter: Option<&FilterSet>,
) -> Line {
    let take = (byte_len as usize).min(raw_prefix.len());
    let raw_line = &raw_prefix[..take];
    // 줄 전체를 읽은 경우에만 끝의 '\r'을 표시 길이 계산에서 제외한다.
    // (16KB+1바이트이고 끝이 '\r'인 줄은 \r 제외 시 16KB에 들어가 잘리지 않는다)
    let full = take as u64 == byte_len;
    let content_len = if full && raw_line.ends_with(b"\r") {
        byte_len - 1
    } else {
        byte_len
    };
    let (text, truncated) = if content_len > MAX_DISPLAY_BYTES as u64 {
        let display_slice =
            truncate_utf8_boundary(&raw_line[..MAX_DISPLAY_BYTES.min(raw_line.len())]);
        (String::from_utf8_lossy(display_slice).into_owned(), true)
    } else {
        let display_slice = if raw_line.ends_with(b"\r") {
            &raw_line[..raw_line.len() - 1]
        } else {
            raw_line
        };
        (String::from_utf8_lossy(display_slice).into_owned(), false)
    };

    let highlights = match matcher {
        Some(m) => {
            let byte_ranges = m.find_ranges(text.as_bytes());
            utf16_ranges(&text, &byte_ranges)
        }
        None => Vec::new(),
    };

    // 규칙별 하이라이트: rule_ranges는 표시 텍스트 기준 바이트 범위와 규칙 인덱스를 반환
    let rule_highlights = match filter {
        Some(f) => {
            let mut out = Vec::new();
            for (bs, be, rule_idx) in f.rule_ranges(text.as_bytes()) {
                if let Some(&(s, e)) = utf16_ranges(&text, &[(bs, be)]).first() {
                    out.push((s, e, rule_idx));
                }
            }
            out
        }
        None => Vec::new(),
    };

    Line {
        no: line_no,
        text,
        byte_len,
        truncated,
        highlights,
        rule_highlights,
    }
}

/// 지정된 오프셋에서 한 줄을 읽어 (Line, 다음 줄 시작 오프셋)을 반환한다.
/// 표시 텍스트는 앞 MAX_DISPLAY_BYTES(+경계 여유)만 읽고, 16KB를 넘는 줄은
/// 개행 위치만 찾기 위해 64KB 청크로 훑는다. 파일이 외부에서 잘려 짧게 읽혀도
/// 패닉 없이 가능한 범위의 결과를 반환한다.
fn decode_line_at(
    source: &Source,
    offset: u64,
    line_no: u64,
    matcher: Option<&Matcher>,
    filter: Option<&FilterSet>,
) -> Result<(Line, u64)> {
    let remaining = source.len().saturating_sub(offset);
    if remaining == 0 {
        return Ok((empty_line(line_no), source.len()));
    }

    let head_want = remaining.min(HEAD_READ_BYTES as u64) as usize;
    let mut head_buf = [0u8; HEAD_READ_BYTES];
    let head_len = source.read_into(&mut head_buf[..head_want], offset)?;
    if head_len == 0 {
        return Ok((empty_line(line_no), source.len()));
    }
    let head = &head_buf[..head_len];

    // head 안에서 개행을 찾는다. 없으면 16KB가 넘는 긴 줄이므로
    // head 이후를 64KB 청크로 훑어 개행 또는 파일 끝을 찾는다.
    let (byte_len, next_offset) = match memchr::memchr(b'\n', head) {
        Some(nl_pos) => (nl_pos as u64, offset + nl_pos as u64 + 1),
        None => match source.nth_newline(offset + head_len as u64, 0)? {
            Some(nl_pos) => (nl_pos - offset, nl_pos + 1),
            None => (source.len() - offset, source.len()),
        },
    };

    Ok((
        make_line(head, byte_len, line_no, matcher, filter),
        next_offset,
    ))
}

/// 줄 시작 오프셋 캐시 최대 항목 수
const LINE_START_CACHE_MAX: usize = 4096;

/// 로그 파일 핸들.
/// Source(read_at 파일)와 LineIndex(희소 줄 인덱스)를 소유하며,
/// Send + Sync를 만족하여 Arc<LogFile> 형태로 여러 스레드에서 공유할 수 있다.
pub struct LogFile {
    source: Source,
    index: LineIndex,
    /// line_no -> 시작 바이트 오프셋 소형 캐시.
    /// 체크포인트 사이에 긴 줄이 끼어 있으면 line_start 계산이 그 긴 줄을
    /// 매번 재스캔하므로, 최근에 확인한 시작 오프셋을 기억해 재스캔을 피한다.
    line_start_cache: Mutex<HashMap<u64, u64>>,
    /// refresh() 직렬화용 (동시 호출 시 체크포인트 중복 반영 방지)
    refresh_lock: Mutex<()>,
}

impl LogFile {
    /// 로그 파일을 열어 LogFile 인스턴스를 생성한다. 인덱싱은 시작하지 않는다.
    pub fn open(path: &Path) -> Result<LogFile> {
        Ok(Self::from_source(Source::open(path)?))
    }

    /// 원격(또는 임의의) 랜덤 액세스 소스로 LogFile을 연다.
    /// 초기 길이는 stat_len()으로 조회한다 (네트워크 호출 가능).
    /// 인덱싱은 시작하지 않으며, 이후 refresh()가 stat_len 증가분만 Appended로
    /// 반영한다. inode가 없어 Replaced 판정은 없고 길이 감소는 Truncated다.
    pub fn open_remote(r: Arc<dyn RandomRead>) -> Result<LogFile> {
        Ok(Self::from_source(Source::from_remote(r)?))
    }

    /// 원격 소스의 URI(예: "sftp://host/path")를 반환한다.
    /// 로컬 파일·익명 소스는 None.
    pub fn uri(&self) -> Option<String> {
        self.source.uri()
    }

    /// 이미 열린 파일 핸들(익명 소스)로부터 LogFile을 생성한다.
    /// gzip 해제 임시 파일처럼 경로가 없는 소스에 사용한다.
    /// 인덱싱은 시작하지 않으며, 이후 refresh()가 길이 증가분만 반영한다.
    pub(crate) fn from_source(source: Source) -> LogFile {
        let index = LineIndex::new(source.len());
        LogFile {
            source,
            index,
            line_start_cache: Mutex::new(HashMap::new()),
            refresh_lock: Mutex::new(()),
        }
    }

    /// N번째 줄의 시작 오프셋을 캐시를 거쳐 반환한다.
    /// 캐시 미스 시 index.line_start로 계산하고 결과를 저장한다.
    fn line_start_offset(&self, line_no: u64) -> Option<u64> {
        if let Some(&off) = self.line_start_cache.lock().unwrap().get(&line_no) {
            return Some(off);
        }
        let off = self.index.line_start(&self.source, line_no)?;
        let mut cache = self.line_start_cache.lock().unwrap();
        if cache.len() >= LINE_START_CACHE_MAX {
            cache.clear();
        }
        cache.insert(line_no, off);
        Some(off)
    }

    /// 확인된 줄 시작 오프셋을 캐시에 기록한다 (next_offset 확보 시 호출).
    fn cache_line_start(&self, line_no: u64, offset: u64) {
        if offset >= self.source.len() {
            return; // 파일 끝 이후는 유효한 줄 시작이 아님
        }
        let mut cache = self.line_start_cache.lock().unwrap();
        if cache.len() >= LINE_START_CACHE_MAX {
            cache.clear();
        }
        cache.insert(line_no, offset);
    }

    /// 파일 소스에 대한 참조를 반환한다.
    #[inline]
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// 줄 인덱스에 대한 참조를 반환한다.
    #[inline]
    pub fn index(&self) -> &LineIndex {
        &self.index
    }

    /// 인덱스 빌드를 시작한다 (블로킹).
    /// 호출자가 별도 백그라운드 스레드에서 실행해야 한다.
    pub fn build_index(
        &self,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<()> {
        self.index.build(&self.source, cancel, on_progress)
    }

    /// 지정된 범위의 줄들을 읽어온다.
    /// 현재까지 인덱싱이 완료되어 확인된 줄 범위까지만 반환한다.
    /// matcher가 제공되면 각 줄의 highlights(UTF-16 범위)를 계산하여 채운다.
    /// filter가 제공되면 각 줄의 rule_highlights(규칙별 UTF-16 범위)도 계산한다.
    pub fn read_lines(
        &self,
        start: u64,
        count: u64,
        matcher: Option<&Matcher>,
        filter: Option<&FilterSet>,
    ) -> Result<Vec<Line>> {
        let total_lines = self.index.line_count();
        if start >= total_lines || count == 0 {
            return Ok(Vec::new());
        }

        let end_line = (start + count).min(total_lines);
        let actual_count = (end_line - start) as usize;
        let mut lines = Vec::with_capacity(actual_count);

        let mut current_offset = match self.line_start_offset(start) {
            Some(offset) => offset,
            None => return Ok(Vec::new()),
        };

        // 파일을 한 번에 크게 읽어 여러 줄을 버퍼에서 디코딩한다 (pread syscall 절감).
        // 청크 크기는 요청 줄 수에 비례시켜 64KB~1MB로 잡는다.
        let chunk_size = (actual_count * 512).clamp(64 * 1024, 1024 * 1024);
        let mut buf = vec![0u8; chunk_size];
        let mut buf_start = 0u64; // buf[0]에 해당하는 파일 오프셋
        let mut buf_len = 0usize; // buf의 유효 바이트 수

        for line_no in start..end_line {
            // 현재 줄 시작 위치가 버퍼 범위 밖이면 다음 청크를 읽는다
            if current_offset < buf_start || current_offset >= buf_start + buf_len as u64 {
                buf_start = current_offset;
                buf_len = self.source.read_into(&mut buf, current_offset)?;
            }

            let rel = (current_offset - buf_start) as usize;
            let chunk = &buf[rel..buf_len];

            match memchr::memchr(b'\n', chunk) {
                Some(nl_pos) => {
                    // 개행이 청크 안에 있으면 버퍼에서 바로 디코딩
                    lines.push(make_line(
                        &chunk[..nl_pos],
                        nl_pos as u64,
                        line_no,
                        matcher,
                        filter,
                    ));
                    current_offset += nl_pos as u64 + 1;
                }
                None => {
                    // 개행이 청크에 없음: 청크 경계에 걸친 긴 줄이므로 개별 디코딩
                    let (line, next_offset) =
                        decode_line_at(&self.source, current_offset, line_no, matcher, filter)?;
                    self.cache_line_start(line_no + 1, next_offset);
                    lines.push(line);
                    current_offset = next_offset;
                }
            }
        }

        Ok(lines)
    }

    /// 오름차순 정렬된 임의 줄 번호들을 읽는다 (필터 모드용).
    /// 현재까지 확인되지 않은 줄 번호는 건너뛴다.
    /// 연속한 줄 번호는 이전 위치에서 이어서 전진하고, 멀리 떨어지면 index.line_start를 사용한다.
    /// filter가 제공되면 각 줄의 rule_highlights(규칙별 UTF-16 범위)도 계산한다.
    pub fn read_lines_at(
        &self,
        line_nos: &[u64],
        matcher: Option<&Matcher>,
        filter: Option<&FilterSet>,
    ) -> Result<Vec<Line>> {
        let total_lines = self.index.line_count();
        let mut lines: Vec<Line> = Vec::with_capacity(line_nos.len());

        let mut prev_line_no: Option<u64> = None;
        let mut current_offset: u64 = 0;

        for &line_no in line_nos {
            if line_no >= total_lines {
                // 확인 안 된 줄 번호는 건너뜀
                continue;
            }

            let target_offset = match prev_line_no {
                Some(prev) if line_no == prev => {
                    // 동일 줄 번호 재요청 시 이전 줄 복제 후 계속
                    if let Some(last_line) = lines.last() {
                        lines.push(last_line.clone());
                        continue;
                    } else {
                        current_offset
                    }
                }
                Some(prev) if line_no == prev + 1 => {
                    // 바로 다음 줄인 경우 current_offset이 이미 해당 줄 시작 위치
                    current_offset
                }
                Some(prev) if line_no > prev + 1 && line_no <= prev + 16 => {
                    // 소량(16줄 이하) 전진인 경우 순차 개행 스캔 (64KB 청크)
                    let lines_to_skip = (line_no - prev - 1) as usize;
                    match self.source.nth_newline(current_offset, lines_to_skip - 1) {
                        Ok(Some(nl_pos)) => nl_pos + 1,
                        _ => match self.line_start_offset(line_no) {
                            Some(off) => off,
                            None => continue,
                        },
                    }
                }
                _ => match self.line_start_offset(line_no) {
                    Some(off) => off,
                    None => continue,
                },
            };

            let (line, next_offset) =
                decode_line_at(&self.source, target_offset, line_no, matcher, filter)?;
            self.cache_line_start(line_no + 1, next_offset);
            lines.push(line);
            prev_line_no = Some(line_no);
            current_offset = next_offset;
        }

        Ok(lines)
    }

    /// [start, end] (양끝 포함, 0-based) 줄들의 원문을 '\n'으로 이어 붙인 문자열을 반환한다.
    /// 각 줄 끝의 '\r'은 제거하고, 잘림 없이 원문 그대로 lossy UTF-8로 디코딩한다.
    /// 확인된 줄 수를 넘는 end는 마지막 확인 줄로 clamp하며, start > end이면 빈 문자열.
    /// 누적 바이트가 max_bytes를 넘으면 Err(Error::TooLarge)를 반환한다.
    /// 시작 오프셋을 한 번 찾은 뒤 1MB 청크로 순차 전진하므로 줄 수가 많아도 빠르다.
    pub fn read_text_range(&self, start: u64, end: u64, max_bytes: u64) -> Result<String> {
        let total_lines = self.index.line_count();
        if start >= total_lines {
            return Ok(String::new());
        }
        let end = end.min(total_lines - 1);
        if start > end {
            return Ok(String::new());
        }
        let Some(mut offset) = self.line_start_offset(start) else {
            return Ok(String::new());
        };

        let mut out = String::new();
        // 청크 경계를 걸친 줄의 앞부분 원본 바이트 (개행 제외)
        let mut pending: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; RANGE_READ_CHUNK];
        let mut line_no = start;
        let mut need_sep = false;

        'outer: while line_no <= end {
            let n = self.source.read_into(&mut buf, offset)?;
            if n == 0 {
                break; // 확인된 길이 끝 (외부 truncate 포함)
            }
            offset += n as u64;
            let chunk = &buf[..n];
            let mut seg = 0usize;
            for nl in memchr::memchr_iter(b'\n', chunk) {
                // 청크 안에서 완결된 줄: pending + chunk[seg..nl]
                let raw = if pending.is_empty() {
                    &chunk[seg..nl]
                } else {
                    pending.extend_from_slice(&chunk[seg..nl]);
                    &pending[..]
                };
                push_raw_line(&mut out, raw, need_sep, max_bytes)?;
                need_sep = true;
                pending.clear();
                line_no += 1;
                seg = nl + 1;
                if line_no > end {
                    break 'outer;
                }
            }
            // 개행 없이 남은 꼬리는 다음 청크와 이어지는 줄의 일부
            pending.extend_from_slice(&chunk[seg..]);
            // 디코딩 결과는 원본보다 작아지지 않으므로(줄 끝 '\r' 최대 1바이트 감소)
            // 이 시점에 이미 예산 초과가 확실하면 큰 줄을 끝까지 읽지 않고 중단한다.
            if out.len() as u64 + pending.len() as u64 > max_bytes.saturating_add(1) {
                return Err(Error::TooLarge { limit: max_bytes });
            }
        }

        // 개행 없이 파일 끝까지 읽었는데 pending이 비어 있으면
        // (읽는 도중 파일이 잘린 경우) 빈 줄을 추가하지 않는다.
        if line_no <= end && !pending.is_empty() {
            push_raw_line(&mut out, &pending, need_sep, max_bytes)?;
        }

        Ok(out)
    }

    /// no 번째 줄의 원문 전체를 반환한다 (잘림 없음, 끝의 '\r' 제거, lossy UTF-8).
    /// 줄이 max_bytes보다 길면 앞 max_bytes까지만 UTF-8 경계에 맞춰 자르고 truncated=true.
    /// 줄이 없으면 Ok(None).
    pub fn read_full_line(&self, no: u64, max_bytes: u64) -> Result<Option<(String, bool)>> {
        let total_lines = self.index.line_count();
        if no >= total_lines {
            return Ok(None);
        }
        let Some(mut offset) = self.line_start_offset(no) else {
            return Ok(None);
        };

        // max_bytes + 1바이트까지만 읽어 잘림 여부를 판정한다.
        // 개행을 만나면 그 앞까지가 줄 원문이고, 개행 없이 max_bytes를 넘으면 잘린 것으로 본다.
        let mut raw: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; RANGE_READ_CHUNK];
        while raw.len() as u64 <= max_bytes {
            let want = max_bytes
                .saturating_add(1)
                .saturating_sub(raw.len() as u64)
                .min(buf.len() as u64) as usize;
            let n = self.source.read_into(&mut buf[..want], offset)?;
            if n == 0 {
                break; // 파일 끝: 개행 없이 끝난 마지막 줄
            }
            offset += n as u64;
            if let Some(nl) = memchr::memchr(b'\n', &buf[..n]) {
                raw.extend_from_slice(&buf[..nl]);
                break;
            }
            raw.extend_from_slice(&buf[..n]);
        }

        let (text, truncated) = if raw.len() as u64 > max_bytes {
            // 자르는 위치가 UTF-8 문자 중간이면 경계까지 후퇴한다
            let cut = truncate_utf8_boundary(&raw[..max_bytes as usize]);
            (String::from_utf8_lossy(cut).into_owned(), true)
        } else {
            let line = if raw.ends_with(b"\r") {
                &raw[..raw.len() - 1]
            } else {
                &raw[..]
            };
            (String::from_utf8_lossy(line).into_owned(), false)
        };

        Ok(Some((text, truncated)))
    }

    /// 파일 상태를 확인하고 끝에 추가된 부분만 인덱스에 반영한다 (tail 모드용).
    ///
    /// 판정 순서: Replaced → Truncated → (인덱싱 미완료면 Pending) →
    /// 길이 증가면 Appended → Unchanged.
    /// 파일이 외부에서 잘렸거나 교체된 경우 인덱스는 건드리지 않고 해당 결과만 반환한다.
    pub fn refresh(&self) -> Result<RefreshOutcome> {
        // 동시 refresh 호출 직렬화: 체크포인트 중복 반영과 경합을 막는다
        let _guard = self.refresh_lock.lock().unwrap();

        let file_len = self.source.file_len()?;
        let known_len = self.source.len();

        match self.source.kind() {
            SourceKind::LocalPath => {
                // 경로가 같은 파일을 가리키는지 먼저 확인 (rename 로테이션/삭제 감지)
                if !self.source.path_matches()? {
                    self.line_start_cache.lock().unwrap().clear();
                    return Ok(RefreshOutcome::Replaced);
                }

                // 열린 파일 핸들의 현재 크기가 확인된 길이보다 작으면 truncate
                if file_len < known_len {
                    self.line_start_cache.lock().unwrap().clear();
                    return Ok(RefreshOutcome::Truncated);
                }
            }
            SourceKind::Remote => {
                // 원격 소스는 inode가 없어 교체 판정이 불가하다.
                // 길이가 줄면 truncate로 보고, 증가는 아래 공통 로직에서 Appended로 반영한다.
                if file_len < known_len {
                    self.line_start_cache.lock().unwrap().clear();
                    return Ok(RefreshOutcome::Truncated);
                }
            }
            SourceKind::LocalAnonymous => {
                // 익명 소스(해제 임시 파일 등)는 경로 비교·truncate 판정 없이
                // 핸들의 길이 증가만 Appended로 반영한다
                if file_len <= known_len {
                    return Ok(RefreshOutcome::Unchanged);
                }
            }
        }

        // 최초 인덱싱이 끝나기 전에는 추가분을 반영할 수 없다
        if !self.index.progress().done {
            return Ok(RefreshOutcome::Pending);
        }

        if file_len == known_len {
            return Ok(RefreshOutcome::Unchanged);
        }

        // 추가분 인덱싱: extend가 new_len까지 읽을 수 있도록 길이를 먼저 갱신한다.
        // 이 시점의 인덱스는 여전히 이전 범위만 공개하므로 일반 읽기에는 영향이 없다.
        self.source.set_len(file_len);
        let old_line_count = self.index.line_count();
        if let Err(e) = self.index.extend(&self.source, file_len) {
            // 인덱스가 따라가지 못했으므로 확인된 길이를 되돌려 다음 refresh가 재시도하게 한다
            self.source.set_len(known_len);
            return Err(e);
        }
        // 이전 마지막 줄이 개행 없이 끝나 있었다면 추가분과 이어져
        // 줄 경계가 바뀔 수 있으므로 시작 오프셋 캐시를 무효화한다
        self.line_start_cache.lock().unwrap().clear();

        // 실제로 커버된 길이까지만 확인된 길이로 확정한다.
        // (읽는 도중 파일이 잘려 일부만 반영됐으면 다음 refresh가 나머지를 이어서 처리)
        let covered_len = self.index.progress().total_bytes;
        self.source.set_len(covered_len);
        Ok(RefreshOutcome::Appended {
            old_line_count,
            new_line_count: self.index.line_count(),
            new_len: covered_len,
        })
    }
}
