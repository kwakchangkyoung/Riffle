import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview, type DragDropEvent } from "@tauri-apps/api/webview";
import { listen, type Event as TauriEvent } from "@tauri-apps/api/event";
import {
    cancelFilter,
    closeFile,
    copyLines,
    filterIndexOf,
    getRecentFiles,
    getSettings,
    openFile,
    searchDensity,
    setTail,
    startFilter,
    takePendingOpen,
    type FileInfo,
    type FilterSpec,
    type IndexProgress,
    type MenuEvent,
    type QueryOptions,
    type SearchEvent,
    type SearchHit,
    type TailEvent,
} from "@/lib/ipc";
import { checkForUpdate } from "@/lib/updater";
import { LineCache } from "@/lib/lineCache";
import { useTauriListener } from "@/lib/useTauriListener";
import {
    clampFontSize,
    DEFAULT_FONT_SIZE,
    loadFontSize,
    rowHeightFor,
    saveFontSize,
} from "@/lib/fontSize";
import { formatBytes } from "@/lib/format";
import { LogViewport } from "@/components/viewer/LogViewport";
import { FullLinePanel } from "@/components/viewer/FullLinePanel";
import { GotoLineDialog } from "@/components/GotoLineDialog";
import {
    UpdateDialog,
    type UpdateDialogState,
} from "@/components/UpdateDialog";
import { SearchBar } from "@/components/search/SearchBar";
import { FilterBar } from "@/components/filter/FilterBar";
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
        ]),
    });
}

/** 규칙 하이라이트를 계산할 필요가 있는 스펙이면 반환, 아니면 null */
function ruleHighlightSpec(spec: FilterSpec): FilterSpec | null {
    return spec.rules.some((r) => r.enabled && r.kind === "include")
        ? spec
        : null;
}

/**
 * Riffle 메인 애플리케이션 컴포넌트
 */
