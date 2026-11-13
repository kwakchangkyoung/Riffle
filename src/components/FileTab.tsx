import React, {
    useCallback,
    useEffect,
    useMemo,
    useRef,
    useState,
} from "react";
import { listen, type Event as TauriEvent } from "@tauri-apps/api/event";
import {
    cancelFilter,
    closeFile,
    copyLines,
    detectColumns,
    filterIndexOf,
    getBookmarks,
    mergePositionAtTime,
    mergeSources,
    openFile,
    openMerge,
    searchDensity,
    searchNav,
    setBookmarks as saveBookmarksIpc,
    setTail,
    startFilter,
    type Bookmark,
    type Columns,
    type FileInfo,
    type FilterSpec,
    type IndexProgress,
    type MenuEvent,
    type MergeSource,
    type QueryOptions,
    type RuleKind,
    type SearchEvent,
    type SearchHit,
    type TailEvent,
} from "@/lib/ipc";
import { isMergePath, mergeShortName, mergeSourceColor } from "@/lib/merge";
import { LineCache } from "@/lib/lineCache";
import { useTauriListener } from "@/lib/useTauriListener";
import { loadScrollPos, saveScrollPos } from "@/lib/scrollPos";
import { rowHeightFor } from "@/lib/fontSize";
import { formatBytes } from "@/lib/format";
import { DEFAULT_BOOKMARK_COLOR } from "@/lib/bookmarks";
import { LogViewport } from "@/components/viewer/LogViewport";
import { FullLinePanel } from "@/components/viewer/FullLinePanel";
import { BookmarkPanel } from "@/components/bookmarks/BookmarkPanel";
import { GotoLineDialog } from "@/components/GotoLineDialog";
import { SearchBar } from "@/components/search/SearchBar";
import { FilterBar } from "@/components/filter/FilterBar";
import { pickRuleColor } from "@/lib/ruleColor";
import {
    StatusBar,
    type FilterStatusInfo,
    type SearchStatusInfo,
} from "@/components/StatusBar";

/** 필터 조건이 하나도 없는 초기 스펙 (레벨 전체 선택 = 조건 없음) */
const EMPTY_FILTER_SPEC: FilterSpec = {
    rules: [],
    combine: "or",
    levels: ["error", "warn", "info", "debug", "trace"],
};

/** 적용할 필터 조건(켜진 규칙 또는 레벨 부분 집합)이 있는지 확인 */
function specHasCondition(spec: FilterSpec): boolean {
    const hasEnabledRule = spec.rules.some((r) => r.enabled);
    const hasLevelCondition = spec.levels.length > 0 && spec.levels.length < 5;
    return hasEnabledRule || hasLevelCondition;
}

/**
 * 필터 판정과 무관한 필드(규칙 색상)를 제외한 스펙 서명.
 * 색상만 바뀐 변경으로 전체 재스캔이 일어나지 않게 비교에 사용한다.
 */
function filterSignature(spec: FilterSpec): string {
    return JSON.stringify({
        combine: spec.combine,
        levels: spec.levels,
        rules: spec.rules.map((r) => [
            r.pattern,
            r.regex,
            r.caseSensitive,
            r.kind,
            r.enabled,
            r.field ?? null,
        ]),
    });
}

/** 규칙 하이라이트를 계산할 필요가 있는 스펙이면 반환, 아니면 null */
function ruleHighlightSpec(spec: FilterSpec): FilterSpec | null {
    return spec.rules.some((r) => r.enabled && r.kind === "include")
        ? spec
        : null;
}

