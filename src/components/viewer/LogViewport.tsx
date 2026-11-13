import React, { useCallback, useEffect, useRef, useState } from "react";
import type { FileInfo, Line } from "@/lib/ipc";
import type { LineCache } from "@/lib/lineCache";
import { Gutter } from "./Gutter";
import { Scrollbar } from "./Scrollbar";

const OVERSCAN = 3; // 뷰포트 위아래 추가 렌더링 행 수

/** 로그 레벨 토큰 탐색: 줄 앞 200자 안의 첫 번째 매치만 사용 */
const LEVEL_TOKEN_RE = /\b(ERROR|FATAL|WARN(?:ING)?|INFO|DEBUG|TRACE)\b/;
const LEVEL_SEARCH_CHARS = 200;

interface LevelSpan {
    start: number;
    end: number;
    className: string;
}

/**
 * 줄 텍스트에서 첫 로그 레벨 토큰의 범위와 색상 클래스를 찾습니다.
 */
function findLevelSpan(text: string): LevelSpan | null {
    const head = text.slice(0, LEVEL_SEARCH_CHARS);
    const match = LEVEL_TOKEN_RE.exec(head);
    if (!match) return null;

    const token = match[0];
    let className: string;
    if (token === "ERROR" || token === "FATAL") {
        className = "text-red-600 dark:text-red-400 font-semibold";
    } else if (token === "WARN" || token === "WARNING") {
        className = "text-amber-600 dark:text-amber-400 font-semibold";
    } else if (token === "INFO") {
        className = "text-blue-600 dark:text-blue-400";
    } else {
        // DEBUG, TRACE
        className = "text-zinc-400 dark:text-zinc-500";
    }
    return { start: match.index, end: match.index + token.length, className };
}

/**
 * 검색 하이라이트 범위와 로그 레벨 토큰 범위를 경계로 잘라
 * 각 조각에 적절한 스타일을 입혀 렌더링합니다.
 */
function renderLineContent(
    text: string,
    highlights: [number, number][] | undefined,
    isCurrentHit: boolean,
): React.ReactNode {
    const level = findLevelSpan(text);
    const hl = highlights ?? [];
    if (level === null && hl.length === 0) {
        return text;
    }

    // 하이라이트와 레벨 토큰의 모든 경계를 모아 조각 단위로 분할
    const bounds = new Set<number>([0, text.length]);
    if (level !== null) {
        bounds.add(level.start);
        bounds.add(level.end);
    }
    for (const [s, e] of hl) {
        if (s >= 0 && s <= text.length) bounds.add(s);
        if (e >= 0 && e <= text.length) bounds.add(e);
    }
    const sorted = Array.from(bounds).sort((a, b) => a - b);

    const nodes: React.ReactNode[] = [];
    for (let i = 0; i < sorted.length - 1; i++) {
        const s = sorted[i];
        const e = sorted[i + 1];
        if (e <= s) continue;
        const seg = text.slice(s, e);
        const inHighlight = hl.some(([hs, he]) => s >= hs && e <= he);
        const inLevel = level !== null && s >= level.start && e <= level.end;

        if (inHighlight) {
            nodes.push(
                <mark
                    key={i}
                    className={
                        isCurrentHit
                            ? "bg-amber-400 text-zinc-950 dark:bg-amber-400 dark:text-zinc-950 font-semibold rounded-xs px-0.5"
                            : "bg-amber-200/90 text-zinc-900 dark:bg-amber-500/40 dark:text-zinc-100 rounded-xs px-0.5"
                    }
                >
                    {inLevel ? (
                        <span className={level.className}>{seg}</span>
                    ) : (
                        seg
                    )}
                </mark>,
            );
        } else if (inLevel) {
            nodes.push(
                <span key={i} className={level.className}>
                    {seg}
                </span>,
            );
        } else {
            nodes.push(seg);
        }
    }

    return nodes;
}

interface LogViewportProps {
    file: FileInfo;
    lineCount: number; // 현재 뷰포트가 표시하는 총 줄 수 (일반 모드: 파일 줄 수, 필터 모드: 매치 건수)
    totalFileLines: number; // 원본 파일의 전체 줄 수 (거터 폭 계산용)
    lineCache: LineCache;
    topLine: number;
    onTopLineChange: (topLine: number) => void;
    currentHitLine: number | null; // 현재 검색 결과 줄 번호 (0-based)
    isFilterMode: boolean; // 필터 모드 여부
    onLineDoubleClick?: (lineNo: number) => void; // 필터 모드에서 줄 더블클릭 시 호출
    densityBins?: number[]; // 스크롤바 미니맵용 밀도 배열
    onViewportHeightChange?: (height: number) => void;
    onTrackHeightChange?: (height: number) => void;
    fontSize: number; // 본문 폰트 크기 (px)
    rowHeight: number; // 행 높이 (px)
    selectedRange?: { start: number; end: number } | null; // 줄 단위 선택 범위 (0-based, 일반 모드만)
    onGutterLineClick?: (index: number, shiftKey: boolean) => void;
    onTruncatedLineClick?: (lineNo: number) => void; // 잘린 줄의 "…" 클릭 시 원문 줄 번호(0-based)
}

