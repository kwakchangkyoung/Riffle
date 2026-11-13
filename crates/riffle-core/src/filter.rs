//! 다중 필터: 포함(OR/AND)/제외 규칙과 로그 레벨 조건을 조합해
//! "보이는 줄"을 판정한다. 검색과 분리되어 있으며, 필터 통과 여부는
//! `FilterSet::passes` 한 곳에서 결정한다.

use std::sync::OnceLock;

use regex::bytes::{Regex, RegexSet, RegexSetBuilder};

use crate::error::{Error, Result};
use crate::search::{Matcher, SearchOptions};
use crate::structured::{Columns, split_fields};

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
    /// Some(컬럼명)이면 줄 전체가 아니라 해당 컬럼 값에만 매치한다.
    /// 컬럼 뷰 파일(CSV/JSONL)에서만 유효하며, 없는 이름이면 FilterSet::new이 에러를 반환한다.
    pub field: Option<String>,
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
    /// 필드 규칙이면 대상 컬럼의 names 기준 인덱스, 줄 전체 규칙이면 None
    field_index: Option<usize>,
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
    /// 켜진 포함 규칙 — 줄 전체 대상 (spec 순서 유지)
    include_rules: Vec<CompiledRule>,
    /// 켜진 제외 규칙 — 줄 전체 대상
    exclude_rules: Vec<CompiledRule>,
    /// 켜진 포함 규칙 — 지정 컬럼 값 대상 (field_index는 항상 Some)
    field_include_rules: Vec<CompiledRule>,
    /// 켜진 제외 규칙 — 지정 컬럼 값 대상
    field_exclude_rules: Vec<CompiledRule>,
    /// 모든 포함 규칙이 빈 매치 불가일 때만 Some (한 번의 RegexSet 평가로 판정)
    include_set: Option<RegexSet>,
    /// 모든 제외 규칙이 빈 매치 불가일 때만 Some
    exclude_set: Option<RegexSet>,
    /// 필드 규칙의 줄 분할에 쓰는 컬럼 정보 (필드 규칙이 있을 때만 의미 있음)
    columns: Option<Columns>,
}

/// 모든 레벨 비트 (level 조건이 사실상 무의미해지는 상태)
const ALL_LEVELS_MASK: u8 = 0b1_1111;

impl FilterSet {
    /// FilterSpec을 컴파일한다.
    /// 잘못된 정규식/빈 패턴은 `Error::InvalidQuery("규칙 N: …")`로 반환한다.
    /// 꺼진 규칙은 컴파일하지 않는다.
    /// field가 지정된 규칙은 columns에서 그 이름의 컬럼을 찾아 컬럼 값에만 매치한다.
    /// columns가 None이거나 그 이름이 없으면 "규칙 N: 컬럼 'x' 를 찾을 수 없습니다" 에러다.
    pub fn new(spec: &FilterSpec, columns: Option<&Columns>) -> Result<FilterSet> {
        let mut level_mask = 0u8;
        for &level in &spec.levels {
            level_mask |= 1 << level as u8;
        }
        let level_active = level_mask != 0 && level_mask != ALL_LEVELS_MASK;

        let mut include_rules = Vec::new();
        let mut exclude_rules = Vec::new();
        let mut field_include_rules = Vec::new();
        let mut field_exclude_rules = Vec::new();
        for (i, rule) in spec.rules.iter().enumerate() {
            if !rule.enabled {
                continue;
            }
            let matcher =
                Matcher::new(&rule.pattern, rule.options).map_err(|e| invalid_rule_error(i, &e))?;
            let field_index = match &rule.field {
                Some(name) => Some(
                    columns
                        .and_then(|c| c.names.iter().position(|n| n == name))
                        .ok_or_else(|| {
                            Error::InvalidQuery(format!(
                                "규칙 {}: 컬럼 '{}' 를 찾을 수 없습니다",
                                i + 1,
                                name
                            ))
                        })?,
                ),
                None => None,
            };
            let entry = CompiledRule {
                spec_index: i,
                matcher,
                field_index,
            };
            match (rule.kind, field_index.is_some()) {
                (RuleKind::Include, true) => field_include_rules.push(entry),
                (RuleKind::Exclude, true) => field_exclude_rules.push(entry),
                (RuleKind::Include, false) => include_rules.push(entry),
                (RuleKind::Exclude, false) => exclude_rules.push(entry),
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
            field_include_rules,
            field_exclude_rules,
            include_set,
            exclude_set,
            columns: columns.cloned(),
        })
    }

