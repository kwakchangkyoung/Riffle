use std::env;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process;
use std::time::Instant;

/// xorshift64 기반의 빠른 의사 난수 생성기 (외부 의존성 없음)
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        // 시드가 0이면 의사 난수 생성이 동작하지 않으므로 기본 non-zero 시드 사용
        let non_zero_seed = if seed == 0 {
            0x853c_49e6_748f_ea9b
        } else {
            seed
        };
        Self {
            state: non_zero_seed,
        }
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    #[inline]
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }

    #[inline]
    fn gen_range(&mut self, low: u64, high: u64) -> u64 {
        if low >= high {
            return low;
        }
        low + (self.next_u64() % (high - low))
    }
}

/// 유닉스 일수로부터 그레고리력 년, 월, 일 계산 (Howard Hinnant 알고리즘)
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

/// 밀리초 타임스탬프를 ISO 8601 형식 문자열로 버퍼에 기록 (예: 2026-11-12T07:15:42.123Z)
fn format_iso8601(total_ms: u64, buf: &mut Vec<u8>) {
    let sec = total_ms / 1000;
    let days = (sec / 86400) as i64;
    let (year, month, day) = civil_from_days(days);
    let rem_sec = sec % 86400;
    let hour = rem_sec / 3600;
    let min = (rem_sec % 3600) / 60;
    let second = rem_sec % 60;
    let ms = total_ms % 1000;

    let _ = write!(
        buf,
        "{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{second:02}.{ms:03}Z"
    );
}

/// 로그 레벨 선택 (INFO가 약 80%로 대부분 차지)
fn pick_level(rng: &mut Rng) -> &'static str {
    let r = rng.next_u32() % 100;
    if r < 80 {
        "INFO "
    } else if r < 90 {
        "DEBUG"
    } else if r < 95 {
        "WARN "
    } else if r < 98 {
        "ERROR"
    } else {
        "TRACE"
    }
}

/// 모듈 및 스레드 이름 목록
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

fn pick_module(rng: &mut Rng) -> &'static str {
    let idx = (rng.next_u32() as usize) % MODULES.len();
    MODULES[idx]
}

/// 일반 로그 메시지 작성 (한글/영문 템플릿과 숫자/ID 치환)
fn write_normal_message(buf: &mut Vec<u8>, rng: &mut Rng) {
    let template_idx = rng.next_u32() % 12;
    match template_idx {
        0 => {
            let user_id = rng.gen_range(10000, 99999);
            let latency = rng.gen_range(1, 150);
            let _ = write!(
                buf,
                "사용자 로그인 성공 user_id={user_id} latency={latency}ms"
            );
        }
        1 => {
            let item_id = rng.gen_range(1000, 999999);
            let duration = rng.gen_range(5, 500);
            let _ = write!(
                buf,
                "HTTP GET /api/v1/items/{item_id} status=200 duration={duration}ms"
            );
        }
        2 => {
            let rows = rng.gen_range(1, 50);
            let time = rng.gen_range(2, 80);
            let _ = write!(
                buf,
                "데이터베이스 쿼리 실행 완료 table=orders rows_affected={rows} time={time}ms"
            );
        }
        3 => {
            let order_id = rng.gen_range(100000, 999999);
            let amount = rng.gen_range(5000, 1500000);
            let _ = write!(
                buf,
                "주문 생성 요청 처리됨 order_id=ORD-{order_id} amount={amount} KRW"
            );
        }
        4 => {
            let session_id = rng.gen_range(100000, 999999);
            let ttl = rng.gen_range(60, 3600);
            let _ = write!(buf, "캐시 히트 key=user:session:{session_id} ttl={ttl}s");
        }
        5 => {
            let items = rng.gen_range(100, 5000);
            let elapsed = rng.gen_range(50, 2000);
            let _ = write!(
                buf,
                "배치 작업 동기화 완료 processed_items={items} failed_items=0 elapsed={elapsed}ms"
            );
        }
        6 => {
            let tx_id = rng.gen_range(10000000, 99999999);
            let _ = write!(
                buf,
                "외부 결제 게이트웨이 응답 수신 transaction_id=tx-{tx_id} status=APPROVED"
            );
        }
        7 => {
            let partition = rng.gen_range(0, 8);
            let offset = rng.gen_range(100000, 9999999);
            let _ = write!(
                buf,
                "Kafka 메시지 소비 완료 topic=events partition={partition} offset={offset}"
            );
        }
        8 => {
            let sess = rng.gen_range(10000, 99999);
            let _ = write!(buf, "세션 만료 감지 session_id=sess-{sess}");
        }
        9 => {
            let file_id = rng.gen_range(1000, 9999);
            let size = rng.gen_range(1024, 10485760);
            let _ = write!(
                buf,
                "파일 업로드 완료 filename=doc_{file_id}.pdf size={size} bytes"
            );
        }
        10 => {
            let ip_last = rng.gen_range(1, 254);
            let _ = write!(
                buf,
                "API 요청 인증 실패 ip=192.168.1.{ip_last} reason=invalid_token"
            );
        }
        _ => {
            let query_hash = rng.next_u32();
            let duration = rng.gen_range(500, 3000);
            let _ = write!(
                buf,
                "슬로우 쿼리 경고 query_hash=0x{query_hash:08x} duration={duration}ms threshold=500ms"
            );
        }
    }
}

