use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use riffle_core::{
    Columns, Combine, FilterRule, FilterSet, FilterSpec, Level, LineSet, LogFile, LogFormat,
    Matcher, RuleKind, SearchOptions,
};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri::ipc::Channel;
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::state::{AppState, Doc};

/// 파일 정보 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub id: u32,
    /// 파일 경로. 원격 파일이면 "sftp://호스트/경로" 또는 "s3://버킷/키" 형태의 URI
    pub path: String,
    pub size: u64,
    /// gzip 압축 파일 여부
    pub compressed: bool,
    /// 원격(sftp://, s3://) 파일 여부
    pub remote: bool,
    /// 파일 출처: "local" | "sftp" | "s3" | "merge"
    pub source: String,
}

/// 인덱싱/해제 진행률 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgressDto {
    pub indexed_bytes: u64,
    pub total_bytes: u64,
    pub line_count: u64,
    pub done: bool,
    /// 현재 읽기 가능한 바이트 수 (일반 파일: 파일 크기, gz: 해제된 크기)
    pub content_bytes: u64,
    /// gz 해제 진행 중 여부
    pub decompressing: bool,
    /// gz 해제 실패/디스크 부족 등 오류 메시지 (그 전까지 내용은 유효)
    pub error: Option<String>,
}

impl From<riffle_core::IndexProgress> for IndexProgressDto {
    fn from(progress: riffle_core::IndexProgress) -> Self {
        Self {
            indexed_bytes: progress.indexed_bytes,
            total_bytes: progress.total_bytes,
            line_count: progress.line_count,
            done: progress.done,
            content_bytes: progress.total_bytes,
            decompressing: false,
            error: None,
        }
    }
}

/// 한 줄 정보 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineDto {
    pub no: u64,
    pub text: String,
    pub byte_len: u64,
    pub truncated: bool,
    pub highlights: Vec<(u32, u32)>,
    /// 활성 포함 규칙의 매치 범위 (UTF-16 start, end, 규칙 인덱스)
    pub rule_highlights: Vec<(u32, u32, u32)>,
    /// 컬럼 뷰용 분할 값. with_fields=true이고 파일 형식이 plain이 아닐 때만 채운다.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    /// 병합 뷰에서 원본 순번 (병합 파일이 아니면 null)
    pub source: Option<u16>,
    /// 병합 뷰에서 원본 줄 번호 (0-based, 병합 파일이 아니면 null)
    pub source_line: Option<u64>,
}

impl From<riffle_core::Line> for LineDto {
    fn from(line: riffle_core::Line) -> Self {
        Self {
            no: line.no,
            text: line.text,
            byte_len: line.byte_len,
            truncated: line.truncated,
            highlights: line.highlights,
            rule_highlights: line.rule_highlights,
            fields: None,
            source: None,
            source_line: None,
        }
    }
}

/// 검색 옵션 및 쿼리 DTO
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryDto {
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
}

impl From<&QueryDto> for SearchOptions {
    fn from(dto: &QueryDto) -> Self {
        Self {
            regex: dto.regex,
            case_sensitive: dto.case_sensitive,
        }
    }
}

/// 필터 규칙 DTO.
/// color는 프론트 표시용이며 Rust는 저장만 하고 판정에 사용하지 않는다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterRuleDto {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// "include" | "exclude"
    pub kind: String,
    pub enabled: bool,
    /// 규칙별 하이라이트 색상
    pub color: String,
    /// 컬럼 뷰 필드 필터: 지정된 컬럼 값에만 규칙을 적용한다 (None이면 줄 전체).
    /// 이전 버전 프리셋에는 이 키가 없으므로 역직렬화 시 기본값을 허용한다.
    #[serde(default)]
    pub field: Option<String>,
}

/// 필터 조건 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterSpecDto {
    pub rules: Vec<FilterRuleDto>,
    /// "and" | "or"
    pub combine: String,
    /// "error" | "warn" | "info" | "debug" | "trace"
    pub levels: Vec<String>,
}

/// 파일의 컬럼 정보 DTO (컬럼 뷰용 형식 감지 결과)
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ColumnsDto {
    /// "plain" | "csv" | "jsonl"
    pub format: String,
    /// CSV 구분자 문자열 (plain/jsonl이면 None)
    pub delimiter: Option<String>,
    /// CSV 첫 줄이 헤더로 감지됐는지 여부
    pub has_header: bool,
    /// 컬럼 이름 목록 (표시 순서, plain이면 빈 목록)
    pub names: Vec<String>,
}

impl From<&Columns> for ColumnsDto {
    fn from(cols: &Columns) -> Self {
        let (format, delimiter, has_header) = match &cols.format {
            LogFormat::Plain => ("plain", None, false),
            LogFormat::Csv {
                delimiter,
                has_header,
            } => ("csv", Some((*delimiter as char).to_string()), *has_header),
            LogFormat::JsonLines => ("jsonl", None, false),
        };
        Self {
            format: format.to_string(),
            delimiter,
            has_header,
            names: cols.names.clone(),
        }
    }
}

/// 필터 프리셋 DTO (이름 + 조건)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterPresetDto {
    pub name: String,
    pub spec: FilterSpecDto,
}

/// 필터 상태 조회 결과 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterInfoDto {
    pub id: u32,
    pub file_id: u32,
    pub spec: FilterSpecDto,
    /// 현재까지 판정된 보이는 줄 수
    pub total: u64,
    pub done: bool,
    pub capped: bool,
}

/// 규칙 종류 문자열을 코어 RuleKind로 변환한다.
fn rule_kind_of(s: &str) -> Result<RuleKind, String> {
    match s {
        "include" => Ok(RuleKind::Include),
        "exclude" => Ok(RuleKind::Exclude),
        _ => Err(format!("알 수 없는 필터 규칙 종류: '{s}'")),
    }
}

/// 결합 방식 문자열을 코어 Combine으로 변환한다.
fn combine_of(s: &str) -> Result<Combine, String> {
    match s {
        "and" => Ok(Combine::And),
        "or" => Ok(Combine::Or),
        _ => Err(format!("알 수 없는 필터 결합 방식: '{s}'")),
    }
}

/// 레벨 문자열을 코어 Level로 변환한다.
fn level_of(s: &str) -> Result<Level, String> {
    match s {
        "error" => Ok(Level::Error),
        "warn" => Ok(Level::Warn),
        "info" => Ok(Level::Info),
        "debug" => Ok(Level::Debug),
        "trace" => Ok(Level::Trace),
        _ => Err(format!("알 수 없는 로그 레벨: '{s}'")),
    }
}

