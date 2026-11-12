import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import {
    closeFile,
    openFile,
    searchDensity,
    type FileInfo,
    type IndexProgress,
    type QueryOptions,
    type SearchHit,
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

    // 활성 라인 캐시 및 총 라인 수 (필터 모드 / 일반 모드 분기)
    const activeLineCache = isFilterMode ? filteredLineCache : normalLineCache;
    const activeLineCount = isFilterMode
        ? (searchStatus?.matchCount ?? 0)
        : lineCount;

    // 파일 열기 처리 함수
    const handleOpenFileByPath = useCallback(
        async (path: string) => {
            const seq = ++openSeqRef.current;
            setErrorMessage(null);
            latestProgressRef.current = null;

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
            setLineCount(0);
            setIndexProgress(null);
            setTopLine(0);
            setIsFilterMode(false);
            setSearchStatus(null);
            setCurrentHit(null);
            setDensityBins([]);
            activeSearchIdRef.current = null;

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

    // Cmd+O 단축키 등록
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.metaKey && e.key.toLowerCase() === "o") {
                e.preventDefault();
                handleOpenDialog();
            }
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [handleOpenDialog]);

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
            setTopLine(clampedTop);
        },
        [isFilterMode, viewportHeight, activeLineCount],
    );

    // 필터 모드 토글
    const handleToggleFilterMode = useCallback(() => {
        setIsFilterMode((prev) => {
            const next = !prev;
            setTopLine(0);
            return next;
        });
    }, []);

    // 필터 모드에서 행 더블클릭 시 필터 해제 후 원본 줄로 이동
    const handleLineDoubleClick = useCallback(
        (originalLineNo: number) => {
            setIsFilterMode(false);
            const visibleRows =
                viewportHeight > 0 ? viewportHeight / ROW_HEIGHT : 30;
            const targetTop = originalLineNo - Math.floor(visibleRows / 3);
            const maxTop = Math.max(0, lineCount - visibleRows);
            const clampedTop = Math.max(0, Math.min(maxTop, targetTop));
            setTopLine(clampedTop);
        },
        [viewportHeight, lineCount],
    );

    const fileName = file ? file.path.split("/").pop() || file.path : null;
    const currentHitLine = currentHit !== null ? currentHit.line : null;

    // 필터 모드 시 화면 맨 위 행의 실제 원본 파일 줄 번호 계산
    const topFilteredRow = isFilterMode
        ? filteredLineCache.getLine(Math.floor(topLine))
        : undefined;
    const actualTopLine = topFilteredRow ? topFilteredRow.no + 1 : null;

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

                {/* 우측: 파일 열기 버튼 */}
                <div className="flex items-center space-x-2 shrink-0">
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
                        onTopLineChange={setTopLine}
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
                lineCount={lineCount}
                indexProgress={indexProgress}
                topLine={topLine}
                searchStatus={searchStatus}
                isFilterMode={isFilterMode}
                actualTopLine={actualTopLine}
            />
        </div>
    );
}
