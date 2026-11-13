//! S3 객체를 `RandomRead` 로 노출하고, 프로필·버킷 탐색 API 를 제공한다.
//!
//! - 큰 읽기(4MiB 이상): 8MiB 단위 Range GET 을 최대 16개까지 동시에 보낸다.
//! - 작은 읽기: 1MiB 블록 LRU 캐시(최대 64MiB)를 거친다.
//! - 일시 오류 재시도는 SDK 표준 재시도 정책(5xx·스로틀링·타임아웃)에 맡긴다.
//! - 버킷 리전은 HeadBucket 또는 리다이렉트 응답 헤더로 자동 결정해 캐시한다.
//! - 읽기 전용 API(HeadObject, GetObject, ListObjectsV2, ListBuckets, HeadBucket)만 호출한다.
//! - 모든 공개 메서드는 동기이며 내부 토키오 런타임(워커 2개)에서 실행한다.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use aws_config::BehaviorVersion;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use riffle_core::RandomRead;
use tokio::runtime::Runtime;
use tokio::task::JoinSet;

use crate::block_cache::BlockCache;
use crate::error::{
    RemoteError, map_s3_error, remote_to_io, s3_is_invalid_range, s3_redirect_region,
};

/// 이 크기 이상의 read_at 은 캐시 없이 병렬 Range GET 으로 처리한다.
const LARGE_READ: usize = 4 * 1024 * 1024;
/// 큰 읽기의 Range GET 조각 크기.
const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
/// 동시 Range GET 상한.
const READ_PARALLEL: usize = 16;
/// 작은 읽기가 거치는 캐시 블록 크기.
const BLOCK_SIZE: u64 = 1024 * 1024;
/// 블록 캐시 최대 총량.
const CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;
/// list_prefix 가 반환하는 항목 수 상한. 넘으면 나머지는 잘린다 (표시는 호출자 몫).
const LIST_MAX_ENTRIES: usize = 10_000;
/// 리전을 알 수 없을 때 첫 시도에 쓰는 리전. S3 는 잘못된 리전을 리다이렉트로 알려준다.
const DEFAULT_REGION: &str = "us-east-1";

/// `s3://버킷/키` 형태의 URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Uri {
    pub bucket: String,
    pub key: String,
}

impl S3Uri {
    /// `"s3://bucket/key"` 를 파싱한다. 키가 없으면 빈 문자열이다 (버킷 루트).
    /// 키 안의 `/`, 공백, 한글, 특수문자는 그대로 보존한다 (percent 인코딩하지 않는다).
    pub fn parse(s: &str) -> Option<S3Uri> {
        let rest = s.strip_prefix("s3://")?;
        let (bucket, key) = match rest.split_once('/') {
            Some((b, k)) => (b, k),
            None => (rest, ""),
        };
        if bucket.is_empty() {
            return None;
        }
        Some(S3Uri {
            bucket: bucket.to_string(),
            key: key.to_string(),
        })
    }
}

impl fmt::Display for S3Uri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s3://{}/{}", self.bucket, self.key)
    }
}

/// `list_prefix` 결과 한 항목. `name` 은 요청 prefix 기준 상대 이름이다.
#[derive(Debug, Clone)]
pub struct S3Entry {
    /// 접두사 기준 상대 이름. 폴더는 끝에 `/` 가 없다.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// unix 시간(초). S3 가 주지 않으면 `None`.
    pub mtime: Option<u64>,
}

/// `~/.aws/config` 와 `~/.aws/credentials` 에서 프로필 이름을 수집한다.
/// 정렬·중복 제거된 목록에 `"default"` 가 항상 포함된다.
/// 섹션 헤더만 읽고 자격 증명 값 등 내용은 보지 않는다.
pub fn list_aws_profiles() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return vec!["default".to_string()];
    };
    list_aws_profiles_from(
        Some(&home.join(".aws/config")),
        Some(&home.join(".aws/credentials")),
    )
}