/// DTO 필터 조건을 코어 FilterSpec으로 변환한다.
/// 알 수 없는 kind/combine/level 문자열은 Err를 반환한다.
pub fn filter_spec_of(dto: &FilterSpecDto) -> Result<FilterSpec, String> {
    let rules = dto
        .rules
        .iter()
        .map(|r| {
            Ok(FilterRule {
                pattern: r.pattern.clone(),
                options: SearchOptions {
                    regex: r.regex,
                    case_sensitive: r.case_sensitive,
                },
                kind: rule_kind_of(&r.kind)?,
                enabled: r.enabled,
                field: r.field.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let combine = combine_of(&dto.combine)?;
    let levels = dto
        .levels
        .iter()
        .map(|l| level_of(l))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(FilterSpec {
        rules,
        combine,
        levels,
    })
}

/// spec이 유효하면 컴파일된 FilterSet을 반환한다.
/// 무효한 spec(나쁜 정규식, 알 수 없는 값)이면 None — 하이라이트 없이 진행하기 위함.
/// 필드 규칙이 있으면 파일의 컬럼 정보(columns)가 있어야 컴파일된다.
fn compile_filter_set(
    spec: Option<&FilterSpecDto>,
    columns: Option<&Columns>,
) -> Option<FilterSet> {
    let dto = spec?;
    let core = filter_spec_of(dto).ok()?;
    FilterSet::new(&core, columns).ok()
}

/// LineSet을 bins 구간으로 나눠 각 구간의 보이는 줄 수를 계산한다.
/// 각 bin의 줄 범위 [lo, hi) 안 원소 수 = rank(hi) - rank(lo).
/// bin 경계는 compute_search_density와 같은 floor(line * bins / total_lines) 규칙이다.
pub fn compute_filter_density(lines: &LineSet, bins: u32, total_lines: u64) -> Vec<u32> {
    let bins = bins.min(2000);
    if bins == 0 {
        return Vec::new();
    }
    let mut counts = vec![0u32; bins as usize];
    if total_lines == 0 {
        return counts;
    }

    let bins_u128 = bins as u128;
    let total_u128 = total_lines as u128;

    for b in 0..bins as u64 {
        // bin b에 속하는 줄 l: floor(l * bins / total) == b
        // → l ∈ [ceil(b*total/bins), ceil((b+1)*total/bins))
        let lo = (b as u128 * total_u128).div_ceil(bins_u128);
        let hi = ((b as u128 + 1) * total_u128).div_ceil(bins_u128);
        if hi <= lo {
            continue;
        }
        let lo = lo.min(u64::MAX as u128) as u64;
        let hi = hi.min(u64::MAX as u128) as u64;
        let cnt = lines.rank(hi).saturating_sub(lines.rank(lo));
        counts[b as usize] = cnt.min(u32::MAX as u64) as u32;
    }

    counts
}

/// 검색 진행 이벤트 스트림 DTO
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum SearchEvent {
    Progress {
        scanned_bytes: u64,
        total_bytes: u64,
        match_count: u64,
    },
    Done {
        total: u64,
        capped: bool,
    },
    Cancelled,
    Error {
        message: String,
    },
}

/// copy_lines로 복사할 수 있는 최대 범위 (100MB)
pub const MAX_COPY_BYTES: u64 = 100 * 1024 * 1024;
/// get_full_line이 읽는 최대 바이트 (10MB)
pub const MAX_FULL_LINE_BYTES: u64 = 10 * 1024 * 1024;

/// copy_lines 결과 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyResult {
    /// 실제로 클립보드에 기록된 줄 수
    pub lines: u64,
    /// 기록된 UTF-8 바이트 수
    pub bytes: u64,
}

/// get_full_line 결과 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullLine {
    /// 줄 번호 (0-based)
    pub no: u64,
    /// 원문 텍스트 (줄 끝 '\r' 제거, 최대 MAX_FULL_LINE_BYTES까지)
    pub text: String,
    /// max_bytes 초과로 잘렸는지 여부
    pub truncated: bool,
    /// 원본 줄의 바이트 길이 (\n 제외, \r 포함)
    pub byte_len: u64,
}

/// 검색 결과 위치 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// 결과 목록 내 위치 (0-based)
    pub index: u64,
    /// 파일 내 실제 줄 번호 (0-based)
    pub line: u64,
}

/// get_lines 요청 시 한 번에 가져올 수 있는 최대 줄 수
pub const MAX_GET_LINES_COUNT: u32 = 1000;

/// get_lines의 count 인자를 상한선(1000)으로 제한한다.
#[inline]
pub fn clamp_get_lines_count(count: u32) -> u32 {
    count.min(MAX_GET_LINES_COUNT)
}

/// 오름차순 정렬된 매치 줄 번호 목록에서 forward/backward 방향으로 다음 결과를 이진 탐색한다.
pub fn compute_search_nav(results: &[u64], from_line: u64, forward: bool) -> Option<SearchHit> {
    if results.is_empty() {
        return None;
    }
    if forward {
        let idx = results.partition_point(|&line| line <= from_line);
        if idx < results.len() {
            Some(SearchHit {
                index: idx as u64,
                line: results[idx],
            })
        } else {
            None
        }
    } else {
        let idx = results.partition_point(|&line| line < from_line);
        if idx > 0 {
            let hit_idx = idx - 1;
            Some(SearchHit {
                index: hit_idx as u64,
                line: results[hit_idx],
            })
        } else {
            None
        }
    }
}

/// 결과 목록의 특정 인덱스에 해당하는 SearchHit를 조회한다.
pub fn compute_search_hit_at(results: &[u64], index: u64) -> Option<SearchHit> {
    results
        .get(index as usize)
        .map(|&line| SearchHit { index, line })
}

/// 전체 줄 수와 bins 개수를 기준으로 결과 목록을 순회하여 각 구간별 매치 줄 수를 계산한다.
pub fn compute_search_density(results: &[u64], bins: u32, total_lines: u64) -> Vec<u32> {
    let bins = bins.min(2000);
    if bins == 0 {
        return Vec::new();
    }
    let mut counts = vec![0u32; bins as usize];
    if total_lines == 0 || results.is_empty() {
        return counts;
    }

    let bins_u128 = bins as u128;
    let total_lines_u128 = total_lines as u128;

    for &line in results {
        let bin = ((line as u128 * bins_u128) / total_lines_u128) as usize;
        let bin = bin.min(bins as usize - 1);
        counts[bin] = counts[bin].saturating_add(1);
    }

    counts
}

/// 파일 열기 결과. gzip 파일이면 gz_writer가 Some이며,
/// 해제는 호출 측이 백그라운드 스레드에서 run_gz_decompression으로 실행한다.
pub struct OpenedFile {
    pub info: FileInfo,
    pub file: Arc<LogFile>,
    pub cancel: Arc<AtomicBool>,
    pub gz_writer: Option<riffle_core::GzWriter>,
}

/// 경로가 원격(sftp://, s3://) URI인지 반환한다.
/// open_file 라우팅과 canonicalize 우회 판별에 공통으로 사용한다.
pub fn is_remote_uri(path: &str) -> bool {
    path.starts_with("sftp://") || path.starts_with("s3://")
}

/// 파일 열기 내부 로직: 파일을 열고(필요시 gzip 해제 준비) AppState에 등록한다.
/// path가 "sftp://"로 시작하면 원격(SFTP), "s3://"로 시작하면 S3 객체로 라우팅한다.
pub fn open_file_internal(state: &AppState, path: &str) -> Result<OpenedFile, String> {
    if path.starts_with("sftp://") {
        return crate::remote::open_remote_internal(state, path);
    }
    if path.starts_with("s3://") {
        return crate::s3::open_s3_internal(state, path);
    }
    let file_path = Path::new(path);
    let gz = riffle_core::is_gzip(file_path)
        .map_err(|e| format!("파일을 열 수 없습니다 ('{path}'): {e}"))?;

    let (log_file, gz_writer, size) = if gz {
        let (file, writer) = riffle_core::open_gzip(file_path)
            .map_err(|e| format!("gzip 파일을 열 수 없습니다 ('{path}'): {e}"))?;
        // FileInfo.size는 디스크 실제 파일 크기(압축 크기)를 표시한다
        let size = std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);
        (file, Some(writer), size)
    } else {
        let file = LogFile::open(file_path)
            .map_err(|e| format!("파일을 열 수 없습니다 ('{path}'): {e}"))?;
        let size = file.source().len();
        (file, None, size)
    };

    let file = Arc::new(log_file);
    let cancel = Arc::new(AtomicBool::new(false));
    let compressed = gz_writer.is_some();
    let id = state.insert_file(
        file.clone(),
        cancel.clone(),
        PathBuf::from(path),
        compressed,
    );

    Ok(OpenedFile {
        info: FileInfo {
            id,
            path: path.to_string(),
            size,
            compressed,
            remote: false,
            source: "local".to_string(),
        },
        file,
        cancel,
        gz_writer,
    })
}

/// core Line을 LineDto로 변환한다.
/// columns가 plain이 아닌 형식이면 컬럼 뷰용 분할 값(fields)도 채운다.
fn line_dto_with_fields(line: riffle_core::Line, columns: Option<&Columns>) -> LineDto {
    let mut dto = LineDto::from(line);
    if let Some(cols) = columns
        && cols.format != LogFormat::Plain
    {
        dto.fields = Some(riffle_core::split_fields(&dto.text, cols));
    }
    dto
}

/// LogFile 인스턴스에서 지정한 범위의 줄들을 읽어 LineDto 벡터로 변환한다.
/// filter가 주어지면 각 줄의 활성 포함 규칙 매치 범위(rule_highlights)를 채운다.
/// columns가 plain이 아니면 각 줄의 컬럼 분할 값(fields)도 채운다.
pub fn read_lines_from_file(
    file: &LogFile,
    start: u64,
    count: u32,
    matcher: Option<&Matcher>,
    filter: Option<&FilterSet>,
    columns: Option<&Columns>,
) -> Result<Vec<LineDto>, String> {
    let lines = file
        .read_lines(start, count as u64, matcher, filter)
        .map_err(|e| format!("줄 읽기 실패: {e}"))?;
    Ok(lines
        .into_iter()
        .map(|l| line_dto_with_fields(l, columns))
        .collect())
}

/// LogFile 인스턴스에서 임의 줄 번호 목록을 읽어 LineDto 벡터로 변환한다.
/// filter가 주어지면 각 줄의 활성 포함 규칙 매치 범위(rule_highlights)를 채운다.
/// columns가 plain이 아니면 각 줄의 컬럼 분할 값(fields)도 채운다.
pub fn read_filtered_lines_from_file(
    file: &LogFile,
    line_nos: &[u64],
    matcher: Option<&Matcher>,
    filter: Option<&FilterSet>,
    columns: Option<&Columns>,
) -> Result<Vec<LineDto>, String> {
    let lines = file
        .read_lines_at(line_nos, matcher, filter)
        .map_err(|e| format!("필터 줄 읽기 실패: {e}"))?;
    Ok(lines
        .into_iter()
        .map(|l| line_dto_with_fields(l, columns))
        .collect())
}

/// 백그라운드 인덱싱 스레드 실행 로직.
pub fn run_indexing(
    file: Arc<LogFile>,
    cancel: Arc<AtomicBool>,
    on_progress: Channel<IndexProgressDto>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut is_first = true;
        let mut last_send = Instant::now();

        file.build_index(&cancel, &mut |progress| {
            let now = Instant::now();
            let is_done = progress.done;
            let should_send =
                is_first || is_done || now.duration_since(last_send) >= Duration::from_millis(50);

            if should_send {
                is_first = false;
                last_send = now;
                let dto = IndexProgressDto::from(progress);
                if let Err(e) = on_progress.send(dto) {
                    eprintln!("인덱싱 진행률 채널 전송 오류: {e}");
                }
            }
        })
    }));

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("인덱싱 오류 발생: {e}");
        }
        Err(panic_payload) => {
            eprintln!("인덱싱 스레드 패닉 발생: {panic_payload:?}");
        }
    }
}

