//! S3 `RandomRead`·목록 API 를 가짜 HTTP S3 서버로 검증한다.
//! 가짜 서버는 std::net 만 쓰는 스레드 기반 구현이며 서명은 검증하지 않는다.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use riffle_core::{
    DecompressProgress, LogFile, Matcher, SearchOptions, is_gzip_reader, open_gzip_remote, search,
};
use riffle_remote::{RemoteError, S3Pool, S3Uri, list_aws_profiles_from};

// ---------------------------------------------------------------------------
// 가짜 S3 서버
// ---------------------------------------------------------------------------

/// 서버가 기록한 요청 한 건.
#[derive(Debug, Clone)]
struct RecordedRequest {
    method: String,
    /// 경로 + 쿼리 문자열.
    target: String,
    /// Range 헤더 값 (없으면 None).
    range: Option<String>,
}

struct FakeState {
    /// ListBuckets 응답용 버킷 목록.
    buckets: Vec<String>,
    /// (bucket, key) → 객체 내용.
    objects: HashMap<(String, String), Vec<u8>>,
    /// 이 버킷들은 모든 요청에 403 을 돌린다.
    forbidden_buckets: HashSet<String>,
    /// ListObjectsV2 페이지 크기. 작게 두면 페이지네이션을 유도할 수 있다.
    page_size: usize,
}

struct FakeS3 {
    url: String,
    state: Arc<Mutex<FakeState>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    /// 현재 처리 중인 요청 수 / 관측된 최대 동시 요청 수.
    inflight: Arc<AtomicUsize>,
    max_inflight: Arc<AtomicUsize>,
    /// Range GET 응답을 목표 수만큼 동시 도착할 때까지 보류하는 장치 (0=비활성).
    rendezvous: Arc<Rendezvous>,
}

/// Range GET 요청을 목표 수만큼 모아서 한꺼번에 풀어 주는 장치.
/// 클라이언트가 정말 병렬로 요청할 때만 목표에 도달하므로 서버 측
/// 동시 요청 수를 타이밍이 아니라 구조적으로 검증할 수 있다.
/// 목표에 못 미친 채 마감이 지나면(직렬 클라이언트) 그냥 풀어 줘 무한 대기를 피한다.
struct Rendezvous {
    target: AtomicUsize,
    inner: Mutex<RendezvousState>,
    cv: Condvar,
}

struct RendezvousState {
    arrived: usize,
    /// 최초 도착 시각 + 제한 시간. 직렬 클라이언트의 데드락 방지용.
    deadline: Option<Instant>,
}

impl Rendezvous {
    fn new() -> Self {
        Self {
            target: AtomicUsize::new(0),
            inner: Mutex::new(RendezvousState {
                arrived: 0,
                deadline: None,
            }),
            cv: Condvar::new(),
        }
    }

    /// 목표 도달까지 기다린다. target=0 이면 아무것도 하지 않는다.
    fn wait(&self) {
        let target = self.target.load(Ordering::Relaxed);
        if target == 0 {
            return;
        }
        let mut st = self.inner.lock().unwrap();
        if st.arrived == 0 {
            // 상한은 짧게 둔다: 보류가 길면 부하 상태에서 SDK 재시도와 엮여
            // 읽기 자체가 실패할 수 있다. 모이지 않아도 응답은 내보낸다.
            st.deadline = Some(Instant::now() + Duration::from_secs(2));
        }
        st.arrived += 1;
        if st.arrived >= target {
            self.cv.notify_all();
            return;
        }
        let deadline = st.deadline.unwrap();
        while st.arrived < target {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (g, timeout) = self.cv.wait_timeout(st, remaining).unwrap();
            st = g;
            if timeout.timed_out() {
                break;
            }
        }
    }
}

