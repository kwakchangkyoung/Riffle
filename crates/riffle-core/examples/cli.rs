use std::env;
use std::io::Write;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use riffle_core::{LogFile, Matcher, RefreshOutcome, SearchOptions};

fn print_usage() {
    eprintln!("Riffle CLI - 파일 인덱싱 및 읽기/검색 검증 도구\n");
    eprintln!("사용법:");
    eprintln!("    cli <파일경로> stats");
    eprintln!("    cli <파일경로> lines <start> <count>");
    eprintln!("    cli <파일경로> open");
    eprintln!("    cli <파일경로> search <query> [--regex] [--ignore-case]");
    eprintln!("    cli <파일경로> tail\n");
    eprintln!("서브커맨드:");
    eprintln!("    stats                인덱싱 실행 후 파일 크기, 줄 수, 소요 시간 출력");
    eprintln!("    lines <start> <cnt>  지정 범위 줄 텍스트를 원문 그대로 출력 (0-based)");
    eprintln!("    open                 열기 후 첫 20줄 읽기까지의 소요 시간 및 내용 출력");
    eprintln!("    search <query> [opt] 파일 전체 검색 (매치 수, 첫 매치/전체 시간, 첫 5줄 출력)");
    eprintln!(
        "    tail                 인덱싱 후 200ms마다 refresh하여 추가분/로테이션 출력 (Ctrl+C까지)"
    );
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 3 {
        print_usage();
        process::exit(1);
    }

    let file_path = Path::new(&args[1]);
    let command = args[2].as_str();

    match command {
        "stats" => run_stats(file_path),
        "lines" => {
            if args.len() < 5 {
                eprintln!("오류: start와 count 인자가 필요합니다.");
                print_usage();
                process::exit(1);
            }
            let start: u64 = args[3].parse().unwrap_or_else(|_| {
                eprintln!("오류: start는 0 이상의 정수여야 합니다: {}", args[3]);
                process::exit(1);
            });
            let count: u64 = args[4].parse().unwrap_or_else(|_| {
                eprintln!("오류: count는 0 이상의 정수여야 합니다: {}", args[4]);
                process::exit(1);
            });
            run_lines(file_path, start, count);
        }
        "open" => run_open(file_path),
        "search" => {
            if args.len() < 4 {
                eprintln!("오류: search 커맨드에는 query 인자가 필요합니다.");
                print_usage();
                process::exit(1);
            }
            let query = &args[3];
            let mut regex = false;
            let mut case_sensitive = true;

            for arg in &args[4..] {
                match arg.as_str() {
                    "--regex" => regex = true,
                    "--ignore-case" | "-i" => case_sensitive = false,
                    other => {
                        eprintln!("경고: 알 수 없는 옵션 '{}'", other);
                    }
                }
            }

            let options = SearchOptions {
                regex,
                case_sensitive,
            };
            run_search(file_path, query, options);
        }
        "tail" => run_tail(file_path),
        _ => {
            eprintln!("오류: 알 수 없는 서브커맨드 '{}'", command);
            print_usage();
            process::exit(1);
        }
    }
}