/// `list_aws_profiles` 의 파일 경로를 지정하는 테스트용 변형.
pub fn list_aws_profiles_from(config: Option<&Path>, credentials: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = vec!["default".to_string()];
    if let Some(path) = config {
        for section in read_sections(path) {
            if section == "default" {
                names.push(section);
            } else if let Some(name) = section.strip_prefix("profile ") {
                names.push(unquote(name));
            }
            // sso-session, services 등 나머지 섹션은 프로필이 아니다.
        }
    }
    if let Some(path) = credentials {
        names.extend(read_sections(path).iter().map(|s| unquote(s)));
    }
    names.sort();
    names.dedup();
    names
}

/// INI 파일의 섹션 헤더 `[name]` 이름 목록.
fn read_sections(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
            Some(inner.to_string())
        })
        .collect()
}

/// 앞뒤 공백과 감싼 따옴표를 벗긴다.
fn unquote(s: &str) -> String {
    s.trim().trim_matches('"').trim().to_string()
}

struct S3PoolInner {
    rt: Runtime,
    /// 고정 엔드포인트(테스트용). `Some` 이면 path-style + 정적 자격 증명 + 고정 리전.
    endpoint: Option<String>,
    /// (프로필, 리전) → 클라이언트 캐시. endpoint 모드는 `("", "")` 하나만 쓴다.
    clients: StdMutex<HashMap<(String, String), Client>>,
    /// (프로필, 버킷) → 리전 캐시.
    regions: StdMutex<HashMap<(String, String), String>>,
}

