use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::Result;
use crate::index::{IndexProgress, LineIndex};
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
    pub fn read_lines(&self, start: u64, count: u64) -> Result<Vec<Line>> {
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
            let remaining_bytes = self.source.bytes(current_offset, self.source.len());
            let (raw_line, next_offset) = match memchr::memchr(b'\n', remaining_bytes) {
                Some(nl_pos) => (
                    &remaining_bytes[..nl_pos],
                    current_offset + nl_pos as u64 + 1,
                ),
                None => (remaining_bytes, self.source.len()),
            };

            let byte_len = raw_line.len() as u64;
            let (text, truncated) = if raw_line.len() > MAX_DISPLAY_BYTES {
                // 16KB 초과 줄: 줄 전체를 String으로 복사하지 않고 앞 16KB만 UTF-8 경계에 맞춰 디코딩
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

            lines.push(Line {
                no: line_no,
                text,
                byte_len,
                truncated,
            });

            current_offset = next_offset;
        }

        Ok(lines)
    }
}
