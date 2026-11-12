import React from "react";
import type { FileInfo, IndexProgress } from "@/lib/ipc";
import { formatBytes } from "@/lib/format";

interface StatusBarProps {
    file: FileInfo | null;
    lineCount: number;
    indexProgress: IndexProgress | null;
    topLine: number;
}

/**
 * 하단 상태바 컴포넌트
 */
export const StatusBar: React.FC<StatusBarProps> = React.memo(
    ({ file, lineCount, indexProgress, topLine }) => {
        if (!file) {
            return (
                <footer className="h-7 shrink-0 px-3 flex items-center justify-between text-xs bg-zinc-100 dark:bg-zinc-900 border-t border-zinc-200 dark:border-zinc-800 text-zinc-500 dark:text-zinc-400 select-none">
                    <span>열린 파일 없음</span>
                    <span>Riffle</span>
                </footer>
            );
        }

        // 현재 보이는 줄 (1-based, topLine 정수부 + 1)
        const currentLine =
            lineCount === 0 ? 0 : Math.min(lineCount, Math.floor(topLine) + 1);

        // 인덱싱 진행률 텍스트
        let progressText = "";
        if (indexProgress) {
            if (indexProgress.done) {
                progressText = "인덱싱 완료 (100%)";
            } else if (indexProgress.totalBytes > 0) {
                const pct = (
                    (indexProgress.indexedBytes / indexProgress.totalBytes) *
                    100
                ).toFixed(1);
                progressText = `인덱싱 중 ${pct}%`;
            } else {
                progressText = "인덱싱 중...";
            }
        }

        return (
            <footer className="h-7 shrink-0 px-3 flex items-center justify-between text-xs bg-zinc-100 dark:bg-zinc-900 border-t border-zinc-200 dark:border-zinc-800 text-zinc-600 dark:text-zinc-400 select-none font-mono">
                {/* 좌측: 파일 경로 */}
                <div className="flex items-center space-x-2 truncate mr-4">
                    <span className="truncate" title={file.path}>
                        {file.path}
                    </span>
                </div>

                {/* 우측: 크기, 줄 수, 진행률, 현재 줄 위치 */}
                <div className="flex items-center space-x-4 shrink-0">
                    <span title="파일 크기">{formatBytes(file.size)}</span>
                    <span className="text-zinc-300 dark:text-zinc-700">|</span>
                    <span title="전체 줄 수">
                        {lineCount.toLocaleString("ko-KR")} 줄
                    </span>
                    {progressText && (
                        <>
                            <span className="text-zinc-300 dark:text-zinc-700">
                                |
                            </span>
                            <span
                                className={
                                    indexProgress?.done
                                        ? "text-emerald-600 dark:text-emerald-400"
                                        : "text-amber-600 dark:text-amber-400"
                                }
                                title="인덱싱 진행률"
                            >
                                {progressText}
                            </span>
                        </>
                    )}
                    <span className="text-zinc-300 dark:text-zinc-700">|</span>
                    <span title="현재 줄 / 전체 줄">
                        {currentLine.toLocaleString("ko-KR")} /{" "}
                        {lineCount.toLocaleString("ko-KR")}
                    </span>
                </div>
            </footer>
        );
    },
);

StatusBar.displayName = "StatusBar";