/**
 * 가상 스크롤 로그 뷰포트 컴포넌트
 */
export const LogViewport: React.FC<LogViewportProps> = React.memo(
    ({
        file,
        lineCount,
        totalFileLines,
        lineCache,
        topLine,
        onTopLineChange,
        currentHitLine,
        isFilterMode,
        onLineDoubleClick,
        densityBins,
        onViewportHeightChange,
        onTrackHeightChange,
        fontSize,
        rowHeight,
        selectedRange,
        onGutterLineClick,
        onTruncatedLineClick,
    }) => {
        const containerRef = useRef<HTMLDivElement>(null);
        const bodyScrollRef = useRef<HTMLDivElement>(null);

        const [viewportHeight, setViewportHeight] = useState(0);

        // 컨테이너 크기 관찰
        useEffect(() => {
            const container = containerRef.current;
            if (!container) return;

            const observer = new ResizeObserver((entries) => {
                for (const entry of entries) {
                    const h = entry.contentRect.height;
                    setViewportHeight(h);
                    onViewportHeightChange?.(h);
                }
            });
            observer.observe(container);
            return () => observer.disconnect();
        }, [onViewportHeightChange]);

        // 화면에 완전히/부분적으로 들어오는 행 수
        const visibleRows = viewportHeight > 0 ? viewportHeight / rowHeight : 0;

        // 줄 수나 크기 변화로 topLine이 범위를 벗어난 경우 보정
        useEffect(() => {
            if (viewportHeight <= 0) return;
            const maxTop = Math.max(0, lineCount - viewportHeight / rowHeight);
            if (topLine > maxTop) {
                onTopLineChange(maxTop);
            }
        }, [lineCount, viewportHeight, rowHeight, topLine, onTopLineChange]);

        // 캐시 업데이트 시 리렌더링 트리거
        const [cacheVersion, setCacheVersion] = useState(0);
        useEffect(() => {
            return lineCache.subscribe(() => {
                setCacheVersion((v) => v + 1);
            });
        }, [lineCache]);

        // 서브픽셀 렌더링 계산
        const intTopLine = Math.floor(topLine);
        const frac = topLine - intTopLine;

        const renderStart = Math.max(0, intTopLine - OVERSCAN);
        const overscanAbove = intTopLine - renderStart;
        const renderEnd = Math.min(
            lineCount,
            intTopLine + Math.ceil(visibleRows) + OVERSCAN,
        );
        const rowCount = Math.max(0, renderEnd - renderStart);

        // 컨테이너 이동값: 행의 서브픽셀 오프셋 반영
        const translateY = -(overscanAbove + frac) * rowHeight;

        // 보이는 구간 데이터 요청
        useEffect(() => {
            lineCache.requestRange(renderStart, renderEnd);
        }, [lineCache, file.id, lineCount, renderStart, renderEnd]);

        // 휠 스크롤 상태 및 RAF 묶기
        const accumulatedDeltaYRef = useRef(0);
        const rafIdRef = useRef<number | null>(null);
        const topLineRef = useRef(topLine);
        topLineRef.current = topLine;

        const flushScroll = useCallback(() => {
            rafIdRef.current = null;
            const deltaY = accumulatedDeltaYRef.current;
            accumulatedDeltaYRef.current = 0;

            const deltaLines = deltaY / rowHeight;
            const maxTop = Math.max(0, lineCount - viewportHeight / rowHeight);
            const nextTop = Math.max(
                0,
                Math.min(maxTop, topLineRef.current + deltaLines),
            );
            onTopLineChange(nextTop);
        }, [lineCount, viewportHeight, rowHeight, onTopLineChange]);

        const flushScrollRef = useRef(flushScroll);
        flushScrollRef.current = flushScroll;

        // 휠 이벤트 등록 (passive: false)
        useEffect(() => {
            const container = containerRef.current;
            if (!container) return;

            const handleWheel = (e: WheelEvent) => {
                e.preventDefault();

                // 가로 스크롤: 본문 영역만 가로 스크롤
                if (bodyScrollRef.current && e.deltaX !== 0) {
                    bodyScrollRef.current.scrollLeft += e.deltaX;
                }

                // 세로 스크롤: deltaMode(픽셀/줄/페이지) 정규화 후 누적
                let dy = e.deltaY;
                if (e.deltaMode === 1) {
                    dy *= rowHeight;
                } else if (e.deltaMode === 2) {
                    dy *= viewportHeight;
                }

                accumulatedDeltaYRef.current += dy;

                if (rafIdRef.current === null) {
                    rafIdRef.current = requestAnimationFrame(() => {
                        flushScrollRef.current();
                    });
                }
            };

            container.addEventListener("wheel", handleWheel, {
                passive: false,
            });
            return () => {
                container.removeEventListener("wheel", handleWheel);
                if (rafIdRef.current !== null) {
                    cancelAnimationFrame(rafIdRef.current);
                    rafIdRef.current = null;
                }
            };
        }, [viewportHeight, rowHeight]);

        // 키보드 내비게이션
        useEffect(() => {
            const handleKeyDown = (e: KeyboardEvent) => {
                // 모달(aria-modal)이 열려 있으면 뷰어 단축키를 무시한다
                // (SearchBar의 전역 Enter 핸들러와 같은 가드)
                if (document.querySelector('[aria-modal="true"]') !== null) {
                    return;
                }
                if (
                    document.activeElement instanceof HTMLInputElement ||
                    document.activeElement instanceof HTMLTextAreaElement
                ) {
                    return;
                }

                const maxTop = Math.max(
                    0,
                    lineCount - viewportHeight / rowHeight,
                );
                const pageLines = Math.floor(visibleRows) || 1;

                // 참고: macOS WKWebView는 Cmd+화살표를 텍스트 커서 이동 명령으로
                // 먼저 소비해 네이티브 메뉴 accelerator(go_top/go_bottom)가
                // 발화하지 않는 경우가 있으므로 여기서도 처음/끝 이동을 처리한다.
                // 두 경로 모두 같은 동작이라 중복 실행돼도 문제없다 (멱등).
                if ((e.metaKey && e.key === "ArrowUp") || e.key === "Home") {
                    e.preventDefault();
                    onTopLineChange(0);
                } else if (
                    (e.metaKey && e.key === "ArrowDown") ||
                    e.key === "End"
                ) {
                    e.preventDefault();
                    onTopLineChange(maxTop);
                } else if (e.key === "ArrowUp" && !e.metaKey) {
                    e.preventDefault();
                    onTopLineChange(Math.max(0, topLineRef.current - 1));
                } else if (e.key === "ArrowDown" && !e.metaKey) {
                    e.preventDefault();
                    onTopLineChange(Math.min(maxTop, topLineRef.current + 1));
                } else if (e.key === "PageUp") {
                    e.preventDefault();
                    onTopLineChange(
                        Math.max(0, topLineRef.current - pageLines),
                    );
                } else if (e.key === "PageDown") {
                    e.preventDefault();
                    onTopLineChange(
                        Math.min(maxTop, topLineRef.current + pageLines),
                    );
                }
            };

            window.addEventListener("keydown", handleKeyDown);
            return () => window.removeEventListener("keydown", handleKeyDown);
        }, [
            lineCount,
            viewportHeight,
            visibleRows,
            rowHeight,
            onTopLineChange,
        ]);

        const getLineCallback = useCallback(
            (idx: number): Line | undefined => lineCache.getLine(idx),
            [lineCache],
        );

        // 스크롤바 드래그 상태를 LineCache에 전달 (드래그 중 프리페치 생략)
        const handleScrollbarDragging = useCallback(
            (dragging: boolean) => lineCache.setDragging(dragging),
            [lineCache],
        );

        // 잘린 줄 "전체 보기" 배지: 가로 스크롤 영역 밖 오버레이에 행 위치별로 표시
        const truncatedBadges: {
            index: number;
            top: number;
            lineNo: number;
        }[] = [];
        const badgeHeight = Math.min(16, rowHeight);

        // 본문 행 렌더링
        const rows: React.ReactNode[] = [];
        for (let i = 0; i < rowCount; i++) {
            const index = renderStart + i;
            const line = lineCache.getLine(index);

            // 현재 검색 결과 줄인지 확인
            const isCurrentHit =
                currentHitLine !== null &&
                (isFilterMode
                    ? line?.no === currentHitLine
                    : index === currentHitLine);

            // 줄 단위 선택 범위에 포함되는지 확인 (일반 모드에서만 index == 줄 번호)
            const isSelected =
                !isFilterMode &&
                selectedRange != null &&
                index >= selectedRange.start &&
                index <= selectedRange.end;

            if (
                line !== undefined &&
                line.truncated &&
                onTruncatedLineClick !== undefined
            ) {
                truncatedBadges.push({
                    index,
                    top: translateY + i * rowHeight,
                    lineNo: line.no,
                });
            }

            rows.push(
                <div
                    key={index}
                    onDoubleClick={() => {
                        if (isFilterMode && line && onLineDoubleClick) {
                            onLineDoubleClick(line.no);
                        } else if (
                            !isFilterMode &&
                            line !== undefined &&
                            line.truncated
                        ) {
                            // 잘린 줄 더블클릭: 전체 줄 패널 열기
                            onTruncatedLineClick?.(line.no);
                        }
                    }}
                    className={`flex items-center font-mono whitespace-pre select-text px-2 cursor-default ${
                        isCurrentHit
                            ? "bg-amber-100/80 dark:bg-amber-950/50 text-zinc-950 dark:text-zinc-50 border-l-2 border-amber-500"
                            : isSelected
                              ? "bg-sky-200/70 dark:bg-sky-800/50 text-zinc-900 dark:text-zinc-100"
                              : "text-zinc-900 dark:text-zinc-100 hover:bg-zinc-100/70 dark:hover:bg-zinc-800/40"
                    }`}
                    style={{
                        height: rowHeight,
                        lineHeight: `${rowHeight}px`,
                        fontSize: `${fontSize}px`,
                    }}
                >
                    <span>
                        {line ? (
                            renderLineContent(
                                line.text,
                                line.highlights,
                                isCurrentHit,
                            )
                        ) : lineCache.isLineFailed(index) ? (
                            <span className="text-zinc-400 dark:text-zinc-600 italic">
                                불러오기 실패
                            </span>
                        ) : (
                            ""
                        )}
                    </span>
                    {line !== undefined && line.truncated && (
                        <button
                            type="button"
                            onClick={(e) => {
                                e.stopPropagation();
                                onTruncatedLineClick?.(line.no);
                            }}
                            className="ml-1 text-zinc-400 dark:text-zinc-600 hover:text-zinc-700 dark:hover:text-zinc-300 font-bold cursor-pointer"
                            title="줄이 잘림 — 클릭하면 전체 줄 보기"
                        >
                            …
                        </button>
                    )}
                </div>,
            );
        }

        return (
            <div
                ref={containerRef}
                className="relative flex-1 flex h-full w-full overflow-hidden select-none bg-white dark:bg-zinc-950"
            >
                {/* 줄 번호 거터 */}
                <Gutter
                    startLine={renderStart}
                    rowCount={rowCount}
                    totalLines={totalFileLines}
                    translateY={translateY}
                    rowHeight={rowHeight}
                    fontSize={fontSize}
                    isFilterMode={isFilterMode}
                    getLine={getLineCallback}
                    cacheVersion={cacheVersion}
                    onLineClick={onGutterLineClick}
                />

                {/* 본문 영역 (가로 스크롤 가능) */}
                <div
                    ref={bodyScrollRef}
                    className="flex-1 h-full overflow-x-auto overflow-y-hidden"
                    style={{ scrollbarWidth: "thin" }}
                >
                    <div
                        className="inline-block min-w-full"
                        style={{
                            transform: `translateY(${translateY}px)`,
                            willChange: "transform",
                        }}
                    >
                        {rows}
                    </div>
                </div>

                {/* 커스텀 수직 스크롤바 (필터 모드에서는 밀도 마커 숨김) */}
                <Scrollbar
                    topLine={topLine}
                    lineCount={lineCount}
                    visibleRows={visibleRows}
                    onScroll={onTopLineChange}
                    densityBins={isFilterMode ? undefined : densityBins}
                    onTrackHeightChange={onTrackHeightChange}
                    onDraggingChange={handleScrollbarDragging}
                />

                {/* 잘린 줄 "전체 보기" 배지: 가로 스크롤과 무관하게
                    뷰포트 오른쪽 끝(세로 스크롤바 왼쪽)에 행 위치별로 고정 */}
                {truncatedBadges.length > 0 && (
                    <div className="absolute inset-0 z-10 pointer-events-none">
                        {truncatedBadges.map((badge) => (
                            <button
                                key={badge.index}
                                type="button"
                                onClick={() =>
                                    onTruncatedLineClick?.(badge.lineNo)
                                }
                                className="absolute right-[18px] px-1.5 rounded bg-zinc-900/60 hover:bg-zinc-900/90 text-white dark:bg-zinc-100/60 dark:hover:bg-zinc-100/90 dark:text-zinc-900 text-[10px] font-medium cursor-pointer pointer-events-auto whitespace-nowrap"
                                style={{
                                    top:
                                        badge.top +
                                        (rowHeight - badgeHeight) / 2,
                                    height: badgeHeight,
                                    lineHeight: `${badgeHeight}px`,
                                }}
                                title="잘린 줄 — 클릭하면 전체 줄 보기"
                            >
                                전체 보기
                            </button>
                        ))}
                    </div>
                )}
            </div>
        );
    },
);

LogViewport.displayName = "LogViewport";
