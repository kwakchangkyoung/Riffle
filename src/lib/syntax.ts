/**
 * 로그 줄 토큰 구문 강조.
 * 화면에 이미 받아 둔 줄 텍스트만 다루는 순수 함수 모음 (IPC/브라우저 상태 없음).
 */

export type TokenKind =
    | "timestamp"
    | "level"
    | "component"
    | "key"
    | "value"
    | "number"
    | "ip"
    | "url"
    | "path"
    | "string"
    | "uuid"
    | "hex"
    | "httpStatus2xx"
    | "httpStatus3xx"
    | "httpStatus4xx"
    | "httpStatus5xx";

export interface Token {
    start: number;
    end: number;
    kind: TokenKind;
}

/** 한 줄에서 토크나이즈할 최대 길이. 초과분은 일반 텍스트로 둔다 */
const SCAN_LIMIT = 2000;
/** 타임스탬프를 인정하는 줄 앞부분 길이 */
const TIMESTAMP_LIMIT = 40;
/** 로그 레벨 토큰을 인정하는 줄 앞부분 길이 */
const LEVEL_LIMIT = 200;

const RE_ISO = String.raw`\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(?:[.,]\d{1,6})?(?:Z|[+-]\d{2}:?\d{2})?`;
const RE_SYSLOG = String.raw`(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) +\d{1,2} +\d{2}:\d{2}:\d{2}`;
const RE_APACHE = String.raw`\[\d{2}\/[A-Za-z]{3}\/\d{4}:\d{2}:\d{2}:\d{2} [+-]\d{4}\]`;
const RE_TIME = String.raw`\d{2}:\d{2}:\d{2}(?:[.,]\d{1,6})?`;
const RE_LEVEL = String.raw`\b(?:ERROR|FATAL|WARN(?:ING)?|INFO|DEBUG|TRACE)\b`;
const RE_COMP = String.raw`\[[\w.-]{1,30}\]`;
const RE_UUID = String.raw`\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b`;
const RE_URL = String.raw`https?:\/\/[^\s"'()[\]<>{},;]+`;
// key=value / "key":value — 따옴표 키(JSON 스타일)와 따옴표 값(공백 포함)도 인식한다
const RE_KV = String.raw`(?<kvk>"?[A-Za-z_][\w.-]*"?)[=:][ \t]*(?<kvv>"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*'|[^\s,;\]}\)]+)`;
const RE_IP = String.raw`\b\d{1,3}(?:\.\d{1,3}){3}(?::\d{1,5})?\b`;
const RE_HEX = String.raw`\b0x[0-9a-fA-F]+\b`;
const RE_PATH = String.raw`\/[^\s"'()[\]{},;]*\/[^\s"'()[\]{},;]*`;
const RE_STR = String.raw`"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*'`;
const RE_UNIT = String.raw`(?:ms|us|µs|ns|s|m|h|d|KB|KiB|MB|MiB|GB|GiB|TB|TiB|kB|B|k|M|G|%)`;
const RE_NUM = String.raw`\b\d+(?:\.\d+)?${RE_UNIT}?\b`;

/**
 * 줄 전체를 한 번 스캔하는 결합 토큰 정규식.
 * 앞쪽 대안이 같은 위치에서 우선한다 (타임스탬프 > 레벨 > 컴포넌트 > … > 숫자).
 */
const TOKEN_RE = new RegExp(
    [
        `(?<ts>${RE_ISO}|${RE_SYSLOG}|${RE_APACHE}|${RE_TIME})`,
        `(?<level>${RE_LEVEL})`,
        `(?<comp>${RE_COMP})`,
        `(?<uuid>${RE_UUID})`,
        `(?<url>${RE_URL})`,
        RE_KV,
        `(?<ip>${RE_IP})`,
        `(?<hex>${RE_HEX})`,
        `(?<path>${RE_PATH})`,
        `(?<str>${RE_STR})`,
        `(?<num>${RE_NUM})`,
    ].join("|"),
    "g",
);