/// gzip 해제 진행률을 IndexProgressDto로 변환해 전송한다.
/// indexedBytes/totalBytes는 압축 원본 기준, contentBytes는 해제된 크기,
/// decompressing은 해제 진행 중 true. 첫 이벤트와 마지막 이벤트는 반드시 전송한다.
fn gz_progress_dto(p: riffle_core::DecompressProgress, decompressing: bool) -> IndexProgressDto {
    IndexProgressDto {
        indexed_bytes: p.compressed_read,
        total_bytes: p.compressed_total,
        line_count: p.line_count,
        done: p.done,
        content_bytes: p.decompressed,
        decompressing,
        error: None,
    }
}

/// 백그라운드 gzip 해제 스레드 실행 로직.
/// 해제와 인덱싱 확장은 GzWriter::run 안에서 함께 진행되며,
/// 진행률은 50ms 스로틀로 전송한다 (첫/마지막 이벤트는 항상 전송).
/// 종료 시 decompressing=false와 오류(있으면)를 담은 최종 이벤트를 한 번 더 보낸다.
pub fn run_gz_decompression(
    writer: riffle_core::GzWriter,
    file: Arc<LogFile>,
    cancel: Arc<AtomicBool>,
    on_progress: Channel<IndexProgressDto>,
) {
    let mut last_prog = riffle_core::DecompressProgress {
        compressed_read: 0,
        compressed_total: 0,
        decompressed: 0,
        line_count: 0,
        done: false,
    };
    let mut is_first = true;
    let mut last_send = Instant::now();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        writer.run(&file, &cancel, &mut |progress| {
            last_prog = progress;
            let now = Instant::now();
            let should_send = is_first
                || progress.done
                || now.duration_since(last_send) >= Duration::from_millis(50);

            if should_send {
                is_first = false;
                last_send = now;
                if let Err(e) = on_progress.send(gz_progress_dto(progress, true)) {
                    eprintln!("gzip 해제 진행률 채널 전송 오류: {e}");
                }
            }
        })
    }));

    // 최종 이벤트: decompressing=false. 오류는 error 필드로 알리고
    // 그 전까지 해제·인덱싱된 내용은 그대로 유효하다.
    let error = match result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(panic_payload) => {
            let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "gzip 해제 스레드 실행 중 알 수 없는 패닉이 발생했습니다.".to_string()
            };
            Some(msg)
        }
    };

    let mut dto = gz_progress_dto(last_prog, false);
    dto.done = last_prog.done && error.is_none();
    dto.error = error;
    if let Err(e) = on_progress.send(dto) {
        eprintln!("gzip 해제 최종 진행률 채널 전송 오류: {e}");
    }
}

/// 로그 파일을 열고 즉시 파일 정보를 반환하며, 백그라운드에서 인덱싱을 시작한다.
/// 성공 시 최근 파일 목록을 갱신하고 네이티브 메뉴를 재구성한다.
/// 파일 열기와 recent.json 읽기-수정-쓰기는 spawn_blocking에서 수행한다.
#[tauri::command]
pub async fn open_file(
    app: tauri::AppHandle,
    _state: tauri::State<'_, AppState>,
    path: String,
    on_progress: Channel<IndexProgressDto>,
) -> Result<FileInfo, String> {
    // 파일 열기(무거운 IO)를 블로킹 스레드에서 수행.
    // AppHandle을 통해 블로킹 스레드 안에서 AppState를 다시 얻는다.
    // 탭 중복 판별과 최근 파일 기록에 쓸 수 있게 canonical 경로로 변환한다
    // (심볼릭 링크 등으로 같은 파일을 가리키는 경로를 하나로 맞춤, 실패 시 원래 경로).
    // sftp://, s3:// 원격 URI는 로컬 경로가 아니므로 canonicalize하지 않는다.
    let open_app = app.clone();
    let open_path = path;
    let opened = tauri::async_runtime::spawn_blocking(move || {
        let st = open_app.state::<AppState>();
        let target = if is_remote_uri(&open_path) {
            open_path
        } else {
            std::fs::canonicalize(&open_path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(open_path)
        };
        open_file_internal(st.inner(), &target)
    })
    .await
    .map_err(|e| format!("파일 열기 비동기 태스크 실패: {e}"))??;

    match opened.gz_writer {
        Some(writer) => {
            let file = opened.file;
            let cancel = opened.cancel;
            std::thread::spawn(move || {
                run_gz_decompression(writer, file, cancel, on_progress);
            });
        }
        None => {
            let file = opened.file;
            let cancel = opened.cancel;
            std::thread::spawn(move || {
                run_indexing(file, cancel, on_progress);
            });
        }
    }

    // 최근 파일 기록 + 메뉴의 "최근 파일" 서브메뉴 재구성.
    // 기록/메뉴 갱신 실패는 파일 열기 자체를 실패시키지 않는다.
    // FileInfo.path는 canonical 경로이므로 그대로 기록한다.
    let recent_app = app.clone();
    let recent_path = opened.info.path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::recent::with_recent_lock(|| {
            match crate::recent::record_open(&recent_app, &recent_path) {
                Ok(list) => {
                    // 목록이 이전과 같으면 메뉴 재구성을 생략한다
                    let prev = recent_app.state::<AppState>().recent_files();
                    recent_app
                        .state::<AppState>()
                        .set_recent_files(list.clone());
                    if list != prev
                        && let Err(e) = crate::menu::apply_menu(&recent_app, &list)
                    {
                        eprintln!("메뉴 갱신 실패: {e}");
                    }
                }
                Err(e) => eprintln!("최근 파일 기록 실패: {e}"),
            }
        })
    })
    .await
    .map_err(|e| format!("최근 파일 기록 비동기 태스크 실패: {e}"))?;

    Ok(opened.info)
}

/// 지정된 범위의 로그 줄들을 읽어 반환한다.
/// 전달된 query가 무효한 경우(문법 오류 등) 에러 대신 하이라이트 없이 정상 줄을 반환한다.
/// spec이 오면 유효한 규칙의 매치 범위를 rule_highlights로 채운다 (무효 spec이면 비워둠).
/// with_fields=true이고 파일 형식이 plain이 아니면 각 줄의 컬럼 분할 값(fields)도 채운다.
#[tauri::command]
pub async fn get_lines(
    state: tauri::State<'_, AppState>,
    id: u32,
    start: u64,
    count: u32,
    query: Option<QueryDto>,
    spec: Option<FilterSpecDto>,
    with_fields: Option<bool>,
) -> Result<Vec<LineDto>, String> {
    let doc = state.get_doc(id)?;
    // 컬럼 캐시는 단일 파일만 가진다 (병합 문서는 컬럼 미지원)
    let columns_cache = match &doc {
        Doc::File(_) => Some(state.get_columns_handle(id)?.1),
        Doc::Merge(_) => None,
    };
    let clamped_count = clamp_get_lines_count(count);
    // 컬럼 분할 요청이거나 spec에 필드 규칙이 있으면 컬럼 정보가 필요하다
    let need_columns = with_fields.unwrap_or(false)
        || spec
            .as_ref()
            .is_some_and(|s| s.rules.iter().any(|r| r.field.is_some()));

    tauri::async_runtime::spawn_blocking(move || {
        let matcher = match query {
            Some(ref q) => Matcher::new(&q.query, q.into()).ok(),
            None => None,
        };
        match doc {
            Doc::File(file) => {
                let columns = if need_columns {
                    Some(crate::state::detect_columns_cached(
                        &file,
                        &columns_cache.expect("단일 파일은 컬럼 캐시가 있다"),
                    )?)
                } else {
                    None
                };
                let highlight_filter = compile_filter_set(spec.as_ref(), columns.as_ref());
                read_lines_from_file(
                    &file,
                    start,
                    clamped_count,
                    matcher.as_ref(),
                    highlight_filter.as_ref(),
                    columns.as_ref(),
                )
            }
            Doc::Merge(m) => {
                // 병합 위치 → 원본 줄로 해석해 읽는다 (no는 병합 위치)
                let positions: Vec<u64> = (start..start + clamped_count as u64).collect();
                let highlight_filter = compile_filter_set(spec.as_ref(), None);
                crate::merge::read_merged_positions(
                    &m.index,
                    &positions,
                    matcher.as_ref(),
                    highlight_filter.as_ref(),
                )
            }
        }
    })
    .await
    .map_err(|e| format!("줄 읽기 비동기 태스크 실패: {e}"))?
}

