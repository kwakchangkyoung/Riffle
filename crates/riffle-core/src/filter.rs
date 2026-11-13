//! 다중 필터: 포함(OR/AND)/제외 규칙과 로그 레벨 조건을 조합해
//! "보이는 줄"을 판정한다. 검색과 분리되어 있으며, 필터 통과 여부는
//! `FilterSet::passes` 한 곳에서 결정한다.

use std::sync::OnceLock;

use regex::bytes::{Regex, RegexSet, RegexSetBuilder};

use crate::error::{Error, Result};
use crate::search::{Matcher, SearchOptions};

/// 필터 규칙의 종류
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// 포함 규칙 (보이려면 매치 필요)
    Include,
    /// 제외 규칙 (매치하면 숨김)
    Exclude,
}

/// 포함 규칙의 결합 방식
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combine {
    /// 모든 포함 규칙이 매치해야 함
    And,
    /// 하나라도 매치하면 됨
    Or,
}

/// 로그 레벨 (줄 앞부분의 첫 레벨 토큰에서 판정)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// 레벨 판정에 사용하는 줄 앞부분 바이트 수
const LEVEL_SCAN_BYTES: usize = 200;

/// 필터 규칙 하나의 정의 (컴파일 전)
#[derive(Debug, Clone)]
pub struct FilterRule {
    pub pattern: String,
    pub options: SearchOptions,
    pub kind: RuleKind,
    pub enabled: bool,
}

/// 필터 조건 전체 정의 (컴파일 전)
#[derive(Debug, Clone)]
pub struct FilterSpec {
    pub rules: Vec<FilterRule>,
    pub combine: Combine,
    pub levels: Vec<Level>,
}

/// 컴파일된 단일 규칙
struct CompiledRule {
    /// spec.rules 내 인덱스 (rule_ranges의 rule_idx로 사용)
    spec_index: usize,
    matcher: Matcher,
}

/// 컴파일된 필터. Send + Sync.
/// 판정 규칙: 보이는 줄 = level_ok ∧ include_ok ∧ ¬exclude_any
/// - level_ok: levels가 비어있지 않고 전체(5개)가 아닐 때만 적용.
///   레벨 토큰이 없는 줄은 level 조건 적용 시 제외된다.
/// - include_ok: 켜진 포함 규칙이 없으면 true. Or면 하나라도, And면 모두 매치.
/// - exclude_any: 켜진 제외 규칙 중 하나라도 매치하면 제외.
///
/// 매치 판정은 Phase 7의 줄 단위 의미(개행 제외, 끝 \r 제거)와 동일하며
/// 빈 매치는 매치로 치지 않는다.
pub struct FilterSet {
    combine: Combine,
    /// 레벨 조건이 적용되는지 여부 (levels 비어있지 않음 && 5개 전체 아님)
    level_active: bool,
    /// 활성화된 레벨 비트마스크 (Level::Error=bit0 ... Trace=bit4)
    level_mask: u8,
    /// 켜진 포함 규칙 (spec 순서 유지)
    include_rules: Vec<CompiledRule>,
    /// 켜진 제외 규칙
    exclude_rules: Vec<CompiledRule>,
    /// 모든 포함 규칙이 빈 매치 불가일 때만 Some (한 번의 RegexSet 평가로 판정)
    include_set: Option<RegexSet>,
    /// 모든 제외 규칙이 빈 매치 불가일 때만 Some
    exclude_set: Option<RegexSet>,
}

/// 모든 레벨 비트 (level 조건이 사실상 무의미해지는 상태)
const ALL_LEVELS_MASK: u8 = 0b1_1111;

