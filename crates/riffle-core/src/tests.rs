use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::search::{
    Matcher, SearchConfig, SearchOptions, SearchOutcome, search, search_with_config, utf16_ranges,
};
use crate::{Error, LogFile, MAX_DISPLAY_BYTES};

/// 테스트용 RAII 임시 파일 도우미. 스코프를 벗어나면 파일이 자동 삭제된다.
struct TempTestFile {
    path: PathBuf,
}

impl TempTestFile {
    fn new(prefix: &str, content: &[u8]) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let filename = format!("riffle_test_{}_{}_{}.tmp", prefix, pid, id);
        let path = std::env::temp_dir().join(filename);
        let mut file = File::create(&path).expect("임시 파일 생성 실패");
        file.write_all(content).expect("임시 파일 쓰기 실패");
        file.flush().expect("임시 파일 플러시 실패");
        TempTestFile { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempTestFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[test]
fn test_empty_file() {
    let tmp = TempTestFile::new("empty", b"");
    let log_file = LogFile::open(tmp.path()).expect("LogFile::open 실패");

    assert_eq!(log_file.source().len(), 0);
    assert!(log_file.source().is_empty());
    assert_eq!(log_file.source().bytes(0, 10), &[]);

    let cancel = AtomicBool::new(false);
    let mut progress_called = false;
    log_file
        .build_index(&cancel, &mut |p| {
            progress_called = true;
            assert_eq!(p.indexed_bytes, 0);
            assert_eq!(p.line_count, 0);
            assert!(p.done);
        })
        .expect("build_index 실패");

    assert!(progress_called);
    assert_eq!(log_file.index().line_count(), 0);
    assert!(log_file.index().progress().done);

    let lines = log_file.read_lines(0, 10, None).expect("read_lines 실패");
    assert!(lines.is_empty());
    assert_eq!(log_file.index().line_start(log_file.source(), 0), None);
    assert_eq!(log_file.index().line_of_offset(log_file.source(), 0), None);
}

#[test]
fn test_last_line_without_newline() {
    // 1. 단일 줄 (개행 없음)
    let tmp1 = TempTestFile::new("no_nl_single", b"hello world");
    let log_file1 = LogFile::open(tmp1.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file1
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    assert_eq!(log_file1.index().line_count(), 1);
    let lines1 = log_file1.read_lines(0, 10, None).expect("읽기 실패");
    assert_eq!(lines1.len(), 1);
    assert_eq!(lines1[0].no, 0);
    assert_eq!(lines1[0].text, "hello world");
    assert_eq!(lines1[0].byte_len, 11);
    assert!(!lines1[0].truncated);

    // 2. 다중 줄 중 마지막 줄 개행 없음
    let tmp2 = TempTestFile::new("no_nl_multi", b"line1\nline2\nline3");
    let log_file2 = LogFile::open(tmp2.path()).expect("열기 실패");
    log_file2
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    assert_eq!(log_file2.index().line_count(), 3);
    let lines2 = log_file2.read_lines(0, 10, None).expect("읽기 실패");
    assert_eq!(lines2.len(), 3);
    assert_eq!(lines2[0].text, "line1");
    assert_eq!(lines2[1].text, "line2");
    assert_eq!(lines2[2].text, "line3");
    assert_eq!(lines2[2].byte_len, 5);
}

#[test]
fn test_crlf_handling() {
    let content = b"first line\r\nsecond line\r\nthird without nl\r";
    let tmp = TempTestFile::new("crlf", content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    // 3줄: first line, second line, third without nl\r
    assert_eq!(log_file.index().line_count(), 3);
    let lines = log_file.read_lines(0, 10, None).expect("읽기 실패");
    assert_eq!(lines.len(), 3);

    // 첫 줄: \r 제거된 텍스트, byte_len은 \r 포함 (11바이트)
    assert_eq!(lines[0].text, "first line");
    assert_eq!(lines[0].byte_len, 11);

    // 둘째 줄: \r 제거된 텍스트, byte_len 12
    assert_eq!(lines[1].text, "second line");
    assert_eq!(lines[1].byte_len, 12);

    // 셋째 줄: 파일 끝에 \r이 남아있는 경우 끝의 \r 하나 제거
    assert_eq!(lines[2].text, "third without nl");
    assert_eq!(lines[2].byte_len, 17);
}

#[test]
fn test_newlines_only() {
    let tmp = TempTestFile::new("newlines_only", b"\n\n\n");
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    assert_eq!(log_file.index().line_count(), 3);
    let lines = log_file.read_lines(0, 10, None).expect("읽기 실패");
    assert_eq!(lines.len(), 3);

    for (i, line) in lines.iter().enumerate() {
        assert_eq!(line.no, i as u64);
        assert_eq!(line.text, "");
        assert_eq!(line.byte_len, 0);
        assert!(!line.truncated);
    }
}

#[test]
fn test_long_line_over_16kb_and_korean_utf8_boundary() {
    let mut content = Vec::new();
    let korean_char = "가";
    for _ in 0..7000 {
        content.extend_from_slice(korean_char.as_bytes());
    }
    content.push(b'\n'); // 21001 바이트

    let tmp = TempTestFile::new("long_korean", &content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    assert_eq!(log_file.index().line_count(), 1);
    let lines = log_file.read_lines(0, 1, None).expect("읽기 실패");
    assert_eq!(lines.len(), 1);

    let line = &lines[0];
    assert!(line.truncated);
    assert_eq!(line.byte_len, 21000);
    assert!(line.text.len() <= MAX_DISPLAY_BYTES);
    assert_eq!(line.text.len(), 16383);
    assert!(!line.text.contains('\u{FFFD}'));
    assert!(line.text.chars().all(|c| c == '가'));
    assert_eq!(line.text.chars().count(), 5461);
}

#[test]
fn test_invalid_utf8_bytes() {
    let mut content = Vec::new();
    content.extend_from_slice(b"Valid prefix ");
    content.extend_from_slice(&[0xFF, 0xFE]);
    content.extend_from_slice(b" valid suffix\n");

    let tmp = TempTestFile::new("invalid_utf8", &content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    let lines = log_file.read_lines(0, 1, None).expect("읽기 실패");
    assert_eq!(lines.len(), 1);
    assert!(lines[0].text.contains('\u{FFFD}'));
    assert!(lines[0].text.starts_with("Valid prefix "));
    assert!(lines[0].text.ends_with(" valid suffix"));
}

#[test]
fn test_chunk_segment_boundary_straddle() {
    let mut content = Vec::new();
    content.extend_from_slice(b"line 0: 0123456789012345678901234567890123456789012\n");
    content.extend_from_slice(b"line 1: cross boundary 64 bytes right here!\n");
    content.extend_from_slice(b"line 2: cross boundary 128 bytes right here as well!\n");

    let tmp = TempTestFile::new("boundary_straddle", &content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    log_file.index().set_segment_size(64);

    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    assert_eq!(log_file.index().line_count(), 3);
    let lines = log_file.read_lines(0, 5, None).expect("읽기 실패");
    assert_eq!(lines.len(), 3);
    assert_eq!(
        lines[0].text,
        "line 0: 0123456789012345678901234567890123456789012"
    );
    assert_eq!(lines[1].text, "line 1: cross boundary 64 bytes right here!");
    assert_eq!(
        lines[2].text,
        "line 2: cross boundary 128 bytes right here as well!"
    );
}

#[test]
fn test_k_multiples_checkpoints() {
    let mut content = Vec::new();
    for i in 0..2500 {
        content.extend_from_slice(format!("log message line {:05}\n", i).as_bytes());
    }

    let tmp = TempTestFile::new("k_multiples", &content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    assert_eq!(log_file.index().line_count(), 2500);

    let test_indices = [0, 1023, 1024, 1025, 2047, 2048, 2049];
    for &idx in &test_indices {
        let lines = log_file.read_lines(idx, 1, None).expect("read_lines 실패");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].no, idx);
        let expected_text = format!("log message line {:05}", idx);
        assert_eq!(lines[0].text, expected_text);

        let start_offset = log_file
            .index()
            .line_start(log_file.source(), idx)
            .expect("line_start 실패");
        assert_eq!(
            log_file
                .index()
                .line_of_offset(log_file.source(), start_offset),
            Some(idx)
        );
    }
}

#[test]
fn test_read_lines_past_eof() {
    let content = b"line 0\nline 1\nline 2\nline 3\nline 4\n";
    let tmp = TempTestFile::new("past_eof", content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    assert_eq!(log_file.index().line_count(), 5);

    let lines = log_file.read_lines(3, 10, None).expect("읽기 실패");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].no, 3);
    assert_eq!(lines[0].text, "line 3");
    assert_eq!(lines[1].no, 4);
    assert_eq!(lines[1].text, "line 4");

    let lines_eof = log_file.read_lines(5, 5, None).expect("읽기 실패");
    assert!(lines_eof.is_empty());

    let lines_far = log_file.read_lines(100, 5, None).expect("읽기 실패");
    assert!(lines_far.is_empty());
}

#[test]
fn test_line_of_offset_roundtrip() {
    let mut content = Vec::new();
    for i in 0..200 {
        if i % 3 == 0 {
            content.extend_from_slice(format!("줄 번호 {:04} CRLF 테스트\r\n", i).as_bytes());
        } else if i % 3 == 1 {
            content.extend_from_slice(format!("line {:04} simple LF\n", i).as_bytes());
        } else {
            content.extend_from_slice(
                format!("line {:04} with extra spaces and symbols!@#$\n", i).as_bytes(),
            );
        }
    }

    let tmp = TempTestFile::new("roundtrip", &content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    let total_lines = log_file.index().line_count();
    assert_eq!(total_lines, 200);

    for line_no in 0..total_lines {
        let start = log_file
            .index()
            .line_start(log_file.source(), line_no)
            .expect("line_start 실패");

        assert_eq!(
            log_file.index().line_of_offset(log_file.source(), start),
            Some(line_no),
            "line_no {} 시작 오프셋 왕복 실패",
            line_no
        );

        let next_start = if line_no + 1 < total_lines {
            log_file
                .index()
                .line_start(log_file.source(), line_no + 1)
                .unwrap()
        } else {
            log_file.source().len()
        };

        if next_start > start + 1 {
            let mid = start + (next_start - start) / 2;
            assert_eq!(
                log_file.index().line_of_offset(log_file.source(), mid),
                Some(line_no),
                "line_no {} 중간 오프셋 확인 실패",
                line_no
            );
        }
    }

    assert_eq!(
        log_file
            .index()
            .line_of_offset(log_file.source(), log_file.source().len()),
        None
    );
}

#[test]
fn test_concurrent_read_during_indexing() {
    let mut content = Vec::new();
    for i in 0..50_000 {
        content.extend_from_slice(format!("concurrent line {:06}\n", i).as_bytes());
    }

    let tmp = TempTestFile::new("concurrent", &content);
    let log_file = Arc::new(LogFile::open(tmp.path()).expect("열기 실패"));
    log_file.index().set_segment_size(32 * 1024);

    let cancel = Arc::new(AtomicBool::new(false));
    let lf_clone = Arc::clone(&log_file);
    let cancel_clone = Arc::clone(&cancel);

    let index_handle = thread::spawn(move || lf_clone.build_index(&cancel_clone, &mut |_| {}));

    let mut read_success_count = 0;
    while !log_file.index().progress().done {
        if log_file.index().line_count() >= 50 {
            let lines = log_file.read_lines(0, 50, None).expect("읽기 실패");
            assert_eq!(lines.len(), 50);
            assert_eq!(lines[0].text, "concurrent line 000000");
            assert_eq!(lines[49].text, "concurrent line 000049");
            read_success_count += 1;
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    index_handle.join().unwrap().expect("인덱싱 실패");
    assert!(read_success_count > 0 || log_file.index().line_count() == 50_000);
    assert_eq!(log_file.index().line_count(), 50_000);
}

#[test]
fn test_cancel_behavior() {
    let mut content = Vec::new();
    for i in 0..10_000 {
        content.extend_from_slice(format!("cancel test line {:05}\n", i).as_bytes());
    }

    let tmp = TempTestFile::new("cancel", &content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    log_file.index().set_segment_size(1024);

    let cancel = AtomicBool::new(true);
    let mut progress_count = 0;
    let res = log_file.build_index(&cancel, &mut |_| {
        progress_count += 1;
    });

    assert!(res.is_ok());
    assert!(!log_file.index().progress().done);
    assert_eq!(progress_count, 0);
}

// =========================================================================
// Phase 3 검색 및 하이라이트 테스트
// =========================================================================

#[test]
fn test_utf16_ranges_hangul_and_emoji() {
    // 1. 한글 하이라이트: "가나다 abc 가나" 에서 "abc" -> UTF-16 (4, 7)
    let text = "가나다 abc 가나";
    // "abc"의 바이트 범위: "가나다 "는 3*3 + 1 = 10바이트. "abc"는 10..13
    let byte_ranges = vec![(10, 13)];
    let u16 = utf16_ranges(text, &byte_ranges);
    assert_eq!(u16, vec![(4, 7)]);

    // 2. 이모지(서로게이트 쌍: UTF-16 2단위) 하이라이트
    // "🚀 abc 🚀" 에서 "abc"
    // '🚀'는 4바이트, UTF-16 2코드유닛.
    // ' '는 1바이트, UTF-16 1코드유닛.
    // 'a'는 바이트 5에서 시작. "abc"는 5..8.
    let text2 = "🚀 abc 🚀";
    let byte_ranges2 = vec![(5, 8)];
    let u16_2 = utf16_ranges(text2, &byte_ranges2);
    assert_eq!(u16_2, vec![(3, 6)]);

    // 3. 다중 및 인접 매치
    let text3 = "가a나b";
    // "a" (byte 3..4), "b" (byte 7..8)
    let u16_3 = utf16_ranges(text3, &[(3, 4), (7, 8)]);
    assert_eq!(u16_3, vec![(1, 2), (3, 4)]);
}

#[test]
fn test_matcher_options_and_errors() {
    // 1. 빈 검색어 -> Err(InvalidQuery)
    let empty_res = Matcher::new("", SearchOptions::default());
    assert!(matches!(empty_res, Err(Error::InvalidQuery(_))));

    // 2. 잘못된 정규식 -> Err(InvalidQuery)
    let invalid_re = Matcher::new(
        "[a-",
        SearchOptions {
            regex: true,
            case_sensitive: true,
        },
    );
    assert!(matches!(invalid_re, Err(Error::InvalidQuery(_))));

    // 3. 대소문자 구분 리터럴 vs 대소문자 무시 리터럴
    let m_sensitive = Matcher::new(
        "abc",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let m_insensitive = Matcher::new(
        "abc",
        SearchOptions {
            regex: false,
            case_sensitive: false,
        },
    )
    .unwrap();

    let sample = b"ABC and abc and AbC";
    assert_eq!(m_sensitive.find_ranges(sample), vec![(8, 11)]);
    assert_eq!(
        m_insensitive.find_ranges(sample),
        vec![(0, 3), (8, 11), (16, 19)]
    );

    // 4. 정규식 find_ranges: 빈 매치 제외
    let m_re = Matcher::new(
        r"\d+",
        SearchOptions {
            regex: true,
            case_sensitive: true,
        },
    )
    .unwrap();
    assert_eq!(
        m_re.find_ranges(b"item 123 and 4567"),
        vec![(5, 8), (13, 17)]
    );
}

#[test]
fn test_search_chunk_and_buffer_boundary_straddle() {
    // 버퍼/세그먼트 경계(작은 64바이트)를 걸치는 매치 검증
    let mut content = Vec::new();
    content.extend_from_slice(b"line 0: 0123456789012345678901234567890123456789\n"); // 50바이트
    content.extend_from_slice(b"line 1: cross 64-byte segment TARGET right here!\n"); // 64바이트 경계 걸침
    content.extend_from_slice(b"line 2: normal line without keyword\n");
    content.extend_from_slice(b"line 3: another TARGET at the end\n");

    let tmp = TempTestFile::new("search_straddle", &content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    let matcher = Matcher::new(
        "TARGET",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut matched_lines = Vec::new();

    let outcome = search_with_config(
        &log_file,
        &matcher,
        &cancel,
        &mut |batch| matched_lines.extend_from_slice(batch),
        &mut |_| {},
        SearchConfig {
            segment_size: 64,
            max_results: 1000,
        },
    )
    .unwrap();

    assert_eq!(outcome, SearchOutcome::Completed { total: 2 });
    assert_eq!(matched_lines, vec![1, 3]);
}

#[test]
fn test_search_single_line_larger_than_buffer() {
    // 버퍼 크기(64바이트)보다 훨씬 큰 단일 줄(200바이트) 내부의 매치 검증
    let mut content = Vec::new();
    content.extend_from_slice(b"short line 0\n");

    // 200바이트 길이의 긴 줄 생성
    let mut long_line = vec![b'a'; 180];
    long_line.extend_from_slice(b"TARGET_INSIDE");
    long_line.extend_from_slice(b"aaaaaaa\n");
    content.extend_from_slice(&long_line);

    content.extend_from_slice(b"short line 2\n");
    content.extend_from_slice(b"short line 3 with TARGET_INSIDE\n");

    let tmp = TempTestFile::new("search_long_line", &content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    let matcher = Matcher::new(
        "TARGET_INSIDE",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut matched_lines = Vec::new();

    let outcome = search_with_config(
        &log_file,
        &matcher,
        &cancel,
        &mut |batch| matched_lines.extend_from_slice(batch),
        &mut |_| {},
        SearchConfig {
            segment_size: 64,
            max_results: 1000,
        },
    )
    .unwrap();

    assert_eq!(outcome, SearchOutcome::Completed { total: 2 });
    assert_eq!(matched_lines, vec![1, 3]);
}

#[test]
fn test_search_multiple_matches_one_line_and_order() {
    // 한 줄 다중 매치는 1회만 기록되며 결과는 전역 오름차순이어야 함
    let content =
        b"match match match line 0\nnothing here 1\nmatch line 2\nmatch and match line 3\n";
    let tmp = TempTestFile::new("multi_match", content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    let matcher = Matcher::new(
        "match",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut matched_lines = Vec::new();

    let outcome = search(
        &log_file,
        &matcher,
        &cancel,
        &mut |batch| matched_lines.extend_from_slice(batch),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome, SearchOutcome::Completed { total: 3 });
    assert_eq!(matched_lines, vec![0, 2, 3]);
}

#[test]
fn test_search_last_line_without_newline() {
    let content = b"line 0\nline 1 without newline keyword";
    let tmp = TempTestFile::new("search_no_nl", content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    let matcher = Matcher::new(
        "keyword",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut matched_lines = Vec::new();

    let outcome = search(
        &log_file,
        &matcher,
        &cancel,
        &mut |batch| matched_lines.extend_from_slice(batch),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome, SearchOutcome::Completed { total: 1 });
    assert_eq!(matched_lines, vec![1]);
}

#[test]
fn test_search_crlf_and_dollar_anchor() {
    // CRLF 파일에서 regex $ 앵커가 \r 앞에서 올바르게 매치되는지 검증
    let content = b"status=200\r\nstatus=500\r\nstatus=404\r\n";
    let tmp = TempTestFile::new("search_crlf_anchor", content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    // $ 앵커 사용
    let matcher = Matcher::new(
        r"status=5\d\d$",
        SearchOptions {
            regex: true,
            case_sensitive: true,
        },
    )
    .unwrap();

    let cancel = AtomicBool::new(false);
    let mut matched_lines = Vec::new();

    let outcome = search(
        &log_file,
        &matcher,
        &cancel,
        &mut |batch| matched_lines.extend_from_slice(batch),
        &mut |_| {},
    )
    .unwrap();

    assert_eq!(outcome, SearchOutcome::Completed { total: 1 });
    assert_eq!(matched_lines, vec![1]);
}

#[test]
fn test_search_capped_max_results() {
    let mut content = Vec::new();
    for i in 0..50 {
        content.extend_from_slice(format!("match line {:02}\n", i).as_bytes());
    }

    let tmp = TempTestFile::new("search_capped", &content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    let matcher = Matcher::new(
        "match",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut matched_lines = Vec::new();

    let outcome = search_with_config(
        &log_file,
        &matcher,
        &cancel,
        &mut |batch| matched_lines.extend_from_slice(batch),
        &mut |_| {},
        SearchConfig {
            segment_size: 1024,
            max_results: 5,
        },
    )
    .unwrap();

    assert_eq!(outcome, SearchOutcome::Capped { total: 5 });
    assert_eq!(matched_lines.len(), 5);
    assert_eq!(matched_lines, vec![0, 1, 2, 3, 4]);
}

#[test]
fn test_search_cancel() {
    let mut content = Vec::new();
    for i in 0..10_000 {
        content.extend_from_slice(format!("data line {:05}\n", i).as_bytes());
    }

    let tmp = TempTestFile::new("search_cancel", &content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    let matcher = Matcher::new(
        "data",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let cancel = AtomicBool::new(true); // 시작부터 취소
    let mut matched_lines = Vec::new();

    let outcome = search_with_config(
        &log_file,
        &matcher,
        &cancel,
        &mut |batch| matched_lines.extend_from_slice(batch),
        &mut |_| {},
        SearchConfig {
            segment_size: 512,
            max_results: 1000,
        },
    )
    .unwrap();

    assert_eq!(outcome, SearchOutcome::Cancelled);
    assert!(matched_lines.is_empty());
}

#[test]
fn test_search_grep_equivalence() {
    // 다양한 형태(한국어, CRLF, LF, 긴 줄 등)의 로그 300줄 생성
    let mut content = Vec::new();
    for i in 0..300 {
        if i % 7 == 0 {
            content.extend_from_slice(
                format!(
                    "로그 #{:03}: 외부 결제 모듈 정상 승인 transaction_id=tx-{:05}\r\n",
                    i,
                    i * 13
                )
                .as_bytes(),
            );
        } else if i % 7 == 1 {
            content.extend_from_slice(
                format!(
                    "log #{:03}: ERROR database connection timed out code=503\n",
                    i
                )
                .as_bytes(),
            );
        } else if i % 7 == 2 {
            content.extend_from_slice(
                format!("log #{:03}: [worker-1] error retry succeeded\r\n", i).as_bytes(),
            );
        } else {
            content.extend_from_slice(
                format!("log #{:03}: normal heartbeat info status=200\n", i).as_bytes(),
            );
        }
    }

    let tmp = TempTestFile::new("grep_equiv", &content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    // 1. 한국어 리터럴 "결제" 검색 검증
    {
        let query = "결제";
        let matcher = Matcher::new(
            query,
            SearchOptions {
                regex: false,
                case_sensitive: true,
            },
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut actual_matches = Vec::new();
        search(
            &log_file,
            &matcher,
            &cancel,
            &mut |batch| actual_matches.extend_from_slice(batch),
            &mut |_| {},
        )
        .unwrap();

        // 단순 참조 구현: 줄 단위로 분리하여 바이트 검색
        let text = std::fs::read(tmp.path()).unwrap();
        let mut expected_matches = Vec::new();
        let mut line_no = 0u64;
        for line in text.split(|&b| b == b'\n') {
            let line_bytes = if line.ends_with(b"\r") {
                &line[..line.len() - 1]
            } else {
                line
            };
            if line_bytes
                .windows(query.len())
                .any(|w| w == query.as_bytes())
            {
                expected_matches.push(line_no);
            }
            line_no += 1;
        }
        // 마지막 빈 줄 처리 보정
        if text.ends_with(b"\n") && expected_matches.last() == Some(&(line_no - 1)) {
            expected_matches.pop();
        }

        assert_eq!(actual_matches, expected_matches);
    }

    // 2. 대소문자 무시 리터럴 "error"
    {
        let matcher = Matcher::new(
            "error",
            SearchOptions {
                regex: false,
                case_sensitive: false,
            },
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut actual_matches = Vec::new();
        search(
            &log_file,
            &matcher,
            &cancel,
            &mut |batch| actual_matches.extend_from_slice(batch),
            &mut |_| {},
        )
        .unwrap();

        let text = std::fs::read_to_string(tmp.path()).unwrap();
        let expected_matches: Vec<u64> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.to_lowercase().contains("error"))
            .map(|(i, _)| i as u64)
            .collect();

        assert_eq!(actual_matches, expected_matches);
    }

    // 3. 정규식 `status=5\d\d`
    {
        let re_pattern = r"status=5\d\d";
        let matcher = Matcher::new(
            re_pattern,
            SearchOptions {
                regex: true,
                case_sensitive: true,
            },
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut actual_matches = Vec::new();
        search(
            &log_file,
            &matcher,
            &cancel,
            &mut |batch| actual_matches.extend_from_slice(batch),
            &mut |_| {},
        )
        .unwrap();

        let re = regex::bytes::Regex::new(re_pattern).unwrap();
        let text = std::fs::read(tmp.path()).unwrap();
        let expected_matches: Vec<u64> = text
            .split(|&b| b == b'\n')
            .take(300)
            .enumerate()
            .filter(|(_, line)| re.is_match(line))
            .map(|(i, _)| i as u64)
            .collect();

        assert_eq!(actual_matches, expected_matches);
    }
}

#[test]
fn test_read_lines_highlights() {
    let content = b"hello world\nsecond hello line\nno match here\n";
    let tmp = TempTestFile::new("highlights", content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("build_index 실패");

    let matcher = Matcher::new(
        "hello",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let lines = log_file.read_lines(0, 3, Some(&matcher)).unwrap();

    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].highlights, vec![(0, 5)]);
    assert_eq!(lines[1].highlights, vec![(7, 12)]);
    assert!(lines[2].highlights.is_empty());
}

#[test]
fn test_read_lines_at_arbitrary_and_filter() {
    let mut content = Vec::new();
    for i in 0..1500 {
        content.extend_from_slice(format!("line {:04} text message\n", i).as_bytes());
    }

    let tmp = TempTestFile::new("read_lines_at", &content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("build_index 실패");

    let matcher = Matcher::new(
        "0002",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();

    // 임의 오름차순 줄 번호: 연속(1,2), 인접(4), 체크포인트(1024) 넘음, 중복(1024, 1024), 범위 초과(9999)
    let requested = [1, 2, 4, 1024, 1024, 9999];
    let lines = log_file.read_lines_at(&requested, Some(&matcher)).unwrap();

    // 9999는 인덱스 범위를 벗어나므로 건너뜀 -> 총 5개 반환
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0].no, 1);
    assert_eq!(lines[0].text, "line 0001 text message");
    assert!(lines[0].highlights.is_empty());

    assert_eq!(lines[1].no, 2);
    assert_eq!(lines[1].text, "line 0002 text message");
    // "0002" 는 "line " 뒤에 있으므로 바이트/UTF-16 5..9
    assert_eq!(lines[1].highlights, vec![(5, 9)]);

    assert_eq!(lines[2].no, 4);
    assert_eq!(lines[2].text, "line 0004 text message");

    assert_eq!(lines[3].no, 1024);
    assert_eq!(lines[3].text, "line 1024 text message");

    assert_eq!(lines[4].no, 1024);
    assert_eq!(lines[4].text, "line 1024 text message");
}