/// stats 서브커맨드: 인덱싱 실행 후 줄 수, 파일 크기, 인덱싱 시간, 첫 줄 읽기까지 걸린 시간 출력
fn run_stats(path: &Path) {
    let log_file = Arc::new(LogFile::open(path).unwrap_or_else(|e| {
        eprintln!("파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    }));

    let t0 = Instant::now();
    let cancel = Arc::new(AtomicBool::new(false));

    let lf = Arc::clone(&log_file);
    let cancel_clone = Arc::clone(&cancel);
    let handle = thread::spawn(move || lf.build_index(&cancel_clone, &mut |_| {}));

    // 첫 줄이 확인될 때까지 대기하여 첫 줄 읽기 시간 측정
    while log_file.index().line_count() == 0 && !log_file.index().progress().done {
        thread::yield_now();
    }
    let _ = log_file.read_lines(0, 1, None);
    let first_line_elapsed = t0.elapsed();

    // 인덱싱 완료 대기
    handle
        .join()
        .unwrap()
        .unwrap_or_else(|e| eprintln!("인덱싱 오류: {}", e));
    let total_elapsed = t0.elapsed();

    let total_lines = log_file.index().line_count();
    let file_size = log_file.source().len();

    println!("파일: {}", path.display());
    println!(
        "파일 크기: {} 바이트 ({:.2} MB)",
        file_size,
        file_size as f64 / 1_048_576.0
    );
    println!("줄 수: {}", total_lines);
    println!(
        "첫 줄 읽기까지 걸린 시간: {:.3}ms",
        first_line_elapsed.as_secs_f64() * 1000.0
    );
    println!(
        "전체 인덱싱 시간: {:.3}ms ({:.2} s)",
        total_elapsed.as_secs_f64() * 1000.0,
        total_elapsed.as_secs_f64()
    );
}

/// lines 서브커맨드: 요청 범위가 확인될 때까지만 대기 후 원문 텍스트만 한 줄씩 출력 (sed 비교용)
fn run_lines(path: &Path, start: u64, count: u64) {
    let log_file = Arc::new(LogFile::open(path).unwrap_or_else(|e| {
        eprintln!("파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    }));

    let cancel = Arc::new(AtomicBool::new(false));
    let lf = Arc::clone(&log_file);
    let cancel_clone = Arc::clone(&cancel);
    let handle = thread::spawn(move || lf.build_index(&cancel_clone, &mut |_| {}));

    let target = start.saturating_add(count);
    // 요청 범위가 확인되거나 인덱싱이 끝날 때까지 대기
    while log_file.index().line_count() < target && !log_file.index().progress().done {
        thread::sleep(Duration::from_millis(1));
    }

    let lines = log_file.read_lines(start, count, None).unwrap_or_else(|e| {
        eprintln!("줄 읽기 실패: {}", e);
        process::exit(1);
    });

    for line in lines {
        println!("{}", line.text);
    }

    // 작업 완료 후 백그라운드 인덱싱 취소 및 종료 대기
    cancel.store(true, Ordering::Relaxed);
    let _ = handle.join();
}

/// open 서브커맨드: 첫 구간 완료 후 첫 20줄 읽기까지의 시간과 내용 출력
fn run_open(path: &Path) {
    let t0 = Instant::now();
    let log_file = Arc::new(LogFile::open(path).unwrap_or_else(|e| {
        eprintln!("파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    }));

    let cancel = Arc::new(AtomicBool::new(false));
    let lf = Arc::clone(&log_file);
    let cancel_clone = Arc::clone(&cancel);
    let handle = thread::spawn(move || lf.build_index(&cancel_clone, &mut |_| {}));

    // 첫 20줄이 확인될 때까지 대기
    while log_file.index().line_count() < 20 && !log_file.index().progress().done {
        thread::yield_now();
    }

    let lines = log_file.read_lines(0, 20, None).unwrap_or_else(|e| {
        eprintln!("줄 읽기 실패: {}", e);
        process::exit(1);
    });
    let elapsed = t0.elapsed();

    // 첫 20줄 확인 후 백그라운드 스레드 취소
    cancel.store(true, Ordering::Relaxed);
    let _ = handle.join();

    println!(
        "첫 20줄 읽기 소요 시간: {:.3}ms (읽은 줄 수: {})",
        elapsed.as_secs_f64() * 1000.0,
        lines.len()
    );
    for line in lines {
        println!("{:>6}: {}", line.no + 1, line.text);
    }
}

/// search 서브커맨드: 파일 전체 검색 실행 후 매치 통계 및 첫 5줄 출력
fn run_search(path: &Path, query: &str, options: SearchOptions) {
    let log_file = Arc::new(LogFile::open(path).unwrap_or_else(|e| {
        eprintln!("파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    }));

    let matcher = Matcher::new(query, options).unwrap_or_else(|e| {
        eprintln!("검색어 컴파일 실패: {}", e);
        process::exit(1);
    });

    let cancel = Arc::new(AtomicBool::new(false));
    let t0 = Instant::now();
    let mut first_match_time: Option<Duration> = None;
    let mut total_match_count: u64 = 0;
    let mut first_5_lines: Vec<u64> = Vec::new();

    let outcome = riffle_core::search(
        &log_file,
        &matcher,
        &cancel,
        &mut |batch| {
            if first_match_time.is_none() && !batch.is_empty() {
                first_match_time = Some(t0.elapsed());
            }
            for &line_no in batch {
                if first_5_lines.len() < 5 {
                    first_5_lines.push(line_no);
                }
                total_match_count += 1;
            }
        },
        &mut |_progress| {},
    )
    .unwrap_or_else(|e| {
        eprintln!("검색 오류: {}", e);
        process::exit(1);
    });

    let total_elapsed = t0.elapsed();

    println!("파일: {}", path.display());
    println!(
        "검색어: {:?} (정규식: {}, 대소문자구분: {})",
        query, options.regex, options.case_sensitive
    );
    println!("결과 상태: {:?}", outcome);
    println!("매치 줄 수: {}", total_match_count);
    match first_match_time {
        Some(d) => println!("첫 매치까지 걸린 시간: {:.3}ms", d.as_secs_f64() * 1000.0),
        None => println!("첫 매치까지 걸린 시간: 없음"),
    }
    println!(
        "전체 검색 시간: {:.3}ms ({:.3}s)",
        total_elapsed.as_secs_f64() * 1000.0,
        total_elapsed.as_secs_f64()
    );

    if !first_5_lines.is_empty() {
        println!("\n처음 5개 매치 줄 (1-based 줄 번호):");
        print_matching_lines(&log_file, &first_5_lines);
    }
}

/// tail 서브커맨드: 백그라운드 인덱싱 후 200ms마다 refresh()를 호출하여
/// 추가된 줄 수 또는 truncate/파일 교체를 출력한다. Ctrl+C로 종료.
fn run_tail(path: &Path) {
    let log_file = Arc::new(LogFile::open(path).unwrap_or_else(|e| {
        eprintln!("파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    }));

    // 백그라운드에서 최초 인덱싱 (완료 전 refresh는 Pending을 반환)
    let cancel = Arc::new(AtomicBool::new(false));
    let lf = Arc::clone(&log_file);
    let cancel_clone = Arc::clone(&cancel);
    thread::spawn(move || lf.build_index(&cancel_clone, &mut |_| {}));

    eprintln!("tail 시작: {} (인덱싱 중...)", path.display());
    let mut rotation_notified = false;
    loop {
        match log_file.refresh() {
            Ok(RefreshOutcome::Appended {
                old_line_count,
                new_line_count,
                new_len,
            }) => {
                println!(
                    "[tail] appended: +{}줄 (총 {}줄, {:.1} KiB)",
                    new_line_count - old_line_count,
                    new_line_count,
                    new_len as f64 / 1024.0
                );
                let _ = std::io::stdout().flush();
            }
            Ok(RefreshOutcome::Truncated) if !rotation_notified => {
                rotation_notified = true;
                println!("[tail] truncated: 파일이 잘렸습니다 (재오픈 필요)");
                let _ = std::io::stdout().flush();
            }
            Ok(RefreshOutcome::Replaced) if !rotation_notified => {
                rotation_notified = true;
                println!("[tail] replaced: 파일이 다른 inode로 교체되었습니다 (재오픈 필요)");
                let _ = std::io::stdout().flush();
            }
            Ok(_) => {}
            Err(e) => eprintln!("[tail] 오류: {}", e),
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// 인덱스 유무와 무관하게 매치된 줄 번호들의 원문 텍스트를 출력한다.
fn print_matching_lines(log_file: &LogFile, line_nos: &[u64]) {
    if log_file.index().line_count() > 0
        && let Ok(lines) = log_file.read_lines_at(line_nos, None)
    {
        for line in lines {
            println!("{:>8}: {}", line.no + 1, line.text);
        }
        return;
    }

    // 인덱스가 없는 경우(인덱싱 전/취소) 64KB 청크로 순차 스캔하며 대상 줄만 출력
    let source = log_file.source();
    let total = source.len();
    let mut cur_line = 0u64;
    let mut target_idx = 0usize;
    let mut offset = 0u64;
    let mut carry: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];

    while offset < total && target_idx < line_nos.len() {
        let want = (total - offset).min(buf.len() as u64) as usize;
        let n = match source.read_into(&mut buf[..want], offset) {
            Ok(n) => n,
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        offset += n as u64;
        carry.extend_from_slice(&buf[..n]);

        // 마지막 개행까지는 완성된 줄이므로 처리하고, 나머지는 carry에 남긴다
        let Some(complete_end) = memchr::memrchr(b'\n', &carry).map(|p| p + 1) else {
            continue;
        };

        let mut seg_start = 0usize;
        for nl_pos in memchr::memchr_iter(b'\n', &carry[..complete_end]) {
            let raw_line = &carry[seg_start..nl_pos];
            if cur_line == line_nos[target_idx] {
                print_raw_line(cur_line, raw_line);
                target_idx += 1;
                if target_idx >= line_nos.len() {
                    break;
                }
            }
            cur_line += 1;
            seg_start = nl_pos + 1;
        }
        carry.drain(..seg_start);
    }

    // 파일 끝: 개행 없이 끝난 마지막 줄 처리
    if target_idx < line_nos.len() && !carry.is_empty() && cur_line == line_nos[target_idx] {
        print_raw_line(cur_line, &carry);
    }
}

/// 원시 줄 바이트(개행 제외, \r 포함 가능)를 표시용으로 정리해 출력한다.
fn print_raw_line(line_no: u64, raw: &[u8]) {
    let clean = if raw.ends_with(b"\r") {
        &raw[..raw.len() - 1]
    } else {
        raw
    };
    let display = if clean.len() > riffle_core::MAX_DISPLAY_BYTES {
        &clean[..riffle_core::MAX_DISPLAY_BYTES]
    } else {
        clean
    };
    let text = String::from_utf8_lossy(display);
    println!("{:>8}: {}", line_no + 1, text);
}
