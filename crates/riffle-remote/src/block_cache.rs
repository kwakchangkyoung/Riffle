//! 고정 크기 바이트 블록의 단순 LRU 캐시.
//!
//! 키는 블록 시작 오프셋, 값은 블록 시작부터의 실제 바이트다
//! (EOF 에 걸린 블록은 블록 크기보다 짧을 수 있다). SFTP·S3 구현체가 공유한다.

use std::collections::{HashMap, VecDeque};

pub(crate) struct BlockCache {
    map: HashMap<u64, Vec<u8>>,
    /// 앞이 가장 오래된 LRU 순서.
    order: VecDeque<u64>,
    bytes: usize,
    max_bytes: usize,
}

impl BlockCache {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            max_bytes,
        }
    }

    /// 블록을 조회한다. 히트면 LRU 순서를 최신으로 갱신한다.
    pub(crate) fn get(&mut self, bs: u64) -> Option<&Vec<u8>> {
        if self.map.contains_key(&bs) {
            self.touch(bs);
            return self.map.get(&bs);
        }
        None
    }

    pub(crate) fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
    }

    fn touch(&mut self, bs: u64) {
        if let Some(pos) = self.order.iter().position(|&k| k == bs) {
            self.order.remove(pos);
        }
        self.order.push_back(bs);
    }

    pub(crate) fn insert(&mut self, bs: u64, data: Vec<u8>) {
        self.remove_key(bs);
        self.bytes += data.len();
        self.map.insert(bs, data);
        self.order.push_back(bs);
        while self.bytes > self.max_bytes {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.remove_key(oldest);
        }
    }

    fn remove_key(&mut self, bs: u64) {
        if let Some(d) = self.map.remove(&bs) {
            self.bytes = self.bytes.saturating_sub(d.len());
        }
        self.order.retain(|&k| k != bs);
    }

    pub(crate) fn invalidate(&mut self, keys: impl IntoIterator<Item = u64>) {
        for k in keys {
            self.remove_key(k);
        }
    }
}
