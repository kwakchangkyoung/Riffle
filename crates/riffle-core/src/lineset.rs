//! 정렬·서로소인 반열린 구간 [start, end) 목록 + 누적 개수.
//! 필터 결과(보이는 줄 번호 집합)를 압축해서 보관하기 위해 사용한다.

/// 필터 결과 구간 수 상한. 넘으면 결과가 "너무 많음"(capped) 상태가 된다.
pub const MAX_FILTER_RANGES: usize = 10_000_000;

/// 보이는 줄 번호 집합. ranges는 오름차순·서로소·비인접이 아닌
/// (인접 구간은 항상 하나로 합쳐진 상태를 유지) 반열린 구간 목록이다.
/// prefix[i]는 ranges[..i]에 포함된 원소의 총합으로, prefix.len() == ranges.len() + 1.
#[derive(Debug, Clone, Default)]
pub struct LineSet {
    ranges: Vec<(u64, u64)>,
    prefix: Vec<u64>,
}

impl LineSet {
    /// 빈 집합을 생성한다.
    pub fn new() -> Self {
        Self {
            ranges: Vec::new(),
            prefix: vec![0],
        }
    }

    /// 집합에 포함된 줄 수를 반환한다.
    #[inline]
    pub fn len(&self) -> u64 {
        *self.prefix.last().unwrap_or(&0)
    }

    /// 집합이 비어 있는지 반환한다.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// 저장된 구간 수를 반환한다 (메모리 상한 판정용).
    #[inline]
    pub fn range_count(&self) -> usize {
        self.ranges.len()
    }

    /// 내부 구간 목록 참조 (밀도 계산 등 외부 순회용).
    #[inline]
    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    /// index 번째(0-based) 원소의 줄 번호를 반환한다. 범위 밖이면 None.
    pub fn nth(&self, index: u64) -> Option<u64> {
        if index >= self.len() {
            return None;
        }
        // prefix[i] <= index < prefix[i+1] 인 i를 찾는다
        let i = self.prefix.partition_point(|&p| p <= index) - 1;
        let (start, _) = self.ranges[i];
        Some(start + (index - self.prefix[i]))
    }

    /// line 미만인 원소의 수를 반환한다
    /// (= line 이상인 첫 원소의 index. 보던 위치 유지용).
    pub fn rank(&self, line: u64) -> u64 {
        // end <= line 인 범위는 원소가 모두 line 미만이다
        let idx = self.ranges.partition_point(|&(_, e)| e <= line);
        let mut rank = self.prefix[idx];
        if idx < self.ranges.len() {
            let (s, _) = self.ranges[idx];
            if line > s {
                // 이 범위에서 line 미만인 원소 수 (line < end 임이 보장됨)
                rank += line - s;
            }
        }
        rank
    }

    /// line이 집합에 포함되는지 반환한다.
    pub fn contains(&self, line: u64) -> bool {
        let idx = self.ranges.partition_point(|&(_, e)| e <= line);
        idx < self.ranges.len() && self.ranges[idx].0 <= line
    }

    /// 줄 번호를 추가한다. 이미 포함되어 있으면 아무것도 하지 않는다.
    /// 오름차순 추가가 빠른 경로이며, 순서가 어긋난 추가도 올바르게 처리한다.
    pub fn push_line(&mut self, line: u64) {
        match self.ranges.last() {
            None => {
                self.ranges.push((line, line + 1));
                self.prefix.push(1);
            }
            Some(&(s, e)) => {
                if line < s {
                    // 마지막 구간보다 앞에 삽입해야 하는 드문 경우
                    self.insert_slow(line);
                } else if line < e {
                    // 이미 포함
                } else if line == e {
                    // 마지막 구간과 인접 → 확장
                    self.ranges.last_mut().unwrap().1 = e + 1;
                    *self.prefix.last_mut().unwrap() += 1;
                } else {
                    // 마지막 구간과 떨어진 새 원소
                    self.ranges.push((line, line + 1));
                    let total = *self.prefix.last().unwrap() + 1;
                    self.prefix.push(total);
                }
            }
        }
    }

