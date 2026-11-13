import React, { useCallback, useEffect, useRef, useState } from "react";
import {
    cancelSearch,
    search,
    searchHitAt,
    searchNav,
    validateQuery,
    type QueryOptions,
    type SearchEvent,
    type SearchHit,
} from "@/lib/ipc";

interface SearchBarProps {
    fileId: number | null;
    getViewportLineNo: () => number; // 뷰포트 맨 위 실제 줄 번호 조회 (매 프레임 리렌더 방지)
    filterId: number | null; // 활성 다중 필터 ID (검색 범위 제한용)
    visibleRows: number;
    findSignal?: number; // 증가할 때마다 검색 입력창에 포커스 (메뉴 찾기 이벤트용)
    isFilterMode: boolean;
    onToggleFilterMode: () => void;
    onNavigateToHit: (hit: SearchHit) => void;
    onSearchStarted: (query: QueryOptions, searchId: number) => void;
    onSearchProgress: (searchId: number, matchCount: number) => void;
    onSearchDone: (searchId: number, total: number, capped: boolean) => void;
    onSearchDensityRequest: (searchId: number) => void;
    onClearSearch: () => void;
    currentHit: SearchHit | null;
    setCurrentHit: React.Dispatch<React.SetStateAction<SearchHit | null>>;
    decompressing: boolean; // gzip 해제 진행 중 여부 (검색 시작 시점 판정용)
    isActive?: boolean; // 활성 탭 여부 — false이면 전역 Enter 탐색을 무시한다
}

/**
 * 상단 검색바 컴포넌트.
 * 정규식/대소문자 옵션 토글, 필터 모드 토글, 실시간 유효성 검사,
 * 다음/이전 결과 이동 및 진행률/결과 건수를 표시합니다.
 */
