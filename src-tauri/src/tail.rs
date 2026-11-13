// tail 모드(실시간 추적) IPC 커맨드와 파일 감시 루프

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use notify::{Event as NotifyEvent, RecursiveMode, Watcher};
use riffle_core::{LogFile, RefreshOutcome};
use serde::Serialize;
use tauri::ipc::Channel;

use crate::state::{AppState, FilterMap, FilterState, TailHandle};

/// tail 루프가 Appended 시 활성 필터를 갱신하기 위한 컨텍스트
pub struct TailFilterCtx {
    /// AppState.filters와 공유하는 필터 맵
    pub filters: FilterMap,
    /// 감시 대상 파일 ID
    pub file_id: u32,
}

/// 파일 변경 이벤트 디바운스 시간
const DEBOUNCE: Duration = Duration::from_millis(50);
/// FSEvents 지연 등에 대비한 폴링 주기
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// tail 감시 루프의 내부 채널 메시지.
/// notify 이벤트와 외부 중지 신호(Stop)를 하나의 채널로 받아
/// 대기 중에도 즉시 깨어날 수 있게 한다.
/// 이벤트 내용은 사용하지 않고 도착 여부만이 wake 신호다.
pub enum TailLoopMsg {
    Event,
    Stop,
}

/// tail 모드 이벤트 DTO (프론트엔드 ipc.ts의 TailEvent와 1:1)
#[derive(Serialize, Clone)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum TailEvent {
    Appended {
        line_count: u64,
        total_bytes: u64,
        old_line_count: u64,
        /// 활성 필터의 보이는 줄 수 (필터가 없으면 None)
        filter_count: Option<u64>,
    },
    Rotated {
        reason: String,
    },
    Error {
        message: String,
    },
}

/// tail 추가분을 해당 파일의 활성 필터들에 반영하고 필터 보기 줄 수를 반환한다.
/// 파일당 활성 필터는 하나이지만 방어적으로 모두 갱신하고 최대 줄 수를 돌려준다.
/// 필터가 없으면 None.
fn update_filters_on_append(
    filters: &FilterMap,
    file_id: u32,
    file: &LogFile,
    old_line_count: u64,
    new_line_count: u64,
) -> Option<u64> {
    let states: Vec<Arc<FilterState>> = {
        let map = filters.lock().unwrap_or_else(|p| p.into_inner());
        map.values()
            .filter(|f| f.file_id == file_id)
            .cloned()
            .collect()
    };
    if states.is_empty() {
        return None;
    }
    let mut max_len = 0u64;
    for st in &states {
        st.apply_appended(file, old_line_count, new_line_count);
        let len = st.lines.read().map(|l| l.len()).unwrap_or(0);
        max_len = max_len.max(len);
    }
    Some(max_len)
}

/// refresh 1회 수행 후 결과를 이벤트로 전달한다.
/// 감시를 계속할지 여부를 반환한다 (로테이션 감지 시 false).
/// Appended 시 filter_ctx의 활성 필터 LineSet을 갱신하고 filter_count를 이벤트에 담는다.
fn refresh_once(
    file: &LogFile,
    filter_ctx: &TailFilterCtx,
    on_event: &mut impl FnMut(TailEvent),
) -> bool {
    match file.refresh() {
        Ok(RefreshOutcome::Unchanged) | Ok(RefreshOutcome::Pending) => true,
        Ok(RefreshOutcome::Appended {
            old_line_count,
            new_line_count,
            new_len,
        }) => {
            // 필터 갱신을 이벤트 전송보다 먼저 수행해 filter_count가 최신 값이 되게 한다
            let filter_count = update_filters_on_append(
                &filter_ctx.filters,
                filter_ctx.file_id,
                file,
                old_line_count,
                new_line_count,
            );
            on_event(TailEvent::Appended {
                line_count: new_line_count,
                total_bytes: new_len,
                old_line_count,
                filter_count,
            });
            true
        }
        Ok(RefreshOutcome::Truncated) => {
            on_event(TailEvent::Rotated {
                reason: "truncated".to_string(),
            });
            false
        }
        Ok(RefreshOutcome::Replaced) => {
            on_event(TailEvent::Rotated {
                reason: "replaced".to_string(),
            });
            false
        }
        Err(e) => {
            on_event(TailEvent::Error {
                message: e.to_string(),
            });
            true
        }
    }
}