    /// 적용할 조건이 하나도 없는지 반환한다 (레벨 조건·켜진 규칙 모두 없음).
    /// true이면 프론트는 필터를 적용하지 않는다.
    pub fn is_empty(&self) -> bool {
        !self.level_active
            && self.include_rules.is_empty()
            && self.exclude_rules.is_empty()
            && self.field_include_rules.is_empty()
            && self.field_exclude_rules.is_empty()
    }

    /// 줄(개행 제외, 끝 \r 제거된 바이트)이 필터를 통과하는지 판정한다.
    /// 필드 규칙이 있으면 줄을 컬럼 값으로 한 번 분할해 그 값에만 매치한다.
    /// 필드 규칙이 없으면 기존과 동일한 경로로 판정한다 (분할 비용 없음).
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

        // 필드 규칙이 있을 때만 줄을 컬럼 값으로 분할한다 (포함/제외 규칙이 공유)
        let fields = if self.has_field_rules() {
            self.columns
                .as_ref()
                .map(|cols| split_fields(&String::from_utf8_lossy(line), cols))
        } else {
            None
        };

        // include_ok
        if !self.include_rules.is_empty() || !self.field_include_rules.is_empty() {
            let ok = match self.combine {
                Combine::Or => {
                    self.any_include_matches(line)
                        || self.any_field_matches(&self.field_include_rules, fields.as_deref())
                }
                Combine::And => {
                    self.all_include_matches(line)
                        && self.all_field_matches(&self.field_include_rules, fields.as_deref())
                }
            };
            if !ok {
                return false;
            }
        }

        // ¬exclude_any
        if (!self.exclude_rules.is_empty() && self.any_exclude_matches(line))
            || self.any_field_matches(&self.field_exclude_rules, fields.as_deref())
        {
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

    /// 필드 규칙이 하나라도 있는지 (줄 분할 필요 여부)
    fn has_field_rules(&self) -> bool {
        !self.field_include_rules.is_empty() || !self.field_exclude_rules.is_empty()
    }

    /// 필드 규칙 하나의 매치 판정: 분할된 해당 컬럼 값에만 Matcher를 적용한다.
    /// 줄 단위 규칙(field_index None)이거나 분할 결과가 없으면 false.
    fn field_rule_match(rule: &CompiledRule, fields: Option<&[String]>) -> bool {
        match rule.field_index {
            Some(idx) => fields
                .and_then(|f| f.get(idx))
                .is_some_and(|v| rule.matcher.line_has_match(v.as_bytes())),
            None => false,
        }
    }

    /// 필드 규칙 중 하나라도 매치하는지 (Or 포함 / 제외 판정용)
    fn any_field_matches(&self, rules: &[CompiledRule], fields: Option<&[String]>) -> bool {
        rules.iter().any(|r| Self::field_rule_match(r, fields))
    }

    /// 필드 규칙이 모두 매치하는지 (And 포함 판정용)
    fn all_field_matches(&self, rules: &[CompiledRule], fields: Option<&[String]>) -> bool {
        rules.iter().all(|r| Self::field_rule_match(r, fields))
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
            field: None,
        }
    }

    /// 필드 규칙 헬퍼: 지정 컬럼 값에만 매치하는 규칙
    fn field_rule(pattern: &str, kind: RuleKind, field: &str) -> FilterRule {
        FilterRule {
            field: Some(field.to_string()),
            ..rule(pattern, kind)
        }
    }