impl FilterSet {
    /// FilterSpec을 컴파일한다.
    /// 잘못된 정규식/빈 패턴은 `Error::InvalidQuery("규칙 N: …")`로 반환한다.
    /// 꺼진 규칙은 컴파일하지 않는다.
    pub fn new(spec: &FilterSpec) -> Result<FilterSet> {
        let mut level_mask = 0u8;
        for &level in &spec.levels {
            level_mask |= 1 << level as u8;
        }
        let level_active = level_mask != 0 && level_mask != ALL_LEVELS_MASK;

        let mut include_rules = Vec::new();
        let mut exclude_rules = Vec::new();
        for (i, rule) in spec.rules.iter().enumerate() {
            if !rule.enabled {
                continue;
            }
            let matcher =
                Matcher::new(&rule.pattern, rule.options).map_err(|e| invalid_rule_error(i, &e))?;
            let entry = CompiledRule {
                spec_index: i,
                matcher,
            };
            match rule.kind {
                RuleKind::Include => include_rules.push(entry),
                RuleKind::Exclude => exclude_rules.push(entry),
            }
        }

        let include_set = build_rule_set(&include_rules)?;
        let exclude_set = build_rule_set(&exclude_rules)?;

        Ok(FilterSet {
            combine: spec.combine,
            level_active,
            level_mask,
            include_rules,
            exclude_rules,
            include_set,
            exclude_set,
        })
    }

    /// 적용할 조건이 하나도 없는지 반환한다 (레벨 조건·켜진 규칙 모두 없음).
    /// true이면 프론트는 필터를 적용하지 않는다.
    pub fn is_empty(&self) -> bool {
        !self.level_active && self.include_rules.is_empty() && self.exclude_rules.is_empty()
    }

    /// 줄(개행 제외, 끝 \r 제거된 바이트)이 필터를 통과하는지 판정한다.
    pub fn passes(&self, line: &[u8]) -> bool {
        if self.is_empty() {
            return true;
        }

        // level_ok: 레벨 조건이 있으면 줄의 레벨이 선택된 레벨이어야 한다.
        // 레벨 토큰이 없는 줄은 제외된다.
        if self.level_active {
            match line_level(line) {
                Some(level) if self.level_mask & (1 << level as u8) != 0 => {}
                _ => return false,
            }
        }

        // include_ok
        if !self.include_rules.is_empty() {
            let ok = match self.combine {
                Combine::Or => self.any_include_matches(line),
                Combine::And => self.all_include_matches(line),
            };
            if !ok {
                return false;
            }
        }

        // ¬exclude_any
        if !self.exclude_rules.is_empty() && self.any_exclude_matches(line) {
            return false;
        }

        true
    }

    /// 표시 텍스트 기준 규칙별 하이라이트용 매치 범위.
    /// 반환값은 (바이트 start, 바이트 end, spec.rules 인덱스) 목록이며
    /// 켜진 포함 규칙의 비어있지 않은 매치만 포함한다.
    /// UTF-16 변환은 호출자가 수행한다.
    pub fn rule_ranges(&self, line: &[u8]) -> Vec<(usize, usize, u32)> {
        let mut out = Vec::new();
        for rule in &self.include_rules {
            for (start, end) in rule.matcher.find_ranges(line) {
                out.push((start, end, rule.spec_index as u32));
            }
        }
        out
    }

    /// 포함 규칙 중 하나라도 매치하는지 (Or 판정용)
    fn any_include_matches(&self, line: &[u8]) -> bool {
        match &self.include_set {
            Some(set) => set.is_match(line),
            None => self
                .include_rules
                .iter()
                .any(|r| r.matcher.line_has_match(line)),
        }
    }

    /// 포함 규칙이 모두 매치하는지 (And 판정용)
    fn all_include_matches(&self, line: &[u8]) -> bool {
        match &self.include_set {
            Some(set) => {
                let matched = set.matches(line);
                self.include_rules
                    .iter()
                    .enumerate()
                    .all(|(i, _)| matched.matched(i))
            }
            None => self
                .include_rules
                .iter()
                .all(|r| r.matcher.line_has_match(line)),
        }
    }

    /// 제외 규칙 중 하나라도 매치하는지
    fn any_exclude_matches(&self, line: &[u8]) -> bool {
        match &self.exclude_set {
            Some(set) => set.is_match(line),
            None => self
                .exclude_rules
                .iter()
                .any(|r| r.matcher.line_has_match(line)),
        }
    }
}

/// 규칙 컴파일 오류를 "규칙 N: …" 형태로 만든다 (N은 1-based 표시용)
fn invalid_rule_error(index: usize, err: &Error) -> Error {
    let msg = match err {
        Error::InvalidQuery(m) => m.clone(),
        other => other.to_string(),
    };
    Error::InvalidQuery(format!("규칙 {}: {}", index + 1, msg))
}