/// tail 감시 루프 본체.
/// 파일의 부모 디렉터리를 NonRecursive로 감시하고, 이벤트 수신 또는 1초 타임아웃마다
/// 디바운스(50ms) 확인 후 refresh()를 호출한다.
/// stop 플래그가 서거나 Stop 메시지가 오거나 로테이션(Rotated)이 감지되면 반환한다.
/// `tx`는 notify watcher가 Event를 보내는 송신자, `rx`는 루프가 받는 수신자다
/// (Stop 전송자는 TailHandle이 보관해 중지 시 즉시 깨운다).
pub fn run_tail_loop(
    file: Arc<LogFile>,
    watch_dir: &Path,
    stop: Arc<AtomicBool>,
    tx: std::sync::mpsc::Sender<TailLoopMsg>,
    rx: std::sync::mpsc::Receiver<TailLoopMsg>,
    filter_ctx: TailFilterCtx,
    mut on_event: impl FnMut(TailEvent),
) {
    // notify 이벤트를 TailLoopMsg::Event로 변환해 같은 채널로 전달.
    // 루프 자체도 송신자 하나를 잡아 두어 감시자가 죽어도 채널이 끊기지 않게 한다
    // (Stop 수신을 위해 항상 recv_timeout을 사용해야 하므로).
    let keepalive_tx = tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<NotifyEvent>| {
        if res.is_ok() {
            let _ = tx.send(TailLoopMsg::Event);
        }
    })
    .ok();

    // 감시자 생성/등록 실패 시에도 타임아웃 폴링으로 계속 동작한다
    if let Some(w) = watcher.as_mut()
        && let Err(e) = w.watch(watch_dir, RecursiveMode::NonRecursive)
    {
        on_event(TailEvent::Error {
            message: format!("디렉터리 감시 시작 실패 ('{}'): {e}", watch_dir.display()),
        });
        watcher = None;
    }
    // 감시자는 이 함수가 반환할 때까지 살아 있어야 한다
    let _watcher = watcher;
    let _keepalive_tx = keepalive_tx;

    // 시작 시 한 번 즉시 확인 (열기 이후 변경분 반영)
    if !refresh_once(&file, &filter_ctx, &mut on_event) {
        return;
    }
    let mut last_refresh = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // 디바운스 잔여 시간 또는 폴링 주기만큼 대기 (이벤트/Stop 수신 시 즉시 깸)
        let since = last_refresh.elapsed();
        let wait = if since >= DEBOUNCE {
            POLL_INTERVAL
        } else {
            DEBOUNCE - since
        };

        match rx.recv_timeout(wait) {
            Ok(TailLoopMsg::Event) | Err(RecvTimeoutError::Timeout) => {}
            Ok(TailLoopMsg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
        }

        if stop.load(Ordering::Relaxed) {
            break;
        }
        if last_refresh.elapsed() >= DEBOUNCE {
            if !refresh_once(&file, &filter_ctx, &mut on_event) {
                break; // 로테이션 감지 → 감시 종료
            }
            last_refresh = Instant::now();
        }
    }
}