    /// CSV 컬럼 테스트용 Columns (구분자 ',', 헤더 이름 목록)
    fn csv_cols(names: &[&str]) -> Columns {
        Columns {
            format: crate::structured::LogFormat::Csv {
                delimiter: b',',
                has_header: true,
            },
            names: names.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// 컬럼 없이 spec을 컴파일한다 (기존 테스트 공통 경로)
    fn compile(spec: FilterSpec) -> Result<FilterSet> {
        FilterSet::new(&spec, None)
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
        let s = compile(spec(vec![], Combine::Or, vec![])).unwrap();
        assert!(s.is_empty());
        assert!(s.passes(b"anything"));

        // 모든 규칙이 꺼져 있으면 비어 있음
        let mut r = rule("x", RuleKind::Include);
        r.enabled = false;
        let s = compile(spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(s.is_empty());

        // 레벨 5개 전체 → 레벨 조건 무의미 → 비어 있음
        let s = compile(spec(
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
        let s = compile(spec(vec![], Combine::Or, vec![Level::Error])).unwrap();
        assert!(!s.is_empty());
        assert!(s.passes(b"2024 ERROR something"));
        assert!(!s.passes(b"2024 INFO something"));
        assert!(!s.passes(b"no level token"));
    }

    #[test]
    fn test_include_or_and() {
        // OR: 하나라도 매치
        let s = compile(spec(
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
        let s = compile(spec(
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
        let s = compile(spec(
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
        let s = compile(spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(!s.passes(b"Heartbeat ping"));
        assert!(s.passes(b"real work"));
    }

    #[test]
    fn test_include_and_exclude_combined() {
        let s = compile(spec(
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
        let s = compile(spec(vec![], Combine::Or, vec![Level::Error])).unwrap();
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
        let s = compile(spec(vec![], Combine::Or, vec![Level::Warn])).unwrap();
        assert!(s.passes(b"WARN careful"));
        assert!(s.passes(b"WARNING careful")); // WARNING → Warn 정규화
        assert!(!s.passes(b"ERROR bad"));
    }

    #[test]
    fn test_level_multi_select() {
        let s = compile(spec(vec![], Combine::Or, vec![Level::Error, Level::Warn])).unwrap();
        assert!(s.passes(b"ERROR x"));
        assert!(s.passes(b"WARN x"));
        assert!(!s.passes(b"INFO x"));
        assert!(!s.passes(b"plain"));
    }

    #[test]
    fn test_regex_rules() {
        let s = compile(spec(
            vec![regex_rule(r"status=5\d\d", RuleKind::Include)],
            Combine::Or,
            vec![],
        ))
        .unwrap();
        assert!(s.passes(b"status=500 error"));
        assert!(!s.passes(b"status=200 ok"));

        // 정규식 + 제외 정규식
        let s = compile(spec(
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
        let s = compile(spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(s.passes(b"ERROR happened"));
        assert!(s.passes(b"error happened"));
        assert!(!s.passes(b"all fine"));
    }

    #[test]
    fn test_invalid_regex_error_has_rule_number() {
        let s = compile(spec(
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
        let s = compile(spec(vec![rule("", RuleKind::Include)], Combine::Or, vec![]));
        assert!(matches!(s, Err(Error::InvalidQuery(_))));
    }

    #[test]
    fn test_disabled_rule_not_compiled() {
        // 꺼진 규칙의 잘못된 정규식은 무시된다
        let mut r = regex_rule("([bad", RuleKind::Include);
        r.enabled = false;
        let s = compile(spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn test_nullable_pattern_per_rule_fallback() {
        // `a*`는 빈 매치가 가능 → RegexSet 경로를 쓰지 않고 규칙별 판정
        // "z"가 없는 줄은 매치로 치지 않는다 (빈 매치 무시)
        let s = compile(spec(
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
        let s = compile(spec(
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
        let s = compile(spec(
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

    // ===== 필드 규칙 (컬럼 값 매치) =====

    fn csv_filter(rules: Vec<FilterRule>, combine: Combine, names: &[&str]) -> FilterSet {
        let cols = csv_cols(names);
        FilterSet::new(&spec(rules, combine, vec![]), Some(&cols)).unwrap()
    }

    #[test]
    fn test_field_rule_csv_only_matches_that_column() {
        // status 컬럼 = 500 인 줄만 통과. 다른 컬럼에 500이 있어도 매치하지 않는다.
        let s = csv_filter(
            vec![field_rule("500", RuleKind::Include, "status")],
            Combine::Or,
            &["time", "status", "msg"],
        );
        assert!(s.passes(b"2024-01-01,500,internal error"));
        // msg에 500이 있어도 status가 500이 아니면 불통과
        assert!(!s.passes(b"2024-01-01,200,port 500 open"));
        // 필드 수가 부족한 줄(500이 status가 아닌 위치에만 있음)도 불통과
        assert!(!s.passes(b"500"));
        // 필드 규칙은 컬럼 안의 부분 문자열도 매치한다
        assert!(s.passes(b"2024-01-01,1500,weird"));
    }

    #[test]
    fn test_field_rule_csv_combine_and_exclude() {
        let names = &["time", "status", "msg"];
        // AND: status=5xx 정규식 ∧ msg에 timeout 포함
        let mut r1 = field_rule("5..", RuleKind::Include, "status");
        r1.options.regex = true;
        let s = csv_filter(
            vec![
                r1,
                field_rule("timeout", RuleKind::Include, "msg"),
                field_rule("health", RuleKind::Exclude, "msg"),
            ],
            Combine::And,
            names,
        );
        assert!(s.passes(b"2024-01-01,500,timeout occurred"));
        assert!(!s.passes(b"2024-01-01,200,timeout occurred")); // status 불일치
        assert!(!s.passes(b"2024-01-01,500,all good")); // msg 불일치
        assert!(!s.passes(b"2024-01-01,500,timeout health check")); // 제외 규칙
    }

    #[test]
    fn test_field_rule_csv_or_mixes_with_line_rule() {
        // OR: 필드 규칙과 줄 전체 규칙이 섞여도 각각 평가된다
        let s = csv_filter(
            vec![
                field_rule("500", RuleKind::Include, "status"),
                rule("urgent", RuleKind::Include),
            ],
            Combine::Or,
            &["time", "status", "msg"],
        );
        assert!(s.passes(b"2024-01-01,500,plain msg")); // 필드 규칙
        assert!(s.passes(b"2024-01-01,200,urgent work")); // 줄 규칙
        assert!(!s.passes(b"2024-01-01,200,plain msg"));
    }

    #[test]
    fn test_field_rule_jsonl() {
        let cols = Columns {
            format: crate::structured::LogFormat::JsonLines,
            names: vec!["level".to_string(), "msg".to_string()],
        };
        let s = FilterSet::new(
            &spec(
                vec![field_rule("error", RuleKind::Include, "level")],
                Combine::Or,
                vec![],
            ),
            Some(&cols),
        )
        .unwrap();
        assert!(s.passes(br#"{"level":"error","msg":"boom"}"#));
        // msg에 error가 있어도 level이 error가 아니면 불통과
        assert!(!s.passes(br#"{"level":"info","msg":"error recovered"}"#));
        // 파싱 실패 줄: 첫 컬럼(level 위치)에 원문이 들어가 error 매치
        assert!(s.passes(b"not json error"));
        assert!(!s.passes(b"not json at all"));
    }

    #[test]
    fn test_field_rule_unknown_column_error() {
        let cols = csv_cols(&["a", "b"]);
        let err = FilterSet::new(
            &spec(
                vec![
                    rule("x", RuleKind::Include),
                    field_rule("y", RuleKind::Include, "nosuch"),
                ],
                Combine::Or,
                vec![],
            ),
            Some(&cols),
        );
        match err {
            Err(Error::InvalidQuery(msg)) => {
                assert_eq!(msg, "규칙 2: 컬럼 'nosuch' 를 찾을 수 없습니다");
            }
            other => panic!("InvalidQuery 기대, 실제 {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn test_field_rule_without_columns_error() {
        // columns=None이면 필드 규칙은 무효 에러
        let err = compile(spec(
            vec![field_rule("y", RuleKind::Include, "status")],
            Combine::Or,
            vec![],
        ));
        assert!(matches!(err, Err(Error::InvalidQuery(_))));
    }

    #[test]
    fn test_disabled_field_rule_ignored() {
        // 꺼진 필드 규칙은 컬럼 존재 여부를 검사하지 않는다
        let mut r = field_rule("y", RuleKind::Include, "nosuch");
        r.enabled = false;
        let s = compile(spec(vec![r], Combine::Or, vec![])).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn test_field_rule_no_highlight_ranges() {
        // 필드 규칙의 매치는 rule_highlights 대상이 아니다 (줄 규칙만)
        let s = csv_filter(
            vec![
                field_rule("500", RuleKind::Include, "status"),
                rule("msg", RuleKind::Include), // spec idx 1
            ],
            Combine::Or,
            &["time", "status", "msg"],
        );
        let ranges = s.rule_ranges(b"2024-01-01,500,msg here");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].2, 1); // 줄 전체 규칙만 인덱스 1로 나온다
    }

    #[test]
    fn test_field_rule_csv_or_two_fields() {
        // OR: 서로 다른 두 컬럼의 규칙 — 하나만 맞아도 통과
        let s = csv_filter(
            vec![
                field_rule("500", RuleKind::Include, "status"),
                field_rule("timeout", RuleKind::Include, "msg"),
            ],
            Combine::Or,
            &["time", "status", "msg"],
        );
        assert!(s.passes(b"2024-01-01,500,plain msg")); // status만 매치
        assert!(s.passes(b"2024-01-01,200,timeout hit")); // msg만 매치
        assert!(s.passes(b"2024-01-01,500,timeout both"));
        assert!(!s.passes(b"2024-01-01,200,plain msg"));
    }

    #[test]
    fn test_field_rule_and_mixed_line_and_exclude() {
        // AND: 필드 규칙 ∧ 줄 전체 규칙 — 둘 다 맞아야 통과
        let s = csv_filter(
            vec![
                field_rule("500", RuleKind::Include, "status"),
                rule("urgent", RuleKind::Include),
            ],
            Combine::And,
            &["time", "status", "msg"],
        );
        assert!(s.passes(b"2024-01-01,500,urgent fix"));
        assert!(!s.passes(b"2024-01-01,500,plain msg")); // 줄 규칙 불만족
        assert!(!s.passes(b"2024-01-01,200,urgent fix")); // 필드 규칙 불만족

        // 필드 포함 + 줄 전체 제외: 포함을 만족해도 제외 규칙이 우선
        let s = csv_filter(
            vec![
                field_rule("500", RuleKind::Include, "status"),
                rule("health", RuleKind::Exclude),
            ],
            Combine::Or,
            &["time", "status", "msg"],
        );
        assert!(s.passes(b"2024-01-01,500,timeout occurred"));
        assert!(!s.passes(b"2024-01-01,500,health check")); // 줄 어디든 health면 제외
        // 필드 포함 + 필드 제외(다른 컬럼): 제외 컬럼 값만 검사
        let s = csv_filter(
            vec![
                field_rule("500", RuleKind::Include, "status"),
                field_rule("2024", RuleKind::Exclude, "time"),
            ],
            Combine::Or,
            &["time", "status", "msg"],
        );
        assert!(s.passes(b"2025-01-01,500,ok"));
        assert!(!s.passes(b"2024-01-01,500,ok")); // time 컬럼이 2024면 제외
        // msg에 2024가 있어도 time 컬럼만 검사하므로 제외되지 않는다
        assert!(s.passes(b"2025-01-01,500,since 2024"));
    }
}
