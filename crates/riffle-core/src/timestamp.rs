//! 줄 앞부분에서 로그 타임스탬프를 인식해 epoch 밀리초로 변환한다.
//!
//! 인식 형식 (줄 앞 64바이트 안에서 첫 매치):
//! - ISO-8601: `2026-11-12T07:15:42.134Z`, `+09:00` 오프셋, 공백 구분, `,` 소수점
//! - 슬래시 날짜: `2026/11/12 07:15:42`
//! - syslog: `Nov 12 07:15:42` (연도는 컨텍스트 기본값)
//! - Apache: `[12/Nov/2026:07:15:42 +0900]`
//! - 유닉스 epoch: 줄 시작이 정확히 10자리(초) 또는 13자리(밀리초) 숫자
//!
//! 시간대 표기가 없는 형식은 로컬 시간대로 간주한다 (local_offset_minutes 적용).
//! 날짜 계산(그레고리력 <-> epoch)은 외부 의존성 없이 Howard Hinnant 알고리즘을 쓴다.

/// 타임스탬프 스캔 범위 (줄 앞부분 바이트 수)
pub const SCAN_BYTES: usize = 64;

/// 타임스탬프가 없는 줄의 유효 타임스탬프 (-∞, 병합 시 맨 앞에 온다)
pub const NEG_INF_TS: i64 = i64::MIN;

/// 타임스탬프 파싱 컨텍스트.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsContext {
    /// 로컬 시간대 오프셋(분). 로컬 시각 = UTC + 오프셋 (예: KST = 540).
    /// JS `Date#getTimezoneOffset()`의 부호를 바꾼 값과 같다.
    /// 시간대 표기가 없는 타임스탬프를 epoch으로 변환할 때 적용한다.
    pub local_offset_minutes: i32,
    /// 연도가 없는 형식(syslog)에 적용할 기본 연도
    pub default_year: i32,
}

/// 줄 앞부분에서 첫 번째로 인식되는 타임스탬프를 epoch 밀리초로 반환한다.
/// 인식되지 않으면 None.
pub fn parse_timestamp(line: &[u8], ctx: &TsContext) -> Option<i64> {
    let s = &line[..line.len().min(SCAN_BYTES)];
    for pos in 0..s.len() {
        let b = s[pos];
        // epoch은 줄 맨 앞에서만 인정한다
        if pos == 0
            && b.is_ascii_digit()
            && let Some(v) = parse_epoch(s)
        {
            return Some(v);
        }
        // 앞 글자가 영숫자면 토큰 중간이므로 후보가 아니다
        if pos > 0 && s[pos - 1].is_ascii_alphanumeric() {
            continue;
        }
        if b.is_ascii_digit() {
            if let Some(v) = parse_dated(s, pos, ctx) {
                return Some(v);
            }
        } else if b == b'[' {
            if let Some(v) = parse_apache(s, pos, ctx) {
                return Some(v);
            }
        } else if b.is_ascii_alphabetic()
            && let Some(v) = parse_syslog(s, pos, ctx)
        {
            return Some(v);
        }
    }
    None
}

/// s[pos..pos+len]의 정확히 len자리 숫자를 u32로 읽는다.
fn fixed_num(s: &[u8], pos: usize, len: usize) -> Option<u32> {
    if pos + len > s.len() {
        return None;
    }
    let mut v: u32 = 0;
    for &b in &s[pos..pos + len] {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as u32;
    }
    Some(v)
}

/// s[pos..]의 min~max자리 숫자를 (값, 다음 위치)로 읽는다.
fn var_num(s: &[u8], pos: usize, min: usize, max: usize) -> Option<(u32, usize)> {
    let mut v: u32 = 0;
    let mut n = 0usize;
    while n < max && pos + n < s.len() && s[pos + n].is_ascii_digit() {
        v = v * 10 + (s[pos + n] - b'0') as u32;
        n += 1;
    }
    if n < min { None } else { Some((v, pos + n)) }
}

