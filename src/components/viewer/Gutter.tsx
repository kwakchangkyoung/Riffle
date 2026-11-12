import React from "react";

interface GutterProps {
    startLine: number; // 0-based 시작 줄 인덱스
    rowCount: number; // 렌더링할 줄 수
    totalLines: number; // 전체 줄 수 (자릿수 폭 계산용)
    translateY: number; // 서브픽셀 수직 이동값 (px)
}

/**
 * 줄 번호 거터 컴포넌트
 */
export const Gutter: React.FC<GutterProps> = React.memo(
    ({ startLine, rowCount, totalLines, translateY }) => {
        // 전체 줄 수의 자릿수에 비례하여 폭 계산 (최소 4글자 폭 확보)
        const digits = Math.max(4, String(totalLines || 1).length);
        const gutterWidthPx = (digits + 2) * 8 + 16;

        const lines: React.ReactNode[] = [];
        for (let i = 0; i < rowCount; i++) {
            const lineNo = startLine + i;
            // 파일 내 유효한 줄 번호만 1-based 숫자로 표시
            const displayNo = lineNo < totalLines ? lineNo + 1 : "";
            lines.push(
                <div
                    key={lineNo}
                    className="h-[20px] leading-[20px] pr-3 text-right font-mono text-xs text-zinc-400 dark:text-zinc-600 select-none"
                    style={{ height: 20, lineHeight: "20px" }}
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