/// 필터 모드: 검색 결과 목록의 [start_index, start_index + count) 줄들을 읽어 반환한다.
/// 전달된 query가 무효한 경우 에러 대신 하이라이트 없이 정상 줄을 반환한다.
/// spec이 오면 유효한 규칙의 매치 범위를 rule_highlights로 채운다 (무효 spec이면 비워둠).
/// with_fields=true이고 파일 형식이 plain이 아니면 각 줄의 컬럼 분할 값(fields)도 채운다.
// IPC 인자는 프론트 계약이라 묶을 수 없다
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn get_filtered_lines(
    state: tauri::State<'_, AppState>,
    id: u32,
    search_id: u32,
    start_index: u64,
    count: u32,
    query: Option<QueryDto>,
    spec: Option<FilterSpecDto>,
    with_fields: Option<bool>,
) -> Result<Vec<LineDto>, String> {
    let doc = state.get_doc(id)?;
    let columns_cache = match &doc {
        Doc::File(_) => Some(state.get_columns_handle(id)?.1),
        Doc::Merge(_) => None,
    };
    let search = state.get_search(search_id)?;
    if search.file_id != id {
        return Err(format!(
            "검색 ID {}는 파일 ID {}에 속하지 않습니다.",
            search_id, id
        ));
    }

    let clamped_count = count.min(1000);
    let line_nos = {
        let results = search.results.read().map_err(|e| e.to_string())?;
        let start = start_index as usize;
        if start >= results.len() {
            Vec::new()
        } else {
            let end = (start + clamped_count as usize).min(results.len());
            results[start..end].to_vec()
        }
    };

    // 컬럼 분할 요청이거나 spec에 필드 규칙이 있으면 컬럼 정보가 필요하다
    let need_columns = with_fields.unwrap_or(false)
        || spec
            .as_ref()
            .is_some_and(|s| s.rules.iter().any(|r| r.field.is_some()));

    tauri::async_runtime::spawn_blocking(move || {
        let matcher = match query {
            Some(ref q) => Matcher::new(&q.query, q.into()).ok(),
            None => None,
        };
        match doc {
            Doc::File(file) => {
                let columns = if need_columns {
                    Some(crate::state::detect_columns_cached(
                        &file,
                        &columns_cache.expect("단일 파일은 컬럼 캐시가 있다"),
                    )?)
                } else {
                    None
                };
                let highlight_filter = compile_filter_set(spec.as_ref(), columns.as_ref());
                read_filtered_lines_from_file(
                    &file,
                    &line_nos,
                    matcher.as_ref(),
                    highlight_filter.as_ref(),
                    columns.as_ref(),
                )
            }
            Doc::Merge(m) => {
                // 검색 결과는 병합 위치 — 원본 줄로 해석해 읽는다
                let highlight_filter = compile_filter_set(spec.as_ref(), None);
                crate::merge::read_merged_positions(
                    &m.index,
                    &line_nos,
                    matcher.as_ref(),
                    highlight_filter.as_ref(),
                )
            }
        }
    })
    .await
    .map_err(|e| format!("필터 줄 읽기 비동기 태스크 실패: {e}"))?
}

/// 다중 필터 모드: 필터 결과 목록의 [start_index, start_index + count) 줄들을 읽어 반환한다.
/// query는 검색 하이라이트용이며, spec은 규칙 하이라이트(rule_highlights)용이다.
/// 둘 다 무효하면 에러 대신 하이라이트 없이 정상 줄을 반환한다.
/// with_fields=true이고 파일 형식이 plain이 아니면 각 줄의 컬럼 분할 값(fields)도 채운다.
// IPC 인자는 프론트 계약이라 묶을 수 없다
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn get_filter_lines(
    state: tauri::State<'_, AppState>,
    id: u32,
    filter_id: u32,
    start_index: u64,
    count: u32,
    query: Option<QueryDto>,
    spec: Option<FilterSpecDto>,
    with_fields: Option<bool>,
) -> Result<Vec<LineDto>, String> {
    let doc = state.get_doc(id)?;
    let columns_cache = match &doc {
        Doc::File(_) => Some(state.get_columns_handle(id)?.1),
        Doc::Merge(_) => None,
    };
    let filter = state.get_filter(filter_id)?;
    if filter.file_id != id {
        return Err(format!(
            "필터 ID {}는 파일 ID {}에 속하지 않습니다.",
            filter_id, id
        ));
    }

    let clamped_count = clamp_get_lines_count(count);
    let line_nos = {
        let lines = filter.lines.read().map_err(|e| e.to_string())?;
        let len = lines.len() as usize;
        let start = start_index as usize;
        if start >= len {
            Vec::new()
        } else {
            let end = (start + clamped_count as usize).min(len);
            (start..end).filter_map(|i| lines.nth(i as u64)).collect()
        }
    };

    // 컬럼 분할 요청이거나 spec에 필드 규칙이 있으면 컬럼 정보가 필요하다
    let need_columns = with_fields.unwrap_or(false)
        || spec
            .as_ref()
            .is_some_and(|s| s.rules.iter().any(|r| r.field.is_some()));

    tauri::async_runtime::spawn_blocking(move || {
        let matcher = match query {
            Some(ref q) => Matcher::new(&q.query, q.into()).ok(),
            None => None,
        };
        match doc {
            Doc::File(file) => {
                let columns = if need_columns {
                    Some(crate::state::detect_columns_cached(
                        &file,
                        &columns_cache.expect("단일 파일은 컬럼 캐시가 있다"),
                    )?)
                } else {
                    None
                };
                let highlight_filter = compile_filter_set(spec.as_ref(), columns.as_ref());
                read_filtered_lines_from_file(
                    &file,
                    &line_nos,
                    matcher.as_ref(),
                    highlight_filter.as_ref(),
                    columns.as_ref(),
                )
            }
            Doc::Merge(m) => {
                // 필터 결과는 병합 위치 — 원본 줄로 해석해 읽는다
                let highlight_filter = compile_filter_set(spec.as_ref(), None);
                crate::merge::read_merged_positions(
                    &m.index,
                    &line_nos,
                    matcher.as_ref(),
                    highlight_filter.as_ref(),
                )
            }
        }
    })
    .await
    .map_err(|e| format!("필터 줄 읽기 비동기 태스크 실패: {e}"))?
}

/// 파일의 컬럼 정보를 반환한다 (컬럼 뷰용 형식 감지).
/// 파일별로 첫 호출에 감지를 수행하고 OpenFile에 캐시해 이후 호출은 재사용한다.
/// 표본 읽기(최대 1MB)가 발생하므로 spawn_blocking에서 수행한다.
#[tauri::command]
pub async fn detect_columns(
    state: tauri::State<'_, AppState>,
    id: u32,
) -> Result<ColumnsDto, String> {
    let (file, cache) = state.get_columns_handle(id)?;
    let cols = tauri::async_runtime::spawn_blocking(move || {
        crate::state::detect_columns_cached(&file, &cache)
    })
    .await
    .map_err(|e| format!("컬럼 감지 비동기 태스크 실패: {e}"))??;
    Ok(ColumnsDto::from(&cols))
}

/// 열린 파일을 닫고 취소 플래그를 설정하여 자원을 정리한다.
#[tauri::command]
pub async fn close_file(state: tauri::State<'_, AppState>, id: u32) -> Result<(), String> {
    state.close_file(id);
    Ok(())
}

/// [start, end] (양끝 포함) 줄 범위의 원문을 읽어 클립보드에 직접 기록한다.
/// 누적 크기가 MAX_COPY_BYTES를 넘으면 에러를 반환한다.
#[tauri::command]
pub async fn copy_lines(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    id: u32,
    start: u64,
    end: u64,
) -> Result<CopyResult, String> {
    let doc = state.get_doc(id)?;

    // 100MB 범위 읽기와 클립보드 쓰기(무거운 IO)를 블로킹 스레드에서 수행
    let clipboard_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let text = match &doc {
            Doc::File(file) => file
                .read_text_range(start, end, MAX_COPY_BYTES)
                .map_err(|e| e.to_string())?,
            Doc::Merge(m) => {
                // 병합 위치 범위를 원본 줄로 해석해 원본 순서대로 복사한다
                crate::merge::read_merged_text_range(&m.index, start, end, MAX_COPY_BYTES)?
            }
        };
        clipboard_app
            .clipboard()
            .write_text(text.clone())
            .map_err(|e| format!("클립보드 기록 실패: {e}"))?;
        Ok::<CopyResult, String>(CopyResult {
            lines: if text.is_empty() {
                0
            } else {
                text.bytes().filter(|&b| b == b'\n').count() as u64 + 1
            },
            bytes: text.len() as u64,
        })
    })
    .await
    .map_err(|e| format!("범위 읽기 비동기 태스크 실패: {e}"))??;

    Ok(result)
}

/// no 번째 줄의 원문 전체를 반환한다 (16KB 표시용 잘림 없이, 최대 MAX_FULL_LINE_BYTES).
/// 줄이 없으면 None.
#[tauri::command]
pub async fn get_full_line(
    state: tauri::State<'_, AppState>,
    id: u32,
    no: u64,
) -> Result<Option<FullLine>, String> {
    let doc = state.get_doc(id)?;

    tauri::async_runtime::spawn_blocking(move || {
        if let Doc::Merge(m) = &doc {
            return crate::merge::get_merged_full_line(m, no);
        }
        let Doc::File(file) = &doc else {
            unreachable!()
        };
        // byte_len은 read_lines가 제공한다 (16KB 초과 줄도 개행까지 스캔해 정확한 길이를 계산).
        // 16KB 이내 줄은 이미 전체 원문이므로 read_full_line 호출 없이 바로 반환한다.
        let meta = read_lines_from_file(file, no, 1, None, None, None)?;
        let Some(line) = meta.into_iter().next() else {
            return Ok(None);
        };
        if !line.truncated {
            return Ok(Some(FullLine {
                no: line.no,
                text: line.text,
                truncated: false,
                byte_len: line.byte_len,
            }));
        }
        let Some((text, truncated)) = file
            .read_full_line(no, MAX_FULL_LINE_BYTES)
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        Ok(Some(FullLine {
            no,
            text,
            truncated,
            byte_len: line.byte_len,
        }))
    })
    .await
    .map_err(|e| format!("전체 줄 읽기 비동기 태스크 실패: {e}"))?
}

/// 최근 파일 목록을 반환한다 (존재하지 않는 경로는 제외).
/// 경로별 try_exists 확인이 있으므로 블로킹 스레드에서 수행한다.
#[tauri::command]
pub async fn get_recent_files(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(move || crate::recent::load_recent(&app))
        .await
        .map_err(|e| format!("최근 파일 목록 조회 비동기 태스크 실패: {e}"))?
}

