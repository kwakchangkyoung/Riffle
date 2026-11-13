import { Channel, invoke } from "@tauri-apps/api/core";

/** 파일을 연 소스 종류 */
export type FileSource = "local" | "sftp" | "s3";

export interface FileInfo {
    id: number;
    path: string;
    size: number;
    compressed: boolean;
    /** 원격 파일 여부 (path는 "sftp://host/path" 또는 "s3://bucket/key" 형태) */
    remote: boolean;
    /** 파일 소스 종류 */
    source: FileSource;
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

// ===== 다중 필터 (Phase 10) =====

export type RuleKind = "include" | "exclude";
export type LevelName = "error" | "warn" | "info" | "debug" | "trace";
export type FilterCombine = "and" | "or";

export interface FilterRule {
    pattern: string;
    regex: boolean;
    caseSensitive: boolean;
    kind: RuleKind;
    enabled: boolean;
    /** 규칙 하이라이트 색상 (#rrggbb). Rust는 판정에 사용하지 않고 프리셋 저장용으로만 보관한다 */
    color: string;
    /** 지정된 컬럼 값에만 규칙을 적용한다 (없으면 줄 전체). 컬럼 이름은 detectColumns 결과의 names */
    field?: string | null;
}

export interface FilterSpec {
    rules: FilterRule[];
    combine: FilterCombine;
    levels: LevelName[];
}

export interface FilterPreset {
    name: string;
    spec: FilterSpec;
}

/**
 * 필터 스펙의 정규식 유효성을 검사합니다.
 * spec에 필드 규칙이 있으면 id 파일의 감지된 컬럼으로 컬럼명도 검증합니다
 * (id가 null이면 컬럼 없음으로 간주해 필드 규칙은 무효 에러가 됩니다).
 */
export function validateFilter(
    id: number | null,
    spec: FilterSpec,
): Promise<void> {
    return invoke<void>("validate_filter", { id, spec });
}

/**
 * 백그라운드 필터 스캔을 시작하고 필터 ID를 반환합니다.
 * 같은 파일의 이전 필터는 자동 취소·제거됩니다.
 */
export function startFilter(
    id: number,
    spec: FilterSpec,
    onEvent: (event: SearchEvent) => void,
): Promise<number> {
    const channel = new Channel<SearchEvent>();
    channel.onmessage = onEvent;
    return invoke<number>("start_filter", { id, spec, onEvent: channel });
}

/**
 * 진행 중인 필터 스캔을 취소합니다.
 */
export function cancelFilter(filterId: number): Promise<void> {
    return invoke<void>("cancel_filter", { filterId });
}

/**
 * 저장된 필터 프리셋 목록을 조회합니다.
 */
export function listFilterPresets(): Promise<FilterPreset[]> {
    return invoke<FilterPreset[]>("list_filter_presets");
}

/**
 * 필터 프리셋을 저장합니다. 같은 이름이 있으면 덮어씁니다.
 */
export function saveFilterPreset(
    name: string,
    spec: FilterSpec,
): Promise<void> {
    return invoke<void>("save_filter_preset", { name, spec });
}

/**
 * 필터 프리셋을 삭제합니다.
 */
export function deleteFilterPreset(name: string): Promise<void> {
    return invoke<void>("delete_filter_preset", { name });
}

export interface Line {
    no: number;
    text: string;
    byteLen: number;
    truncated: boolean;
    highlights: [number, number][];
    /** 필터 규칙 하이라이트 [UTF-16 시작, UTF-16 끝, spec.rules 인덱스] */
    ruleHighlights: [number, number, number][];
    /** 컬럼 뷰용 분할 값. withFields=true이고 형식이 plain이 아닐 때만 채워진다 */
    fields?: string[];
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
    filterId: number | null,
    onEvent: (event: SearchEvent) => void,
): Promise<number> {
    const channel = new Channel<SearchEvent>();
    channel.onmessage = onEvent;
    return invoke<number>("search", {
        id,
        query,
        filterId,
        onEvent: channel,
    });
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
 * withFields=true이고 파일 형식이 plain이 아니면 각 줄의 fields에 컬럼 분할 값이 채워집니다.
 */
export function getLines(
    id: number,
    start: number,
    count: number,
    query?: QueryOptions | null,
    spec?: FilterSpec | null,
    withFields?: boolean,
): Promise<Line[]> {
    return invoke<Line[]>("get_lines", {
        id,
        start,
        count,
        query: query ?? null,
        spec: spec ?? null,
        withFields: withFields ?? null,
    });
}

/**
 * 필터 모드: 검색 결과 목록의 특정 구간에 해당하는 줄들을 조회합니다.
 * withFields=true이고 파일 형식이 plain이 아니면 각 줄의 fields에 컬럼 분할 값이 채워집니다.
 */
export function getFilteredLines(
    id: number,
    searchId: number,
    startIndex: number,
    count: number,
    query?: QueryOptions | null,
    spec?: FilterSpec | null,
    withFields?: boolean,
): Promise<Line[]> {
    return invoke<Line[]>("get_filtered_lines", {
        id,
        searchId,
        startIndex,
        count,
        query: query ?? null,
        spec: spec ?? null,
        withFields: withFields ?? null,
    });
}

/**
 * 필터 결과 목록의 특정 구간에 해당하는 줄들을 조회합니다.
 * withFields=true이고 파일 형식이 plain이 아니면 각 줄의 fields에 컬럼 분할 값이 채워집니다.
 */
export function getFilterLines(
    id: number,
    filterId: number,
    startIndex: number,
    count: number,
    query?: QueryOptions | null,
    spec?: FilterSpec | null,
    withFields?: boolean,
): Promise<Line[]> {
    return invoke<Line[]>("get_filter_lines", {
        id,
        filterId,
        startIndex,
        count,
        query: query ?? null,
        spec: spec ?? null,
        withFields: withFields ?? null,
    });
}

/**
 * 필터 보기에서 실제 줄 번호가 처음 나타나는 필터 인덱스를 조회합니다.
 */
export function filterIndexOf(filterId: number, line: number): Promise<number> {
    return invoke<number>("filter_index_of", { filterId, line });
}

export type TailEvent =
    | {
          type: "appended";
          lineCount: number;
          totalBytes: number;
          oldLineCount: number;
          /** 활성 필터의 현재 줄 수 (필터가 없으면 null) */
          filterCount?: number | null;
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

// ===== 컬럼 뷰 (CSV / JSON Lines, Phase 16) =====

/**
 * 파일의 컬럼 정보 (Rust ColumnsDto와 1:1, camelCase).
 * format이 "plain"이면 구조화된 형식이 아니므로 names는 빈 배열입니다.
 */
export interface Columns {
    /** 감지된 형식: "plain" | "csv" | "jsonl" */
    format: "plain" | "csv" | "jsonl";
    /** CSV 구분자 문자 (plain/jsonl이면 null) */
    delimiter: string | null;
    /** CSV 첫 줄이 헤더로 감지됐는지 여부 */
    hasHeader: boolean;
    /** 컬럼 이름 목록 (표시 순서) */
    names: string[];
}

/**
 * 파일의 컬럼 정보를 조회합니다 (형식 감지, 파일별 캐시).
 */
export function detectColumns(id: number): Promise<Columns> {
    return invoke<Columns>("detect_columns", { id });
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
 * Finder 파일 연결로 보관된 대기 경로 목록을 한 번만 가져옵니다 (콜드 스타트 대응).
 * 반환 후 백엔드에서 비워지므로 같은 경로가 두 번 열리지 않습니다.
 */
export async function takePendingOpen(): Promise<string[]> {
    const value = await invoke<string[] | string | null>("take_pending_open");
    // 구버전 백엔드(단일 경로 문자열)와의 호환을 위해 배열로 정규화한다
    if (value === null) return [];
    return Array.isArray(value) ? value : [value];
}

/**
 * 종료 시 저장된 탭 세션 (열린 탭 경로 목록 + 활성 탭 인덱스).
 */
export interface Session {
    paths: string[];
    activeIndex: number | null;
}

/**
 * 현재 탭 세션을 저장합니다.
 */
export function saveSession(session: Session): Promise<void> {
    return invoke<void>("save_session", { session });
}

/**
 * 저장된 탭 세션을 조회합니다. 복원 설정이 꺼져 있거나 세션이 없으면 null.
 */
export function loadSession(): Promise<Session | null> {
    return invoke<Session | null>("load_session");
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
    /** 시작 시 이전 탭 세션 복원 여부 (기본 true) */
    restoreSession: boolean;
    /** 마지막으로 선택한 AWS 프로필 (없으면 기본 자격 증명 체인) */
    s3Profile: string | null;
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

// ===== 원격 파일 (SSH/SFTP, Phase 12) =====

/** ~/.ssh/config 의 Host 항목 */
export interface SshHost {
    alias: string;
    hostname: string | null;
    user: string | null;
}

/** 원격 디렉터리 목록의 한 항목 */
export interface RemoteEntry {
    name: string;
    isDir: boolean;
    size: number;
    /** 수정 시각 (unix 초), 없으면 null */
    mtime: number | null;
}

/**
 * ~/.ssh/config 의 Host 별칭 목록을 조회합니다.
 */
export function listSshHosts(): Promise<SshHost[]> {
    return invoke<SshHost[]>("list_ssh_hosts");
}

/**
 * 원격 호스트의 디렉터리 목록을 조회합니다 (이름순, 디렉터리 먼저).
 */
export function listRemoteDir(
    host: string,
    path: string,
): Promise<RemoteEntry[]> {
    return invoke<RemoteEntry[]>("list_remote_dir", { host, path });
}

/**
 * 원격 호스트의 홈 디렉터리 절대 경로를 조회합니다.
 */
export function remoteHome(host: string): Promise<string> {
    return invoke<string>("remote_home", { host });
}

// ===== S3 객체 열기 (Phase 13) =====

/** S3 접두사 목록의 한 항목 (폴더 또는 객체) */
export interface S3Entry {
    /** 접두사 기준 상대 이름 (폴더는 끝에 "/" 없음) */
    name: string;
    isDir: boolean;
    size: number;
    /** 수정 시각 (unix 초), 없으면 null */
    mtime: number | null;
}

/**
 * ~/.aws/config, ~/.aws/credentials 에 정의된 프로필 이름 목록을 조회합니다.
 * 키 값 등 다른 내용은 읽지 않습니다.
 */
export function listAwsProfiles(): Promise<string[]> {
    return invoke<string[]>("list_aws_profiles");
}

/**
 * 선택한 프로필(없으면 기본 체인)로 버킷 목록을 조회합니다.
 * ListBuckets 권한이 없으면 에러를 반환합니다.
 */
export function s3ListBuckets(profile: string | null): Promise<string[]> {
    return invoke<string[]>("s3_list_buckets", { profile });
}

/**
 * 버킷의 접두사 아래 항목을 "/" 구분으로 조회합니다 (폴더 먼저, 이름순).
 */
export function s3ListPrefix(
    profile: string | null,
    bucket: string,
    prefix: string,
): Promise<S3Entry[]> {
    return invoke<S3Entry[]>("s3_list_prefix", { profile, bucket, prefix });
}

/**
 * S3 객체의 크기(바이트)를 조회합니다. uri는 "s3://bucket/key" 형식입니다.
 */
export function s3Head(profile: string | null, uri: string): Promise<number> {
    return invoke<number>("s3_head", { profile, uri });
}

/**
 * S3 프로필 선택을 설정에 저장합니다. null이면 기본 자격 증명 체인을 사용합니다.
 */
export function setS3Profile(profile: string | null): Promise<void> {
    return invoke<void>("set_s3_profile", { profile });
}

/**
 * 버킷 즐겨찾기 목록을 조회합니다. 각 항목은 "bucket" 또는 "bucket/prefix/" 형식입니다.
 */
export function getS3Bookmarks(): Promise<string[]> {
    return invoke<string[]>("get_s3_bookmarks");
}

/**
 * 버킷 즐겨찾기 목록을 저장합니다.
 */
export function setS3Bookmarks(items: string[]): Promise<void> {
    return invoke<void>("set_s3_bookmarks", { items });
}

// ===== 줄 북마크 (Phase 15) =====

/** 파일의 한 줄에 붙는 북마크 (Rust BookmarkDto와 1:1, camelCase) */
export interface Bookmark {
    /** 0-based 줄 번호 */
    line: number;
    /** 메모 */
    note: string;
    /** 표시 색상 ("#rrggbb") */
    color: string;
    /** 생성 시각 (unix 초) */
    createdAt: number;
}

/** 북마크 조회 결과 */
export interface Bookmarks {
    items: Bookmark[];
    /** 저장 이후 파일 내용이 바뀌어 북마크 위치가 어긋났을 수 있음 */
    stale: boolean;
}

/**
 * 열린 파일의 북마크 목록을 조회합니다.
 * 저장된 파일 지문과 현재 지문이 다르면 stale=true로 반환됩니다.
 */
export function getBookmarks(id: number): Promise<Bookmarks> {
    return invoke<Bookmarks>("get_bookmarks", { id });
}

/**
 * 열린 파일의 북마크 목록을 저장합니다.
 * 빈 목록이면 해당 파일의 북마크 항목을 삭제합니다.
 */
export function setBookmarks(id: number, items: Bookmark[]): Promise<void> {
    return invoke<void>("set_bookmarks", { id, items });
}