export const SearchBar: React.FC<SearchBarProps> = React.memo(
    ({
        fileId,
        getViewportLineNo,
        filterId,
        isFilterMode,
        findSignal = 0,
        onToggleFilterMode,
        onNavigateToHit,
        onSearchStarted,
        onSearchProgress,
        onSearchDone,
        onSearchDensityRequest,
        onClearSearch,
        currentHit,
        setCurrentHit,
        decompressing,
        isActive = true,
    }) => {
        const inputRef = useRef<HTMLInputElement>(null);

        const [queryText, setQueryText] = useState("");
        const [isRegex, setIsRegex] = useState(false);
        const [isCaseSensitive, setIsCaseSensitive] = useState(false);

        const [validationError, setValidationError] = useState<string | null>(
            null,
        );
        const [isSearching, setIsSearching] = useState(false);
        const [matchCount, setMatchCount] = useState(0);
        const [isSearchDone, setIsSearchDone] = useState(false);
        const [isCapped, setIsCapped] = useState(false);
        const [scannedBytes, setScannedBytes] = useState(0);
        const [totalBytes, setTotalBytes] = useState(0);
        // 해제 완료 전에 시작한 검색은 부분 결과임을 표시하기 위한 플래그
        const [partialSearch, setPartialSearch] = useState(false);

        const activeSearchIdRef = useRef<number | null>(null);
        const searchSeqRef = useRef(0);
        const userNavigatedRef = useRef(false);
        const hasAutoNavigatedRef = useRef(false);
        // 디바운스 대기 중에도 최신 해제 상태를 읽기 위한 ref
        const decompressingRef = useRef(decompressing);
        useEffect(() => {
            decompressingRef.current = decompressing;
        }, [decompressing]);

        // 메뉴 "찾기" 이벤트(findSignal 증가) 시 검색 입력창 포커스.
        // 마운트 시점의 값은 초기값으로 간주해 무시한다.
        const lastFindSignalRef = useRef(findSignal);
        useEffect(() => {
            if (findSignal === lastFindSignalRef.current) return;
            lastFindSignalRef.current = findSignal;
            if (inputRef.current) {
                inputRef.current.focus();
                inputRef.current.select();
            }
        }, [findSignal]);

        // 이전 활성 검색과 화면 캐시를 해제한다.
        // 검색어가 비었거나 무효 정규식이 확정되었을 때만 호출한다.
        const releaseActiveSearch = useCallback(() => {
            if (activeSearchIdRef.current !== null) {
                cancelSearch(activeSearchIdRef.current).catch(() => {});
                activeSearchIdRef.current = null;
            }
            onClearSearch();
            setIsSearching(false);
            setMatchCount(0);
            setIsSearchDone(false);
            setIsCapped(false);
            setScannedBytes(0);
            setTotalBytes(0);
            setCurrentHit(null);
            setValidationError(null);
            setPartialSearch(false);
        }, [onClearSearch, setCurrentHit]);

        // 파일 변경 시 검색 상태 초기화
        useEffect(() => {
            releaseActiveSearch();
            setQueryText("");
        }, [fileId, releaseActiveSearch]);

        // 250ms 디바운스 검색어 유효성 검사 및 검색 시작.
        // 디바운스 대기 중에는 이전 활성 쿼리와 본문 캐시를 그대로 유지해
        // 키 입력마다 화면이 깜빡이지 않게 한다.
        useEffect(() => {
            const trimmed = queryText.trim();
            const seq = ++searchSeqRef.current;

            if (!trimmed || fileId === null) {
                // 빈 검색어는 즉시 확정: 이전 검색을 해제한다
                releaseActiveSearch();
                return;
            }

            const queryDto: QueryOptions = {
                query: trimmed,
                regex: isRegex,
                caseSensitive: isCaseSensitive,
            };

            const timer = setTimeout(async () => {
                if (searchSeqRef.current !== seq) return;

                // 1. 유효성 검사
                try {
                    await validateQuery(queryDto);
                } catch (err) {
                    if (searchSeqRef.current !== seq) return;
                    const msg =
                        typeof err === "string"
                            ? err
                            : err instanceof Error
                              ? err.message
                              : "잘못된 검색어입니다.";
                    // 무효 정규식 확정: 이전 활성 쿼리를 해제하고 에러 표시
                    releaseActiveSearch();
                    setValidationError(msg);
                    return;
                }

                if (searchSeqRef.current !== seq) return;
                setValidationError(null);
                setIsSearching(true);
                setIsSearchDone(false);
                setIsCapped(false);
                setMatchCount(0);
                setScannedBytes(0);
                setTotalBytes(0);
                setCurrentHit(null);
                // 검색 시작 시점에 gzip 해제가 끝나지 않았으면 부분 결과로 표시
                setPartialSearch(decompressingRef.current);
                userNavigatedRef.current = false;
                hasAutoNavigatedRef.current = false;

                // 2. 검색 시작 (유효성 검사 통과 및 시작된 경우에만 활성 쿼리로 등록)
                try {
                    // Channel 이벤트는 invoke 응답(searchId)보다 먼저 도착할 수 있다.
                    // 응답 전 이벤트는 버퍼링했다가 resolve 직후 순서대로 재생한다.
                    const bufferedEvents: SearchEvent[] = [];
                    let resolvedId = 0;
                    let hasResolvedId = false;

                    const dispatchEvent = (
                        event: SearchEvent,
                        searchId: number,
                    ) => {
                        if (event.type === "progress") {
                            setScannedBytes(event.scannedBytes);
                            setTotalBytes(event.totalBytes);
                            setMatchCount(event.matchCount);
                            onSearchProgress(searchId, event.matchCount);
                            onSearchDensityRequest(searchId);

                            // 첫 매치 발견 시 사용자가 아직 직접 이동하지 않았다면 첫 결과로 자동 이동
                            if (
                                event.matchCount > 0 &&
                                !userNavigatedRef.current &&
                                !hasAutoNavigatedRef.current
                            ) {
                                hasAutoNavigatedRef.current = true;
                                const from = Math.floor(getViewportLineNo());
                                searchNav(searchId, from, true)
                                    .then((hit) => {
                                        if (
                                            searchSeqRef.current !== seq ||
                                            userNavigatedRef.current
                                        )
                                            return;
                                        if (hit) {
                                            setCurrentHit(hit);
                                            onNavigateToHit(hit);
                                        } else {
                                            searchHitAt(searchId, 0).then(
                                                (wrapHit) => {
                                                    if (
                                                        searchSeqRef.current !==
                                                            seq ||
                                                        userNavigatedRef.current
                                                    )
                                                        return;
                                                    if (wrapHit) {
                                                        setCurrentHit(wrapHit);
                                                        onNavigateToHit(
                                                            wrapHit,
                                                        );
                                                    }
                                                },
                                            );
                                        }
                                    })
                                    .catch(() => {});
                            }
                        } else if (event.type === "done") {
                            setIsSearching(false);
                            setIsSearchDone(true);
                            setIsCapped(event.capped);
                            setMatchCount(event.total);
                            onSearchDone(searchId, event.total, event.capped);
                            onSearchDensityRequest(searchId);
                        } else if (event.type === "cancelled") {
                            setIsSearching(false);
                        } else if (event.type === "error") {
                            setIsSearching(false);
                            setValidationError(event.message);
                        }
                    };

                    const searchId = await search(
                        fileId,
                        queryDto,
                        filterId,
                        (event: SearchEvent) => {
                            if (searchSeqRef.current !== seq) return;
                            if (!hasResolvedId) {
                                bufferedEvents.push(event);
                                return;
                            }
                            dispatchEvent(event, resolvedId);
                        },
                    );

                    if (searchSeqRef.current !== seq) {
                        cancelSearch(searchId).catch(() => {});
                        return;
                    }

                    resolvedId = searchId;
                    hasResolvedId = true;
                    activeSearchIdRef.current = searchId;
                    onSearchStarted(queryDto, searchId);

                    // 응답 전에 도착한 이벤트를 순서대로 재생
                    for (const event of bufferedEvents) {
                        dispatchEvent(event, searchId);
                    }
                    bufferedEvents.length = 0;
                } catch (err) {
                    if (searchSeqRef.current !== seq) return;
                    setIsSearching(false);
                    const msg =
                        typeof err === "string"
                            ? err
                            : err instanceof Error
                              ? err.message
                              : "검색을 시작할 수 없습니다.";
                    setValidationError(msg);
                }
            }, 250);

            return () => {
                clearTimeout(timer);
            };
            // filterId가 바뀌면(필터 스펙 변경) 같은 검색어라도 필터 안에서 다시 검색한다
        }, [queryText, isRegex, isCaseSensitive, fileId, filterId]); // eslint-disable-line react-hooks/exhaustive-deps

        // 다음 결과로 이동
        const handleNext = useCallback(async () => {
            const searchId = activeSearchIdRef.current;
            if (searchId === null || matchCount === 0) return;

            userNavigatedRef.current = true;
            const fromLine =
                currentHit !== null
                    ? currentHit.line
                    : Math.floor(getViewportLineNo());

            try {
                let hit = await searchNav(searchId, fromLine, true);
                if (!hit) {
                    // Wrap: 처음 결과로 이동
                    hit = await searchHitAt(searchId, 0);
                }
                if (hit) {
                    setCurrentHit(hit);
                    onNavigateToHit(hit);
                }
            } catch (err) {
                console.error("다음 검색 결과 탐색 실패:", err);
            }
        }, [
            currentHit,
            matchCount,
            onNavigateToHit,
            setCurrentHit,
            getViewportLineNo,
        ]);

        // 이전 결과로 이동
        const handlePrev = useCallback(async () => {
            const searchId = activeSearchIdRef.current;
            if (searchId === null || matchCount === 0) return;

            userNavigatedRef.current = true;
            const fromLine =
                currentHit !== null
                    ? currentHit.line
                    : Math.floor(getViewportLineNo());

            try {
                let hit = await searchNav(searchId, fromLine, false);
                if (!hit) {
                    // Wrap: 마지막 결과로 이동
                    const lastIndex = Math.max(0, matchCount - 1);
                    hit = await searchHitAt(searchId, lastIndex);
                }
                if (hit) {
                    setCurrentHit(hit);
                    onNavigateToHit(hit);
                }
            } catch (err) {
                console.error("이전 검색 결과 탐색 실패:", err);
            }
        }, [
            currentHit,
            matchCount,
            onNavigateToHit,
            setCurrentHit,
            getViewportLineNo,
        ]);

        // 키보드 이벤트: Enter (다음), Shift+Enter (이전), Esc (블러)
        const handleInputKeyDown = (
            e: React.KeyboardEvent<HTMLInputElement>,
        ) => {
            if (e.key === "Enter") {
                e.preventDefault();
                if (e.shiftKey) {
                    handlePrev();
                } else {
                    handleNext();
                }
            } else if (e.key === "Escape") {
                e.preventDefault();
                inputRef.current?.blur();
            }
        };

        // 검색창 외부 전역 단축키: Enter / Shift+Enter로 다음/이전 탐색.
        // 입력/버튼 등에 포커스가 있거나 모달이 열려 있거나 이미 처리된
        // 이벤트(defaultPrevented)는 무시한다 (예: 줄 이동 다이얼로그 Enter).
        useEffect(() => {
            const handleGlobalKeyDown = (e: KeyboardEvent) => {
                // 숨겨진 탭(비활성)은 검색 결과 이동 단축키를 처리하지 않는다
                if (!isActive) return;
                if (e.key !== "Enter" || e.defaultPrevented) return;
                const active = document.activeElement;
                if (
                    active instanceof HTMLInputElement ||
                    active instanceof HTMLTextAreaElement ||
                    active instanceof HTMLSelectElement ||
                    active instanceof HTMLButtonElement ||
                    (active instanceof HTMLElement && active.isContentEditable)
                ) {
                    return;
                }
                if (document.querySelector('[aria-modal="true"]') !== null) {
                    return;
                }
                if (activeSearchIdRef.current !== null) {
                    e.preventDefault();
                    if (e.shiftKey) {
                        handlePrev();
                    } else {
                        handleNext();
                    }
                }
            };
            window.addEventListener("keydown", handleGlobalKeyDown);
            return () =>
                window.removeEventListener("keydown", handleGlobalKeyDown);
        }, [handleNext, handlePrev, isActive]);

        // 검색 세션이 살아 있고(isSearching/isSearchDone) 결과가 있을 때만
        // 필터/이동 버튼을 활성화한다. ref가 아닌 state로 판단해 리렌더에 반영된다.
        const canNavigate = (isSearching || isSearchDone) && matchCount > 0;

        // 결과 건수 및 진행률 텍스트
        let resultCountText = "";
        if (queryText.trim()) {
            if (isSearching) {
                const pct =
                    totalBytes > 0
                        ? Math.floor((scannedBytes / totalBytes) * 100)
                        : 0;
                const currentNum =
                    currentHit !== null ? `${currentHit.index + 1}` : "-";
                resultCountText = `${currentNum} / ${matchCount.toLocaleString("ko-KR")}+ (${pct}%)`;
            } else if (isSearchDone) {
                if (matchCount === 0) {
                    resultCountText = "결과 없음";
                } else {
                    const currentNum =
                        currentHit !== null ? `${currentHit.index + 1}` : "-";
                    resultCountText = `${currentNum} / ${matchCount.toLocaleString("ko-KR")}`;
                }
            }
        }

        return (
            <div className="relative flex flex-col justify-center">
                <div className="flex items-center space-x-1.5 bg-zinc-100 dark:bg-zinc-800 rounded-lg p-1 border border-zinc-200 dark:border-zinc-700">
                    {/* 검색어 입력창 */}
                    <div className="relative flex items-center">
                        <svg
                            className="w-3.5 h-3.5 absolute left-2 text-zinc-400 pointer-events-none"
                            fill="none"
                            stroke="currentColor"
                            viewBox="0 0 24 24"
                        >
                            <path
                                strokeLinecap="round"
                                strokeLinejoin="round"
                                strokeWidth={2}
                                d="M21 21l-6-6m2-5a7 7 0 11-14 0 7 7 0 0114 0z"
                            />
                        </svg>
                        <input
                            ref={inputRef}
                            type="text"
                            value={queryText}
                            onChange={(e) => setQueryText(e.target.value)}
                            onKeyDown={handleInputKeyDown}
                            placeholder="검색 (⌘F)"
                            className={`h-6 pl-7 pr-6 text-xs font-mono bg-white dark:bg-zinc-900 border rounded focus:outline-none w-48 text-zinc-900 dark:text-zinc-100 transition-colors ${
                                validationError
                                    ? "border-rose-500 focus:ring-1 focus:ring-rose-500"
                                    : "border-zinc-300 dark:border-zinc-700 focus:ring-1 focus:ring-blue-500"
                            }`}
                        />
                        {queryText && (
                            <button
                                type="button"
                                onClick={() => setQueryText("")}
                                className="absolute right-1.5 text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-200 cursor-pointer text-xs"
                                title="검색어 지우기"
                            >
                                ✕
                            </button>
                        )}
                    </div>

                    {/* 정규식 토글 (.*) */}
                    <button
                        type="button"
                        onClick={() => setIsRegex((v) => !v)}
                        className={`h-6 px-1.5 text-xs font-mono rounded cursor-pointer transition-colors ${
                            isRegex
                                ? "bg-blue-600 text-white font-bold"
                                : "text-zinc-600 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                        }`}
                        title="정규식 (.*)"
                    >
                        .*
                    </button>

                    {/* 대소문자 구분 토글 (Aa) */}
                    <button
                        type="button"
                        onClick={() => setIsCaseSensitive((v) => !v)}
                        className={`h-6 px-1.5 text-xs font-mono rounded cursor-pointer transition-colors ${
                            isCaseSensitive
                                ? "bg-blue-600 text-white font-bold"
                                : "text-zinc-600 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                        }`}
                        title="대소문자 구분 (Aa)"
                    >
                        Aa
                    </button>

                    {/* 검색 결과만 보기 토글 (다중 필터와 별개) */}
                    <button
                        type="button"
                        onClick={onToggleFilterMode}
                        disabled={!canNavigate}
                        className={`h-6 px-2 text-xs rounded cursor-pointer transition-colors flex items-center space-x-1 ${
                            isFilterMode
                                ? "bg-indigo-600 text-white font-medium"
                                : "text-zinc-600 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-40 disabled:cursor-not-allowed"
                        }`}
                        title={
                            filterId !== null
                                ? "검색 결과만 표시 (필터 적용 중: 보이는 줄 안의 검색 결과)"
                                : "검색 결과만 표시 (매치된 줄만 보기)"
                        }
                    >
                        <span>검색 결과만</span>
                    </button>

                    {/* 결과 건수 표시 */}
                    {resultCountText && (
                        <div className="text-xs font-mono text-zinc-600 dark:text-zinc-400 px-1 whitespace-nowrap min-w-[70px] text-center">
                            {resultCountText}
                            {isCapped && (
                                <span
                                    className="ml-1 text-rose-500 font-bold"
                                    title="결과가 너무 많음 (1,000만 건 상한)"
                                >
                                    !
                                </span>
                            )}
                        </div>
                    )}

                    {/* gzip 해제 완료 전에 시작한 검색의 부분 결과 안내 */}
                    {partialSearch && (isSearching || isSearchDone) && (
                        <span
                            className="text-[10px] font-medium text-amber-600 dark:text-amber-400 whitespace-nowrap"
                            title="압축 해제가 끝나기 전에 시작한 검색입니다. 해제된 내용만 검색했습니다. 다시 검색하면 전체 결과가 나옵니다."
                        >
                            {decompressing
                                ? "해제 중 — 부분 결과"
                                : "부분 결과"}
                        </span>
                    )}

                    {/* 이전/다음 이동 버튼 */}
                    <div className="flex items-center space-x-0.5">
                        <button
                            type="button"
                            onClick={handlePrev}
                            disabled={!canNavigate}
                            className="h-6 w-6 flex items-center justify-center rounded text-zinc-600 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-30 disabled:cursor-not-allowed cursor-pointer"
                            title="이전 결과 (Shift+Enter)"
                        >
                            ▲
                        </button>
                        <button
                            type="button"
                            onClick={handleNext}
                            disabled={!canNavigate}
                            className="h-6 w-6 flex items-center justify-center rounded text-zinc-600 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-30 disabled:cursor-not-allowed cursor-pointer"
                            title="다음 결과 (Enter)"
                        >
                            ▼
                        </button>
                    </div>
                </div>

                {/* 유효성 검사 에러 팝오버 (한 줄 말줄임 + 빨간 테두리 + title 속성) */}
                {validationError && (
                    <div
                        className="absolute top-full left-0 mt-1.5 z-50 max-w-md px-2.5 py-1 text-[11px] font-mono text-rose-600 dark:text-rose-300 bg-white dark:bg-zinc-900 border border-rose-500 rounded-md shadow-lg truncate pointer-events-auto"
                        title={validationError}
                    >
                        <span className="font-semibold text-rose-500 mr-1">
                            오류:
                        </span>
                        {validationError}
                    </div>
                )}
            </div>
        );
    },
);

SearchBar.displayName = "SearchBar";