/// 원격(sftp://) 파일 tail 폴링 루프.
/// notify는 원격 파일에 쓸 수 없으므로 1초 간격으로 refresh()를 폴링한다.
/// 연결 오류는 Error 이벤트로 전달하고 루프는 계속된다
/// (RandomRead 구현체가 다음 read에서 1회 재연결을 시도한다).
pub fn run_poll_tail_loop(
    file: Arc<LogFile>,
    stop: Arc<AtomicBool>,
    rx: std::sync::mpsc::Receiver<TailLoopMsg>,
    filter_ctx: TailFilterCtx,
    mut on_event: impl FnMut(TailEvent),
) {
    // 시작 시 한 번 즉시 확인 (열기 이후 변경분 반영)
    if !refresh_once(&file, &filter_ctx, &mut on_event) {
        return;
    }

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // Stop 수신 또는 1초 타임아웃까지 대기
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(TailLoopMsg::Event) | Err(RecvTimeoutError::Timeout) => {}
            Ok(TailLoopMsg::Stop) | Err(RecvTimeoutError::Disconnected) => break,
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if !refresh_once(&file, &filter_ctx, &mut on_event) {
            break; // 로테이션(크기 감소) 감지 → 감시 종료
        }
    }
}

/// 파일 경로의 부모 디렉터리를 반환한다. 부모가 없으면 현재 디렉터리.
fn watch_dir_of(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// tail 감시를 시작한다. 이미 감시 중이고 스레드가 살아 있으면 아무것도 하지 않는다.
/// 스레드 spawn부터 핸들 저장까지 files lock을 유지해 disable과의 경쟁을 막는다.
pub fn enable_tail<F>(state: &AppState, id: u32, on_event: F) -> Result<(), String>
where
    F: FnMut(TailEvent) + Send + 'static,
{
    let mut files = state.files.lock().unwrap_or_else(|p| p.into_inner());
    let open_file = files
        .get_mut(&id)
        .ok_or_else(|| format!("ID {id}에 해당하는 열린 파일을 찾을 수 없습니다."))?;
    // gzip 압축 파일은 임시 파일 해제본이라 원본 경로 감시가 의미 없으므로 tail 미지원
    if open_file.compressed {
        return Err("압축 파일은 tail 모드를 지원하지 않습니다".to_string());
    }
    // S3 객체는 불변이므로 추가분 감시(tail)를 지원하지 않는다
    if open_file
        .path
        .to_str()
        .is_some_and(|p| p.starts_with("s3://"))
    {
        return Err("S3 객체는 tail 을 지원하지 않습니다".to_string());
    }
    // 이미 감시 중이고 스레드가 살아 있으면 재사용
    if let Some(handle) = open_file.tail.as_ref()
        && !handle.thread.is_finished()
    {
        return Ok(());
    }
    let Some(file) = open_file.doc.as_file().cloned() else {
        return Err("병합 파일은 tail 모드를 지원하지 않습니다".to_string());
    };
    // 원격(sftp://) 파일은 notify 대신 1초 폴링으로 refresh한다
    let is_remote = open_file
        .path
        .to_str()
        .is_some_and(|p| p.starts_with("sftp://"));
    let watch_dir = watch_dir_of(&open_file.path);
    // Appended 시 활성 필터의 LineSet을 갱신하기 위해 필터 맵을 공유한다
    let filter_ctx = TailFilterCtx {
        filters: state.filters.clone(),
        file_id: id,
    };

    // Stop 메시지로 대기 중인 루프를 즉시 깨우기 위한 채널.
    // 송신자 하나는 watcher용(run_tail_loop 인자), 하나는 TailHandle이 보관한다.
    let (tx, rx) = std::sync::mpsc::channel::<TailLoopMsg>();
    let wake_tx = tx.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = if is_remote {
        // 폴링 루프는 tx를 쓰지 않으므로 원본 송신자는 여기서 버린다
        // (wake_tx가 채널을 열어 두므로 Stop 수신은 유지된다)
        std::thread::spawn(move || run_poll_tail_loop(file, thread_stop, rx, filter_ctx, on_event))
    } else {
        let thread_tx = tx;
        std::thread::spawn(move || {
            run_tail_loop(
                file,
                &watch_dir,
                thread_stop,
                thread_tx,
                rx,
                filter_ctx,
                on_event,
            );
        })
    };
    open_file.tail = Some(TailHandle {
        stop,
        wake: wake_tx,
        thread,
    });
    Ok(())
}

/// tail 감시를 중지하고 스레드를 정리한다.
pub fn disable_tail(state: &AppState, id: u32) {
    let handle = {
        let mut files = state.files.lock().unwrap_or_else(|p| p.into_inner());
        files.get_mut(&id).and_then(|f| f.tail.take())
    };
    if let Some(handle) = handle {
        handle.stop.store(true, Ordering::Relaxed);
        // Stop 메시지로 루프를 즉시 깨운 뒤 join하므로 최대 1초 블로킹이 없다
        let _ = handle.wake.send(TailLoopMsg::Stop);
        let _ = handle.thread.join();
    }
}

/// tail 모드(실시간 추적) 감시를 켜거나 끈다.
#[tauri::command]
pub async fn set_tail(
    state: tauri::State<'_, AppState>,
    id: u32,
    enabled: bool,
    on_event: Channel<TailEvent>,
) -> Result<(), String> {
    if enabled {
        enable_tail(&state, id, move |event| {
            let _ = on_event.send(event);
        })
    } else {
        disable_tail(&state, id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::open_file_internal;
    use std::collections::HashMap;
    use std::io::Write;
    use std::sync::Mutex;

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn create(content: &[u8]) -> Self {
            let path = crate::testutil::temp_log_path("tail_test");
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

    /// 파일을 열고 인덱싱을 완료한다 (refresh가 Pending을 반환하지 않도록).
    fn open_indexed(path: &Path) -> Arc<LogFile> {
        let file = Arc::new(LogFile::open(path).expect("파일 열기 실패"));
        let cancel = AtomicBool::new(false);
        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");
        file
    }

    /// 조건에 맞는 이벤트가 도착할 때까지 기다린다.
    fn wait_for_event(
        events: &Mutex<Vec<TailEvent>>,
        timeout: Duration,
        pred: impl Fn(&TailEvent) -> bool,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if events.lock().unwrap().iter().any(&pred) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    fn append_line(path: &Path, line: &str) {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .expect("append 열기 실패");
        writeln!(file, "{line}").expect("append 쓰기 실패");
        file.sync_all().expect("append 플러시 실패");
    }

    #[test]
    fn test_tail_loop_appended() {
        let temp = TempFile::create(b"line1\n");
        let file = open_indexed(&temp.path);
        let watch_dir = temp.path.parent().unwrap().to_path_buf();

        let events = Arc::new(Mutex::new(Vec::new()));
        let events2 = events.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop_tx = tx.clone();
        let handle = std::thread::spawn(move || {
            run_tail_loop(
                file,
                &watch_dir,
                stop2,
                tx,
                rx,
                TailFilterCtx {
                    filters: Arc::new(Mutex::new(HashMap::new())),
                    file_id: 1,
                },
                move |e| {
                    events2.lock().unwrap().push(e);
                },
            );
        });

        append_line(&temp.path, "line2");

        let found = wait_for_event(&events, Duration::from_secs(5), |e| {
            matches!(e, TailEvent::Appended { .. })
        });
        assert!(found, "Appended 이벤트가 도착해야 함");

        let events = events.lock().unwrap();
        let appended = events.iter().find_map(|e| match e {
            TailEvent::Appended {
                line_count,
                old_line_count,
                ..
            } => Some((*old_line_count, *line_count)),
            _ => None,
        });
        assert_eq!(appended, Some((1, 2)));
        drop(events);

        stop.store(true, Ordering::Relaxed);
        let _ = stop_tx.send(TailLoopMsg::Stop);
        handle.join().expect("tail 스레드 join 실패");
    }

    #[test]
    fn test_tail_loop_rotated_replaced() {
        let temp = TempFile::create(b"line1\n");
        let file = open_indexed(&temp.path);
        let watch_dir = temp.path.parent().unwrap().to_path_buf();

        let events = Arc::new(Mutex::new(Vec::new()));
        let events2 = events.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop_tx = tx.clone();
        let handle = std::thread::spawn(move || {
            run_tail_loop(
                file,
                &watch_dir,
                stop2,
                tx,
                rx,
                TailFilterCtx {
                    filters: Arc::new(Mutex::new(HashMap::new())),
                    file_id: 1,
                },
                move |e| {
                    events2.lock().unwrap().push(e);
                },
            );
        });

        // 파일을 다른 이름으로 rename → 경로의 파일이 사라짐 (Replaced)
        let moved = temp.path.with_extension("moved");
        std::fs::rename(&temp.path, &moved).expect("rename 실패");

        let found = wait_for_event(&events, Duration::from_secs(5), |e| {
            matches!(e, TailEvent::Rotated { .. })
        });
        assert!(found, "Rotated 이벤트가 도착해야 함");

        let events = events.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            TailEvent::Rotated { reason } if reason == "replaced"
        )));
        drop(events);

        // 로테이션 감지 후 루프는 스스로 종료한다
        let _ = stop_tx.send(TailLoopMsg::Stop);
        handle.join().expect("tail 스레드 join 실패");
        let _ = std::fs::remove_file(&moved);
    }

    #[test]
    fn test_tail_loop_rotated_truncated() {
        let temp = TempFile::create(b"line1\nline2\n");
        let file = open_indexed(&temp.path);
        let watch_dir = temp.path.parent().unwrap().to_path_buf();

        let events = Arc::new(Mutex::new(Vec::new()));
        let events2 = events.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let stop_tx = tx.clone();
        let handle = std::thread::spawn(move || {
            run_tail_loop(
                file,
                &watch_dir,
                stop2,
                tx,
                rx,
                TailFilterCtx {
                    filters: Arc::new(Mutex::new(HashMap::new())),
                    file_id: 1,
                },
                move |e| {
                    events2.lock().unwrap().push(e);
                },
            );
        });

        // 같은 inode에서 크기를 줄임 (Truncated)
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&temp.path)
            .expect("truncate 열기 실패");
        f.set_len(4).expect("truncate 실패"); // "line" 만 남김

        let found = wait_for_event(&events, Duration::from_secs(5), |e| {
            matches!(e, TailEvent::Rotated { .. })
        });
        assert!(found, "Rotated 이벤트가 도착해야 함");

        let events = events.lock().unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            TailEvent::Rotated { reason } if reason == "truncated"
        )));
        drop(events);

        let _ = stop_tx.send(TailLoopMsg::Stop);
        handle.join().expect("tail 스레드 join 실패");
    }

    #[test]
    fn test_enable_tail_single_watcher_and_disable() {
        let state = AppState::default();
        let temp = TempFile::create(b"a\nb\n");
        let opened = open_file_internal(&state, temp.path_str()).expect("파일 열기 실패");
        let info = opened.info;
        let file = opened.file;
        let cancel = AtomicBool::new(false);
        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");

        enable_tail(&state, info.id, |_| {}).expect("tail 활성화 실패");
        let tid1 = {
            let files = state.files.lock().unwrap();
            files
                .get(&info.id)
                .and_then(|f| f.tail.as_ref())
                .map(|t| t.thread.thread().id())
        };
        assert!(tid1.is_some());

        // 두 번 켜도 감시는 하나
        enable_tail(&state, info.id, |_| {}).expect("tail 재활성화 실패");
        let tid2 = {
            let files = state.files.lock().unwrap();
            files
                .get(&info.id)
                .and_then(|f| f.tail.as_ref())
                .map(|t| t.thread.thread().id())
        };
        assert_eq!(tid1, tid2);

        // 끄면 핸들이 정리된다
        disable_tail(&state, info.id);
        let files = state.files.lock().unwrap();
        assert!(files.get(&info.id).unwrap().tail.is_none());
        drop(files);

        state.close_file(info.id);
    }

    #[test]
    fn test_close_file_stops_tail() {
        let state = AppState::default();
        let temp = TempFile::create(b"a\n");
        let opened = open_file_internal(&state, temp.path_str()).expect("파일 열기 실패");
        let info = opened.info;
        let file = opened.file;
        let cancel = AtomicBool::new(false);
        file.build_index(&cancel, &mut |_| {}).expect("인덱싱 실패");

        let events = Arc::new(Mutex::new(Vec::new()));
        let events2 = events.clone();
        enable_tail(&state, info.id, move |e| {
            events2.lock().unwrap().push(e);
        })
        .expect("tail 활성화 실패");

        state.close_file(info.id);

        // close_file은 감시 스레드를 join하므로 이후 append는 이벤트를 만들지 않는다
        append_line(&temp.path, "b");
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .all(|e| !matches!(e, TailEvent::Appended { .. }))
        );
    }

    #[test]
    fn test_tail_loop_updates_active_filter() {
        use crate::commands::{FilterRuleDto, FilterSpecDto};
        use riffle_core::FilterSet;

        let state = AppState::default();
        let temp = TempFile::create(b"INFO ok\nERROR bad\n");
        let opened = open_file_internal(&state, temp.path_str()).expect("파일 열기 실패");
        let info = opened.info;
        let file = opened.file;
        file.build_index(&AtomicBool::new(false), &mut |_| {})
            .expect("인덱싱 실패");

        // include ERROR 필터를 등록하고 스캔 완료 상태를 흉내낸다 (줄 1만 보임)
        let spec = FilterSpecDto {
            rules: vec![FilterRuleDto {
                pattern: "ERROR".to_string(),
                regex: false,
                case_sensitive: true,
                kind: "include".to_string(),
                enabled: true,
                color: "#ff0000".to_string(),
                field: None,
            }],
            combine: "or".to_string(),
            levels: Vec::new(),
        };
        let core = crate::commands::filter_spec_of(&spec).expect("spec 변환 실패");
        let set = Arc::new(FilterSet::new(&core, None).expect("FilterSet 생성 실패"));
        let filter = state
            .insert_filter(info.id, spec, set)
            .expect("필터 등록 실패");
        filter.lines.write().unwrap().push_line(1);

        let events = Arc::new(Mutex::new(Vec::new()));
        let events2 = events.clone();
        enable_tail(&state, info.id, move |e| {
            events2.lock().unwrap().push(e);
        })
        .expect("tail 활성화 실패");

        append_line(&temp.path, "ERROR new");

        // Appended 이벤트의 filter_count가 갱신된 필터 줄 수여야 한다
        let found = wait_for_event(&events, Duration::from_secs(5), |e| {
            matches!(
                e,
                TailEvent::Appended {
                    filter_count: Some(_),
                    ..
                }
            )
        });
        assert!(found, "filter_count가 있는 Appended 이벤트가 도착해야 함");

        let events = events.lock().unwrap();
        let appended = events.iter().find_map(|e| match e {
            TailEvent::Appended { filter_count, .. } => *filter_count,
            _ => None,
        });
        assert_eq!(appended, Some(2)); // 줄 1, 2 둘 다 ERROR
        drop(events);

        // 필터의 LineSet에도 반영됐는지 확인
        let lines = filter.lines.read().unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines.contains(1));
        assert!(lines.contains(2));
        drop(lines);

        state.close_file(info.id);
    }

    #[test]
    fn test_enable_tail_unknown_file() {
        let state = AppState::default();
        let result = enable_tail(&state, 9999, |_| {});
        assert!(result.is_err());
    }

    #[test]
    fn test_enable_tail_rejects_gzip_file() {
        let state = AppState::default();
        let gz_path = crate::testutil::create_gzip_file("tail_gz", b"line1\nline2\n");
        let opened =
            open_file_internal(&state, gz_path.to_str().unwrap()).expect("gzip 파일 열기 실패");
        assert!(opened.info.compressed);
        assert!(opened.gz_writer.is_some());
        let info = opened.info;

        // gzip 파일은 tail 모드를 지원하지 않으므로 Err를 반환해야 한다
        let result = enable_tail(&state, info.id, |_| {});
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("tail 모드를 지원하지 않습니다")
        );

        state.close_file(info.id);
        let _ = std::fs::remove_file(&gz_path);
    }
}
