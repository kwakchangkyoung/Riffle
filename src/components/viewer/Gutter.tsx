import React from "react";
import type { Line } from "@/lib/ipc";

interface GutterProps {
    startLine: number; // 0-based 시작 인덱스
    rowCount: number; // 렌더링할 줄 수
    totalLines: number; // 전체 줄 수 (자릿수 폭 계산용)
    translateY: number; // 서브픽셀 수직 이동값 (px)
    rowHeight: number; // 행 높이 (px)
    fontSize: number; // 폰트 크기 (px)
    isFilterMode?: boolean; // 필터 모드 여부
    getLine?: (index: number) => Line | undefined; // 줄 데이터 조회 함수 (필터 모드용)
    cacheVersion?: number; // 캐시 갱신 시 거터 리렌더링 트리거
    onLineClick?: (index: number, shiftKey: boolean) => void; // 거터 줄 클릭 (줄 선택용)
}

/**
 * 줄 번호 거터 컴포넌트
 */
export const Gutter: React.FC<GutterProps> = React.memo(
    ({
        startLine,
        rowCount,
        totalLines,
        translateY,
        rowHeight,
        fontSize,
        isFilterMode,
        getLine,
        onLineClick,
    }) => {
        // 전체 줄 수의 자릿수에 비례하여 폭 계산 (최소 4글자 폭 확보)
        const digits = Math.max(4, String(totalLines || 1).length);
        const gutterWidthPx = (digits + 2) * 8 + 16;

        const lines: React.ReactNode[] = [];
        for (let i = 0; i < rowCount; i++) {
            const index = startLine + i;
            let displayNo: number | string;

            if (isFilterMode) {
                // 필터 모드: 원본 파일의 실제 줄 번호(1-based) 표시
                const line = getLine ? getLine(index) : undefined;
                displayNo = line !== undefined ? line.no + 1 : "";
            } else {
                // 일반 모드: 파일 내 유효한 줄 번호만 1-based 숫자로 표시
                displayNo = index < totalLines ? index + 1 : "";
            }

            lines.push(
                <div
                    key={index}
                    onClick={(e) => onLineClick?.(index, e.shiftKey)}
                    className="pr-3 text-right font-mono text-zinc-400 dark:text-zinc-600 select-none"
                    style={{
                        height: rowHeight,
                        lineHeight: `${rowHeight}px`,
                        fontSize: `${fontSize}px`,
                    }}
                >
                    {displayNo}
                </div>,
            );
        }

        return (
            <div
                className="relative shrink-0 overflow-hidden bg-zinc-50 dark:bg-zinc-900 border-r border-zinc-200 dark:border-zinc-800"
                style={{ width: `${gutterWidthPx}px` }}
                aria-hidden="true"
            >
                <div
                    style={{
                        transform: `translateY(${translateY}px)`,
                        willChange: "transform",
                    }}
                >
                    {lines}
                </div>
            </div>
        );
    },
);

Gutter.displayName = "Gutter";