/** 문자열을 정규식 리터럴로 이스케이프한다 (필드 필터의 값 매치용) */
function escapeRegex(s: string): string {
    return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

interface FileTabProps {
    /** 탭 ID (App의 탭 목록과 대응) */
    tabId: string;
    /** 이 탭이 열 경로 (사용자가 요청한 경로). 병합 탭은 "merge://<tabId>" (표시용) */
    path: string;
    /** 병합 탭이면 원본 파일 id 목록 — null이 아니면 openFile 대신 openMerge로 연다 */
    mergeSourceIds?: number[] | null;
    /** 활성 탭 여부 — false이면 화면을 숨기고 단축키/메뉴 명령을 무시한다 */
    isActive: boolean;
    /** 전역 글자 크기 (App 상태) */
    fontSize: number;
    /** 파일 열기 완료 시 FileInfo 보고 (탭 제목/중복 경로 판정용) */
    onFileInfo: (tabId: string, info: FileInfo) => void;
    /** 인덱싱/해제/병합 구축 진행률 보고 (탭 진행률 표시용) */
    onProgress: (tabId: string, progress: IndexProgress | null) => void;
    /** tail 모드 상태 보고 (탭 표시용) */
    onTailChange: (tabId: string, enabled: boolean) => void;
    /** 글자 크기 조절 (+1/-1 단계) — 트랙패드 핀치 줌 */
    onFontSizeAdjust?: (delta: number) => void;
    /** 병합 탭에서 원본 탭으로 이동 요청 (원본 경로, 0-based 원본 줄) */
    onJumpToSource?: (path: string, sourceLine: number) => void;
    /** App이 요청한 줄 이동 (병합 탭에서 이 탭으로 점프할 때). key는 재요청 구분용 */
    gotoRequest?: { line: number; key: number } | null;
}

/**
 * 단일 파일 탭 컴포넌트.
 * 마운트 시 path의 파일을 열고, 열기/인덱싱/검색/필터/tail/뷰어 상태를 모두 소유한다.
 * 비활성 탭도 마운트된 채 숨겨지므로 백그라운드 작업은 계속 진행된다.
 */
export const FileTab: React.FC<FileTabProps> = React.memo(
    ({
        tabId,
        path,
        mergeSourceIds = null,
        isActive,
        fontSize,
        onFileInfo,
        onProgress,
        onTailChange,
        onFontSizeAdjust,
        onJumpToSource,
        gotoRequest = null,
    }) => {
        const [file, setFile] = useState<FileInfo | null>(null);
        // tail appended 이벤트의 totalBytes로 갱신되는 표시용 파일 크기 (null이면 file.size 사용)
        const [liveFileSize, setLiveFileSize] = useState<number | null>(null);
        const [lineCount, setLineCount] = useState(0);
        const [indexProgress, setIndexProgress] =
            useState<IndexProgress | null>(null);
        const [topLine, setTopLine] = useState(0);
        const [errorMessage, setErrorMessage] = useState<string | null>(null);

        // 검색 및 필터 모드 관련 상태
        const [isFilterMode, setIsFilterMode] = useState(false);
        const [searchStatus, setSearchStatus] =
            useState<SearchStatusInfo | null>(null);
        const [currentHit, setCurrentHit] = useState<SearchHit | null>(null);
        const [densityBins, setDensityBins] = useState<number[]>([]);
        const [viewportHeight, setViewportHeight] = useState(0);

        // 다중 필터 상태: 스펙(탭별 유지), 최신/표시 필터 ID, 계산 진행률
        const [filterSpec, setFilterSpec] =
            useState<FilterSpec>(EMPTY_FILTER_SPEC);
        const [filterId, setFilterId] = useState<number | null>(null);
        const [activeFilterId, setActiveFilterId] = useState<number | null>(
            null,
        );
        const [filterStatus, setFilterStatus] =
            useState<FilterStatusInfo | null>(null);
        const [filterMatchCount, setFilterMatchCount] = useState(0);
        const [filterFocusSignal, setFilterFocusSignal] = useState(0);

        // tail 모드 상태: 사용자 의도, 일시정지 중 새 줄 수, 로테이션 안내
        const [tailEnabled, setTailEnabled] = useState(false);
        const [newLinesCount, setNewLinesCount] = useState(0);
        const [rotationReason, setRotationReason] = useState<
            "truncated" | "replaced" | null
        >(null);

        // Phase 5 상태: 줄 단위 선택, 복사, 줄 이동, 전체 줄 패널
        const [lineSelection, setLineSelection] = useState<{
            anchor: number;
            focus: number;
        } | null>(null);
        const [copying, setCopying] = useState(false);
        const [toast, setToast] = useState<string | null>(null);
        const [gotoLineOpen, setGotoLineOpen] = useState(false);
        const [fullLinePanel, setFullLinePanel] = useState<{
            fileId: number;
            no: number;
        } | null>(null);
        const [findSignal, setFindSignal] = useState(0);

        // 북마크 상태 (Phase 15): 항목 목록(줄 번호 오름차순), stale 안내, 사이드 패널
        const [bookmarks, setBookmarkItems] = useState<Bookmark[]>([]);
        const [bookmarksStale, setBookmarksStale] = useState(false);
        const [bookmarkPanelOpen, setBookmarkPanelOpen] = useState(false);

        // 컬럼 뷰 상태 (Phase 16): 감지된 컬럼 정보와 탭별 컬럼 보기 토글.
        // columnView는 파일을 바꿔 열어도 탭 안에서 유지된다 (단, plain이면 비활성).
        const [columns, setColumns] = useState<Columns | null>(null);
        const [columnView, setColumnView] = useState(false);

        // 병합 뷰 상태 (Phase 17): 원본 목록(순번·경로·줄 수). 병합 탭이 아니면 빈 배열.
        const [mergeSourceList, setMergeSourceList] = useState<MergeSource[]>(
            [],
        );

        const rowHeight = rowHeightFor(fontSize);
        // 최신 rowHeight를 ref로 유지해 안정적인 콜백(tail 핸들러 등)에서 참조한다
        const rowHeightRef = useRef(rowHeight);
        useEffect(() => {
            rowHeightRef.current = rowHeight;
        }, [rowHeight]);

        const normalLineCache = useMemo(() => new LineCache(), []);
        const filteredLineCache = useMemo(() => new LineCache(), []);
        const filterLineCache = useMemo(() => new LineCache(), []);

        const [, setCacheVersion] = useState(0);
        useEffect(() => {
            const u1 = normalLineCache.subscribe(() =>
                setCacheVersion((v) => v + 1),
            );
            const u2 = filteredLineCache.subscribe(() =>
                setCacheVersion((v) => v + 1),
            );
            const u3 = filterLineCache.subscribe(() =>
                setCacheVersion((v) => v + 1),
            );
            return () => {
                u1();
                u2();
                u3();
            };
        }, [normalLineCache, filteredLineCache, filterLineCache]);

        const currentFileRef = useRef<FileInfo | null>(null);
        const openSeqRef = useRef(0);
        const latestProgressRef = useRef<IndexProgress | null>(null);
        // 진행률 이벤트로 이미 표시한 해제 오류 (같은 오류 재표시 방지)
        const lastProgressErrorRef = useRef<string | null>(null);
        const lineCountRef = useRef(0);
        useEffect(() => {
            lineCountRef.current = lineCount;
        }, [lineCount]);

        const trackHeightRef = useRef(0);
        const lastDensityTimeRef = useRef(0);
        const densityTimerRef = useRef<ReturnType<typeof setTimeout> | null>(
            null,
        );
        const activeSearchIdRef = useRef<number | null>(null);
        // Done 이벤트가 Started보다 먼저 도착한 경우를 대비한 임시 보관
        const searchDoneRef = useRef<{
            searchId: number;
            total: number;
            capped: boolean;
        } | null>(null);
        const topLineRef = useRef(0);
        // 탭별 스크롤 위치 기억: 복원 대기 위치와 복원 완료 여부 (완료 전엔 저장하지 않는다)
        const pendingPosRef = useRef<number | null>(null);
        const restoreSettledRef = useRef(false);

        // 북마크 관련 ref: 비동기/이벤트 콜백에서 최신 값을 읽기 위한 미러와 저장 예약 상태
        const bookmarksRef = useRef<Bookmark[]>([]);
        const bookmarksDirtyRef = useRef(false);
        const bookmarkSaveTimerRef = useRef<ReturnType<
            typeof setTimeout
        > | null>(null);
        const currentHitRef = useRef<SearchHit | null>(null);
        useEffect(() => {
            bookmarksRef.current = bookmarks;
        }, [bookmarks]);

        // tail 모드 관련 ref
        const tailEnabledRef = useRef(false);
        const pendingAppendedRef = useRef<{
            oldLineCount: number;
            lineCount: number;
            totalBytes: number;
            filterCount?: number | null;
        } | null>(null);
        const tailRafRef = useRef<number | null>(null);
        const atBottomRef = useRef(true);
        const isFilterModeRef = useRef(false);
        const viewportHeightRef = useRef(0);
        const scrollToBottomAfterReopenRef = useRef(false);

        // 다중 필터 관련 ref: 비동기 콜백에서 최신 값을 읽기 위해 state와 병행 유지
        const filterIdRef = useRef<number | null>(null); // 최신 start_filter 결과 (검색 스코프용)
        const activeFilterIdRef = useRef<number | null>(null); // 화면에 표시 중인 필터
        const filterViewActiveRef = useRef(false);
        const filterSeqRef = useRef(0); // 필터 작업 세대 (stale 이벤트 폐기용)
        const runningFilterKeyRef = useRef(""); // 현재 계산 중/완료된 스펙 서명
        const pendingPromoteRef = useRef<{
            fid: number;
            fileId: number;
            count: number;
            done: boolean;
        } | null>(null); // 첫 결과 도착 전 최신 건수 보관
        const promoteInFlightRef = useRef<number | null>(null);
        const filterSpecRef = useRef(filterSpec);
        const activeQueryRef = useRef<QueryOptions | null>(null); // 검색 하이라이트용 현재 쿼리
        const filterCountRef = useRef(0); // 표시 중 필터의 마지막 확인 줄 수 (tail 무효화용)

        // App 콜백은 ref로 미러링해 내부 핸들러의 의존성을 안정적으로 유지한다
        const onFileInfoRef = useRef(onFileInfo);
        const onProgressRef = useRef(onProgress);
        const onTailChangeRef = useRef(onTailChange);
        const onJumpToSourceRef = useRef(onJumpToSource);
        useEffect(() => {
            onFileInfoRef.current = onFileInfo;
            onProgressRef.current = onProgress;
            onTailChangeRef.current = onTailChange;
            onJumpToSourceRef.current = onJumpToSource;
        });

        // 활성 탭 여부를 ref로 미러링해 window 이벤트/메뉴 핸들러에서 참조한다
        const isActiveRef = useRef(isActive);
        useEffect(() => {
            isActiveRef.current = isActive;
        }, [isActive]);

        useEffect(() => {
            isFilterModeRef.current = isFilterMode;
        }, [isFilterMode]);

        useEffect(() => {
            filterSpecRef.current = filterSpec;
        }, [filterSpec]);

        useEffect(() => {
            filterIdRef.current = filterId;
        }, [filterId]);

        useEffect(() => {
            activeFilterIdRef.current = activeFilterId;
            filterViewActiveRef.current = activeFilterId !== null;
        }, [activeFilterId]);

        // 규칙 하이라이트 스펙을 일반/검색 결과 캐시에 전파한다.
        // 색상만 바뀐 경우 하이라이트 범위는 동일하므로 서명이 같으면 재요청하지 않는다.
        const lastRuleSpecSigRef = useRef("");
        useEffect(() => {
            const sig = filterSignature(filterSpec);
            if (sig === lastRuleSpecSigRef.current) {
                return;
            }
            lastRuleSpecSigRef.current = sig;
            const hs = ruleHighlightSpec(filterSpec);
            normalLineCache.setRuleSpec(hs);
            filteredLineCache.setRuleSpec(hs);
        }, [filterSpec, normalLineCache, filteredLineCache]);

        useEffect(() => {
            viewportHeightRef.current = viewportHeight;
        }, [viewportHeight]);

        // 다중 필터 보기 활성 여부 (화면에 표시 중인 필터가 있을 때)
        const filterViewActive = activeFilterId !== null;

        // 컬럼 뷰 활성 여부 (토글 on + 감지 형식이 plain이 아닐 때만)
        const columnViewActive =
            columnView && columns !== null && columns.format !== "plain";

        // 컬럼 뷰가 켜지면 모든 라인 캐시가 줄 분할 값(fields)을 함께 요청하도록 한다
        useEffect(() => {
            normalLineCache.setWithFields(columnViewActive);
            filteredLineCache.setWithFields(columnViewActive);
            filterLineCache.setWithFields(columnViewActive);
        }, [
            columnViewActive,
            normalLineCache,
            filteredLineCache,
            filterLineCache,
        ]);

        // 단축키 핸들러에서 최신 컬럼 정보를 읽기 위한 ref
        const columnsRef = useRef<Columns | null>(null);
        useEffect(() => {
            columnsRef.current = columns;
        }, [columns]);

        // 컬럼 뷰 토글 (툴바 버튼 / Cmd+Shift+C). plain 파일에서는 무시한다
        const handleToggleColumnView = useCallback(() => {
            const cols = columnsRef.current;
            if (cols === null || cols.format === "plain") return;
            setColumnView((v) => !v);
        }, []);

        // 활성 라인 캐시 및 총 라인 수 (검색 결과만 / 다중 필터 / 일반 모드 분기)
        const activeLineCache = isFilterMode
            ? filteredLineCache
            : filterViewActive
              ? filterLineCache
              : normalLineCache;
        const activeLineCount = isFilterMode
            ? (searchStatus?.matchCount ?? 0)
            : filterViewActive
              ? filterMatchCount
              : lineCount;

        // 표시 줄 수 (검색 결과만: 매치 건수, 다중 필터: 필터 통과 건수, 일반: 파일 줄 수)
        const displayLineCount = activeLineCount;
        const displayCountRef = useRef(0);
        useEffect(() => {
            displayCountRef.current = displayLineCount;
        }, [displayLineCount]);

        // 뷰포트 topLine 변경 래퍼: 맨 아래 여부를 즉시 갱신하고 도달 시 새 줄 카운터를 리셋한다.
        // rowHeight 등은 ref로 읽어 콜백 identity가 바뀌지 않게 한다 (tail 재등록 방지)
        const handleTopLineChange = useCallback((v: number) => {
            topLineRef.current = v;
            setTopLine(v);
            const maxTop = Math.max(
                0,
                displayCountRef.current -
                    viewportHeightRef.current / rowHeightRef.current,
            );
            const atBottom = v >= maxTop - 0.5;
            atBottomRef.current = atBottom;
            if (atBottom) {
                setNewLinesCount(0);
            }
        }, []);

        // 뷰포트 행 인덱스의 실제 파일 줄 번호.
        // 인덱스 기반 보기(검색 결과만/다중 필터)에서는 캐시된 줄의 실제 번호를 반환한다.
        const actualLineAtIndex = useCallback(
            (index: number): number => {
                const i = Math.max(0, Math.floor(index));
                if (isFilterModeRef.current) {
                    return filteredLineCache.getLine(i)?.no ?? i;
                }
                if (filterViewActiveRef.current) {
                    return filterLineCache.getLine(i)?.no ?? i;
                }
                return i;
            },
            [filteredLineCache, filterLineCache],
        );

        // 뷰포트 맨 위 행의 실제 파일 줄 번호.
        const currentTopActualLine = useCallback(
            (): number => actualLineAtIndex(topLineRef.current),
            [actualLineAtIndex],
        );

        // topLine 직접 변경(setTopLine) 경로에 대한 ref 동기화 백업
        useEffect(() => {
            topLineRef.current = topLine;
        }, [topLine]);

        // 맨 아래 여부 추적 (줄 수/뷰포트 변화에 대한 백업 갱신)
        // 숨겨진 탭은 뷰포트 높이가 0이 되므로 마지막 값을 유지한다
        // (tail 모드가 백그라운드에서도 맨 아래를 따라가게 하기 위함)
        useEffect(() => {
            if (viewportHeight <= 0) return;
            const maxTop = Math.max(
                0,
                displayLineCount - viewportHeight / rowHeight,
            );
            atBottomRef.current = topLine >= maxTop - 0.5;
        }, [topLine, displayLineCount, viewportHeight, rowHeight]);

        // 기억된 스크롤 위치 적용: 인덱싱이 해당 위치까지 진행되거나 끝나면 한 번만 적용한다.
        // 파일이 그 사이 줄어 위치가 범위를 벗어나면 maxTop에 clamp된다.
        useEffect(() => {
            const pending = pendingPosRef.current;
            if (pending === null) return;
            const maxTop = Math.max(0, lineCount - viewportHeight / rowHeight);
            if (pending <= maxTop || indexProgress?.done === true) {
                pendingPosRef.current = null;
                restoreSettledRef.current = true;
                handleTopLineChange(Math.min(pending, maxTop));
            }
        }, [
            lineCount,
            indexProgress?.done,
            viewportHeight,
            rowHeight,
            handleTopLineChange,
        ]);

        // 스크롤 위치 저장: 복원이 끝난 뒤 topLine 변경을 디바운스로 기록한다 (병합 탭 제외)
        useEffect(() => {
            if (
                file === null ||
                !restoreSettledRef.current ||
                isMergePath(file.path)
            ) {
                return;
            }
            const path = file.path;
            const timer = setTimeout(() => {
                saveScrollPos(path, topLine);
            }, 400);
            return () => clearTimeout(timer);
        }, [file, topLine]);

        // tail IPC 커맨드는 순차 실행해 enable/disable 순서가 뒤바뀌지 않게 한다
        const tailQueueRef = useRef<Promise<void>>(Promise.resolve());
        const queueTailCommand = useCallback(
            <T,>(cmd: () => Promise<T>): Promise<T> => {
                const result = tailQueueRef.current.then(cmd, cmd);
                tailQueueRef.current = result.then(
                    () => {},
                    () => {},
                );
                return result;
            },
            [],
        );

        // 대기 중인 북마크 저장을 즉시 실행한다 (디바운스 타이머 취소 포함).
        // 저장이 성공하면 백엔드가 현재 지문으로 갱신하므로 stale 안내도 함께 해제한다.
        const flushBookmarks = useCallback(async () => {
            if (bookmarkSaveTimerRef.current !== null) {
                clearTimeout(bookmarkSaveTimerRef.current);
                bookmarkSaveTimerRef.current = null;
            }
            if (!bookmarksDirtyRef.current) return;
            bookmarksDirtyRef.current = false;
            const current = currentFileRef.current;
            if (current === null) return;
            try {
                await saveBookmarksIpc(current.id, bookmarksRef.current);
                setBookmarksStale(false);
            } catch (err) {
                // 실패 시 dirty를 되돌려 다음 flush(탭 닫기 등)에서 재시도한다
                bookmarksDirtyRef.current = true;
                console.warn("북마크 저장 실패:", err);
            }
        }, []);

        // 북마크 변경을 300ms 디바운스로 저장 예약한다
        const scheduleBookmarkSave = useCallback(() => {
            bookmarksDirtyRef.current = true;
            if (bookmarkSaveTimerRef.current !== null) {
                clearTimeout(bookmarkSaveTimerRef.current);
            }
            bookmarkSaveTimerRef.current = setTimeout(() => {
                bookmarkSaveTimerRef.current = null;
                void flushBookmarks();
            }, 300);
        }, [flushBookmarks]);

        // 스크롤바 트랙 높이 변경 콜백 (인라인 함수면 memoized LogViewport이 무력화된다)
        const handleTrackHeightChange = useCallback((h: number) => {
            trackHeightRef.current = h;
        }, []);

        // 파일 열기 처리 함수
        const handleOpenFileByPath = useCallback(
            async (openPath: string, options?: { preserveTail?: boolean }) => {
                const seq = ++openSeqRef.current;
                setErrorMessage(null);
                latestProgressRef.current = null;
                lastProgressErrorRef.current = null;
                pendingPosRef.current = null;
                restoreSettledRef.current = false;
                onProgressRef.current(tabId, null);

                // 진행 중이던 tail appended 배치 취소 (이전 파일 기준 이벤트는 폐기)
                pendingAppendedRef.current = null;
                if (tailRafRef.current !== null) {
                    cancelAnimationFrame(tailRafRef.current);
                    tailRafRef.current = null;
                }

                // 대기 중인 북마크 저장이 남아 있으면 이전 파일 id로 즉시 flush한다.
                // 같은 경로 재오픈(로테이션)이어도 항목이 유실되지 않게 닫기 전에 보낸다.
                await flushBookmarks();

                // 이전 파일이 열려있다면 닫기 요청
                if (currentFileRef.current) {
                    const prevId = currentFileRef.current.id;
                    closeFile(prevId).catch((err) => {
                        console.warn("이전 파일 닫기 실패:", err);
                    });
                }

                // 병합 원본 목록 초기화 (열기 성공 시 다시 채운다)
                setMergeSourceList([]);

                // 캐시 및 화면 상태 초기화
                normalLineCache.setFile(null);
                filteredLineCache.setFile(null);
                filterLineCache.setFile(null);
                setFile(null);
                setLiveFileSize(null);
                setLineCount(0);
                setIndexProgress(null);
                setTopLine(0);
                setIsFilterMode(false);
                setSearchStatus(null);
                setCurrentHit(null);
                setDensityBins([]);
                activeSearchIdRef.current = null;
                searchDoneRef.current = null;
                setNewLinesCount(0);
                setLineSelection(null);
                setFullLinePanel(null);
                setGotoLineOpen(false);

                // 북마크 상태 초기화 (저장된 목록은 파일이 열리면 다시 불러온다)
                bookmarksDirtyRef.current = false;
                bookmarksRef.current = [];
                setBookmarkItems([]);
                setBookmarksStale(false);
                setBookmarkPanelOpen(false);

                // 컬럼 감지 결과 초기화 (토글 자체는 탭 안에서 유지)
                setColumns(null);

                // 다중 필터 계산 상태 초기화 (스펙 자체는 탭 내에서 유지)
                filterSeqRef.current++;
                runningFilterKeyRef.current = "";
                pendingPromoteRef.current = null;
                promoteInFlightRef.current = null;
                const prevFilterId = filterIdRef.current;
                filterIdRef.current = null;
                activeFilterIdRef.current = null;
                filterViewActiveRef.current = false;
                filterCountRef.current = 0;
                setFilterId(null);
                setActiveFilterId(null);
                setFilterStatus(null);
                setFilterMatchCount(0);
                if (prevFilterId !== null) {
                    cancelFilter(prevFilterId).catch(() => {});
                }

                // 파일 전환 시 tail은 자동 해제 (로테이션 재오픈 시에만 유지)
                if (!options?.preserveTail) {
                    tailEnabledRef.current = false;
                    setTailEnabled(false);
                    setRotationReason(null);
                }

                try {
                    const onOpenProgress = (progress: IndexProgress) => {
                        // 최신 열기 작업이 아닌 경우 콜백 무시
                        if (openSeqRef.current !== seq) return;
                        latestProgressRef.current = progress;
                        setIndexProgress(progress);
                        setLineCount(progress.lineCount);
                        onProgressRef.current(tabId, progress);
                        normalLineCache.setLineCount(
                            progress.lineCount,
                            progress.done,
                        );
                        // gzip 해제 실패/병합 구축 실패 등: 그 전까지 확정된 내용은 계속 사용 가능
                        if (
                            progress.error !== null &&
                            progress.error !== lastProgressErrorRef.current
                        ) {
                            lastProgressErrorRef.current = progress.error;
                            setErrorMessage(
                                mergeSourceIds !== null
                                    ? `병합 구축 실패: ${progress.error}`
                                    : `압축 해제 실패: ${progress.error} (그때까지 해제된 내용은 볼 수 있습니다)`,
                            );
                        }
                    };
                    // 병합 탭은 openFile이 아니라 openMerge로 가상 파일을 연다
                    const info =
                        mergeSourceIds !== null
                            ? await openMerge(mergeSourceIds, onOpenProgress)
                            : await openFile(openPath, onOpenProgress);

                    // 열리는 도중 다른 파일 열기 요청이 시작되었으면 취소
                    if (openSeqRef.current !== seq) {
                        closeFile(info.id).catch(() => {});
                        return;
                    }

                    currentFileRef.current = info;
                    setFile(info);
                    normalLineCache.setFile(info.id);
                    onFileInfoRef.current(tabId, info);

                    if (info.source === "merge") {
                        // 병합 탭: 원본 목록(칩/범례용)을 가져온다. 컬럼 감지·북마크는 지원하지 않는다
                        mergeSources(info.id)
                            .then((srcs) => {
                                if (openSeqRef.current !== seq) return;
                                setMergeSourceList(srcs);
                            })
                            .catch((err) => {
                                console.warn("병합 원본 목록 조회 실패:", err);
                            });
                    } else {
                        // 인덱싱 시작과 함께 컬럼 형식을 감지한다 (파일별 캐시, 실패해도 뷰어 동작에 지장 없음)
                        detectColumns(info.id)
                            .then((cols) => {
                                if (openSeqRef.current !== seq) return;
                                setColumns(cols);
                            })
                            .catch((err) => {
                                console.warn("컬럼 감지 실패:", err);
                            });
                    }

                    // 저장된 북마크를 불러온다 (머리 지문이 다르면 stale=true로 안내)
                    // 병합 뷰는 북마크를 지원하지 않으므로 건너뛴다
                    const bookmarksPromise =
                        info.source === "merge" ? null : getBookmarks(info.id);
                    bookmarksPromise
                        ?.then((bm) => {
                            if (openSeqRef.current !== seq) return;
                            setBookmarkItems((prev) => {
                                // 불러오기가 끝나기 전 사용자가 단 북마크가 있으면
                                // 병합한다 (같은 줄은 로컬 항목 우선)
                                if (prev.length === 0) return bm.items;
                                const localLines = new Set(
                                    prev.map((b) => b.line),
                                );
                                return [
                                    ...prev,
                                    ...bm.items.filter(
                                        (b) => !localLines.has(b.line),
                                    ),
                                ].sort((a, b) => a.line - b.line);
                            });
                            setBookmarksStale(bm.stale);
                        })
                        .catch((err) => {
                            console.warn("북마크 조회 실패:", err);
                        });

                    // 이전에 보던 스크롤 위치 복원 예약 (로테이션 재오픈은 맨 아래로 가므로 제외)
                    // 병합 탭은 매번 새로 만드는 스냅샷이라 위치 복원을 하지 않는다
                    if (!options?.preserveTail && !isMergePath(info.path)) {
                        const saved = loadScrollPos(info.path);
                        if (saved !== null) {
                            pendingPosRef.current = saved;
                        } else {
                            restoreSettledRef.current = true;
                        }
                    } else {
                        restoreSettledRef.current = true;
                    }

                    // openFile resolve 전 도착한 진행률이 있다면 확정 적용
                    const progress =
                        latestProgressRef.current as IndexProgress | null;
                    if (progress) {
                        normalLineCache.setLineCount(
                            progress.lineCount,
                            progress.done,
                        );
                    }
                } catch (err) {
                    if (openSeqRef.current === seq) {
                        restoreSettledRef.current = true;
                        const msg =
                            typeof err === "string"
                                ? err
                                : err instanceof Error
                                  ? err.message
                                  : "파일을 열 수 없습니다.";
                        setErrorMessage(msg);
                    }
                }
            },
            [
                tabId,
                mergeSourceIds,
                normalLineCache,
                filteredLineCache,
                filterLineCache,
                flushBookmarks,
            ],
        );

        // 마운트 시 요청 경로의 파일을 연다.
        // StrictMode 이중 실행은 openSeqRef로 이전 요청이 자동 정리된다.
        useEffect(() => {
            void handleOpenFileByPath(path);
        }, [handleOpenFileByPath, path]);

        // 진행 중인 열기 작업을 무효화한다 (언마운트 cleanup에서도 사용)
        const invalidateOpenSeq = useCallback(() => {
            openSeqRef.current++;
        }, []);

        // 언마운트(탭 닫기) 시 진행 중인 열기를 무효화하고 열린 파일을 정리한다.
        // 인덱싱/검색/필터/tail은 백엔드 close_file이 함께 취소한다.
        useEffect(() => {
            return () => {
                invalidateOpenSeq();
                pendingAppendedRef.current = null;
                if (tailRafRef.current !== null) {
                    cancelAnimationFrame(tailRafRef.current);
                    tailRafRef.current = null;
                }
                if (densityTimerRef.current !== null) {
                    clearTimeout(densityTimerRef.current);
                }
                // 디바운스로 대기 중인 북마크 저장은 닫기 전에 즉시 flush한다
                void flushBookmarks();
                const current = currentFileRef.current;
                if (current !== null) {
                    // 탭이 닫혀도 마지막 위치가 남도록 즉시 저장한다 (병합 탭 제외)
                    if (
                        restoreSettledRef.current &&
                        !isMergePath(current.path)
                    ) {
                        saveScrollPos(current.path, topLineRef.current);
                    }
                    closeFile(current.id).catch((err) => {
                        console.warn("파일 닫기 실패:", err);
                    });
                }
            };
        }, [invalidateOpenSeq, flushBookmarks]);

        // tail 모드 토글 (버튼 / Cmd+T). 켜기 방향은 인덱싱 완료 effect가 처리한다.
        const handleToggleTail = useCallback(() => {
            const current = currentFileRef.current;
            // 압축 파일(.gz)은 임시 파일 해제 방식이고, S3 객체는 불변,
            // 병합 뷰는 스냅샷이라 tail 모드를 지원하지 않는다
            if (
                !current ||
                current.compressed ||
                current.source === "s3" ||
                current.source === "merge"
            )
                return;
            const next = !tailEnabledRef.current;
            tailEnabledRef.current = next;
            setTailEnabled(next);
            if (!next) {
                setNewLinesCount(0);
                pendingAppendedRef.current = null;
                queueTailCommand(() =>
                    setTail(current.id, false, () => {}),
                ).catch((err) => {
                    console.warn("tail 모드 해제 실패:", err);
                });
            }
        }, [queueTailCommand]);

        // 검색 밀도 요청 (최대 4회/초 스로틀)
        const requestDensityFetch = useCallback(
            (searchId: number) => {
                const totalLines = lineCountRef.current;
                if (totalLines <= 0) return;

                const trackH = trackHeightRef.current || viewportHeight || 600;
                const bins = Math.max(
                    10,
                    Math.min(2000, Math.floor(trackH / 2)),
                );

                searchDensity(searchId, bins, totalLines)
                    .then((counts) => {
                        if (activeSearchIdRef.current === searchId) {
                            setDensityBins(counts);
                        }
                    })
                    .catch(() => {});
            },
            [viewportHeight],
        );

        const handleSearchDensityRequest = useCallback(
            (searchId: number) => {
                activeSearchIdRef.current = searchId;
                const now = Date.now();
                if (now - lastDensityTimeRef.current >= 250) {
                    lastDensityTimeRef.current = now;
                    if (densityTimerRef.current) {
                        clearTimeout(densityTimerRef.current);
                        densityTimerRef.current = null;
                    }
                    requestDensityFetch(searchId);
                } else if (!densityTimerRef.current) {
                    const delay = 250 - (now - lastDensityTimeRef.current);
                    densityTimerRef.current = setTimeout(() => {
                        densityTimerRef.current = null;
                        lastDensityTimeRef.current = Date.now();
                        requestDensityFetch(searchId);
                    }, delay);
                }
            },
            [requestDensityFetch],
        );

        // 검색 시작 시 콜백
        const handleSearchStarted = useCallback(
            (queryDto: QueryOptions, searchId: number) => {
                activeSearchIdRef.current = searchId;
                normalLineCache.setQuery(queryDto);
                filterLineCache.setQuery(queryDto);
                activeQueryRef.current = queryDto;
                if (file) {
                    filteredLineCache.setFilteredSource(
                        file.id,
                        searchId,
                        queryDto,
                    );
                }
                // Done 이벤트가 Started보다 먼저 도착했다면 완료 상태를 바로 반영
                const pendingDone = searchDoneRef.current;
                searchDoneRef.current = null;
                if (pendingDone !== null && pendingDone.searchId === searchId) {
                    filteredLineCache.setLineCount(pendingDone.total, true);
                    setSearchStatus({
                        currentHitIndex: null,
                        matchCount: pendingDone.total,
                        isSearching: false,
                        isCapped: pendingDone.capped,
                        query: queryDto.query,
                    });
                    requestDensityFetch(searchId);
                    return;
                }
                setSearchStatus({
                    currentHitIndex: null,
                    matchCount: 0,
                    isSearching: true,
                    isCapped: false,
                    query: queryDto.query,
                });
                handleSearchDensityRequest(searchId);
            },
            [
                file,
                normalLineCache,
                filteredLineCache,
                filterLineCache,
                handleSearchDensityRequest,
                requestDensityFetch,
            ],
        );

        // 검색 진행률 업데이트 시 콜백
        const handleSearchProgress = useCallback(
            (searchId: number, matchCount: number) => {
                if (activeSearchIdRef.current !== searchId) return;
                filteredLineCache.setLineCount(matchCount, false);
                setSearchStatus((prev) =>
                    prev
                        ? {
                              ...prev,
                              matchCount,
                              isSearching: true,
                          }
                        : null,
                );
            },
            [filteredLineCache],
        );

        // 검색 완료 시 콜백
        const handleSearchDone = useCallback(
            (searchId: number, total: number, capped: boolean) => {
                // Started보다 먼저 도착한 경우에 대비해 기록해 둔다
                searchDoneRef.current = { searchId, total, capped };
                if (activeSearchIdRef.current !== searchId) {
                    return;
                }
                filteredLineCache.setLineCount(total, true);
                setSearchStatus((prev) =>
                    prev
                        ? {
                              ...prev,
                              matchCount: total,
                              isSearching: false,
                              isCapped: capped,
                          }
                        : null,
                );
                requestDensityFetch(searchId);
            },
            [filteredLineCache, requestDensityFetch],
        );

        // 검색어 초기화/지우기 시 콜백
        const handleClearSearch = useCallback(() => {
            activeSearchIdRef.current = null;
            searchDoneRef.current = null;
            if (densityTimerRef.current) {
                clearTimeout(densityTimerRef.current);
                densityTimerRef.current = null;
            }
            normalLineCache.setQuery(null);
            filterLineCache.setQuery(null);
            activeQueryRef.current = null;
            // 필터 캐시는 searchId와 함께 분리해 취소된 searchId로 재요청하지 않게 한다
            filteredLineCache.setFile(null);
            setIsFilterMode(false);
            setSearchStatus(null);
            setCurrentHit(null);
            setDensityBins([]);
        }, [normalLineCache, filteredLineCache, filterLineCache]);

        // 검색 결과 항목으로 뷰포트 이동
        const handleNavigateToHit = useCallback(
            (hit: SearchHit) => {
                setCurrentHit(hit);
                setSearchStatus((prev) =>
                    prev ? { ...prev, currentHitIndex: hit.index } : null,
                );

                // 해당 줄이 화면 위에서 1/3 지점에 오도록 topLine 설정
                const visibleRows =
                    viewportHeight > 0 ? viewportHeight / rowHeight : 30;
                const offset = Math.floor(visibleRows / 3);
                const scrollToIndex = (targetIndex: number) => {
                    const targetTop = targetIndex - offset;
                    const maxTop = Math.max(0, activeLineCount - visibleRows);
                    handleTopLineChange(
                        Math.max(0, Math.min(maxTop, targetTop)),
                    );
                };

                if (isFilterMode) {
                    scrollToIndex(hit.index);
                } else if (
                    filterViewActive &&
                    activeFilterIdRef.current !== null
                ) {
                    // 다중 필터 보기: 결과의 실제 줄 번호를 필터 인덱스로 변환해 이동
                    const fid = activeFilterIdRef.current;
                    filterIndexOf(fid, hit.line)
                        .then((idx) => scrollToIndex(idx))
                        .catch(() => scrollToIndex(hit.line));
                } else {
                    scrollToIndex(hit.line);
                }
            },
            [
                isFilterMode,
                filterViewActive,
                viewportHeight,
                rowHeight,
                activeLineCount,
                handleTopLineChange,
            ],
        );

        // 필터 모드 토글 (필터 모드에서는 실제 줄 번호 기준 줄 선택을 지원하지 않으므로 해제)
        const handleToggleFilterMode = useCallback(() => {
            setIsFilterMode((prev) => !prev);
            setLineSelection(null);
            handleTopLineChange(0);
        }, [handleTopLineChange]);

        // 필터 모드에서 행 더블클릭 시 필터 해제 후 원본 줄로 이동
        const handleLineDoubleClick = useCallback(
            (originalLineNo: number) => {
                setIsFilterMode(false);
                const visibleRows =
                    viewportHeight > 0 ? viewportHeight / rowHeight : 30;
                const targetTop = originalLineNo - Math.floor(visibleRows / 3);
                const maxTop = Math.max(0, lineCount - visibleRows);
                const clampedTop = Math.max(0, Math.min(maxTop, targetTop));
                handleTopLineChange(clampedTop);
            },
            [viewportHeight, rowHeight, lineCount, handleTopLineChange],
        );

        // ===== 다중 필터 (Phase 10) =====

        // 셀 우클릭 "이 값만 보기 / 이 값 제외": 해당 컬럼에 ^값$ 정규식 field 규칙을 추가한다
        const handleFieldFilter = useCallback(
            (field: string, value: string, kind: RuleKind) => {
                setFilterSpec((prev) => ({
                    ...prev,
                    rules: [
                        ...prev.rules,
                        {
                            pattern: `^${escapeRegex(value)}$`,
                            regex: true,
                            caseSensitive: true,
                            kind,
                            enabled: true,
                            color: pickRuleColor(prev.rules),
                            field,
                        },
                    ],
                }));
                setToast(
                    kind === "exclude"
                        ? `필드 '${field}' 제외 규칙 추가`
                        : `필드 '${field}' 포함 규칙 추가`,
                );
            },
            [],
        );

        // 첫 progress/done 시점에 새 필터 결과로 화면을 전환한다.
        // 전환 직전 화면 맨 위의 실제 줄 번호를 filter_index_of로 변환해 보던 위치를 유지한다.
        const promoteFilter = useCallback(async () => {
            const pending = pendingPromoteRef.current;
            if (pending === null) return;
            const { fid, fileId } = pending;
            if (
                promoteInFlightRef.current === fid ||
                activeFilterIdRef.current === fid
            ) {
                return;
            }
            promoteInFlightRef.current = fid;
            try {
                const topActual = currentTopActualLine();
                let index = 0;
                try {
                    index = await filterIndexOf(fid, topActual);
                } catch (err) {
                    console.warn("filter_index_of 실패, 처음부터 표시:", err);
                }
                // 기다리는 동안 더 새 필터가 시작됐거나 파일이 바뀌었으면 폐기
                if (
                    filterIdRef.current !== fid ||
                    currentFileRef.current?.id !== fileId
                ) {
                    return;
                }
                const latest = pendingPromoteRef.current;
                const count =
                    latest !== null && latest.fid === fid
                        ? latest.count
                        : pending.count;
                const done =
                    latest !== null && latest.fid === fid
                        ? latest.done
                        : pending.done;
                if (latest?.fid === fid) {
                    pendingPromoteRef.current = null;
                }
                activeFilterIdRef.current = fid;
                filterViewActiveRef.current = true;
                setActiveFilterId(fid);
                // 이전 필터 결과 블록은 유지한 채 새 필터로 교체 (깜빡임 방지)
                filterLineCache.setFilterSource(
                    fileId,
                    fid,
                    activeQueryRef.current,
                    filterSpecRef.current,
                    true,
                );
                filterLineCache.setLineCount(count, done);
                setFilterMatchCount(count);
                filterCountRef.current = count;
                handleTopLineChange(index);
            } finally {
                if (promoteInFlightRef.current === fid) {
                    promoteInFlightRef.current = null;
                }
            }
        }, [currentTopActualLine, filterLineCache, handleTopLineChange]);

        // 필터 스캔 이벤트 처리 (progress/done/error). fid는 start_filter가 반환한 필터 ID.
        const handleFilterEvent = useCallback(
            (seq: number, fileId: number, fid: number, event: SearchEvent) => {
                if (filterSeqRef.current !== seq) return;
                if (event.type === "progress") {
                    setFilterStatus((prev) => ({
                        matchCount: event.matchCount,
                        isSearching: true,
                        isCapped: prev?.isCapped ?? false,
                        scannedBytes: event.scannedBytes,
                        totalBytes: event.totalBytes,
                    }));
                    if (activeFilterIdRef.current === fid) {
                        filterLineCache.setLineCount(event.matchCount, false);
                        setFilterMatchCount(event.matchCount);
                        filterCountRef.current = event.matchCount;
                    } else {
                        pendingPromoteRef.current = {
                            fid,
                            fileId,
                            count: event.matchCount,
                            done: false,
                        };
                        void promoteFilter();
                    }
                } else if (event.type === "done") {
                    setFilterStatus((prev) => ({
                        matchCount: event.total,
                        isSearching: false,
                        isCapped: event.capped,
                        scannedBytes: prev?.scannedBytes ?? 0,
                        totalBytes: prev?.totalBytes ?? 0,
                    }));
                    if (activeFilterIdRef.current === fid) {
                        filterLineCache.setLineCount(event.total, true);
                        setFilterMatchCount(event.total);
                        filterCountRef.current = event.total;
                    } else {
                        pendingPromoteRef.current = {
                            fid,
                            fileId,
                            count: event.total,
                            done: true,
                        };
                        void promoteFilter();
                    }
                } else if (event.type === "error") {
                    setErrorMessage(event.message);
                    filterLineCache.setSuspended(false);
                }
                // cancelled는 seq 가드로 최신 필터만 통과하므로 별도 처리 없음
            },
            [filterLineCache, promoteFilter],
        );

        // 필터 스캔 시작. start_filter 호출 즉시 이전 필터는 백엔드에서 제거되므로,
        // 새 결과가 화면에 반영되기 전까지 이전 소스의 재요청을 보류해 오류를 막는다.
        const startFilterScan = useCallback(
            (fileId: number, spec: FilterSpec, key: string, seq: number) => {
                let resolvedId: number | null = null;
                const buffered: SearchEvent[] = [];

                filterLineCache.setSuspended(true);

                startFilter(fileId, spec, (event) => {
                    if (filterSeqRef.current !== seq) return;
                    if (resolvedId === null) {
                        buffered.push(event);
                        return;
                    }
                    handleFilterEvent(seq, fileId, resolvedId, event);
                })
                    .then((newId) => {
                        if (filterSeqRef.current !== seq) {
                            cancelFilter(newId).catch(() => {});
                            return;
                        }
                        resolvedId = newId;
                        runningFilterKeyRef.current = key;
                        filterIdRef.current = newId;
                        setFilterId(newId);
                        // 계산 중 상태바에는 이전 표시 건수를 이어서 보여준다
                        setFilterStatus({
                            matchCount: filterCountRef.current,
                            isSearching: true,
                            isCapped: false,
                            scannedBytes: 0,
                            totalBytes: 0,
                        });
                        for (const ev of buffered) {
                            handleFilterEvent(seq, fileId, newId, ev);
                        }
                        buffered.length = 0;
                    })
                    .catch((err) => {
                        if (filterSeqRef.current !== seq) return;
                        filterLineCache.setSuspended(false);
                        const msg =
                            typeof err === "string"
                                ? err
                                : "필터를 시작할 수 없습니다.";
                        setErrorMessage(msg);
                    });
            },
            [filterLineCache, handleFilterEvent],
        );

        // 필터 스펙 변경 시 300ms 디바운스 후 start_filter.
        // 색상 등 판정과 무관한 변경은 서명이 같으므로 재스캔하지 않는다.
        useEffect(() => {
            const fileId = file?.id ?? null;
            if (fileId === null || !specHasCondition(filterSpec)) {
                // 필터 없음: 계산 중이던 작업을 폐기하고 일반/검색 보기로 복귀.
                // 이펙트 본문의 동기 setState를 피하기 위해 마이크로태스크로 미룬다.
                queueMicrotask(() => {
                    filterSeqRef.current++;
                    runningFilterKeyRef.current = "";
                    pendingPromoteRef.current = null;
                    const prevId = filterIdRef.current;
                    filterIdRef.current = null;
                    activeFilterIdRef.current = null;
                    filterViewActiveRef.current = false;
                    filterCountRef.current = 0;
                    setFilterId(null);
                    setActiveFilterId(null);
                    setFilterStatus(null);
                    setFilterMatchCount(0);
                    if (prevId !== null) {
                        cancelFilter(prevId).catch(() => {});
                    }
                });
                return;
            }

            const key = `${fileId}:${filterSignature(filterSpec)}`;
            if (key === runningFilterKeyRef.current) {
                return;
            }

            const seq = ++filterSeqRef.current;
            const timer = setTimeout(() => {
                if (filterSeqRef.current !== seq) return;
                startFilterScan(fileId, filterSpecRef.current, key, seq);
            }, 300);
            return () => clearTimeout(timer);
        }, [filterSpec, file, startFilterScan]);

        // ===== tail 모드 =====

        // rAF 단위로 묶인 appended 이벤트를 반영한다 (프레임당 마지막 배치만 처리)
        const flushTailAppended = useCallback(() => {
            tailRafRef.current = null;
            const pending = pendingAppendedRef.current;
            pendingAppendedRef.current = null;
            if (!pending) return;

            const {
                oldLineCount,
                lineCount: newLineCount,
                totalBytes,
                filterCount,
            } = pending;

            // 줄 수/파일 크기 갱신 + 개행 없이 끝나던 마지막 줄이 속한 블록 무효화
            normalLineCache.setLineCount(newLineCount, true);
            setLineCount(newLineCount);
            setLiveFileSize(totalBytes);
            if (oldLineCount > 0) {
                normalLineCache.invalidateLine(oldLineCount - 1);
            }

            // 필터 모드에서는 스크롤/버튼 처리 없이 캐시만 최신화한다
            if (isFilterModeRef.current) return;

            // 다중 필터 보기: 필터 줄 수 기준으로 갱신하고 따라가기/새 줄 카운터를 처리한다.
            // 필터가 아직 계산 중이거나 최신이 아니면 스크롤 처리를 생략한다.
            if (filterViewActiveRef.current) {
                const fid = activeFilterIdRef.current;
                if (
                    filterCount != null &&
                    fid !== null &&
                    fid === filterIdRef.current
                ) {
                    const prevCount = filterCountRef.current;
                    filterLineCache.setLineCount(filterCount, true);
                    setFilterMatchCount(filterCount);
                    filterCountRef.current = filterCount;
                    setFilterStatus((prev) =>
                        prev
                            ? {
                                  ...prev,
                                  matchCount: filterCount,
                                  isSearching: false,
                              }
                            : prev,
                    );
                    // 마지막 줄이 이어쓰였을 수 있으므로 직전 끝 줄은 재요청되게 무효화
                    if (prevCount > 0) {
                        filterLineCache.invalidateLine(prevCount - 1);
                    }
                    if (atBottomRef.current) {
                        const maxTop = Math.max(
                            0,
                            filterCount -
                                viewportHeightRef.current /
                                    rowHeightRef.current,
                        );
                        handleTopLineChange(maxTop);
                    } else {
                        const added = Math.max(0, filterCount - prevCount);
                        setNewLinesCount((prev) => prev + added);
                    }
                }
                return;
            }

            if (atBottomRef.current) {
                // 맨 아래에 있으면 새 끝으로 자동 스크롤
                const maxTop = Math.max(
                    0,
                    newLineCount -
                        viewportHeightRef.current / rowHeightRef.current,
                );
                handleTopLineChange(maxTop);
            } else {
                const added = Math.max(0, newLineCount - oldLineCount);
                setNewLinesCount((prev) => prev + added);
            }
        }, [normalLineCache, filterLineCache, handleTopLineChange]);

        // 로테이션 감지 시 같은 경로로 자동 재오픈 (tail 상태 유지)
        const handleTailRotated = useCallback(
            (reason: "truncated" | "replaced") => {
                setRotationReason(reason);
                const current = currentFileRef.current;
                if (current) {
                    scrollToBottomAfterReopenRef.current = true;
                    handleOpenFileByPath(current.path, { preserveTail: true });
                }
            },
            [handleOpenFileByPath],
        );

        // 백엔드 tail 이벤트 수신 핸들러 (Channel 콜백)
        const handleTailEvent = useCallback(
            (event: TailEvent) => {
                if (event.type === "appended") {
                    // 프레임 내 이벤트는 첫 oldLineCount ~ 마지막 lineCount로 합침
                    const prev = pendingAppendedRef.current;
                    pendingAppendedRef.current = {
                        oldLineCount: prev
                            ? prev.oldLineCount
                            : event.oldLineCount,
                        lineCount: event.lineCount,
                        totalBytes: event.totalBytes,
                        filterCount:
                            event.filterCount ?? prev?.filterCount ?? null,
                    };
                    if (tailRafRef.current === null) {
                        tailRafRef.current = requestAnimationFrame(() => {
                            flushTailAppended();
                        });
                    }
                } else if (event.type === "rotated") {
                    handleTailRotated(event.reason);
                } else {
                    setErrorMessage(event.message);
                }
            },
            [flushTailAppended, handleTailRotated],
        );

        // tail 활성화: 인덱싱 완료 후 set_tail 호출 (로테이션 재오픈 시 재호출 포함)
        useEffect(() => {
            if (!file || !tailEnabled || !indexProgress?.done) {
                return;
            }
            // 로테이션 재오픈 직후라면 인덱싱 완료 시점에 맨 아래로 이동
            if (scrollToBottomAfterReopenRef.current) {
                scrollToBottomAfterReopenRef.current = false;
                const maxTop = Math.max(
                    0,
                    lineCountRef.current -
                        viewportHeightRef.current / rowHeightRef.current,
                );
                handleTopLineChange(maxTop);
            }
            let cancelled = false;
            queueTailCommand(() => setTail(file.id, true, handleTailEvent))
                .then(() => {
                    if (cancelled) {
                        queueTailCommand(() =>
                            setTail(file.id, false, () => {}),
                        ).catch(() => {});
                    }
                })
                .catch((err) => {
                    if (cancelled) return;
                    const msg =
                        typeof err === "string"
                            ? err
                            : "tail 모드를 켤 수 없습니다.";
                    setErrorMessage(msg);
                    tailEnabledRef.current = false;
                    setTailEnabled(false);
                });
            return () => {
                cancelled = true;
            };
        }, [
            file,
            tailEnabled,
            indexProgress?.done,
            handleTailEvent,
            queueTailCommand,
            handleTopLineChange,
        ]);

        // "새 줄 N개 ↓" 버튼: 맨 아래로 이동하고 자동 스크롤 재개
        const handleScrollToBottom = useCallback(() => {
            const maxTop = Math.max(
                0,
                displayCountRef.current -
                    viewportHeightRef.current / rowHeightRef.current,
            );
            handleTopLineChange(maxTop);
        }, [handleTopLineChange]);

        // ===== Phase 5: 편의 기능 =====

        // 비동기 핸들러에서 최신 줄 선택을 읽기 위한 ref
        const lineSelectionRef = useRef(lineSelection);
        useEffect(() => {
            lineSelectionRef.current = lineSelection;
        }, [lineSelection]);

        // 거터 줄 번호 클릭: 일반 클릭은 해당 줄 선택, Shift+클릭은 anchor~클릭 범위
        const handleGutterLineClick = useCallback(
            (index: number, shiftKey: boolean) => {
                // 인덱스 기반 보기(검색 결과만/다중 필터)에서는 줄 선택을 지원하지 않는다
                if (isFilterModeRef.current || filterViewActiveRef.current)
                    return;
                setLineSelection((prev) => {
                    if (shiftKey && prev !== null) {
                        return { anchor: prev.anchor, focus: index };
                    }
                    return { anchor: index, focus: index };
                });
            },
            [],
        );

        // Cmd+C (메뉴 copy 이벤트): 입력창/DOM 선택이 있으면 기본 복사,
        // 아니면 줄 단위 선택 범위를 Rust에서 읽어 클립보드에 기록한다.
        const handleCopyMenu = useCallback(async () => {
            const active = document.activeElement;
            if (
                active instanceof HTMLInputElement ||
                active instanceof HTMLTextAreaElement
            ) {
                document.execCommand("copy");
                return;
            }
            const domSelection = window.getSelection();
            if (
                domSelection !== null &&
                !domSelection.isCollapsed &&
                domSelection.toString().length > 0
            ) {
                document.execCommand("copy");
                return;
            }

            const current = currentFileRef.current;
            const selection = lineSelectionRef.current;
            if (current === null || selection === null) return;

            const start = Math.min(selection.anchor, selection.focus);
            const end = Math.max(selection.anchor, selection.focus);
            setCopying(true);
            try {
                const result = await copyLines(current.id, start, end);
                setToast(
                    `${result.lines.toLocaleString("ko-KR")}줄 복사됨 (${formatBytes(result.bytes)})`,
                );
            } catch (err) {
                const msg =
                    typeof err === "string" ? err : "복사에 실패했습니다.";
                setErrorMessage(msg);
            } finally {
                setCopying(false);
            }
        }, []);

        // Cmd+L: 필터 모드에서는 실제 줄 번호 기준 이동을 위해 먼저 필터 해제
        const handleOpenGotoLine = useCallback(() => {
            if (currentFileRef.current === null) return;
            if (isFilterModeRef.current) {
                setIsFilterMode(false);
            }
            setGotoLineOpen(true);
        }, []);

        // 줄 이동 모달에서 확정: 해당 줄이 화면 위 1/3 지점에 오도록 스크롤
        const handleGotoNavigate = useCallback(
            (lineNo: number) => {
                setGotoLineOpen(false);
                const visibleRows =
                    viewportHeightRef.current > 0
                        ? viewportHeightRef.current / rowHeight
                        : 30;
                const offset = Math.floor(visibleRows / 3);
                const targetIndex = lineNo - 1; // 1-based → 0-based

                // 다중 필터 보기(검색 결과만 제외): 실제 줄 번호 → 필터 인덱스 변환
                if (filterViewActiveRef.current && !isFilterModeRef.current) {
                    const fid = activeFilterIdRef.current;
                    if (fid !== null) {
                        filterIndexOf(fid, targetIndex)
                            .then((idx) => {
                                const maxTop = Math.max(
                                    0,
                                    displayCountRef.current - visibleRows,
                                );
                                handleTopLineChange(
                                    Math.max(0, Math.min(maxTop, idx - offset)),
                                );
                            })
                            .catch(() => {});
                        return;
                    }
                }

                const targetTop = targetIndex - offset;
                const maxTop = Math.max(0, lineCountRef.current - visibleRows);
                handleTopLineChange(Math.max(0, Math.min(maxTop, targetTop)));
            },
            [rowHeight, handleTopLineChange],
        );

        // 잘린 줄의 "…" 클릭: 원문 전체 패널 열기/교체
        const handleTruncatedLineClick = useCallback((lineNo: number) => {
            const current = currentFileRef.current;
            if (!current) return;
            // 오른쪽 사이드 패널은 하나만 열린다
            setBookmarkPanelOpen(false);
            setFullLinePanel({ fileId: current.id, no: lineNo });
        }, []);

        // ===== 로그 병합 뷰 (Phase 17) =====

        // Cmd+L에서 시각 이동 (병합 탭): 시각을 로컬로 해석한 epoch ms → 첫 병합 위치로 스크롤
        const handleGotoTime = useCallback(
            (tsMs: number) => {
                const current = currentFileRef.current;
                if (current === null || current.source !== "merge") return;
                setGotoLineOpen(false);
                mergePositionAtTime(current.id, tsMs)
                    .then((pos) => {
                        const visibleRows =
                            viewportHeightRef.current > 0
                                ? viewportHeightRef.current /
                                  rowHeightRef.current
                                : 30;
                        const offset = Math.floor(visibleRows / 3);
                        const maxTop = Math.max(
                            0,
                            displayCountRef.current - visibleRows,
                        );
                        handleTopLineChange(
                            Math.max(0, Math.min(maxTop, pos - offset)),
                        );
                    })
                    .catch((err) => {
                        const msg =
                            typeof err === "string"
                                ? err
                                : "시각 위치 계산에 실패했습니다.";
                        setErrorMessage(msg);
                    });
            },
            [handleTopLineChange],
        );

        // 병합 탭에서 원본 탭으로 이동 (행 더블클릭/원본 칩/범례 클릭)
        const jumpToSource = useCallback(
            (sourceIndex: number, sourceLine: number) => {
                const info = mergeSourceList.find(
                    (s) => s.index === sourceIndex,
                );
                if (info === undefined) return;
                onJumpToSourceRef.current?.(info.path, sourceLine);
            },
            [mergeSourceList],
        );

        // 범례 클릭: 현재 화면에 보이는 해당 원본의 첫 줄로 원본 탭을 연다.
        // 보이는 줄이 없으면 원본 처음으로 이동한다.
        const handleLegendClick = useCallback(
            (sourceIndex: number) => {
                const cached = activeLineCache
                    .sampleLines(2000)
                    .find((l) => l.source === sourceIndex);
                jumpToSource(sourceIndex, cached?.sourceLine ?? 0);
            },
            [activeLineCache, jumpToSource],
        );

        // App이 보낸 원본 탭 줄 이동 요청: 파일이 열리고 대상 줄이 확인 범위에
        // 들어오면(또는 인덱싱이 끝나면) 해당 줄이 화면 위 1/3 지점에 오도록 스크롤한다
        const pendingGotoRef = useRef<number | null>(null);
        useEffect(() => {
            if (gotoRequest !== null) {
                pendingGotoRef.current = gotoRequest.line;
            }
            const target = pendingGotoRef.current;
            if (target === null || file === null) return;
            if (target >= lineCount && indexProgress?.done !== true) return;
            pendingGotoRef.current = null;
            // 저장된 스크롤 위치 복원보다 이 이동 요청을 우선한다
            pendingPosRef.current = null;
            restoreSettledRef.current = true;
            // 검색 결과만 보기에서는 실제 줄 번호로 갈 수 없으므로 보기를 해제한다
            setIsFilterMode(false);
            handleGotoNavigate(target + 1);
        }, [
            gotoRequest,
            file,
            lineCount,
            indexProgress?.done,
            handleGotoNavigate,
        ]);

        const handleCloseFullLinePanel = useCallback(() => {
            setFullLinePanel(null);
        }, []);

        // 뷰포트에 전달할 정규화된 선택 범위 (일반 모드에서만)
        const selectedRange = useMemo(() => {
            if (isFilterMode || filterViewActive || lineSelection === null) {
                return null;
            }
            return {
                start: Math.min(lineSelection.anchor, lineSelection.focus),
                end: Math.max(lineSelection.anchor, lineSelection.focus),
            };
        }, [isFilterMode, filterViewActive, lineSelection]);

        // ===== 북마크 (Phase 15) =====

        // 병합 뷰는 북마크를 지원하지 않는다 — 단축키/버튼은 안내 토스트만 표시한다
        const bookmarkBlockedForMerge = useCallback((): boolean => {
            if (currentFileRef.current?.source !== "merge") return false;
            setToast("병합 보기에서는 북마크를 지원하지 않습니다");
            return true;
        }, []);

        // 비동기 북마크 핸들러에서 최신 검색 결과를 읽기 위한 ref
        useEffect(() => {
            currentHitRef.current = currentHit;
        }, [currentHit]);

        // 북마크 토글/이동의 기준 줄 (실제 파일 줄 번호, 0-based):
        // 줄 선택이 있으면 선택 시작 줄 → 없으면 현재 검색 결과 줄 → 없으면 화면 위 1/3 지점 줄
        const bookmarkTargetLine = useCallback((): number | null => {
            if (currentFileRef.current === null) return null;
            const sel = lineSelectionRef.current;
            if (
                sel !== null &&
                !isFilterModeRef.current &&
                !filterViewActiveRef.current
            ) {
                return Math.min(sel.anchor, sel.focus);
            }
            if (currentHitRef.current !== null) {
                return currentHitRef.current.line;
            }
            const visibleRows =
                viewportHeightRef.current > 0
                    ? viewportHeightRef.current / rowHeightRef.current
                    : 30;
            const index =
                Math.floor(topLineRef.current) + Math.floor(visibleRows / 3);
            return actualLineAtIndex(index);
        }, [actualLineAtIndex]);

        // 지정한 실제 줄 번호의 북마크를 추가/제거한다
        const toggleBookmarkAt = useCallback(
            (lineNo: number) => {
                if (currentFileRef.current === null) return;
                const prev = bookmarksRef.current;
                const next = prev.some((b) => b.line === lineNo)
                    ? prev.filter((b) => b.line !== lineNo)
                    : [
                          ...prev,
                          {
                              line: lineNo,
                              note: "",
                              color: DEFAULT_BOOKMARK_COLOR,
                              createdAt: Math.floor(Date.now() / 1000),
                          },
                      ].sort((a, b) => a.line - b.line);
                bookmarksRef.current = next;
                setBookmarkItems(next);
                scheduleBookmarkSave();
            },
            [scheduleBookmarkSave],
        );

        // Cmd+B: 기준 줄의 북마크 토글
        const handleToggleBookmark = useCallback(() => {
            if (bookmarkBlockedForMerge()) return;
            const line = bookmarkTargetLine();
            if (line !== null) toggleBookmarkAt(line);
        }, [bookmarkBlockedForMerge, bookmarkTargetLine, toggleBookmarkAt]);

        // 실제 파일 줄 번호로 스크롤한다 (줄이 화면 위 1/3 지점에 오도록).
        // 인덱스 기반 보기에서는 보이는 줄 인덱스로 변환한다.
        // 반환값: 이동한 줄이 현재 보기에 있어 실제로 이동했으면 true.
        const navigateToBookmarkLine = useCallback(
            async (actualLine: number): Promise<boolean> => {
                const visibleRows =
                    viewportHeightRef.current > 0
                        ? viewportHeightRef.current / rowHeightRef.current
                        : 30;
                const offset = Math.floor(visibleRows / 3);
                const scrollToIndex = (targetIndex: number) => {
                    const targetTop = targetIndex - offset;
                    const maxTop = Math.max(
                        0,
                        displayCountRef.current - visibleRows,
                    );
                    handleTopLineChange(
                        Math.max(0, Math.min(maxTop, targetTop)),
                    );
                };

                if (isFilterModeRef.current) {
                    // 검색 결과만 보기: 북마크 줄이 검색 결과에 있을 때만
                    // 그 결과 인덱스로 이동한다
                    const sid = activeSearchIdRef.current;
                    if (sid === null || actualLine <= 0) return false;
                    try {
                        const hit = await searchNav(sid, actualLine - 1, true);
                        if (hit === null || hit.line !== actualLine) {
                            return false;
                        }
                        scrollToIndex(hit.index);
                        return true;
                    } catch {
                        return false;
                    }
                }
                if (
                    filterViewActiveRef.current &&
                    activeFilterIdRef.current !== null
                ) {
                    const fid = activeFilterIdRef.current;
                    try {
                        // line 이상인 첫 보이는 줄의 필터 인덱스
                        const idx = await filterIndexOf(fid, actualLine);
                        scrollToIndex(idx);
                    } catch {
                        scrollToIndex(actualLine);
                    }
                    return true;
                }
                scrollToIndex(actualLine);
                return true;
            },
            [handleTopLineChange],
        );

        // 마지막 북마크 이동 시각 — 네이티브 메뉴 go_top/go_bottom이
        // Cmd+Option+↑/↓ 조합에서도 발화하는 경우를 대비해 중복 이동을 막는다
        const lastBookmarkNavAtRef = useRef(0);

        // F2 / Shift+F2 (또는 Cmd+Option+↓/↑): 다음/이전 북마크로 순환 이동
        const handleBookmarkNav = useCallback(
            async (forward: boolean) => {
                lastBookmarkNavAtRef.current = Date.now();
                if (bookmarkBlockedForMerge()) return;
                const items = bookmarksRef.current;
                if (items.length === 0 || currentFileRef.current === null) {
                    return;
                }
                const ref = bookmarkTargetLine();
                if (ref === null) return;

                const ordered = forward ? items : [...items].reverse();
                // 기준 줄 다음/이전 후보부터 시도하고, 끝에 도달하면 처음으로 순환한다
                const candidates = [
                    ...ordered.filter((b) =>
                        forward ? b.line > ref : b.line < ref,
                    ),
                    ...ordered.filter((b) =>
                        forward ? b.line <= ref : b.line >= ref,
                    ),
                ];
                for (const b of candidates) {
                    if (await navigateToBookmarkLine(b.line)) return;
                }
                // 검색 결과만 보기에서 결과에 속한 북마크가 하나도 없는 경우
                setToast("현재 보기에 북마크가 없습니다");
            },
            [
                bookmarkBlockedForMerge,
                bookmarkTargetLine,
                navigateToBookmarkLine,
            ],
        );

        // 북마크 패널 토글 (Cmd+Shift+B). 사이드 패널은 하나만 열린다
        const handleToggleBookmarkPanel = useCallback(() => {
            if (bookmarkBlockedForMerge()) return;
            setBookmarkPanelOpen((prev) => !prev);
            setFullLinePanel(null);
        }, [bookmarkBlockedForMerge]);

        // 패널에서 북마크 속성을 고친다 (메모/색상/삭제 공통 패턴)
        const updateBookmarkItem = useCallback(
            (line: number, patch: (b: Bookmark) => Bookmark | null) => {
                const next = bookmarksRef.current
                    .map((b) => (b.line === line ? patch(b) : b))
                    .filter((b): b is Bookmark => b !== null);
                bookmarksRef.current = next;
                setBookmarkItems(next);
                scheduleBookmarkSave();
            },
            [scheduleBookmarkSave],
        );

        const handleBookmarkNoteChange = useCallback(
            (line: number, note: string) => {
                updateBookmarkItem(line, (b) => ({ ...b, note }));
            },
            [updateBookmarkItem],
        );

        const handleBookmarkColorChange = useCallback(
            (line: number, color: string) => {
                updateBookmarkItem(line, (b) => ({ ...b, color }));
            },
            [updateBookmarkItem],
        );

        const handleBookmarkDelete = useCallback(
            (line: number) => {
                updateBookmarkItem(line, () => null);
            },
            [updateBookmarkItem],
        );

        // 패널 항목 클릭: 해당 줄로 이동
        const handleBookmarkPanelNavigate = useCallback(
            (line: number) => {
                void navigateToBookmarkLine(line);
            },
            [navigateToBookmarkLine],
        );

        // stale 안내 [확인]: 현재 상태로 다시 저장해 지문을 갱신한다
        const handleConfirmStaleBookmarks = useCallback(() => {
            const current = currentFileRef.current;
            if (current === null) return;
            saveBookmarksIpc(current.id, bookmarksRef.current)
                .then(() => setBookmarksStale(false))
                .catch((err) => {
                    const msg =
                        typeof err === "string"
                            ? err
                            : "북마크 저장에 실패했습니다.";
                    setErrorMessage(msg);
                });
        }, []);

        // 북마크 단축키: Cmd+B 토글, Cmd+Shift+B 패널, F2/Shift+F2(또는 Cmd+Option+↓/↑) 이동.
        // 메뉴에 넣지 않고 keydown으로 처리한다 (WKWebView가 Cmd+B를 막으면 안 된다).
        useEffect(() => {
            const handleKeyDown = (e: KeyboardEvent) => {
                if (!isActiveRef.current) return;
                if (e.defaultPrevented) return;
                const active = document.activeElement;
                const inInput =
                    active instanceof HTMLInputElement ||
                    active instanceof HTMLTextAreaElement;

                // Cmd+Shift+B: 북마크 패널 토글 (입력창에서도 동작)
                if (
                    e.metaKey &&
                    e.shiftKey &&
                    !e.ctrlKey &&
                    !e.altKey &&
                    e.code === "KeyB"
                ) {
                    e.preventDefault();
                    if (currentFileRef.current !== null) {
                        handleToggleBookmarkPanel();
                    }
                    return;
                }

                if (inInput) return;
                if (
                    document.querySelector('[aria-modal="true"]') !== null ||
                    gotoLineOpen
                ) {
                    return;
                }

                // Cmd+B: 기준 줄 북마크 토글
                if (
                    e.metaKey &&
                    !e.ctrlKey &&
                    !e.altKey &&
                    !e.shiftKey &&
                    e.code === "KeyB"
                ) {
                    e.preventDefault();
                    handleToggleBookmark();
                    return;
                }

                // F2 / Shift+F2: 다음/이전 북마크
                if (e.key === "F2" && !e.metaKey && !e.ctrlKey && !e.altKey) {
                    e.preventDefault();
                    void handleBookmarkNav(!e.shiftKey);
                    return;
                }
                // Cmd+Option+↓/↑: 다음/이전 북마크
                if (
                    e.metaKey &&
                    e.altKey &&
                    !e.ctrlKey &&
                    !e.shiftKey &&
                    (e.key === "ArrowDown" || e.key === "ArrowUp")
                ) {
                    e.preventDefault();
                    void handleBookmarkNav(e.key === "ArrowDown");
                }
            };
            window.addEventListener("keydown", handleKeyDown);
            return () => window.removeEventListener("keydown", handleKeyDown);
        }, [
            gotoLineOpen,
            handleToggleBookmark,
            handleToggleBookmarkPanel,
            handleBookmarkNav,
        ]);

        // 거터 색 점용 맵 (실제 파일 줄 번호 → 색상)
        const bookmarkLineMap = useMemo(
            () => new Map(bookmarks.map((b) => [b.line, b.color])),
            [bookmarks],
        );

        // 스크롤바 마커용 배열
        const bookmarkMarkers = useMemo(
            () => bookmarks.map((b) => ({ line: b.line, color: b.color })),
            [bookmarks],
        );

        // 네이티브 메뉴 이벤트 수신 — 파일(탭) 범위 명령만 처리한다.
        // 비활성 탭은 무시해 활성 탭에만 동작이 적용되게 한다.
        // (파일 열기/탭 닫기/탭 이동/업데이트/글자 크기는 App이 처리한다)
        useTauriListener(
            (handler) => listen<MenuEvent>("menu", handler),
            (event: TauriEvent<MenuEvent>) => {
                if (!isActiveRef.current) return;
                const { id } = event.payload;
                switch (id) {
                    case "copy":
                        void handleCopyMenu();
                        break;
                    case "find":
                        setFindSignal((s) => s + 1);
                        break;
                    case "add_filter_rule":
                        // "보기 → 필터 규칙 추가" 메뉴 (FilterBar가 keydown/시그널로 처리)
                        setFilterFocusSignal((s) => s + 1);
                        break;
                    case "goto_line":
                        handleOpenGotoLine();
                        break;
                    case "go_top":
                        // 북마크 이동(Cmd+Option+↑) 직후의 go_top은 중복 이동이므로 무시
                        if (Date.now() - lastBookmarkNavAtRef.current < 100) {
                            break;
                        }
                        handleTopLineChange(0);
                        break;
                    case "go_bottom":
                        // 북마크 이동(Cmd+Option+↓) 직후의 go_bottom은 중복 이동이므로 무시
                        if (Date.now() - lastBookmarkNavAtRef.current < 100) {
                            break;
                        }
                        handleScrollToBottom();
                        break;
                    case "toggle_tail":
                        handleToggleTail();
                        break;
                    case "toggle_columns":
                        // 컬럼 형식이 감지되지 않은 탭이면 토글 내부에서 무시된다
                        handleToggleColumnView();
                        break;
                    default:
                        break;
                }
            },
        );

        // Cmd+A: 일반 보기에서 전체 줄 선택 (인덱스 기반 보기는 줄 선택을 지원하지 않는다)
        useEffect(() => {
            const handleKeyDown = (e: KeyboardEvent) => {
                if (!isActiveRef.current) return;
                if (
                    !e.metaKey ||
                    e.ctrlKey ||
                    e.altKey ||
                    e.shiftKey ||
                    (e.key !== "a" && e.key !== "A")
                ) {
                    return;
                }
                const active = document.activeElement;
                if (
                    active instanceof HTMLInputElement ||
                    active instanceof HTMLTextAreaElement
                ) {
                    return;
                }
                if (
                    document.querySelector('[aria-modal="true"]') !== null ||
                    gotoLineOpen
                ) {
                    return;
                }
                if (isFilterModeRef.current || filterViewActiveRef.current) {
                    return;
                }
                const count = lineCountRef.current;
                if (count <= 0) return;
                e.preventDefault();
                setLineSelection({ anchor: 0, focus: count - 1 });
            };
            window.addEventListener("keydown", handleKeyDown);
            return () => window.removeEventListener("keydown", handleKeyDown);
        }, [gotoLineOpen]);

        // Esc: 줄 선택 해제 (모달/패널/입력창에서는 각 컴포넌트가 우선 처리)
        useEffect(() => {
            const handleKeyDown = (e: KeyboardEvent) => {
                if (!isActiveRef.current) return;
                if (e.key !== "Escape") return;
                const active = document.activeElement;
                if (
                    active instanceof HTMLInputElement ||
                    active instanceof HTMLTextAreaElement
                ) {
                    return;
                }
                if (gotoLineOpen || fullLinePanel !== null || bookmarkPanelOpen)
                    return;
                // 전역 모달(업데이트 등)이 열려 있으면 무시한다
                if (document.querySelector('[aria-modal="true"]') !== null) {
                    return;
                }
                setLineSelection(null);
            };
            window.addEventListener("keydown", handleKeyDown);
            return () => window.removeEventListener("keydown", handleKeyDown);
        }, [gotoLineOpen, fullLinePanel, bookmarkPanelOpen]);

        // 토스트 메시지는 2초 후 자동으로 사라진다
        useEffect(() => {
            if (toast === null) return;
            const timer = setTimeout(() => setToast(null), 2000);
            return () => clearTimeout(timer);
        }, [toast]);

        // tail 상태를 App(탭 표시)에 보고한다
        useEffect(() => {
            onTailChangeRef.current(tabId, tailEnabled);
        }, [tailEnabled, tabId]);

        // 로테이션 배너는 5초 후 자동으로 사라진다
        useEffect(() => {
            if (!rotationReason) return;
            const timer = setTimeout(() => setRotationReason(null), 5000);
            return () => clearTimeout(timer);
        }, [rotationReason]);

        const currentHitLine = currentHit !== null ? currentHit.line : null;

        // 인덱스 기반 보기(검색 결과만/다중 필터) 시 화면 맨 위 행의 실제 원본 파일 줄 번호 계산
        const topIndexCache = isFilterMode
            ? filteredLineCache
            : filterViewActive
              ? filterLineCache
              : null;
        const topIndexRow = topIndexCache?.getLine(Math.floor(topLine));
        const actualTopLine = topIndexRow ? topIndexRow.no + 1 : null;

        // 규칙 하이라이트에 사용할 규칙 색상 목록 (spec.rules 인덱스와 1:1)
        const ruleColors = useMemo(
            () => filterSpec.rules.map((r) => r.color),
            [filterSpec],
        );

        // 병합 뷰 여부와 원본 칩 정보 (원본 순번 → 색·짧은 이름)
        const isMergeView = file?.source === "merge";
        const mergeChips = useMemo(
            () =>
                mergeSourceList.map((s) => ({
                    index: s.index,
                    name: mergeShortName(s.path),
                    color: mergeSourceColor(s.index),
                    path: s.path,
                })),
            [mergeSourceList],
        );

        // 현재 렌더 기준 맨 아래 여부 (상태바 tail 일시정지 표시용)
        const maxTopNow = Math.max(
            0,
            displayLineCount - viewportHeight / rowHeight,
        );
        const isAtBottomNow = topLine >= maxTopNow - 0.5;

        // gzip 해제가 끝나지 않은 상태 (done 수신 전까지 검색 결과는 부분 결과)
        const decompressing =
            file?.compressed === true && indexProgress?.done !== true;

        return (
            <div
                className={
                    isActive ? "flex flex-col flex-1 min-h-0 min-w-0" : "hidden"
                }
            >
                {/* 툴바: 검색바 + tail 토글 (파일명은 TabBar가 표시) */}
                <header className="h-10 shrink-0 px-3 flex items-center justify-between border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50/80 dark:bg-zinc-900/80 backdrop-blur space-x-3 relative z-20">
                    <div className="flex-1" />
                    {file && (
                        <div className="flex-1 flex justify-center max-w-2xl">
                            <SearchBar
                                fileId={file.id}
                                getViewportLineNo={currentTopActualLine}
                                filterId={filterId}
                                visibleRows={
                                    viewportHeight > 0
                                        ? viewportHeight / rowHeight
                                        : 30
                                }
                                findSignal={findSignal}
                                isFilterMode={isFilterMode}
                                isActive={isActive}
                                onToggleFilterMode={handleToggleFilterMode}
                                onNavigateToHit={handleNavigateToHit}
                                onSearchStarted={handleSearchStarted}
                                onSearchProgress={handleSearchProgress}
                                onSearchDone={handleSearchDone}
                                onSearchDensityRequest={
                                    handleSearchDensityRequest
                                }
                                onClearSearch={handleClearSearch}
                                currentHit={currentHit}
                                setCurrentHit={setCurrentHit}
                                decompressing={decompressing}
                            />
                        </div>
                    )}
                    <div className="flex items-center space-x-2 shrink-0">
                        {/* 컬럼 뷰 토글: CSV / JSONL 감지 시에만 표시 */}
                        {columns !== null && columns.format !== "plain" && (
                            <button
                                type="button"
                                onClick={handleToggleColumnView}
                                title={
                                    columns.format === "csv"
                                        ? `컬럼 뷰 토글 (⌘⇧C) — CSV 감지 (구분자 "${columns.delimiter ?? ","}"${columns.hasHeader ? ", 헤더 있음" : ""})`
                                        : "컬럼 뷰 토글 (⌘⇧C) — JSON Lines 감지"
                                }
                                className={`px-2.5 py-1 text-xs font-medium rounded-md transition-colors flex items-center space-x-1.5 cursor-pointer ${
                                    columnViewActive
                                        ? "bg-blue-600 text-white hover:bg-blue-500 shadow-xs"
                                        : "bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700"
                                }`}
                            >
                                <span>컬럼</span>
                                <span
                                    className={`text-[10px] px-1 py-0.5 rounded font-mono ${
                                        columnViewActive
                                            ? "bg-blue-700 text-blue-100"
                                            : "bg-zinc-200 dark:bg-zinc-700 text-zinc-500 dark:text-zinc-400"
                                    }`}
                                >
                                    {columns.format === "csv" ? "CSV" : "JSONL"}
                                </span>
                                <kbd
                                    className={`text-[10px] px-1 py-0.5 rounded font-mono ${
                                        columnViewActive
                                            ? "bg-blue-700 text-blue-100"
                                            : "bg-zinc-200 dark:bg-zinc-700 text-zinc-500 dark:text-zinc-400"
                                    }`}
                                >
                                    ⌘⇧C
                                </kbd>
                            </button>
                        )}
                        <button
                            type="button"
                            onClick={handleToggleTail}
                            disabled={
                                !file ||
                                file.compressed ||
                                file.source === "s3" ||
                                file.source === "merge"
                            }
                            title={
                                file?.source === "merge"
                                    ? "병합 뷰는 병합 시점 스냅샷이라 tail 을 지원하지 않습니다"
                                    : file?.source === "s3"
                                      ? "S3 객체는 불변이므로 tail 을 지원하지 않습니다"
                                      : file?.compressed
                                        ? "압축 파일은 tail 을 지원하지 않습니다"
                                        : file?.remote
                                          ? "tail 모드 토글 (⌘T) — 원격: 1초 간격으로 확인합니다"
                                          : "tail 모드 토글 (⌘T) — 파일 끝을 실시간으로 따라갑니다"
                            }
                            className={`px-2.5 py-1 text-xs font-medium rounded-md transition-colors flex items-center space-x-1.5 ${
                                tailEnabled
                                    ? "bg-emerald-600 text-white hover:bg-emerald-500 shadow-xs"
                                    : "bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700"
                            } ${
                                file &&
                                !file.compressed &&
                                file.source !== "s3" &&
                                file.source !== "merge"
                                    ? "cursor-pointer"
                                    : "opacity-40 cursor-not-allowed"
                            }`}
                        >
                            <span>Tail</span>
                            <kbd
                                className={`text-[10px] px-1 py-0.5 rounded font-mono ${
                                    tailEnabled
                                        ? "bg-emerald-700 text-emerald-100"
                                        : "bg-zinc-200 dark:bg-zinc-700 text-zinc-500 dark:text-zinc-400"
                                }`}
                            >
                                ⌘T
                            </kbd>
                        </button>
                    </div>
                </header>

                {/* 다중 필터 바: 파일이 열려 있을 때만 표시 */}
                {file && (
                    <FilterBar
                        spec={filterSpec}
                        onSpecChange={setFilterSpec}
                        focusSignal={filterFocusSignal}
                        isActive={isActive}
                        fileId={file.id}
                        columns={columns}
                    />
                )}

                {/* 병합 원본 범례: 원본별 색·이름·줄 수.
                    클릭하면 해당 원본 탭으로 이동한다 (원본 숨기기는 지원하지 않음) */}
                {isMergeView && mergeSourceList.length > 0 && (
                    <div className="h-8 shrink-0 px-3 flex items-center gap-x-4 border-b border-zinc-200 dark:border-zinc-800 bg-violet-50/50 dark:bg-violet-950/20 overflow-x-auto">
                        <span className="text-[10px] font-semibold text-violet-600 dark:text-violet-400 shrink-0">
                            병합
                        </span>
                        {mergeSourceList.map((s) => (
                            <button
                                key={s.index}
                                type="button"
                                onClick={() => handleLegendClick(s.index)}
                                className="flex items-center gap-1.5 text-xs text-zinc-600 dark:text-zinc-400 hover:text-zinc-900 dark:hover:text-zinc-100 cursor-pointer shrink-0"
                                title={`${s.path} — 클릭하면 이 원본 탭으로 이동`}
                            >
                                <span
                                    className="w-2 h-2 rounded-full shrink-0"
                                    style={{
                                        backgroundColor: mergeSourceColor(
                                            s.index,
                                        ),
                                    }}
                                />
                                <span className="font-medium">
                                    {mergeShortName(s.path)}
                                </span>
                                <span className="text-zinc-400 dark:text-zinc-500">
                                    {s.lineCount.toLocaleString("ko-KR")}줄
                                </span>
                            </button>
                        ))}
                    </div>
                )}

                {/* 로테이션 안내 배너 (5초 후 자동 숨김) */}
                {rotationReason && (
                    <div className="px-4 py-2 bg-sky-50 text-sky-700 dark:bg-sky-950/50 dark:text-sky-300 text-xs flex items-center justify-between border-b border-sky-200 dark:border-sky-900">
                        <span>
                            {rotationReason === "truncated"
                                ? "파일이 잘렸습니다(로테이션). 다시 엽니다."
                                : "파일이 교체됐습니다(로테이션). 다시 엽니다."}
                        </span>
                        <button
                            type="button"
                            onClick={() => setRotationReason(null)}
                            className="font-bold hover:underline cursor-pointer ml-4"
                        >
                            닫기
                        </button>
                    </div>
                )}

                {/* 북마크 stale 안내 바: 저장 이후 파일 내용이 바뀐 경우 */}
                {bookmarksStale && (
                    <div className="px-4 py-2 bg-amber-50 text-amber-800 dark:bg-amber-950/50 dark:text-amber-300 text-xs flex items-center justify-between border-b border-amber-200 dark:border-amber-900">
                        <span className="select-text">
                            파일 내용이 바뀌어 북마크 위치가 다를 수 있습니다.
                        </span>
                        <div className="flex items-center gap-3 ml-4 shrink-0">
                            <button
                                type="button"
                                onClick={handleConfirmStaleBookmarks}
                                className="font-bold hover:underline cursor-pointer"
                                title="현재 파일 기준으로 북마크를 다시 저장합니다"
                            >
                                확인
                            </button>
                            <button
                                type="button"
                                onClick={() => setBookmarksStale(false)}
                                className="hover:underline cursor-pointer"
                            >
                                닫기
                            </button>
                        </div>
                    </div>
                )}

                {/* 에러 메시지 알림 바 (텍스트 선택 가능) */}
                {errorMessage && (
                    <div className="px-4 py-2 bg-rose-50 text-rose-700 dark:bg-rose-950/50 dark:text-rose-300 text-xs flex items-center justify-between border-b border-rose-200 dark:border-rose-900">
                        <span className="select-text">{errorMessage}</span>
                        <button
                            type="button"
                            onClick={() => setErrorMessage(null)}
                            className="font-bold hover:underline cursor-pointer ml-4"
                        >
                            닫기
                        </button>
                    </div>
                )}

                {/* 메인 영역: 가상 스크롤 뷰포트 또는 여는 중 표시 */}
                <main className="relative flex-1 min-h-0 min-w-0 overflow-hidden flex">
                    {file ? (
                        <>
                            <LogViewport
                                file={file}
                                lineCount={activeLineCount}
                                totalFileLines={lineCount}
                                lineCache={activeLineCache}
                                topLine={topLine}
                                onTopLineChange={handleTopLineChange}
                                currentHitLine={currentHitLine}
                                isFilterMode={isFilterMode}
                                filterViewActive={filterViewActive}
                                ruleColors={ruleColors}
                                onLineDoubleClick={handleLineDoubleClick}
                                densityBins={densityBins}
                                onViewportHeightChange={setViewportHeight}
                                onTrackHeightChange={handleTrackHeightChange}
                                fontSize={fontSize}
                                rowHeight={rowHeight}
                                selectedRange={selectedRange}
                                onGutterLineClick={handleGutterLineClick}
                                onTruncatedLineClick={handleTruncatedLineClick}
                                onPinchZoom={onFontSizeAdjust}
                                isActive={isActive}
                                bookmarks={
                                    isMergeView ? undefined : bookmarkLineMap
                                }
                                bookmarkMarkers={
                                    isMergeView ? undefined : bookmarkMarkers
                                }
                                onBookmarkToggle={
                                    isMergeView ? undefined : toggleBookmarkAt
                                }
                                columns={columns}
                                columnView={columnViewActive}
                                onFieldFilter={handleFieldFilter}
                                mergeSources={mergeChips}
                                onJumpToSource={
                                    isMergeView ? jumpToSource : undefined
                                }
                            />
                            {isActive &&
                                fullLinePanel !== null &&
                                fullLinePanel.fileId === file.id && (
                                    <FullLinePanel
                                        key={fullLinePanel.no}
                                        fileId={fullLinePanel.fileId}
                                        lineNo={fullLinePanel.no}
                                        onClose={handleCloseFullLinePanel}
                                    />
                                )}
                            {isActive && bookmarkPanelOpen && (
                                <BookmarkPanel
                                    fileId={file.id}
                                    items={bookmarks}
                                    onClose={handleToggleBookmarkPanel}
                                    onNavigate={handleBookmarkPanelNavigate}
                                    onChangeNote={handleBookmarkNoteChange}
                                    onChangeColor={handleBookmarkColorChange}
                                    onDelete={handleBookmarkDelete}
                                />
                            )}
                        </>
                    ) : (
                        <div className="flex-1 flex items-center justify-center">
                            <span className="text-xs text-zinc-400 dark:text-zinc-500">
                                {errorMessage === null ? "파일을 여는 중…" : ""}
                            </span>
                        </div>
                    )}

                    {/* tail 모드: 위로 스크롤 중 새 줄이 있으면 하단 이동 버튼 표시 */}
                    {tailEnabled &&
                        !isFilterMode &&
                        !isAtBottomNow &&
                        newLinesCount > 0 && (
                            <button
                                type="button"
                                onClick={handleScrollToBottom}
                                className="absolute bottom-3 right-16 z-30 px-3 py-1.5 rounded-full bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900 text-xs font-medium shadow-lg hover:bg-zinc-700 dark:hover:bg-zinc-300 transition-colors cursor-pointer"
                            >
                                새 줄 {newLinesCount.toLocaleString("ko-KR")}개
                                ↓
                            </button>
                        )}
                </main>

                {/* 하단 상태바 */}
                <StatusBar
                    file={file}
                    fileSize={liveFileSize}
                    lineCount={lineCount}
                    indexProgress={indexProgress}
                    topLine={topLine}
                    searchStatus={searchStatus}
                    isFilterMode={isFilterMode}
                    filterViewActive={filterViewActive}
                    filterStatus={filterStatus}
                    actualTopLine={actualTopLine}
                    tailOn={tailEnabled && file !== null}
                    tailPaused={tailEnabled && !isAtBottomNow}
                    copying={copying}
                    selectionLines={
                        selectedRange !== null
                            ? selectedRange.end - selectedRange.start + 1
                            : 0
                    }
                    mergeSourceCount={
                        isMergeView ? mergeSourceList.length : null
                    }
                    mergeSourcePaths={
                        isMergeView
                            ? mergeSourceList.map((s) => s.path)
                            : undefined
                    }
                />

                {/* 줄 번호 이동 모달 (Cmd+L) — 활성 탭에서만 표시 */}
                {isActive && gotoLineOpen && file && (
                    <GotoLineDialog
                        totalLines={lineCount}
                        indexing={indexProgress?.done !== true}
                        mergeMode={file.source === "merge"}
                        onClose={() => setGotoLineOpen(false)}
                        onNavigate={handleGotoNavigate}
                        onNavigateTime={handleGotoTime}
                    />
                )}

                {/* 복사 완료 토스트 (텍스트 선택 가능) */}
                {toast && (
                    <div className="fixed bottom-10 left-1/2 -translate-x-1/2 z-50 px-3 py-1.5 rounded-md bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900 text-xs font-medium shadow-lg select-text">
                        {toast}
                    </div>
                )}
            </div>
        );
    },
);

FileTab.displayName = "FileTab";
