use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use rayon::prelude::*;

use crate::Result;
use crate::source::Source;

/// 체크포인트 간격 (줄 수 단위, K)
pub const CHECKPOINT_INTERVAL: u64 = 1024;

/// 기본 인덱싱 구간 크기 (16MB)
pub(crate) const DEFAULT_SEGMENT_SIZE: usize = 16 * 1024 * 1024;

/// 첫 번째 구간 크기 (2MB - 첫 화면 20줄 읽기 지연 단축용)
pub(crate) const FIRST_SEGMENT_SIZE: usize = 2 * 1024 * 1024;

/// 인덱싱 진행 상태 정보
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexProgress {
    /// 현재까지 인덱싱 완료된 바이트 수
    pub indexed_bytes: u64,
    /// 파일 전체 바이트 크기
    pub total_bytes: u64,
    /// 현재까지 확인된 줄 수
    pub line_count: u64,
    /// 인덱싱 완료 여부
    pub done: bool,
}

/// 희소 줄 인덱스 (Sparse Line Index).
/// K줄마다 시작 바이트 오프셋을 기록하여 메모리를 절약하며,
/// 점진적으로 인덱스를 공개하여 인덱싱 중에도 앞부분 줄을 즉시 읽을 수 있다.
/// 인덱싱 완료 후에는 extend()로 파일 끝에 추가된 부분만 반영할 수 있다 (tail 모드).
pub struct LineIndex {
    total_bytes: AtomicU64,
    checkpoints: RwLock<Vec<u64>>,
    line_count: AtomicU64,
    indexed_bytes: AtomicU64,
    done: AtomicBool,
    segment_size: AtomicUsize,
}

impl LineIndex {
    /// 주어진 전체 바이트 크기에 대한 새로운 LineIndex를 생성한다.
    pub fn new(total_bytes: u64) -> LineIndex {
        // checkpoints[0]은 항상 줄 0의 시작 오프셋(0). 비어 있는 파일이어도
        // 이후 extend() 시 체크포인트 인덱스 정렬을 위해 유지한다.
        LineIndex {
            total_bytes: AtomicU64::new(total_bytes),
            checkpoints: RwLock::new(vec![0]),
            line_count: AtomicU64::new(0),
            indexed_bytes: AtomicU64::new(0),
            done: AtomicBool::new(total_bytes == 0),
            segment_size: AtomicUsize::new(DEFAULT_SEGMENT_SIZE),
        }
    }

    /// 테스트용 구간(segment) 크기 설정 함수
    #[allow(dead_code)]
    pub(crate) fn set_segment_size(&self, size: usize) {
        self.segment_size.store(size, Ordering::Relaxed);
    }

    /// 외부(원격 에이전트)에서 만든 체크포인트로 완료 상태의 인덱스를 생성한다.
    /// checkpoints[i] 는 i*K 번째 줄의 시작 오프셋이어야 한다.
    /// 첫 값이 0이 아니거나 비어 있으면 0을 앞에 보정한다 (checkpoints[0] 불변 조건 유지).
    pub fn from_checkpoints(total_bytes: u64, line_count: u64, checkpoints: Vec<u64>) -> LineIndex {
        let mut cps = checkpoints;
        if cps.first() != Some(&0) {
            cps.insert(0, 0);
        }
        LineIndex {
            total_bytes: AtomicU64::new(total_bytes),
            checkpoints: RwLock::new(cps),
            line_count: AtomicU64::new(line_count),
            indexed_bytes: AtomicU64::new(total_bytes),
            done: AtomicBool::new(true),
            segment_size: AtomicUsize::new(DEFAULT_SEGMENT_SIZE),
        }
    }

    /// 체크포인트 목록의 복사본을 반환한다 (에이전트의 index 응답 직렬화용).
    pub fn checkpoints(&self) -> Vec<u64> {
        self.checkpoints.read().unwrap().clone()
    }

    /// 다른 인덱스의 완성 상태를 이 인덱스로 옮겨 온다 (LogFile::adopt_index 전용).
    /// done 을 마지막에 세워 읽는 쪽이 항상 완성된 상태만 보게 한다.
    pub(crate) fn adopt(&self, src: LineIndex) {
        let cps = src.checkpoints.into_inner().unwrap();
        *self.checkpoints.write().unwrap() = cps;
        self.line_count
            .store(src.line_count.load(Ordering::Acquire), Ordering::Release);
        self.indexed_bytes
            .store(src.indexed_bytes.load(Ordering::Acquire), Ordering::Release);
        self.total_bytes
            .store(src.total_bytes.load(Ordering::Acquire), Ordering::Release);
        self.done
            .store(src.done.load(Ordering::Acquire), Ordering::Release);
    }

