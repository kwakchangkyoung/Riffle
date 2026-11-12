use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::search::{
    Matcher, SearchConfig, SearchOptions, SearchOutcome, search, search_with_config, utf16_ranges,
};
use crate::{Error, LogFile, MAX_DISPLAY_BYTES, RefreshOutcome};

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
    assert!(log_file.source().read_range(0, 10).unwrap().is_empty());

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

// =========================================================================
// Phase 4 tail 모드 (refresh / 추가분 인덱싱) 테스트
// =========================================================================

/// 파일 끝에 바이트를 추가한다 (append 모드)
fn append_bytes(path: &Path, data: &[u8]) {
    let mut f = OpenOptions::new()
        .append(true)
        .open(path)
        .expect("append open 실패");
    f.write_all(data).expect("append 쓰기 실패");
    f.flush().expect("append flush 실패");
}

/// 인덱싱을 동기적으로 완료한다
fn build_index(log_file: &LogFile) {
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("build_index 실패");
}

#[test]
fn test_refresh_appended_lines() {
    let tmp = TempTestFile::new("refresh_append", b"line 0\nline 1\nline 2\n");
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    build_index(&log_file);
    assert_eq!(log_file.index().line_count(), 3);

    // 변경 없음 → Unchanged
    assert_eq!(log_file.refresh().unwrap(), RefreshOutcome::Unchanged);

    // append 후 refresh → Appended, 새 줄 내용이 정확해야 한다
    append_bytes(tmp.path(), b"line 3\nline 4\nline 5\n");
    let outcome = log_file.refresh().unwrap();
    match outcome {
        RefreshOutcome::Appended {
            old_line_count,
            new_line_count,
            new_len,
        } => {
            assert_eq!(old_line_count, 3);
            assert_eq!(new_line_count, 6);
            assert_eq!(new_len, log_file.source().len());
        }
        other => panic!("Appended 기대, 실제 {:?}", other),
    }
    assert_eq!(log_file.index().line_count(), 6);

    let lines = log_file.read_lines(3, 3, None).unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].text, "line 3");
    assert_eq!(lines[1].text, "line 4");
    assert_eq!(lines[2].text, "line 5");

    // 두 번째 append도 이어서 정확해야 한다 (extend 반복)
    append_bytes(tmp.path(), b"line 6\n");
    let outcome2 = log_file.refresh().unwrap();
    assert!(matches!(
        outcome2,
        RefreshOutcome::Appended {
            old_line_count: 6,
            new_line_count: 7,
            ..
        }
    ));
    let last = log_file.read_lines(6, 1, None).unwrap();
    assert_eq!(last[0].text, "line 6");
}

#[test]
fn test_refresh_continues_partial_last_line() {
    // 개행 없이 끝난 파일: "a\nb" → 2줄("a", "b")
    let tmp = TempTestFile::new("refresh_partial", b"a\nb");
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    build_index(&log_file);
    assert_eq!(log_file.index().line_count(), 2);

    // "c\nd\n" 추가 → "a\nbc\nd\n" → 3줄("a", "bc", "d")
    append_bytes(tmp.path(), b"c\nd\n");
    let outcome = log_file.refresh().unwrap();
    assert!(matches!(
        outcome,
        RefreshOutcome::Appended {
            old_line_count: 2,
            new_line_count: 3,
            ..
        }
    ));

    let lines = log_file.read_lines(0, 10, None).unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].text, "a");
    assert_eq!(lines[1].text, "bc");
    assert_eq!(lines[2].text, "d");
}