impl S3PoolInner {
    fn new(endpoint: Option<String>) -> Arc<Self> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio 런타임 생성 실패");
        Arc::new(Self {
            rt,
            endpoint,
            clients: StdMutex::new(HashMap::new()),
            regions: StdMutex::new(HashMap::new()),
        })
    }

    /// 동기 API 가 내부 런타임 위에서 비동기 작업을 돌리기 위한 진입점.
    fn block_on<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    /// 프로필·리전에 맞는 클라이언트를 반환한다. 없으면 만들어 캐시한다.
    /// endpoint 모드에서는 프로필·리전을 무시하고 고정 클라이언트 하나를 재사용한다.
    async fn client_for(&self, profile: Option<&str>, region: Option<&str>) -> Client {
        let key = if self.endpoint.is_some() {
            (String::new(), String::new())
        } else {
            (
                profile.unwrap_or_default().to_string(),
                region.unwrap_or_default().to_string(),
            )
        };
        if let Some(c) = self.clients.lock().unwrap().get(&key) {
            return c.clone();
        }
        let client = match &self.endpoint {
            Some(url) => {
                let conf = aws_sdk_s3::config::Builder::new()
                    .endpoint_url(url.clone())
                    .force_path_style(true)
                    .region(Region::new(DEFAULT_REGION))
                    .credentials_provider(Credentials::new(
                        "riffle-test",
                        "riffle-test",
                        None,
                        None,
                        "riffle-test",
                    ))
                    .behavior_version(BehaviorVersion::latest())
                    .build();
                Client::from_conf(conf)
            }
            None => {
                let mut loader = aws_config::defaults(BehaviorVersion::latest());
                if let Some(p) = profile {
                    loader = loader.profile_name(p.to_string());
                }
                let conf = loader.load().await;
                // 우선순위: 명시 리전 > 프로필/환경 설정 > us-east-1.
                let region = region
                    .map(str::to_string)
                    .or_else(|| conf.region().map(|r| r.to_string()))
                    .unwrap_or_else(|| DEFAULT_REGION.to_string());
                let s3conf = aws_sdk_s3::config::Builder::from(&conf)
                    .region(Region::new(region))
                    .build();
                Client::from_conf(s3conf)
            }
        };
        self.clients.lock().unwrap().insert(key, client.clone());
        client
    }

    /// 버킷의 리전을 결정한다. 캐시 → HeadBucket(기본 리전) → 리다이렉트 헤더 순.
    /// HeadBucket 권한이 없는 계정을 위해 실패해도 기본 리전으로 진행해
    /// 실제 호출의 리다이렉트 응답이 올바른 리전을 알려주게 한다.
    async fn resolve_region(&self, profile: Option<&str>, bucket: &str) -> String {
        let key = (profile.unwrap_or_default().to_string(), bucket.to_string());
        if let Some(r) = self.regions.lock().unwrap().get(&key) {
            return r.clone();
        }
        let client = self.client_for(profile, None).await;
        let region = match client.head_bucket().bucket(bucket).send().await {
            Ok(out) => out.bucket_region().unwrap_or(DEFAULT_REGION).to_string(),
            Err(e) => s3_redirect_region(&e).unwrap_or_else(|| DEFAULT_REGION.to_string()),
        };
        self.regions.lock().unwrap().insert(key, region.clone());
        region
    }

    /// 버킷 리전의 클라이언트로 `op` 를 실행한다.
    /// 리전 리다이렉트 응답이 오면 캐시를 갱신하고 새 리전으로 1회 재시도한다.
    async fn run_op<F, Fut, T, E>(
        &self,
        profile: Option<&str>,
        bucket: &str,
        what: &str,
        op: F,
    ) -> Result<T, RemoteError>
    where
        F: Fn(Client) -> Fut,
        Fut: Future<Output = Result<T, SdkError<E>>>,
        E: ProvideErrorMetadata,
    {
        let mut region = self.resolve_region(profile, bucket).await;
        for attempt in 0..2 {
            let client = self.client_for(profile, Some(&region)).await;
            match op(client).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if attempt == 0
                        && let Some(new_region) = s3_redirect_region(&e).filter(|r| *r != region)
                    {
                        self.regions.lock().unwrap().insert(
                            (profile.unwrap_or_default().to_string(), bucket.to_string()),
                            new_region.clone(),
                        );
                        region = new_region;
                        continue;
                    }
                    return Err(map_s3_error(&e, what));
                }
            }
        }
        unreachable!()
    }

    async fn list_buckets_async(&self, profile: Option<&str>) -> Result<Vec<String>, RemoteError> {
        let client = self.client_for(profile, None).await;
        let out = client
            .list_buckets()
            .send()
            .await
            .map_err(|e| map_s3_error(&e, "버킷 목록"))?;
        let mut names: Vec<String> = out
            .buckets()
            .iter()
            .filter_map(|b| b.name().map(str::to_string))
            .collect();
        names.sort();
        Ok(names)
    }

    async fn list_prefix_async(
        &self,
        profile: Option<&str>,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<S3Entry>, RemoteError> {
        let mut entries = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page_token = token.clone();
            let out = self
                .run_op(profile, bucket, bucket, |client| {
                    let (bucket, prefix) = (bucket.to_string(), prefix.to_string());
                    let token = page_token.clone();
                    async move {
                        let mut req = client
                            .list_objects_v2()
                            .bucket(bucket)
                            .prefix(prefix)
                            .delimiter("/");
                        if let Some(t) = token {
                            req = req.continuation_token(t);
                        }
                        req.send().await
                    }
                })
                .await?;

            for cp in out.common_prefixes() {
                if let Some(name) = cp
                    .prefix()
                    .and_then(|p| p.strip_prefix(prefix))
                    .map(|s| s.trim_end_matches('/'))
                    .filter(|s| !s.is_empty())
                {
                    entries.push(S3Entry {
                        name: name.to_string(),
                        is_dir: true,
                        size: 0,
                        mtime: None,
                    });
                }
            }
            for obj in out.contents() {
                let key = obj.key().unwrap_or_default();
                // 요청한 접두사 자체(0바이트 폴더 마커)와 '/' 로 끝나는 마커는 제외한다.
                if key == prefix || key.ends_with('/') {
                    continue;
                }
                if let Some(name) = key.strip_prefix(prefix).filter(|s| !s.is_empty()) {
                    entries.push(S3Entry {
                        name: name.to_string(),
                        is_dir: false,
                        size: obj.size().unwrap_or(0).max(0) as u64,
                        mtime: obj.last_modified().map(|t| t.secs().max(0) as u64),
                    });
                }
            }

            if entries.len() >= LIST_MAX_ENTRIES {
                break;
            }
            match out.next_continuation_token() {
                Some(t) => token = Some(t.to_string()),
                None => break,
            }
        }
        // 디렉터리 우선, 그다음 이름순.
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        Ok(entries)
    }

    async fn head_async(&self, profile: Option<&str>, uri: &S3Uri) -> Result<u64, RemoteError> {
        let out = self
            .run_op(profile, &uri.bucket, &uri.to_string(), |client| {
                let (bucket, key) = (uri.bucket.clone(), uri.key.clone());
                async move { client.head_object().bucket(bucket).key(key).send().await }
            })
            .await?;
        Ok(out.content_length().unwrap_or(0).max(0) as u64)
    }
}

