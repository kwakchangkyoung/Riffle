import React, { useEffect, useRef, useState } from "react";

interface ScrollbarProps {
    topLine: number;
    lineCount: number;
    visibleRows: number;
    onScroll: (topLine: number) => void;
    densityBins?: number[];
    /** 북마크 위치 마커 (실제 파일 줄 번호와 색상). 인덱스 기반 보기에서는 전달하지 않는다 */
    bookmarkMarkers?: { line: number; color: string }[];
    onTrackHeightChange?: (height: number) => void;
    onDraggingChange?: (dragging: boolean) => void; // 드래그 시작/종료 알림 (프리페치 제어용)
}

/**
 * 대용량 줄 수에 대응하는 커스텀 수직 스크롤바 컴포넌트.
 * 검색 결과 밀도를 트랙에 캔버스로 시각화합니다.
 */
export const Scrollbar: React.FC<ScrollbarProps> = React.memo(
    ({
        topLine,
        lineCount,
        visibleRows,
        onScroll,
        densityBins,
        bookmarkMarkers,
        onTrackHeightChange,
        onDraggingChange,
    }) => {
        const trackRef = useRef<HTMLDivElement>(null);
        const canvasRef = useRef<HTMLCanvasElement>(null);
        const [isDragging, setIsDragging] = useState(false);
        const dragStartRef = useRef<{ clientY: number; startTopLine: number }>({
            clientY: 0,
            startTopLine: 0,
        });

        // 마지막 줄이 뷰포트 맨 아래에 올 때의 최대 topLine
        const maxTopLine = Math.max(0, lineCount - visibleRows);

        // 스크롤바 트랙 높이
        const [trackHeight, setTrackHeight] = useState(0);

        useEffect(() => {
            const track = trackRef.current;
            if (!track) return;

            const observer = new ResizeObserver((entries) => {
                for (const entry of entries) {
                    const h = entry.contentRect.height;
                    setTrackHeight(h);
                    onTrackHeightChange?.(h);
                }
            });
            observer.observe(track);
            return () => observer.disconnect();
        }, [onTrackHeightChange]);

        // 검색 밀도 캔버스 렌더링
        useEffect(() => {
            const canvas = canvasRef.current;
            if (!canvas) return;

            if (!densityBins || densityBins.length === 0 || trackHeight <= 0) {
                const ctx = canvas.getContext("2d");
                if (ctx) {
                    ctx.clearRect(0, 0, canvas.width, canvas.height);
                }
                return;
            }

            const dpr = window.devicePixelRatio || 1;
            const width = canvas.clientWidth || 14;
            canvas.width = Math.floor(width * dpr);
            canvas.height = Math.floor(trackHeight * dpr);

            const ctx = canvas.getContext("2d");
            if (!ctx) return;
            ctx.clearRect(0, 0, canvas.width, canvas.height);

            let maxCount = 0;
            for (let i = 0; i < densityBins.length; i++) {
                if (densityBins[i] > maxCount) {
                    maxCount = densityBins[i];
                }
            }
            if (maxCount === 0) return;

            const numBins = densityBins.length;
            const binHeightPx = canvas.height / numBins;

            for (let i = 0; i < numBins; i++) {
                const count = densityBins[i];
                if (count > 0) {
                    // 밀도에 따른 불투명도 계산 (최소 0.25, 최대 1.0)
                    const alpha = Math.min(
                        1.0,
                        0.25 + 0.75 * (count / maxCount),
                    );
                    ctx.fillStyle = `rgba(245, 158, 11, ${alpha.toFixed(3)})`;
                    const y = Math.floor(i * binHeightPx);
                    const h = Math.max(1, Math.ceil(binHeightPx));
                    ctx.fillRect(0, y, canvas.width, h);
                }
            }
        }, [densityBins, trackHeight]);

        // Thumb 높이: max(24px, track * visible / lineCount)
        const thumbHeight =
            lineCount <= 0 || trackHeight <= 0
                ? trackHeight
                : Math.max(
                      24,
                      Math.min(
                          trackHeight,
                          (trackHeight * visibleRows) / lineCount,
                      ),
                  );

        // Thumb 이동 가능 범위 (트랙 높이 - Thumb 높이)
        const availableTrack = Math.max(0, trackHeight - thumbHeight);

        // Thumb y 좌표 오프셋: topLine / (lineCount - visible)
        let thumbTop = 0;
        if (maxTopLine > 0 && availableTrack > 0) {
            const ratio = Math.min(1, Math.max(0, topLine / maxTopLine));
            thumbTop = ratio * availableTrack;
        }

        // 트랙 클릭 시 해당 위치로 점프한 뒤, 마우스를 떼지 않고 움직이면 그대로 드래그가 이어진다
        const handleTrackMouseDown = (e: React.MouseEvent<HTMLDivElement>) => {
            if (e.target !== trackRef.current || availableTrack <= 0) return;
            e.preventDefault();
            const rect = trackRef.current.getBoundingClientRect();
            const clickY = e.clientY - rect.top;
            const targetThumbTop = Math.max(
                0,
                Math.min(availableTrack, clickY - thumbHeight / 2),
            );
            const fraction = targetThumbTop / availableTrack;
            const targetLine =
                targetThumbTop >= availableTrack
                    ? maxTopLine
                    : fraction * maxTopLine;
            onScroll(targetLine);
            setIsDragging(true);
            onDraggingChange?.(true);
            dragStartRef.current = {
                clientY: e.clientY,
                startTopLine: targetLine,
            };
        };

        // Thumb 드래그 시작
        const handleThumbMouseDown = (e: React.MouseEvent<HTMLDivElement>) => {
            e.preventDefault();
            e.stopPropagation();
            setIsDragging(true);
            onDraggingChange?.(true);
            dragStartRef.current = {
                clientY: e.clientY,
                startTopLine: topLine,
            };
        };

        // 마우스 드래그 이동 및 마우스 업 처리
        useEffect(() => {
            if (!isDragging) return;

            const handleMouseMove = (e: MouseEvent) => {
                if (availableTrack <= 0 || maxTopLine <= 0) return;

                const deltaY = e.clientY - dragStartRef.current.clientY;
                const initialRatio =
                    dragStartRef.current.startTopLine / maxTopLine;
                const initialThumbTop = initialRatio * availableTrack;
                const currentThumbTop = Math.max(
                    0,
                    Math.min(availableTrack, initialThumbTop + deltaY),
                );

                // 스크롤바 맨 끝에 도달한 경우 마지막 줄이 정확히 화면 맨 아래에 오도록 설정
                if (currentThumbTop >= availableTrack) {
                    onScroll(maxTopLine);
                    return;
                }
                if (currentThumbTop <= 0) {
                    onScroll(0);
                    return;
                }

                const fraction = currentThumbTop / availableTrack;
                const nextTopLine = fraction * maxTopLine;
                onScroll(nextTopLine);
            };

            const handleMouseUp = () => {
                setIsDragging(false);
                onDraggingChange?.(false);
            };

            window.addEventListener("mousemove", handleMouseMove);
            window.addEventListener("mouseup", handleMouseUp);
            return () => {
                window.removeEventListener("mousemove", handleMouseMove);
                window.removeEventListener("mouseup", handleMouseUp);
            };
        }, [
            isDragging,
            availableTrack,
            maxTopLine,
            onScroll,
            onDraggingChange,
        ]);

        // 언마운트 시 드래그 상태가 남지 않도록 해제
        useEffect(() => {
            return () => onDraggingChange?.(false);
        }, [onDraggingChange]);

        const canScroll = maxTopLine > 0 && availableTrack > 0;

        return (
            <div
                ref={trackRef}
                onMouseDown={handleTrackMouseDown}
                className="relative w-3.5 shrink-0 select-none bg-zinc-100 dark:bg-zinc-900 border-l border-zinc-200 dark:border-zinc-800 cursor-pointer overflow-hidden"
                role="scrollbar"
                aria-orientation="vertical"
                aria-valuemin={1}
                aria-valuemax={Math.max(1, lineCount)}
                aria-valuenow={Math.min(
                    Math.max(1, lineCount),
                    Math.floor(topLine) + 1,
                )}
            >
                {/* 검색 밀도 캔버스 */}
                <canvas
                    ref={canvasRef}
                    className="absolute inset-0 pointer-events-none w-full h-full"
                />

                {/* 북마크 위치 마커: 밀도 캔버스와 구분되는 북마크 색의 작은 가로선 */}
                {bookmarkMarkers !== undefined &&
                    lineCount > 1 &&
                    trackHeight > 0 &&
                    bookmarkMarkers.map((m) => (
                        <div
                            key={m.line}
                            className="absolute left-0.5 right-0.5 h-0.5 rounded-full pointer-events-none"
                            style={{
                                top: `${Math.min(
                                    trackHeight - 2,
                                    (m.line / (lineCount - 1)) * trackHeight,
                                )}px`,
                                backgroundColor: m.color,
                            }}
                        />
                    ))}

                {/* 스크롤바 Thumb */}
                {canScroll && (
                    <div
                        onMouseDown={handleThumbMouseDown}
                        className={`absolute left-0.5 w-2.5 rounded-full transition-colors cursor-grab active:cursor-grabbing z-10 ${
                            isDragging
                                ? "bg-zinc-600 dark:bg-zinc-400"
                                : "bg-zinc-400 hover:bg-zinc-500 dark:bg-zinc-600 dark:hover:bg-zinc-500"
                        }`}
                        style={{
                            height: `${thumbHeight}px`,
                            transform: `translateY(${thumbTop}px)`,
                            willChange: "transform",
                        }}
                    />
                )}
            </div>
        );
    },
);

Scrollbar.displayName = "Scrollbar";
