import React, { useCallback, useEffect, useRef, useState } from "react";
import type { FileInfo } from "@/lib/ipc";
import type { LineCache } from "@/lib/lineCache";
import { Gutter } from "./Gutter";
import { Scrollbar } from "./Scrollbar";

const ROW_HEIGHT = 20; // 고정 행 높이 20px
const OVERSCAN = 3; // 뷰포트 위아래 추가 렌더링 행 수

interface LogViewportProps {
    file: FileInfo;
    lineCount: number;
    lineCache: LineCache;
    topLine: number;
    onTopLineChange: (topLine: number) => void;
}

/**
 * 가상 스크롤 로그 뷰포트 컴포넌트
 */
export const LogViewport: React.FC<LogViewportProps> = React.memo(
    ({ file, lineCount, lineCache, topLine, onTopLineChange }) => {
        const containerRef = useRef<HTMLDivElement>(null);
        const bodyScrollRef = useRef<HTMLDivElement>(null);

        const [viewportHeight, setViewportHeight] = useState(0);

        // 컨테이너 크기 관찰
        useEffect(() => {
            const container = containerRef.current;
            if (!container) return;

            const observer = new ResizeObserver((entries) => {
                for (const entry of entries) {
                    setViewportHeight(entry.contentRect.height);
                }
            });
            observer.observe(container);
            return () => observer.disconnect();
        }, []);

        // 화면에 완전히/부분적으로 들어오는 행 수
        const visibleRows =
            viewportHeight > 0 ? viewportHeight / ROW_HEIGHT : 0;

        // 줄 수나 크기 변화로 topLine이 범위를 벗어난 경우 보정
        useEffect(() => {
            if (viewportHeight <= 0) return;
            const maxTop = Math.max(0, lineCount - viewportHeight / ROW_HEIGHT);
            if (topLine > maxTop) {
                onTopLineChange(maxTop);
            }
        }, [lineCount, viewportHeight, topLine, onTopLineChange]);

        // 캐시 업데이트 시 리렌더링 트리거
        const [, setCacheVersion] = useState(0);
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
        const translateY = -(overscanAbove + frac) * ROW_HEIGHT;

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

            const deltaLines = deltaY / ROW_HEIGHT;
            const maxTop = Math.max(0, lineCount - viewportHeight / ROW_HEIGHT);
            const nextTop = Math.max(
                0,
                Math.min(maxTop, topLineRef.current + deltaLines),
            );
            onTopLineChange(nextTop);
        }, [lineCount, viewportHeight, onTopLineChange]);

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
                    dy *= ROW_HEIGHT;
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
        }, [viewportHeight]);

        // 키보드 내비게이션
        useEffect(() => {
            const handleKeyDown = (e: KeyboardEvent) => {
                if (
                    document.activeElement instanceof HTMLInputElement ||
                    document.activeElement instanceof HTMLTextAreaElement
                ) {
                    return;
                }

                const maxTop = Math.max(
                    0,
                    lineCount - viewportHeight / ROW_HEIGHT,
                );
                const pageLines = Math.floor(visibleRows) || 1;

                if ((e.metaKey && e.key === "ArrowUp") || e.key === "Home") {
                    e.preventDefault();
                    onTopLineChange(0);
                } else if (
                    (e.metaKey && e.key === "ArrowDown") ||
                    e.key === "End"
                ) {
                    e.preventDefault();
                    onTopLineChange(maxTop);
                } else if (e.key === "ArrowUp") {
                    e.preventDefault();
                    onTopLineChange(Math.max(0, topLineRef.current - 1));
                } else if (e.key === "ArrowDown") {
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
        }, [lineCount, viewportHeight, visibleRows, onTopLineChange]);

        // 본문 행 렌더링
        const rows: React.ReactNode[] = [];
        for (let i = 0; i < rowCount; i++) {
            const lineNo = renderStart + i;
            const line = lineCache.getLine(lineNo);
            rows.push(
                <div
                    key={lineNo}
                    className="flex items-center h-[20px] font-mono text-xs text-zinc-900 dark:text-zinc-100 whitespace-pre hover:bg-zinc-100/70 dark:hover:bg-zinc-800/40 select-text px-2"
                    style={{ height: 20, lineHeight: "20px" }}
                >
                    <span>{line ? line.text : ""}</span>
                    {line?.truncated && (
                        <span
                            className="ml-1 text-zinc-400 dark:text-zinc-600 select-none font-bold"
                            title="줄이 잘림 (16KB 초과)"
                        >
                            …
                        </span>
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
                    totalLines={lineCount}
                    translateY={translateY}
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

                {/* 커스텀 수직 스크롤바 */}
                <Scrollbar
                    topLine={topLine}
                    lineCount={lineCount}
                    visibleRows={visibleRows}
                    onScroll={onTopLineChange}
                />
            </div>
        );
    },
);

LogViewport.displayName = "LogViewport";
