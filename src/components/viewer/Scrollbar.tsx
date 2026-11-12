import React, { useEffect, useRef, useState } from "react";

interface ScrollbarProps {
    topLine: number;
    lineCount: number;
    visibleRows: number;
    onScroll: (topLine: number) => void;
}

/**
 * 대용량 줄 수에 대응하는 커스텀 수직 스크롤바 컴포넌트
 */
export const Scrollbar: React.FC<ScrollbarProps> = React.memo(
    ({ topLine, lineCount, visibleRows, onScroll }) => {
        const trackRef = useRef<HTMLDivElement>(null);
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
                    setTrackHeight(entry.contentRect.height);
                }
            });
            observer.observe(track);
            return () => observer.disconnect();
        }, []);

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

        // 트랙 클릭 시 해당 위치로 점프
        const handleTrackMouseDown = (e: React.MouseEvent<HTMLDivElement>) => {
            if (e.target !== trackRef.current || availableTrack <= 0) return;
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
        };

        // Thumb 드래그 시작
        const handleThumbMouseDown = (e: React.MouseEvent<HTMLDivElement>) => {
            e.preventDefault();
            e.stopPropagation();
            setIsDragging(true);
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
            };

            window.addEventListener("mousemove", handleMouseMove);
            window.addEventListener("mouseup", handleMouseUp);
            return () => {
                window.removeEventListener("mousemove", handleMouseMove);
                window.removeEventListener("mouseup", handleMouseUp);
            };
        }, [isDragging, availableTrack, maxTopLine, onScroll]);

        const canScroll = maxTopLine > 0 && availableTrack > 0;

        return (
            <div
                ref={trackRef}
                onMouseDown={handleTrackMouseDown}
                className="relative w-3.5 shrink-0 select-none bg-zinc-100 dark:bg-zinc-900 border-l border-zinc-200 dark:border-zinc-800 cursor-pointer"
                title="스크롤바"
            >
                {canScroll && (
                    <div
                        onMouseDown={handleThumbMouseDown}
                        className={`absolute left-0.5 w-2.5 rounded-full transition-colors cursor-grab active:cursor-grabbing ${
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
