/**
 * 로그 줄 안의 JSON 페이로드 감지·파싱.
 * 화면에 표시된 줄 텍스트만 다룬다 (파일 전체를 읽지 않는다).
 */

export type JsonValue =
    | string
    | number
    | boolean
    | null
    | JsonValue[]
    | { [key: string]: JsonValue };

/** 파싱 결과 캐시 (거터 아이콘/패널에서 같은 줄 텍스트를 반복 검사하므로) */
const parseCache = new Map<string, JsonValue | null>();
const PARSE_CACHE_MAX = 500;

/**
 * 줄에서 첫 번째 `{` 또는 `[` 부터 끝까지를 JSON으로 파싱합니다.
 * 성공하면 파싱된 값, 아니면(구분자가 없거나 파싱 실패) null을 반환합니다.
 */
export function parseLineJson(text: string): JsonValue | null {
    if (parseCache.has(text)) {
        return parseCache.get(text) ?? null;
    }
    const value = parseSuffix(text);
    if (parseCache.size >= PARSE_CACHE_MAX) {
        parseCache.clear();
    }
    parseCache.set(text, value);
    return value;
}

/** 시작 위치 후보를 시도하는 최대 횟수 (긴 줄에서 파싱을 반복하지 않도록) */
const MAX_START_TRIES = 8;

function parseSuffix(text: string): JsonValue | null {
    // JSON 페이로드는 줄 끝까지 이어지므로 끝 글자로 먼저 걸러낸다
    const trimmed = text.trimEnd();
    const last = trimmed[trimmed.length - 1];
    if (last !== "}" && last !== "]") return null;
    // `[api] request done {...}` 처럼 앞쪽 괄호가 JSON 이 아닐 수 있으므로
    // `{` / `[` 위치를 왼쪽부터 차례로 시도한다
    let tries = 0;
    for (let i = 0; i < trimmed.length && tries < MAX_START_TRIES; i++) {
        const c = trimmed[i];
        if (c !== "{" && c !== "[") continue;
        tries++;
        try {
            return JSON.parse(trimmed.slice(i)) as JsonValue;
        } catch {
            // 다음 후보 위치로
        }
    }
    return null;
}