    /// 블로킹 방식으로 인덱스를 생성한다. 호출자가 별도 스레드에서 실행해야 한다.
    /// cancel 플래그가 true가 되면 즉시 Ok(())로 중단하며, 진행 상태는 done=false로 유지된다.
    /// I/O 읽기와 개행 스캔을 더블 버퍼링(2개 재사용 버퍼)으로 파이프라이닝하여 디스크 한계 속도를 달성한다.
    pub fn build(
        &self,
        source: &Source,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(IndexProgress),
    ) -> Result<()> {
        let total_bytes = self.total_bytes.load(Ordering::Acquire);
        if total_bytes == 0 {
            self.done.store(true, Ordering::Release);
            on_progress(self.progress());
            return Ok(());
        }

        let normal_segment_size = self.segment_size.load(Ordering::Relaxed);
        let first_segment_size = FIRST_SEGMENT_SIZE.min(normal_segment_size);

        // 재사용 버퍼 채널 (더블 버퍼링: 2개 버퍼로 I/O와 스캔 파이프라이닝)
        let (free_tx, free_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(2);
        let (data_tx, data_rx) = std::sync::mpsc::sync_channel::<Result<(u64, Vec<u8>, usize)>>(2);

        let _ = free_tx.send(vec![0u8; normal_segment_size]);
        let _ = free_tx.send(vec![0u8; normal_segment_size]);

        let mut current_line_count: u64 = 0;
        let mut new_checkpoints: Vec<u64> = Vec::new();
        let mut build_error: Option<crate::Error> = None;

        std::thread::scope(|s| {
            // 백그라운드 I/O 스레드: 디스크에서 순차 read_at으로 읽어 data_tx로 전송
            s.spawn(move || {
                let mut offset: u64 = 0;
                let mut is_first = true;
                while offset < total_bytes {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }

                    let chunk_size = if is_first {
                        first_segment_size
                    } else {
                        normal_segment_size
                    };
                    let to_read = (chunk_size as u64).min(total_bytes - offset) as usize;

                    let mut buf = match free_rx.recv() {
                        Ok(b) => b,
                        Err(_) => break, // 메인 루프 종료 시 수신 중단
                    };

                    if buf.len() < to_read {
                        buf.resize(to_read, 0);
                    }

                    match source.read_into(&mut buf[..to_read], offset) {
                        Ok(n) => {
                            if n == 0 {
                                break;
                            }
                            if data_tx.send(Ok((offset, buf, n))).is_err() {
                                break;
                            }
                            offset += n as u64;
                            is_first = false;
                        }
                        Err(e) => {
                            let _ = data_tx.send(Err(e));
                            break;
                        }
                    }
                }
            });

            // 메인 스레드: data_rx로부터 버퍼를 받아 Rayon 병렬 개행 위치 수집
            while let Ok(msg) = data_rx.recv() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }

                let (segment_start, buf, valid_len) = match msg {
                    Ok(data) => data,
                    Err(e) => {
                        build_error = Some(e);
                        break;
                    }
                };

                let segment_bytes = &buf[..valid_len];
                let segment_end = segment_start + valid_len as u64;

                // 서브청크 분할 (rayon 병렬 처리)
                let num_threads = rayon::current_num_threads();
                let chunk_count = if num_threads <= 1 || segment_bytes.len() < 32 * 1024 {
                    1
                } else {
                    num_threads.min((segment_bytes.len() / (16 * 1024)).max(1))
                };

                let chunk_len = segment_bytes.len().div_ceil(chunk_count);
                let mut chunk_slices = Vec::with_capacity(chunk_count);
                let mut offset_in_segment = 0;

                while offset_in_segment < segment_bytes.len() {
                    let this_len = chunk_len.min(segment_bytes.len() - offset_in_segment);
                    chunk_slices.push((
                        offset_in_segment,
                        &segment_bytes[offset_in_segment..offset_in_segment + this_len],
                    ));
                    offset_in_segment += this_len;
                }

                // 병렬로 각 서브청크 내 개행 위치 수집
                let subchunks_newlines: Vec<(usize, Vec<u32>)> = chunk_slices
                    .into_par_iter()
                    .map(|(slice_start, slice)| {
                        let nls: Vec<u32> = memchr::memchr_iter(b'\n', slice)
                            .map(|p| p as u32)
                            .collect();
                        (slice_start, nls)
                    })
                    .collect();

                // 순차 병합하여 전역 줄 번호 계산 및 새 체크포인트 기록
                for (slice_start, nls) in subchunks_newlines {
                    let base = segment_start + slice_start as u64;
                    for rel_pos in nls {
                        let global_newline_offset = base + rel_pos as u64;
                        current_line_count += 1;
                        if current_line_count.is_multiple_of(CHECKPOINT_INTERVAL) {
                            new_checkpoints.push(global_newline_offset + 1);
                        }
                    }
                }

                // 새 체크포인트를 점진적으로 공개
                if !new_checkpoints.is_empty() {
                    self.checkpoints
                        .write()
                        .unwrap()
                        .extend_from_slice(&new_checkpoints);
                    new_checkpoints.clear();
                }

                // 원자적으로 진행 상태 갱신
                self.line_count.store(current_line_count, Ordering::Release);
                self.indexed_bytes.store(segment_end, Ordering::Release);

                on_progress(self.progress());

                // 버퍼를 I/O 스레드로 반환 (재사용)
                let _ = free_tx.send(buf);

                if cancel.load(Ordering::Relaxed) {
                    break;
                }
            }

            drop(data_rx);
            drop(free_tx);
        });

        if let Some(err) = build_error {
            return Err(err);
        }

        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }

        // 파일의 마지막 줄 처리: 마지막 바이트가 '\n'이 아니면 마지막 조각도 한 줄로 인정
        // (외부 truncate 등으로 마지막 바이트를 읽지 못하면 마지막 조각으로 세지 않는다)
        if total_bytes > 0 {
            let last_byte = source.read_range(total_bytes - 1, total_bytes)?;
            if last_byte.first().is_some_and(|&b| b != b'\n') {
                current_line_count += 1;
                self.line_count.store(current_line_count, Ordering::Release);
            }
        }

        self.done.store(true, Ordering::Release);
        on_progress(self.progress());

        Ok(())
    }

    /// 완료된 인덱스에 [이전 total_bytes, new_len) 구간의 추가분만 반영한다 (tail 모드용).
    ///
    /// 이전 인덱스의 마지막 줄이 개행 없이 끝나 있었다면 그 줄은 추가분과 이어지는 것으로
    /// 처리하여 같은 줄을 두 번 세지 않는다. 추가분이 매우 커도 되도록 4MB 단위로 끊어 읽는다.
    /// 실제로 읽힌 만큼만 반영하며(total_bytes를 실제 커버 끝으로 설정), 읽기 도중 파일이
    /// 외부에서 잘린 경우 다음 refresh 호출이 나머지를 이어서 처리할 수 있다.
    pub(crate) fn extend(&self, source: &Source, new_len: u64) -> Result<()> {
        /// 추가분 스캔 시 한 번에 읽는 크기 (4MB)
        const EXTEND_CHUNK_SIZE: usize = 4 * 1024 * 1024;

        let old_len = self.total_bytes.load(Ordering::Acquire);
        if new_len <= old_len {
            return Ok(());
        }

        // 이전 인덱스가 개행 없는 마지막 조각을 1줄로 포함하고 있었다면(p_old=1),
        // 그 줄은 추가분과 이어지므로 새 줄 수 계산에서 한 번 차감한다.
        let p_old = if old_len == 0 {
            0
        } else {
            let last = source.read_range(old_len - 1, old_len)?;
            u64::from(last.first() != Some(&b'\n'))
        };

        // 개행으로 종료된 줄 수 = 다음에 시작하는 줄 번호
        let mut completed_lines = self.line_count() - p_old;
        let mut new_checkpoints: Vec<u64> = Vec::new();
        let mut buf = vec![0u8; EXTEND_CHUNK_SIZE];
        let mut offset = old_len;

        while offset < new_len {
            let want = (new_len - offset).min(buf.len() as u64) as usize;
            let n = source.read_at_raw(&mut buf[..want], offset)?;
            if n == 0 {
                break; // 읽는 도중 파일이 잘린 경우: 읽힌 만큼만 반영
            }
            for pos in memchr::memchr_iter(b'\n', &buf[..n]) {
                completed_lines += 1;
                if completed_lines.is_multiple_of(CHECKPOINT_INTERVAL) {
                    new_checkpoints.push(offset + pos as u64 + 1);
                }
            }
            offset += n as u64;
        }

        let covered_end = offset;
        if covered_end <= old_len {
            return Ok(());
        }

        // 추가 영역의 마지막 바이트가 개행이 아니면 미완성 마지막 조각도 1줄로 인정
        let mut last = [0u8; 1];
        let p_new =
            u64::from(source.read_at_raw(&mut last, covered_end - 1)? == 1 && last[0] != b'\n');
        let new_line_count = completed_lines + p_new;

        // 체크포인트는 줄 번호 오름차순으로만 추가되므로 기존 벡터 뒤에 이어 붙이면 정렬이 유지된다
        if !new_checkpoints.is_empty() {
            self.checkpoints
                .write()
                .unwrap()
                .extend_from_slice(&new_checkpoints);
        }
        self.line_count.store(new_line_count, Ordering::Release);
        self.indexed_bytes.store(covered_end, Ordering::Release);
        self.total_bytes.store(covered_end, Ordering::Release);

        Ok(())
    }

    /// 현재 인덱싱 진행 상태를 반환한다. 인덱싱 중에도 다른 스레드에서 안전하게 호출 가능하다.
    pub fn progress(&self) -> IndexProgress {
        IndexProgress {
            indexed_bytes: self.indexed_bytes.load(Ordering::Acquire),
            total_bytes: self.total_bytes.load(Ordering::Acquire),
            line_count: self.line_count.load(Ordering::Acquire),
            done: self.done.load(Ordering::Acquire),
        }
    }

    /// 현재까지 확인된 줄 수를 반환한다.
    #[inline]
    pub fn line_count(&self) -> u64 {
        self.line_count.load(Ordering::Acquire)
    }

    /// N번째 줄(0-based)의 시작 바이트 오프셋을 반환한다.
    /// 아직 확인되지 않은 줄이면 None을 반환한다.
    pub fn line_start(&self, source: &Source, line_no: u64) -> Option<u64> {
        if line_no >= self.line_count() {
            return None;
        }

        let cp_idx = (line_no / CHECKPOINT_INTERVAL) as usize;
        let start_offset = {
            let cps = self.checkpoints.read().unwrap();
            if cp_idx >= cps.len() {
                return None;
            }
            cps[cp_idx]
        };

        let remaining_lines = (line_no % CHECKPOINT_INTERVAL) as usize;
        if remaining_lines == 0 {
            return Some(start_offset);
        }

        // 체크포인트 이후 remaining_lines번째(1-based) 개행을 찾아 그 다음 위치를 반환한다.
        // I/O 오류나 개행 부족(외부 truncate) 시 None.
        source
            .nth_newline(start_offset, remaining_lines - 1)
            .ok()
            .flatten()
            .map(|nl_pos| nl_pos + 1)
    }

    /// 주어진 바이트 오프셋이 속한 줄 번호(0-based)를 반환한다.
    /// 아직 인덱싱되지 않은 영역이거나 범위를 벗어난 경우 None을 반환한다.
    pub fn line_of_offset(&self, source: &Source, offset: u64) -> Option<u64> {
        if offset >= source.len() {
            return None;
        }

        let indexed = self.indexed_bytes.load(Ordering::Acquire);
        let is_done = self.done.load(Ordering::Acquire);

        // 아직 인덱싱되지 않은 오프셋이면 None
        if !is_done && offset >= indexed {
            return None;
        }

        let (cp_idx, start_offset) = {
            let cps = self.checkpoints.read().unwrap();
            if cps.is_empty() {
                return None;
            }
            match cps.binary_search(&offset) {
                Ok(idx) => (idx, cps[idx]),
                Err(idx) => {
                    if idx == 0 {
                        return None;
                    }
                    (idx - 1, cps[idx - 1])
                }
            }
        };

        let start_line = (cp_idx as u64) * CHECKPOINT_INTERVAL;
        if start_offset == offset {
            if start_line < self.line_count() {
                return Some(start_line);
            } else {
                return None;
            }
        }

        let count = source.count_newlines(start_offset, offset).ok()?;
        let line_no = start_line + count;

        if line_no < self.line_count() {
            Some(line_no)
        } else {
            None
        }
    }
}
