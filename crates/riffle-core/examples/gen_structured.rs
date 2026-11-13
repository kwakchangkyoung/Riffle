//! 컬럼 뷰 검증용 구조화 로그 생성기 (CSV / JSON Lines).
//! 사용법: gen_structured <출력경로> <크기> csv|jsonl [시드]
//! 생성되는 형식은 detect_columns이 감지할 수 있는 형태다:
//! - csv: 첫 줄 헤더 + time,level,status,user,latency,msg 컬럼
//! - jsonl: 한 줄에 {"ts","level","status","user","latency","msg"} 객체 하나

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
        Self {
            state: if seed == 0 {
                0x853c_49e6_748f_ea9b
            } else {
                seed
            },
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

/// 로그 레벨 선택 (info가 대부분, error는 드묾 — 필드 필터 선택도 차이용)
fn pick_level(rng: &mut Rng) -> &'static str {
    match rng.next_u32() % 100 {
        r if r < 70 => "info",
        r if r < 85 => "debug",
        r if r < 95 => "warn",
        _ => "error",
    }
}

/// HTTP 상태 코드 선택 (500은 드묾 — status=500 필드 필터 선택도 차이용)
fn pick_status(rng: &mut Rng) -> u64 {
    match rng.next_u32() % 100 {
        r if r < 80 => 200,
        r if r < 90 => 304,
        r if r < 97 => 404,
        r if r < 99 => 500,
        _ => 503,
    }
}

/// 메시지 본문. status와 무관하게 숫자(500 포함)가 섞여 있어
/// 줄 전체 매치와 컬럼 매치의 결과 차이를 만든다.
fn pick_msg(rng: &mut Rng) -> String {
    match rng.next_u32() % 6 {
        0 => format!("요청 처리 완료 user_req={}", rng.gen_range(10000, 99999)),
        1 => format!(
            "upstream=10.0.0.{} bytes={}",
            rng.gen_range(1, 254),
            rng.gen_range(100, 99999)
        ),
        2 => format!(
            "timeout={}ms retry={}",
            rng.gen_range(100, 5000),
            rng.gen_range(0, 3)
        ),
        3 => format!("port={} conn reuse", rng.gen_range(1024, 65000)),
        4 => format!("code_path=handler/{}", rng.gen_range(100, 999)),
        _ => format!("결제 승인 amount={} KRW", rng.gen_range(1000, 500000)),
    }
}

/// CSV 한 줄 작성. 따옴표/구분자 테스트를 위해 가끔 따옴표 안 쉼표를 넣는다.
fn write_csv_line(buf: &mut Vec<u8>, rng: &mut Rng, ts_ms: u64) {
    let level = pick_level(rng);
    let status = pick_status(rng);
    let user = rng.gen_range(1000, 99999);
    let latency = rng.gen_range(1, 3000);
    let msg = pick_msg(rng);
    // 약 1/100 확률로 따옴표 안 쉼표 필드 (구분자 인식 분할 검증용)
    let msg_field = if rng.next_u32().is_multiple_of(100) {
        format!("\"{}, trailing\"", msg)
    } else {
        msg
    };
    let _ = writeln!(
        buf,
        "{ts_ms},{level},{status},u{user},{latency},{msg_field}"
    );
}

/// JSONL 한 줄 작성 (compact JSON).
fn write_jsonl_line(buf: &mut Vec<u8>, rng: &mut Rng, ts_ms: u64) {
    let level = pick_level(rng);
    let status = pick_status(rng);
    let user = rng.gen_range(1000, 99999);
    let latency = rng.gen_range(1, 3000);
    let msg = pick_msg(rng);
    // 가끔 중첩 객체·배열 키와 names에 없는 키를 섞는다
    let extra = if rng.next_u32().is_multiple_of(50) {
        format!(
            r#","tags":["a","b"],"meta":{{"node":{}}}"#,
            rng.gen_range(0, 8)
        )
    } else {
        String::new()
    };
    let _ = writeln!(
        buf,
        r#"{{"ts":{ts_ms},"level":"{level}","status":{status},"user":"u{user}","latency":{latency},"msg":"{msg}"{extra}}}"#
    );
}

/// 크기 인자 문자열 파싱 (예: 500M, 1G, 1024K, 1073741824)
fn parse_size(s: &str) -> Option<u64> {
    let trimmed = s.trim();
    let s_lower = trimmed.to_ascii_lowercase();
    let (num_str, multiplier) = if s_lower.ends_with('g') {
        (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024)
    } else if s_lower.ends_with('m') {
        (&trimmed[..trimmed.len() - 1], 1024 * 1024)
    } else if s_lower.ends_with('k') {
        (&trimmed[..trimmed.len() - 1], 1024)
    } else {
        (trimmed, 1)
    };
    num_str.trim().parse::<u64>().ok()?.checked_mul(multiplier)
}

fn print_usage_and_exit(prog_name: &str) -> ! {
    eprintln!("사용법: {prog_name} <출력경로> <크기> csv|jsonl [시드]");
    eprintln!("  크기: 생성할 파일 크기 (예: 500M, 1G, 1024K, 1073741824)");
    process::exit(1);
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let prog_name = args.first().map(|s| s.as_str()).unwrap_or("gen_structured");
    if args.len() < 4 || args.len() > 5 {
        print_usage_and_exit(prog_name);
    }

    let output_path = Path::new(&args[1]);
    let target_bytes = match parse_size(&args[2]) {
        Some(b) if b > 0 => b,
        _ => print_usage_and_exit(prog_name),
    };
    let is_csv = match args[3].as_str() {
        "csv" => true,
        "jsonl" => false,
        _ => print_usage_and_exit(prog_name),
    };
    let seed: u64 = if args.len() == 5 {
        args[4]
            .parse()
            .unwrap_or_else(|_| print_usage_and_exit(prog_name))
    } else {
        42
    };

    if let Some(parent) = output_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let file = File::create(output_path)?;
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);
    let mut rng = Rng::new(seed);
    let start_time = Instant::now();

    // 가상의 밀리초 타임스탬프 기준 시각 (단조 증가)
    let mut ts_ms: u64 = 1_794_467_742_123;
    let mut written_bytes: u64 = 0;
    let mut total_lines: u64 = 0;
    let mut line_buf = Vec::with_capacity(1024);

    // csv는 헤더 줄로 시작한다 (첫 줄이 헤더로 감지되도록)
    if is_csv {
        writer.write_all(b"time,level,status,user,latency,msg\n")?;
        written_bytes += 26;
    }

    while written_bytes < target_bytes {
        line_buf.clear();
        ts_ms += rng.gen_range(1, 25);
        if is_csv {
            write_csv_line(&mut line_buf, &mut rng, ts_ms);
        } else {
            write_jsonl_line(&mut line_buf, &mut rng, ts_ms);
        }
        writer.write_all(&line_buf)?;
        written_bytes += line_buf.len() as u64;
        total_lines += 1;
    }
    writer.flush()?;

    eprintln!(
        "생성 완료: {} ({} 바이트, {} 줄, {}) 소요 시간: {:.2?}",
        output_path.display(),
        written_bytes,
        total_lines,
        if is_csv { "csv" } else { "jsonl" },
        start_time.elapsed()
    );
    Ok(())
}
