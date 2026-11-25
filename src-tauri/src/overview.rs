// 파일 개요 스캔 IPC: 위치 분포(ERROR/WARN 밀도 지도)와 시간 히스토그램.
// 인덱싱이 끝난 파일을 백그라운드 스레드에서 한 번 스캔하고, 완료된
// 개요는 파일 ID 별로 AppState 에 보관해 get_timeline 이 재사용한다.
// 스캔 시작 여부는 프론트가 결정한다 (Rust 는 요청받을 때만 계산한다).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use riffle_core::{LogFile, Overview, TsContext, compute_overview, year_of_unix_secs};
use riffle_remote::AgentSession;
use serde::Serialize;
use tauri::ipc::Channel;

use crate::state::{AppState, OverviewJob};

/// 개요 스캔 진행 이벤트 (프론트 OverviewEvent 와 1:1)
#[derive(Debug, Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OverviewEvent {
    /// 스캔 진행률 (0.0~1.0)
    Progress {
        fraction: f64,
    },
    /// 스캔 완료. 초 단위 시간 분포는 저장만 하고 요약만 보낸다.
    Done {
        line_count: u64,
        pos_error: Vec<u32>,
        pos_warn: Vec<u32>,
        /// 시간 분포(타임스탬프)가 있었는지 여부
        has_time: bool,
        error_total: u64,
        warn_total: u64,
    },
    Cancelled,
    Error {
        message: String,
    },
}

/// get_timeline 응답 DTO (프론트 Timeline 과 1:1)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineDto {
    /// 막대 하나가 덮는 초 수
    pub bucket_secs: i64,
    /// 첫 막대의 시작 epoch 초
    pub start_sec: i64,
    /// 연속된 막대 목록 (빈 구간도 0으로 채움)
    pub buckets: Vec<TimeBucketDto>,
}

/// timeline 막대 하나의 DTO
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeBucketDto {
    pub start_sec: i64,
    pub total: u32,
    pub error: u32,
    pub warn: u32,
    pub first_line: u64,
}

/// 개요 스캔을 백그라운드에서 시작한다.
/// 같은 파일의 이전 작업은 자동 취소·제거된다.
/// 진행률(0~1)과 결과 요약은 on_event 로 보내고, 완료된 개요는 상태에 저장한다.
/// `local_offset_minutes` 는 시간대 없는 타임스탬프 해석용(분, UTC+9면 540)이다.
#[tauri::command]
pub async fn start_overview(
    state: tauri::State<'_, AppState>,
    id: u32,
    local_offset_minutes: i32,
    on_event: Channel<OverviewEvent>,
) -> Result<(), String> {
    // 병합 문서는 단일 파일 전용 스캔이라 지원하지 않는다
    let file = state.get_file(id)?;
    let agent = state.get_agent(id);
    // 파일이 이미 닫혔으면 Err — 스레드를 띄우지 않는다
    let job = state.insert_overview(id)?;
    std::thread::spawn(move || {
        run_overview(file, agent, local_offset_minutes, job, on_event);
    });
    Ok(())
}

/// 파일의 진행 중인 개요 스캔을 취소한다.
#[tauri::command]
pub async fn cancel_overview(state: tauri::State<'_, AppState>, id: u32) -> Result<(), String> {
    state.cancel_overview(id);
    Ok(())
}

/// 완료된 개요의 시간 분포를 buckets 개 막대 히스토그램으로 반환한다.
/// 개요가 없거나(미실행/진행 중) 시간 분포가 없으면(타임스탬프 없는 파일) null.
#[tauri::command]
pub async fn get_timeline(
    state: tauri::State<'_, AppState>,
    id: u32,
    buckets: u32,
) -> Result<Option<TimelineDto>, String> {
    let Some(job) = state.get_overview(id) else {
        return Ok(None);
    };
    // 개요는 크므로 락 안에서 timeline 만 계산하고 결과 DTO 만 꺼낸다
    let tl = {
        let guard = job.result.lock().unwrap_or_else(|p| p.into_inner());
        let Some(ov) = guard.as_ref() else {
            return Ok(None);
        };
        if ov.seconds.is_empty() {
            return Ok(None);
        }
        ov.timeline(buckets as usize)
    };
    Ok(Some(TimelineDto {
        bucket_secs: tl.bucket_secs,
        start_sec: tl.start_sec,
        buckets: tl
            .buckets
            .iter()
            .map(|b| TimeBucketDto {
                start_sec: b.start_sec,
                total: b.total,
                error: b.error,
                warn: b.warn,
                first_line: b.first_line,
            })
            .collect(),
    }))
}

