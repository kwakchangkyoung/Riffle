import { Channel, invoke } from "@tauri-apps/api/core";

export interface FileInfo {
    id: number;
    path: string;
    size: number;
    compressed: boolean;
}

export interface IndexProgress {
    indexedBytes: number;
    totalBytes: number;
    lineCount: number;
    done: boolean;
    contentBytes: number;
    decompressing: boolean;
    error: string | null;
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

export interface CopyResult {
    lines: number;
    bytes: number;
}

/**
 * 줄 범위 [start, end] (0-based, 양끝 포함)의 원문을 클립보드에 복사합니다.
 */
export function copyLines(
    id: number,
    start: number,
    end: number,
): Promise<CopyResult> {
    return invoke<CopyResult>("copy_lines", { id, start, end });
}

export interface FullLine {
    no: number;
    text: string;
    truncated: boolean;
    byteLen: number;
}

/**
 * 잘리지 않은 원문 전체 줄을 조회합니다. 너무 긴 줄은 잘려서 반환됩니다.
 */
export function getFullLine(id: number, no: number): Promise<FullLine | null> {
    return invoke<FullLine | null>("get_full_line", { id, no });
}

/**
 * 최근에 열었던 파일 경로 목록을 조회합니다.
 */
export function getRecentFiles(): Promise<string[]> {
    return invoke<string[]>("get_recent_files");
}

/**
 * Finder 파일 연결로 보관된 대기 경로를 한 번만 가져옵니다 (콜드 스타트 대응).
 * 반환 후 백엔드에서 비워지므로 같은 경로가 두 번 열리지 않습니다.
 */
export function takePendingOpen(): Promise<string | null> {
    return invoke<string | null>("take_pending_open");
}

/**
 * 네이티브 메뉴에서 발생한 이벤트 페이로드.
 * recent 항목은 id가 "open_recent"이고 path에 파일 경로가 담깁니다.
 */
export interface MenuEvent {
    id: string;
    path?: string;
}

/**
 * 앱 설정 (Rust Settings와 1:1).
 */
export interface Settings {
    autoCheckUpdates: boolean;
}

/**
 * 현재 앱 설정을 조회합니다.
 */
export function getSettings(): Promise<Settings> {
    return invoke<Settings>("get_settings");
}

/**
 * 시작 시 업데이트 자동 확인 설정을 변경합니다.
 */
export function setAutoCheckUpdates(enabled: boolean): Promise<void> {
    return invoke<void>("set_auto_check_updates", { enabled });
}