/// 긴 줄 메시지 작성 (20KB~200KB, minified JSON 형식에 한글 포함하여 16KB 경계 검증)
fn write_long_message(buf: &mut Vec<u8>, rng: &mut Rng) {
    let target_len = rng.gen_range(20 * 1024, 200 * 1024) as usize;
    let batch_id = rng.gen_range(10000, 99999);
    let _ = write!(
        buf,
        r#"페이로드 덤프: {{"batch_id":{batch_id},"meta":"한글_로그_페이로드_검증용_대용량_JSON_데이터","items":["#
    );
    let mut item_idx = 0;
    while buf.len() < target_len {
        if item_idx > 0 {
            buf.push(b',');
        }
        let item_id = rng.gen_range(100000, 999999);
        let price = rng.gen_range(1000, 500000);
        // 한글과 영문이 섞인 JSON 객체를 반복 추가하여 16KB(16384 바이트) 경계에 3바이트 UTF-8 문자가 자연스럽게 걸치도록 함
        let _ = write!(
            buf,
            r#"{{"id":{item_id},"name":"상품_{item_idx}","desc":"상세설명_{item_id}_테스트데이터","price":{price},"status":"정상처리"}}"#
        );
        item_idx += 1;
    }
    let _ = write!(buf, "]}}");
}

/// 크기 인자 문자열 파싱 (예: 500M, 10G, 1024K, 1073741824)
fn parse_size(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }

    let s_lower = trimmed.to_ascii_lowercase();
    let (num_str, multiplier) = if s_lower.ends_with("tib") || s_lower.ends_with("tb") {
        let end = if s_lower.ends_with("tib") {
            s_lower.len() - 3
        } else {
            s_lower.len() - 2
        };
        (&trimmed[..end], 1024 * 1024 * 1024 * 1024)
    } else if s_lower.ends_with('t') {
        (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024 * 1024)
    } else if s_lower.ends_with("gib") || s_lower.ends_with("gb") {
        let end = if s_lower.ends_with("gib") {
            s_lower.len() - 3
        } else {
            s_lower.len() - 2
        };
        (&trimmed[..end], 1024 * 1024 * 1024)
    } else if s_lower.ends_with('g') {
        (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024)
    } else if s_lower.ends_with("mib") || s_lower.ends_with("mb") {
        let end = if s_lower.ends_with("mib") {
            s_lower.len() - 3
        } else {
            s_lower.len() - 2
        };
        (&trimmed[..end], 1024 * 1024)
    } else if s_lower.ends_with('m') {
        (&trimmed[..trimmed.len() - 1], 1024 * 1024)
    } else if s_lower.ends_with("kib") || s_lower.ends_with("kb") {
        let end = if s_lower.ends_with("kib") {
            s_lower.len() - 3
        } else {
            s_lower.len() - 2
        };
        (&trimmed[..end], 1024)
    } else if s_lower.ends_with('k') {
        (&trimmed[..trimmed.len() - 1], 1024)
    } else {
        (trimmed, 1)
    };

    let num: u64 = num_str.trim().parse().ok()?;
    num.checked_mul(multiplier)
}