    /// 마지막 구간 앞쪽에 위치하는 원소를 삽입한다 (드문 경로).
    /// 인접한 기존 구간이 있으면 병합한다.
    fn insert_slow(&mut self, line: u64) {
        // end <= line 인 마지막 범위의 다음 위치가 삽입 위치
        let idx = self.ranges.partition_point(|&(_, e)| e <= line);
        if idx < self.ranges.len() {
            let (s, e) = self.ranges[idx];
            if s <= line && line < e {
                return; // 이미 포함
            }
        }

        // 왼쪽 병합: 이전 구간의 끝이 line과 같으면 흡수
        let (mut start, mut left) = (line, idx);
        if idx > 0 && self.ranges[idx - 1].1 == line {
            start = self.ranges[idx - 1].0;
            left = idx - 1;
        }
        // 오른쪽 병합: 다음 구간의 시작이 line+1과 같으면 흡수
        let (mut end, mut right) = (line + 1, idx);
        if idx < self.ranges.len() && self.ranges[idx].0 == line + 1 {
            end = self.ranges[idx].1;
            right = idx + 1;
        }

        // ranges[left..right]를 (start, end) 하나로 교체 (없으면 삽입)
        if left == idx && right == idx {
            self.ranges.insert(idx, (start, end));
        } else {
            self.ranges.splice(left..right, [(start, end)]);
        }
        self.rebuild_prefix();
    }

    /// line을 집합에서 제거한다. 포함되어 있지 않으면 아무것도 하지 않는다.
    /// tail 재판정 시 이전 판정 결과를 되돌리는 용도.
    pub fn remove_line(&mut self, line: u64) {
        let idx = self.ranges.partition_point(|&(_, e)| e <= line);
        if idx >= self.ranges.len() {
            return;
        }
        let (s, e) = self.ranges[idx];
        if !(s <= line && line < e) {
            return;
        }
        if s == line && e == line + 1 {
            self.ranges.remove(idx);
        } else if s == line {
            self.ranges[idx].0 = s + 1;
        } else if e == line + 1 {
            self.ranges[idx].1 = e - 1;
        } else {
            // 구간 한가운데 → 둘로 분할
            self.ranges[idx] = (s, line);
            self.ranges.insert(idx + 1, (line + 1, e));
        }
        self.rebuild_prefix();
    }