/// 3글자 영문 월 이름(대소문자 무관) → 1..=12
fn month_of(s: &[u8], pos: usize) -> Option<u32> {
    if pos + 3 > s.len() {
        return None;
    }
    let b = [s[pos] | 0x20, s[pos + 1] | 0x20, s[pos + 2] | 0x20];
    Some(match &b {
        b"jan" => 1,
        b"feb" => 2,
        b"mar" => 3,
        b"apr" => 4,
        b"may" => 5,
        b"jun" => 6,
        b"jul" => 7,
        b"aug" => 8,
        b"sep" => 9,
        b"oct" => 10,
        b"nov" => 11,
        b"dec" => 12,
        _ => return None,
    })
}

/// 윤년 여부
fn is_leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// 그레고리력 월의 일수
fn days_in_month(y: i32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// 날짜/시각 유효성 검사
fn valid_date(y: i32, m: u32, d: u32) -> bool {
    y >= 1 && (1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m)
}

fn valid_time(h: u32, mi: u32, s: u32) -> bool {
    h < 24 && mi < 60 && s < 60
}

/// 그레고리력 날짜 → 1970-01-01 기준 일수 (Howard Hinnant 알고리즘)
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = y as i64 - i64::from(m <= 2);
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 1970-01-01 기준 일수 → 그레고리력 (년, 월, 일) (Howard Hinnant 알고리즘)
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1024 + doe / 1461 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// 유닉스 초 → 그레고리력 연도 (syslog 기본 연도 계산용)
pub fn year_of_unix_secs(secs: u64) -> i32 {
    civil_from_days((secs / 86400) as i64).0
}

/// `YYYY-MM-DD` 또는 `YYYY/MM/DD` (두 구분자는 같아야 함) → (y, m, d, 다음 위치)
fn parse_date(s: &[u8], pos: usize) -> Option<(i32, u32, u32, usize)> {
    let y = fixed_num(s, pos, 4)?;
    let p = pos + 4;
    if p >= s.len() || (s[p] != b'-' && s[p] != b'/') {
        return None;
    }
    let sep = s[p];
    let m = fixed_num(s, p + 1, 2)?;
    let p2 = p + 3;
    if p2 >= s.len() || s[p2] != sep {
        return None;
    }
    let d = fixed_num(s, p2 + 1, 2)?;
    let y = y as i32;
    if !valid_date(y, m, d) {
        return None;
    }
    Some((y, m, d, p2 + 3))
}

/// `HH:MM:SS` → (h, m, s, 다음 위치)
fn parse_time(s: &[u8], pos: usize) -> Option<(u32, u32, u32, usize)> {
    let h = fixed_num(s, pos, 2)?;
    let mut p = pos + 2;
    if p >= s.len() || s[p] != b':' {
        return None;
    }
    let mi = fixed_num(s, p + 1, 2)?;
    p += 3;
    if p >= s.len() || s[p] != b':' {
        return None;
    }
    let sec = fixed_num(s, p + 1, 2)?;
    if !valid_time(h, mi, sec) {
        return None;
    }
    Some((h, mi, sec, p + 3))
}

/// 소수 초: `.` 또는 `,` 뒤 1~9자리 숫자 → 밀리초 (첫 3자리). 없으면 (0, pos).
fn parse_frac(s: &[u8], pos: usize) -> (u32, usize) {
    if pos >= s.len() || (s[pos] != b'.' && s[pos] != b',') {
        return (0, pos);
    }
    let mut p = pos + 1;
    let mut ms: u32 = 0;
    let mut digits = 0usize;
    while p < s.len() && s[p].is_ascii_digit() && digits < 9 {
        if digits < 3 {
            ms = ms * 10 + (s[p] - b'0') as u32;
        }
        digits += 1;
        p += 1;
    }
    if digits == 0 {
        return (0, pos);
    }
    // 1~2자리는 오른쪽에 0을 채워 밀리초로 환산
    while digits < 3 {
        ms *= 10;
        digits += 1;
    }
    (ms, p)
}

/// 시간대 표기: `Z`/`z` → 0, `±HH[:MM]`/`±HHMM`/`±HH` → 오프셋(분).
/// 없거나 형식이 안 맞으면 (None, pos).
fn parse_zone(s: &[u8], pos: usize) -> (Option<i32>, usize) {
    if pos >= s.len() {
        return (None, pos);
    }
    if s[pos] == b'Z' || s[pos] == b'z' {
        return (Some(0), pos + 1);
    }
    if s[pos] != b'+' && s[pos] != b'-' {
        return (None, pos);
    }
    let sign = if s[pos] == b'-' { -1 } else { 1 };
    let Some(hh) = fixed_num(s, pos + 1, 2) else {
        return (None, pos);
    };
    let mut p = pos + 3;
    let mut mm = 0;
    if p < s.len() && s[p] == b':' {
        let Some(v) = fixed_num(s, p + 1, 2) else {
            return (None, pos);
        };
        mm = v;
        p += 3;
    } else if let Some(v) = fixed_num(s, p, 2) {
        // `+0900` 형태 (콜론 없음)
        mm = v;
        p += 2;
    }
    if hh > 23 || mm > 59 {
        return (None, pos);
    }
    (Some(sign * (hh * 60 + mm) as i32), p)
}

/// 벽시각 필드 묶음 (to_epoch_ms의 인자를 줄이기 위한 구조체)
struct WallTime {
    y: i32,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    sec: u32,
    frac_ms: u32,
}

/// 벽시각(로컬/표기대) + 오프셋 → epoch 밀리초.
/// zone이 None이면 로컬 시간대(ctx.local_offset_minutes)로 간주한다.
fn to_epoch_ms(w: WallTime, zone: Option<i32>, ctx: &TsContext) -> i64 {
    let wall_ms = days_from_civil(w.y, w.mo, w.d) * 86_400_000
        + w.h as i64 * 3_600_000
        + w.mi as i64 * 60_000
        + w.sec as i64 * 1000
        + w.frac_ms as i64;
    // 로컬/표기 시각 = UTC + 오프셋 → UTC = 시각 - 오프셋
    wall_ms - zone.unwrap_or(ctx.local_offset_minutes) as i64 * 60_000
}

/// ISO-8601/슬래시 날짜-시각:
/// `YYYY-MM-DD[T|t| ]HH:MM:SS[.frac][zone]`, `YYYY/MM/DD[ ]HH:MM:SS[.frac][zone]`
fn parse_dated(s: &[u8], pos: usize, ctx: &TsContext) -> Option<i64> {
    let (y, m, d, p) = parse_date(s, pos)?;
    if p >= s.len() || (s[p] != b'T' && s[p] != b't' && s[p] != b' ') {
        return None;
    }
    let (h, mi, sec, p) = parse_time(s, p + 1)?;
    let (frac_ms, p) = parse_frac(s, p);
    let (zone, _) = parse_zone(s, p);
    Some(to_epoch_ms(
        WallTime {
            y,
            mo: m,
            d,
            h,
            mi,
            sec,
            frac_ms,
        },
        zone,
        ctx,
    ))
}

/// syslog: `Nov 12 07:15:42` (필드 사이 공백은 1개 이상, 연도는 기본값, 로컬 시간대)
fn parse_syslog(s: &[u8], pos: usize, ctx: &TsContext) -> Option<i64> {
    let m = month_of(s, pos)?;
    let mut p = pos + 3;
    if p >= s.len() || s[p] != b' ' {
        return None;
    }
    while p < s.len() && s[p] == b' ' {
        p += 1;
    }
    let (d, p2) = var_num(s, p, 1, 2)?;
    if !(1..=31).contains(&d) {
        return None;
    }
    p = p2;
    if p >= s.len() || s[p] != b' ' {
        return None;
    }
    while p < s.len() && s[p] == b' ' {
        p += 1;
    }
    let (h, mi, sec, p) = parse_time(s, p)?;
    let (frac_ms, p) = parse_frac(s, p);
    let (zone, _) = parse_zone(s, p);
    Some(to_epoch_ms(
        WallTime {
            y: ctx.default_year,
            mo: m,
            d,
            h,
            mi,
            sec,
            frac_ms,
        },
        zone,
        ctx,
    ))
}

/// Apache: `[12/Nov/2026:07:15:42 +0900]` (시간대 필수)
fn parse_apache(s: &[u8], pos: usize, ctx: &TsContext) -> Option<i64> {
    let mut p = pos + 1; // '['
    let (d, p2) = var_num(s, p, 1, 2)?;
    if !(1..=31).contains(&d) {
        return None;
    }
    p = p2;
    if p >= s.len() || s[p] != b'/' {
        return None;
    }
    let m = month_of(s, p + 1)?;
    p += 4;
    if p >= s.len() || s[p] != b'/' {
        return None;
    }
    let y = fixed_num(s, p + 1, 4)? as i32;
    p += 5;
    if p >= s.len() || s[p] != b':' {
        return None;
    }
    let (h, mi, sec, p) = parse_time(s, p + 1)?;
    if !valid_date(y, m, d) {
        return None;
    }
    if p >= s.len() || s[p] != b' ' {
        return None;
    }
    let (zone, p) = parse_zone(s, p + 1);
    let zone = zone?;
    if p >= s.len() || s[p] != b']' {
        return None;
    }
    let _ = ctx; // 시간대가 항상 명시되므로 로컬 오프셋은 쓰지 않는다
    Some(to_epoch_ms(
        WallTime {
            y,
            mo: m,
            d,
            h,
            mi,
            sec,
            frac_ms: 0,
        },
        Some(zone),
        ctx,
    ))
}

/// 유닉스 epoch: 줄 시작이 정확히 10자리(초) 또는 13자리(밀리초) 숫자.
fn parse_epoch(s: &[u8]) -> Option<i64> {
    let mut n = 0usize;
    while n < s.len() && s[n].is_ascii_digit() {
        n += 1;
    }
    if n != 10 && n != 13 {
        return None;
    }
    let mut v: i64 = 0;
    for &b in &s[..n] {
        v = v * 10 + (b - b'0') as i64;
    }
    Some(if n == 10 { v * 1000 } else { v })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTC 기준 컨텍스트 (오프셋 0)
    const UTC: TsContext = TsContext {
        local_offset_minutes: 0,
        default_year: 2026,
    };

    /// KST(+09:00) 기준 컨텍스트 (오프셋 +540분)
    const KST: TsContext = TsContext {
        local_offset_minutes: 540,
        default_year: 2026,
    };

    fn ts(s: &str, ctx: &TsContext) -> Option<i64> {
        parse_timestamp(s.as_bytes(), ctx)
    }

    // 2026-11-12T07:15:42.123Z = 1794467742123 (gen_log 기준 시각과 동일)
    const REF_MS: i64 = 1_794_467_742_123;

    #[test]
    fn iso_z() {
        assert_eq!(ts("2026-11-12T07:15:42.123Z INFO x", &UTC), Some(REF_MS));
        // 소수점 없음
        assert_eq!(ts("2026-11-12T07:15:42Z x", &UTC), Some(REF_MS - 123));
        // 접두어 + 대괄호
        assert_eq!(ts("[2026-11-12T07:15:42.123Z] x", &UTC), Some(REF_MS));
    }

    #[test]
    fn iso_frac_digits() {
        // 소수 자릿수 1~9: 첫 3자리만 밀리초로 사용
        assert_eq!(ts("2026-11-12T07:15:42.1Z", &UTC), Some(REF_MS - 23));
        assert_eq!(ts("2026-11-12T07:15:42.12Z", &UTC), Some(REF_MS - 3));
        assert_eq!(ts("2026-11-12T07:15:42.1234Z", &UTC), Some(REF_MS));
        assert_eq!(ts("2026-11-12T07:15:42.123456789Z", &UTC), Some(REF_MS));
        // 콤마 소수점
        assert_eq!(ts("2026-11-12T07:15:42,123Z", &UTC), Some(REF_MS));
        // 점만 있고 숫자 없음 → 소수부 없는 것으로 처리 (타임존 없음 → 로컬=UTC)
        assert_eq!(ts("2026-11-12T07:15:42.x", &UTC), Some(REF_MS - 123));
    }

    #[test]
    fn iso_zone_offset() {
        // +09:00 → UTC보다 9시간 빠름 = epoch에서 9시간 차감
        assert_eq!(ts("2026-11-12T16:15:42.123+09:00 x", &UTC), Some(REF_MS));
        // 콜론 없는 오프셋
        assert_eq!(ts("2026-11-12T16:15:42.123+0900", &UTC), Some(REF_MS));
        // 시만 있는 오프셋
        assert_eq!(ts("2026-11-12T16:15:42.123+09", &UTC), Some(REF_MS));
        // 음수 오프셋 -05:00
        assert_eq!(ts("2026-11-12T02:15:42.123-05:00", &UTC), Some(REF_MS));
    }

    #[test]
    fn iso_space_separator_and_local() {
        // 공백 구분 + 시간대 없음 → 로컬 적용
        // UTC 컨텍스트: 그대로 UTC
        assert_eq!(ts("2026-11-12 07:15:42.123 INFO", &UTC), Some(REF_MS));
        // KST 컨텍스트: 같은 벽시각이면 epoch은 9시간 작다
        assert_eq!(ts("2026-11-12 16:15:42.123 INFO", &KST), Some(REF_MS));
        assert_eq!(
            ts("2026-11-12 07:15:42.123 INFO", &KST),
            Some(REF_MS - 9 * 3_600_000)
        );
    }

    #[test]
    fn slash_date() {
        assert_eq!(ts("2026/11/12 07:15:42.123 x", &UTC), Some(REF_MS));
        assert_eq!(ts("2026/11/12 07:15:42 x", &UTC), Some(REF_MS - 123));
    }

    #[test]
    fn syslog() {
        // Nov 12 07:15:42 (연도 2026, 로컬 UTC)
        assert_eq!(ts("Nov 12 07:15:42 app: msg", &UTC), Some(REF_MS - 123));
        // 한 자리 일 + 두 칸 공백
        assert_eq!(
            ts("Nov  5 07:15:42 x", &UTC),
            Some(REF_MS - 123 - 7 * 86_400_000)
        );
        // 밀리초 소수부
        assert_eq!(ts("Nov 12 07:15:42.123 x", &UTC), Some(REF_MS));
        // 월 이름 소문자
        assert_eq!(ts("nov 12 07:15:42 x", &UTC), Some(REF_MS - 123));
        // KST 컨텍스트
        assert_eq!(ts("Nov 12 16:15:42 x", &KST), Some(REF_MS - 123));
    }

    #[test]
    fn apache() {
        // [12/Nov/2026:07:15:42 +0000]
        assert_eq!(
            ts("[12/Nov/2026:07:15:42 +0000] GET /", &UTC),
            Some(REF_MS - 123)
        );
        // +0900 → KST 벽시각
        assert_eq!(
            ts("[12/Nov/2026:16:15:42 +0900] x", &UTC),
            Some(REF_MS - 123)
        );
        // 로컬 오프셋과 무관하게 표기 시간대를 쓴다
        assert_eq!(
            ts("[12/Nov/2026:16:15:42 +0900] x", &KST),
            Some(REF_MS - 123)
        );
    }

    #[test]
    fn unix_epoch() {
        // 10자리 = 초
        assert_eq!(ts("1794467742 log", &UTC), Some(1_794_467_742_000));
        // 13자리 = 밀리초
        assert_eq!(ts("1794467742123 log", &UTC), Some(REF_MS));
        // 자릿수 미만/초과는 불인정
        assert_eq!(ts("179446774 log", &UTC), None);
        assert_eq!(ts("179446774212 log", &UTC), None);
        assert_eq!(ts("17944677421234 log", &UTC), None);
        // 중간에 나타나는 숫자는 epoch이 아니다
        assert_eq!(ts("user=1794467742 x", &UTC), None);
    }

    #[test]
    fn invalid_dates_rejected() {
        // 존재하지 않는 날짜
        assert_eq!(ts("2026-02-30T07:15:42Z", &UTC), None);
        assert_eq!(ts("2026-13-01T07:15:42Z", &UTC), None);
        assert_eq!(ts("2026-00-10T07:15:42Z", &UTC), None);
        assert_eq!(ts("2026-11-12T25:15:42Z", &UTC), None);
        assert_eq!(ts("2026-11-12T07:60:42Z", &UTC), None);
        assert_eq!(ts("2026-11-12T07:15:60Z", &UTC), None);
        // 평년 2/29 → 거부, 윤년 2/29 → 허용
        assert_eq!(ts("2026-02-29T00:00:00Z", &UTC), None);
        assert!(ts("2024-02-29T00:00:00Z", &UTC).is_some());
        // 월 말 경계: 4월은 30일
        assert_eq!(ts("2026-04-31T00:00:00Z", &UTC), None);
        assert!(ts("2026-04-30T00:00:00Z", &UTC).is_some());
        // 날짜만 있고 시각이 없으면 불인정
        assert_eq!(ts("2026-11-12 INFO x", &UTC), None);
        // 알 수 없는 월 이름
        assert_eq!(ts("Xxx 12 07:15:42", &UTC), None);
        // 일 0일 / 32일
        assert_eq!(ts("Nov 0 07:15:42", &UTC), None);
        assert_eq!(ts("Nov 32 07:15:42", &UTC), None);
    }

    #[test]
    fn scan_window_is_64_bytes() {
        // 타임스탬프 시작 위치가 64바이트 이내여야 한다
        let pad = "x".repeat(44); // 44바이트 접두어 → 타임스탬프는 44부터 시작
        let line = format!("{pad} Nov 12 07:15:42 end");
        assert_eq!(ts(&line, &UTC), Some(REF_MS - 123));

        // 타임스탬프가 64바이트 밖에 있으면 무시한다
        let pad = "y".repeat(64);
        let line = format!("{pad}Nov 12 07:15:42 end");
        assert_eq!(ts(&line, &UTC), None);

        // 시작이 64B 안이어도 형식 전체가 안에 들어와야 한다 (경계에서 잘리는 경우)
        let pad = "z".repeat(63);
        let line = format!("{pad}Nov 12 07:15:42");
        assert_eq!(ts(&line, &UTC), None);
    }

    #[test]
    fn no_timestamp() {
        assert_eq!(
            ts("   at java.lang.Thread.run(Thread.java:750)", &UTC),
            None
        );
        assert_eq!(ts("INFO just a message", &UTC), None);
        assert_eq!(ts("", &UTC), None);
        assert_eq!(ts("Caused by: java.io.IOException", &UTC), None);
    }

    #[test]
    fn boundary_guard() {
        // 영숫자 바로 뒤의 후보는 토큰 중간으로 본다
        assert_eq!(ts("v2026-11-12T07:15:42.123Z", &UTC), None);
        // 단, 구분 문자 뒤면 인정
        assert_eq!(ts("v=2026-11-12T07:15:42.123Z", &UTC), Some(REF_MS));
        // 숫자 앞의 숫자: 10자리 안에 또 다른 10자리가 생기지 않도록
        assert_eq!(ts("11794467742 log", &UTC), None);
    }

    #[test]
    fn first_match_wins() {
        // 앞쪽 매치를 우선한다
        assert_eq!(
            ts("Nov 12 07:15:42 then 2026-11-13T00:00:00Z", &UTC),
            Some(REF_MS - 123)
        );
    }

    #[test]
    fn known_epoch_values() {
        // 1970-01-01T00:00:00Z = 0
        assert_eq!(ts("1970-01-01T00:00:00Z", &UTC), Some(0));
        // 2000-02-29T12:00:00Z = 951825600 (윤년 날짜 계산 검증)
        assert_eq!(ts("2000-02-29T12:00:00Z", &UTC), Some(951_825_600_000));
        // 1900년은 윤년이 아님
        assert_eq!(ts("1900-02-29T00:00:00Z", &UTC), None);
        // 2000년은 400으로 나눠 떨어져 윤년
        assert!(ts("2000-02-29T00:00:00Z", &UTC).is_some());
        // year_of_unix_secs sanity
        assert_eq!(year_of_unix_secs(0), 1970);
        assert_eq!(year_of_unix_secs(1_794_467_742), 2026);
    }
}