fn print_usage_and_exit(prog_name: &str) -> ! {
    eprintln!("사용법: {prog_name} <출력경로> <크기> [시드]");
    eprintln!("  출력경로: 생성할 로그 파일 경로 (예: testdata/sample_1g.log)");
    eprintln!("  크기:     생성할 파일 크기 (예: 500M, 10G, 1024K, 1073741824)");
    eprintln!("  시드:     (선택) 난수 시드값 (기본값: 42)");
    process::exit(1);
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let prog_name = args.first().map(|s| s.as_str()).unwrap_or("gen_log");

    if args.len() < 3 || args.len() > 4 {
        print_usage_and_exit(prog_name);
    }

    let output_path = Path::new(&args[1]);
    let target_bytes = match parse_size(&args[2]) {
        Some(b) if b > 0 => b,
        _ => {
            eprintln!("오류: 올바르지 않은 크기 형식입니다: '{}'", args[2]);
            print_usage_and_exit(prog_name);
        }
    };

    let seed = if args.len() == 4 {
        match args[3].parse::<u64>() {
            Ok(s) => s,
            Err(_) => {
                eprintln!("오류: 시드는 양의 정수여야 합니다: '{}'", args[3]);
                print_usage_and_exit(prog_name);
            }
        }
    } else {
        42 // 기본 고정 시드
    };

    // 출력 디렉터리가 없으면 자동 생성
    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let file = File::create(output_path)?;
    // 고속 I/O를 위해 8MB 대용량 버퍼 사용
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);

    let mut rng = Rng::new(seed);
    // 기준 시각: 2026-11-12T07:15:42.123Z (밀리초 단위)
    let mut current_time_ms: u64 = 1_794_467_742_123;

    let mut written_bytes: u64 = 0;
    let mut total_lines: u64 = 0;
    let mut line_buf = Vec::with_capacity(256 * 1024);

    let start_time = Instant::now();
    let mut next_report_bytes: u64 = 1024 * 1024 * 1024; // 1GB 단위로 진행 상황 출력

    while written_bytes < target_bytes {
        line_buf.clear();

        // 1. 타임스탬프 (단조 증가: 매 줄마다 1~24ms 증가)
        current_time_ms += rng.gen_range(1, 25);
        format_iso8601(current_time_ms, &mut line_buf);
        line_buf.push(b' ');

        // 2. 로그 레벨 (INFO 가 대부분)
        let level = pick_level(&mut rng);
        line_buf.extend_from_slice(level.as_bytes());
        line_buf.push(b' ');

        // 3. 모듈/스레드
        let module = pick_module(&mut rng);
        let _ = write!(&mut line_buf, "[{module}] ");

        // 4. 메시지 (약 1/2000 확률로 20KB~200KB 긴 줄 생성, 그 외 일반 메시지)
        let is_long = rng.next_u32().is_multiple_of(2000);
        if is_long {
            write_long_message(&mut line_buf, &mut rng);
        } else {
            write_normal_message(&mut line_buf, &mut rng);
        }

        // 5. 개행 (약 1/500 확률로 CRLF, 그 외 LF)
        let is_crlf = rng.next_u32().is_multiple_of(500);
        if is_crlf {
            line_buf.extend_from_slice(b"\r\n");
        } else {
            line_buf.push(b'\n');
        }

        writer.write_all(&line_buf)?;
        written_bytes += line_buf.len() as u64;
        total_lines += 1;

        // 1GB마다 stderr에 진행 상황 출력
        if written_bytes >= next_report_bytes {
            let elapsed_sec = start_time.elapsed().as_secs_f64();
            let speed_mb = (written_bytes as f64 / (1024.0 * 1024.0)) / elapsed_sec;
            let written_gb = written_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            let target_gb = target_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
            let pct = (written_bytes as f64 / target_bytes as f64) * 100.0;
            eprintln!(
                "진행 상황: {:.2} GB / {:.2} GB ({:.1}%) - {:.1} MB/s (줄 수: {})",
                written_gb, target_gb, pct, speed_mb, total_lines
            );
            next_report_bytes += 1024 * 1024 * 1024;
        }
    }

    writer.flush()?;

    let elapsed = start_time.elapsed();
    let total_mb = written_bytes as f64 / (1024.0 * 1024.0);
    let speed_mb = total_mb / elapsed.as_secs_f64().max(0.0001);
    eprintln!(
        "생성 완료: {} ({} 바이트, {} 줄) 소요 시간: {:.2?} ({:.1} MB/s)",
        output_path.display(),
        written_bytes,
        total_lines,
        elapsed,
        speed_mb
    );

    Ok(())
}
