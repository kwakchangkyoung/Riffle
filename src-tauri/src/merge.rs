//! 병합 문서(여러 로그의 시간순 병합 뷰)용 IPC와 위임 로직.
//!
//! 병합 파일은 실제 파일을 만들지 않는 가상 뷰이며, 희소 병합 인덱스
//! (MergeIndex)가 원본 LogFile들을 Arc로 보유한다. 줄 읽기·검색·필터·복사는
//! 병합 위치를 원본 줄로 해석(resolve)한 뒤 각 원본의 기존 코어 API에 위임한다.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use riffle_core::{FilterSet, Matcher, MergeIndex, MergedLine, TsContext, year_of_unix_secs};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

use crate::commands::{
    FileInfo, FullLine, IndexProgressDto, LineDto, MAX_FULL_LINE_BYTES, SearchEvent,
};
use crate::state::{AppState, MergeDoc, SearchState};

/// 병합에 참여할 수 있는 원본 탭 수 범위
pub const MIN_MERGE_SOURCES: usize = 2;
pub const MAX_MERGE_SOURCES: usize = 8;

/// 병합 원본 정보 DTO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeSourceDto {
    /// 원본 순번 (병합 선택 순서)
    pub index: u16,
    /// 원본 파일 경로 또는 원격 URI
    pub path: String,
    /// 병합 시점 스냅샷의 원본 줄 수
    pub line_count: u64,
}

/// MergedLine 목록을 원본별로 묶어 읽고 입력 순서대로 LineDto를 만든다.
/// positions[i]는 resolved[i]에 대응하는 병합 위치다 (반환 dto.no에 그대로 기록).
/// 해석되지 않는 위치(확인 범위 밖)는 건너뛴다.
pub fn read_merged_lines(
    index: &MergeIndex,
    resolved: &[Option<MergedLine>],
    positions: &[u64],
    matcher: Option<&Matcher>,
    filter: Option<&FilterSet>,
) -> Result<Vec<LineDto>, String> {
    let sources = index.sources();
    let nsrc = sources.len();

    // 원본별 줄 번호 목록 (병합 순서 유지 — 같은 원본 안에서는 오름차순)
    let mut per_src: Vec<Vec<u64>> = vec![Vec::new(); nsrc];
    for ml in resolved.iter().flatten() {
        per_src[ml.source as usize].push(ml.line);
    }
    // 원본별로 한 번씩 읽는다. read_lines_at은 요청 순서대로 반환한다.
    let mut fetched: Vec<VecDeque<riffle_core::Line>> = Vec::with_capacity(nsrc);
    for (s, lines) in per_src.iter().enumerate() {
        let lines = sources[s]
            .read_lines_at(lines, matcher, filter)
            .map_err(|e| format!("병합 원본 {s} 줄 읽기 실패: {e}"))?;
        fetched.push(lines.into());
    }

    let mut out = Vec::with_capacity(resolved.len());
    for (i, ml) in resolved.iter().enumerate() {
        let Some(ml) = ml else { continue };
        let Some(line) = fetched[ml.source as usize].pop_front() else {
            return Err("병합 원본 줄 읽기 결과가 요청과 맞지 않습니다.".to_string());
        };
        let mut dto = LineDto::from(line);
        dto.no = positions[i];
        dto.source = Some(ml.source);
        dto.source_line = Some(ml.line);
        out.push(dto);
    }
    Ok(out)
}

/// 병합 위치 목록(오름차순)을 해석해 원본 줄 내용까지 읽어 반환한다.
pub fn read_merged_positions(
    index: &MergeIndex,
    positions: &[u64],
    matcher: Option<&Matcher>,
    filter: Option<&FilterSet>,
) -> Result<Vec<LineDto>, String> {
    let resolved = index
        .resolve_many(positions)
        .map_err(|e| format!("병합 위치 해석 실패: {e}"))?;
    read_merged_lines(index, &resolved, positions, matcher, filter)
}

