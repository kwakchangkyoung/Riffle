//! riffle-agent: 원격 서버에 설치해 실행하는 서버 측 가속 프로세스.
//!
//! SFTP 만으로는 인덱싱·검색·필터에 파일 전체를 네트워크로 받아야 한다.
//! 에이전트는 그 작업을 원격에서 riffle-core 로 직접 수행하고 결과(체크포인트,
//! 줄 번호, 구간)만 돌려준다. 통신은 stdin/stdout 줄 단위 JSON 이다.
//!
//! 사용: `riffle-agent --stdio` (프로토콜 모드), `riffle-agent --version`.
//! stderr 는 로그 전용이다. 한 세션은 파일 하나를 대상으로 하며,
//! 인덱싱·검색·필터 작업은 각각 별도 스레드에서 동시에 진행될 수 있다.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use riffle_core::{
    Combine, FilterRule, FilterSet, FilterSpec, Level, LineSet, LogFile, MAX_FILTER_RANGES,
    Matcher, RuleKind, SearchOptions, detect_columns, filter_scan, is_gzip, search_filtered,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// 프로토콜 버전. 요청/응답 형식이 바뀌면 올린다.
const PROTOCOL_VERSION: u64 = 1;
/// 검색 결과 한 메시지(`batch`)에 담는 최대 줄 번호 수.
const SEARCH_BATCH_MAX: usize = 10_000;
/// 필터 결과 한 메시지(`ranges`)에 담는 최대 구간 수.
const RANGE_BATCH_MAX: usize = 10_000;
/// 인덱싱 진행률 메시지의 최소 전송 간격.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 클라이언트 요청 한 줄.
/// op 에 따라 쓰는 필드만 채워지고 나머지는 None 이다.
#[derive(Debug, Deserialize)]
struct Request {
    id: u64,
    op: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    query: Option<QueryJson>,
    /// search 의 필터 조건 (FilterSpecDto 형식, null 허용)
    #[serde(default)]
    filter: Option<Value>,
    /// filter op 의 조건 (FilterSpecDto 형식)
    #[serde(default)]
    spec: Option<Value>,
    /// cancel op 의 대상 작업 id
    #[serde(default)]
    target: Option<u64>,
}

/// search 요청의 query 객체. 앱의 QueryDto 와 같은 형식(camelCase).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueryJson {
    query: String,
    regex: bool,
    case_sensitive: bool,
}

/// FilterSpecDto 와 같은 형식의 규칙 DTO (필드명은 앱과 동일하게 유지).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilterRuleDto {
    pattern: String,
    regex: bool,
    case_sensitive: bool,
    /// "include" | "exclude"
    kind: String,
    enabled: bool,
    /// 프론트 표시용 색상. 에이전트는 판정에 쓰지 않고 파싱만 한다.
    #[serde(default)]
    #[allow(dead_code)]
    color: String,
    /// 컬럼 뷰 필드 필터: 지정된 컬럼 값에만 매치 (없으면 줄 전체)
    #[serde(default)]
    field: Option<String>,
}

/// FilterSpecDto 와 같은 형식의 필터 조건 DTO.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilterSpecDto {
    rules: Vec<FilterRuleDto>,
    /// "and" | "or"
    combine: String,
    /// "error" | "warn" | "info" | "debug" | "trace"
    levels: Vec<String>,
}

/// 진행 중인 세션 상태. 작업 스레드들이 Arc 로 공유한다.
/// W 는 응답을 쓸 대상(stdout BufWriter 또는 테스트용 버퍼).
struct Session<W: Write + Send> {
    /// open op 으로 연 파일 (세션당 하나)
    file: Mutex<Option<Arc<LogFile>>>,
    /// 작업 id → 취소 플래그
    jobs: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    /// 작업 스레드 핸들 (stdin EOF 시 회수용)
    handles: Mutex<Vec<JoinHandle<()>>>,
    out: Mutex<W>,
}

