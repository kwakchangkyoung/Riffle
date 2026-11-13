use std::env;
use std::io::Write;
use std::path::Path;
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use riffle_core::{
    Combine, FilterRule, FilterSet, FilterSpec, Level, LogFile, Matcher, RefreshOutcome, RuleKind,
    SearchOptions,
};

fn print_usage() {
    eprintln!("Riffle CLI - 파일 인덱싱 및 읽기/검색 검증 도구 (.gz 자동 판별)\n");
    eprintln!("사용법:");
    eprintln!("    cli <파일경로> stats");
    eprintln!("    cli <파일경로> lines <start> <count>");
    eprintln!("    cli <파일경로> open");
    eprintln!("    cli <파일경로> search <query> [--regex] [--ignore-case]");
    eprintln!(
        "    cli <파일경로> filter [--or|--and] [--inc PAT]... [--exc PAT]... [--level error,warn] [--regex] [--search Q]"
    );
    eprintln!("    cli <파일경로> tail");
    eprintln!("    cli <파일경로> copy <start> <end>\n");
    eprintln!("서브커맨드:");
    eprintln!("    stats                인덱싱 실행 후 파일 크기, 줄 수, 소요 시간 출력");
    eprintln!("                          (.gz면 해제+인덱싱 시간, 첫 줄까지 시간, 최대 RSS 출력)");
    eprintln!("    lines <start> <cnt>  지정 범위 줄 텍스트를 원문 그대로 출력 (0-based)");
    eprintln!("    open                 열기 후 첫 20줄 읽기까지의 소요 시간 및 내용 출력");
    eprintln!("    search <query> [opt] 파일 전체 검색 (매치 수, 첫 매치/전체 시간, 첫 5줄 출력)");
    eprintln!("                       옵션: --regex --ignore-case --count-only(줄 출력 생략)");
    eprintln!(
        "    tail                 인덱싱 후 200ms마다 refresh하여 추가분/로테이션 출력 (Ctrl+C까지)"
    );
    eprintln!(
        "    filter [opts]        다중 필터 보이는 줄 수 출력 (--search 지정 시 교집합 수도 출력)"
    );
    eprintln!(
        "                       --inc/--exc 반복 지정, --level 콤마 구분, --regex는 규칙·검색 공통"
    );
    eprintln!("    copy <start> <end>   [start, end] 줄 범위 원문을 stdout으로 출력 (0-based)");
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
            let mut count_only = false;

            for arg in &args[4..] {
                match arg.as_str() {
                    "--regex" => regex = true,
                    "--ignore-case" | "-i" => case_sensitive = false,
                    "--count-only" => count_only = true,
                    other => {
                        eprintln!("경고: 알 수 없는 옵션 '{}'", other);
                    }
                }
            }

            let options = SearchOptions {
                regex,
                case_sensitive,
            };
            run_search(file_path, query, options, count_only);
        }
        "filter" => run_filter(file_path, &args[3..]),
        "tail" => run_tail(file_path),
        "copy" => {
            if args.len() < 5 {
                eprintln!("오류: copy 커맨드에는 start와 end 인자가 필요합니다.");
                print_usage();
                process::exit(1);
            }
            let start: u64 = args[3].parse().unwrap_or_else(|_| {
                eprintln!("오류: start는 0 이상의 정수여야 합니다: {}", args[3]);
                process::exit(1);
            });
            let end: u64 = args[4].parse().unwrap_or_else(|_| {
                eprintln!("오류: end는 0 이상의 정수여야 합니다: {}", args[4]);
                process::exit(1);
            });
            run_copy(file_path, start, end);
        }
        _ => {
            eprintln!("오류: 알 수 없는 서브커맨드 '{}'", command);
            print_usage();
            process::exit(1);
        }
    }
}