/// Finder 파일 연결로 보관된 대기 경로 목록을 한 번만 반환한다 (콜드 스타트 대응).
/// 반환 후에는 비워지므로 같은 경로가 두 번 열리지 않는다.
#[tauri::command]
pub async fn take_pending_open(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    Ok(state.take_pending_open())
}

/// 검색어 문법 유효성을 검사한다 (Matcher 컴파일 성공 여부).
#[tauri::command]
pub async fn validate_query(query: QueryDto) -> Result<(), String> {
    Matcher::new(&query.query, (&query).into())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 필터 조건 유효성을 검사한다 (FilterSet 컴파일 성공 여부).
/// 잘못된 정규식은 "규칙 N: …" 형태의 메시지를 반환한다.
/// spec에 필드 규칙이 있으면 id로 지정된 파일의 감지된 컬럼으로 컬럼명을 검증한다
/// (id가 없으면 컬럼 없음으로 간주해 필드 규칙은 무효 에러가 된다).
#[tauri::command]
pub async fn validate_filter(
    state: tauri::State<'_, AppState>,
    id: Option<u32>,
    spec: FilterSpecDto,
) -> Result<(), String> {
    let core = filter_spec_of(&spec)?;
    let columns = if core.rules.iter().any(|r| r.field.is_some())
        && let Some(fid) = id
    {
        let (file, cache) = state.get_columns_handle(fid)?;
        Some(
            tauri::async_runtime::spawn_blocking(move || {
                crate::state::detect_columns_cached(&file, &cache)
            })
            .await
            .map_err(|e| format!("컬럼 감지 비동기 태스크 실패: {e}"))??,
        )
    } else {
        None
    };
    FilterSet::new(&core, columns.as_ref())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 백그라운드 필터 스캔을 시작하고 필터 ID를 반환한다.
/// 같은 파일의 이전 필터는 자동 취소·제거된다.
#[tauri::command]
pub async fn start_filter(
    state: tauri::State<'_, AppState>,
    id: u32,
    spec: FilterSpecDto,
    on_event: Channel<SearchEvent>,
) -> Result<u32, String> {
    let doc = state.get_doc(id)?;
    let core_spec = filter_spec_of(&spec)?;
    // 필드 규칙이 있으면 파일의 감지된 컬럼으로 컴파일한다 (병합 문서는 컬럼 미지원)
    let columns = if core_spec.rules.iter().any(|r| r.field.is_some()) {
        let (cf, cache) = state.get_columns_handle(id)?;
        Some(
            tauri::async_runtime::spawn_blocking(move || {
                crate::state::detect_columns_cached(&cf, &cache)
            })
            .await
            .map_err(|e| format!("컬럼 감지 비동기 태스크 실패: {e}"))??,
        )
    } else {
        None
    };
    let set = Arc::new(FilterSet::new(&core_spec, columns.as_ref()).map_err(|e| e.to_string())?);
    if set.is_empty() {
        return Err("적용할 필터 조건이 없습니다.".to_string());
    }

    // 파일이 이미 닫혔으면 Err — 스캔 스레드를 띄우지 않는다
    let filter_state = state.insert_filter(id, spec, set)?;

    let filter_id = filter_state.id;
    match doc {
        Doc::File(file) => {
            std::thread::spawn(move || {
                run_filter_scan(file, filter_state, on_event);
            });
        }
        Doc::Merge(m) => {
            // 각 원본에 filter_scan을 돌리고 병합 위치 LineSet을 채운다
            std::thread::spawn(move || {
                crate::merge::run_merge_filter_scan(m, filter_state, on_event);
            });
        }
    }

    Ok(filter_id)
}

/// 진행 중인 필터 스캔을 취소한다.
#[tauri::command]
pub async fn cancel_filter(
    state: tauri::State<'_, AppState>,
    filter_id: u32,
) -> Result<(), String> {
    state.cancel_filter(filter_id);
    Ok(())
}

/// 필터 상태(조건, 현재까지의 보이는 줄 수, 완료/상한 여부)를 조회한다.
#[tauri::command]
pub async fn get_filter(
    state: tauri::State<'_, AppState>,
    filter_id: u32,
) -> Result<FilterInfoDto, String> {
    let filter = state.get_filter(filter_id)?;
    let total = {
        let lines = filter.lines.read().map_err(|e| e.to_string())?;
        lines.len()
    };
    Ok(FilterInfoDto {
        id: filter.id,
        file_id: filter.file_id,
        spec: filter.spec.clone(),
        total,
        done: filter.done.load(Ordering::Relaxed),
        capped: filter.capped.load(Ordering::Relaxed),
    })
}

/// line 이상인 첫 보이는 줄의 필터 내 index(rank)를 반환한다.
/// 스크롤 위치 유지용: 보던 줄이 숨겨지면 그 다음 보이는 줄의 위치를 구한다.
#[tauri::command]
pub async fn filter_index_of(
    state: tauri::State<'_, AppState>,
    filter_id: u32,
    line: u64,
) -> Result<u64, String> {
    let filter = state.get_filter(filter_id)?;
    let lines = filter.lines.read().map_err(|e| e.to_string())?;
    Ok(lines.rank(line))
}

/// 스크롤바 미니맵용 필터 밀도를 조회한다.
#[tauri::command]
pub async fn filter_density(
    state: tauri::State<'_, AppState>,
    filter_id: u32,
    bins: u32,
    total_lines: u64,
) -> Result<Vec<u32>, String> {
    let filter = state.get_filter(filter_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let lines = filter.lines.read().map_err(|e| e.to_string())?;
        Ok(compute_filter_density(&lines, bins, total_lines))
    })
    .await
    .map_err(|e| format!("필터 밀도 계산 비동기 태스크 실패: {e}"))?
}

/// 백그라운드 필터 스캔 스레드 본체.
/// 매치된 줄 번호를 LineSet에 누적하고 진행률/완료/취소/오류를 Channel로 보낸다.
/// 스캔 완료 후에는 인덱스를 한 번 갱신해 tail로 추가된 꼬리도 반영한다.
fn run_filter_scan(
    file: Arc<LogFile>,
    filter_state: Arc<crate::state::FilterState>,
    on_event: Channel<SearchEvent>,
) {
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut last_progress_send = Instant::now();
        let mut sent_first_match = false;

        let outcome = riffle_core::filter_scan(
            &file,
            filter_state.set.as_ref(),
            &filter_state.cancel,
            &mut |batch| {
                let mut lines = filter_state
                    .lines
                    .write()
                    .unwrap_or_else(|p| p.into_inner());
                for &n in batch {
                    lines.push_line(n);
                }
                if lines.range_count() > riffle_core::MAX_FILTER_RANGES {
                    filter_state.capped.store(true, Ordering::Relaxed);
                    filter_state.cancel.store(true, Ordering::Relaxed);
                }
            },
            &mut |p| {
                let now = Instant::now();
                let is_first_match = !sent_first_match && p.match_count > 0;
                let should_send = is_first_match
                    || now.duration_since(last_progress_send) >= Duration::from_millis(50);

                if should_send {
                    if is_first_match {
                        sent_first_match = true;
                    }
                    last_progress_send = now;
                    let _ = on_event.send(SearchEvent::Progress {
                        scanned_bytes: p.scanned_bytes,
                        total_bytes: p.total_bytes,
                        match_count: p.match_count,
                    });
                }
            },
        );

        // 스캔 중 tail로 추가된 꼬리 반영: 인덱스를 갱신하고 마지막 줄을 재판정한다.
        // 스캔이 취소/상한이어도 이미 모은 줄은 유지한다.
        if let Ok(riffle_core::RefreshOutcome::Appended {
            old_line_count,
            new_line_count,
            ..
        }) = file.refresh()
        {
            filter_state.apply_appended(&file, old_line_count, new_line_count);
        }

        let total = {
            let lines = filter_state.lines.read().unwrap_or_else(|p| p.into_inner());
            lines.len()
        };
        let capped = filter_state.capped.load(Ordering::Relaxed);

        match outcome {
            Ok(riffle_core::SearchOutcome::Completed { .. }) => {
                filter_state.done.store(true, Ordering::Relaxed);
                let _ = on_event.send(SearchEvent::Done { total, capped });
            }
            Ok(riffle_core::SearchOutcome::Capped { .. }) => {
                filter_state.done.store(true, Ordering::Relaxed);
                let _ = on_event.send(SearchEvent::Done {
                    total,
                    capped: true,
                });
            }
            Ok(riffle_core::SearchOutcome::Cancelled) => {
                if capped {
                    filter_state.done.store(true, Ordering::Relaxed);
                    let _ = on_event.send(SearchEvent::Done { total, capped });
                } else {
                    let _ = on_event.send(SearchEvent::Cancelled);
                }
            }
            Err(err) => {
                let _ = on_event.send(SearchEvent::Error {
                    message: err.to_string(),
                });
            }
        }
    }));

    if let Err(panic_payload) = panic_result {
        let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "필터 스캔 스레드 실행 중 알 수 없는 패닉이 발생했습니다.".to_string()
        };
        let _ = on_event.send(SearchEvent::Error { message: msg });
    }
}