impl FakeS3 {
    fn start() -> FakeS3 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("리스너 바인드 실패");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let fake = FakeS3 {
            url,
            state: Arc::new(Mutex::new(FakeState {
                buckets: Vec::new(),
                objects: HashMap::new(),
                forbidden_buckets: HashSet::new(),
                page_size: 1000,
            })),
            requests: Arc::new(Mutex::new(Vec::new())),
            inflight: Arc::new(AtomicUsize::new(0)),
            max_inflight: Arc::new(AtomicUsize::new(0)),
            rendezvous: Arc::new(Rendezvous::new()),
        };
        let (state, requests, inflight, max_inflight, rendezvous) = (
            fake.state.clone(),
            fake.requests.clone(),
            fake.inflight.clone(),
            fake.max_inflight.clone(),
            fake.rendezvous.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (state, requests, inflight, max_inflight, rendezvous) = (
                    state.clone(),
                    requests.clone(),
                    inflight.clone(),
                    max_inflight.clone(),
                    rendezvous.clone(),
                );
                // 스레드 생성이 실패하면(극심한 부하) 연결만 닫힌다 —
                // accept 루프는 유지하고 클라이언트 재시도가 다음 연결로 복구한다.
                let _ = std::thread::Builder::new().spawn(move || {
                    handle_conn(stream, state, requests, inflight, max_inflight, rendezvous);
                });
            }
        });
        fake
    }

    /// Range GET 응답을 `n`개가 동시에 도착할 때까지 보류하게 한다.
    /// 병렬 읽기 검증 전에 호출한다.
    fn set_range_rendezvous(&self, n: usize) {
        *self.rendezvous.inner.lock().unwrap() = RendezvousState {
            arrived: 0,
            deadline: None,
        };
        self.rendezvous.target.store(n, Ordering::Relaxed);
    }

    fn add_bucket(&self, name: &str) {
        self.state.lock().unwrap().buckets.push(name.to_string());
    }

    fn forbid_bucket(&self, name: &str) {
        let mut st = self.state.lock().unwrap();
        st.buckets.push(name.to_string());
        st.forbidden_buckets.insert(name.to_string());
    }

    fn set_page_size(&self, n: usize) {
        self.state.lock().unwrap().page_size = n;
    }

    fn put_object(&self, bucket: &str, key: &str, data: &[u8]) {
        self.state
            .lock()
            .unwrap()
            .objects
            .insert((bucket.to_string(), key.to_string()), data.to_vec());
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

struct Request {
    method: String,
    /// 요청 라인의 대상 (`/bucket/key?query`).
    target: String,
    /// 소문자 이름 → 값.
    headers: HashMap<String, String>,
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn handle_conn(
    mut stream: TcpStream,
    state: Arc<Mutex<FakeState>>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    inflight: Arc<AtomicUsize>,
    max_inflight: Arc<AtomicUsize>,
    rendezvous: Arc<Rendezvous>,
) {
    let Some(req) = read_request(&mut stream) else {
        return;
    };
    requests.lock().unwrap().push(RecordedRequest {
        method: req.method.clone(),
        target: req.target.clone(),
        range: req.headers.get("range").cloned(),
    });
    let cur = inflight.fetch_add(1, Ordering::Relaxed) + 1;
    max_inflight.fetch_max(cur, Ordering::Relaxed);
    let resp = route(&req, &state.lock().unwrap());
    // 병렬 검증이 켜져 있으면 Range GET 응답을 목표 수만큼 모을 때까지 보류한다.
    if req.method == "GET" && req.headers.contains_key("range") {
        rendezvous.wait();
    }
    let _ = write_response(&mut stream, &resp);
    inflight.fetch_sub(1, Ordering::Relaxed);
}

/// 헤더 `\r\n\r\n` 까지 읽고, Content-Length 만큼의 본문은 읽어 버린다.
fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
            break pos;
        }
        let n = stream.read(&mut tmp).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1_000_000 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    // 남은 본문은 읽어 버린다 (GET/HEAD 는 보통 없다).
    let body_len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut got = buf.len() - header_end;
    while got < body_len {
        match stream.read(&mut tmp) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    Some(Request {
        method,
        target,
        headers,
    })
}

fn write_response(stream: &mut TcpStream, resp: &Response) -> std::io::Result<()> {
    let reason = match resp.status {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        _ => "Error",
    };
    let mut out = format!("HTTP/1.1 {} {}\r\n", resp.status, reason);
    // HEAD 응답처럼 본문과 Content-Length 가 다른 경우를 위해 명시 헤더를 우선한다.
    if !resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        out.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    for (k, v) in &resp.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    stream.write_all(out.as_bytes())?;
    stream.write_all(&resp.body)
}

fn route(req: &Request, st: &FakeState) -> Response {
    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (req.target.clone(), String::new()),
    };
    let mut segs = path.trim_start_matches('/').splitn(2, '/');
    let bucket = percent_decode(segs.next().unwrap_or_default());
    let key = percent_decode(segs.next().unwrap_or_default());

    if st.forbidden_buckets.contains(&bucket) {
        return error_response(403, "AccessDenied", "접근이 거부되었습니다");
    }
    if bucket.is_empty() {
        // GET / → ListBuckets
        if req.method == "GET" {
            return list_buckets_response(st);
        }
        return error_response(400, "BadRequest", "지원하지 않는 요청");
    }
    if !st.buckets.iter().any(|b| b == &bucket) {
        return error_response(404, "NoSuchBucket", "버킷이 없습니다");
    }
    match (req.method.as_str(), key.is_empty()) {
        ("HEAD", true) => Response {
            status: 200,
            headers: vec![("x-amz-bucket-region".to_string(), "us-east-1".to_string())],
            body: Vec::new(),
        },
        ("GET", true) if query_param(&query, "list-type").as_deref() == Some("2") => {
            list_objects_response(st, &bucket, &query)
        }
        ("HEAD", false) => head_object_response(st, &bucket, &key),
        ("GET", false) => get_object_response(st, &bucket, &key, req.headers.get("range")),
        _ => error_response(400, "BadRequest", "지원하지 않는 요청"),
    }
}

