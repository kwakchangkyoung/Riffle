//! append_log: tail 모드 검증용 로그 append 테스트 도구 (std만 사용, 외부 의존성 없음)
//!
//! 사용법:
//!   append_log <path> [--rate <줄/초>=1000] [--duration <초>=30]
//!              [--rotate-every <초>] [--truncate-every <초>] [--partial]
//!
//! - 10ms마다 rate/100줄씩 기록하고 flush한다.
//! - --rotate-every N: N초마다 path를 path.1로 rename한 뒤 새 파일에 계속 기록한다.
//! - --truncate-every N: N초마다 set_len(0)으로 파일을 비우고 계속 기록한다.
//! - --partial: 매초 첫 번째 줄을 두 번의 write로 나눠 쓰고 그 사이 50ms 대기한다
//!   (개행 없는 마지막 줄 상태를 만들어 tail의 미완성 줄 처리를 검증).
//! - 매초 stderr에 누적 줄 수를 출력한다.

use std::env;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// 10ms 틱 간격
const TICK: Duration = Duration::from_millis(10);
/// partial 모드에서 두 write 사이의 대기 시간
const PARTIAL_DELAY: Duration = Duration::from_millis(50);

/// 로그 레벨 선택 (seq 기반 결정적 선택)
fn pick_level(seq: u64) -> &'static str {
    match seq % 10 {
        0 => "ERROR",
        1 => "WARN ",
        2 => "DEBUG",
        _ => "INFO ",
    }
}

const MODULES: [&str; 8] = [
    "worker-1",
    "worker-2",
    "worker-3",
    "http-pool",
    "db-conn",
    "auth-svc",
    "cache-mgr",
    "indexer",
];

/// 한 줄 로그를 buf에 작성한다 (개행 포함)
fn write_line(buf: &mut Vec<u8>, seq: u64, time_ms: u64) {
    buf.clear();

    // 타임스탬프 (gen_log와 같은 ISO 8601 형식)
    let sec = time_ms / 1000;
    let days = (sec / 86400) as i64;
    let (year, month, day) = civil_from_days(days);
    let rem_sec = sec % 86400;
    let hour = rem_sec / 3600;
    let min = (rem_sec % 3600) / 60;
    let second = rem_sec % 60;
    let ms = time_ms % 1000;
    let _ = write!(
        buf,
        "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{second:02}.{ms:03}Z "
    );

    let _ = write!(buf, "{} ", pick_level(seq));
    let module = MODULES[(seq as usize) % MODULES.len()];
    let _ = write!(buf, "[{module}] ");

    // seq 기반 결정적 의사 난수로 메시지 생성
    let user_id = 10000 + seq % 90000;
    let latency = 1 + seq % 150;
    match seq % 6 {
        0 => {
            let _ = write!(
                buf,
                "사용자 로그인 성공 user_id={user_id} latency={latency}ms"
            );
        }
        1 => {
            let _ = write!(
                buf,
                "HTTP GET /api/v1/items/{} status=200 duration={}ms",
                1000 + seq % 999000,
                latency
            );
        }
        2 => {
            let _ = write!(
                buf,
                "외부 결제 게이트웨이 응답 수신 transaction_id=tx-{} status=APPROVED",
                10000000 + seq % 90000000
            );
        }
        3 => {
            let _ = write!(
                buf,
                "데이터베이스 쿼리 실행 완료 rows_affected={} time={}ms",
                seq % 50,
                latency
            );
        }
        4 => {
            let _ = write!(
                buf,
                "캐시 히트 key=user:session:{} ttl={}s",
                seq % 90000,
                60 + seq % 3540
            );
        }
        _ => {
            let _ = write!(buf, "일반 heartbeat seq={seq} worker={module}");
        }
    }
    buf.push(b'\n');
}

/// 유닉스 일수로부터 그레고리력 년, 월, 일 계산 (gen_log와 동일 알고리즘)
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// append 모드로 로그 파일을 연다 (기존 내용은 버린다).
/// append 모드여야 --truncate-every의 set_len(0) 이후 쓰기가 파일 끝(=0)부터 이어진다.
fn open_log_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    file.set_len(0)?;
    Ok(file)
}

/// path를 "path.1" 형태의 경로로 만든다
fn rotated_path(path: &Path) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