/** 로그 레벨 토큰 탐색 (레거시 단독 검색용 — 토크나이저와 같은 대상) */
const LEVEL_TOKEN_RE = /\b(ERROR|FATAL|WARN(?:ING)?|INFO|DEBUG|TRACE)\b/;
const LEVEL_WORD_RE = /^(?:ERROR|FATAL|WARN(?:ING)?|INFO|DEBUG|TRACE)$/;

/** 값 전체 매치용 보조 정규식 (key=value 값 분류) */
const FULL_IP_RE = /^\d{1,3}(?:\.\d{1,3}){3}(?::\d{1,5})?$/;
const FULL_URL_RE = /^https?:\/\//;
const FULL_UUID_RE =
    /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/;
const FULL_HEX_RE = /^0x[0-9a-fA-F]+$/;
const FULL_PATH_RE = /^\/[^\s"'()[\]{},;]*\/[^\s"'()[\]{},;]*$/;
const FULL_NUM_RE = new RegExp(`^\\d+(?:\\.\\d+)?${RE_UNIT}?$`);
const STATUS_NUM_RE = /^[1-5]\d{2}$/;
/** HTTP 상태 자리로 간주하는 키 이름 (status=503, status: 404 등) */
const STATUS_KEY_RE =
    /^(?:status|status_code|statuscode|http_status|httpstatus|code|sc)$/i;

function statusKind(digits: string): TokenKind {
    switch (digits[0]) {
        case "2":
            return "httpStatus2xx";
        case "3":
            return "httpStatus3xx";
        case "4":
            return "httpStatus4xx";
        default:
            return "httpStatus5xx";
    }
}

/** key=value / key: value 의 값 부분을 분류한다 (key의 따옴표는 벗겨 비교) */
function classifyValue(key: string, val: string): TokenKind {
    if (
        val.length >= 2 &&
        ((val.startsWith('"') && val.endsWith('"')) ||
            (val.startsWith("'") && val.endsWith("'")))
    ) {
        return "string";
    }
    const bareKey = key.startsWith('"') ? key.slice(1, -1) : key;
    if (STATUS_KEY_RE.test(bareKey) && STATUS_NUM_RE.test(val)) {
        return statusKind(val);
    }
    if (FULL_IP_RE.test(val)) return "ip";
    if (FULL_URL_RE.test(val)) return "url";
    if (FULL_UUID_RE.test(val)) return "uuid";
    if (FULL_HEX_RE.test(val)) return "hex";
    if (FULL_PATH_RE.test(val)) return "path";
    if (FULL_NUM_RE.test(val)) return "number";
    return "value";
}

/**
 * 따옴표 값/문자열 안의 첫 레벨 단어 범위를 찾는다 (레거시 LEVEL_TOKEN_RE와 같은 대상).
 * 토큰은 겹칠 수 없으므로 문자열 안 레벨 단어는 문자열 토큰 대신 level 토큰으로 둔다.
 */
function innerLevelSpan(
    text: string,
    start: number,
    end: number,
): Token | null {
    if (end - start < 3) return null;
    const a = text[start];
    const b = text[end - 1];
    if (!((a === '"' && b === '"') || (a === "'" && b === "'"))) {
        return null;
    }
    const m = LEVEL_TOKEN_RE.exec(text.slice(start + 1, end - 1));
    if (m === null) return null;
    return {
        start: start + 1 + m.index,
        end: start + 1 + m.index + m[0].length,
        kind: "level",
    };
}

/** tokenizeLine 결과 캐시 (스크롤 리렌더 시 재계산 방지) */
const tokenCache = new Map<string, Token[]>();
const TOKEN_CACHE_MAX = 2000;

/**
 * 줄 텍스트를 토큰 목록으로 분해합니다.
 * 반환되는 토큰은 겹치지 않고 시작 위치 오름차순입니다.
 */
export function tokenizeLine(text: string): Token[] {
    const cached = tokenCache.get(text);
    if (cached !== undefined) return cached;

    const head = text.length > SCAN_LIMIT ? text.slice(0, SCAN_LIMIT) : text;
    const tokens: Token[] = [];
    let levelUsed = false;

    TOKEN_RE.lastIndex = 0;
    let m: RegExpExecArray | null;
    while ((m = TOKEN_RE.exec(head)) !== null) {
        const g = m.groups ?? {};
        const s = m.index;
        const e = s + m[0].length;

        if (g.ts !== undefined) {
            // 타임스탬프는 줄 앞부분에서만 인정 (뒤쪽 매치는 일반 텍스트)
            if (s < TIMESTAMP_LIMIT) {
                tokens.push({ start: s, end: e, kind: "timestamp" });
            }
            continue;
        }
        if (g.level !== undefined) {
            // 첫 레벨 토큰만 인정 (레거시 LEVEL_TOKEN_RE 동작과 동일)
            if (!levelUsed && s < LEVEL_LIMIT) {
                levelUsed = true;
                tokens.push({ start: s, end: e, kind: "level" });
            }
            continue;
        }
        if (g.comp !== undefined) {
            const inner = head.slice(s + 1, e - 1);
            if (LEVEL_WORD_RE.test(inner)) {
                // [INFO] 같이 레벨 단어만 든 괄호는 컴포넌트가 아니라 레벨로 표시
                if (!levelUsed && s + 1 < LEVEL_LIMIT) {
                    levelUsed = true;
                    tokens.push({ start: s + 1, end: e - 1, kind: "level" });
                }
                continue;
            }
            tokens.push({ start: s, end: e, kind: "component" });
            continue;
        }
        if (g.uuid !== undefined) {
            tokens.push({ start: s, end: e, kind: "uuid" });
            continue;
        }
        if (g.url !== undefined) {
            tokens.push({ start: s, end: e, kind: "url" });
            continue;
        }
        if (g.kvk !== undefined && g.kvv !== undefined) {
            // key=value / "key":value — 키는 흐리게, 값은 종류별로 분류
            const keyEnd = s + g.kvk.length;
            const valStart = e - g.kvv.length;
            tokens.push({ start: s, end: keyEnd, kind: "key" });
            // 따옴표 값 안의 레벨 단어("level":"ERROR")는 레벨 토큰으로 표시한다
            const lv =
                !levelUsed && valStart < LEVEL_LIMIT
                    ? innerLevelSpan(head, valStart, e)
                    : null;
            if (lv !== null) {
                levelUsed = true;
                tokens.push(lv);
            } else {
                tokens.push({
                    start: valStart,
                    end: e,
                    kind: classifyValue(g.kvk, g.kvv),
                });
            }
            continue;
        }
        if (g.ip !== undefined) {
            tokens.push({ start: s, end: e, kind: "ip" });
            continue;
        }
        if (g.hex !== undefined) {
            tokens.push({ start: s, end: e, kind: "hex" });
            continue;
        }
        if (g.path !== undefined) {
            tokens.push({ start: s, end: e, kind: "path" });
            continue;
        }
        if (g.str !== undefined) {
            // 따옴표 문자열 안의 레벨 단어("ERROR")는 레거시처럼 레벨 토큰으로 표시한다
            const lv =
                !levelUsed && s < LEVEL_LIMIT
                    ? innerLevelSpan(head, s, e)
                    : null;
            if (lv !== null) {
                levelUsed = true;
                tokens.push(lv);
            } else {
                tokens.push({ start: s, end: e, kind: "string" });
            }
            continue;
        }
        if (g.num !== undefined) {
            // 접근 로그 상태 자리: `" NNN` (따옴표+공백 뒤 100~599) 는 HTTP 상태로 표시
            const kind =
                s >= 2 &&
                head[s - 1] === " " &&
                head[s - 2] === '"' &&
                STATUS_NUM_RE.test(m[0])
                    ? statusKind(m[0])
                    : "number";
            tokens.push({ start: s, end: e, kind });
            continue;
        }
    }

    if (tokenCache.size >= TOKEN_CACHE_MAX) {
        tokenCache.clear();
    }
    tokenCache.set(text, tokens);
    return tokens;
}

export type LineLevel = "error" | "warn" | "info" | "debug" | "trace";

/**
 * 줄의 대표 로그 레벨을 반환합니다 (줄 앞 200자 첫 매치, 레거시 규칙과 동일).
 */
export function lineLevel(text: string): LineLevel | null {
    const m = LEVEL_TOKEN_RE.exec(text.slice(0, LEVEL_LIMIT));
    if (m === null) return null;
    const w = m[0];
    if (w === "ERROR" || w === "FATAL") return "error";
    if (w === "WARN" || w === "WARNING") return "warn";
    if (w === "INFO") return "info";
    return "debug"; // DEBUG, TRACE
}

/** 레벨 단어의 텍스트 색상 클래스 (구문 강조 OFF 시 레거시 렌더와 동일) */
export function levelTextClass(word: string): string {
    if (word === "ERROR" || word === "FATAL") {
        return "text-red-600 dark:text-red-400 font-semibold";
    }
    if (word === "WARN" || word === "WARNING") {
        return "text-amber-600 dark:text-amber-400 font-semibold";
    }
    if (word === "INFO") {
        return "text-blue-600 dark:text-blue-400";
    }
    return "text-zinc-400 dark:text-zinc-500";
}

/** 레벨 단어의 배지 클래스 (구문 강조 ON — 색 글자 + 작은 둥근 배경) */
export function levelBadgeClass(word: string): string {
    if (word === "ERROR" || word === "FATAL") {
        return "text-red-700 dark:text-red-300 bg-red-100 dark:bg-red-900/50 font-semibold rounded-sm px-0.5";
    }
    if (word === "WARN" || word === "WARNING") {
        return "text-amber-700 dark:text-amber-300 bg-amber-100 dark:bg-amber-900/50 font-semibold rounded-sm px-0.5";
    }
    if (word === "INFO") {
        return "text-blue-700 dark:text-blue-300 bg-blue-100 dark:bg-blue-900/40 rounded-sm px-0.5";
    }
    return "text-zinc-500 dark:text-zinc-400 bg-zinc-200/70 dark:bg-zinc-800/70 rounded-sm px-0.5";
}

/** 컴포넌트 이름 해시로 고르는 8색 팔레트 */
const COMPONENT_CLASSES = [
    "text-cyan-700 dark:text-cyan-300",
    "text-violet-700 dark:text-violet-300",
    "text-pink-700 dark:text-pink-300",
    "text-teal-700 dark:text-teal-300",
    "text-orange-700 dark:text-orange-300",
    "text-indigo-700 dark:text-indigo-300",
    "text-lime-700 dark:text-lime-300",
    "text-rose-700 dark:text-rose-300",
];

/** [name] 컴포넌트의 이름 해시 → 팔레트 색상 클래스 */
export function componentClass(name: string): string {
    let h = 0;
    for (let i = 0; i < name.length; i++) {
        h = (h * 31 + name.charCodeAt(i)) | 0;
    }
    return COMPONENT_CLASSES[(h >>> 0) % COMPONENT_CLASSES.length];
}

/** 토큰 종류별 색상 클래스 (level/component는 동적이므로 별도 처리) */
const KIND_CLASSES: Partial<Record<TokenKind, string>> = {
    timestamp: "text-zinc-400 dark:text-zinc-500",
    key: "text-zinc-500 dark:text-zinc-500",
    value: "text-zinc-950 dark:text-zinc-50",
    number: "text-teal-600 dark:text-teal-400",
    ip: "text-sky-600 dark:text-sky-400",
    url: "text-sky-600 dark:text-sky-400",
    path: "text-sky-600 dark:text-sky-400",
    string: "text-emerald-700 dark:text-emerald-400",
    uuid: "text-violet-600 dark:text-violet-400",
    hex: "text-violet-600 dark:text-violet-400",
    httpStatus2xx: "text-green-600 dark:text-green-400",
    httpStatus3xx: "text-sky-600 dark:text-sky-400",
    httpStatus4xx: "text-amber-600 dark:text-amber-400",
    httpStatus5xx: "text-red-600 dark:text-red-400 font-semibold",
};

/**
 * 토큰의 표시 클래스를 반환합니다.
 * syntaxOn=false(레거시)인 level 토큰은 배지 대신 텍스트 색상만 입힙니다.
 */
export function tokenClassName(
    token: Token,
    text: string,
    syntaxOn: boolean,
): string {
    switch (token.kind) {
        case "level": {
            const w = text.slice(token.start, token.end);
            return syntaxOn ? levelBadgeClass(w) : levelTextClass(w);
        }
        case "component":
            return componentClass(text.slice(token.start + 1, token.end - 1));
        default:
            return KIND_CLASSES[token.kind] ?? "";
    }
}

/** 더블클릭 단어의 경계 구분자 (이 문자들 사이가 하나의 단어) */
const WORD_DELIM_RE = /[\s[\]()"',;]/;

function isWordDelim(ch: string): boolean {
    return WORD_DELIM_RE.test(ch);
}

export interface WordSpan {
    start: number;
    end: number;
    word: string;
}

/**
 * 문자 오프셋 위치의 단어를 반환합니다.
 * 구분자([ ] ( ) " ' , ; 와 공백)로 끊긴 토큰 단위 — `key=value`, `a.b.c` 는 통째로 한 단어입니다.
 * 구분자 위나 범위 밖이면 null을 반환합니다.
 */
export function wordAt(text: string, offset: number): WordSpan | null {
    if (offset < 0 || offset >= text.length) return null;
    if (isWordDelim(text[offset])) return null;
    let s = offset;
    let e = offset;
    while (s > 0 && !isWordDelim(text[s - 1])) s--;
    while (e < text.length && !isWordDelim(text[e])) e++;
    const word = text.slice(s, e);
    return word.length > 0 ? { start: s, end: e, word } : null;
}

/**
 * 줄 안에서 단어와 정확히 일치하는 범위 목록을 반환합니다.
 * 양옆이 구분자(또는 줄 끝)인 매치만 인정합니다 — 부분 문자열은 강조하지 않습니다.
 */
export function findWordSpans(text: string, word: string): [number, number][] {
    if (word === "") return [];
    const spans: [number, number][] = [];
    let idx = text.indexOf(word);
    while (idx !== -1) {
        const end = idx + word.length;
        const before = idx === 0 || isWordDelim(text[idx - 1]);
        const after = end >= text.length || isWordDelim(text[end]);
        if (before && after) {
            spans.push([idx, end]);
        }
        idx = text.indexOf(word, idx + 1);
    }
    return spans;
}

// ===== 구문 강조 설정 (모든 탭 공통, localStorage) =====

const SYNTAX_KEY = "riffle.syntaxHighlight";

/** 구문 강조 활성 여부를 읽습니다 (기본 true). */
export function loadSyntaxEnabled(): boolean {
    try {
        const raw = window.localStorage.getItem(SYNTAX_KEY);
        return raw === null ? true : raw === "1";
    } catch {
        return true;
    }
}

/** 구문 강조 활성 여부를 저장합니다. */
export function saveSyntaxEnabled(enabled: boolean): void {
    try {
        window.localStorage.setItem(SYNTAX_KEY, enabled ? "1" : "0");
    } catch {
        // 저장 실패는 무시
    }
}