/// `s3://` 객체를 읽는 풀.
///
/// 내부에 멀티스레드 토키오 런타임(워커 2개)을 소유한다.
/// 모든 메서드는 블로킹이며, 무거운 호출은 호출 측에서 `spawn_blocking` 등으로 감싸는 것을 권장한다.
pub struct S3Pool {
    inner: Arc<S3PoolInner>,
}

impl S3Pool {
    /// AWS 기본 자격 증명 체인(프로필, SSO, `aws login`, 환경 변수)을 쓰는 풀.
    pub fn new() -> S3Pool {
        S3Pool {
            inner: S3PoolInner::new(None),
        }
    }

    /// 테스트용: 고정 endpoint(가짜 서버), path-style, 정적 자격 증명, 고정 리전.
    pub fn with_endpoint(endpoint_url: &str) -> S3Pool {
        S3Pool {
            inner: S3PoolInner::new(Some(endpoint_url.to_string())),
        }
    }

    /// 계정의 버킷 이름 목록. ListBuckets 권한이 없으면 오류를 반환한다.
    pub fn list_buckets(&self, profile: Option<&str>) -> Result<Vec<String>, RemoteError> {
        self.inner.block_on(self.inner.list_buckets_async(profile))
    }

    /// `prefix` 아래를 delimiter `/` 로 나열한다.
    /// 디렉터리 먼저, 이름순. 페이지네이션을 끝까지 따라가며 최대 10,000개까지 반환한다.
    pub fn list_prefix(
        &self,
        profile: Option<&str>,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<S3Entry>, RemoteError> {
        self.inner
            .block_on(self.inner.list_prefix_async(profile, bucket, prefix))
    }

    /// 객체 크기(바이트).
    pub fn head(&self, profile: Option<&str>, uri: &S3Uri) -> Result<u64, RemoteError> {
        self.inner.block_on(self.inner.head_async(profile, uri))
    }

    /// 객체를 `RandomRead` 로 연다. 먼저 HeadObject 로 크기를 확인한다.
    pub fn open_object(
        &self,
        profile: Option<&str>,
        uri: &S3Uri,
    ) -> Result<Arc<dyn RandomRead>, RemoteError> {
        let size = self.inner.block_on(self.inner.head_async(profile, uri))?;
        Ok(Arc::new(S3Object {
            inner: self.inner.clone(),
            profile: profile.map(str::to_string),
            uri: uri.clone(),
            uri_str: uri.to_string(),
            size: AtomicU64::new(size),
            cache: StdMutex::new(BlockCache::new(CACHE_MAX_BYTES)),
        }))
    }
}

impl Default for S3Pool {
    fn default() -> Self {
        Self::new()
    }
}

/// 한 Range GET 의 결과. 416 이면 빈 조각이다.
type RangeResult = Result<Vec<u8>, RemoteError>;

/// 범위 `[off, off+len)` 를 Range GET 으로 가져온다.
/// 요청 범위가 전부 객체 밖이면(객체가 교체돼 줄어든 경우) 빈 벡터를 반환한다.
async fn fetch_range(
    inner: &S3PoolInner,
    profile: Option<&str>,
    uri: &S3Uri,
    off: u64,
    len: u64,
) -> RangeResult {
    let range = format!("bytes={}-{}", off, off + len - 1);
    let what = uri.to_string();
    let out = inner
        .run_op(profile, &uri.bucket, &what, |client| {
            let (bucket, key, range) = (uri.bucket.clone(), uri.key.clone(), range.clone());
            async move {
                match client
                    .get_object()
                    .bucket(bucket)
                    .key(key)
                    .range(range)
                    .send()
                    .await
                {
                    Err(e) if s3_is_invalid_range(&e) => Ok(None),
                    r => r.map(Some),
                }
            }
        })
        .await?;
    let Some(out) = out else {
        return Ok(Vec::new());
    };
    let bytes = out
        .body
        .collect()
        .await
        .map_err(|e| RemoteError::S3(format!("{what}: 본문 읽기 실패: {e}")))?
        .into_bytes();
    Ok(bytes.to_vec())
}

/// 작업 범위를 블록 경계로 자른 조각.
struct Piece {
    /// 블록 시작 오프셋 (BLOCK_SIZE 정렬).
    bs: u64,
    /// 이 조각이 필요한 파일 범위 [ps, pe).
    ps: u64,
    pe: u64,
}

/// S3 객체의 `RandomRead` 구현. 객체는 불변이라고 가정한다 (크기 변화 시 캐시 전체 무효화).
struct S3Object {
    inner: Arc<S3PoolInner>,
    profile: Option<String>,
    uri: S3Uri,
    uri_str: String,
    /// 마지막으로 확인한 객체 크기.
    size: AtomicU64,
    cache: StdMutex<BlockCache>,
}

impl S3Object {
    /// 크기가 바뀌면(객체 교체) 캐시를 전부 지운다.
    fn note_size(&self, new: u64) {
        if self.size.swap(new, Ordering::Relaxed) != new {
            self.cache.lock().unwrap().clear();
        }
    }