    /// prefix 누적합을 다시 계산한다 (구간 변경 후 호출).
    fn rebuild_prefix(&mut self) {
        self.prefix.clear();
        self.prefix.push(0);
        let mut acc = 0u64;
        for &(s, e) in &self.ranges {
            acc += e - s;
            self.prefix.push(acc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_of(lines: &[u64]) -> LineSet {
        let mut s = LineSet::new();
        for &l in lines {
            s.push_line(l);
        }
        s
    }

    #[test]
    fn test_empty_set() {
        let s = LineSet::new();
        assert_eq!(s.len(), 0);
        assert!(s.is_empty());
        assert_eq!(s.range_count(), 0);
        assert_eq!(s.nth(0), None);
        assert_eq!(s.rank(0), 0);
        assert_eq!(s.rank(100), 0);
        assert!(!s.contains(0));
    }

    #[test]
    fn test_adjacent_merge() {
        let mut s = LineSet::new();
        s.push_line(0);
        s.push_line(2);
        s.push_line(1); // 0,1,2 연속 → [0,3)
        s.push_line(4);
        s.push_line(3); // 3,4 → [0,5)
        assert_eq!(s.ranges(), &[(0, 5)]);
        assert_eq!(s.len(), 5);
        assert_eq!(s.range_count(), 1);
        for i in 0..5 {
            assert!(s.contains(i));
            assert_eq!(s.nth(i), Some(i));
        }
    }

    #[test]
    fn test_disjoint_ranges() {
        let s = set_of(&[0, 1, 5, 6, 7, 20]);
        assert_eq!(s.ranges(), &[(0, 2), (5, 8), (20, 21)]);
        assert_eq!(s.len(), 6);
        assert_eq!(s.range_count(), 3);
        assert_eq!(s.nth(0), Some(0));
        assert_eq!(s.nth(1), Some(1));
        assert_eq!(s.nth(2), Some(5));
        assert_eq!(s.nth(3), Some(6));
        assert_eq!(s.nth(4), Some(7));
        assert_eq!(s.nth(5), Some(20));
        assert_eq!(s.nth(6), None);
    }

    #[test]
    fn test_rank_boundaries() {
        let s = set_of(&[0, 1, 5, 6, 7, 20]);
        // line 미만인 원소 수
        assert_eq!(s.rank(0), 0);
        assert_eq!(s.rank(1), 1);
        assert_eq!(s.rank(2), 2);
        assert_eq!(s.rank(5), 2);
        assert_eq!(s.rank(6), 3);
        assert_eq!(s.rank(7), 4);
        assert_eq!(s.rank(8), 5);
        assert_eq!(s.rank(20), 5);
        assert_eq!(s.rank(21), 6);
        assert_eq!(s.rank(100), 6);
    }

    #[test]
    fn test_contains() {
        let s = set_of(&[3, 4, 10]);
        assert!(!s.contains(2));
        assert!(s.contains(3));
        assert!(s.contains(4));
        assert!(!s.contains(5));
        assert!(!s.contains(9));
        assert!(s.contains(10));
        assert!(!s.contains(11));
    }

    #[test]
    fn test_duplicate_push() {
        let mut s = set_of(&[1, 2, 3]);
        s.push_line(2);
        s.push_line(2);
        assert_eq!(s.ranges(), &[(1, 4)]);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn test_out_of_order_insert() {
        // tail 재판정 등으로 순서가 어긋난 추가가 들어와도 정합성이 유지되어야 한다
        let mut s = set_of(&[10, 11, 20]);
        s.push_line(0);
        s.push_line(10); // 중복
        s.push_line(21); // 끝에 추가
        assert_eq!(s.ranges(), &[(0, 1), (10, 12), (20, 22)]);
        assert_eq!(s.len(), 5);
        assert_eq!(s.nth(0), Some(0));
        assert_eq!(s.nth(4), Some(21));
        assert_eq!(s.rank(10), 1);
        assert_eq!(s.rank(20), 3);
    }

    #[test]
    fn test_remove_line() {
        let mut s = set_of(&[0, 1, 2, 5, 6, 10]);
        // 구간 앞에서 제거 → 구간 축소
        s.remove_line(0);
        assert_eq!(s.ranges(), &[(1, 3), (5, 7), (10, 11)]);
        // 구간 끝에서 제거
        s.remove_line(2);
        assert_eq!(s.ranges(), &[(1, 2), (5, 7), (10, 11)]);
        // 구간 끝 원소 제거 → 구간 축소
        s.remove_line(6);
        assert_eq!(s.ranges(), &[(1, 2), (5, 6), (10, 11)]);
    }

    #[test]
    fn test_remove_line_splits_range() {
        let mut s = set_of(&[0, 1, 2, 3, 4]);
        s.remove_line(2);
        assert_eq!(s.ranges(), &[(0, 2), (3, 5)]);
        assert_eq!(s.len(), 4);
        assert!(s.contains(1));
        assert!(!s.contains(2));
        assert!(s.contains(3));
        // 남은 원소 모두 제거
        for l in [0, 1, 3, 4] {
            s.remove_line(l);
        }
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn test_remove_single_element_range() {
        let mut s = set_of(&[5]);
        s.remove_line(5);
        assert!(s.is_empty());
        assert_eq!(s.range_count(), 0);
    }

    #[test]
    fn test_remove_absent_line() {
        let mut s = set_of(&[1, 3]);
        s.remove_line(2);
        s.remove_line(99);
        assert_eq!(s.ranges(), &[(1, 2), (3, 4)]);
    }

    #[test]
    fn test_push_after_remove() {
        // 제거 후 같은 위치에 다시 추가
        let mut s = set_of(&[0, 1, 2]);
        s.remove_line(1);
        assert_eq!(s.ranges(), &[(0, 1), (2, 3)]);
        s.push_line(1);
        assert_eq!(s.ranges(), &[(0, 3)]);
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn test_large_random_consistency() {
        // 난수 순서로 push/remove해도 len/contains가 순진한 Vec와 일치해야 한다
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut s = LineSet::new();
        let mut naive: Vec<u64> = Vec::new();
        for _ in 0..20_000 {
            let line = next() % 500;
            if next() % 3 == 0 {
                s.remove_line(line);
                naive.retain(|&l| l != line);
            } else {
                s.push_line(line);
                if !naive.contains(&line) {
                    naive.push(line);
                }
            }
        }
        naive.sort_unstable();
        assert_eq!(s.len(), naive.len() as u64);
        for (i, &expected) in naive.iter().enumerate() {
            assert_eq!(s.nth(i as u64), Some(expected));
            assert!(s.contains(expected));
        }
        // rank 검증: 각 경계값에 대해 순진 계산과 비교
        for line in 0..501u64 {
            let expected = naive.iter().filter(|&&l| l < line).count() as u64;
            assert_eq!(s.rank(line), expected, "rank({})", line);
        }
    }
}