/// stats 서브커맨드: 인덱싱 실행 후 줄 수, 파일 크기, 인덱싱 시간, 첫 줄 읽기까지 걸린 시간 출력
fn run_stats(path: &Path) {
    if riffle_core::is_gzip(path).unwrap_or(false) {
        run_stats_gz(path);
        return;
    }

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
    let _ = log_file.read_lines(0, 1, None, None);
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

/// 최대 RSS 샘플러를 시작한다.
/// 반환된 stop 플래그를 세우고 join하면 peak(KB)에 50ms 간격 ps 샘플의 최댓값이 기록된다.
fn start_rss_sampler() -> (
    Arc<AtomicBool>,
    Arc<std::sync::atomic::AtomicU64>,
    thread::JoinHandle<()>,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let sampler = {
        let stop = Arc::clone(&stop);
        let peak = Arc::clone(&peak);
        thread::spawn(move || {
            let pid = std::process::id().to_string();
            while !stop.load(Ordering::Relaxed) {
                if let Ok(out) = std::process::Command::new("ps")
                    .args(["-o", "rss=", "-p", &pid])
                    .output()
                    && let Ok(text) = String::from_utf8(out.stdout)
                    && let Ok(kb) = text.trim().parse::<u64>()
                {
                    peak.fetch_max(kb, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(50));
            }
        })
    };
    (stop, peak, sampler)
}

/// stats 서브커맨드 (.gz): 스트리밍 해제+인덱싱 시간, 첫 줄까지 시간,
/// 해제 속도, 최대 RSS를 측정한다. 최대 RSS는 ps로 50ms마다 샘플링한 최댓값이다.
fn run_stats_gz(path: &Path) {
    let t0 = Instant::now();

    let (log_file, writer) = riffle_core::open_gzip(path).unwrap_or_else(|e| {
        eprintln!("gzip 파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    });
    let log_file = Arc::new(log_file);
    let cancel = Arc::new(AtomicBool::new(false));

    let (stop_sample, rss_peak_kb, sampler) = start_rss_sampler();

    let lf = Arc::clone(&log_file);
    let cancel_clone = Arc::clone(&cancel);
    let worker = thread::spawn(move || writer.run(&lf, &cancel_clone, &mut |_| {}));

    // 첫 줄이 확인될 때까지 대기하여 첫 줄 읽기 시간 측정
    while log_file.index().line_count() == 0 && !worker.is_finished() {
        thread::yield_now();
    }
    let _ = log_file.read_lines(0, 1, None, None);
    let first_line_elapsed = t0.elapsed();

    let run_result = worker.join().unwrap_or_else(|_| {
        eprintln!("gzip 해제 스레드 패닉 발생");
        process::exit(1);
    });
    if let Err(e) = run_result {
        eprintln!("gzip 해제 오류: {}", e);
        process::exit(1);
    }
    stop_sample.store(true, Ordering::Relaxed);
    let _ = sampler.join();
    let total_elapsed = t0.elapsed();

    let total_lines = log_file.index().line_count();
    let decompressed = log_file.source().len();
    let compressed = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let secs = total_elapsed.as_secs_f64();

    println!("파일: {}", path.display());
    println!(
        "압축 크기: {} 바이트 ({:.2} MB)",
        compressed,
        compressed as f64 / 1_048_576.0
    );
    println!(
        "해제 후 크기: {} 바이트 ({:.2} MB)",
        decompressed,
        decompressed as f64 / 1_048_576.0
    );
    println!("줄 수: {}", total_lines);
    println!(
        "첫 줄 읽기까지 걸린 시간: {:.3}ms",
        first_line_elapsed.as_secs_f64() * 1000.0
    );
    println!(
        "해제+인덱싱 시간: {:.3}ms ({:.2} s)",
        total_elapsed.as_secs_f64() * 1000.0,
        secs
    );
    if secs > 0.0 {
        println!(
            "해제 속도: {:.1} MB/s",
            decompressed as f64 / 1_048_576.0 / secs
        );
    }
    println!(
        "최대 RSS: {:.1} MB",
        rss_peak_kb.load(Ordering::Relaxed) as f64 / 1024.0
    );
}

/// lines 서브커맨드: 요청 범위가 확인될 때까지만 대기 후 원문 텍스트만 한 줄씩 출력 (sed 비교용)
fn run_lines(path: &Path, start: u64, count: u64) {
    if riffle_core::is_gzip(path).unwrap_or(false) {
        run_lines_gz(path, start, count);
        return;
    }

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

    let lines = log_file
        .read_lines(start, count, None, None)
        .unwrap_or_else(|e| {
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

/// lines 서브커맨드 (.gz): 요청 범위가 해제·인덱싱으로 확인될 때까지 기다린 후
/// 원문 텍스트를 출력한다. 인덱스 done은 처음부터 true이므로 해제 스레드 종료 여부로 판정한다.
fn run_lines_gz(path: &Path, start: u64, count: u64) {
    let (log_file, writer) = riffle_core::open_gzip(path).unwrap_or_else(|e| {
        eprintln!("gzip 파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    });
    let log_file = Arc::new(log_file);
    let cancel = Arc::new(AtomicBool::new(false));

    let lf = Arc::clone(&log_file);
    let cancel_clone = Arc::clone(&cancel);
    let handle = thread::spawn(move || writer.run(&lf, &cancel_clone, &mut |_| {}));

    let target = start.saturating_add(count);
    // 요청 범위가 확인되거나 해제가 끝날 때까지 대기
    while log_file.index().line_count() < target && !handle.is_finished() {
        thread::sleep(Duration::from_millis(1));
    }

    let lines = log_file
        .read_lines(start, count, None, None)
        .unwrap_or_else(|e| {
            eprintln!("줄 읽기 실패: {}", e);
            process::exit(1);
        });

    for line in lines {
        println!("{}", line.text);
    }

    // 작업 완료 후 해제 취소 및 종료 대기
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

    let lines = log_file.read_lines(0, 20, None, None).unwrap_or_else(|e| {
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

/// search 서브커맨드: 파일 전체 검색 실행 후 매치 통계 및 첫 5줄 출력.
/// count_only이면 줄 내용 출력(파일 재스캔)을 생략해 검색 자체의 비용만 측정한다.
fn run_search(path: &Path, query: &str, options: SearchOptions, count_only: bool) {
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

    if !count_only && !first_5_lines.is_empty() {
        println!("\n처음 5개 매치 줄 (1-based 줄 번호):");
        print_matching_lines(&log_file, &first_5_lines);
    }
}

/// "error" 등 레벨 문자열을 Level로 변환한다.
fn parse_level(s: &str) -> Option<Level> {
    match s {
        "error" => Some(Level::Error),
        "warn" => Some(Level::Warn),
        "info" => Some(Level::Info),
        "debug" => Some(Level::Debug),
        "trace" => Some(Level::Trace),
        _ => None,
    }
}

/// filter 서브커맨드: 다중 필터로 보이는 줄 수를 출력한다.
/// --search가 주어지면 검색 매치 ∧ 필터 통과 교집합 수도 함께 출력한다.
/// 각 단계의 시간과 최대 RSS(ps 50ms 샘플링)를 출력한다.
fn run_filter(path: &Path, args: &[String]) {
    let mut combine = Combine::Or;
    let mut regex = false;
    let mut incs: Vec<String> = Vec::new();
    let mut excs: Vec<String> = Vec::new();
    let mut levels: Vec<Level> = Vec::new();
    let mut search_query: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let take_value = |i: &mut usize| -> String {
            *i += 1;
            if *i >= args.len() {
                eprintln!("오류: '{}' 옵션에는 값이 필요합니다.", args[*i - 1]);
                process::exit(1);
            }
            args[*i].clone()
        };
        match args[i].as_str() {
            "--or" => combine = Combine::Or,
            "--and" => combine = Combine::And,
            "--regex" => regex = true,
            "--inc" => incs.push(take_value(&mut i)),
            "--exc" => excs.push(take_value(&mut i)),
            "--level" => {
                let v = take_value(&mut i);
                for name in v.split(',') {
                    match parse_level(name.trim()) {
                        Some(l) => levels.push(l),
                        None => {
                            eprintln!(
                                "오류: 알 수 없는 레벨 '{}' (--level은 error,warn,info,debug,trace)",
                                name.trim()
                            );
                            process::exit(1);
                        }
                    }
                }
            }
            "--search" => search_query = Some(take_value(&mut i)),
            other => eprintln!("경고: 알 수 없는 옵션 '{}'", other),
        }
        i += 1;
    }

    if incs.is_empty() && excs.is_empty() && levels.is_empty() {
        eprintln!("오류: 필터 조건이 없습니다 (--inc/--exc/--level 중 하나 이상 필요)");
        process::exit(1);
    }

    let options = SearchOptions {
        regex,
        case_sensitive: true,
    };
    let mut rules = Vec::new();
    for p in incs {
        rules.push(FilterRule {
            pattern: p,
            options,
            kind: RuleKind::Include,
            enabled: true,
        });
    }
    for p in excs {
        rules.push(FilterRule {
            pattern: p,
            options,
            kind: RuleKind::Exclude,
            enabled: true,
        });
    }
    let spec = FilterSpec {
        rules,
        combine,
        levels,
    };
    let set = FilterSet::new(&spec).unwrap_or_else(|e| {
        eprintln!("필터 컴파일 실패: {}", e);
        process::exit(1);
    });
    if set.is_empty() {
        eprintln!("오류: 적용할 필터 조건이 없습니다.");
        process::exit(1);
    }

    let log_file = Arc::new(LogFile::open(path).unwrap_or_else(|e| {
        eprintln!("파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    }));

    // 필터 스캔 (시간 + 최대 RSS 측정)
    let (stop_sample, rss_peak_kb, sampler) = start_rss_sampler();
    let t0 = Instant::now();
    let mut visible_count: u64 = 0;
    let cancel = AtomicBool::new(false);
    let outcome = riffle_core::filter_scan(
        &log_file,
        &set,
        &cancel,
        &mut |batch| visible_count += batch.len() as u64,
        &mut |_p| {},
    )
    .unwrap_or_else(|e| {
        eprintln!("필터 스캔 오류: {}", e);
        process::exit(1);
    });
    let filter_elapsed = t0.elapsed();
    stop_sample.store(true, Ordering::Relaxed);
    let _ = sampler.join();

    println!("파일: {}", path.display());
    println!(
        "필터: combine={:?}, include={}개, exclude={}개, levels={:?}, regex={}",
        combine,
        spec.rules
            .iter()
            .filter(|r| r.kind == RuleKind::Include)
            .count(),
        spec.rules
            .iter()
            .filter(|r| r.kind == RuleKind::Exclude)
            .count(),
        spec.levels,
        regex
    );
    println!("보이는 줄 수: {}", visible_count);
    println!("필터 결과 상태: {:?}", outcome);
    println!(
        "필터 스캔 시간: {:.3}ms ({:.3}s)",
        filter_elapsed.as_secs_f64() * 1000.0,
        filter_elapsed.as_secs_f64()
    );
    println!(
        "최대 RSS: {:.1} MB",
        rss_peak_kb.load(Ordering::Relaxed) as f64 / 1024.0
    );

    // --search 지정 시 교집합(검색 매치 ∧ 필터 통과) 수도 출력
    if let Some(q) = search_query {
        let matcher = Matcher::new(&q, options).unwrap_or_else(|e| {
            eprintln!("검색어 컴파일 실패: {}", e);
            process::exit(1);
        });
        let t1 = Instant::now();
        let mut intersection: u64 = 0;
        let outcome2 = riffle_core::search_filtered(
            &log_file,
            &matcher,
            Some(&set),
            &cancel,
            &mut |batch| intersection += batch.len() as u64,
            &mut |_p| {},
        )
        .unwrap_or_else(|e| {
            eprintln!("교집합 검색 오류: {}", e);
            process::exit(1);
        });
        let search_elapsed = t1.elapsed();
        println!("검색어: {:?}", q);
        println!("교집합 줄 수: {}", intersection);
        println!("교집합 결과 상태: {:?}", outcome2);
        println!(
            "교집합 검색 시간: {:.3}ms ({:.3}s)",
            search_elapsed.as_secs_f64() * 1000.0,
            search_elapsed.as_secs_f64()
        );
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

/// copy 서브커맨드: [start, end] 줄 범위 원문을 stdout으로 출력하고
/// 읽기 소요 시간과 바이트 수를 stderr로 출력한다 (대량 복사 성능 검증용)
fn run_copy(path: &Path, start: u64, end: u64) {
    let log_file = Arc::new(LogFile::open(path).unwrap_or_else(|e| {
        eprintln!("파일 열기 실패 ({}): {}", path.display(), e);
        process::exit(1);
    }));

    let cancel = Arc::new(AtomicBool::new(false));
    let lf = Arc::clone(&log_file);
    let cancel_clone = Arc::clone(&cancel);
    let handle = thread::spawn(move || lf.build_index(&cancel_clone, &mut |_| {}));

    // 요청 범위가 확인되거나 인덱싱이 끝날 때까지 대기
    let target = end.saturating_add(1);
    while log_file.index().line_count() < target && !log_file.index().progress().done {
        thread::sleep(Duration::from_millis(1));
    }

    let t0 = Instant::now();
    let text = log_file
        .read_text_range(start, end, u64::MAX / 2)
        .unwrap_or_else(|e| {
            eprintln!("범위 읽기 실패: {}", e);
            process::exit(1);
        });
    let elapsed = t0.elapsed();

    // 인덱싱 스레드 정리
    cancel.store(true, Ordering::Relaxed);
    let _ = handle.join();

    eprintln!(
        "copy {}..{}: {} bytes, {:.2}ms",
        start,
        end,
        text.len(),
        elapsed.as_secs_f64() * 1000.0
    );
    print!("{text}");
    let _ = std::io::stdout().flush();
}

/// 인덱스 유무와 무관하게 매치된 줄 번호들의 원문 텍스트를 출력한다.
fn print_matching_lines(log_file: &LogFile, line_nos: &[u64]) {
    if log_file.index().line_count() > 0
        && let Ok(lines) = log_file.read_lines_at(line_nos, None, None)
    {
        for line in lines {
            println!("{:>8}: {}", line.no + 1, line.text);
        }
        return;
    }

    // 인덱스가 없는 경우(인덱싱 전/취소) 파일을 1MB 청크로 한 번만 순차 스캔한다.
    // 개행 탐색은 새로 읽은 구간에만 적용하고(이미 검사한 부분 재스캔 없음),
    // 대상 줄의 앞 MAX_DISPLAY_BYTES만 보관하므로 메모리는 버퍼 수준으로 유지된다.
    let source = log_file.source();
    let total = source.len();
    let mut cur_line = 0u64;
    let mut target_idx = 0usize;
    let mut line_buf: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;

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

        let mut pos = 0usize;
        while pos < n && target_idx < line_nos.len() {
            match memchr::memchr(b'\n', &buf[pos..n]) {
                Some(rel) => {
                    let nl = pos + rel;
                    // cur_line의 마지막 조각을 붙여 줄 완성 → 대상이면 출력
                    if cur_line == line_nos[target_idx] {
                        append_capped(&mut line_buf, &buf[pos..nl]);
                        print_raw_line(cur_line, &line_buf);
                        line_buf.clear();
                        target_idx += 1;
                    }
                    cur_line += 1;
                    pos = nl + 1;
                }
                None => {
                    // 개행 없이 청크 끝까지 → cur_line의 첫/중간 조각
                    if cur_line == line_nos[target_idx] {
                        append_capped(&mut line_buf, &buf[pos..n]);
                    }
                    pos = n;
                }
            }
        }
    }

    // 파일 끝: 개행 없이 끝난 마지막 줄 처리
    if target_idx < line_nos.len() && cur_line == line_nos[target_idx] && !line_buf.is_empty() {
        print_raw_line(cur_line, &line_buf);
    }
}

/// line_buf가 MAX_DISPLAY_BYTES를 넘지 않도록 조각을 추가한다.
fn append_capped(buf: &mut Vec<u8>, piece: &[u8]) {
    let room = riffle_core::MAX_DISPLAY_BYTES.saturating_sub(buf.len());
    if room > 0 {
        buf.extend_from_slice(&piece[..piece.len().min(room)]);
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