    async fn stat_len_async(&self) -> io::Result<u64> {
        let size = self
            .inner
            .head_async(self.profile.as_deref(), &self.uri)
            .await
            .map_err(remote_to_io)?;
        self.note_size(size);
        Ok(size)
    }

    async fn read_at_async(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        // 알고 있는 크기로 클램프한다. 객체가 뒤에서 더 길어지는 일은 없으므로
        // 범위 초과 읽기는 그냥 짧게 반환한다.
        let size = self.size.load(Ordering::Relaxed);
        let want = size.saturating_sub(offset).min(buf.len() as u64) as usize;
        if want == 0 {
            return Ok(0);
        }
        let work = &mut buf[..want];
        if work.len() >= LARGE_READ {
            self.read_large(work, offset).await
        } else {
            self.read_small(work, offset).await
        }
    }

    /// 큰 읽기: work 를 CHUNK_SIZE 단위로 나눠 최대 READ_PARALLEL 개까지 동시에 가져온다.
    async fn read_large(&self, work: &mut [u8], offset: u64) -> io::Result<usize> {
        let total = work.len() as u64;
        let nchunks = total.div_ceil(CHUNK_SIZE) as usize;
        let ranges: Vec<(u64, u64)> = (0..nchunks)
            .map(|i| {
                let off = offset + i as u64 * CHUNK_SIZE;
                (off, (total - i as u64 * CHUNK_SIZE).min(CHUNK_SIZE))
            })
            .collect();
        let results = self.fetch_ranges(&ranges).await;

        // 앞에서부터 연속으로 채워진 길이를 반환한다.
        let mut filled = 0usize;
        for (i, res) in results.into_iter().enumerate() {
            let data = res?;
            let pos = i * CHUNK_SIZE as usize;
            let n = data.len().min(work.len() - pos);
            work[pos..pos + n].copy_from_slice(&data[..n]);
            filled += n;
            if n < ranges[i].1 as usize {
                break;
            }
        }
        Ok(filled)
    }

