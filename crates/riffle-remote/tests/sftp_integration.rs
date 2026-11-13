//! `/usr/libexec/sftp-server` 를 직접 자식 프로세스로 띄워
//! `Transport::Command` 경로의 SFTP 통신을 검증하는 통합 테스트.
//!
//! 각 테스트는 자기만의 SftpPool(=전용 런타임+연결)을 만들고,
//! `sftp://local/...` URI 로 로컬 파일을 "원격"처럼 읽는다.

use std::fs;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use riffle_core::LogFile;
use riffle_remote::{RemoteError, RemoteUri, SftpPool, Transport};

/// 결정적 의사난수 바이트열. 내용 비교용이라 암호학적 품질은 필요 없다.
fn gen_bytes(len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    while v.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(len);
    v
}

fn tempdir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "riffle-sftp-test-{}-{}-{}",
        tag,
        std::process::id(),
        id
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 일반 sftp-server 를 쓰는 풀.
fn pool() -> SftpPool {
    SftpPool::new(Transport::Command(vec!["/usr/libexec/sftp-server".into()]))
}

/// sftp-server 의 PID 를 파일에 기록하는 래퍼를 쓰는 풀.
/// `sh -c 'echo $$ > pidfile; exec sftp-server'` — exec 후 $$ 가 sftp-server PID 가 된다.
fn pool_with_pidfile(dir: &Path) -> (SftpPool, PathBuf) {
    let pidfile = dir.join("sftp.pid");
    let script = format!(
        "echo $$ > '{}'; exec /usr/libexec/sftp-server",
        pidfile.display()
    );
    (
        SftpPool::new(Transport::Command(vec![
            "/bin/sh".into(),
            "-c".into(),
            script,
        ])),
        pidfile,
    )
}

fn read_pidfile(pidfile: &Path) -> u32 {
    wait_pidfile(pidfile, None)
}

