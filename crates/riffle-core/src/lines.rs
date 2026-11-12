use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::Result;
use crate::index::{IndexProgress, LineIndex};
use crate::search::{Matcher, utf16_ranges};
use crate::source::Source;

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

/// 지정된 오프셋에서 한 줄을 디코딩하고 (Line, 다음 줄 시작 오프셋)을 반환한다.
fn decode_line_at(
    source: &Source,
    offset: u64,
    line_no: u64,
    matcher: Option<&Matcher>,
) -> (Line, u64) {
    let remaining_bytes = source.bytes(offset, source.len());
    let (raw_line, next_offset) = match memchr::memchr(b'\n', remaining_bytes) {
        Some(nl_pos) => (&remaining_bytes[..nl_pos], offset + nl_pos as u64 + 1),
        None => (remaining_bytes, source.len()),
    };

    let byte_len = raw_line.len() as u64;
    let (text, truncated) = if raw_line.len() > MAX_DISPLAY_BYTES {
        let display_slice = truncate_utf8_boundary(&raw_line[..MAX_DISPLAY_BYTES]);
        let text = String::from_utf8_lossy(display_slice).into_owned();
        (text, true)
    } else {
        let display_slice = if raw_line.ends_with(b"\r") {
            &raw_line[..raw_line.len() - 1]
        } else {
            raw_line
        };
        let text = String::from_utf8_lossy(display_slice).into_owned();
        (text, false)
    };

    let highlights = match matcher {
        Some(m) => {
            let byte_ranges = m.find_ranges(text.as_bytes());
            utf16_ranges(&text, &byte_ranges)
        }
        None => Vec::new(),
    };

    (
        Line {
            no: line_no,
            text,
            byte_len,
            truncated,
            highlights,
        },
        next_offset,
    )
}

/// 로그 파일 핸들.
/// Source(mmap 파일)와 LineIndex(희소 줄 인덱스)를 소유하며,
/// Send + Sync를 만족하여 Arc<LogFile> 형태로 여러 스레드에서 공유할 수 있다.
pub struct LogFile {
    source: Source,
    index: LineIndex,
}

impl LogFile {
    /// 로그 파일을 열어 LogFile 인스턴스를 생성한다. 인덱싱은 시작하지 않는다.
    pub fn open(path: &Path) -> Result<LogFile> {
        let source = Source::open(path)?;
        let index = LineIndex::new(source.len());
        Ok(LogFile { source, index })
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
    pub fn read_lines(
        &self,
        start: u64,
        count: u64,
        matcher: Option<&Matcher>,
    ) -> Result<Vec<Line>> {
        let total_lines = self.index.line_count();
        if start >= total_lines || count == 0 {
            return Ok(Vec::new());
        }

        let end_line = (start + count).min(total_lines);
        let actual_count = (end_line - start) as usize;
        let mut lines = Vec::with_capacity(actual_count);

        let mut current_offset = match self.index.line_start(&self.source, start) {
            Some(offset) => offset,
            None => return Ok(Vec::new()),
        };

        for line_no in start..end_line {
            let (line, next_offset) =
                decode_line_at(&self.source, current_offset, line_no, matcher);
            lines.push(line);
            current_offset = next_offset;
        }

        Ok(lines)
    }

    /// 오름차순 정렬된 임의 줄 번호들을 읽는다 (필터 모드용).
    /// 현재까지 확인되지 않은 줄 번호는 건너뛴다.
    /// 연속한 줄 번호는 이전 위치에서 이어서 전진하고, 멀리 떨어지면 index.line_start를 사용한다.
    pub fn read_lines_at(&self, line_nos: &[u64], matcher: Option<&Matcher>) -> Result<Vec<Line>> {
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
                    // 소량(16줄 이하) 전진인 경우 순차 개행 스캔
                    let remaining = self.source.bytes(current_offset, self.source.len());
                    let lines_to_skip = (line_no - prev - 1) as usize;
                    let mut iter = memchr::memchr_iter(b'\n', remaining);
                    match iter.nth(lines_to_skip - 1) {
                        Some(nl_pos) => current_offset + nl_pos as u64 + 1,
                        None => match self.index.line_start(&self.source, line_no) {
                            Some(off) => off,
                            None => continue,
                        },
                    }
                }
                _ => match self.index.line_start(&self.source, line_no) {
                    Some(off) => off,
                    None => continue,
                },
            };

            let (line, next_offset) = decode_line_at(&self.source, target_offset, line_no, matcher);
            lines.push(line);
            prev_line_no = Some(line_no);
            current_offset = next_offset;
        }

        Ok(lines)
    }
}