fn list_buckets_response(st: &FakeState) -> Response {
    let mut items = String::new();
    for b in &st.buckets {
        items.push_str(&format!(
            "<Bucket><Name>{}</Name><CreationDate>2024-01-01T00:00:00.000Z</CreationDate></Bucket>",
            xml_escape(b)
        ));
    }
    xml_response(
        200,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListAllMyBucketsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Buckets>{items}</Buckets></ListAllMyBucketsResult>"
        ),
        Vec::new(),
    )
}

/// 페이지 단위 항목: 공통 접두사 또는 객체 키.
enum ListItem {
    Prefix(String),
    Object(String),
}

fn list_objects_response(st: &FakeState, bucket: &str, query: &str) -> Response {
    let prefix = query_param(query, "prefix").unwrap_or_default();
    let start: usize = query_param(query, "continuation-token")
        .and_then(|t| t.parse().ok())
        .unwrap_or(0);

    // 키 순으로 순회하며 contents 와 common prefixes 를 섞은 목록을 만든다.
    let mut keys: Vec<&String> = st
        .objects
        .keys()
        .filter(|(b, _)| b == bucket)
        .map(|(_, k)| k)
        .collect();
    keys.sort();
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for k in keys {
        let Some(rest) = k.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if let Some(slash) = rest.find('/') {
            let cp = format!("{}{}/", prefix, &rest[..slash]);
            if seen.insert(cp.clone()) {
                items.push(ListItem::Prefix(cp));
            }
        } else {
            items.push(ListItem::Object(k.clone()));
        }
    }

    let end = (start + st.page_size).min(items.len());
    let page = &items[start..end];
    let truncated = end < items.len();

    let mut contents = String::new();
    let mut common = String::new();
    for item in page {
        match item {
            ListItem::Prefix(p) => {
                common.push_str(&format!(
                    "<CommonPrefixes><Prefix>{}</Prefix></CommonPrefixes>",
                    xml_escape(p)
                ));
            }
            ListItem::Object(k) => {
                let size = st.objects[&(bucket.to_string(), k.clone())].len();
                contents.push_str(&format!(
                    "<Contents><Key>{}</Key><LastModified>2024-01-02T03:04:05.000Z</LastModified>\
                     <ETag>\"e\"</ETag><Size>{size}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                    xml_escape(k)
                ));
            }
        }
    }
    let next = if truncated {
        format!("<NextContinuationToken>{end}</NextContinuationToken>")
    } else {
        String::new()
    };
    xml_response(
        200,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>{}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount><MaxKeys>{}</MaxKeys>\
             <Delimiter>/</Delimiter><IsTruncated>{truncated}</IsTruncated>\
             {contents}{common}{next}</ListBucketResult>",
            xml_escape(bucket),
            xml_escape(&prefix),
            page.len(),
            st.page_size,
        ),
        Vec::new(),
    )
}