#[test]
fn test_refresh_checkpoint_boundary_and_bulk_append() {
    // K(1024) 배수 경계를 넘는 append 검증
    // 초기: 1023개 완결 줄 + 개행 없는 마지막 조각 → line_count = 1024
    let mut content = String::new();
    for i in 0..1023 {
        content.push_str(&format!("base line {:04}\n", i));
    }
    content.push_str("partial-tail");
    let tmp = TempTestFile::new("refresh_k_boundary", content.as_bytes());
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    build_index(&log_file);
    assert_eq!(log_file.index().line_count(), 1024);

    // 개행으로 partial-tail을 닫고 새 줄 추가 → 1024번 줄("rest-line")이 K 배수 체크포인트 대상
    append_bytes(tmp.path(), b"\nrest-line\n");
    let outcome = log_file.refresh().unwrap();
    assert!(matches!(
        outcome,
        RefreshOutcome::Appended {
            old_line_count: 1024,
            new_line_count: 1025,
            ..
        }
    ));

    // 1024번 줄은 체크포인트에서 바로 시작 오프셋을 얻어야 한다
    let line1024 = log_file.read_lines(1024, 1, None).unwrap();
    assert_eq!(line1024.len(), 1);
    assert_eq!(line1024[0].no, 1024);
    assert_eq!(line1024[0].text, "rest-line");

    // 수천 줄 + 4MB 청크 경계를 넘는 분량을 한꺼번에 추가
    // (줄당 약 900바이트 × 5000줄 ≈ 4.5MB)
    let mut bulk = String::new();
    for i in 0..5000u64 {
        bulk.push_str(&format!(
            "bulk line {:05} {}\n",
            i,
            "x".repeat(880 + (i % 50) as usize)
        ));
    }
    append_bytes(tmp.path(), bulk.as_bytes());
    let outcome2 = log_file.refresh().unwrap();
    assert!(matches!(outcome2, RefreshOutcome::Appended { .. }));
    assert_eq!(log_file.index().line_count(), 1025 + 5000);

    // 임의 줄들을 순진한 기대값과 비교
    let total = log_file.index().line_count();
    for &idx in &[0u64, 1, 1022, 1023, 1024, 1025, 2000, 3000, 6024] {
        if idx >= total {
            continue;
        }
        let lines = log_file.read_lines(idx, 1, None).unwrap();
        assert_eq!(lines.len(), 1, "줄 {} 읽기 실패", idx);
        assert_eq!(lines[0].no, idx);
        let expected = if idx < 1023 {
            format!("base line {:04}", idx)
        } else if idx == 1023 {
            "partial-tail".to_string()
        } else if idx == 1024 {
            "rest-line".to_string()
        } else {
            let b = idx - 1025;
            format!("bulk line {:05} {}", b, "x".repeat(880 + (b % 50) as usize))
        };
        assert_eq!(lines[0].text, expected, "줄 {} 내용 불일치", idx);
    }
}

#[test]
fn test_refresh_pending_while_indexing() {
    let tmp = TempTestFile::new("refresh_pending", b"initial\n");
    let log_file = LogFile::open(tmp.path()).expect("open 실패");

    // 인덱싱 미완료(done=false) 상태에서는 변경 여부와 무관하게 Pending
    assert_eq!(log_file.refresh().unwrap(), RefreshOutcome::Pending);

    // 파일이 커진 상태에서도 인덱싱 미완료면 Pending
    append_bytes(tmp.path(), b"appended\n");
    assert_eq!(log_file.refresh().unwrap(), RefreshOutcome::Pending);

    // 인덱싱 완료 후에는 추가분이 Appended로 반영된다
    // (빌드는 open 시점 길이만 인덱싱하므로 append분은 여전히 미반영)
    build_index(&log_file);
    let outcome = log_file.refresh().unwrap();
    assert!(matches!(outcome, RefreshOutcome::Appended { .. }));
    assert_eq!(log_file.index().line_count(), 2);
    assert_eq!(log_file.refresh().unwrap(), RefreshOutcome::Unchanged);
}

#[test]
fn test_refresh_truncated_and_safe_reads() {
    let tmp = TempTestFile::new("refresh_trunc", b"line 0\nline 1\nline 2\nline 3\n");
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    build_index(&log_file);
    assert_eq!(log_file.index().line_count(), 4);

    // 외부에서 truncate (set_len(0))
    let f = OpenOptions::new()
        .write(true)
        .open(tmp.path())
        .expect("쓰기 핸들 열기 실패");
    f.set_len(0).expect("set_len 실패");
    drop(f);

    assert_eq!(log_file.refresh().unwrap(), RefreshOutcome::Truncated);

    // truncate 이후 모든 읽기 API가 패닉 없이 반환해야 한다
    let lines = log_file.read_lines(0, 10, None).unwrap();
    assert_eq!(lines.len(), 4); // 인덱스는 이전 줄 수를 유지
    assert!(lines.iter().all(|l| l.text.is_empty()));

    let at_lines = log_file.read_lines_at(&[0, 1, 2, 3], None).unwrap();
    assert_eq!(at_lines.len(), 4);

    // 인덱스 조회 함수들도 패닉 없어야 한다
    let _ = log_file.index().line_start(log_file.source(), 0);
    let _ = log_file.index().line_of_offset(log_file.source(), 0);
}

