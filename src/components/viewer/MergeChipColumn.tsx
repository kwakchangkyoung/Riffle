import React from "react";
import type { Line } from "@/lib/ipc";
import { s3DisplayPath } from "@/lib/remote";

/** 병합 원본 칩 정보 (원본 순번 → 색·짧은 이름) */
export interface MergeChipInfo {
    index: number;
    /** 파일명 앞 12자 */
    name: string;
    /** #rrggbb 고정 색상 */
    color: string;
    /** 원본 전체 경로 (툴팁용) */
    path: string;
}

interface MergeChipColumnProps {
    startLine: number; // 0-based 시작 인덱스
    rowCount: number; // 렌더링할 줄 수
    translateY: number; // 서브픽셀 수직 이동값 (px)
    rowHeight: number; // 행 높이 (px)
    fontSize: number; // 폰트 크기 (px)
    getLine: (index: number) => Line | undefined; // 줄 데이터 조회 함수
    cacheVersion: number; // 캐시 갱신 시 리렌더링 트리거
    sources: MergeChipInfo[]; // 원본 순번 → 칩 정보
    onJumpToSource?: (sourceIndex: number, sourceLine: number) => void;
}

/**
 * 병합 뷰의 원본 표시 칩 열 (거터 오른쪽, 본문 왼쪽).
 * 각 행에 줄이 속한 원본의 고정 색 점 + 짧은 이름을 고정 폭으로 표시한다.
 * 칩 클릭 시 해당 원본 탭의 같은 줄로 이동한다.
 */
export const MergeChipColumn: React.FC<MergeChipColumnProps> = React.memo(
    ({
        startLine,
        rowCount,
        translateY,
        rowHeight,
        fontSize,
        getLine,
        sources,
        onJumpToSource,
    }) => {
        // 칩 폭 고정: 짧은 이름(최대 12자) + 색 점 + 여백이 들어갈 폭
        const widthPx = Math.ceil(12 * fontSize * 0.62) + 26;

        const rows: React.ReactNode[] = [];
        for (let i = 0; i < rowCount; i++) {
            const index = startLine + i;
            const line = getLine(index);
            const src =
                line !== undefined &&
                line.source !== null &&
                line.source !== undefined
                    ? sources[line.source]
                    : undefined;
            rows.push(
                <div
                    key={index}
                    className="flex items-center px-1 overflow-hidden"
                    style={{ height: rowHeight }}
                >
                    {src !== undefined && (
                        <button
                            type="button"
                            onClick={() => {
                                if (
                                    line?.source !== null &&
                                    line?.source !== undefined &&
                                    line.sourceLine !== null &&
                                    line.sourceLine !== undefined
                                ) {
                                    onJumpToSource?.(
                                        line.source,
                                        line.sourceLine,
                                    );
                                }
                            }}
                            className="flex items-center gap-1 max-w-full px-1 rounded text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200/70 dark:hover:bg-zinc-700/60 cursor-pointer"
                            style={{
                                fontSize: `${Math.max(9, fontSize - 3)}px`,
                                lineHeight: `${rowHeight}px`,
                            }}
                            title={`${s3DisplayPath(src.path, null)} — 클릭하면 이 원본 탭으로 이동`}
                        >
                            <span
                                className="w-1.5 h-1.5 rounded-full shrink-0"
                                style={{ backgroundColor: src.color }}
                            />
                            <span className="truncate">{src.name}</span>
                        </button>
                    )}
                </div>,
            );
        }

        return (
            <div
                className="relative shrink-0 overflow-hidden bg-zinc-50/60 dark:bg-zinc-900/60 border-r border-zinc-200/70 dark:border-zinc-800/70"
                style={{ width: `${widthPx}px` }}
            >
                <div
                    style={{
                        transform: `translateY(${translateY}px)`,
                        willChange: "transform",
                    }}
                >
                    {rows}
                </div>
            </div>
        );
    },
);

MergeChipColumn.displayName = "MergeChipColumn";