fn print_usage_and_exit(prog: &str) -> ! {
    eprintln!("사용법: {prog} <path> [--rate <줄/초>=1000] [--duration <초>=30]");
    eprintln!("             [--rotate-every <초>] [--truncate-every <초>] [--partial]");
    eprintln!("  path를 append 모드로 열어 로그 줄을 계속 기록한다 (기존 내용은 버림)");
    process::exit(1);
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let prog = args.first().map(|s| s.as_str()).unwrap_or("append_log");
    if args.len() < 2 {
        print_usage_and_exit(prog);
    }
    let path = PathBuf::from(&args[1]);

    let mut rate: u64 = 1000;
    let mut duration: u64 = 30;
    let mut rotate_every: Option<u64> = None;
    let mut truncate_every: Option<u64> = None;
    let mut partial = false;

    let mut i = 2;
    while i < args.len() {
        let flag = args[i].as_str();
        let need_value = |i: usize| -> String {
            args.get(i + 1).cloned().unwrap_or_else(|| {
                eprintln!("오류: '{flag}'에 값이 필요합니다");
                print_usage_and_exit(prog);
            })
        };
        match flag {
            "--rate" => {
                rate = need_value(i).parse().unwrap_or_else(|_| {
                    eprintln!("오류: --rate는 양의 정수여야 합니다");
                    print_usage_and_exit(prog);
                });
                i += 1;
            }
            "--duration" => {
                duration = need_value(i).parse().unwrap_or_else(|_| {
                    eprintln!("오류: --duration은 0 이상의 정수여야 합니다");
                    print_usage_and_exit(prog);
                });
                i += 1;
            }
            "--rotate-every" => {
                rotate_every = Some(need_value(i).parse().unwrap_or_else(|_| {
                    eprintln!("오류: --rotate-every는 양의 정수여야 합니다");
                    print_usage_and_exit(prog);
                }));
                i += 1;
            }
            "--truncate-every" => {
                truncate_every = Some(need_value(i).parse().unwrap_or_else(|_| {
                    eprintln!("오류: --truncate-every는 양의 정수여야 합니다");
                    print_usage_and_exit(prog);
                }));
                i += 1;
            }
            "--partial" => partial = true,
            other => {
                eprintln!("오류: 알 수 없는 옵션 '{other}'");
                print_usage_and_exit(prog);
            }
        }
        i += 1;
    }

    let mut file = open_log_file(&path)?;
    eprintln!(
        "append_log 시작: {} (rate={}줄/초, duration={}초, rotate={:?}, truncate={:?}, partial={})",
        path.display(),
        rate,
        duration,
        rotate_every,
        truncate_every,
        partial
    );

    let start = Instant::now();
    let tick_secs = TICK.as_secs_f64();
    let mut line_credit = 0.0f64;
    let mut seq: u64 = 0;
    let mut written_bytes: u64 = 0;
    let mut line_buf: Vec<u8> = Vec::with_capacity(256);

    let mut next_rotate = rotate_every.map(|s| start + Duration::from_secs(s));
    let mut next_truncate = truncate_every.map(|s| start + Duration::from_secs(s));
    // partial 모드: 매초 첫 번째 줄을 두 번의 write로 나눠 쓴다
    let mut partial_pending = false;
    let mut last_report_sec = 0u64;

    loop {
        let now = Instant::now();
        let elapsed = now.duration_since(start);
        if duration > 0 && elapsed.as_secs() >= duration {
            break;
        }

        // 초가 바뀔 때마다 진행 상황 출력 + partial 예약
        let cur_sec = elapsed.as_secs();
        if cur_sec != last_report_sec {
            last_report_sec = cur_sec;
            eprintln!(
                "append_log: {}줄 기록 ({} bytes, {:.1}s 경과)",
                seq,
                written_bytes,
                elapsed.as_secs_f64()
            );
            if partial {
                partial_pending = true;
            }
        }

        if let Some(deadline) = next_rotate
            && now >= deadline
        {
            let rotated = rotated_path(&path);
            fs::rename(&path, &rotated)?;
            file = open_log_file(&path)?;
            eprintln!("append_log: rotate → {}", rotated.display());
            next_rotate = Some(now + Duration::from_secs(rotate_every.unwrap()));
        }
        if let Some(deadline) = next_truncate
            && now >= deadline
        {
            file.set_len(0)?;
            eprintln!("append_log: truncate (set_len(0))");
            next_truncate = Some(now + Duration::from_secs(truncate_every.unwrap()));
        }

        // 10ms 틱마다 rate/100줄씩 기록 (루프 1회 ≈ TICK)
        line_credit += rate as f64 * tick_secs;
        let to_write = line_credit.floor() as usize;
        line_credit -= to_write as f64;

        for _ in 0..to_write {
            seq += 1;
            let time_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            write_line(&mut line_buf, seq, time_ms);
            if partial_pending {
                // 줄을 두 번의 write로 나눠 쓰고 그 사이 대기 (개행 없는 마지막 줄 상태 생성)
                let mid = line_buf.len() / 2;
                file.write_all(&line_buf[..mid])?;
                thread::sleep(PARTIAL_DELAY);
                file.write_all(&line_buf[mid..])?;
                partial_pending = false;
            } else {
                file.write_all(&line_buf)?;
            }
            written_bytes += line_buf.len() as u64;
        }
        file.flush()?;

        thread::sleep(TICK);
    }

    eprintln!("append_log 종료: 총 {}줄, {} bytes", seq, written_bytes);
    Ok(())
}