/// 백그라운드 검색을 시작하고 검색 ID를 반환한다.
/// filter_id가 주어지면 해당 필터의 조건과 교집합(검색 매치 ∧ 필터 통과)으로 검색한다.
#[tauri::command]
pub async fn search(
    state: tauri::State<'_, AppState>,
    id: u32,
    query: QueryDto,
    filter_id: Option<u32>,
    on_event: Channel<SearchEvent>,
) -> Result<u32, String> {
    let doc = state.get_doc(id)?;
    let matcher = Matcher::new(&query.query, (&query).into()).map_err(|e| e.to_string())?;

    // filter_id가 있으면 그 필터의 spec으로 FilterSet을 만든다
    let filter_set = match filter_id {
        Some(fid) => {
            let f = state.get_filter(fid)?;
            if f.file_id != id {
                return Err(format!(
                    "필터 ID {}는 파일 ID {}에 속하지 않습니다.",
                    fid, id
                ));
            }
            Some(f.set.clone())
        }
        None => None,
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let results = Arc::new(std::sync::RwLock::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    let capped = Arc::new(AtomicBool::new(false));

    // 파일이 이미 닫혔으면 Err — 검색 스레드를 띄우지 않는다
    let search_state = state.insert_search(
        id,
        cancel.clone(),
        results.clone(),
        done.clone(),
        capped.clone(),
    )?;
    let search_id = search_state.id;

    // 병합 문서: 각 원본에 검색을 위임하고 결과를 병합 위치로 변환한다
    if let Doc::Merge(m) = doc {
        std::thread::spawn(move || {
            crate::merge::run_merge_search(m, matcher, filter_set, search_state, on_event);
        });
        return Ok(search_id);
    }
    let Doc::File(file) = doc else { unreachable!() };

    let thread_cancel = cancel;
    let thread_results = results;
    let thread_done = done;
    let thread_capped = capped;
    let thread_event = on_event;

    std::thread::spawn(move || {
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut last_progress_send = Instant::now();
            let mut sent_first_match = false;

            // filter가 없으면(None) 기존 search와 동일한 경로로 실행된다
            let outcome = riffle_core::search_filtered(
                &file,
                &matcher,
                filter_set.as_deref(),
                &thread_cancel,
                &mut |batch| {
                    if let Ok(mut lock) = thread_results.write() {
                        lock.extend_from_slice(batch);
                    }
                },
                &mut |p| {
                    let now = Instant::now();
                    let is_first_match = !sent_first_match && p.match_count > 0;
                    let should_send = is_first_match
                        || now.duration_since(last_progress_send) >= Duration::from_millis(50);

                    if should_send {
                        if is_first_match {
                            sent_first_match = true;
                        }
                        last_progress_send = now;
                        let _ = thread_event.send(SearchEvent::Progress {
                            scanned_bytes: p.scanned_bytes,
                            total_bytes: p.total_bytes,
                            match_count: p.match_count,
                        });
                    }
                },
            );

            match outcome {
                Ok(riffle_core::SearchOutcome::Completed { total }) => {
                    thread_done.store(true, Ordering::Relaxed);
                    let _ = thread_event.send(SearchEvent::Done {
                        total,
                        capped: false,
                    });
                }
                Ok(riffle_core::SearchOutcome::Capped { total }) => {
                    thread_done.store(true, Ordering::Relaxed);
                    thread_capped.store(true, Ordering::Relaxed);
                    let _ = thread_event.send(SearchEvent::Done {
                        total,
                        capped: true,
                    });
                }
                Ok(riffle_core::SearchOutcome::Cancelled) => {
                    let _ = thread_event.send(SearchEvent::Cancelled);
                }
                Err(err) => {
                    let _ = thread_event.send(SearchEvent::Error {
                        message: err.to_string(),
                    });
                }
            }
        }));

        if let Err(panic_payload) = panic_result {
            let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "검색 스레드 실행 중 알 수 없는 패닉이 발생했습니다.".to_string()
            };
            let _ = thread_event.send(SearchEvent::Error { message: msg });
        }
    });

    Ok(search_id)
}

/// 진행 중인 검색을 취소한다.
#[tauri::command]
pub async fn cancel_search(
    state: tauri::State<'_, AppState>,
    search_id: u32,
) -> Result<(), String> {
    state.cancel_search(search_id);
    Ok(())
}

/// 현재 줄 번호 기준으로 다음 또는 이전 검색 결과를 탐색한다.
#[tauri::command]
pub async fn search_nav(
    state: tauri::State<'_, AppState>,
    search_id: u32,
    from_line: u64,
    forward: bool,
) -> Result<Option<SearchHit>, String> {
    let search = state.get_search(search_id)?;
    let results = search.results.read().map_err(|e| e.to_string())?;
    Ok(compute_search_nav(&results, from_line, forward))
}

/// 결과 목록 내 특정 인덱스의 검색 결과를 조회한다.
#[tauri::command]
pub async fn search_hit_at(
    state: tauri::State<'_, AppState>,
    search_id: u32,
    index: u64,
) -> Result<Option<SearchHit>, String> {
    let search = state.get_search(search_id)?;
    let results = search.results.read().map_err(|e| e.to_string())?;
    Ok(compute_search_hit_at(&results, index))
}

/// 스크롤바 미니맵용 검색 밀도를 조회한다.
#[tauri::command]
pub async fn search_density(
    state: tauri::State<'_, AppState>,
    search_id: u32,
    bins: u32,
    total_lines: u64,
) -> Result<Vec<u32>, String> {
    let search = state.get_search(search_id)?;
    tauri::async_runtime::spawn_blocking(move || {
        let results = search.results.read().map_err(|e| e.to_string())?;
        Ok(compute_search_density(&results, bins, total_lines))
    })
    .await
    .map_err(|e| format!("검색 밀도 계산 비동기 태스크 실패: {e}"))?
}

/// 프리셋 저장 디렉터리 (app_data_dir)를 반환한다.
fn presets_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("앱 데이터 디렉터리 조회 실패: {e}"))
}

/// 저장된 필터 프리셋 목록을 이름순으로 반환한다.
#[tauri::command]
pub async fn list_filter_presets(app: tauri::AppHandle) -> Result<Vec<FilterPresetDto>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = presets_dir(&app)?;
        crate::presets::with_presets_lock(|| crate::presets::list_presets(&dir))
    })
    .await
    .map_err(|e| format!("프리셋 목록 조회 비동기 태스크 실패: {e}"))?
}

/// 필터 프리셋을 저장한다. 같은 이름이 있으면 덮어쓴다.
/// spec이 컴파일되지 않으면(나쁜 정규식 등) 저장하지 않고 Err를 반환한다.
#[tauri::command]
pub async fn save_filter_preset(
    app: tauri::AppHandle,
    name: String,
    spec: FilterSpecDto,
) -> Result<(), String> {
    // 저장 전에 spec 유효성을 검사한다
    let core = filter_spec_of(&spec)?;
    FilterSet::new(&core, None).map_err(|e| e.to_string())?;

    tauri::async_runtime::spawn_blocking(move || {
        let dir = presets_dir(&app)?;
        crate::presets::with_presets_lock(|| crate::presets::save_preset(&dir, &name, spec))
    })
    .await
    .map_err(|e| format!("프리셋 저장 비동기 태스크 실패: {e}"))?
}