#[test]
fn test_refresh_replaced_and_deleted() {
    let tmp = TempTestFile::new("refresh_rotate", b"old content\n");
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    build_index(&log_file);

    // rename 로테이션: path를 path.1로 옮기고 같은 경로에 새 파일 생성 → Replaced
    let rotated = PathBuf::from(format!("{}.1", tmp.path().display()));
    std::fs::rename(tmp.path(), &rotated).expect("rename 실패");
    let mut nf = File::create(tmp.path()).expect("새 파일 생성 실패");
    nf.write_all(b"new file content\n").expect("쓰기 실패");
    drop(nf);

    assert_eq!(log_file.refresh().unwrap(), RefreshOutcome::Replaced);

    // 경로의 파일이 사라지면 → Replaced
    std::fs::remove_file(tmp.path()).expect("삭제 실패");
    assert_eq!(log_file.refresh().unwrap(), RefreshOutcome::Replaced);

    let _ = std::fs::remove_file(&rotated);
}

#[test]
fn test_refresh_on_initially_empty_file() {
    let tmp = TempTestFile::new("refresh_empty", b"");
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    build_index(&log_file);
    assert_eq!(log_file.index().line_count(), 0);

    // 빈 파일에 append → Appended (체크포인트 정렬 검증)
    append_bytes(tmp.path(), b"first\nsecond\n");
    let outcome = log_file.refresh().unwrap();
    assert!(matches!(
        outcome,
        RefreshOutcome::Appended {
            old_line_count: 0,
            new_line_count: 2,
            ..
        }
    ));

    let lines = log_file.read_lines(0, 10, None).unwrap();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].text, "first");
    assert_eq!(lines[1].text, "second");
}

// =========================================================================
// Phase 5 원문 범위/전체 줄 읽기 (read_text_range / read_full_line) 테스트
// =========================================================================

