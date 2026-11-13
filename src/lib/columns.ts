// 컬럼 뷰의 경로별 레이아웃(컬럼 폭·숨김 상태)을 localStorage에 보관한다.
// 키: riffle.cols.v2.<path>, 인덱스(LRU 순서): riffle.cols.v2.index
// v1(riffle.cols.*)은 기본 폭 추정값까지 통째로 저장해 이후 더 정확한
// 추정이 나와도 무시되는 문제가 있어 키 버전을 올려 구 레이아웃을 무시한다.

const INDEX_KEY = "riffle.cols.v2.index";
const KEY_PREFIX = "riffle.cols.v2.";
const MAX_ENTRIES = 100;

/** 저장되는 컬럼 레이아웃 */
export interface ColumnLayout {
    /**
     * 사용자가 드래그로 직접 정한 컬럼 폭(px). 인덱스별 항목이며
     * null이거나 배열에 없는 인덱스는 "기본 폭 사용"을 뜻한다
     * (기본 폭 추정값 자체는 저장하지 않는다).
     */
    widths: (number | null)[];
    /** 숨긴 컬럼 인덱스 */
    hidden: number[];
}

const EMPTY_LAYOUT: ColumnLayout = { widths: [], hidden: [] };

/** LRU 인덱스(경로 배열, 오래된 것이 앞)를 읽는다. */
function readIndex(): string[] {
    try {
        const raw = window.localStorage.getItem(INDEX_KEY);
        if (raw === null) return [];
        const parsed: unknown = JSON.parse(raw);
        return Array.isArray(parsed)
            ? parsed.filter((p): p is string => typeof p === "string")
            : [];
    } catch {
        return [];
    }
}

function writeIndex(paths: string[]): void {
    try {
        window.localStorage.setItem(INDEX_KEY, JSON.stringify(paths));
    } catch {
        // 저장 실패는 무시
    }
}

/**
 * 파일 경로의 저장된 컬럼 레이아웃을 읽는다. 없거나 파싱 실패면 빈 레이아웃.
 */
export function loadColumnLayout(path: string): ColumnLayout {
    try {
        const raw = window.localStorage.getItem(`${KEY_PREFIX}${path}`);
        if (raw === null) return EMPTY_LAYOUT;
        const parsed: unknown = JSON.parse(raw);
        if (typeof parsed !== "object" || parsed === null) return EMPTY_LAYOUT;
        const obj = parsed as { widths?: unknown; hidden?: unknown };
        const widths = Array.isArray(obj.widths)
            ? obj.widths.map((w) => (typeof w === "number" && w > 0 ? w : null))
            : [];
        const hidden = Array.isArray(obj.hidden)
            ? obj.hidden.filter(
                  (h): h is number => Number.isInteger(h) && h >= 0,
              )
            : [];
        return { widths, hidden };
    } catch {
        return EMPTY_LAYOUT;
    }
}

/**
 * 파일 경로의 컬럼 레이아웃을 저장하고 LRU 인덱스를 갱신한다.
 * 경로가 MAX_ENTRIES를 넘으면 가장 오래된 항목부터 지운다.
 */
export function saveColumnLayout(path: string, layout: ColumnLayout): void {
    try {
        window.localStorage.setItem(
            `${KEY_PREFIX}${path}`,
            JSON.stringify(layout),
        );
        const index = readIndex().filter((p) => p !== path);
        index.push(path);
        while (index.length > MAX_ENTRIES) {
            const evicted = index.shift();
            if (evicted !== undefined) {
                window.localStorage.removeItem(`${KEY_PREFIX}${evicted}`);
            }
        }
        writeIndex(index);
    } catch {
        // 저장 실패는 무시
    }
}