fn head_object_response(st: &FakeState, bucket: &str, key: &str) -> Response {
    let Some(data) = st.objects.get(&(bucket.to_string(), key.to_string())) else {
        return Response {
            status: 404,
            headers: Vec::new(),
            body: Vec::new(), // HEAD 오류는 본문이 없다
        };
    };
    Response {
        status: 200,
        headers: vec![
            ("Content-Length".to_string(), data.len().to_string()),
            ("ETag".to_string(), "\"e\"".to_string()),
            (
                "Last-Modified".to_string(),
                "Tue, 02 Jan 2024 03:04:05 GMT".to_string(),
            ),
        ],
        body: Vec::new(),
    }
}

fn get_object_response(
    st: &FakeState,
    bucket: &str,
    key: &str,
    range: Option<&String>,
) -> Response {
    let Some(data) = st.objects.get(&(bucket.to_string(), key.to_string())) else {
        return error_response(404, "NoSuchKey", "키가 없습니다");
    };
    let Some(range) = range else {
        return Response {
            status: 200,
            headers: Vec::new(),
            body: data.clone(),
        };
    };
    // "bytes=a-b" 파싱.
    let Some((a, b)) = range
        .strip_prefix("bytes=")
        .and_then(|r| r.split_once('-'))
        .map(|(a, b)| (a.parse::<u64>(), b.parse::<u64>()))
    else {
        return error_response(400, "InvalidArgument", "잘못된 Range");
    };
    let (Ok(a), Ok(b)) = (a, b) else {
        return error_response(400, "InvalidArgument", "잘못된 Range");
    };
    if a >= data.len() as u64 {
        return error_response(416, "InvalidRange", "범위가 객체 밖입니다");
    }
    let end = (b as usize).min(data.len() - 1);
    let body = data[a as usize..=end].to_vec();
    Response {
        status: 206,
        headers: vec![(
            "Content-Range".to_string(),
            format!("bytes {}-{}/{}", a, end, data.len()),
        )],
        body,
    }
}

fn xml_response(status: u16, body: String, mut headers: Vec<(String, String)>) -> Response {
    headers.push(("Content-Type".to_string(), "application/xml".to_string()));
    Response {
        status,
        headers,
        body: body.into_bytes(),
    }
}

fn error_response(status: u16, code: &str, msg: &str) -> Response {
    xml_response(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>{code}</Code><Message>{}</Message></Error>",
            xml_escape(msg)
        ),
        Vec::new(),
    )
}

fn query_param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| percent_decode(v))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap_or(b'?');
                out.push(h);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// 테스트 유틸
// ---------------------------------------------------------------------------

fn make_pool(s: &FakeS3) -> S3Pool {
    S3Pool::with_endpoint(&s.url)
}

/// i 번째 줄이 `"s3 line {i:06} 메시지 alpha"` 인 로그 내용.
fn log_content(lines: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..lines {
        v.extend_from_slice(format!("s3 line {i:06} 메시지 alpha\n").as_bytes());
    }
    v
}

/// 테스트용 임시 파일 (drop 시 삭제).
struct TmpFile {
    path: PathBuf,
}

