// 파일 경로별 마지막 스크롤 위치(topLine)를 localStorage에 보관한다.
// 키: riffle.pos.<path>, 인덱스(LRU 순서): riffle.pos.index

const INDEX_KEY = "riffle.pos.index";
const KEY_PREFIX = "riffle.pos.";
const MAX_ENTRIES = 200;

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
 * 파일 경로의 마지막 스크롤 위치(topLine)를 읽는다. 없으면 null.
 */
export function loadScrollPos(path: string): number | null {
    try {
        const raw = window.localStorage.getItem(`${KEY_PREFIX}${path}`);
        if (raw === null) return null;
        const pos = Number(raw);
        return Number.isFinite(pos) && pos > 0 ? pos : null;
    } catch {
        return null;
    }
}

/**
 * 파일 경로의 스크롤 위치(topLine)를 저장하고 LRU 인덱스를 갱신한다.
 * 경로가 MAX_ENTRIES를 넘으면 가장 오래된 항목부터 지운다.
 */
export function saveScrollPos(path: string, topLine: number): void {
    try {
        window.localStorage.setItem(
            `${KEY_PREFIX}${path}`,
            String(Math.floor(topLine)),
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