impl<W: Write + Send> Session<W> {
    /// 응답 한 줄을 쓴다. 줄 단위로 flush 되며 여러 스레드의 쓰기는 Mutex 로 직렬화된다.
    fn send(&self, v: Value) {
        let mut w = self.out.lock().unwrap();
        if serde_json::to_writer(&mut *w, &v).is_ok() {
            let _ = w.write_all(b"\n");
        }
        let _ = w.flush();
    }

    fn send_error(&self, id: u64, msg: impl Into<String>) {
        self.send(json!({"id": id, "error": msg.into()}));
    }

    /// 작업 id 에 취소 플래그를 등록하고 반환한다.
    fn register_job(&self, id: u64) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        self.jobs.lock().unwrap().insert(id, flag.clone());
        flag
    }

    /// 작업 종료 시 등록을 지운다.
    fn finish_job(&self, id: u64) {
        self.jobs.lock().unwrap().remove(&id);
    }

    fn file(&self) -> Option<Arc<LogFile>> {
        self.file.lock().unwrap().clone()
    }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("riffle-agent {VERSION} protocol {PROTOCOL_VERSION}");
        }
        Some("--stdio") => run_stdio(),
        _ => {
            eprintln!("사용법: riffle-agent --stdio | --version");
            std::process::exit(2);
        }
    }
}

/// stdio 프로토콜 루프. stdin EOF(채널 종료)까지 요청을 한 줄씩 처리한다.
fn run_stdio() {
    let session = Arc::new(Session {
        file: Mutex::new(None),
        jobs: Mutex::new(HashMap::new()),
        handles: Mutex::new(Vec::new()),
        out: Mutex::new(BufWriter::new(io::stdout())),
    });

    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if !line.trim().is_empty() {
                    dispatch_line(&session, &line);
                }
            }
        }
    }

    // stdin EOF: 진행 중인 작업을 모두 취소하고 스레드가 끝나길 기다린다.
    for flag in session.jobs.lock().unwrap().values() {
        flag.store(true, Ordering::Relaxed);
    }
    for h in session.handles.lock().unwrap().drain(..) {
        let _ = h.join();
    }
}

/// 요청 한 줄을 파싱해 처리한다.
/// 잘못된 JSON 은 id 를 알 수 없으므로 id=0 으로 오류를 보내고 계속 동작한다.
fn dispatch_line<W: Write + Send + 'static>(session: &Arc<Session<W>>, line: &str) {
    let req = match serde_json::from_str::<Request>(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            session.send_error(0, format!("잘못된 요청입니다: {e}"));
            return;
        }
    };
    dispatch(session, req);
}

fn dispatch<W: Write + Send + 'static>(session: &Arc<Session<W>>, req: Request) {
    let id = req.id;
    match req.op.as_str() {
        "hello" => session
            .send(json!({"id": id, "hello": {"version": VERSION, "protocol": PROTOCOL_VERSION}})),
        "open" => handle_open(session, req),
        "index" => {
            let Some(file) = session.file() else {
                session.send_error(id, "파일이 열려 있지 않습니다");
                return;
            };
            let cancel = session.register_job(id);
            let s = session.clone();
            spawn_job(session, move || job_index(s, id, file, cancel));
        }
        "search" => {
            let Some(file) = session.file() else {
                session.send_error(id, "파일이 열려 있지 않습니다");
                return;
            };
            let Some(q) = req.query else {
                session.send_error(id, "query 필드가 필요합니다");
                return;
            };
            let matcher = match Matcher::new(
                &q.query,
                SearchOptions {
                    regex: q.regex,
                    case_sensitive: q.case_sensitive,
                },
            ) {
                Ok(m) => m,
                Err(e) => {
                    session.send_error(id, e.to_string());
                    return;
                }
            };
            let spec = match req.filter {
                Some(v) if !v.is_null() => match spec_from_value(v) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        session.send_error(id, e);
                        return;
                    }
                },
                _ => None,
            };
            let cancel = session.register_job(id);
            let s = session.clone();
            spawn_job(session, move || {
                job_search(s, id, file, matcher, spec, cancel)
            });
        }
        "filter" => {
            let Some(file) = session.file() else {
                session.send_error(id, "파일이 열려 있지 않습니다");
                return;
            };
            let Some(v) = req.spec else {
                session.send_error(id, "spec 필드가 필요합니다");
                return;
            };
            let spec = match spec_from_value(v) {
                Ok(s) => s,
                Err(e) => {
                    session.send_error(id, e);
                    return;
                }
            };
            let cancel = session.register_job(id);
            let s = session.clone();
            spawn_job(session, move || job_filter(s, id, file, spec, cancel));
        }
        "cancel" => {
            if let Some(target) = req.target
                && let Some(flag) = session.jobs.lock().unwrap().get(&target)
            {
                flag.store(true, Ordering::Relaxed);
            }
        }
        other => session.send_error(id, format!("알 수 없는 op: {other}")),
    }
}

