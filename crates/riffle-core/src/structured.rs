//! CSV / JSON Lines 컬럼 뷰 지원.
//! 파일 앞부분 표본으로 형식을 감지하고(`detect_columns`), 한 줄을 컬럼 값
//! 목록으로 분할한다(`split_fields`). 한 줄 = 한 행이며, CSV의 따옴표 안
//! 줄바꿈(다중 줄 레코드)은 지원하지 않는다 (로그 뷰어 범위).

use serde_json::Value;

use crate::error::Result;
use crate::lines::LogFile;

/// 형식 감지 표본의 최대 바이트 (파일 앞 1MB)
const SAMPLE_MAX_BYTES: u64 = 1024 * 1024;
/// 형식 감지 표본의 최대 줄 수
const SAMPLE_MAX_LINES: usize = 500;
/// JSONL 컬럼 이름으로 채택하는 키의 최대 개수.
/// 표본 키가 이보다 많으면 앞 64개만 이름으로 쓰고 나머지는 EXTRA_COLUMN에 모은다.
const MAX_JSON_KEYS: usize = 64;
/// 컬럼 이름 목록에 없는 JSON 키를 모아두는 예비 컬럼 이름
pub const EXTRA_COLUMN: &str = "_extra";
/// 형식 판정에 필요한 표본 내 비율 (90%)
const DETECT_RATIO_NUM: usize = 9;
const DETECT_RATIO_DEN: usize = 10;

/// 감지된 파일 형식
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogFormat {
    /// 일반 텍스트 로그 (컬럼 구조 없음)
    Plain,
    /// CSV 류 (delimiter로 구분). has_header는 첫 줄이 헤더로 보이는지 여부.
    Csv { delimiter: u8, has_header: bool },
    /// JSON Lines (한 줄에 JSON 객체 하나)
    JsonLines,
}

/// 파일의 컬럼 정보. names는 컬럼 이름 목록이며 표시 순서를 유지한다.
/// Plain이면 names는 비어 있다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Columns {
    pub format: LogFormat,
    pub names: Vec<String>,
}

/// 파일 앞부분 표본(최대 1MB, 500줄)으로 형식을 감지한다.
/// 판정 순서: JSON Lines → CSV → Plain.
pub fn detect_columns(file: &LogFile) -> Result<Columns> {
    let lines = sample_lines(file)?;
    if let Some(cols) = detect_jsonl(&lines) {
        return Ok(cols);
    }
    if let Some(cols) = detect_csv(&lines) {
        return Ok(cols);
    }
    Ok(Columns {
        format: LogFormat::Plain,
        names: Vec::new(),
    })
}

/// 한 줄을 컬럼 값으로 분할한다.
/// 반환 길이는 항상 names.len()이다. 부족하면 ""로 채우고,
/// 넘치면 마지막 컬럼에 나머지를 구분자로 합친다.
/// CSV는 RFC4180 따옴표("" 이스케이프)를 인식한다.
/// JSONL은 키별 값을 넣고 names에 없는 키는 EXTRA_COLUMN에 compact JSON 객체로 모은다.
/// JSON 파싱 실패 줄은 첫 컬럼에 원문, 나머지 ""다. Plain이면 줄 전체 하나를 반환한다.
pub fn split_fields(line: &str, cols: &Columns) -> Vec<String> {
    match &cols.format {
        LogFormat::Plain => vec![line.to_string()],
        LogFormat::Csv { delimiter, .. } => split_csv_fields(line, *delimiter, cols.names.len()),
        LogFormat::JsonLines => split_json_fields(line, &cols.names),
    }
}

/// 표본 줄 목록을 반환한다 (비어있는 줄 제외, 끝의 '\r' 제거, 최대 500줄).
fn sample_lines(file: &LogFile) -> Result<Vec<String>> {
    let source = file.source();
    let want = source.len().min(SAMPLE_MAX_BYTES);
    let buf = source.read_range(0, want)?;
    // 표본이 파일 중간에서 잘렸다면 마지막 줄은 불완전하므로 버린다
    let truncated = source.len() > want;
    let text = String::from_utf8_lossy(&buf);
    let mut parts: Vec<&str> = text.split('\n').collect();
    if truncated || parts.last() == Some(&"") {
        parts.pop();
    }
    Ok(parts
        .into_iter()
        .take(SAMPLE_MAX_LINES)
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect())
}