export default function App() {
    const [file, setFile] = useState<FileInfo | null>(null);
    // tail appended 이벤트의 totalBytes로 갱신되는 표시용 파일 크기 (null이면 file.size 사용)
    const [liveFileSize, setLiveFileSize] = useState<number | null>(null);
    const [lineCount, setLineCount] = useState(0);
    const [indexProgress, setIndexProgress] = useState<IndexProgress | null>(
        null,
    );
    const [topLine, setTopLine] = useState(0);
    const [isDragOver, setIsDragOver] = useState(false);
    const [errorMessage, setErrorMessage] = useState<string | null>(null);

    // 검색 및 필터 모드 관련 상태
    const [isFilterMode, setIsFilterMode] = useState(false);
    const [searchStatus, setSearchStatus] = useState<SearchStatusInfo | null>(
        null,
    );
    const [currentHit, setCurrentHit] = useState<SearchHit | null>(null);
    const [densityBins, setDensityBins] = useState<number[]>([]);
    const [viewportHeight, setViewportHeight] = useState(0);

    // 다중 필터 상태: 스펙(파일 간 세션 유지), 최신/표시 필터 ID, 계산 진행률
    const [filterSpec, setFilterSpec] = useState<FilterSpec>(EMPTY_FILTER_SPEC);
    const [filterId, setFilterId] = useState<number | null>(null);
    const [activeFilterId, setActiveFilterId] = useState<number | null>(null);
    const [filterStatus, setFilterStatus] = useState<FilterStatusInfo | null>(
        null,
    );
    const [filterMatchCount, setFilterMatchCount] = useState(0);
    const [filterFocusSignal, setFilterFocusSignal] = useState(0);

    // tail 모드 상태: 사용자 의도, 일시정지 중 새 줄 수, 로테이션 안내
    const [tailEnabled, setTailEnabled] = useState(false);
    const [newLinesCount, setNewLinesCount] = useState(0);
    const [rotationReason, setRotationReason] = useState<
        "truncated" | "replaced" | null
    >(null);

    // Phase 5 상태: 폰트 크기, 줄 단위 선택, 복사, 줄 이동, 전체 줄 패널, 최근 파일
    const [fontSize, setFontSize] = useState<number>(loadFontSize);
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
    const [recentFiles, setRecentFiles] = useState<string[]>([]);
    const [findSignal, setFindSignal] = useState(0);

    // 업데이트 다이얼로그 상태 (null이면 닫힘)와 관련 ref들
    const [updateDialog, setUpdateDialog] = useState<UpdateDialogState | null>(
        null,
    );
    const updateDialogRef = useRef<UpdateDialogState | null>(null);
    // 확인 요청 시퀀스: 다이얼로그를 닫으면 진행 중이던 확인 결과를 폐기한다
    const updateCheckSeqRef = useRef(0);
    // 같은 세션에서 "나중에"로 닫은 뒤 자동 확인이 다시 띄우지 않게 하는 플래그
    const suppressAutoUpdateRef = useRef(false);

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
    const densityTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    const activeSearchIdRef = useRef<number | null>(null);
    // Done 이벤트가 Started보다 먼저 도착한 경우를 대비한 임시 보관
    const searchDoneRef = useRef<{
        searchId: number;
        total: number;
        capped: boolean;
    } | null>(null);
    const topLineRef = useRef(0);

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

    // 비동기 업데이트 확인 콜백에서 최신 다이얼로그 상태를 읽기 위한 ref 동기화
    useEffect(() => {
        updateDialogRef.current = updateDialog;
    }, [updateDialog]);

    useEffect(() => {
        viewportHeightRef.current = viewportHeight;
    }, [viewportHeight]);

    // 다중 필터 보기 활성 여부 (화면에 표시 중인 필터가 있을 때)
    const filterViewActive = activeFilterId !== null;

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

    // 뷰포트 맨 위 행의 실제 파일 줄 번호.
    // 인덱스 기반 보기(검색 결과만/다중 필터)에서는 캐시된 줄의 실제 번호를 반환한다.
    const currentTopActualLine = useCallback((): number => {
        const t = Math.floor(topLineRef.current);
        if (isFilterModeRef.current) {
            return filteredLineCache.getLine(t)?.no ?? t;
        }
        if (filterViewActiveRef.current) {
            return filterLineCache.getLine(t)?.no ?? t;
        }
        return t;
    }, [filteredLineCache, filterLineCache]);

    // topLine 직접 변경(setTopLine) 경로에 대한 ref 동기화 백업
    useEffect(() => {
        topLineRef.current = topLine;
    }, [topLine]);

    // 맨 아래 여부 추적 (줄 수/뷰포트 변화에 대한 백업 갱신)
    useEffect(() => {
        const maxTop = Math.max(
            0,
            displayLineCount - viewportHeight / rowHeight,
        );
        atBottomRef.current = topLine >= maxTop - 0.5;
    }, [topLine, displayLineCount, viewportHeight, rowHeight]);

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

    // 스크롤바 트랙 높이 변경 콜백 (인라인 함수면 memoized LogViewport이 무력화된다)
    const handleTrackHeightChange = useCallback((h: number) => {
        trackHeightRef.current = h;
    }, []);

    // 파일 열기 처리 함수
    const handleOpenFileByPath = useCallback(
        async (path: string, options?: { preserveTail?: boolean }) => {
            const seq = ++openSeqRef.current;
            setErrorMessage(null);
            latestProgressRef.current = null;
            lastProgressErrorRef.current = null;

            // 진행 중이던 tail appended 배치 취소 (이전 파일 기준 이벤트는 폐기)
            pendingAppendedRef.current = null;
            if (tailRafRef.current !== null) {
                cancelAnimationFrame(tailRafRef.current);
                tailRafRef.current = null;
            }

            // 이전 파일이 열려있다면 닫기 요청
            if (currentFileRef.current) {
                const prevId = currentFileRef.current.id;
                closeFile(prevId).catch((err) => {
                    console.warn("이전 파일 닫기 실패:", err);
                });
            }

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

            // 다중 필터 계산 상태 초기화 (스펙 자체는 파일 간 세션 유지)
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
                const info = await openFile(path, (progress) => {
                    // 최신 열기 작업이 아닌 경우 콜백 무시
                    if (openSeqRef.current !== seq) return;
                    latestProgressRef.current = progress;
                    setIndexProgress(progress);
                    setLineCount(progress.lineCount);
                    normalLineCache.setLineCount(
                        progress.lineCount,
                        progress.done,
                    );
                    // gzip 해제 실패/디스크 부족 등: 그 전까지 해제된 내용은 계속 사용 가능
                    if (
                        progress.error !== null &&
                        progress.error !== lastProgressErrorRef.current
                    ) {
                        lastProgressErrorRef.current = progress.error;
                        setErrorMessage(
                            `압축 해제 실패: ${progress.error} (그때까지 해제된 내용은 볼 수 있습니다)`,
                        );
                    }
                });

                // 열리는 도중 다른 파일 열기 요청이 시작되었으면 취소
                if (openSeqRef.current !== seq) {
                    closeFile(info.id).catch(() => {});
                    return;
                }

                currentFileRef.current = info;
                setFile(info);
                normalLineCache.setFile(info.id);

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
        [normalLineCache, filteredLineCache, filterLineCache],
    );

    // 파일 선택 다이얼로그 호출
    const handleOpenDialog = useCallback(async () => {
        try {
            const selected = await open({
                multiple: false,
                directory: false,
            });
            if (typeof selected === "string") {
                await handleOpenFileByPath(selected);
            }
        } catch (err) {
            console.error("파일 열기 다이얼로그 호출 실패:", err);
        }
    }, [handleOpenFileByPath]);

    // tail 모드 토글 (버튼 / Cmd+T). 켜기 방향은 인덱싱 완료 effect가 처리한다.
    const handleToggleTail = useCallback(() => {
        const current = currentFileRef.current;
        // 압축 파일(.gz)은 임시 파일 해제 방식이라 tail 모드를 지원하지 않는다
        if (!current || current.compressed) return;
        const next = !tailEnabledRef.current;
        tailEnabledRef.current = next;
        setTailEnabled(next);
        if (!next) {
            setNewLinesCount(0);
            pendingAppendedRef.current = null;
            queueTailCommand(() => setTail(current.id, false, () => {})).catch(
                (err) => {
                    console.warn("tail 모드 해제 실패:", err);
                },
            );
        }
    }, [queueTailCommand]);

    // Finder 파일 연결("open-file" 이벤트)과 콜드 스타트 대기 경로가
    // 동시에 도착할 수 있으므로 같은 경로의 중복 열기를 짧은 시간 내에 무시한다
    const lastFinderOpenRef = useRef<{ path: string; at: number } | null>(null);
    const requestFinderOpen = useCallback(
        (path: string) => {
            const now = Date.now();
            const last = lastFinderOpenRef.current;
            if (last !== null && last.path === path && now - last.at < 1000) {
                return;
            }
            lastFinderOpenRef.current = { path, at: now };
            void handleOpenFileByPath(path);
        },
        [handleOpenFileByPath],
    );

    // Finder "다음으로 열기"/더블클릭으로 앱이 실행 중일 때 오는 파일 열기 이벤트
    useTauriListener(
        (handler) => listen<string>("open-file", handler),
        (event: TauriEvent<string>) => {
            if (event.payload) {
                requestFinderOpen(event.payload);
            }
        },
    );

    // 콜드 스타트: Opened 이벤트가 리스너 등록 전에 도착한 경우
    // 백엔드에 보관된 대기 경로를 한 번만 가져와 연다
    useEffect(() => {
        takePendingOpen()
            .then((path) => {
                if (path) {
                    requestFinderOpen(path);
                }
            })
            .catch((err) => {
                console.warn("대기 파일 경로 조회 실패:", err);
            });
    }, [requestFinderOpen]);

    // Tauri 윈도우 드래그 앤 드롭 이벤트 등록
    useTauriListener(
        (handler) => {
            try {
                return getCurrentWebview().onDragDropEvent(handler);
            } catch (err) {
                console.warn("getCurrentWebview 호출 실패:", err);
                return Promise.resolve(() => {});
            }
        },
        (event: TauriEvent<DragDropEvent>) => {
            const payload = event.payload;
            if (payload.type === "enter" || payload.type === "over") {
                setIsDragOver(true);
            } else if (payload.type === "leave") {
                setIsDragOver(false);
            } else if (payload.type === "drop") {
                setIsDragOver(false);
                if (payload.paths && payload.paths.length > 0) {
                    handleOpenFileByPath(payload.paths[0]);
                }
            }
        },
    );

    // 검색 밀도 요청 (최대 4회/초 스로틀)
    const requestDensityFetch = useCallback(
        (searchId: number) => {
            const totalLines = lineCountRef.current;
            if (totalLines <= 0) return;

            const trackH = trackHeightRef.current || viewportHeight || 600;
            const bins = Math.max(10, Math.min(2000, Math.floor(trackH / 2)));

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
                handleTopLineChange(Math.max(0, Math.min(maxTop, targetTop)));
            };

            if (isFilterMode) {
                scrollToIndex(hit.index);
            } else if (filterViewActive && activeFilterIdRef.current !== null) {
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
                            viewportHeightRef.current / rowHeightRef.current,
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
                newLineCount - viewportHeightRef.current / rowHeightRef.current,
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
                    oldLineCount: prev ? prev.oldLineCount : event.oldLineCount,
                    lineCount: event.lineCount,
                    totalBytes: event.totalBytes,
                    filterCount: event.filterCount ?? prev?.filterCount ?? null,
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

    // 폰트 크기 조절 (localStorage에 저장)
    const adjustFontSize = useCallback((delta: number) => {
        setFontSize((prev) => {
            const next = clampFontSize(prev + delta);
            saveFontSize(next);
            return next;
        });
    }, []);

    const handleFontReset = useCallback(() => {
        setFontSize(DEFAULT_FONT_SIZE);
        saveFontSize(DEFAULT_FONT_SIZE);
    }, []);

    // 거터 줄 번호 클릭: 일반 클릭은 해당 줄 선택, Shift+클릭은 anchor~클릭 범위
    const handleGutterLineClick = useCallback(
        (index: number, shiftKey: boolean) => {
            // 인덱스 기반 보기(검색 결과만/다중 필터)에서는 줄 선택을 지원하지 않는다
            if (isFilterModeRef.current || filterViewActiveRef.current) return;
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
            const msg = typeof err === "string" ? err : "복사에 실패했습니다.";
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
        setFullLinePanel({ fileId: current.id, no: lineNo });
    }, []);

    const handleCloseFullLinePanel = useCallback(() => {
        setFullLinePanel(null);
    }, []);

    // ===== 앱 업데이트 =====

    // 업데이트 확인 실행.
    // visible=true(메뉴 수동 확인)는 결과를 항상 다이얼로그로 표시하고,
    // visible=false(시작 시 자동 확인)는 새 버전이 있을 때만 조용히 표시한다.
    const runUpdateCheck = useCallback((visible: boolean) => {
        const seq = ++updateCheckSeqRef.current;
        if (visible) {
            setUpdateDialog({ status: "checking" });
        }
        checkForUpdate()
            .then((update) => {
                const stale = seq !== updateCheckSeqRef.current;
                if (update === null) {
                    if (!stale && visible) {
                        setUpdateDialog({ status: "upToDate" });
                    }
                    return;
                }
                // 표시하지 않기로 한 업데이트는 Rust 측 리소스를 해제한다
                const suppress =
                    !visible &&
                    (updateDialogRef.current !== null ||
                        suppressAutoUpdateRef.current);
                if (stale || suppress) {
                    update.close().catch(() => {});
                    return;
                }
                setUpdateDialog({ status: "available", update });
            })
            .catch((err: unknown) => {
                if (seq !== updateCheckSeqRef.current) return;
                if (visible) {
                    const msg =
                        err instanceof Error
                            ? err.message
                            : typeof err === "string"
                              ? err
                              : "업데이트 확인에 실패했습니다.";
                    setUpdateDialog({ status: "error", message: msg });
                } else {
                    // 자동 확인의 오류는 조용히 콘솔에만 남긴다
                    console.warn("업데이트 자동 확인 실패:", err);
                }
            });
    }, []);

    // 업데이트 다이얼로그 닫기 ("나중에"/Esc/배경 클릭 공통)
    const handleUpdateDialogClose = useCallback(() => {
        // 진행 중인 확인의 늦은 결과가 다이얼로그를 다시 열지 못하게 무효화한다
        updateCheckSeqRef.current += 1;
        suppressAutoUpdateRef.current = true;
        const current = updateDialogRef.current;
        if (current?.status === "available") {
            current.update.close().catch(() => {});
        }
        setUpdateDialog(null);
    }, []);

    // 시작 시 자동 업데이트 확인: 로드 3초 후, 설정이 켜져 있고 dev 모드가 아닐 때만.
    // 업데이트가 없거나 네트워크 오류면 아무것도 표시하지 않는다.
    useEffect(() => {
        if (import.meta.env.DEV) return;
        const timer = setTimeout(() => {
            getSettings()
                .then((settings) => {
                    if (settings.autoCheckUpdates) {
                        runUpdateCheck(false);
                    }
                })
                .catch((err) => {
                    console.warn("설정 조회 실패:", err);
                });
        }, 3000);
        return () => clearTimeout(timer);
    }, [runUpdateCheck]);

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

    // 네이티브 메뉴 이벤트 수신 (메뉴 accelerator가 키를 먼저 가로채므로
    // 해당 단축키는 프론트 keydown이 아니라 이 이벤트로만 처리한다)
    useTauriListener(
        (handler) => listen<MenuEvent>("menu", handler),
        (event: TauriEvent<MenuEvent>) => {
            const { id, path } = event.payload;
            switch (id) {
                case "open":
                    handleOpenDialog();
                    break;
                case "open_recent":
                    if (path) handleOpenFileByPath(path);
                    break;
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
                    handleTopLineChange(0);
                    break;
                case "go_bottom":
                    handleScrollToBottom();
                    break;
                case "toggle_tail":
                    handleToggleTail();
                    break;
                case "check_updates":
                    runUpdateCheck(true);
                    break;
                case "font_inc":
                    adjustFontSize(1);
                    break;
                case "font_dec":
                    adjustFontSize(-1);
                    break;
                case "font_reset":
                    handleFontReset();
                    break;
                default:
                    break;
            }
        },
    );

    // Esc: 줄 선택 해제 (모달/패널/입력창에서는 각 컴포넌트가 우선 처리)
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.key !== "Escape") return;
            const active = document.activeElement;
            if (
                active instanceof HTMLInputElement ||
                active instanceof HTMLTextAreaElement
            ) {
                return;
            }
            if (gotoLineOpen || fullLinePanel !== null || updateDialog !== null)
                return;
            setLineSelection(null);
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [gotoLineOpen, fullLinePanel, updateDialog]);

    // 토스트 메시지는 2초 후 자동으로 사라진다
    useEffect(() => {
        if (toast === null) return;
        const timer = setTimeout(() => setToast(null), 2000);
        return () => clearTimeout(timer);
    }, [toast]);

    // 파일이 닫혀 있으면 최근 파일 목록을 불러와 빈 화면에 표시한다
    useEffect(() => {
        if (file !== null) return;
        getRecentFiles()
            .then(setRecentFiles)
            .catch((err) => {
                console.warn("최근 파일 목록 조회 실패:", err);
                setRecentFiles([]);
            });
    }, [file]);

    // 로테이션 배너는 5초 후 자동으로 사라진다
    useEffect(() => {
        if (!rotationReason) return;
        const timer = setTimeout(() => setRotationReason(null), 5000);
        return () => clearTimeout(timer);
    }, [rotationReason]);

    const fileName = file ? file.path.split("/").pop() || file.path : null;
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
        <div className="flex flex-col h-screen w-screen overflow-hidden bg-white text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100 select-none">
            {/* 상단 바: 파일명 + 검색바 + 파일 열기 버튼 */}
            <header className="h-10 shrink-0 px-3 flex items-center justify-between border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50/80 dark:bg-zinc-900/80 backdrop-blur space-x-3 relative z-20">
                <div className="flex items-center space-x-3 truncate shrink-0">
                    <span className="font-semibold text-sm tracking-tight text-zinc-800 dark:text-zinc-200">
                        Riffle
                    </span>
                    {fileName && (
                        <>
                            <span className="text-zinc-300 dark:text-zinc-700">
                                /
                            </span>
                            <span
                                className="text-xs font-mono text-zinc-600 dark:text-zinc-400 truncate max-w-[280px]"
                                title={file?.path}
                            >
                                {fileName}
                            </span>
                        </>
                    )}
                </div>

                {/* 중앙: 검색바 (파일이 열려 있을 때 활성화) */}
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
                            onToggleFilterMode={handleToggleFilterMode}
                            onNavigateToHit={handleNavigateToHit}
                            onSearchStarted={handleSearchStarted}
                            onSearchProgress={handleSearchProgress}
                            onSearchDone={handleSearchDone}
                            onSearchDensityRequest={handleSearchDensityRequest}
                            onClearSearch={handleClearSearch}
                            currentHit={currentHit}
                            setCurrentHit={setCurrentHit}
                            decompressing={decompressing}
                        />
                    </div>
                )}

                {/* 우측: tail 토글 + 파일 열기 버튼 */}
                <div className="flex items-center space-x-2 shrink-0">
                    <button
                        type="button"
                        onClick={handleToggleTail}
                        disabled={!file || file.compressed}
                        title={
                            file?.compressed
                                ? "압축 파일은 tail 을 지원하지 않습니다"
                                : "tail 모드 토글 (⌘T) — 파일 끝을 실시간으로 따라갑니다"
                        }
                        className={`px-2.5 py-1 text-xs font-medium rounded-md transition-colors flex items-center space-x-1.5 ${
                            tailEnabled
                                ? "bg-emerald-600 text-white hover:bg-emerald-500 shadow-xs"
                                : "bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700"
                        } ${
                            file && !file.compressed
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
                    <button
                        type="button"
                        onClick={handleOpenDialog}
                        className="px-2.5 py-1 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 transition-colors flex items-center space-x-1.5 cursor-pointer shadow-xs"
                    >
                        <span>파일 열기</span>
                        <kbd className="text-[10px] px-1 py-0.5 rounded bg-zinc-800 dark:bg-zinc-200 text-zinc-300 dark:text-zinc-700 font-mono">
                            ⌘O
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
                />
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

            {/* 에러 메시지 알림 바 */}
            {errorMessage && (
                <div className="px-4 py-2 bg-rose-50 text-rose-700 dark:bg-rose-950/50 dark:text-rose-300 text-xs flex items-center justify-between border-b border-rose-200 dark:border-rose-900">
                    <span>{errorMessage}</span>
                    <button
                        type="button"
                        onClick={() => setErrorMessage(null)}
                        className="font-bold hover:underline cursor-pointer ml-4"
                    >
                        닫기
                    </button>
                </div>
            )}

            {/* 메인 영역: 가상 스크롤 뷰포트 또는 빈 상태 안내 */}
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
                        />
                        {fullLinePanel !== null &&
                            fullLinePanel.fileId === file.id && (
                                <FullLinePanel
                                    key={fullLinePanel.no}
                                    fileId={fullLinePanel.fileId}
                                    lineNo={fullLinePanel.no}
                                    onClose={handleCloseFullLinePanel}
                                />
                            )}
                    </>
                ) : (
                    <div
                        onClick={handleOpenDialog}
                        className="flex-1 flex flex-col items-center justify-center cursor-pointer hover:bg-zinc-50/50 dark:hover:bg-zinc-900/30 transition-colors"
                    >
                        <div className="max-w-md p-8 rounded-2xl border-2 border-dashed border-zinc-300 dark:border-zinc-800 flex flex-col items-center text-center space-y-3">
                            <div className="w-12 h-12 rounded-full bg-zinc-100 dark:bg-zinc-900 flex items-center justify-center text-zinc-400 dark:text-zinc-500">
                                <svg
                                    xmlns="http://www.w3.org/2000/svg"
                                    className="w-6 h-6"
                                    fill="none"
                                    viewBox="0 0 24 24"
                                    stroke="currentColor"
                                >
                                    <path
                                        strokeLinecap="round"
                                        strokeLinejoin="round"
                                        strokeWidth={1.5}
                                        d="M9 13h6m-3-3v6m5 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z"
                                    />
                                </svg>
                            </div>
                            <div className="space-y-1">
                                <p className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
                                    Cmd+O 로 열거나 파일을 끌어다 놓으세요
                                </p>
                                <p className="text-xs text-zinc-500 dark:text-zinc-400">
                                    대용량 텍스트 로그 파일을 빠르게 탐색할 수
                                    있습니다
                                </p>
                            </div>
                        </div>

                        {/* 최근 파일 목록 */}
                        {recentFiles.length > 0 && (
                            <div
                                className="mt-6 w-full max-w-md"
                                onClick={(e) => e.stopPropagation()}
                            >
                                <div className="text-xs text-zinc-500 dark:text-zinc-400 mb-2 text-center">
                                    최근 파일
                                </div>
                                <div className="rounded-lg border border-zinc-200 dark:border-zinc-800 bg-white/70 dark:bg-zinc-900/70 divide-y divide-zinc-100 dark:divide-zinc-800 overflow-hidden">
                                    {recentFiles.slice(0, 10).map((path) => (
                                        <button
                                            key={path}
                                            type="button"
                                            onClick={() =>
                                                handleOpenFileByPath(path)
                                            }
                                            className="w-full px-3 py-1.5 text-left text-xs font-mono text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800 truncate cursor-pointer"
                                            title={path}
                                        >
                                            {path}
                                        </button>
                                    ))}
                                </div>
                            </div>
                        )}
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
                            새 줄 {newLinesCount.toLocaleString("ko-KR")}개 ↓
                        </button>
                    )}

                {/* 창 드래그 오버 안내 오버레이 */}
                {isDragOver && (
                    <div className="absolute inset-0 bg-blue-500/10 dark:bg-blue-500/20 backdrop-blur-xs border-2 border-dashed border-blue-500 flex items-center justify-center z-50 pointer-events-none">
                        <div className="px-4 py-2 rounded-lg bg-blue-600 text-white font-medium text-sm shadow-lg">
                            여기에 놓아 파일 열기
                        </div>
                    </div>
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
            />

            {/* 줄 번호 이동 모달 (Cmd+L) */}
            {gotoLineOpen && file && (
                <GotoLineDialog
                    totalLines={lineCount}
                    onClose={() => setGotoLineOpen(false)}
                    onNavigate={handleGotoNavigate}
                />
            )}

            {/* 앱 업데이트 모달 (status가 바뀌면 리마운트해 설치 상태를 초기화) */}
            {updateDialog !== null && (
                <UpdateDialog
                    key={updateDialog.status}
                    state={updateDialog}
                    onClose={handleUpdateDialogClose}
                />
            )}

            {/* 복사 완료 토스트 */}
            {toast && (
                <div className="fixed bottom-10 left-1/2 -translate-x-1/2 z-50 px-3 py-1.5 rounded-md bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900 text-xs font-medium shadow-lg pointer-events-none">
                    {toast}
                </div>
            )}
        </div>
    );
}