/// 개요 스캔 스레드 본체.
/// 에이전트 세션이 있으면 서버 측 스캔을 먼저 시도하고 실패 시 로컬 스캔으로 폴백한다
/// (index/search/filter 와 같은 원격 폴백 정책).
fn run_overview(
    file: Arc<LogFile>,
    agent: Option<Arc<AgentSession>>,
    local_offset_minutes: i32,
    job: Arc<OverviewJob>,
    on_event: Channel<OverviewEvent>,
) {
    let panic_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 진행률 이벤트는 100ms 이상 간격으로만 보낸다 (마지막 1.0은 항상 보냄)
        let mut last_send = Instant::now()
            .checked_sub(Duration::from_millis(100))
            .unwrap_or_else(Instant::now);
        let mut send_progress = |fraction: f64| {
            let now = Instant::now();
            if fraction >= 1.0 || now.duration_since(last_send) >= Duration::from_millis(100) {
                last_send = now;
                let _ = on_event.send(OverviewEvent::Progress { fraction });
            }
        };

        let result = if let Some(session) = &agent {
            crate::remote::agent_overview(
                session,
                local_offset_minutes,
                &job.cancel,
                &mut send_progress,
            )
            .or_else(|e| {
                eprintln!("에이전트 개요 실패, 일반 방식으로 재시도합니다: {e}");
                scan_local(&file, local_offset_minutes, &job.cancel, &mut send_progress)
            })
        } else {
            scan_local(&file, local_offset_minutes, &job.cancel, &mut send_progress)
        };

        match result {
            Ok(Some(ov)) => {
                let done = OverviewEvent::Done {
                    line_count: ov.line_count,
                    pos_error: ov.pos_error.clone(),
                    pos_warn: ov.pos_warn.clone(),
                    has_time: !ov.seconds.is_empty(),
                    error_total: ov.pos_error.iter().map(|&v| v as u64).sum(),
                    warn_total: ov.pos_warn.iter().map(|&v| v as u64).sum(),
                };
                *job.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(ov));
                let _ = on_event.send(done);
            }
            Ok(None) => {
                let _ = on_event.send(OverviewEvent::Cancelled);
            }
            Err(message) => {
                let _ = on_event.send(OverviewEvent::Error { message });
            }
        }
    }));

    if let Err(panic_payload) = panic_result {
        let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "개요 스캔 스레드 실행 중 알 수 없는 패닉이 발생했습니다.".to_string()
        };
        let _ = on_event.send(OverviewEvent::Error { message: msg });
    }
}

