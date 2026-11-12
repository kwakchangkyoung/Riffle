use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use riffle_core::{LogFile, Matcher, SearchOptions};
use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri::ipc::Channel;
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::state::AppState;

/// 파일 정보 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileInfo {
    pub id: u32,
    pub path: String,
    pub size: u64,
}

/// 인덱싱 진행률 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgressDto {
    pub indexed_bytes: u64,
    pub total_bytes: u64,
    pub line_count: u64,
    pub done: bool,
}

impl From<riffle_core::IndexProgress> for IndexProgressDto {
    fn from(progress: riffle_core::IndexProgress) -> Self {
        Self {
            indexed_bytes: progress.indexed_bytes,
            total_bytes: progress.total_bytes,
            line_count: progress.line_count,
            done: progress.done,
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
}

impl From<riffle_core::Line> for LineDto {
    fn from(line: riffle_core::Line) -> Self {
        Self {
            no: line.no,
            text: line.text,
            byte_len: line.byte_len,
            truncated: line.truncated,
            highlights: line.highlights,
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

/// 파일 열기 내부 로직: LogFile을 열고 AppState에 등록한다.
pub fn open_file_internal(
    state: &AppState,
    path: &str,
) -> Result<(FileInfo, Arc<LogFile>, Arc<AtomicBool>), String> {
    let log_file = LogFile::open(Path::new(path))
        .map_err(|e| format!("파일을 열 수 없습니다 ('{path}'): {e}"))?;
    let file = Arc::new(log_file);
    let cancel = Arc::new(AtomicBool::new(false));
    let size = file.source().len();
    let id = state.insert_file(file.clone(), cancel.clone(), PathBuf::from(path));

    Ok((
        FileInfo {
            id,
            path: path.to_string(),
            size,
        },
        file,
        cancel,
    ))
}

/// LogFile 인스턴스에서 지정한 범위의 줄들을 읽어 LineDto 벡터로 변환한다.
pub fn read_lines_from_file(
    file: &LogFile,
    start: u64,
    count: u32,
    matcher: Option<&Matcher>,
) -> Result<Vec<LineDto>, String> {
    let lines = file
        .read_lines(start, count as u64, matcher)
        .map_err(|e| format!("줄 읽기 실패: {e}"))?;
    Ok(lines.into_iter().map(LineDto::from).collect())
}

/// LogFile 인스턴스에서 임의 줄 번호 목록을 읽어 LineDto 벡터로 변환한다.
pub fn read_filtered_lines_from_file(
    file: &LogFile,
    line_nos: &[u64],
    matcher: Option<&Matcher>,
) -> Result<Vec<LineDto>, String> {
    let lines = file
        .read_lines_at(line_nos, matcher)
        .map_err(|e| format!("필터 줄 읽기 실패: {e}"))?;
    Ok(lines.into_iter().map(LineDto::from).collect())
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
    let open_app = app.clone();
    let open_path = path.clone();
    let (info, file, cancel) = tauri::async_runtime::spawn_blocking(move || {
        let st = open_app.state::<AppState>();
        open_file_internal(st.inner(), &open_path)
    })
    .await
    .map_err(|e| format!("파일 열기 비동기 태스크 실패: {e}"))??;

    std::thread::spawn(move || {
        run_indexing(file, cancel, on_progress);
    });

    // 최근 파일 기록 + 메뉴의 "최근 파일" 서브메뉴 재구성.
    // 기록/메뉴 갱신 실패는 파일 열기 자체를 실패시키지 않는다.
    let recent_app = app.clone();
    let recent_path = path.clone();
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

    Ok(info)
}

/// 지정된 범위의 로그 줄들을 읽어 반환한다.
/// 전달된 query가 무효한 경우(문법 오류 등) 에러 대신 하이라이트 없이 정상 줄을 반환한다.
#[tauri::command]
pub async fn get_lines(
    state: tauri::State<'_, AppState>,
    id: u32,
    start: u64,
    count: u32,
    query: Option<QueryDto>,
) -> Result<Vec<LineDto>, String> {
    let file = state.get_file(id)?;
    let clamped_count = clamp_get_lines_count(count);

    tauri::async_runtime::spawn_blocking(move || {
        let matcher = match query {
            Some(ref q) => Matcher::new(&q.query, q.into()).ok(),
            None => None,
        };
        read_lines_from_file(&file, start, clamped_count, matcher.as_ref())
    })
    .await
    .map_err(|e| format!("줄 읽기 비동기 태스크 실패: {e}"))?
}

/// 필터 모드: 검색 결과 목록의 [start_index, start_index + count) 줄들을 읽어 반환한다.
/// 전달된 query가 무효한 경우 에러 대신 하이라이트 없이 정상 줄을 반환한다.
#[tauri::command]
pub async fn get_filtered_lines(
    state: tauri::State<'_, AppState>,
    id: u32,
    search_id: u32,
    start_index: u64,
    count: u32,
    query: Option<QueryDto>,
) -> Result<Vec<LineDto>, String> {
    let file = state.get_file(id)?;
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

    tauri::async_runtime::spawn_blocking(move || {
        let matcher = match query {
            Some(ref q) => Matcher::new(&q.query, q.into()).ok(),
            None => None,
        };
        read_filtered_lines_from_file(&file, &line_nos, matcher.as_ref())
    })
    .await
    .map_err(|e| format!("필터 줄 읽기 비동기 태스크 실패: {e}"))?
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
    let file = state.get_file(id)?;

    // 100MB 범위 읽기와 클립보드 쓰기(무거운 IO)를 블로킹 스레드에서 수행
    let clipboard_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let text = file
            .read_text_range(start, end, MAX_COPY_BYTES)
            .map_err(|e| e.to_string())?;
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
    let file = state.get_file(id)?;

    tauri::async_runtime::spawn_blocking(move || {
        // byte_len은 read_lines가 제공한다 (16KB 초과 줄도 개행까지 스캔해 정확한 길이를 계산).
        // 16KB 이내 줄은 이미 전체 원문이므로 read_full_line 호출 없이 바로 반환한다.
        let meta = read_lines_from_file(&file, no, 1, None)?;
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

/// Finder 파일 연결로 보관된 대기 경로를 한 번만 반환한다 (콜드 스타트 대응).
/// 반환 후에는 비워지므로 같은 경로가 두 번 열리지 않는다.
#[tauri::command]
pub async fn take_pending_open(
    state: tauri::State<'_, AppState>,
) -> Result<Option<String>, String> {
    Ok(state.take_pending_open())
}

/// 검색어 문법 유효성을 검사한다 (Matcher 컴파일 성공 여부).
#[tauri::command]
pub async fn validate_query(query: QueryDto) -> Result<(), String> {
    Matcher::new(&query.query, (&query).into())
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 백그라운드 검색을 시작하고 검색 ID를 반환한다.
#[tauri::command]
pub async fn search(
    state: tauri::State<'_, AppState>,
    id: u32,
    query: QueryDto,
    on_event: Channel<SearchEvent>,
) -> Result<u32, String> {
    let file = state.get_file(id)?;
    let matcher = Matcher::new(&query.query, (&query).into()).map_err(|e| e.to_string())?;

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

    let thread_cancel = cancel;
    let thread_results = results;
    let thread_done = done;
    let thread_capped = capped;
    let thread_event = on_event;

    std::thread::spawn(move || {
        let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut last_progress_send = Instant::now();
            let mut sent_first_match = false;

            let outcome = riffle_core::search(
                &file,
                &matcher,
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

        let (info, file, cancel) =
            open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        assert_eq!(info.id, 1);
        assert_eq!(info.path, temp_file.path_str());
        assert_eq!(info.size, content.len() as u64);
        assert_eq!(state.file_count(), 1);

        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");

        let lines = read_lines_from_file(&file, 0, 3, None).expect("줄 읽기 실패");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "line 0");
        assert_eq!(lines[1].text, "line 1");
        assert_eq!(lines[2].text, "line 2");

        state.close_file(info.id);
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

        let (_info, file, cancel) =
            open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");

        // 잘못된 정규식 쿼리: 괄호가 닫히지 않음
        let invalid_query = QueryDto {
            query: "status=(".to_string(),
            regex: true,
            case_sensitive: true,
        };

        // Matcher 컴파일 실패 시 에러 대신 하이라이트 없이 정상 줄 반환해야 함
        let matcher = Matcher::new(&invalid_query.query, (&invalid_query).into()).ok();
        assert!(matcher.is_none());

        let lines = read_lines_from_file(&file, 0, 3, matcher.as_ref())
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

        let lines_with_hl = read_lines_from_file(&file, 0, 3, valid_matcher.as_ref())
            .expect("정상 쿼리 줄 읽기 성공");
        assert_eq!(lines_with_hl.len(), 3);
        assert!(lines_with_hl[0].highlights.is_empty());
        assert_eq!(lines_with_hl[1].highlights.len(), 1); // "ERROR" 매치 1개
        assert!(lines_with_hl[2].highlights.is_empty());
    }

    #[test]
    fn test_get_filtered_lines_with_invalid_query() {
        let content = b"line 0: info\nline 1: err\nline 2: err again\n";
        let temp_file = TempFile::create(content);
        let state = AppState::default();

        let (_info, file, cancel) =
            open_file_internal(&state, temp_file.path_str()).expect("파일 열기 실패");
        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");

        // 무효 정규식 쿼리
        let invalid_query = QueryDto {
            query: "(".to_string(),
            regex: true,
            case_sensitive: false,
        };
        let matcher = Matcher::new(&invalid_query.query, (&invalid_query).into()).ok();
        assert!(matcher.is_none());

        let line_nos = [1, 2];
        let lines = read_filtered_lines_from_file(&file, &line_nos, matcher.as_ref())
            .expect("필터 줄 읽기 성공");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].no, 1);
        assert_eq!(lines[0].text, "line 1: err");
        assert!(lines[0].highlights.is_empty());
        assert_eq!(lines[1].no, 2);
        assert_eq!(lines[1].text, "line 2: err again");
        assert!(lines[1].highlights.is_empty());
    }
}