/// 병합 위치 [start, end](양끝 포함)의 원문을 원본에서 읽어 병합 순서로 이어 붙인다.
/// 같은 원본의 연속 구간은 read_text_range로 한 번에 읽어 랜덤 읽기 횟수를 줄인다.
/// 누적 크기가 max_bytes를 넘으면 Err.
pub fn read_merged_text_range(
    index: &MergeIndex,
    start: u64,
    end: u64,
    max_bytes: u64,
) -> Result<String, String> {
    if end < start {
        return Ok(String::new());
    }
    let merged = index
        .resolve(start, end - start + 1)
        .map_err(|e| format!("병합 위치 해석 실패: {e}"))?;
    let sources = index.sources();
    let mut out = String::new();
    let mut i = 0usize;
    while i < merged.len() {
        // 같은 원본의 연속 구간(run)을 모은다
        let s = merged[i].source as usize;
        let run_start = merged[i].line;
        let mut run_end = run_start;
        while i + 1 < merged.len()
            && merged[i + 1].source as usize == s
            && merged[i + 1].line == run_end + 1
        {
            i += 1;
            run_end = merged[i].line;
        }
        let remaining = max_bytes.saturating_sub(out.len() as u64);
        if remaining == 0 {
            return Err(format!(
                "복사 범위가 최대 크기({} bytes)를 초과합니다.",
                max_bytes
            ));
        }
        let text = sources[s]
            .read_text_range(run_start, run_end, remaining)
            .map_err(|e| format!("병합 원본 {s} 범위 읽기 실패: {e}"))?;
        out.push_str(&text);
        // 원본 마지막 줄이 개행 없이 끝났으면 다음 run과 붙지 않게 보정한다
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        i += 1;
    }
    Ok(out)
}

/// 병합 인덱스 구축 백그라운드 스레드 본체.
/// 원본 인덱싱이 끝날 때까지 기다린 뒤(원본 탭이 닫혀 인덱싱이 취소됐으면
/// 그 시점까지 확인된 줄로 스냅샷) MergeIndex::build를 실행한다.
/// 진행률은 IndexProgressDto의 바이트 필드에 줄 수를 매핑해 전송한다.
fn run_merge_build(
    merge: Arc<MergeDoc>,
    source_cancels: Vec<Arc<AtomicBool>>,
    cancel: Arc<AtomicBool>,
    on_progress: Channel<IndexProgressDto>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. 원본 인덱싱 완료 대기. 원본 취소 플래그가 서면 부분 스냅샷으로 진행한다.
        for (s, f) in merge.index.sources().iter().enumerate() {
            while !f.index().progress().done {
                if cancel.load(Ordering::Relaxed) || source_cancels[s].load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            if cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
        }

        // 2. 병합 인덱스 구축 (진행률은 50ms 스로틀, 첫/마지막은 항상 전송)
        let mut is_first = true;
        let mut last_send = Instant::now();
        merge.index.build(&cancel, &mut |p| {
            let now = Instant::now();
            let should_send =
                is_first || p.done || now.duration_since(last_send) >= Duration::from_millis(50);
            if !should_send {
                return;
            }
            is_first = false;
            last_send = now;
            let dto = IndexProgressDto {
                indexed_bytes: p.processed_lines,
                total_bytes: p.total_lines,
                line_count: p.processed_lines,
                done: p.done,
                content_bytes: p.total_lines,
                decompressing: false,
                error: None,
                notice: None,
            };
            if let Err(e) = on_progress.send(dto) {
                eprintln!("병합 진행률 채널 전송 오류: {e}");
            }
        })
    }));

    // 오류가 있으면 error 필드로 알리는 최종 이벤트를 보낸다.
    let error = match &result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e.to_string()),
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "병합 스레드 실행 중 알 수 없는 패닉이 발생했습니다.".to_string()
            };
            Some(msg)
        }
    };
    if let Some(msg) = error {
        let p = merge.index.progress();
        let _ = on_progress.send(IndexProgressDto {
            indexed_bytes: p.processed_lines,
            total_bytes: p.total_lines,
            line_count: p.processed_lines,
            done: false,
            content_bytes: p.total_lines,
            decompressing: false,
            error: Some(msg),
            notice: None,
        });
    }
}

