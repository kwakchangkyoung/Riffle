use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use riffle_core::LogFile;
use std::env;
use std::fs::{self, File};
use std::hint::black_box;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// 벤치마크용 간단한 xorshift64 난수 생성기
struct BenchRng {
    state: u64,
}

impl BenchRng {
    fn new(seed: u64) -> Self {
        let non_zero = if seed == 0 {
            0x853c_49e6_748f_ea9b
        } else {
            seed
        };
        Self { state: non_zero }
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

/// 벤치마크 대상 로그 파일(약 256MB) 생성 또는 기존 파일 경로 반환
fn prepare_bench_file() -> PathBuf {
    // 환경변수 RIFFLE_BENCH_FILE이 지정되어 있으면 해당 파일 사용
    if let Ok(path_str) = env::var("RIFFLE_BENCH_FILE") {
        let path = PathBuf::from(path_str);
        if path.exists() {
            eprintln!("RIFFLE_BENCH_FILE 사용: {}", path.display());
            return path;
        }
    }

    // 기본 임시 디렉터리에 256MB 파일 생성
    let target_path = env::temp_dir().join("riffle_bench_256m.log");
    let target_bytes: u64 = 256 * 1024 * 1024;

    // 이미 올바른 크기로 생성되어 있다면 재사용
    if let Ok(metadata) = fs::metadata(&target_path)
        && metadata.len() >= target_bytes
    {
        return target_path;
    }

    eprintln!(
        "벤치마크용 256MB 로그 파일 생성 중: {}",
        target_path.display()
    );

    let file = File::create(&target_path).expect("벤치마크 파일 생성 실패");
    let mut writer = BufWriter::with_capacity(8 * 1024 * 1024, file);
    let mut rng = BenchRng::new(42);

    let mut written_bytes: u64 = 0;
    let mut line_buf = Vec::with_capacity(64 * 1024);
    let mut time_ms: u64 = 1_794_467_742_123;

    while written_bytes < target_bytes {
        line_buf.clear();
        time_ms += rng.gen_range(1, 20);

        // 기본 로그 라인 작성
        let user_id = rng.gen_range(10000, 99999);
        let latency = rng.gen_range(1, 100);
        let _ = write!(
            &mut line_buf,
            "2026-11-12T07:15:42.{:03}Z INFO  [worker-1] 사용자 로그인 성공 user_id={} latency={}ms",
            time_ms % 1000,
            user_id,
            latency
        );

        // 간헐적으로 긴 줄 및 CRLF 적용 (약 1/500 줄 CRLF)
        if rng.next_u32().is_multiple_of(500) {
            line_buf.extend_from_slice(b"\r\n");
        } else {
            line_buf.push(b'\n');
        }

        writer.write_all(&line_buf).expect("파일 쓰기 실패");
        written_bytes += line_buf.len() as u64;
    }

    writer.flush().expect("파일 플러시 실패");
    eprintln!("벤치마크 로그 생성 완료 ({} 바이트)", written_bytes);

    target_path
}

/// 인덱스 빌드(LogFile::build_index) 성능 측정
fn bench_build_index(c: &mut Criterion) {
    let bench_path = prepare_bench_file();
    let file_size = fs::metadata(&bench_path).map(|m| m.len()).unwrap_or(0);

    let mut group = c.benchmark_group("index_build");
    // 빠른 측정을 위해 표본 수와 측정 시간 축소 (2분 이내 완료 목표)
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Bytes(file_size));

    group.bench_function("build_index_256m", |b| {
        b.iter_batched(
            || LogFile::open(&bench_path).expect("LogFile::open 실패"),
            |log_file| {
                let cancel = AtomicBool::new(false);
                log_file
                    .build_index(&cancel, &mut |_| {})
                    .expect("build_index 실패");
            },
            BatchSize::LargeInput,
        );
    });

    group.finish();
}

/// 임의 위치 100줄 읽기(LogFile::read_lines) 성능 측정
fn bench_read_lines(c: &mut Criterion) {
    let bench_path = prepare_bench_file();
    let log_file = LogFile::open(&bench_path).expect("LogFile::open 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("build_index 실패");

    let total_lines = log_file.index().line_count();
    let max_start = total_lines.saturating_sub(100);

    let mut group = c.benchmark_group("read_lines");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(5));

    let mut rng = BenchRng::new(12345);
    group.bench_function("read_100_lines_random", |b| {
        b.iter(|| {
            let start = rng.gen_range(0, max_start.max(1));
            let lines = log_file
                .read_lines(start, 100, None, None)
                .expect("read_lines 실패");
            black_box(lines);
        });
    });

    group.finish();
}

/// 임의 바이트 오프셋의 줄 번호 검색(LineIndex::line_of_offset) 성능 측정
fn bench_line_of_offset(c: &mut Criterion) {
    let bench_path = prepare_bench_file();
    let log_file = LogFile::open(&bench_path).expect("LogFile::open 실패");
    let cancel = AtomicBool::new(false);
    log_file
        .build_index(&cancel, &mut |_| {})
        .expect("build_index 실패");

    let file_size = log_file.source().len();

    let mut group = c.benchmark_group("line_of_offset");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(5));

    let mut rng = BenchRng::new(67890);
    group.bench_function("line_of_offset_random", |b| {
        b.iter(|| {
            let offset = rng.gen_range(0, file_size.max(1));
            let line_no = log_file.index().line_of_offset(log_file.source(), offset);
            black_box(line_no);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_build_index,
    bench_read_lines,
    bench_line_of_offset
);
criterion_main!(benches);
