import { Channel, invoke } from "@tauri-apps/api/core";

export interface FileInfo {
    id: number;
    path: string;
    size: number;
}

export interface IndexProgress {
    indexedBytes: number;
    totalBytes: number;
    lineCount: number;
    done: boolean;
}

export interface QueryOptions {
    query: string;
    regex: boolean;
    caseSensitive: boolean;
}

export interface Line {
    no: number;
    text: string;
    byteLen: number;
    truncated: boolean;
    highlights: [number, number][];
}

export type SearchEvent =
    | {
          type: "progress";
          scannedBytes: number;
          totalBytes: number;
          matchCount: number;
      }
    | { type: "done"; total: number; capped: boolean }
    | { type: "cancelled" }
    | { type: "error"; message: string };

export interface SearchHit {
    index: number;
    line: number;
}

/**
 * 파일을 열고 백그라운드 인덱싱 진행률을 수신합니다.
 */
export function openFile(
    path: string,
    onProgress: (p: IndexProgress) => void,
): Promise<FileInfo> {
    const channel = new Channel<IndexProgress>();
    channel.onmessage = onProgress;
    return invoke<FileInfo>("open_file", { path, onProgress: channel });
}

/**
 * 검색어 문법 유효성을 검사합니다.
 */
export function validateQuery(query: QueryOptions): Promise<void> {
    return invoke<void>("validate_query", { query });
}

/**
 * 백그라운드 검색을 시작하고 검색 ID를 반환합니다.
 */
export function search(
    id: number,
    query: QueryOptions,
    onEvent: (event: SearchEvent) => void,
): Promise<number> {
    const channel = new Channel<SearchEvent>();
    channel.onmessage = onEvent;
    return invoke<number>("search", { id, query, onEvent: channel });
}

/**
 * 진행 중인 검색을 취소합니다.
 */
export function cancelSearch(searchId: number): Promise<void> {
    return invoke<void>("cancel_search", { searchId });
}

/**
 * 현재 줄 번호 기준으로 다음 또는 이전 검색 결과를 탐색합니다.
 */
export function searchNav(
    searchId: number,
    fromLine: number,
    forward: boolean,
): Promise<SearchHit | null> {
    return invoke<SearchHit | null>("search_nav", {
        searchId,
        fromLine,
        forward,
    });
}

/**
 * 결과 목록 내 특정 인덱스의 검색 결과를 조회합니다.
 */
export function searchHitAt(
    searchId: number,
    index: number,
): Promise<SearchHit | null> {
    return invoke<SearchHit | null>("search_hit_at", { searchId, index });
}

/**
 * 스크롤바 미니맵용 검색 밀도(구간별 매치 줄 수)를 조회합니다.
 */
export function searchDensity(
    searchId: number,
    bins: number,
    totalLines: number,
): Promise<number[]> {
    return invoke<number[]>("search_density", {
        searchId,
        bins,
        totalLines,
    });
}

/**
 * 지정된 파일의 특정 범위 줄들을 가져옵니다.
 */
export function getLines(
    id: number,
    start: number,
    count: number,
    query?: QueryOptions | null,
): Promise<Line[]> {
    return invoke<Line[]>("get_lines", {
        id,
        start,
        count,
        query: query ?? null,
    });
}

/**
 * 필터 모드: 검색 결과 목록의 특정 구간에 해당하는 줄들을 조회합니다.
 */
export function getFilteredLines(
    id: number,
    searchId: number,
    startIndex: number,
    count: number,
    query?: QueryOptions | null,
): Promise<Line[]> {
    return invoke<Line[]>("get_filtered_lines", {
        id,
        searchId,
        startIndex,
        count,
        query: query ?? null,
    });
}

export type TailEvent =
    | {
          type: "appended";
          lineCount: number;
          totalBytes: number;
          oldLineCount: number;
      }
    | { type: "rotated"; reason: "truncated" | "replaced" }
    | { type: "error"; message: string };

/**
 * tail 모드(실시간 추적) 감시를 켜거나 끄고 이벤트를 수신합니다.
 */
export function setTail(
    id: number,
    enabled: boolean,
    onEvent: (event: TailEvent) => void,
): Promise<void> {
    const channel = new Channel<TailEvent>();
    channel.onmessage = onEvent;
    return invoke<void>("set_tail", { id, enabled, onEvent: channel });
}

/**
 * 열린 파일을 닫고 자원을 해제합니다.
 */
export function closeFile(id: number): Promise<void> {
    return invoke<void>("close_file", { id });
}