/// 원본 수집 결과: 파일 핸들, 경로, 인덱싱 취소 플래그
struct MergeSources {
    files: Vec<Arc<riffle_core::LogFile>>,
    paths: Vec<String>,
    cancels: Vec<Arc<AtomicBool>>,
}

/// 원본 ID 목록에서 열린 단일 파일의 핸들·경로·취소 플래그를 수집한다.
/// 병합 문서를 원본으로 지정하거나 닫힌 ID가 있으면 Err.
fn collect_merge_sources(state: &AppState, ids: &[u32]) -> Result<MergeSources, String> {
    let mut files = Vec::with_capacity(ids.len());
    let mut paths = Vec::with_capacity(ids.len());
    let mut cancels = Vec::with_capacity(ids.len());
    let open = state.files.lock().unwrap_or_else(|p| p.into_inner());
    for id in ids {
        let Some(of) = open.get(id) else {
            return Err(format!("ID {id}에 해당하는 열린 파일을 찾을 수 없습니다."));
        };
        let Some(file) = of.doc.as_file() else {
            return Err("병합 파일은 다시 병합할 수 없습니다.".to_string());
        };
        files.push(file.clone());
        paths.push(of.path.to_string_lossy().into_owned());
        cancels.push(of.cancel.clone());
    }
    Ok(MergeSources {
        files,
        paths,
        cancels,
    })
}