fn spawn_job<W: Write + Send + 'static>(
    session: &Arc<Session<W>>,
    job: impl FnOnce() + Send + 'static,
) {
    session
        .handles
        .lock()
        .unwrap()
        .push(std::thread::spawn(job));
}

fn handle_open<W: Write + Send>(session: &Arc<Session<W>>, req: Request) {
    let Some(path) = req.path else {
        session.send_error(req.id, "path 필드가 필요합니다");
        return;
    };
    let p = Path::new(&path);
    match LogFile::open(p) {
        Ok(file) => {
            let size = file.source().len();
            let gzip = is_gzip(p).unwrap_or(false);
            *session.file.lock().unwrap() = Some(Arc::new(file));
            session.send(json!({"id": req.id, "opened": {"size": size, "gzip": gzip}}));
        }
        Err(e) => session.send_error(req.id, e.to_string()),
    }
}

/// FilterSpecDto JSON 을 코어 FilterSpec 으로 변환한다.
/// 알 수 없는 kind/combine/level 문자열은 Err 를 반환한다.
fn spec_from_value(v: Value) -> Result<FilterSpec, String> {
    let dto: FilterSpecDto =
        serde_json::from_value(v).map_err(|e| format!("필터 조건 형식 오류: {e}"))?;
    let rules = dto
        .rules
        .into_iter()
        .map(|r| {
            let kind = match r.kind.as_str() {
                "include" => RuleKind::Include,
                "exclude" => RuleKind::Exclude,
                other => return Err(format!("알 수 없는 필터 규칙 종류: '{other}'")),
            };
            Ok(FilterRule {
                pattern: r.pattern,
                options: SearchOptions {
                    regex: r.regex,
                    case_sensitive: r.case_sensitive,
                },
                kind,
                enabled: r.enabled,
                field: r.field,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let combine = match dto.combine.as_str() {
        "and" => Combine::And,
        "or" => Combine::Or,
        other => return Err(format!("알 수 없는 필터 결합 방식: '{other}'")),
    };
    let levels = dto
        .levels
        .iter()
        .map(|l| match l.as_str() {
            "error" => Ok(Level::Error),
            "warn" => Ok(Level::Warn),
            "info" => Ok(Level::Info),
            "debug" => Ok(Level::Debug),
            "trace" => Ok(Level::Trace),
            other => Err(format!("알 수 없는 로그 레벨: '{other}'")),
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(FilterSpec {
        rules,
        combine,
        levels,
    })
}

/// 필드 규칙을 지원하기 위해 파일의 컬럼을 감지하고 FilterSet 을 컴파일한다.
/// 컬럼 감지는 에이전트가 원격 파일에서 직접 수행한다.
fn compile_filter(file: &LogFile, spec: FilterSpec) -> Result<FilterSet, String> {
    let columns = detect_columns(file).map_err(|e| e.to_string())?;
    FilterSet::new(&spec, Some(&columns)).map_err(|e| e.to_string())
}

/// index 작업: 인덱싱을 실행하고 200ms 간격 진행률 후 체크포인트를 돌려준다.
fn job_index<W: Write + Send>(
    session: Arc<Session<W>>,
    id: u64,
    file: Arc<LogFile>,
    cancel: Arc<AtomicBool>,
) {
    let mut last = Instant::now()
        .checked_sub(PROGRESS_INTERVAL)
        .unwrap_or_else(Instant::now);
    let result = file.build_index(&cancel, &mut |p| {
        if last.elapsed() >= PROGRESS_INTERVAL {
            last = Instant::now();
            session.send(
                json!({"id": id, "progress": {"indexedBytes": p.indexed_bytes, "lineCount": p.line_count}}),
            );
        }
    });
    session.finish_job(id);
    if let Err(e) = result {
        session.send_error(id, e.to_string());
    } else if cancel.load(Ordering::Relaxed) {
        session.send(json!({"id": id, "cancelled": true}));
    } else {
        let p = file.index().progress();
        session.send(json!({
            "id": id,
            "index": {"totalBytes": p.total_bytes, "lineCount": p.line_count,
                      "checkpoints": file.index().checkpoints()},
        }));
    }
}

/// search 작업: 매치 줄 번호를 최대 10,000개씩 배치로 보낸다.
fn job_search<W: Write + Send>(
    session: Arc<Session<W>>,
    id: u64,
    file: Arc<LogFile>,
    matcher: Matcher,
    spec: Option<FilterSpec>,
    cancel: Arc<AtomicBool>,
) {
    let filter = match spec.map(|s| compile_filter(&file, s)).transpose() {
        Ok(f) => f,
        Err(e) => {
            session.finish_job(id);
            session.send_error(id, e);
            return;
        }
    };
    let mut pending: Vec<u64> = Vec::with_capacity(SEARCH_BATCH_MAX);
    let outcome = search_filtered(
        &file,
        &matcher,
        filter.as_ref(),
        &cancel,
        &mut |batch| {
            pending.extend_from_slice(batch);
            if pending.len() >= SEARCH_BATCH_MAX {
                session.send(json!({"id": id, "batch": pending.as_slice()}));
                pending.clear();
            }
        },
        &mut |_| {},
    );
    if !pending.is_empty() {
        session.send(json!({"id": id, "batch": pending.as_slice()}));
    }
    session.finish_job(id);
    finish_scan(&session, id, outcome);
}

/// filter 작업: 보이는 줄의 연속 구간(LineSet ranges)을 최대 10,000개씩 보낸다.
/// 마지막 구간은 이후 줄과 이어질 수 있으므로 진행 중에는 그 앞까지만 보낸다.
fn job_filter<W: Write + Send>(
    session: Arc<Session<W>>,
    id: u64,
    file: Arc<LogFile>,
    spec: FilterSpec,
    cancel: Arc<AtomicBool>,
) {
    let set = match compile_filter(&file, spec) {
        Ok(f) => f,
        Err(e) => {
            session.finish_job(id);
            session.send_error(id, e);
            return;
        }
    };
    let mut lines = LineSet::new();
    let mut emitted = 0usize;
    let mut capped = false;
    let outcome = filter_scan(
        &file,
        &set,
        &cancel,
        &mut |batch| {
            for &l in batch {
                lines.push_line(l);
            }
            if lines.range_count() > MAX_FILTER_RANGES {
                capped = true;
                cancel.store(true, Ordering::Relaxed);
                return;
            }
            // 마지막 구간은 이어질 수 있으므로 그 앞까지만 보낸다
            while lines.range_count().saturating_sub(emitted) > RANGE_BATCH_MAX {
                let upto = lines.range_count() - 1;
                send_ranges(&session, id, &lines.ranges()[emitted..upto]);
                emitted = upto;
            }
        },
        &mut |_| {},
    );
    // 남은 구간을 10,000개 단위로 모두 보낸다
    while emitted < lines.range_count() {
        let upto = (emitted + RANGE_BATCH_MAX).min(lines.range_count());
        send_ranges(&session, id, &lines.ranges()[emitted..upto]);
        emitted = upto;
    }
    session.finish_job(id);
    if capped {
        session.send(json!({"id": id, "done": {"total": lines.len(), "capped": true}}));
    } else {
        match outcome {
            Ok(riffle_core::SearchOutcome::Cancelled) => {
                session.send(json!({"id": id, "cancelled": true}));
            }
            Ok(_) => {
                session.send(json!({"id": id, "done": {"total": lines.len(), "capped": false}}));
            }
            Err(e) => session.send_error(id, e.to_string()),
        }
    }
}

fn send_ranges<W: Write + Send>(session: &Session<W>, id: u64, ranges: &[(u64, u64)]) {
    if !ranges.is_empty() {
        session.send(json!({"id": id, "ranges": ranges}));
    }
}

/// 검색 스캔 종료 응답 (완료/상한/취소/오류).
fn finish_scan<W: Write + Send>(
    session: &Session<W>,
    id: u64,
    outcome: riffle_core::Result<riffle_core::SearchOutcome>,
) {
    use riffle_core::SearchOutcome;
    match outcome {
        Ok(SearchOutcome::Completed { total }) => {
            session.send(json!({"id": id, "done": {"total": total, "capped": false}}));
        }
        Ok(SearchOutcome::Capped { total }) => {
            session.send(json!({"id": id, "done": {"total": total, "capped": true}}));
        }
        Ok(SearchOutcome::Cancelled) => {
            session.send(json!({"id": id, "cancelled": true}));
        }
        Err(e) => session.send_error(id, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    /// Vec<u8> 버퍼를 출력으로 쓰는 테스트 세션.
    fn test_session() -> Arc<Session<Vec<u8>>> {
        Arc::new(Session {
            file: Mutex::new(None),
            jobs: Mutex::new(HashMap::new()),
            handles: Mutex::new(Vec::new()),
            out: Mutex::new(Vec::new()),
        })
    }

    /// 출력 버퍼의 각 줄을 JSON 으로 파싱해 반환한다.
    fn out_values(s: &Session<Vec<u8>>) -> Vec<Value> {
        let text = String::from_utf8(s.out.lock().unwrap().clone()).unwrap();
        text.lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect()
    }

    /// batch 메시지들의 줄 번호를 모으고 done 응답을 반환한다.
    fn collect_search_out(out: &[Value]) -> (Vec<u64>, Option<(u64, bool)>) {
        let mut hits = Vec::new();
        let mut done = None;
        for v in out {
            if let Some(b) = v.get("batch") {
                for x in b.as_array().unwrap() {
                    hits.push(x.as_u64().unwrap());
                }
            }
            if let Some(d) = v.get("done") {
                done = Some((d["total"].as_u64().unwrap(), d["capped"].as_bool().unwrap()));
            }
        }
        (hits, done)
    }

    type FilterOut = (Vec<(u64, u64)>, Option<(u64, bool)>);

    /// ranges 메시지들의 구간을 모으고 done 응답을 반환한다.
    fn collect_filter_out(out: &[Value]) -> FilterOut {
        let mut ranges = Vec::new();
        let mut done = None;
        for v in out {
            if let Some(r) = v.get("ranges") {
                for p in r.as_array().unwrap() {
                    ranges.push((p[0].as_u64().unwrap(), p[1].as_u64().unwrap()));
                }
            }
            if let Some(d) = v.get("done") {
                done = Some((d["total"].as_u64().unwrap(), d["capped"].as_bool().unwrap()));
            }
        }
        (ranges, done)
    }

    /// 테스트용 임시 파일 (drop 시 삭제)
    struct TmpFile {
        path: PathBuf,
    }

    impl TmpFile {
        fn new(name: &str, content: &[u8]) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "riffle_agent_{}_{}_{}.log",
                name,
                std::process::id(),
                id
            ));
            std::fs::write(&path, content).expect("임시 파일 쓰기 실패");
            TmpFile { path }
        }
    }

    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// 레벨 토큰과 한글이 섞인 로그 내용.
    fn gen_content(lines: usize) -> Vec<u8> {
        let mut buf = Vec::new();
        for i in 0..lines {
            buf.extend_from_slice(
                format!(
                    "2026-11-14 10:{:02}:{:02} {} worker-{} 메시지 alpha {}\n",
                    (i / 60) % 60,
                    i % 60,
                    if i % 5 == 0 { "WARN" } else { "INFO" },
                    i % 8,
                    i
                )
                .as_bytes(),
            );
        }
        buf
    }

    /// 로컬 search_filtered 와 같은 조건으로 줄 번호를 수집한다 (기대값 계산용).
    fn local_search(file: &LogFile, m: &Matcher, f: Option<&FilterSet>) -> (Vec<u64>, u64, bool) {
        let cancel = AtomicBool::new(false);
        let mut hits = Vec::new();
        let outcome = search_filtered(
            file,
            m,
            f,
            &cancel,
            &mut |b| hits.extend_from_slice(b),
            &mut |_| {},
        )
        .expect("로컬 검색 실패");
        match outcome {
            riffle_core::SearchOutcome::Completed { total } => (hits, total, false),
            riffle_core::SearchOutcome::Capped { total } => (hits, total, true),
            riffle_core::SearchOutcome::Cancelled => (hits, 0, false),
        }
    }

    #[test]
    fn test_hello() {
        let s = test_session();
        dispatch_line(&s, r#"{"id":1,"op":"hello"}"#);
        let out = out_values(&s);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], json!(1));
        assert_eq!(out[0]["hello"]["version"], json!(VERSION));
        assert_eq!(out[0]["hello"]["protocol"], json!(PROTOCOL_VERSION));
    }

    #[test]
    fn test_errors_and_recovery() {
        let s = test_session();
        // 잘못된 JSON → id 0 오류
        dispatch_line(&s, "{bad json");
        // 알 수 없는 op → 해당 id 오류
        dispatch_line(&s, r#"{"id":9,"op":"nope"}"#);
        // 파일 없이 index → 오류
        dispatch_line(&s, r#"{"id":10,"op":"index"}"#);
        // 없는 작업 취소 → 응답 없음
        dispatch_line(&s, r#"{"id":11,"op":"cancel","target":99}"#);
        let out = out_values(&s);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["id"], json!(0));
        assert!(out[0]["error"].as_str().unwrap().contains("잘못된"));
        assert_eq!(out[1]["id"], json!(9));
        assert!(out[1]["error"].as_str().unwrap().contains("알 수 없는 op"));
        assert_eq!(out[2]["id"], json!(10));
        assert!(out[2]["error"].is_string());

        // 오류 후에도 세션은 계속 동작한다
        dispatch_line(&s, r#"{"id":1,"op":"hello"}"#);
        let out = out_values(&s);
        assert_eq!(out.len(), 4);
        assert!(out[3].get("hello").is_some());
    }

    #[test]
    fn test_open() {
        let s = test_session();
        // path 없음 → 오류
        dispatch_line(&s, r#"{"id":1,"op":"open"}"#);
        // 없는 파일 → 오류
        dispatch_line(&s, r#"{"id":2,"op":"open","path":"/nonexistent/xyz.log"}"#);
        // 정상 파일
        let tmp = TmpFile::new("open", b"a\nb\n");
        dispatch_line(
            &s,
            &format!(r#"{{"id":3,"op":"open","path":"{}"}}"#, tmp.path.display()),
        );
        // gzip 매직 바이트 → gzip:true
        let gz = TmpFile::new("open-gz", &[0x1f, 0x8b, 0x00, 0x11]);
        dispatch_line(
            &s,
            &format!(r#"{{"id":4,"op":"open","path":"{}"}}"#, gz.path.display()),
        );
        let out = out_values(&s);
        assert_eq!(out.len(), 4);
        assert!(out[0].get("error").is_some());
        assert!(out[1].get("error").is_some());
        assert_eq!(out[2]["opened"]["size"], json!(4));
        assert_eq!(out[2]["opened"]["gzip"], json!(false));
        assert_eq!(out[3]["opened"]["gzip"], json!(true));
    }

    #[test]
    fn test_index_via_dispatch() {
        let tmp = TmpFile::new("idx", &gen_content(5000));
        let s = test_session();
        dispatch_line(
            &s,
            &format!(r#"{{"id":1,"op":"open","path":"{}"}}"#, tmp.path.display()),
        );
        dispatch_line(&s, r#"{"id":2,"op":"index"}"#);
        for h in s.handles.lock().unwrap().drain(..) {
            h.join().unwrap();
        }

        // 로컬 인덱싱 기대값
        let local = LogFile::open(&tmp.path).unwrap();
        let cancel = AtomicBool::new(false);
        local.build_index(&cancel, &mut |_| {}).unwrap();

        let out = out_values(&s);
        let last = out.last().unwrap();
        assert_eq!(last["id"], json!(2));
        assert_eq!(
            last["index"]["lineCount"],
            json!(local.index().line_count())
        );
        assert_eq!(last["index"]["totalBytes"], json!(local.source().len()));
        let cps: Vec<u64> = last["index"]["checkpoints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        assert_eq!(cps, local.index().checkpoints());
    }

    #[test]
    fn test_job_search() {
        let tmp = TmpFile::new("srch", &gen_content(8000));
        let file = Arc::new(LogFile::open(&tmp.path).unwrap());

        for (q, regex, case) in [
            ("alpha 1234", false, true),
            (r"alpha\s+12\d\d", true, true),
            ("ALPHA", false, false),
        ] {
            let s = test_session();
            let m = Matcher::new(
                q,
                SearchOptions {
                    regex,
                    case_sensitive: case,
                },
            )
            .unwrap();
            job_search(
                s.clone(),
                4,
                file.clone(),
                m.clone(),
                None,
                Arc::new(AtomicBool::new(false)),
            );
            let out = out_values(&s);
            let (hits, done) = collect_search_out(&out);
            let (lhits, ltotal, lcapped) = local_search(&file, &m, None);
            assert_eq!(hits, lhits, "query {q}");
            assert_eq!(done, Some((ltotal, lcapped)), "query {q}");
        }
    }

    #[test]
    fn test_job_search_with_filter() {
        let tmp = TmpFile::new("sf", &gen_content(6000));
        let file = Arc::new(LogFile::open(&tmp.path).unwrap());
        // 레벨 조건: INFO 만 보이는 필터 (WARN 줄 제외)
        let spec_v = json!({
            "rules": [],
            "combine": "and",
            "levels": ["info"],
        });
        let spec = spec_from_value(spec_v).unwrap();
        let m = Matcher::new(
            "alpha",
            SearchOptions {
                regex: false,
                case_sensitive: true,
            },
        )
        .unwrap();

        let s = test_session();
        job_search(
            s.clone(),
            5,
            file.clone(),
            m.clone(),
            Some(spec.clone()),
            Arc::new(AtomicBool::new(false)),
        );
        let (hits, done) = collect_search_out(&out_values(&s));

        let set = compile_filter(&file, spec).unwrap();
        let (lhits, ltotal, lcapped) = local_search(&file, &m, Some(&set));
        assert_eq!(hits, lhits);
        assert_eq!(done, Some((ltotal, lcapped)));
    }

    #[test]
    fn test_job_filter_field_rule() {
        // CSV 샘플: city 컬럼에 필드 규칙
        let mut csv = String::from("name,city,age\n");
        for i in 0..4000 {
            csv.push_str(&format!(
                "user{i},{},{}\n",
                if i % 3 == 0 { "seoul" } else { "busan" },
                20 + i % 40
            ));
        }
        let tmp = TmpFile::new("csv", csv.as_bytes());
        let file = Arc::new(LogFile::open(&tmp.path).unwrap());
        let spec_v = json!({
            "rules": [{
                "pattern": "seoul", "regex": false, "caseSensitive": true,
                "kind": "include", "enabled": true, "color": "#fff", "field": "city",
            }],
            "combine": "and",
            "levels": [],
        });
        let spec = spec_from_value(spec_v).unwrap();

        // 로컬 기대값
        let cols = detect_columns(&file).unwrap();
        assert!(matches!(cols.format, riffle_core::LogFormat::Csv { .. }));
        let set_local = FilterSet::new(&spec, Some(&cols)).unwrap();
        let cancel = AtomicBool::new(false);
        let mut expected = LineSet::new();
        filter_scan(
            &file,
            &set_local,
            &cancel,
            &mut |b| {
                for &l in b {
                    expected.push_line(l);
                }
            },
            &mut |_| {},
        )
        .unwrap();

        let s = test_session();
        job_filter(s.clone(), 6, file, spec, Arc::new(AtomicBool::new(false)));
        let out = out_values(&s);
        let (ranges, done) = collect_filter_out(&out);
        assert_eq!(ranges.as_slice(), expected.ranges());
        assert_eq!(done, Some((expected.len(), false)));
    }

    #[test]
    fn test_job_cancelled() {
        let tmp = TmpFile::new("cancel", &gen_content(1000));
        let file = Arc::new(LogFile::open(&tmp.path).unwrap());
        let m = Matcher::new(
            "alpha",
            SearchOptions {
                regex: false,
                case_sensitive: true,
            },
        )
        .unwrap();

        // 시작 전 취소된 검색 → cancelled 응답
        let s = test_session();
        job_search(
            s.clone(),
            7,
            file.clone(),
            m,
            None,
            Arc::new(AtomicBool::new(true)),
        );
        let out = out_values(&s);
        assert_eq!(out.last().unwrap()["cancelled"], json!(true));

        // 시작 전 취소된 필터 → cancelled 응답
        let s = test_session();
        let spec = spec_from_value(json!({
            "rules": [{"pattern":"x","regex":false,"caseSensitive":true,
                       "kind":"include","enabled":true,"color":"#fff"}],
            "combine":"and","levels":[]
        }))
        .unwrap();
        job_filter(s.clone(), 8, file, spec, Arc::new(AtomicBool::new(true)));
        let out = out_values(&s);
        assert_eq!(out.last().unwrap()["cancelled"], json!(true));
    }

    #[test]
    fn test_cancel_dispatch_sets_flag() {
        let s = test_session();
        let flag = s.register_job(42);
        dispatch_line(&s, r#"{"id":9,"op":"cancel","target":42}"#);
        assert!(flag.load(Ordering::Relaxed));
        // cancel 요청 자체에는 응답이 없다
        assert!(out_values(&s).is_empty());
    }

    #[test]
    fn test_concurrent_jobs() {
        let tmp = TmpFile::new("conc", &gen_content(3000));
        let file = Arc::new(LogFile::open(&tmp.path).unwrap());
        let s = test_session();
        let s2 = s.clone();
        let f2 = file.clone();
        let t1 = std::thread::spawn(move || {
            let m = Matcher::new(
                "alpha",
                SearchOptions {
                    regex: false,
                    case_sensitive: true,
                },
            )
            .unwrap();
            job_search(s2, 10, f2, m, None, Arc::new(AtomicBool::new(false)));
        });
        let s3 = s.clone();
        let f3 = file.clone();
        let t2 = std::thread::spawn(move || {
            let spec = spec_from_value(json!({
                "rules": [], "combine": "and", "levels": ["info"]
            }))
            .unwrap();
            job_filter(s3, 11, f3, spec, Arc::new(AtomicBool::new(false)));
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let out = out_values(&s);
        assert!(
            out.iter()
                .any(|v| v["id"] == json!(10) && v.get("done").is_some())
        );
        assert!(
            out.iter()
                .any(|v| v["id"] == json!(11) && v.get("done").is_some())
        );
    }

    #[test]
    fn test_spec_from_value_errors() {
        // 잘못된 결합/규칙 종류/레벨
        assert!(spec_from_value(json!({"rules":[],"combine":"xor","levels":[]})).is_err());
        assert!(
            spec_from_value(json!({
                "rules":[{"pattern":"x","regex":false,"caseSensitive":true,
                          "kind":"maybe","enabled":true,"color":"#fff"}],
                "combine":"and","levels":[]
            }))
            .is_err()
        );
        assert!(spec_from_value(json!({"rules":[],"combine":"and","levels":["fatal"]})).is_err());
        // 형식 자체가 틀린 경우
        assert!(spec_from_value(json!({"rules":"oops","combine":"and","levels":[]})).is_err());
    }
}