/// 내용을 쓴 임시 파일을 열고 인덱싱까지 완료한다.
/// 반환된 TempTestFile이 살아 있는 동안 파일이 유지된다.
fn indexed_file(prefix: &str, content: &[u8]) -> (TempTestFile, LogFile) {
    let tmp = TempTestFile::new(prefix, content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    build_index(&log_file);
    (tmp, log_file)
}

/// 순진한 참조 구현: 파일 바이트를 '\n'으로 나누고 각 줄 끝 '\r'을 제거한 뒤 '\n'으로 join.
/// read_text_range의 기대값 계산용 (전체 파일 기준 줄 목록 반환).
fn naive_lines(content: &[u8]) -> Vec<String> {
    let mut parts: Vec<&[u8]> = content.split(|&b| b == b'\n').collect();
    // 파일이 '\n'으로 끝나면 split이 만든 마지막 빈 조각은 줄이 아니다
    if content.last() == Some(&b'\n') {
        parts.pop();
    }
    parts
        .iter()
        .map(|l| {
            let l = if l.ends_with(b"\r") {
                &l[..l.len() - 1]
            } else {
                *l
            };
            String::from_utf8_lossy(l).into_owned()
        })
        .collect()
}

fn naive_join(lines: &[String]) -> String {
    lines.join("\n")
}

#[test]
fn test_read_text_range_crlf_and_no_trailing_newline() {
    let content = b"alpha\r\nbeta\r\ngamma\nlast-no-newline";
    let (_tmp, f) = indexed_file("range_crlf", content);
    assert_eq!(f.index().line_count(), 4);

    // 전체 범위: '\r' 제거, 마지막 개행 없는 줄 포함, 끝에 '\n' 없음
    let s = f.read_text_range(0, 3, 1_000_000).unwrap();
    assert_eq!(s, "alpha\nbeta\ngamma\nlast-no-newline");

    // 부분 범위
    let s = f.read_text_range(1, 2, 1_000_000).unwrap();
    assert_eq!(s, "beta\ngamma");

    // 단일 줄
    let s = f.read_text_range(3, 3, 1_000_000).unwrap();
    assert_eq!(s, "last-no-newline");
}

#[test]
fn test_read_text_range_end_clamp_and_inverted() {
    let content = b"l0\nl1\nl2\nl3\n";
    let (_tmp, f) = indexed_file("range_clamp", content);
    assert_eq!(f.index().line_count(), 4);

    // end가 확인된 줄 수를 넘어도 마지막 줄로 clamp
    let s = f.read_text_range(1, 999, 1_000_000).unwrap();
    assert_eq!(s, "l1\nl2\nl3");

    // start > end → 빈 문자열
    assert_eq!(f.read_text_range(2, 1, 1_000_000).unwrap(), "");
    // 범위 밖 start → 빈 문자열
    assert_eq!(f.read_text_range(4, 9, 1_000_000).unwrap(), "");
    assert_eq!(f.read_text_range(100, 200, 1_000_000).unwrap(), "");
}

#[test]
fn test_read_text_range_too_large() {
    let content = b"aaaa\nbbbb\n";
    let (_tmp, f) = indexed_file("range_toolarge", content);

    // "aaaa\nbbbb" = 9바이트
    let s = f.read_text_range(0, 1, 9).unwrap();
    assert_eq!(s, "aaaa\nbbbb");

    // 8바이트로는 부족 → TooLarge
    match f.read_text_range(0, 1, 8) {
        Err(Error::TooLarge { limit }) => assert_eq!(limit, 8),
        other => panic!("TooLarge 기대, 실제 {:?}", other.map(|s| s.len())),
    }

    // 단일 줄도 예산 초과 시 Err
    match f.read_text_range(1, 1, 3) {
        Err(Error::TooLarge { limit }) => assert_eq!(limit, 3),
        other => panic!("TooLarge 기대, 실제 {:?}", other),
    }
}

#[test]
fn test_read_text_range_long_line_over_16kb_preserved() {
    // 16KB를 넘는 단일 줄도 잘리지 않고 원문 그대로 반환되어야 한다
    let mut content = Vec::new();
    content.extend_from_slice(&vec![b'x'; 20 * 1024]);
    content.extend_from_slice(b"\nshort\n");
    let (_tmp, f) = indexed_file("range_long", &content);
    assert_eq!(f.index().line_count(), 2);

    let s = f.read_text_range(0, 1, 100_000).unwrap();
    assert_eq!(s.len(), 20 * 1024 + 1 + 5);
    assert!(s.starts_with(&"x".repeat(100)));
    assert!(s.ends_with("\nshort"));
}

#[test]
fn test_read_text_range_line_spanning_chunk_boundary() {
    // 1MB 청크 경계를 걸치는 1.5MB짜리 줄 (pending 경로 검증)
    let mut content = Vec::new();
    content.extend_from_slice(b"before\n");
    content.extend_from_slice(&vec![b'y'; 1536 * 1024]);
    content.extend_from_slice(b"\nafter\n");
    let (_tmp, f) = indexed_file("range_straddle", &content);
    assert_eq!(f.index().line_count(), 3);

    let s = f.read_text_range(0, 2, 10_000_000).unwrap();
    assert_eq!(s.len(), 6 + 1 + 1536 * 1024 + 1 + 5);
    assert!(s.starts_with("before\n"));
    assert!(s.ends_with("\nafter"));

    // 긴 줄 단독 범위
    let s = f.read_text_range(1, 1, 10_000_000).unwrap();
    assert_eq!(s.len(), 1536 * 1024);
    assert!(s.bytes().all(|b| b == b'y'));
}

#[test]
fn test_read_full_line_basic_and_missing() {
    let content = b"first\r\nsecond\nthird-no-newline";
    let (_tmp, f) = indexed_file("fullline_basic", content);
    assert_eq!(f.index().line_count(), 3);

    // CRLF 줄: '\r' 제거된 원문
    let (text, truncated) = f.read_full_line(0, 1024).unwrap().unwrap();
    assert_eq!(text, "first");
    assert!(!truncated);

    let (text, truncated) = f.read_full_line(1, 1024).unwrap().unwrap();
    assert_eq!(text, "second");
    assert!(!truncated);

    // 개행 없는 마지막 줄
    let (text, truncated) = f.read_full_line(2, 1024).unwrap().unwrap();
    assert_eq!(text, "third-no-newline");
    assert!(!truncated);

    // 없는 줄 → None
    assert!(f.read_full_line(3, 1024).unwrap().is_none());
    assert!(f.read_full_line(999, 1024).unwrap().is_none());
}

#[test]
fn test_read_full_line_truncation_utf8_boundary() {
    // 16KB 초과 줄: max_bytes 이내면 원문 전체, 초과면 UTF-8 경계에서 잘림
    let mut content = Vec::new();
    content.extend_from_slice(&vec![b'z'; 20 * 1024]);
    content.extend_from_slice(b"\n");
    let (_tmp, f) = indexed_file("fullline_trunc", &content);

    // 충분히 큰 max_bytes → 잘림 없음
    let (text, truncated) = f.read_full_line(0, 1024 * 1024).unwrap().unwrap();
    assert_eq!(text.len(), 20 * 1024);
    assert!(!truncated);

    // 작은 max_bytes → truncated, 경계까지만
    let (text, truncated) = f.read_full_line(0, 1000).unwrap().unwrap();
    assert!(truncated);
    assert_eq!(text.len(), 1000);
}

#[test]
fn test_read_full_line_korean_utf8_boundary() {
    // '가'는 3바이트. max_bytes=10이면 9바이트(3자)까지만 잘려야 한다
    let mut content = Vec::new();
    for _ in 0..100 {
        content.extend_from_slice("가".as_bytes());
    }
    content.push(b'\n');
    let (_tmp, f) = indexed_file("fullline_korean", &content);

    let (text, truncated) = f.read_full_line(0, 10).unwrap().unwrap();
    assert!(truncated);
    assert_eq!(text, "가가가"); // 10바이트 경계의 미완성 문자는 제외
    assert!(!text.contains('\u{FFFD}'));
}

#[test]
fn test_read_text_range_100k_lines_matches_naive() {
    // 10만 줄 규모: CRLF, 한글, 16KB 초과 줄, 개행 없는 마지막 줄이 섞인 파일
    let mut content = Vec::new();
    for i in 0..100_000u64 {
        if i == 50_000 {
            // 16KB 초과 줄
            content.extend_from_slice(&vec![b'L'; 20 * 1024]);
            content.push(b'\n');
        } else if i % 977 == 0 {
            content.extend_from_slice(format!("로그 줄 {:06} 한글 메시지\r\n", i).as_bytes());
        } else if i % 13 == 0 {
            content.extend_from_slice(format!("line {:06} plain ascii\n", i).as_bytes());
        } else {
            content.extend_from_slice(
                format!(
                    "2026-11-12T07:{:02}:{:02}.{:03}Z INFO [w-{}] msg={}\n",
                    i % 60,
                    i % 60,
                    i % 1000,
                    i % 8,
                    i * 7
                )
                .as_bytes(),
            );
        }
    }
    // 개행 없이 끝나는 마지막 줄
    content.extend_from_slice(b"final line no newline");

    let (_tmp, f) = indexed_file("range_100k", &content);
    let total = f.index().line_count();
    assert_eq!(total, 100_001);

    let expected_parts = naive_lines(&content);
    assert_eq!(expected_parts.len(), total as usize);

    // 전체 범위
    let t0 = std::time::Instant::now();
    let s = f
        .read_text_range(0, total - 1, u64::MAX / 2)
        .expect("read_text_range 실패");
    let elapsed = t0.elapsed();
    eprintln!(
        "read_text_range 10만 줄: {:.2}ms, {} bytes",
        elapsed.as_secs_f64() * 1000.0,
        s.len()
    );
    assert_eq!(s, naive_join(&expected_parts));

    // 임의 부분 범위도 순진한 구현과 일치해야 한다
    for &(start, end) in &[
        (1u64, 500u64),
        (12345, 23456),
        (99999, 100_000),
        (77777, 77777),
    ] {
        let s = f.read_text_range(start, end, u64::MAX / 2).unwrap();
        let expected = naive_join(&expected_parts[start as usize..=end as usize]);
        assert_eq!(s, expected, "범위 ({}, {}) 불일치", start, end);
    }
}

// ===== Phase 7 A2/A3/A5 회귀 테스트 =====

/// 검색 헬퍼: 패턴으로 search를 실행해 (outcome, 매치 줄 번호) 반환
fn run_search(content: &[u8], pattern: &str, regex: bool) -> (SearchOutcome, Vec<u64>) {
    let tmp = TempTestFile::new("p7_search", content);
    let log_file = LogFile::open(tmp.path()).expect("open 실패");
    let matcher = Matcher::new(
        pattern,
        SearchOptions {
            regex,
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
    (outcome, matched_lines)
}

#[test]
fn test_search_regex_cross_line_match_rejected() {
    // A2: `foo\sbar`는 "foo\nbar"의 개행을 넘어 매치할 수 있지만
    // 어느 한 줄의 텍스트 안에 매치가 없으므로 기록되지 않아야 한다.
    let (outcome, lines) = run_search(b"foo\nbar\nbaz foo bar\n", r"foo\sbar", true);
    assert_eq!(outcome, SearchOutcome::Completed { total: 1 });
    assert_eq!(lines, vec![2]);
}

#[test]
fn test_search_regex_empty_match_pattern() {
    // A3: `z*`는 모든 위치에서 빈 매치를 반환하지만
    // 비어있지 않은 매치("z" 포함 줄)만 기록해야 한다.
    let (outcome, lines) = run_search(b"abc\nazb\nzzz\nfoo\n", "z*", true);
    assert_eq!(outcome, SearchOutcome::Completed { total: 2 });
    assert_eq!(lines, vec![1, 2]);

    // `(foo)?` 도 동일: "foo"가 있는 줄만 매치
    let (outcome, lines) = run_search(b"xxfoo\nbar\nfoofoo\n", "(foo)?", true);
    assert_eq!(outcome, SearchOutcome::Completed { total: 2 });
    assert_eq!(lines, vec![0, 2]);
}

#[test]
fn test_search_regex_nullable_filter_soundness() {
    // 첫 바이트 필터를 사용하는 널러블 패턴들에서 결과가 정확해야 한다
    for (pattern, content, expected) in [
        ("a?b", b"xb\nab\nac\n".as_slice(), vec![0u64, 1]),
        (
            "colou?r",
            b"color\ncolour\ncolr\n".as_slice(),
            vec![0u64, 1],
        ),
        ("z*", b"no match\nhas z\n".as_slice(), vec![1u64]),
        ("(?:ab)?c", b"c\nabc\nx\n".as_slice(), vec![0u64, 1]),
        (
            "ERROR|WARN?",
            b"WARN\nERROR\ninfo\n".as_slice(),
            vec![0u64, 1],
        ),
    ] {
        let (outcome, lines) = run_search(content, pattern, true);
        assert_eq!(
            outcome,
            SearchOutcome::Completed {
                total: expected.len() as u64
            },
            "pattern {}",
            pattern
        );
        assert_eq!(lines, expected, "pattern {}", pattern);
    }
}

#[test]
fn test_line_crlf_boundary_not_truncated() {
    // A5: 정확히 16KB+1바이트이고 끝이 '\r'인 줄은 \r 제외 시 16KB에 들어가므로
    // truncated=false 여야 한다.
    let mut content = Vec::new();
    let mut line = vec![b'x'; MAX_DISPLAY_BYTES]; // 16384바이트
    line.push(b'\r'); // +1 = 16385, 끝이 \r
    content.extend_from_slice(&line);
    content.push(b'\n');
    content.extend_from_slice(b"next\n");

    let tmp = TempTestFile::new("cr_boundary", &content);
    let f = LogFile::open(tmp.path()).unwrap();
    let cancel = AtomicBool::new(false);
    f.build_index(&cancel, &mut |_| {}).unwrap();

    let lines = f.read_lines(0, 1, None).unwrap();
    assert_eq!(lines.len(), 1);
    assert!(!lines[0].truncated, "16KB+\\r 줄이 잘림으로 표시됨");
    assert_eq!(lines[0].byte_len, (MAX_DISPLAY_BYTES + 1) as u64);
}

#[test]
fn test_line_start_cache_consistency() {
    // A4: 캐시 사용 후에도 read_lines 결과가 동일해야 한다
    let mut content = Vec::new();
    content.extend_from_slice(&vec![b'L'; 300 * 1024]); // 긴 줄 1
    content.push(b'\n');
    for i in 0..100 {
        content.extend_from_slice(format!("normal line {}\n", i).as_bytes());
    }
    let tmp = TempTestFile::new("line_cache", &content);
    let f = LogFile::open(tmp.path()).unwrap();
    let cancel = AtomicBool::new(false);
    f.build_index(&cancel, &mut |_| {}).unwrap();

    let first = f.read_lines(1, 10, None).unwrap();
    let second = f.read_lines(1, 10, None).unwrap();
    assert_eq!(first, second);
    assert_eq!(first[0].text, "normal line 0");
}