/// 열려 있는 탭 2~8개를 타임스탬프 기준으로 섞은 가상 병합 파일을 연다.
/// 병합 파일은 실제 파일을 만들지 않으며, 원본 탭이 닫혀도 Arc로 원본을 유지한다.
/// local_offset_minutes는 JS getTimezoneOffset()의 부호를 반전한 값
/// (로컬 시간 = UTC + offset)이다.
#[tauri::command]
pub async fn open_merge(
    state: tauri::State<'_, AppState>,
    source_ids: Vec<u32>,
    local_offset_minutes: i32,
    on_progress: Channel<IndexProgressDto>,
) -> Result<FileInfo, String> {
    // 중복 id를 제거하고 개수 범위를 검사한다
    let mut ids = Vec::with_capacity(source_ids.len());
    for id in source_ids {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.len() < MIN_MERGE_SOURCES || ids.len() > MAX_MERGE_SOURCES {
        return Err(format!(
            "병합할 탭은 {MIN_MERGE_SOURCES}~{MAX_MERGE_SOURCES}개를 선택해야 합니다 ({}개).",
            ids.len()
        ));
    }

    // 원본 핸들·경로·취소 플래그를 수집한다 (병합 문서는 원본으로 쓸 수 없다)
    let MergeSources {
        files,
        paths,
        cancels,
    } = collect_merge_sources(&state, &ids)?;

    let size: u64 = files.iter().map(|f| f.source().len()).sum();
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ctx = TsContext {
        local_offset_minutes,
        default_year: year_of_unix_secs(now_secs),
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let merge = MergeDoc {
        index: MergeIndex::new(files, ctx),
        source_paths: paths,
    };
    let id = state.insert_merge(merge, cancel.clone());
    let merge = state.get_merge(id)?;

    std::thread::spawn(move || {
        run_merge_build(merge, cancels, cancel, on_progress);
    });

    Ok(FileInfo {
        id,
        path: format!("merge://{id}"),
        size,
        compressed: false,
        remote: false,
        source: "merge".to_string(),
        agent: false,
    })
}

/// 유효 타임스탬프가 ts_ms 이상인 첫 병합 위치를 반환한다.
/// 없으면 병합 줄 수(끝 위치)를 반환한다.
#[tauri::command]
pub async fn merge_position_at_time(
    state: tauri::State<'_, AppState>,
    id: u32,
    ts_ms: i64,
) -> Result<u64, String> {
    let merge = state.get_merge(id)?;
    tauri::async_runtime::spawn_blocking(move || merge.index.position_at_time(ts_ms))
        .await
        .map_err(|e| format!("병합 위치 계산 비동기 태스크 실패: {e}"))
}

/// 병합 원본 목록(순번, 경로, 스냅샷 줄 수)을 반환한다.
#[tauri::command]
pub async fn merge_sources(
    state: tauri::State<'_, AppState>,
    id: u32,
) -> Result<Vec<MergeSourceDto>, String> {
    let merge = state.get_merge(id)?;
    Ok(merge
        .source_paths
        .iter()
        .enumerate()
        .map(|(s, path)| MergeSourceDto {
            index: s as u16,
            path: path.clone(),
            line_count: merge.index.line_cap(s),
        })
        .collect())
}

/// 병합 문서에 대한 백그라운드 검색 스레드 본체.
/// 각 원본에 기존 search_filtered를 실행하고 결과 (원본, 줄)을
/// map_positions로 병합 위치로 변환해 정렬된 results에 채운다.
pub fn run_merge_search(
    merge: Arc<MergeDoc>,
    matcher: Matcher,
    filter_set: Option<Arc<FilterSet>>,
    search_state: Arc<SearchState>,
    on_event: Channel<SearchEvent>,
) {
    let cancel = search_state.cancel.clone();
    let results = search_state.results.clone();
    let done = search_state.done.clone();
    let capped = search_state.capped.clone();
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let index = &merge.index;
        let nsrc = index.source_count();
        let total_bytes: u64 = index.sources().iter().map(|f| f.source().len()).sum();
        let mut per_source: Vec<Vec<u64>> = vec![Vec::new(); nsrc];

        let mut last_progress_send = Instant::now();
        let mut sent_first_match = false;
        let mut scanned_base = 0u64;
        let mut match_base = 0u64;
        let mut cancelled = false;
        let mut hit_cap = false;

        for (s, file) in index.sources().iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let outcome = riffle_core::search_filtered(
                file,
                &matcher,
                filter_set.as_deref(),
                &cancel,
                &mut |batch| {
                    per_source[s].extend_from_slice(batch);
                },
                &mut |p| {
                    let now = Instant::now();
                    let total_match = match_base + p.match_count;
                    let is_first_match = !sent_first_match && total_match > 0;
                    let should_send = is_first_match
                        || now.duration_since(last_progress_send) >= Duration::from_millis(50);
                    if should_send {
                        if is_first_match {
                            sent_first_match = true;
                        }
                        last_progress_send = now;
                        let _ = on_event.send(SearchEvent::Progress {
                            scanned_bytes: scanned_base + p.scanned_bytes,
                            total_bytes,
                            match_count: total_match,
                        });
                    }
                },
            );
            match outcome {
                Ok(riffle_core::SearchOutcome::Completed { .. }) => {}
                Ok(riffle_core::SearchOutcome::Capped { .. }) => {
                    hit_cap = true;
                }
                Ok(riffle_core::SearchOutcome::Cancelled) => {
                    cancelled = true;
                    break;
                }
                Err(err) => {
                    let _ = on_event.send(SearchEvent::Error {
                        message: err.to_string(),
                    });
                    return;
                }
            }
            scanned_base += file.source().len();
            match_base += per_source[s].len() as u64;
        }

        if cancelled && !hit_cap {
            let _ = on_event.send(SearchEvent::Cancelled);
            return;
        }

        // 원본별 결과는 오름차순 — 한 번의 병합 재실행으로 병합 위치 목록을 얻는다
        match index.map_positions(&per_source) {
            Ok(mapped) => {
                let total = mapped.len() as u64;
                if let Ok(mut lock) = results.write() {
                    *lock = mapped;
                }
                done.store(true, Ordering::Relaxed);
                capped.store(hit_cap, Ordering::Relaxed);
                let _ = on_event.send(SearchEvent::Done {
                    total,
                    capped: hit_cap,
                });
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
            "검색 스레드 실행 중 알 수 없는 패닉이 발생했습니다.".to_string()
        };
        let _ = on_event.send(SearchEvent::Error { message: msg });
    }
}

/// 병합 문서에 대한 백그라운드 필터 스캔 스레드 본체.
/// 각 원본에 filter_scan을 실행하고 결과를 map_positions로 병합 위치
/// LineSet에 채운다.
pub fn run_merge_filter_scan(
    merge: Arc<MergeDoc>,
    filter_state: Arc<crate::state::FilterState>,
    on_event: Channel<SearchEvent>,
) {
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let index = &merge.index;
        let nsrc = index.source_count();
        let total_bytes: u64 = index.sources().iter().map(|f| f.source().len()).sum();
        let mut per_source: Vec<Vec<u64>> = vec![Vec::new(); nsrc];

        let mut last_progress_send = Instant::now();
        let mut sent_first_match = false;
        let mut scanned_base = 0u64;
        let mut match_base = 0u64;
        let mut cancelled = false;

        for (s, file) in index.sources().iter().enumerate() {
            if filter_state.cancel.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let outcome = riffle_core::filter_scan(
                file,
                filter_state.set.as_ref(),
                &filter_state.cancel,
                &mut |batch| {
                    per_source[s].extend_from_slice(batch);
                },
                &mut |p| {
                    let now = Instant::now();
                    let total_match = match_base + p.match_count;
                    let is_first_match = !sent_first_match && total_match > 0;
                    let should_send = is_first_match
                        || now.duration_since(last_progress_send) >= Duration::from_millis(50);
                    if should_send {
                        if is_first_match {
                            sent_first_match = true;
                        }
                        last_progress_send = now;
                        let _ = on_event.send(SearchEvent::Progress {
                            scanned_bytes: scanned_base + p.scanned_bytes,
                            total_bytes,
                            match_count: total_match,
                        });
                    }
                },
            );
            match outcome {
                Ok(riffle_core::SearchOutcome::Completed { .. })
                | Ok(riffle_core::SearchOutcome::Capped { .. }) => {}
                Ok(riffle_core::SearchOutcome::Cancelled) => {
                    cancelled = true;
                    break;
                }
                Err(err) => {
                    let _ = on_event.send(SearchEvent::Error {
                        message: err.to_string(),
                    });
                    return;
                }
            }
            scanned_base += file.source().len();
            match_base += per_source[s].len() as u64;
        }

        // 취소됐어도 이미 모은 결과는 유지한다 (기존 필터 스캔과 같은 규칙)
        match index.map_positions(&per_source) {
            Ok(mapped) => {
                let capped = {
                    let mut lines = filter_state
                        .lines
                        .write()
                        .unwrap_or_else(|p| p.into_inner());
                    for pos in mapped {
                        lines.push_line(pos);
                    }
                    lines.range_count() > riffle_core::MAX_FILTER_RANGES
                };
                if capped {
                    filter_state.capped.store(true, Ordering::Relaxed);
                }
                let total = {
                    let lines = filter_state.lines.read().unwrap_or_else(|p| p.into_inner());
                    lines.len()
                };
                if cancelled && !capped {
                    let _ = on_event.send(SearchEvent::Cancelled);
                } else {
                    filter_state.done.store(true, Ordering::Relaxed);
                    let _ = on_event.send(SearchEvent::Done { total, capped });
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

/// 병합 위치 no의 원문 전체를 반환한다 (원본으로 위임).
pub fn get_merged_full_line(merge: &MergeDoc, no: u64) -> Result<Option<FullLine>, String> {
    let resolved = merge
        .index
        .resolve_many(&[no])
        .map_err(|e| format!("병합 위치 해석 실패: {e}"))?;
    let Some(Some(ml)) = resolved.first() else {
        return Ok(None);
    };
    let file = &merge.index.sources()[ml.source as usize];
    // 16KB 이내 줄은 read_lines 결과로 바로 반환한다 (기존 get_full_line과 같은 규칙)
    let meta = file
        .read_lines(ml.line, 1, None, None)
        .map_err(|e| format!("병합 원본 줄 읽기 실패: {e}"))?;
    let Some(line) = meta.into_iter().next() else {
        return Ok(None);
    };
    if !line.truncated {
        return Ok(Some(FullLine {
            no,
            text: line.text,
            truncated: false,
            byte_len: line.byte_len,
        }));
    }
    let Some((text, truncated)) = file
        .read_full_line(ml.line, MAX_FULL_LINE_BYTES)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Doc;
    use riffle_core::{LogFile, SearchOptions};
    use std::path::PathBuf;

    fn ctx() -> TsContext {
        TsContext {
            local_offset_minutes: 0,
            default_year: 2026,
        }
    }

    /// 내용으로 임시 파일을 만들고 인덱싱된 LogFile을 AppState에 등록한다.
    /// 반환된 경로는 테스트 끝에서 삭제한다.
    fn open_indexed(state: &AppState, prefix: &str, content: &str) -> (PathBuf, u32) {
        let path = crate::testutil::temp_log_path(prefix);
        std::fs::write(&path, content).expect("임시 파일 쓰기 실패");
        let file = Arc::new(LogFile::open(&path).expect("열기 실패"));
        file.build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("인덱싱 실패");
        let id = state.insert_file(file, Arc::new(AtomicBool::new(false)), path.clone(), false);
        (path, id)
    }

    /// open_merge와 같은 경로로 병합 문서를 만들고 인덱스를 구축한다.
    fn build_merge(state: &AppState, ids: &[u32]) -> Arc<MergeDoc> {
        let MergeSources { files, paths, .. } =
            collect_merge_sources(state, ids).expect("원본 수집 실패");
        let merge = MergeDoc {
            index: MergeIndex::new(files, ctx()),
            source_paths: paths,
        };
        let id = state.insert_merge(merge, Arc::new(AtomicBool::new(false)));
        let doc = state.get_merge(id).expect("병합 조회 실패");
        doc.index
            .build(&AtomicBool::new(false), &mut |_| {})
            .expect("병합 구축 실패");
        doc
    }

    fn remove(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn merge_read_positions_order_and_source_fields() {
        let state = AppState::default();
        let (p1, id1) = open_indexed(
            &state,
            "mv_a",
            "2026-11-12T00:00:01Z a1\n2026-11-12T00:00:04Z a2\n",
        );
        let (p2, id2) = open_indexed(
            &state,
            "mv_b",
            "2026-11-12T00:00:02Z b1\n2026-11-12T00:00:05Z b2\n",
        );
        let (p3, id3) = open_indexed(&state, "mv_c", "2026-11-12T00:00:03Z c1\n");

        let merge = build_merge(&state, &[id1, id2, id3]);
        assert_eq!(merge.index.line_count(), 5);

        let positions: Vec<u64> = (0..5).collect();
        let lines =
            read_merged_positions(&merge.index, &positions, None, None).expect("병합 줄 읽기 실패");
        assert_eq!(lines.len(), 5);

        // 병합 순서: a1(1s) b1(2s) c1(3s) a2(4s) b2(5s)
        let expected: [(u64, u16, u64, &str); 5] = [
            (0, 0, 0, "a1"),
            (1, 1, 0, "b1"),
            (2, 2, 0, "c1"),
            (3, 0, 1, "a2"),
            (4, 1, 1, "b2"),
        ];
        for (dto, &(no, src, src_line, suffix)) in lines.iter().zip(expected.iter()) {
            assert_eq!(dto.no, no);
            assert_eq!(dto.source, Some(src));
            assert_eq!(dto.source_line, Some(src_line));
            assert!(dto.text.ends_with(suffix), "{}", dto.text);
        }

        // merge_sources 정보 확인
        let doc = state.get_doc(5).ok();
        let _ = doc; // id는 상태 카운터 의존 — merge_sources 로직은 아래에서 직접 확인
        for (s, f) in merge.index.sources().iter().enumerate() {
            assert!(merge.source_paths[s].contains(".log") || !merge.source_paths[s].is_empty());
            assert_eq!(merge.index.line_cap(s), f.index().line_count());
        }

        remove(&p1);
        remove(&p2);
        remove(&p3);
    }

    #[test]
    fn merge_search_positions_sorted() {
        let state = AppState::default();
        let (p1, id1) = open_indexed(
            &state,
            "ms_a",
            "2026-11-12T00:00:01Z ERROR a1\n2026-11-12T00:00:04Z ok a2\n",
        );
        let (p2, id2) = open_indexed(
            &state,
            "ms_b",
            "2026-11-12T00:00:02Z ERROR b1\n2026-11-12T00:00:03Z ERROR b2\n",
        );

        let merge = build_merge(&state, &[id1, id2]);

        // 원본별 검색 후 map_positions로 병합 위치 변환 (run_merge_search의 핵심 경로)
        let matcher = Matcher::new(
            "ERROR",
            SearchOptions {
                regex: false,
                case_sensitive: true,
            },
        )
        .expect("매처 생성 실패");
        let mut per_source: Vec<Vec<u64>> = vec![Vec::new(); merge.index.source_count()];
        for (s, f) in merge.index.sources().iter().enumerate() {
            riffle_core::search(
                f,
                &matcher,
                &AtomicBool::new(false),
                &mut |batch| {
                    per_source[s].extend_from_slice(batch);
                },
                &mut |_| {},
            )
            .expect("원본 검색 실패");
        }
        let mapped = merge.index.map_positions(&per_source).expect("변환 실패");

        // ERROR 줄: a0(1s) → 병합 0, b0(2s) → 병합 1, b1(3s) → 병합 2
        assert_eq!(mapped, vec![0, 1, 2]);
        // 결과는 병합 위치 오름차순이다
        assert!(mapped.windows(2).all(|w| w[0] < w[1]));

        remove(&p1);
        remove(&p2);
    }

    #[test]
    fn merge_survives_source_tab_close() {
        let state = AppState::default();
        let (p1, id1) = open_indexed(&state, "mc_a", "2026-11-12T00:00:01Z a1\n");
        let (p2, id2) = open_indexed(&state, "mc_b", "2026-11-12T00:00:02Z b1\n");

        let merge = build_merge(&state, &[id1, id2]);

        // 원본 탭을 닫아도 병합 탭은 Arc로 원본을 유지한다
        state.close_file(id1);
        state.close_file(id2);
        assert_eq!(state.file_count(), 1); // 병합 문서만 남는다

        let lines = read_merged_positions(&merge.index, &[0, 1], None, None).expect("읽기 실패");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].text.ends_with("a1"));
        assert!(lines[1].text.ends_with("b1"));
        assert_eq!(merge.index.position_of(1, 0), Some(1));

        remove(&p1);
        remove(&p2);
    }

    #[test]
    fn collect_merge_sources_rejects_bad_inputs() {
        let state = AppState::default();
        let (p1, id1) = open_indexed(&state, "mb_a", "2026-11-12T00:00:01Z a1\n");
        let (p2, id2) = open_indexed(&state, "mb_b", "2026-11-12T00:00:02Z b1\n");

        // 닫힌 ID는 실패
        assert!(collect_merge_sources(&state, &[id1, 9999]).is_err());

        // 병합 문서는 원본으로 쓸 수 없다
        let _merge = build_merge(&state, &[id1, id2]);
        let merge_id = state
            .files
            .lock()
            .unwrap()
            .iter()
            .find_map(|(id, of)| matches!(of.doc, Doc::Merge(_)).then_some(*id))
            .expect("병합 문서 없음");
        assert!(collect_merge_sources(&state, &[id1, merge_id]).is_err());

        remove(&p1);
        remove(&p2);
    }

    #[test]
    fn merge_full_line_and_copy() {
        let state = AppState::default();
        let (p1, id1) = open_indexed(
            &state,
            "mf_a",
            "2026-11-12T00:00:01Z a1\n2026-11-12T00:00:03Z a2\n",
        );
        let (p2, id2) = open_indexed(&state, "mf_b", "2026-11-12T00:00:02Z b1\n");

        let merge = build_merge(&state, &[id1, id2]);

        // get_full_line 위임: 병합 위치 1 = b의 줄 0
        let full = get_merged_full_line(&merge, 1)
            .expect("읽기 실패")
            .expect("줄 없음");
        assert_eq!(full.no, 1);
        assert!(full.text.ends_with("b1"));

        // copy 위임: 병합 순서대로 이어진 원문
        let text = read_merged_text_range(&merge.index, 0, 2, 1024 * 1024).expect("범위 읽기 실패");
        assert_eq!(
            text,
            "2026-11-12T00:00:01Z a1\n2026-11-12T00:00:02Z b1\n2026-11-12T00:00:03Z a2\n"
        );

        remove(&p1);
        remove(&p2);
    }
}