/// pidfile 의 PID 가 `not` 과 달라질 때까지 기다렸다가 반환한다.
/// `not` 이 None 이면 아무 PID 나 첫 등장 시 반환한다.
fn wait_pidfile(pidfile: &Path, not: Option<u32>) -> u32 {
    for _ in 0..100 {
        if let Ok(text) = fs::read_to_string(pidfile)
            && let Ok(pid) = text.trim().parse::<u32>()
            && Some(pid) != not
        {
            return pid;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("pid 파일을 읽지 못했습니다: {}", pidfile.display());
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn uri_for(path: &Path) -> RemoteUri {
    // Transport::Command 는 host 를 무시하므로 "local" 고정.
    RemoteUri::parse(&format!("sftp://local{}", path.display())).unwrap()
}

/// 로컬 파일의 [offset, offset+len) 를 std read_at 으로 읽어 기대값을 만든다.
fn expected_at(path: &Path, offset: u64, len: usize) -> Vec<u8> {
    let file = fs::File::open(path).unwrap();
    let mut buf = vec![0u8; len];
    let n = file.read_at(&mut buf, offset).unwrap();
    buf.truncate(n);
    buf
}

fn assert_read_matches(
    remote: &dyn riffle_remote::RandomRead,
    path: &Path,
    offset: u64,
    len: usize,
) {
    let mut buf = vec![0u8; len];
    let n = remote.read_at(&mut buf, offset).expect("read_at 실패");
    let expected = expected_at(path, offset, len);
    assert_eq!(
        n,
        expected.len(),
        "읽은 길이 불일치 (off={offset} len={len})"
    );
    buf.truncate(n);
    assert_eq!(buf, expected, "내용 불일치 (off={offset} len={len})");
}

#[test]
fn read_at_various_offsets() {
    let dir = tempdir("offsets");
    let path = dir.join("data.bin");
    // 블록 경계(256KiB)를 여러 번 넘는 크기.
    let content = gen_bytes(3 * 1024 * 1024 + 12345);
    fs::write(&path, &content).unwrap();

    let p = pool();
    let remote = p.open_file(&uri_for(&path)).expect("open_file 실패");
    // 블록 경계를 걸치는 조합들.
    for (offset, len) in [
        (0u64, 1usize),
        (0, 100),
        (1, 65536),
        (1023, 4097),
        (256 * 1024 - 10, 100),   // 블록 경계 직전
        (256 * 1024, 256 * 1024), // 블록 정렬 큰 읽기(large 경로 직전)
        (256 * 1024 + 777, 99999),
        (1024 * 1024 - 1, 3 * 1024 * 1024), // 큰 읽기 경로(>1MiB)
        (content.len() as u64 - 10, 100),   // EOF 근처
        (content.len() as u64, 100),        // 정확히 EOF
        (content.len() as u64 + 100, 100),  // EOF 너머
    ] {
        assert_read_matches(remote.as_ref(), &path, offset, len);
    }
    p.release("local");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_at_32mb_large() {
    let dir = tempdir("large");
    let path = dir.join("big.bin");
    let content = gen_bytes(34 * 1024 * 1024);
    fs::write(&path, &content).unwrap();

    let p = pool();
    let remote = p.open_file(&uri_for(&path)).unwrap();
    // 32MiB 를 넘는 단일 read_at — 파이프라이닝 경로 검증.
    assert_read_matches(remote.as_ref(), &path, 1024 * 1024, 32 * 1024 * 1024 + 777);
    // 파일 끝을 넘는 큰 읽기: 나머지만 반환돼야 한다.
    let mut buf = vec![0u8; 40 * 1024 * 1024];
    let n = remote
        .read_at(&mut buf, 30 * 1024 * 1024)
        .expect("read_at 실패");
    assert_eq!(n as u64, content.len() as u64 - 30 * 1024 * 1024);
    buf.truncate(n);
    assert_eq!(
        buf,
        expected_at(&path, 30 * 1024 * 1024, n),
        "큰 읽기 내용 불일치"
    );
    p.release("local");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn stat_len_and_uri() {
    let dir = tempdir("stat");
    let path = dir.join("stat.bin");
    fs::write(&path, gen_bytes(123456)).unwrap();

    let p = pool();
    let remote = p.open_file(&uri_for(&path)).unwrap();
    assert_eq!(remote.stat_len().unwrap(), 123456);
    assert_eq!(remote.uri(), format!("sftp://local{}", path.display()));
    p.release("local");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_via_logfile_index_and_lines() {
    let dir = tempdir("logfile");
    let path = dir.join("app.log");
    let mut content = String::new();
    for i in 0..20000u64 {
        content.push_str(&format!(
            "2026-11-13T00:{:02}:{:02} INFO worker-{} processed item {:05} 메시지\n",
            i % 60,
            i % 60,
            i % 8,
            i
        ));
    }
    content.push_str("마지막 줄 개행 없음");
    fs::write(&path, &content).unwrap();

    // 로컬 기준값.
    let local = LogFile::open(&path).unwrap();
    local
        .build_index(&AtomicBool::new(false), &mut |_| {})
        .unwrap();

    let p = pool();
    let remote_src = p.open_file(&uri_for(&path)).unwrap();
    let remote = LogFile::open_remote(remote_src).unwrap();
    assert_eq!(
        remote.uri().as_deref(),
        Some(format!("sftp://local{}", path.display()).as_str())
    );
    remote
        .build_index(&AtomicBool::new(false), &mut |_| {})
        .unwrap();

    assert_eq!(remote.index().line_count(), local.index().line_count());
    for (start, count) in [(0u64, 50u64), (10000, 30), (19990, 20)] {
        let r = remote.read_lines(start, count, None, None).unwrap();
        let l = local.read_lines(start, count, None, None).unwrap();
        assert_eq!(r.len(), l.len(), "줄 수 불일치 ({start}+{count})");
        for (rl, ll) in r.iter().zip(l.iter()) {
            assert_eq!(rl.text, ll.text, "줄 {} 내용 불일치", rl.no);
        }
    }
    p.release("local");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn list_dir_and_home() {
    let dir = tempdir("ls");
    fs::write(dir.join("b.txt"), b"b").unwrap();
    fs::create_dir(dir.join("subdir")).unwrap();
    fs::write(dir.join("a.txt"), b"a").unwrap();

    let p = pool();
    let entries = p.list_dir("local", &dir.to_string_lossy()).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    // 디렉터리 우선 + 이름순, . / .. 제외.
    assert_eq!(names, vec!["subdir", "a.txt", "b.txt"]);
    assert!(entries[0].is_dir);
    assert!(!entries[1].is_dir);
    assert_eq!(entries[1].size, 1);
    assert!(entries[1].mtime.is_some());

    let home = p.home_dir("local").unwrap();
    assert!(
        Path::new(&home).is_absolute(),
        "home 이 절대경로가 아님: {home}"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn open_nonexistent_is_not_found() {
    let dir = tempdir("nf");
    let missing = dir.join("no_such_file.log");
    let p = pool();
    let Err(err) = p.open_file(&uri_for(&missing)) else {
        panic!("존재하지 않는 파일이 열렸습니다");
    };
    assert!(
        matches!(err, RemoteError::NotFound(_)),
        "NotFound 기대, got {err:?}"
    );
    // 연결은 파일 수 0으로 남아 유휴 회수 대상이 된다.
    p.release("local");
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn release_terminates_process() {
    let dir = tempdir("rel");
    let path = dir.join("f.bin");
    fs::write(&path, gen_bytes(4096)).unwrap();

    let (p, pidfile) = pool_with_pidfile(&dir);
    let remote = p.open_file(&uri_for(&path)).unwrap();
    let pid = read_pidfile(&pidfile);
    assert!(pid_alive(pid));
    remote.read_at(&mut [0u8; 16], 0).unwrap();

    p.release("local");
    // shutdown 은 start_kill + wait 를 수행하므로 release 반환 시점에 이미 회수됐다.
    assert!(
        !pid_alive(pid),
        "release 후에도 sftp-server(pid {pid}) 생존"
    );
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn reconnect_after_server_kill() {
    let dir = tempdir("reconn");
    let path = dir.join("f.bin");
    let content = gen_bytes(2 * 1024 * 1024);
    fs::write(&path, &content).unwrap();

    let (p, pidfile) = pool_with_pidfile(&dir);
    let remote = p.open_file(&uri_for(&path)).unwrap();
    let mut buf = [0u8; 100];
    remote.read_at(&mut buf, 0).unwrap();
    let pid1 = read_pidfile(&pidfile);

    // sftp-server 를 강제 종료 → 다음 read 가 1회 재연결을 시도해야 한다.
    let status = std::process::Command::new("kill")
        .args(["-9", &pid1.to_string()])
        .status()
        .unwrap();
    assert!(status.success());

    // 재연결 + 재오픈 후 같은 데이터를 읽을 수 있어야 한다.
    let mut buf = vec![0u8; 500_000];
    let n = remote
        .read_at(&mut buf, 1024)
        .expect("재연결 후 read_at 실패");
    assert_eq!(n, buf.len());
    buf.truncate(n);
    assert_eq!(buf, expected_at(&path, 1024, n));

    // 새 서버 프로세스가 떴는지 확인 (pidfile 이 덮어써진다).
    let pid2 = wait_pidfile(&pidfile, Some(pid1));
    assert!(pid_alive(pid2));

    p.release("local");
    fs::remove_dir_all(&dir).ok();
}

/// 성능 측정(보고용). `cargo test -p riffle-remote --test sftp_integration perf -- --ignored --nocapture`
#[test]
#[ignore]
fn perf_sample_1g() {
    let sample = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/sample_1g.log");
    if !sample.exists() {
        eprintln!("skip: {} 없음", sample.display());
        return;
    }

    // 로컬 직접 인덱싱.
    let t = Instant::now();
    let local = LogFile::open(&sample).unwrap();
    local
        .build_index(&AtomicBool::new(false), &mut |_| {})
        .unwrap();
    let local_elapsed = t.elapsed();
    let size = fs::metadata(&sample).unwrap().len();
    eprintln!(
        "local  : {:?} ({:.1} MB/s)",
        local_elapsed,
        size as f64 / local_elapsed.as_secs_f64() / 1e6
    );

    // 원격(sftp-server 경유) 인덱싱.
    let p = pool();
    let t = Instant::now();
    let remote_src = p.open_file(&uri_for(&sample)).unwrap();
    let remote = LogFile::open_remote(remote_src).unwrap();
    remote
        .build_index(&AtomicBool::new(false), &mut |_| {})
        .unwrap();
    let remote_elapsed = t.elapsed();
    eprintln!(
        "remote : {:?} ({:.1} MB/s, local 대비 {:.2}x)",
        remote_elapsed,
        size as f64 / remote_elapsed.as_secs_f64() / 1e6,
        remote_elapsed.as_secs_f64() / local_elapsed.as_secs_f64()
    );
    assert_eq!(remote.index().line_count(), local.index().line_count());
    p.release("local");
}

/// 파이프라이닝 vs 순차 요청 비교(보고용). 같은 총량을
/// 256KiB 단위 순차 read_at 과 한 번의 큰 read_at 으로 읽어 시간을 비교한다.
#[test]
#[ignore]
fn perf_pipelined_vs_sequential() {
    let dir = tempdir("perf");
    let path = dir.join("seq.bin");
    let size = 64 * 1024 * 1024usize;
    let content = gen_bytes(size);
    fs::write(&path, &content).unwrap();

    let p = pool();
    let remote = p.open_file(&uri_for(&path)).unwrap();

    // 순차: 256KiB 씩 개별 read_at.
    let chunk = 256 * 1024usize;
    let mut buf = vec![0u8; chunk];
    let t = Instant::now();
    let mut off = 0u64;
    while off < size as u64 {
        remote.read_at(&mut buf, off).unwrap();
        off += chunk as u64;
    }
    let seq_elapsed = t.elapsed();
    eprintln!("sequential 256KiB x{}: {:?}", size / chunk, seq_elapsed);

    // 파이프라이닝: 한 번의 큰 read_at (내부에서 32개 동시 요청).
    let mut big = vec![0u8; size];
    let t = Instant::now();
    let n = remote.read_at(&mut big, 0).unwrap();
    let pipe_elapsed = t.elapsed();
    assert_eq!(n, size);
    assert_eq!(big, content);
    eprintln!("pipelined 64MiB single read_at: {:?}", pipe_elapsed);
    eprintln!(
        "speedup: {:.2}x",
        seq_elapsed.as_secs_f64() / pipe_elapsed.as_secs_f64()
    );

    p.release("local");
    fs::remove_dir_all(&dir).ok();
}