/// 로컬(또는 SFTP RandomRead) 파일에서 개요를 직접 스캔한다.
/// syslog 연도는 compute_overview 가 파일 수정 시각으로 정한다.
fn scan_local(
    file: &LogFile,
    local_offset_minutes: i32,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<Option<Overview>, String> {
    let ctx = TsContext {
        local_offset_minutes,
        default_year: year_of_unix_secs(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        ),
    };
    compute_overview(file, &ctx, cancel, progress).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_log_path;
    use std::sync::atomic::Ordering;

    /// 임시 파일을 상태에 등록하고 파일 ID를 반환한다.
    fn insert_test_file(state: &AppState, content: &[u8]) -> (u32, std::path::PathBuf) {
        let path = temp_log_path("ovw_state");
        std::fs::write(&path, content).expect("임시 파일 쓰기 실패");
        let file = Arc::new(LogFile::open(&path).expect("LogFile::open 실패"));
        let id = state.insert_file(file, Arc::new(AtomicBool::new(false)), path.clone(), false);
        (id, path)
    }

    #[test]
    fn test_overview_job_lifecycle() {
        let state = AppState::default();
        let (id, path) = insert_test_file(&state, b"a\nb\n");

        // 등록: 파일이 있어야 성공, 같은 파일 재등록 시 이전 작업은 취소된다
        let j1 = state.insert_overview(id).expect("등록 실패");
        let j2 = state.insert_overview(id).expect("재등록 실패");
        assert!(j1.cancel.load(Ordering::Relaxed));
        assert!(!j2.cancel.load(Ordering::Relaxed));
        assert!(state.get_overview(id).is_some());

        // 취소
        state.cancel_overview(id);
        assert!(j2.cancel.load(Ordering::Relaxed));

        // 닫힌 파일에는 등록 불가
        assert!(state.insert_overview(999).is_err());

        // close_file 은 개요 작업을 정리한다
        let (id2, path2) = insert_test_file(&state, b"x\n");
        let j3 = state.insert_overview(id2).expect("등록 실패");
        state.close_file(id2);
        assert!(j3.cancel.load(Ordering::Relaxed));
        assert!(state.get_overview(id2).is_none());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
    }

    #[test]
    fn test_scan_local_stores_result() {
        // run_overview 없이 scan_local 경로만 검증한다
        let path = temp_log_path("ovw_scan");
        std::fs::write(
            &path,
            b"2026-11-14 10:00:00 INFO a\n2026-11-14 10:00:01 ERROR b\nplain\n",
        )
        .expect("임시 파일 쓰기 실패");
        let file = LogFile::open(&path).expect("열기 실패");
        file.build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("인덱싱 실패");

        let ov = scan_local(&file, 0, &AtomicBool::new(false), &mut |_| {})
            .expect("스캔 실패")
            .expect("취소 아님");
        assert_eq!(ov.line_count, 3);
        assert_eq!(ov.seconds.len(), 2);
        // "plain" 줄은 직전 시각(10:00:01)에 귀속된다
        assert_eq!(ov.seconds[1].total, 2);

        // 취소 플래그가 세워져 있으면 None
        let r = scan_local(&file, 0, &AtomicBool::new(true), &mut |_| {}).expect("스캔 실패");
        assert!(r.is_none());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_overview_event_serializes_camel_case() {
        // 프론트 계약: {kind:"done", lineCount, posError, posWarn, hasTime, errorTotal, warnTotal}
        let v = serde_json::to_value(OverviewEvent::Done {
            line_count: 10,
            pos_error: vec![1, 2],
            pos_warn: vec![0, 3],
            has_time: true,
            error_total: 3,
            warn_total: 3,
        })
        .unwrap();
        assert_eq!(v["kind"], "done");
        assert_eq!(v["lineCount"], 10);
        assert_eq!(v["posError"], serde_json::json!([1, 2]));
        assert_eq!(v["hasTime"], true);
        assert_eq!(v["errorTotal"], 3);
        assert_eq!(v["warnTotal"], 3);

        let v = serde_json::to_value(OverviewEvent::Progress { fraction: 0.5 }).unwrap();
        assert_eq!(v["kind"], "progress");
        assert_eq!(v["fraction"], 0.5);

        let v = serde_json::to_value(OverviewEvent::Cancelled).unwrap();
        assert_eq!(v["kind"], "cancelled");

        let v = serde_json::to_value(OverviewEvent::Error {
            message: "x".into(),
        })
        .unwrap();
        assert_eq!(v["kind"], "error");
        assert_eq!(v["message"], "x");

        // TimelineDto 도 camelCase
        let v = serde_json::to_value(TimelineDto {
            bucket_secs: 60,
            start_sec: 100,
            buckets: vec![TimeBucketDto {
                start_sec: 100,
                total: 5,
                error: 1,
                warn: 2,
                first_line: 7,
            }],
        })
        .unwrap();
        assert_eq!(v["bucketSecs"], 60);
        assert_eq!(v["startSec"], 100);
        assert_eq!(v["buckets"][0]["firstLine"], 7);
    }
}