impl TmpFile {
    fn new(name: &str, content: &[u8]) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "riffle_s3test_{name}_{}_{}.tmp",
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

// ---------------------------------------------------------------------------
// S3Uri / 프로필
// ---------------------------------------------------------------------------

#[test]
fn s3_uri_parse_roundtrip() {
    for raw in [
        "s3://my-bucket/path/to/file.log",
        "s3://b/한글 이름/공백 있는 키.log",
        "s3://b/a//b///c",
        "s3://b/key with spaces & %zz!.log",
        "s3://b/",
        "s3://b",
    ] {
        let uri = S3Uri::parse(raw).unwrap_or_else(|| panic!("parse 실패: {raw}"));
        assert_eq!(
            S3Uri::parse(&uri.to_string()).as_ref(),
            Some(&uri),
            "왕복 실패: {raw}"
        );
    }
    let u = S3Uri::parse("s3://my-bucket/dir/f.log").unwrap();
    assert_eq!(u.bucket, "my-bucket");
    assert_eq!(u.key, "dir/f.log");
    assert_eq!(u.to_string(), "s3://my-bucket/dir/f.log");
    // 버킷이 없거나 스킴이 다르면 None.
    assert!(S3Uri::parse("s3://").is_none());
    assert!(S3Uri::parse("s3:///key").is_none());
    assert!(S3Uri::parse("sftp://b/k").is_none());
    assert!(S3Uri::parse("/local/path").is_none());
}

#[test]
fn aws_profiles_from_files() {
    let dir = std::env::temp_dir().join(format!("riffle_aws_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config");
    let cred = dir.join("credentials");
    std::fs::write(
        &config,
        "[default]\nregion = ap-northeast-2\n\n[profile dev]\nregion = us-west-2\n\n\
         [profile \"team prod\"]\n\n[sso-session corp]\nsso_start_url = https://x\n\n\
         [services s3]\n\n; [comment]\n",
    )
    .unwrap();
    std::fs::write(
        &cred,
        "[default]\naws_access_key_id = X\n\n[dev]\naws_access_key_id = Y\n\n[legacy]\nz = 1\n",
    )
    .unwrap();

    let profiles = list_aws_profiles_from(Some(&config), Some(&cred));
    assert_eq!(profiles, vec!["default", "dev", "legacy", "team prod"]);
    // 파일이 없어도 default 만.
    assert_eq!(list_aws_profiles_from(None, None), vec!["default"]);

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 가짜 서버를 통한 S3Pool / RandomRead
// ---------------------------------------------------------------------------

#[test]
fn s3_read_various_ranges() {
    let s = FakeS3::start();
    s.add_bucket("b1");
    let content = log_content(3000); // 약 100KiB
    s.put_object("b1", "app.log", &content);
    let pool = make_pool(&s);
    let uri = S3Uri::parse("s3://b1/app.log").unwrap();
    let r = pool.open_object(None, &uri).expect("open_object 실패");

    assert_eq!(r.uri(), "s3://b1/app.log");
    assert_eq!(r.stat_len().unwrap(), content.len() as u64);

    let mut buf = vec![0u8; content.len() + 4096];
    for (off, len) in [
        (0u64, 100usize),
        (1, 1),
        (37, 4096),
        (content.len() as u64 - 10, 100), // EOF 에 걸친 읽기
        (content.len() as u64, 100),      // 정확히 EOF
        (content.len() as u64 + 5, 100),  // EOF 너머
        (0, content.len()),               // 전체
        (u64::MAX - 100, 200),            // 아주 먼 오프셋
    ] {
        buf.fill(0xAA);
        let n = r.read_at(&mut buf[..len], off).unwrap();
        let expect = (content.len() as u64).saturating_sub(off).min(len as u64) as usize;
        assert_eq!(n, expect, "off={off} len={len}");
        let start = (off as usize).min(content.len());
        assert_eq!(&buf[..n], &content[start..start + n]);
    }

    // 캐시 블록 재읽기: 같은 범위를 다시 읽어도 결과가 같아야 한다.
    buf.fill(0);
    let n1 = r.read_at(&mut buf[..500], 1234).unwrap();
    let n2 = r.read_at(&mut buf[..500], 1234).unwrap();
    assert_eq!(n1, n2);
    assert_eq!(&buf[..n1], &content[1234..1234 + n1]);
}

#[test]
fn s3_large_read_is_parallel() {
    let s = FakeS3::start();
    s.add_bucket("big");
    // 70MiB 객체: 64MiB 읽기가 8MiB 조각 8개로 나가야 한다.
    let content = {
        let mut v = Vec::with_capacity(70 * 1024 * 1024);
        let block = log_content(1024); // ~33KiB
        while v.len() < 70 * 1024 * 1024 {
            v.extend_from_slice(&block);
        }
        v
    };
    s.put_object("big", "big.log", &content);
    // 8개 조각 요청이 모두 도착할 때까지 응답을 보류해 병렬성을 결정적으로 검증한다.
    s.set_range_rendezvous(8);
    let pool = make_pool(&s);
    let r = pool
        .open_object(None, &S3Uri::parse("s3://big/big.log").unwrap())
        .unwrap();

    let mut buf = vec![0u8; 64 * 1024 * 1024];
    let n = r
        .read_at(&mut buf, 0)
        .unwrap_or_else(|e| panic!("64MiB read_at 실패: {e:?}"));
    assert_eq!(n, buf.len());
    assert_eq!(&buf[..], &content[..buf.len()]);

    // 기록된 Range GET 을 두 관점으로 나눠 검증한다.
    // 1) 레이아웃: 8MiB 조각 8개 범위가 모두 요청됐다 (SDK 재시도로 중복·추가 요청이 섞여도 된다).
    // 2) 병렬성: 랑데부 안에서 최소 2개 요청이 동시에 처리됐다.
    //    부하 상태에서는 도착이 어긋나 8개가 전부 겹치지 않을 수 있으므로
    //    "8개 동시" 대신 "직렬이 아님(≥2)" 을 요구해 타이밍 의존을 없앤다.
    let reqs: Vec<RecordedRequest> = s
        .requests()
        .into_iter()
        .filter(|q| q.method == "GET" && q.range.is_some())
        .collect();
    let seen: Vec<(u64, u64)> = reqs
        .iter()
        .map(|q| {
            let range = q.range.as_ref().unwrap();
            range["bytes=".len()..]
                .split_once('-')
                .map(|(a, b)| (a.parse::<u64>().unwrap(), b.parse::<u64>().unwrap()))
                .unwrap()
        })
        .collect();
    const CHUNK: u64 = 8 * 1024 * 1024;
    for i in 0..8u64 {
        let (a, b) = (i * CHUNK, (i + 1) * CHUNK - 1);
        assert!(seen.contains(&(a, b)), "8MiB 조각 요청 누락: bytes={a}-{b}");
    }
    assert!(
        s.max_inflight.load(Ordering::Relaxed) >= 2,
        "병렬 요청 미관측 (직렬 읽기로 추정): {}",
        s.max_inflight.load(Ordering::Relaxed)
    );
}

#[test]
fn s3_list_and_errors() {
    let s = FakeS3::start();
    s.add_bucket("bkt");
    s.set_page_size(3); // 페이지네이션 유도
    s.put_object("bkt", "logs/2024/a.log", b"a");
    s.put_object("bkt", "logs/2024/b.log", b"bb");
    s.put_object("bkt", "logs/app.log", b"ccc");
    s.put_object("bkt", "logs/x.log", b"dddd");
    s.put_object("bkt", "logs/", b""); // 폴더 마커
    s.put_object("bkt", "top.log", b"t");
    s.forbid_bucket("noaccess");
    let pool = make_pool(&s);

    // 버킷 목록
    assert_eq!(pool.list_buckets(None).unwrap(), vec!["bkt", "noaccess"]);

    // 루트: 디렉터리 먼저, 이름순.
    let root = pool.list_prefix(None, "bkt", "").unwrap();
    assert_eq!(
        root.iter()
            .map(|e| (e.name.as_str(), e.is_dir))
            .collect::<Vec<_>>(),
        vec![("logs", true), ("top.log", false)]
    );

    // logs/ 아래: 페이지네이션을 거쳐 전부 온다 (3개씩 × 2페이지).
    let entries = pool.list_prefix(None, "bkt", "logs/").unwrap();
    let names: Vec<(&str, bool)> = entries
        .iter()
        .map(|e| (e.name.as_str(), e.is_dir))
        .collect();
    assert_eq!(
        names,
        vec![("2024", true), ("app.log", false), ("x.log", false)]
    );
    // 수정 시각이 unix 초로 변환됐는지 확인 (2024-01-02T03:04:05Z).
    let file = entries.iter().find(|e| e.name == "app.log").unwrap();
    assert_eq!(file.size, 3);
    assert_eq!(file.mtime, Some(1_704_164_645));

    // 2페이지 이상 요청이 실제로 나갔는지 확인.
    let list_reqs = s
        .requests()
        .into_iter()
        .filter(|q| q.target.contains("list-type=2") && q.target.contains("bkt"))
        .count();
    assert!(list_reqs >= 2, "페이지네이션 요청 부족: {list_reqs}");

    // 폴더 안으로 한 단계 더.
    let inner = pool.list_prefix(None, "bkt", "logs/2024/").unwrap();
    assert_eq!(
        inner.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
        vec!["a.log", "b.log"]
    );

    // 403 → S3AccessDenied
    let err = pool.list_prefix(None, "noaccess", "").unwrap_err();
    assert!(matches!(err, RemoteError::S3AccessDenied(_)), "{err}");
    // 404 → S3NotFound (객체 / 버킷 모두)
    let err = pool
        .head(None, &S3Uri::parse("s3://bkt/none.log").unwrap())
        .unwrap_err();
    assert!(matches!(err, RemoteError::S3NotFound(_)), "{err}");
    let err = pool.list_prefix(None, "ghost", "").unwrap_err();
    assert!(matches!(err, RemoteError::S3NotFound(_)), "{err}");
}

#[test]
fn s3_logfile_parity() {
    let s = FakeS3::start();
    s.add_bucket("b1");
    let content = log_content(2000);
    s.put_object("b1", "app.log", &content);
    let pool = make_pool(&s);
    let r = pool
        .open_object(None, &S3Uri::parse("s3://b1/app.log").unwrap())
        .unwrap();

    // 로컬과 원격을 나란히 인덱싱해 결과를 비교한다.
    let tmp = TmpFile::new("parity", &content);
    let local = LogFile::open(&tmp.path).expect("로컬 열기 실패");
    let remote = LogFile::open_remote(r).expect("원격 열기 실패");
    let cancel = AtomicBool::new(false);
    local.build_index(&cancel, &mut |_| {}).unwrap();
    remote.build_index(&cancel, &mut |_| {}).unwrap();

    assert_eq!(remote.index().line_count(), local.index().line_count());
    for (start, count) in [(0u64, 10u64), (999, 20), (1990, 20)] {
        let rl = remote.read_lines(start, count, None, None).unwrap();
        let ll = local.read_lines(start, count, None, None).unwrap();
        assert_eq!(rl.len(), ll.len());
        for (a, b) in rl.iter().zip(ll.iter()) {
            assert_eq!(a.text, b.text, "줄 {} 불일치", a.no);
        }
    }

    // 검색 결과 비교
    let matcher = Matcher::new(
        "alpha",
        SearchOptions {
            regex: false,
            case_sensitive: true,
        },
    )
    .unwrap();
    let (mut rh, mut lh) = (Vec::new(), Vec::new());
    search(
        &remote,
        &matcher,
        &cancel,
        &mut |b| rh.extend_from_slice(b),
        &mut |_| {},
    )
    .unwrap();
    search(
        &local,
        &matcher,
        &cancel,
        &mut |b| lh.extend_from_slice(b),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(rh, lh);
    assert_eq!(rh.len(), 2000);
}

#[test]
fn s3_gzip_remote() {
    let s = FakeS3::start();
    s.add_bucket("b1");
    // "s3 gz line 000" .. "s3 gz line 099" 를 gzip -n 으로 압축한 고정 바이트.
    let gz = include_bytes!("fixtures/s3_gz_100lines.gz");
    s.put_object("b1", "logs.gz", gz);
    let pool = make_pool(&s);
    let r = pool
        .open_object(None, &S3Uri::parse("s3://b1/logs.gz").unwrap())
        .unwrap();

    assert!(is_gzip_reader(r.as_ref()).unwrap());
    let (file, writer) = open_gzip_remote(r).expect("open_gzip_remote 실패");
    let cancel = AtomicBool::new(false);
    let mut last = DecompressProgress {
        compressed_read: 0,
        compressed_total: 0,
        decompressed: 0,
        line_count: 0,
        done: false,
    };
    writer.run(&file, &cancel, &mut |p| last = p).unwrap();
    assert!(last.done);
    assert_eq!(file.index().line_count(), 100);
    let lines = file.read_lines(0, 2, None, None).unwrap();
    assert_eq!(lines[0].text, "s3 gz line 000");
    assert_eq!(lines[1].text, "s3 gz line 001");
}

/// 성능 참고치: 가짜 서버(루프백)로 1GiB 객체 인덱싱 시간을 잰다.
/// `cargo test -p riffle-remote --release -- --ignored` 로 실행.
#[test]
#[ignore = "성능 측정용"]
fn s3_index_1gb() {
    let s = FakeS3::start();
    s.add_bucket("big");
    let content = {
        let mut v = Vec::with_capacity(1 << 30);
        let line =
            b"2025-11-13T00:00:00Z INFO worker-01 request completed id=000000 duration=12ms\n";
        while v.len() < (1 << 30) {
            v.extend_from_slice(line);
        }
        v
    };
    s.put_object("big", "1gb.log", &content);
    let pool = make_pool(&s);
    let r = pool
        .open_object(None, &S3Uri::parse("s3://big/1gb.log").unwrap())
        .unwrap();
    let file = LogFile::open_remote(r).unwrap();
    let cancel = AtomicBool::new(false);
    let t = Instant::now();
    file.build_index(&cancel, &mut |_| {}).unwrap();
    eprintln!(
        "1GiB S3(가짜 서버) 인덱싱: {:?} ({}줄)",
        t.elapsed(),
        file.index().line_count()
    );
}