    /// 작은 읽기: BLOCK_SIZE 블록 캐시를 거쳐 필요한 블록만 채운다.
    async fn read_small(&self, work: &mut [u8], offset: u64) -> io::Result<usize> {
        let end = offset + work.len() as u64;
        let mut pieces = Vec::new();
        let mut bs = offset / BLOCK_SIZE * BLOCK_SIZE;
        while bs < end {
            pieces.push(Piece {
                bs,
                ps: offset.max(bs),
                pe: end.min(bs + BLOCK_SIZE),
            });
            bs += BLOCK_SIZE;
        }
        let size = self.size.load(Ordering::Relaxed);

        // 1) 캐시 조회: 히트는 즉시 복사하고, 미스 블록은 모아 둔다.
        let mut covered = vec![0u32; pieces.len()];
        let mut missing: Vec<u64> = Vec::new();
        {
            let mut cache = self.cache.lock().unwrap();
            for (i, p) in pieces.iter().enumerate() {
                if let Some(block) = cache.get(p.bs) {
                    apply_block(&mut covered, i, p, block, work, offset);
                } else {
                    missing.push(p.bs);
                }
            }
        }

        // 2) 미스 블록을 병렬 Range GET 으로 가져와 캐시에 넣고 복사한다.
        if !missing.is_empty() {
            let ranges: Vec<(u64, u64)> = missing
                .iter()
                .map(|&b| (b, BLOCK_SIZE.min(size.saturating_sub(b))))
                .collect();
            let results = self.fetch_ranges(&ranges).await;
            for (&b, res) in missing.iter().zip(results) {
                let data = res?;
                if let Some(i) = pieces.iter().position(|p| p.bs == b) {
                    apply_block(&mut covered, i, &pieces[i], &data, work, offset);
                }
                if !data.is_empty() {
                    self.cache.lock().unwrap().insert(b, data);
                }
            }
        }

        // 3) 앞에서부터 연속으로 채워진 길이를 반환한다.
        let mut filled = 0usize;
        for (i, p) in pieces.iter().enumerate() {
            filled += covered[i] as usize;
            if covered[i] < (p.pe - p.ps) as u32 {
                break;
            }
        }
        Ok(filled)
    }

    /// 범위 목록을 최대 READ_PARALLEL 개 동시 Range GET 으로 가져온다.
    /// 결과는 요청 순서대로 반환한다.
    async fn fetch_ranges(&self, ranges: &[(u64, u64)]) -> Vec<io::Result<Vec<u8>>> {
        let n = ranges.len();
        let mut out: Vec<Option<io::Result<Vec<u8>>>> = Vec::new();
        out.resize_with(n, || None);
        if n == 0 {
            return Vec::new();
        }

        let mut set = JoinSet::new();
        let mut next = 0usize;
        let mut finished = 0usize;
        while finished < n {
            while next < n && next - finished < READ_PARALLEL {
                let idx = next;
                let (off, len) = ranges[next];
                let inner = self.inner.clone();
                let profile = self.profile.clone();
                let uri = self.uri.clone();
                set.spawn(async move {
                    (
                        idx,
                        fetch_range(&inner, profile.as_deref(), &uri, off, len).await,
                    )
                });
                next += 1;
            }
            match set.join_next().await {
                Some(Ok((i, res))) => {
                    out[i] = Some(res.map_err(remote_to_io));
                    finished += 1;
                }
                // 태스크 패닉: 해당 슬롯은 None 으로 남겨 최종 변환에서 오류가 된다.
                Some(Err(_)) => finished += 1,
                None => break,
            }
        }

        out.into_iter()
            .map(|o| o.unwrap_or_else(|| Err(io::Error::other("읽기 작업이 중단되었습니다"))))
            .collect()
    }
}

/// 블록 데이터 `[bs, bs+data.len())` 와 피스 `[ps,pe)` 의 교집합을 work 에 복사하고
/// covered 를 갱신한다. 데이터는 항상 블록 시작부터 연속이라고 가정한다.
fn apply_block(
    covered: &mut [u32],
    piece_idx: usize,
    p: &Piece,
    data: &[u8],
    work: &mut [u8],
    offset: u64,
) {
    let avail_end = p.bs + data.len() as u64;
    let cov_end = p.pe.min(avail_end);
    if cov_end > p.ps {
        let buf_idx = (p.ps - offset) as usize;
        let src_idx = (p.ps - p.bs) as usize;
        let n = (cov_end - p.ps) as usize;
        work[buf_idx..buf_idx + n].copy_from_slice(&data[src_idx..src_idx + n]);
        covered[piece_idx] = n as u32;
    }
}

impl RandomRead for S3Object {
    fn stat_len(&self) -> io::Result<u64> {
        self.inner.block_on(self.stat_len_async())
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.inner.block_on(self.read_at_async(buf, offset))
    }

    fn uri(&self) -> String {
        self.uri_str.clone()
    }
}
