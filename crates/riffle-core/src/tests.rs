use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::{LogFile, MAX_DISPLAY_BYTES};

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

    let lines = log_file.read_lines(0, 10).expect("read_lines 실패");
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
    let lines1 = log_file1.read_lines(0, 10).expect("읽기 실패");
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
    let lines2 = log_file2.read_lines(0, 10).expect("읽기 실패");
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
    let lines = log_file.read_lines(0, 10).expect("읽기 실패");
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
    let lines = log_file.read_lines(0, 10).expect("읽기 실패");
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
    // 한글 "가"는 UTF-8 3바이트: 0xEA, 0xB0, 0x80
    // 16KB 경계(16384바이트) 근처에서 한글 글자 경계가 잘리는지 확인
    // 5461개의 "가" = 16383바이트 (3바이트 * 5461)
    // 5462번째 "가"는 16383..16386 에 위치하게 됨.
    // 16384바이트 슬라이스를 취하면 5462번째 "가"의 첫 1바이트(0xEA)만 포함되므로,
    // 올바른 UTF-8 경계 후퇴 로직은 16383바이트(5461글자)까지만 취해야 함!
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
    let lines = log_file.read_lines(0, 1).expect("읽기 실패");
    assert_eq!(lines.len(), 1);

    let line = &lines[0];
    assert!(line.truncated);
    assert_eq!(line.byte_len, 21000);
    // 표시 텍스트는 16KB(16384) 이하
    assert!(line.text.len() <= MAX_DISPLAY_BYTES);
    // 16383바이트에서 안전하게 잘렸는지 확인
    assert_eq!(line.text.len(), 16383);
    // 깨진 문자(U+FFFD)가 없어야 함
    assert!(!line.text.contains('\u{FFFD}'));
    // 모든 문자가 '가'여야 함
    assert!(line.text.chars().all(|c| c == '가'));
    assert_eq!(line.text.chars().count(), 5461);
}

#[test]
fn test_invalid_utf8_bytes() {
    // 잘못된 UTF-8 바이트 시퀀스가 들어있는 경우 U+FFFD로 치환되는지 검증
    let mut content = Vec::new();
    content.extend_from_slice(b"Valid prefix ");
    content.extend_from_slice(&[0xFF, 0xFE]); // 유효하지 않은 바이트
    content.extend_from_slice(b" valid suffix\n");

    let tmp = TempTestFile::new("invalid_utf8", &content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    let lines = log_file.read_lines(0, 1).expect("읽기 실패");
    assert_eq!(lines.len(), 1);
    assert!(lines[0].text.contains('\u{FFFD}'));
    assert!(lines[0].text.starts_with("Valid prefix "));
    assert!(lines[0].text.ends_with(" valid suffix"));
}

#[test]
fn test_chunk_segment_boundary_straddle() {
    // 테스트용으로 구간 크기를 64바이트로 작게 줄여서 경계를 걸치는 줄 검증
    let mut content = Vec::new();
    // 0~49 (50바이트 + 개행 = 51바이트)
    content.extend_from_slice(b"line 0: 0123456789012345678901234567890123456789012\n");
    // 51~99 (48바이트 + 개행 = 49바이트) -> 64바이트 경계를 걸침!
    content.extend_from_slice(b"line 1: cross boundary 64 bytes right here!\n");
    // 100~159 (59바이트 + 개행 = 60바이트) -> 128바이트 경계를 걸침!
    content.extend_from_slice(b"line 2: cross boundary 128 bytes right here as well!\n");

    let tmp = TempTestFile::new("boundary_straddle", &content);
    let log_file = LogFile::open(tmp.path()).expect("열기 실패");

    // 구간 크기 64바이트로 설정
    log_file.index().set_segment_size(64);

    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("인덱싱 실패");

    assert_eq!(log_file.index().line_count(), 3);
    let lines = log_file.read_lines(0, 5).expect("읽기 실패");
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
    // K = 1024
    // 1023, 1024, 1025, 2048 번째 줄 검증을 위해 2500줄 생성
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
        let lines = log_file.read_lines(idx, 1).expect("read_lines 실패");
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

    // 3번부터 10개 요청 -> 3, 4번 2개만 반환되어야 함
    let lines = log_file.read_lines(3, 10).expect("읽기 실패");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0].no, 3);
    assert_eq!(lines[0].text, "line 3");
    assert_eq!(lines[1].no, 4);
    assert_eq!(lines[1].text, "line 4");

    // 파일 끝 이후 요청 -> 빈 Vec 반환
    let lines_eof = log_file.read_lines(5, 5).expect("읽기 실패");
    assert!(lines_eof.is_empty());

    let lines_far = log_file.read_lines(100, 5).expect("읽기 실패");
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

        // 시작 오프셋의 줄 번호 확인
        assert_eq!(
            log_file.index().line_of_offset(log_file.source(), start),
            Some(line_no),
            "line_no {} 시작 오프셋 왕복 실패",
            line_no
        );

        // 줄 내부 중간 오프셋 확인
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

    // 파일 길이를 넘는 오프셋은 None
    assert_eq!(
        log_file
            .index()
            .line_of_offset(log_file.source(), log_file.source().len()),
        None
    );
}

#[test]
fn test_concurrent_read_during_indexing() {
    // 50,000줄 파일 생성, 구간 크기 32KB로 설정하여 인덱싱 중에 다중 스레드에서 즉시 읽기 검증
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

    // 인덱싱 중에 다른 스레드에서 앞부분 읽기
    let mut read_success_count = 0;
    while !log_file.index().progress().done {
        if log_file.index().line_count() >= 50 {
            let lines = log_file.read_lines(0, 50).expect("읽기 실패");
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
    log_file.index().set_segment_size(1024); // 작은 세그먼트 크기

    let cancel = AtomicBool::new(true); // 처음부터 취소 설정
    let mut progress_count = 0;
    let res = log_file.build_index(&cancel, &mut |_| {
        progress_count += 1;
    });

    assert!(res.is_ok());
    assert!(!log_file.index().progress().done);
    assert_eq!(progress_count, 0);
}