/// 규칙 목록이 전부 "빈 매치 불가"이면 RegexSet을 만든다.
/// 하나라도 빈 매치를 만들 수 있는 규칙이 있으면 None을 반환해
/// 규칙별 Matcher(line_has_match) 판정으로 되돌린다.
fn build_rule_set(rules: &[CompiledRule]) -> Result<Option<RegexSet>> {
    if rules.is_empty() || rules.iter().any(|r| r.matcher.can_match_empty()) {
        return Ok(None);
    }
    let patterns: Vec<String> = rules.iter().map(|r| set_pattern(&r.matcher)).collect();
    RegexSetBuilder::new(&patterns)
        .build()
        .map(Some)
        .map_err(|e| Error::InvalidQuery(e.to_string()))
}

/// Matcher를 RegexSet용 패턴 문자열로 변환한다.
/// 리터럴은 escape하고, 대소문자 무시는 인라인 플래그로 감싼다.
fn set_pattern(matcher: &Matcher) -> String {
    let options = matcher.options();
    let body = if options.regex {
        matcher.query().to_string()
    } else {
        regex::escape(matcher.query())
    };
    if options.case_sensitive {
        body
    } else {
        format!("(?i:{body})")
    }
}

/// 줄의 로그 레벨을 반환한다.
/// 줄 앞 200바이트에서 `\b(ERROR|FATAL|WARN(?:ING)?|INFO|DEBUG|TRACE)\b`
/// 첫 매치를 찾고, FATAL→Error, WARNING→Warn으로 정규화한다.
/// 프론트엔드 LogViewport.tsx의 레벨 판정과 같은 규칙이다.
pub fn line_level(line: &[u8]) -> Option<Level> {
    static LEVEL_RE: OnceLock<Regex> = OnceLock::new();
    let re = LEVEL_RE.get_or_init(|| {
        Regex::new(r"\b(ERROR|FATAL|WARN(?:ING)?|INFO|DEBUG|TRACE)\b")
            .expect("레벨 정규식 컴파일 실패")
    });
    let head = &line[..line.len().min(LEVEL_SCAN_BYTES)];
    let m = re.find(head)?;
    match m.as_bytes() {
        b"ERROR" | b"FATAL" => Some(Level::Error),
        b"WARN" | b"WARNING" => Some(Level::Warn),
        b"INFO" => Some(Level::Info),
        b"DEBUG" => Some(Level::Debug),
        b"TRACE" => Some(Level::Trace),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str, kind: RuleKind) -> FilterRule {
        FilterRule {
            pattern: pattern.to_string(),
            options: SearchOptions {
                regex: false,
                case_sensitive: true,
            },
            kind,
            enabled: true,
        }
    }

    fn regex_rule(pattern: &str, kind: RuleKind) -> FilterRule {
        FilterRule {
            options: SearchOptions {
                regex: true,
                case_sensitive: true,
            },
            ..rule(pattern, kind)
        }
    }

    fn spec(rules: Vec<FilterRule>, combine: Combine, levels: Vec<Level>) -> FilterSpec {
        FilterSpec {
            rules,
            combine,
            levels,
        }
    }

    #[test]
    fn test_is_empty() {
        // 규칙 없음 + 레벨 없음 → 비어 있음
        let s = FilterSet::new(&spec(vec![], Combine::Or, vec![])).unwrap();
        assert!(s.is_empty());
        assert!(s.passes(b"anything"));

        // 모든 규칙이 꺼져 있으면 비어 있음
        let mut r = rule("x", RuleKind::Include);
        r.enabled = false;
        let s = FilterSet::new(&spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(s.is_empty());

        // 레벨 5개 전체 → 레벨 조건 무의미 → 비어 있음
        let s = FilterSet::new(&spec(
            vec![],
            Combine::Or,
            vec![
                Level::Error,
                Level::Warn,
                Level::Info,
                Level::Debug,
                Level::Trace,
            ],
        ))
        .unwrap();
        assert!(s.is_empty());

        // 레벨 일부 → 비어 있지 않음
        let s = FilterSet::new(&spec(vec![], Combine::Or, vec![Level::Error])).unwrap();
        assert!(!s.is_empty());
        assert!(s.passes(b"2024 ERROR something"));
        assert!(!s.passes(b"2024 INFO something"));
        assert!(!s.passes(b"no level token"));
    }

    #[test]
    fn test_include_or_and() {
        // OR: 하나라도 매치
        let s = FilterSet::new(&spec(
            vec![
                rule("ERROR", RuleKind::Include),
                rule("WARN", RuleKind::Include),
            ],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        assert!(s.passes(b"ERROR happened"));
        assert!(s.passes(b"WARN happened"));
        assert!(!s.passes(b"INFO all good"));

        // AND: 모두 매치
        let s = FilterSet::new(&spec(
            vec![
                rule("결제", RuleKind::Include),
                rule("APPROVED", RuleKind::Include),
            ],
            Combine::And,
            vec![],
        ))
        .unwrap();
        assert!(s.passes("결제 승인 APPROVED".as_bytes()));
        assert!(!s.passes("결제 승인 PENDING".as_bytes()));
        assert!(!s.passes("APPROVED only".as_bytes()));
    }

    #[test]
    fn test_exclude() {
        let s = FilterSet::new(&spec(
            vec![rule("heartbeat", RuleKind::Exclude)],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        assert!(s.passes(b"real work done"));
        assert!(!s.passes(b"heartbeat ok"));
        // 대소문자 구분: HEARTBEAT는 heartbeat 규칙에 매치되지 않아 통과
        assert!(s.passes(b"HEARTBEAT? no - case sensitive"));

        // 대소문자 무시 제외
        let mut r = rule("heartbeat", RuleKind::Exclude);
        r.options.case_sensitive = false;
        let s = FilterSet::new(&spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(!s.passes(b"Heartbeat ping"));
        assert!(s.passes(b"real work"));
    }

    #[test]
    fn test_include_and_exclude_combined() {
        let s = FilterSet::new(&spec(
            vec![
                rule("ERROR", RuleKind::Include),
                rule("retry", RuleKind::Exclude),
            ],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        assert!(s.passes(b"ERROR disk full"));
        assert!(!s.passes(b"ERROR retry scheduled"));
        assert!(!s.passes(b"INFO retry scheduled")); // include 불만족
    }

    #[test]
    fn test_level_filter() {
        let s = FilterSet::new(&spec(vec![], Combine::Or, vec![Level::Error])).unwrap();
        assert!(s.passes(b"2024-01-01 ERROR failed"));
        assert!(s.passes(b"FATAL crash")); // FATAL → Error
        assert!(!s.passes(b"WARN careful"));
        assert!(!s.passes(b"WARNING careful")); // WARNING → Warn
        assert!(!s.passes(b"INFO ok"));
        assert!(!s.passes(b"no level here")); // 토큰 없는 줄 제외
        // 첫 매치만 사용: WARN 뒤의 ERROR는 무시
        assert!(!s.passes(b"WARN then ERROR"));
    }

    #[test]
    fn test_level_warn_normalization() {
        let s = FilterSet::new(&spec(vec![], Combine::Or, vec![Level::Warn])).unwrap();
        assert!(s.passes(b"WARN careful"));
        assert!(s.passes(b"WARNING careful")); // WARNING → Warn 정규화
        assert!(!s.passes(b"ERROR bad"));
    }

    #[test]
    fn test_level_multi_select() {
        let s =
            FilterSet::new(&spec(vec![], Combine::Or, vec![Level::Error, Level::Warn])).unwrap();
        assert!(s.passes(b"ERROR x"));
        assert!(s.passes(b"WARN x"));
        assert!(!s.passes(b"INFO x"));
        assert!(!s.passes(b"plain"));
    }

    #[test]
    fn test_regex_rules() {
        let s = FilterSet::new(&spec(
            vec![regex_rule(r"status=5\d\d", RuleKind::Include)],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        assert!(s.passes(b"status=500 error"));
        assert!(!s.passes(b"status=200 ok"));

        // 정규식 + 제외 정규식
        let s = FilterSet::new(&spec(
            vec![
                regex_rule(r"user_\d+", RuleKind::Include),
                regex_rule(r"user_999", RuleKind::Exclude),
            ],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        assert!(s.passes(b"login user_123"));
        assert!(!s.passes(b"login user_999"));
    }

    #[test]
    fn test_case_insensitive_include() {
        let mut r = rule("error", RuleKind::Include);
        r.options.case_sensitive = false;
        let s = FilterSet::new(&spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(s.passes(b"ERROR happened"));
        assert!(s.passes(b"error happened"));
        assert!(!s.passes(b"all fine"));
    }

    #[test]
    fn test_invalid_regex_error_has_rule_number() {
        let s = FilterSet::new(&spec(
            vec![
                rule("ok", RuleKind::Include),
                regex_rule("([unclosed", RuleKind::Exclude),
            ],
            Combine::Or,
            vec![],
        ));
        match s {
            Err(Error::InvalidQuery(msg)) => {
                assert!(msg.contains("규칙 2"), "메시지: {}", msg);
            }
            other => panic!("InvalidQuery 기대, 실제 {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn test_empty_pattern_is_error() {
        let s = FilterSet::new(&spec(
            vec![rule("", RuleKind::Include)],
            Combine::Or,
            vec![],
        ));
        assert!(matches!(s, Err(Error::InvalidQuery(_))));
    }

    #[test]
    fn test_disabled_rule_not_compiled() {
        // 꺼진 규칙의 잘못된 정규식은 무시된다
        let mut r = regex_rule("([bad", RuleKind::Include);
        r.enabled = false;
        let s = FilterSet::new(&spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn test_nullable_pattern_per_rule_fallback() {
        // `a*`는 빈 매치가 가능 → RegexSet 경로를 쓰지 않고 규칙별 판정
        // "z"가 없는 줄은 매치로 치지 않는다 (빈 매치 무시)
        let s = FilterSet::new(&spec(
            vec![regex_rule("z*", RuleKind::Include)],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        assert!(s.passes(b"has z inside"));
        assert!(!s.passes(b"no match here"));
        assert!(!s.is_empty());
    }

    #[test]
    fn test_nullable_exclude() {
        // 빈 매치 가능한 제외 패턴: 실제 비어있지 않은 매치가 있을 때만 제외
        let s = FilterSet::new(&spec(
            vec![regex_rule("q*", RuleKind::Exclude)],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        assert!(!s.passes(b"has q here"));
        assert!(s.passes(b"clean line"));
    }

    #[test]
    fn test_rule_ranges() {
        // 규칙 인덱스는 spec.rules 기준이어야 하고, 제외 규칙은 나오지 않아야 한다
        let mut disabled = rule("unused", RuleKind::Include);
        disabled.enabled = false;
        let s = FilterSet::new(&spec(
            vec![
                rule("foo", RuleKind::Include), // spec idx 0
                rule("bar", RuleKind::Exclude), // spec idx 1 (하이라이트 제외)
                rule("baz", RuleKind::Include), // spec idx 2
                disabled,                       // spec idx 3 (꺼짐)
            ],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        let ranges = s.rule_ranges(b"xx foo yy baz zz");
        assert!(ranges.contains(&(3, 6, 0)));
        assert!(ranges.contains(&(10, 13, 2)));
        assert_eq!(ranges.len(), 2);
        assert!(ranges.iter().all(|&(_, _, idx)| idx != 1 && idx != 3));
    }

    #[test]
    fn test_line_level_edge() {
        // 200바이트 이후의 토큰은 무시
        let mut line = vec![b'x'; 250];
        line.extend_from_slice(b" ERROR");
        assert_eq!(line_level(&line), None);

        // 경계 바로 앞 토큰은 인식
        let mut line2 = vec![b'x'; 193];
        line2.extend_from_slice(b" INFO rest"); // INFO가 193..197
        assert_eq!(line_level(&line2), Some(Level::Info));

        // 단어 경계: "ERRORCODE"는 매치 아님
        assert_eq!(line_level(b"ERRORCODE happened"), None);
        assert_eq!(line_level(b"an ERROR occurred"), Some(Level::Error));
        // 빈 줄
        assert_eq!(line_level(b""), None);
    }
}