/// 표본이 JSON Lines이면 컬럼 정보를 반환한다.
/// 비어있지 않은 표본 줄의 90% 이상이 `{`로 시작하고 JSON 객체로 파싱돼야 한다.
/// names는 표본 객체 최상위 키의 합집합(처음 등장 순서)이며 최대 64개다.
/// 64개를 넘는 키가 있으면 마지막에 EXTRA_COLUMN을 추가한다.
fn detect_jsonl(lines: &[String]) -> Option<Columns> {
    if lines.is_empty() {
        return None;
    }
    let ok = lines
        .iter()
        .filter(|l| l.trim_start().starts_with('{') && parse_json_object(l).is_some())
        .count();
    if ok * DETECT_RATIO_DEN < lines.len() * DETECT_RATIO_NUM {
        return None;
    }

    let mut names: Vec<String> = Vec::new();
    let mut overflow = false;
    for line in lines {
        if parse_json_object(line).is_none() {
            continue;
        }
        // serde_json::Map은 키 정렬 순서로 순회하므로, 표시 순서(처음 등장
        // 순서)는 문서 상의 키 위치로 직접 추출한다
        for key in json_object_keys_in_order(line) {
            if names.iter().any(|n| n == &key) {
                continue;
            }
            if names.len() < MAX_JSON_KEYS {
                names.push(key);
            } else {
                overflow = true;
            }
        }
    }
    if overflow && !names.iter().any(|n| n == EXTRA_COLUMN) {
        names.push(EXTRA_COLUMN.to_string());
    }
    Some(Columns {
        format: LogFormat::JsonLines,
        names,
    })
}

