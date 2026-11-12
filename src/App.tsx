import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
    closeFile,
    openFile,
    searchDensity,
    setTail,
    type FileInfo,
    type IndexProgress,
    type QueryOptions,
    type SearchHit,
    type TailEvent,
} from "@/lib/ipc";
import { LineCache } from "@/lib/lineCache";
import { LogViewport } from "@/components/viewer/LogViewport";
import { SearchBar } from "@/components/search/SearchBar";
import { StatusBar, type SearchStatusInfo } from "@/components/StatusBar";

const ROW_HEIGHT = 20;

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

    // tail 모드 상태: 사용자 의도, 일시정지 중 새 줄 수, 로테이션 안내
    const [tailEnabled, setTailEnabled] = useState(false);
    const [newLinesCount, setNewLinesCount] = useState(0);
    const [rotationReason, setRotationReason] = useState<
        "truncated" | "replaced" | null
    >(null);

    const normalLineCache = useMemo(() => new LineCache(), []);
    const filteredLineCache = useMemo(() => new LineCache(), []);

    const [, setCacheVersion] = useState(0);
    useEffect(() => {
        const u1 = normalLineCache.subscribe(() =>
            setCacheVersion((v) => v + 1),
        );
        const u2 = filteredLineCache.subscribe(() =>
            setCacheVersion((v) => v + 1),
        );
        return () => {
            u1();
            u2();
        };
    }, [normalLineCache, filteredLineCache]);

    const currentFileRef = useRef<FileInfo | null>(null);
    const openSeqRef = useRef(0);
    const latestProgressRef = useRef<IndexProgress | null>(null);
    const lineCountRef = useRef(0);
    useEffect(() => {
        lineCountRef.current = lineCount;
    }, [lineCount]);

    const trackHeightRef = useRef(0);
    const lastDensityTimeRef = useRef(0);
    const densityTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
    const activeSearchIdRef = useRef<number | null>(null);

    // tail 모드 관련 ref
    const tailEnabledRef = useRef(false);
    const pendingAppendedRef = useRef<{
        oldLineCount: number;
        lineCount: number;
        totalBytes: number;
    } | null>(null);
    const tailRafRef = useRef<number | null>(null);
    const atBottomRef = useRef(true);
    const isFilterModeRef = useRef(false);
    const viewportHeightRef = useRef(0);
    const scrollToBottomAfterReopenRef = useRef(false);

    useEffect(() => {
        isFilterModeRef.current = isFilterMode;
    }, [isFilterMode]);

    useEffect(() => {
        viewportHeightRef.current = viewportHeight;
    }, [viewportHeight]);

    // 활성 라인 캐시 및 총 라인 수 (필터 모드 / 일반 모드 분기)
    const activeLineCache = isFilterMode ? filteredLineCache : normalLineCache;
    const activeLineCount = isFilterMode
        ? (searchStatus?.matchCount ?? 0)
        : lineCount;

    // 표시 줄 수 (필터 모드: 매치 건수, 일반 모드: 파일 줄 수)
    const displayLineCount = isFilterMode
        ? (searchStatus?.matchCount ?? 0)
        : lineCount;
    const displayCountRef = useRef(0);
    useEffect(() => {
        displayCountRef.current = displayLineCount;
    }, [displayLineCount]);

    // 뷰포트 topLine 변경 래퍼: 맨 아래 여부를 즉시 갱신하고 도달 시 새 줄 카운터를 리셋한다
    const handleTopLineChange = useCallback((v: number) => {
        setTopLine(v);
        const maxTop = Math.max(
            0,
            displayCountRef.current - viewportHeightRef.current / ROW_HEIGHT,
        );
        const atBottom = v >= maxTop - 0.5;
        atBottomRef.current = atBottom;
        if (atBottom) {
            setNewLinesCount(0);
        }
    }, []);

    // 맨 아래 여부 추적 (줄 수/뷰포트 변화에 대한 백업 갱신)
    useEffect(() => {
        const maxTop = Math.max(
            0,
            displayLineCount - viewportHeight / ROW_HEIGHT,
        );
        atBottomRef.current = topLine >= maxTop - 0.5;
    }, [topLine, displayLineCount, viewportHeight]);

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

    // 파일 열기 처리 함수
    const handleOpenFileByPath = useCallback(
        async (path: string, options?: { preserveTail?: boolean }) => {
            const seq = ++openSeqRef.current;
            setErrorMessage(null);
            latestProgressRef.current = null;

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
            setNewLinesCount(0);

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
        [normalLineCache, filteredLineCache],
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
        if (!current) return;
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

    // Cmd+O / Cmd+T 단축키 등록
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.metaKey && e.key.toLowerCase() === "o") {
                e.preventDefault();
                handleOpenDialog();
            } else if (e.metaKey && e.key.toLowerCase() === "t") {
                e.preventDefault();
                handleToggleTail();
            }
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [handleOpenDialog, handleToggleTail]);

    // Tauri 윈도우 드래그 앤 드롭 이벤트 등록
    useEffect(() => {
        let unlistenFn: (() => void) | undefined;

        try {
            const webview = getCurrentWebview();
            webview
                .onDragDropEvent((event) => {
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
                })
                .then((unlisten) => {
                    unlistenFn = unlisten;
                })
                .catch((err) => {
                    console.warn("드래그 앤 드롭 리스너 등록 실패:", err);
                });
        } catch (err) {
            console.warn("getCurrentWebview 호출 실패:", err);
        }

        return () => {
            if (unlistenFn) unlistenFn();
        };
    }, [handleOpenFileByPath]);

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
            if (file) {
                filteredLineCache.setFilteredSource(
                    file.id,
                    searchId,
                    queryDto,
                );
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
        [file, normalLineCache, filteredLineCache, handleSearchDensityRequest],
    );

    // 검색 진행률 업데이트 시 콜백
    const handleSearchProgress = useCallback(
        (matchCount: number) => {
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
        (total: number, capped: boolean) => {
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
            if (activeSearchIdRef.current !== null) {
                requestDensityFetch(activeSearchIdRef.current);
            }
        },
        [filteredLineCache, requestDensityFetch],
    );

    // 검색어 초기화/지우기 시 콜백
    const handleClearSearch = useCallback(() => {
        activeSearchIdRef.current = null;
        if (densityTimerRef.current) {
            clearTimeout(densityTimerRef.current);
            densityTimerRef.current = null;
        }
        normalLineCache.setQuery(null);
        filteredLineCache.clear();
        setIsFilterMode(false);
        setSearchStatus(null);
        setCurrentHit(null);
        setDensityBins([]);
    }, [normalLineCache, filteredLineCache]);

    // 검색 결과 항목으로 뷰포트 이동
    const handleNavigateToHit = useCallback(
        (hit: SearchHit) => {
            setCurrentHit(hit);
            setSearchStatus((prev) =>
                prev ? { ...prev, currentHitIndex: hit.index } : null,
            );

            // 해당 줄이 화면 위에서 1/3 지점에 오도록 topLine 설정
            const targetIndex = isFilterMode ? hit.index : hit.line;
            const visibleRows =
                viewportHeight > 0 ? viewportHeight / ROW_HEIGHT : 30;
            const targetTop = targetIndex - Math.floor(visibleRows / 3);
            const maxTop = Math.max(0, activeLineCount - visibleRows);
            const clampedTop = Math.max(0, Math.min(maxTop, targetTop));
            handleTopLineChange(clampedTop);
        },
        [isFilterMode, viewportHeight, activeLineCount, handleTopLineChange],
    );

    // 필터 모드 토글
    const handleToggleFilterMode = useCallback(() => {
        setIsFilterMode((prev) => !prev);
        handleTopLineChange(0);
    }, [handleTopLineChange]);

    // 필터 모드에서 행 더블클릭 시 필터 해제 후 원본 줄로 이동
    const handleLineDoubleClick = useCallback(
        (originalLineNo: number) => {
            setIsFilterMode(false);
            const visibleRows =
                viewportHeight > 0 ? viewportHeight / ROW_HEIGHT : 30;
            const targetTop = originalLineNo - Math.floor(visibleRows / 3);
            const maxTop = Math.max(0, lineCount - visibleRows);
            const clampedTop = Math.max(0, Math.min(maxTop, targetTop));
            handleTopLineChange(clampedTop);
        },
        [viewportHeight, lineCount, handleTopLineChange],
    );

    // ===== tail 모드 =====

    // rAF 단위로 묶인 appended 이벤트를 반영한다 (프레임당 마지막 배치만 처리)
    const flushTailAppended = useCallback(() => {
        tailRafRef.current = null;
        const pending = pendingAppendedRef.current;
        pendingAppendedRef.current = null;
        if (!pending) return;

        const { oldLineCount, lineCount: newLineCount, totalBytes } = pending;

        // 줄 수/파일 크기 갱신 + 개행 없이 끝나던 마지막 줄이 속한 블록 무효화
        normalLineCache.setLineCount(newLineCount, true);
        setLineCount(newLineCount);
        setLiveFileSize(totalBytes);
        if (oldLineCount > 0) {
            normalLineCache.invalidateLine(oldLineCount - 1);
        }

        // 필터 모드에서는 스크롤/버튼 처리 없이 캐시만 최신화한다
        if (isFilterModeRef.current) return;

        if (atBottomRef.current) {
            // 맨 아래에 있으면 새 끝으로 자동 스크롤
            const maxTop = Math.max(
                0,
                newLineCount - viewportHeightRef.current / ROW_HEIGHT,
            );
            handleTopLineChange(maxTop);
        } else {
            const added = Math.max(0, newLineCount - oldLineCount);
            setNewLinesCount((prev) => prev + added);
        }
    }, [normalLineCache, handleTopLineChange]);

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
                lineCountRef.current - viewportHeightRef.current / ROW_HEIGHT,
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
            lineCountRef.current - viewportHeightRef.current / ROW_HEIGHT,
        );
        handleTopLineChange(maxTop);
    }, [handleTopLineChange]);

    // 로테이션 배너는 5초 후 자동으로 사라진다
    useEffect(() => {
        if (!rotationReason) return;
        const timer = setTimeout(() => setRotationReason(null), 5000);
        return () => clearTimeout(timer);
    }, [rotationReason]);

    const fileName = file ? file.path.split("/").pop() || file.path : null;
    const currentHitLine = currentHit !== null ? currentHit.line : null;

    // 필터 모드 시 화면 맨 위 행의 실제 원본 파일 줄 번호 계산
    const topFilteredRow = isFilterMode
        ? filteredLineCache.getLine(Math.floor(topLine))
        : undefined;
    const actualTopLine = topFilteredRow ? topFilteredRow.no + 1 : null;

    // 현재 렌더 기준 맨 아래 여부 (상태바 tail 일시정지 표시용)
    const maxTopNow = Math.max(
        0,
        displayLineCount - viewportHeight / ROW_HEIGHT,
    );
    const isAtBottomNow = topLine >= maxTopNow - 0.5;

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
                            topLine={topLine}
                            visibleRows={
                                viewportHeight > 0
                                    ? viewportHeight / ROW_HEIGHT
                                    : 30
                            }
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
                        />
                    </div>
                )}

                {/* 우측: tail 토글 + 파일 열기 버튼 */}
                <div className="flex items-center space-x-2 shrink-0">
                    <button
                        type="button"
                        onClick={handleToggleTail}
                        disabled={!file}
                        title="tail 모드 토글 (⌘T) — 파일 끝을 실시간으로 따라갑니다"
                        className={`px-2.5 py-1 text-xs font-medium rounded-md transition-colors flex items-center space-x-1.5 ${
                            tailEnabled
                                ? "bg-emerald-600 text-white hover:bg-emerald-500 shadow-xs"
                                : "bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700"
                        } ${
                            file
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
                    <LogViewport
                        file={file}
                        lineCount={activeLineCount}
                        totalFileLines={lineCount}
                        lineCache={activeLineCache}
                        topLine={topLine}
                        onTopLineChange={handleTopLineChange}
                        currentHitLine={currentHitLine}
                        isFilterMode={isFilterMode}
                        onLineDoubleClick={handleLineDoubleClick}
                        densityBins={densityBins}
                        onViewportHeightChange={setViewportHeight}
                        onTrackHeightChange={(h) => {
                            trackHeightRef.current = h;
                        }}
                    />
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
                actualTopLine={actualTopLine}
                tailOn={tailEnabled && file !== null}
                tailPaused={tailEnabled && !isAtBottomNow}
            />
        </div>
    );
}
