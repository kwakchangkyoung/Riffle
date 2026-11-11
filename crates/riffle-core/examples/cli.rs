use std::env;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use riffle_core::LogFile;

fn print_usage() {
    eprintln!("Riffle CLI - 파일 인덱싱 및 읽기 검증 도구\n");
    eprintln!("사용법:");
    eprintln!("    cli <파일경로> stats");
    eprintln!("    cli <파일경로> lines <start> <count>");
    eprintln!("    cli <파일경로> open\n");
    eprintln!("서브커맨드:");
    eprintln!("    stats                인덱싱 실행 후 파일 크기, 줄 수, 소요 시간 출력");
    eprintln!("    lines <start> <cnt>  지정 범위 줄 텍스트를 원문 그대로 출력 (0-based)");
    eprintln!("    open                 열기 후 첫 20줄 읽기까지의 소요 시간 및 내용 출력");
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
    let _ = log_file.read_lines(0, 1);
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

    let lines = log_file.read_lines(start, count).unwrap_or_else(|e| {
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

    let lines = log_file.read_lines(0, 20).unwrap_or_else(|e| {
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