/// 필터 프리셋을 삭제한다. 없으면 Err.
#[tauri::command]
pub async fn delete_filter_preset(app: tauri::AppHandle, name: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let dir = presets_dir(&app)?;
        crate::presets::with_presets_lock(|| crate::presets::delete_preset(&dir, &name))
    })
    .await
    .map_err(|e| format!("프리셋 삭제 비동기 태스크 실패: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempFile {
        path: std::path::PathBuf,
    }

    impl TempFile {
        fn create(content: &[u8]) -> Self {
            let path = crate::testutil::temp_log_path("test");
            let mut file = std::fs::File::create(&path).expect("임시 파일 생성 실패");
            file.write_all(content).expect("임시 파일 쓰기 실패");
            file.sync_all().expect("임시 파일 플러시 실패");
            Self { path }
        }

        fn path_str(&self) -> &str {
            self.path.to_str().unwrap()
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn test_clamp_get_lines_count() {
        assert_eq!(clamp_get_lines_count(0), 0);
        assert_eq!(clamp_get_lines_count(1), 1);
        assert_eq!(clamp_get_lines_count(500), 500);
        assert_eq!(clamp_get_lines_count(1000), 1000);
        assert_eq!(clamp_get_lines_count(1001), 1000);
        assert_eq!(clamp_get_lines_count(50_000), 1000);
        assert_eq!(clamp_get_lines_count(u32::MAX), 1000);
    }

    #[test]
    fn test_open_get_lines_close_flow() {
        let content = b"line 0\nline 1\nline 2\nline 3\nline 4\n";
        let temp_file = TempFile::create(content);
        let state = AppState::default();

        let opened = open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        assert!(opened.gz_writer.is_none());
        assert!(!opened.info.compressed);
        assert_eq!(opened.info.id, 1);
        assert_eq!(opened.info.path, temp_file.path_str());
        assert_eq!(opened.info.size, content.len() as u64);
        assert_eq!(state.file_count(), 1);

        let file = &opened.file;
        let cancel = &opened.cancel;
        file.build_index(cancel, &mut |_| {}).expect("인덱싱 실패");

        let lines = read_lines_from_file(file, 0, 3, None, None, None).expect("줄 읽기 실패");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "line 0");
        assert_eq!(lines[1].text, "line 1");
        assert_eq!(lines[2].text, "line 2");

        state.close_file(opened.info.id);
        assert_eq!(state.file_count(), 0);
    }

    #[test]
    fn test_nonexistent_file_id() {
        let state = AppState::default();
        let result = state.get_file(9999);
        assert!(result.is_err());
        let err_msg = match result {
            Err(e) => e,
            Ok(_) => panic!("에러가 발생해야 함"),
        };
        assert!(err_msg.contains("9999"));
    }

    #[test]
    fn test_close_nonexistent_id() {
        let state = AppState::default();
        state.close_file(9999);
        assert_eq!(state.file_count(), 0);
    }

    #[test]
    fn test_open_nonexistent_file() {
        let state = AppState::default();
        let result = open_file_internal(&state, "/nonexistent/path/to/file.log");
        assert!(result.is_err());
        let err_msg = match result {
            Err(e) => e,
            Ok(_) => panic!("에러가 발생해야 함"),
        };
        assert!(err_msg.contains("파일을 열 수 없습니다"));
    }

    #[test]
    fn test_query_dto_to_search_options() {
        let dto = QueryDto {
            query: "test".to_string(),
            regex: true,
            case_sensitive: false,
        };
        let opts: SearchOptions = (&dto).into();
        assert!(opts.regex);
        assert!(!opts.case_sensitive);

        let dto2 = QueryDto {
            query: "abc".to_string(),
            regex: false,
            case_sensitive: true,
        };
        let opts2: SearchOptions = (&dto2).into();
        assert!(!opts2.regex);
        assert!(opts2.case_sensitive);
    }

    #[test]
    fn test_compute_search_nav_comprehensive() {
        let results = [0, 5, 10, 15, 20];

        // 1. Forward 탐색: line > from_line 인 첫 결과
        assert_eq!(
            compute_search_nav(&results, 0, true),
            Some(SearchHit { index: 1, line: 5 })
        );
        assert_eq!(
            compute_search_nav(&results, 4, true),
            Some(SearchHit { index: 1, line: 5 })
        );
        assert_eq!(
            compute_search_nav(&results, 5, true),
            Some(SearchHit { index: 2, line: 10 })
        );
        assert_eq!(
            compute_search_nav(&results, 19, true),
            Some(SearchHit { index: 4, line: 20 })
        );
        // 경계 / None: 마지막 결과 이상인 경우
        assert_eq!(compute_search_nav(&results, 20, true), None);
        assert_eq!(compute_search_nav(&results, 100, true), None);

        // 2. Backward 탐색: line < from_line 인 마지막 결과
        assert_eq!(
            compute_search_nav(&results, 100, false),
            Some(SearchHit { index: 4, line: 20 })
        );
        assert_eq!(
            compute_search_nav(&results, 20, false),
            Some(SearchHit { index: 3, line: 15 })
        );
        assert_eq!(
            compute_search_nav(&results, 16, false),
            Some(SearchHit { index: 3, line: 15 })
        );
        assert_eq!(
            compute_search_nav(&results, 5, false),
            Some(SearchHit { index: 0, line: 0 })
        );
        // 경계 / None: 첫 결과 이하인 경우
        assert_eq!(compute_search_nav(&results, 0, false), None);

        // 3. 빈 목록 탐색
        let empty: [u64; 0] = [];
        assert_eq!(compute_search_nav(&empty, 0, true), None);
        assert_eq!(compute_search_nav(&empty, 0, false), None);
    }

    #[test]
    fn test_compute_search_hit_at() {
        let results = [100, 200, 300];
        assert_eq!(
            compute_search_hit_at(&results, 0),
            Some(SearchHit {
                index: 0,
                line: 100
            })
        );
        assert_eq!(
            compute_search_hit_at(&results, 1),
            Some(SearchHit {
                index: 1,
                line: 200
            })
        );
        assert_eq!(
            compute_search_hit_at(&results, 2),
            Some(SearchHit {
                index: 2,
                line: 300
            })
        );
        assert_eq!(compute_search_hit_at(&results, 3), None);
    }

    #[test]
    fn test_compute_search_density_sum_and_boundaries() {
        let results = [0, 1, 10, 25, 49, 50, 75, 99, 100];
        let total_lines = 101;
        let bins = 10;

        let counts = compute_search_density(&results, bins, total_lines);
        assert_eq!(counts.len(), 10);

        // 핵심 성질: 모든 구간의 매치 합은 전체 결과 개수와 정확히 일치해야 함
        let sum: u32 = counts.iter().sum();
        assert_eq!(sum, results.len() as u32);

        // 경계 확인: 줄 0, 1은 bin 0에 포함되어야 함
        assert!(counts[0] >= 2);
        // 마지막 줄 100은 마지막 bin 9에 포함되어야 함
        assert!(counts[9] >= 1);

        // bins 상한(2000) 클램핑 확인
        let large_bins_counts = compute_search_density(&results, 5000, total_lines);
        assert_eq!(large_bins_counts.len(), 2000);
        let large_sum: u32 = large_bins_counts.iter().sum();
        assert_eq!(large_sum, results.len() as u32);

        // line >= total_lines 인 줄 번호가 있어도 마지막 bin으로 안전하게 클램핑되어야 함
        let overflow_results = [100, 105, 200];
        let overflow_counts = compute_search_density(&overflow_results, 5, 100);
        assert_eq!(overflow_counts.len(), 5);
        let overflow_sum: u32 = overflow_counts.iter().sum();
        assert_eq!(overflow_sum, overflow_results.len() as u32);
        assert_eq!(overflow_counts[4], 3); // 모두 마지막 bin에 누적

        // edge cases: bins = 0, total_lines = 0, results empty
        assert_eq!(
            compute_search_density(&results, 0, total_lines),
            Vec::<u32>::new()
        );
        assert_eq!(compute_search_density(&[], 5, total_lines), vec![0; 5]);
        assert_eq!(compute_search_density(&results, 5, 0), vec![0; 5]);
    }

    #[test]
    fn test_get_lines_with_invalid_query_returns_lines_without_highlights() {
        let content = b"line 0: standard log message\nline 1: ERROR status=(unclosed group\nline 2: normal end\n";
        let temp_file = TempFile::create(content);
        let state = AppState::default();

        let opened = open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        let file = &opened.file;
        let cancel = &opened.cancel;
        file.build_index(cancel, &mut |_| {}).expect("인덱싱 실패");

        // 잘못된 정규식 쿼리: 괄호가 닫히지 않음
        let invalid_query = QueryDto {
            query: "status=(".to_string(),
            regex: true,
            case_sensitive: true,
        };

        // Matcher 컴파일 실패 시 에러 대신 하이라이트 없이 정상 줄 반환해야 함
        let matcher = Matcher::new(&invalid_query.query, (&invalid_query).into()).ok();
        assert!(matcher.is_none());

        let lines = read_lines_from_file(file, 0, 3, matcher.as_ref(), None, None)
            .expect("무효 쿼리 시에도 줄 읽기는 성공해야 함");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "line 0: standard log message");
        assert_eq!(lines[1].text, "line 1: ERROR status=(unclosed group");
        assert_eq!(lines[2].text, "line 2: normal end");

        // 모든 줄의 highlights가 비어 있어야 함
        for line in &lines {
            assert!(line.highlights.is_empty());
        }

        // 정상 쿼리 적용 시 highlights가 채워지는지 확인
        let valid_query = QueryDto {
            query: "ERROR".to_string(),
            regex: false,
            case_sensitive: true,
        };
        let valid_matcher = Matcher::new(&valid_query.query, (&valid_query).into()).ok();
        assert!(valid_matcher.is_some());

        let lines_with_hl = read_lines_from_file(file, 0, 3, valid_matcher.as_ref(), None, None)
            .expect("정상 쿼리 줄 읽기 성공");
        assert_eq!(lines_with_hl.len(), 3);
        assert!(lines_with_hl[0].highlights.is_empty());
        assert_eq!(lines_with_hl[1].highlights.len(), 1); // "ERROR" 매치 1개
        assert!(lines_with_hl[2].highlights.is_empty());
    }

    #[test]
    fn test_close_file_cancels_gzip_decompression() {
        // 해제가 충분히 오래 걸리는 크기의 gzip을 만든다
        let mut content = Vec::new();
        for i in 0..1_000_000 {
            content.extend_from_slice(
                format!("close decompress line {:06} with some payload text\n", i).as_bytes(),
            );
        }
        let gz_path = crate::testutil::create_gzip_file("close_gz", &content);
        let state = AppState::default();

        let opened =
            open_file_internal(&state, gz_path.to_str().unwrap()).expect("gzip 파일 열기 실패");
        assert!(opened.info.compressed);
        let writer = opened.gz_writer.expect("GzWriter가 반환되어야 함");
        let file = opened.file.clone();
        let cancel = opened.cancel.clone();

        // 해제 스레드를 시작한 직후 파일을 닫는다 → cancel이 전파되어 스레드가 종료되어야 함
        let lf = file.clone();
        let cancel2 = cancel.clone();
        let handle = std::thread::spawn(move || writer.run(&lf, &cancel2, &mut |_| {}));

        state.close_file(opened.info.id);

        let start = Instant::now();
        let result = handle.join().expect("해제 스레드 join 실패");
        assert!(start.elapsed() < Duration::from_secs(30));
        // 취소에 의한 중단은 Ok(())이며 해제된 부분만 남는다
        assert!(result.is_ok());
        assert!(file.source().len() < content.len() as u64);

        let _ = std::fs::remove_file(&gz_path);
    }

    #[test]
    fn test_get_filtered_lines_with_invalid_query() {
        let content = b"line 0: info\nline 1: err\nline 2: err again\n";
        let temp_file = TempFile::create(content);
        let state = AppState::default();

        let opened = open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        let file = &opened.file;
        let cancel = &opened.cancel;
        file.build_index(cancel, &mut |_| {}).expect("인덱싱 실패");

        // 무효 정규식 쿼리
        let invalid_query = QueryDto {
            query: "(".to_string(),
            regex: true,
            case_sensitive: false,
        };
        let matcher = Matcher::new(&invalid_query.query, (&invalid_query).into()).ok();
        assert!(matcher.is_none());

        let line_nos = [1, 2];
        let lines = read_filtered_lines_from_file(file, &line_nos, matcher.as_ref(), None, None)
            .expect("필터 줄 읽기 성공");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].no, 1);
        assert_eq!(lines[0].text, "line 1: err");
        assert!(lines[0].highlights.is_empty());
        assert_eq!(lines[1].no, 2);
        assert_eq!(lines[1].text, "line 2: err again");
        assert!(lines[1].highlights.is_empty());
    }

    fn rule(pattern: &str, kind: &str, enabled: bool) -> FilterRuleDto {
        FilterRuleDto {
            pattern: pattern.to_string(),
            regex: false,
            case_sensitive: true,
            kind: kind.to_string(),
            enabled,
            color: "#123456".to_string(),
            field: None,
        }
    }

    fn spec_dto(rules: Vec<FilterRuleDto>, combine: &str, levels: Vec<&str>) -> FilterSpecDto {
        FilterSpecDto {
            rules,
            combine: combine.to_string(),
            levels: levels.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn test_filter_spec_of_conversion() {
        let dto = spec_dto(
            vec![
                rule("ERROR", "include", true),
                rule("heartbeat", "exclude", false),
            ],
            "and",
            vec!["error", "warn"],
        );
        let spec = filter_spec_of(&dto).expect("변환 성공");
        assert_eq!(spec.rules.len(), 2);
        assert_eq!(spec.rules[0].kind, RuleKind::Include);
        assert!(!spec.rules[1].enabled);
        assert_eq!(spec.combine, Combine::And);
        assert_eq!(spec.levels, vec![Level::Error, Level::Warn]);
    }

    #[test]
    fn test_filter_spec_of_rejects_unknown_values() {
        // 알 수 없는 kind
        let bad_kind = spec_dto(vec![rule("x", "maybe", true)], "or", vec![]);
        assert!(filter_spec_of(&bad_kind).is_err());
        // 알 수 없는 combine
        let bad_combine = spec_dto(vec![], "xor", vec![]);
        assert!(filter_spec_of(&bad_combine).is_err());
        // 알 수 없는 level
        let bad_level = spec_dto(vec![], "or", vec!["fatal"]);
        assert!(filter_spec_of(&bad_level).is_err());
    }

    #[test]
    fn test_compile_filter_set_invalid_returns_none() {
        // 나쁜 정규식 → None (하이라이트 없이 진행)
        let mut bad = rule("(", "include", true);
        bad.regex = true;
        let set = compile_filter_set(Some(&spec_dto(vec![bad], "or", vec![])), None);
        assert!(set.is_none());
        // spec 자체가 None → None
        assert!(compile_filter_set(None, None).is_none());
        // 유효 spec → Some
        let ok = spec_dto(vec![rule("ERROR", "include", true)], "or", vec![]);
        assert!(compile_filter_set(Some(&ok), None).is_some());
    }

    #[test]
    fn test_compute_filter_density_matches_search_density() {
        // 같은 줄 집합에 대해 두 밀도 계산이 일치해야 한다
        let results = [0u64, 5, 10, 25, 49, 50, 75, 99, 100];
        let total_lines = 101;
        let bins = 10;

        let mut set = LineSet::new();
        for &l in &results {
            set.push_line(l);
        }

        let filter_counts = compute_filter_density(&set, bins, total_lines);
        let search_counts = compute_search_density(&results, bins, total_lines);
        assert_eq!(filter_counts, search_counts);
        assert_eq!(filter_counts.iter().sum::<u32>(), results.len() as u32);

        // 빈 집합 / total_lines=0 / bins=0 경계
        assert_eq!(compute_filter_density(&LineSet::new(), 5, 100), vec![0; 5]);
        assert_eq!(compute_filter_density(&set, 5, 0), vec![0; 5]);
        assert_eq!(compute_filter_density(&set, 0, 100), Vec::<u32>::new());
        // bins 상한 클램핑
        assert_eq!(compute_filter_density(&set, 5000, total_lines).len(), 2000);
    }

    #[test]
    fn test_read_lines_with_filter_rule_highlights() {
        let content = "2024-01-01 INFO normal\n2024-01-01 ERROR fail 결제\nplain line\n".as_bytes();
        let temp_file = TempFile::create(content);
        let state = AppState::default();

        let opened = open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        let file = &opened.file;
        let cancel = &opened.cancel;
        file.build_index(cancel, &mut |_| {}).expect("인덱싱 실패");

        // include ERROR + include 결제 (OR) → 두 규칙 모두 규칙 0,1의 인덱스를 가진다
        let spec = spec_dto(
            vec![
                rule("ERROR", "include", true),
                rule("결제", "include", true),
            ],
            "or",
            vec![],
        );
        let set = compile_filter_set(Some(&spec), None).expect("spec 컴파일 실패");

        let lines = read_lines_from_file(file, 0, 3, None, Some(&set), None).expect("줄 읽기 실패");
        assert_eq!(lines.len(), 3);
        // 줄 0: 매치 없음
        assert!(lines[0].rule_highlights.is_empty());
        // 줄 1: ERROR(규칙0)와 결제(규칙1) 두 규칙 매치
        assert_eq!(lines[1].rule_highlights.len(), 2);
        let rule_idxs: Vec<u32> = lines[1]
            .rule_highlights
            .iter()
            .map(|(_, _, idx)| *idx)
            .collect();
        assert!(rule_idxs.contains(&0));
        assert!(rule_idxs.contains(&1));
        // UTF-16 범위가 텍스트 안에 있어야 한다
        for (s, e, _) in &lines[1].rule_highlights {
            let s = *s as usize;
            let e = *e as usize;
            assert!(s < e && e <= lines[1].text.encode_utf16().count());
        }
        // 줄 2: 매치 없음
        assert!(lines[2].rule_highlights.is_empty());

        // filter=None이면 rule_highlights가 비어 있어야 한다
        let plain = read_lines_from_file(file, 0, 3, None, None, None).expect("줄 읽기 실패");
        assert!(plain.iter().all(|l| l.rule_highlights.is_empty()));

        // 제외 규칙만 있으면 rule_highlights는 비어 있어야 한다 (포함 규칙만 대상)
        let exc_spec = spec_dto(vec![rule("ERROR", "exclude", true)], "or", vec![]);
        let exc_set = compile_filter_set(Some(&exc_spec), None).expect("spec 컴파일 실패");
        let exc_lines =
            read_lines_from_file(file, 0, 3, None, Some(&exc_set), None).expect("줄 읽기 실패");
        assert!(exc_lines.iter().all(|l| l.rule_highlights.is_empty()));
    }

    #[test]
    fn test_search_filtered_intersection_via_state() {
        // IPC 경로와 동일하게: 필터의 set으로 search_filtered를 호출해 교집합을 확인한다
        let content = "INFO 결제 완료\nERROR 결제 실패\nERROR 네트워크\nINFO 기타\n".as_bytes();
        let temp_file = TempFile::create(content);
        let state = AppState::default();

        let opened = open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        let file = opened.file.clone();
        let cancel = opened.cancel.clone();
        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");

        // 필터: include ERROR → 보이는 줄 {1, 2}
        let spec = spec_dto(vec![rule("ERROR", "include", true)], "or", vec![]);
        let core = filter_spec_of(&spec).unwrap();
        let set = FilterSet::new(&core, None).unwrap();
        let filter_state = state
            .insert_filter(opened.info.id, spec, Arc::new(set))
            .expect("필터 등록 실패");

        // 필터 스캔 결과 수동 채우기 대신 filter_scan으로 채운다
        let mut collected = Vec::new();
        riffle_core::filter_scan(
            &file,
            filter_state.set.as_ref(),
            &AtomicBool::new(false),
            &mut |batch| collected.extend_from_slice(batch),
            &mut |_| {},
        )
        .expect("filter_scan 실패");
        {
            let mut lines = filter_state.lines.write().unwrap();
            for &n in &collected {
                lines.push_line(n);
            }
        }
        assert_eq!(collected, vec![1, 2]);

        // 검색 "결제" ∧ 필터 ERROR → 줄 1만 ("ERROR 결제 실패")
        let matcher = Matcher::new(
            "결제",
            SearchOptions {
                regex: false,
                case_sensitive: true,
            },
        )
        .unwrap();
        let mut hits = Vec::new();
        riffle_core::search_filtered(
            &file,
            &matcher,
            Some(filter_state.set.as_ref()),
            &AtomicBool::new(false),
            &mut |batch| hits.extend_from_slice(batch),
            &mut |_| {},
        )
        .expect("search_filtered 실패");
        assert_eq!(hits, vec![1]);

        state.close_file(opened.info.id);
    }
}