/// 줄을 JSON 객체로 파싱한다. 객체가 아니거나 실패하면 None.
fn parse_json_object(line: &str) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_str::<Value>(line.trim_start()) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// JSON 객체 줄의 최상위 키를 문서 등장 순서로 추출한다.
/// 깊이 1의 문자열 토큰 중 뒤에 ':'가 오는 것만 키로 본다.
/// (유효한 JSON 객체라는 전제 하에 쓰인다)
fn json_object_keys_in_order(line: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let bytes = line.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            b'"' => {
                // 문자열 토큰의 끝을 찾는다 (\" 이스케이프 고려)
                let start = i;
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'"' => break,
                        _ => {}
                    }
                    i += 1;
                }
                // 깊이 1의 문자열이고 바로 뒤가 ':'이면 객체 키다
                if depth == 1 {
                    let mut j = i + 1;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j < bytes.len()
                        && bytes[j] == b':'
                        && let Ok(key) = serde_json::from_str::<String>(&line[start..=i])
                    {
                        keys.push(key);
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    keys
}

/// CSV 후보 구분자 (`,` `\t` `;` `|`). 이 순서대로 우선순위를 가진다.
const CSV_DELIMITERS: [u8; 4] = *b",\t;|";

/// 표본이 CSV 류이면 컬럼 정보를 반환한다.
/// 후보 구분자별로 표본 줄의 필드 수 분포를 보고, 최빈 필드 수가 2 이상이고
/// 그 비율이 90% 이상인 구분자 중 가장 일정한 것(최빈값 비율 최대, 동률이면
/// 후보 순서 우선)을 선택한다.
fn detect_csv(lines: &[String]) -> Option<Columns> {
    if lines.is_empty() {
        return None;
    }
    let mut best: Option<(u8, usize, usize)> = None; // (구분자, 최빈 필드 수, 최빈 빈도)
    for &d in &CSV_DELIMITERS {
        // 필드 수별 빈도 (표본 줄 수가 최대 500이라 선형 스캔으로 충분하다)
        let mut counts: Vec<(usize, usize)> = Vec::new();
        for line in lines {
            let n = parse_csv_line(line, d).len();
            match counts.iter_mut().find(|(f, _)| *f == n) {
                Some((_, c)) => *c += 1,
                None => counts.push((n, 1)),
            }
        }
        let Some(&(mode_fields, mode_count)) = counts.iter().max_by_key(|&&(f, c)| (c, f)) else {
            continue;
        };
        if mode_fields < 2 || mode_count * DETECT_RATIO_DEN < lines.len() * DETECT_RATIO_NUM {
            continue;
        }
        let is_better = match best {
            None => true,
            Some((_, _, best_count)) => mode_count > best_count,
        };
        if is_better {
            best = Some((d, mode_fields, mode_count));
        }
    }
    let (delimiter, mode_fields, _) = best?;

    let has_header = detect_csv_header(lines, delimiter);
    let names = if has_header {
        parse_csv_line(&lines[0], delimiter)
    } else {
        (1..=mode_fields).map(|i| format!("열{i}")).collect()
    };
    Some(Columns {
        format: LogFormat::Csv {
            delimiter,
            has_header,
        },
        names,
    })
}

/// 첫 줄이 CSV 헤더로 보이는지 판정한다.
/// 첫 줄의 필드가 모두 비어있지 않고 숫자가 아니며 서로 다르고,
/// 둘째 줄 이후에 숫자 필드가 하나라도 있으면 헤더로 본다.
fn detect_csv_header(lines: &[String], delimiter: u8) -> bool {
    let first = parse_csv_line(&lines[0], delimiter);
    let header_like = first.iter().all(|f| !f.trim().is_empty() && !is_numeric(f))
        && first
            .iter()
            .enumerate()
            .all(|(i, f)| !first[..i].iter().any(|g| g == f));
    if !header_like {
        return false;
    }
    lines[1..].iter().any(|line| {
        parse_csv_line(line, delimiter)
            .iter()
            .any(|f| is_numeric(f))
    })
}

/// 필드가 숫자 리터럴인지 판정한다 (부호·소수점·지수 표기 포함, NaN/inf 제외).
fn is_numeric(field: &str) -> bool {
    let f = field.trim();
    !f.is_empty()
        && f.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'))
        && f.parse::<f64>().is_ok()
}

/// CSV 한 줄을 따옴표 인식으로 분할한다.
/// `"`로 감싼 필드 안의 구분자는 데이터로 취급하고, `""`는 이스케이프된 따옴표다.
fn parse_csv_line(line: &str, delimiter: u8) -> Vec<String> {
    let delim = delimiter as char;
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes && chars.peek() == Some(&'"') => {
                // 이스케이프된 따옴표
                cur.push('"');
                chars.next();
            }
            '"' => in_quotes = !in_quotes,
            c if c == delim && !in_quotes => fields.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

/// CSV 분할 결과를 컬럼 수에 맞춘다.
/// 부족하면 ""로 채우고, 넘치면 마지막 컬럼에 나머지를 구분자로 합친다.
fn split_csv_fields(line: &str, delimiter: u8, n: usize) -> Vec<String> {
    let mut fields = parse_csv_line(line, delimiter);
    if fields.len() < n {
        fields.resize(n, String::new());
    } else if fields.len() > n && n > 0 {
        let merged = fields
            .split_off(n - 1)
            .join(&(delimiter as char).to_string());
        fields.push(merged);
    }
    fields
}

/// JSONL 한 줄을 names 순서의 값 목록으로 변환한다.
/// 문자열은 값 그대로, 숫자/불/null은 JSON 표기 그대로, 객체/배열은 compact JSON이다.
/// names에 없는 키는 EXTRA_COLUMN에 compact JSON 객체로 모은다 (없으면 버림).
/// 파싱 실패 또는 객체가 아닌 줄은 첫 컬럼에 원문, 나머지 ""다.
fn split_json_fields(line: &str, names: &[String]) -> Vec<String> {
    let mut fields = vec![String::new(); names.len()];
    let Some(map) = parse_json_object(line) else {
        if let Some(first) = fields.first_mut() {
            *first = line.to_string();
        }
        return fields;
    };

    let extra_idx = names.iter().position(|n| n == EXTRA_COLUMN);
    let mut extras = serde_json::Map::new();
    for (key, value) in &map {
        match names.iter().position(|n| n == key) {
            Some(i) => fields[i] = json_field_string(value),
            None => {
                extras.insert(key.clone(), value.clone());
            }
        }
    }
    if let Some(i) = extra_idx
        && !extras.is_empty()
    {
        fields[i] = Value::Object(extras).to_string();
    }
    fields
}

/// JSON 값의 컬럼 표기: 문자열은 내용 그대로, 그 외는 compact JSON 표기.
/// (숫자는 serde가 정규화하므로 1e2 → 100.0 처럼 원본 표기와 다를 수 있다)
fn json_field_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 임시 파일을 만들고 내용을 쓴다. Drop 시 삭제된다.
    struct TmpFile {
        path: std::path::PathBuf,
    }

    impl TmpFile {
        fn new(content: &[u8]) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "riffle_struct_test_{}_{}.tmp",
                std::process::id(),
                id
            ));
            let mut f = std::fs::File::create(&path).expect("임시 파일 생성 실패");
            f.write_all(content).expect("임시 파일 쓰기 실패");
            Self { path }
        }
    }

    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn detect(content: &[u8]) -> Columns {
        let tmp = TmpFile::new(content);
        let file = LogFile::open(&tmp.path).expect("LogFile::open 실패");
        detect_columns(&file).expect("detect_columns 실패")
    }

    // ===== detect_columns: JSONL =====

    #[test]
    fn test_detect_jsonl_basic() {
        let cols = detect(
            br#"{"level":"info","msg":"hello","status":200}
{"level":"error","msg":"boom","status":500}
"#,
        );
        assert_eq!(cols.format, LogFormat::JsonLines);
        // 처음 등장 순서 유지
        assert_eq!(cols.names, vec!["level", "msg", "status"]);
    }

    #[test]
    fn test_detect_jsonl_missing_and_order() {
        // 키 누락·순서가 다른 줄이 섞여도 합집합(첫 등장 순서)을 유지한다
        let cols = detect(
            br#"{"a":1,"b":2}
{"c":3,"a":4}
{"b":5,"d":6}
"#,
        );
        assert_eq!(cols.format, LogFormat::JsonLines);
        assert_eq!(cols.names, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn test_detect_jsonl_document_key_order() {
        // 정렬 순서가 아니라 문서 등장 순서를 유지해야 한다
        let cols = detect(
            br#"{"z":1,"a":2,"m":3}
{"q":4,"a":5}
"#,
        );
        assert_eq!(cols.format, LogFormat::JsonLines);
        assert_eq!(cols.names, vec!["z", "a", "m", "q"]);
    }

    #[test]
    fn test_detect_jsonl_nested() {
        let cols = detect(br#"{"a":{"x":1},"b":[1,2]}"#);
        assert_eq!(cols.format, LogFormat::JsonLines);
        assert_eq!(cols.names, vec!["a", "b"]);
    }

    #[test]
    fn test_detect_jsonl_over_64_keys() {
        // 66개의 서로 다른 키 → 앞 64개 + _extra
        let mut content = String::from("{");
        for i in 0..66 {
            if i > 0 {
                content.push(',');
            }
            content.push_str(&format!("\"k{:02}\":{}", i, i));
        }
        content.push_str("}\n");
        let cols = detect(content.as_bytes());
        assert_eq!(cols.format, LogFormat::JsonLines);
        assert_eq!(cols.names.len(), 65);
        assert_eq!(cols.names[0], "k00");
        assert_eq!(cols.names[63], "k63");
        assert_eq!(cols.names[64], EXTRA_COLUMN);
    }

    #[test]
    fn test_detect_jsonl_ratio_threshold() {
        // 10줄 중 9줄만 JSON 객체 → 90% 경계로 JSONL
        let mut content = String::new();
        for i in 0..9 {
            content.push_str(&format!("{{\"a\":{}}}\n", i));
        }
        content.push_str("not json line\n");
        assert_eq!(detect(content.as_bytes()).format, LogFormat::JsonLines);

        // 10줄 중 8줄만 JSON 객체 → JSONL 아님
        let mut content2 = String::new();
        for i in 0..8 {
            content2.push_str(&format!("{{\"a\":{}}}\n", i));
        }
        content2.push_str("not json\n");
        content2.push_str("also not\n");
        assert_eq!(detect(content2.as_bytes()).format, LogFormat::Plain);
    }

    // ===== detect_columns: CSV =====

    #[test]
    fn test_detect_csv_comma_with_header() {
        let cols = detect(b"name,age,city\nalice,30,seoul\nbob,25,busan\n");
        assert_eq!(
            cols.format,
            LogFormat::Csv {
                delimiter: b',',
                has_header: true
            }
        );
        assert_eq!(cols.names, vec!["name", "age", "city"]);
    }

    #[test]
    fn test_detect_csv_without_header() {
        let cols = detect(b"1,2,3\n4,5,6\n7,8,9\n");
        match cols.format {
            LogFormat::Csv {
                delimiter,
                has_header,
            } => {
                assert_eq!(delimiter, b',');
                assert!(!has_header);
            }
            other => panic!("CSV 기대, 실제 {other:?}"),
        }
        assert_eq!(cols.names, vec!["열1", "열2", "열3"]);
    }

    #[test]
    fn test_detect_csv_delimiters() {
        for (d, name) in [(b'\t', "탭"), (b';', "세미콜론"), (b'|', "파이프")] {
            let sep = (d as char).to_string();
            let content = format!("a{0}1{0}x\nb{0}2{0}y\nc{0}3{0}z\n", sep);
            let cols = detect(content.as_bytes());
            match cols.format {
                LogFormat::Csv { delimiter, .. } => {
                    assert_eq!(delimiter, d, "{name} 구분자 감지 실패");
                }
                other => panic!("{name} CSV 기대, 실제 {other:?}"),
            }
        }
    }

    #[test]
    fn test_detect_csv_quoted_delimiter() {
        // 따옴표 안의 쉼표는 필드 경계가 아니므로 필드 수 3이 일정하다
        let cols = detect(b"name,note,num\n\"a,b\",\"x,y\",1\n\"c\",\"d\",2\n");
        match cols.format {
            LogFormat::Csv {
                delimiter,
                has_header,
            } => {
                assert_eq!(delimiter, b',');
                assert!(has_header);
            }
            other => panic!("CSV 기대, 실제 {other:?}"),
        }
        assert_eq!(cols.names, vec!["name", "note", "num"]);
    }

    #[test]
    fn test_detect_csv_consistency_threshold() {
        // 필드 수가 90% 미만으로 일정하지 않으면 CSV가 아니다
        let mut content = String::new();
        content.push_str("a,b,c\n");
        for i in 0..9 {
            content.push_str(&format!("line {} no commas\n", i));
        }
        assert_eq!(detect(content.as_bytes()).format, LogFormat::Plain);
    }

    #[test]
    fn test_detect_csv_header_needs_numeric_below() {
        // 첫 줄 조건은 맞지만 이후 줄에 숫자가 없으면 헤더 아님
        let cols = detect(b"name,city,code\nalice,seoul,ab\nbob,busan,cd\n");
        match cols.format {
            LogFormat::Csv { has_header, .. } => assert!(!has_header),
            other => panic!("CSV 기대, 실제 {other:?}"),
        }
        assert_eq!(cols.names, vec!["열1", "열2", "열3"]);
    }

    // ===== detect_columns: Plain / 빈 파일 / 혼합 =====

    #[test]
    fn test_detect_plain_log() {
        let content = b"2024-01-01 INFO started\n2024-01-01 ERROR failed\nplain text\n";
        let cols = detect(content);
        assert_eq!(cols.format, LogFormat::Plain);
        assert!(cols.names.is_empty());
    }

    #[test]
    fn test_detect_mixed_returns_plain_or_csv_not_jsonl() {
        // 일반 로그와 JSON이 섞이면 JSONL이 아니다 (90% 미달)
        let content = b"log line one\n{\"a\":1}\nlog line two\nlog line three\n";
        let cols = detect(content);
        assert_eq!(cols.format, LogFormat::Plain);
    }

    #[test]
    fn test_detect_empty_file() {
        let cols = detect(b"");
        assert_eq!(cols.format, LogFormat::Plain);
        assert!(cols.names.is_empty());
    }

    #[test]
    fn test_detect_only_blank_lines() {
        let cols = detect(b"\n\n   \n\n");
        assert_eq!(cols.format, LogFormat::Plain);
    }

    // ===== split_fields: CSV =====

    fn csv_cols(names: &[&str]) -> Columns {
        Columns {
            format: LogFormat::Csv {
                delimiter: b',',
                has_header: true,
            },
            names: names.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_split_csv_basic() {
        let cols = csv_cols(&["a", "b", "c"]);
        assert_eq!(
            split_fields("1,2,3", &cols),
            vec!["1".to_string(), "2".to_string(), "3".to_string()]
        );
    }

    #[test]
    fn test_split_csv_quotes() {
        let cols = csv_cols(&["a", "b"]);
        // 따옴표 안 구분자 + "" 이스케이프
        assert_eq!(
            split_fields("\"x,y\",\"he said \"\"hi\"\"\"", &cols),
            vec!["x,y".to_string(), "he said \"hi\"".to_string()]
        );
    }

    #[test]
    fn test_split_csv_short_and_long() {
        let cols = csv_cols(&["a", "b", "c"]);
        // 부족 → "" 채움
        assert_eq!(
            split_fields("1", &cols),
            vec!["1".to_string(), String::new(), String::new()]
        );
        // 초과 → 마지막 컬럼에 나머지를 구분자로 합침
        assert_eq!(
            split_fields("1,2,3,4,5", &cols),
            vec!["1".to_string(), "2".to_string(), "3,4,5".to_string()]
        );
    }

    #[test]
    fn test_split_csv_korean() {
        let cols = csv_cols(&["이름", "메시지"]);
        assert_eq!(
            split_fields("홍길동,\"안녕, 세계\"", &cols),
            vec!["홍길동".to_string(), "안녕, 세계".to_string()]
        );
    }

    // ===== split_fields: JSONL =====

    fn jsonl_cols(names: &[&str]) -> Columns {
        Columns {
            format: LogFormat::JsonLines,
            names: names.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_split_json_types() {
        let cols = jsonl_cols(&["level", "status", "ok", "nil", "obj"]);
        let line = r#"{"level":"error","status":500,"ok":true,"nil":null,"obj":{"a":1},"extra":9}"#;
        let fields = split_fields(line, &cols);
        assert_eq!(
            fields,
            vec![
                "error".to_string(),
                "500".to_string(),
                "true".to_string(),
                "null".to_string(),
                r#"{"a":1}"#.to_string()
            ]
        );
    }

    #[test]
    fn test_split_json_missing_keys() {
        let cols = jsonl_cols(&["a", "b", "c"]);
        assert_eq!(
            split_fields(r#"{"b":2}"#, &cols),
            vec![String::new(), "2".to_string(), String::new()]
        );
    }

    #[test]
    fn test_split_json_extra_keys_column() {
        let cols = jsonl_cols(&["a", EXTRA_COLUMN]);
        let fields = split_fields(r#"{"a":1,"x":2,"y":{"z":3}}"#, &cols);
        assert_eq!(fields[0], "1");
        // names에 없는 키는 compact JSON 객체로 _extra에
        assert_eq!(fields[1], r#"{"x":2,"y":{"z":3}}"#);
    }

    #[test]
    fn test_split_json_extra_keys_dropped_without_column() {
        // _extra 컬럼이 없으면 names에 없는 키는 버린다
        let cols = jsonl_cols(&["a"]);
        assert_eq!(
            split_fields(r#"{"a":1,"x":2}"#, &cols),
            vec!["1".to_string()]
        );
    }

    #[test]
    fn test_split_json_parse_failure() {
        let cols = jsonl_cols(&["a", "b"]);
        let fields = split_fields("not json at all", &cols);
        assert_eq!(fields[0], "not json at all");
        assert_eq!(fields[1], "");
        // 객체가 아닌 JSON(배열)도 파싱 실패와 동일하게 취급
        let fields = split_fields("[1,2,3]", &cols);
        assert_eq!(fields[0], "[1,2,3]");
    }

    #[test]
    fn test_split_json_korean() {
        let cols = jsonl_cols(&["메시지", "레벨"]);
        let fields = split_fields(r#"{"레벨":"에러","메시지":"결제 실패"}"#, &cols);
        assert_eq!(fields, vec!["결제 실패".to_string(), "에러".to_string()]);
    }

    #[test]
    fn test_split_plain() {
        let cols = Columns {
            format: LogFormat::Plain,
            names: Vec::new(),
        };
        assert_eq!(
            split_fields("anything", &cols),
            vec!["anything".to_string()]
        );
    }

    // ===== is_numeric =====

    #[test]
    fn test_is_numeric() {
        for f in ["1", "-2.5", "1e3", "+7", ".5", " 42 "] {
            assert!(is_numeric(f), "{f}는 숫자여야 함");
        }
        for f in ["", "abc", "12a", "NaN", "inf", "1,000"] {
            assert!(!is_numeric(f), "{f}는 숫자가 아니어야 함");
        }
    }
}
