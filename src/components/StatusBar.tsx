import React from "react";
import type { FileInfo, IndexProgress } from "@/lib/ipc";
import { formatBytes } from "@/lib/format";

export interface SearchStatusInfo {
    currentHitIndex: number | null;
    matchCount: number;
    isSearching: boolean;
    isCapped: boolean;
    query: string;
}

interface StatusBarProps {
    file: FileInfo | null;
    lineCount: number;
    indexProgress: IndexProgress | null;
    topLine: number;
    searchStatus?: SearchStatusInfo | null;
    isFilterMode?: boolean;
    actualTopLine?: number | null; // 필터 모드 시 화면 상단 행의 실제 원본 파일 줄 번호 (1-based)
}

/**
 * 하단 상태바 컴포넌트
 */
export const StatusBar: React.FC<StatusBarProps> = React.memo(
    ({
        file,
        lineCount,
        indexProgress,
        topLine,
        searchStatus,
        isFilterMode,
        actualTopLine,
    }) => {
        if (!file) {
            return (
                <footer className="h-7 shrink-0 px-3 flex items-center justify-between text-xs bg-zinc-100 dark:bg-zinc-900 border-t border-zinc-200 dark:border-zinc-800 text-zinc-500 dark:text-zinc-400 select-none">
                    <span>열린 파일 없음</span>
                    <span>Riffle</span>
                </footer>
            );
        }

        // 현재 보이는 줄 (일반 모드: 1-based topLine, 필터 모드: 화면 상단 행의 실제 파일 줄 번호)
        const normalCurrentLine =
            lineCount === 0 ? 0 : Math.min(lineCount, Math.floor(topLine) + 1);
        const displayCurrentLine = isFilterMode
            ? actualTopLine != null
                ? actualTopLine.toLocaleString("ko-KR")
                : "-"
            : normalCurrentLine.toLocaleString("ko-KR");

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

        // 검색 상태 텍스트
        let searchSummary = "";
        if (searchStatus && searchStatus.query) {
            const currentStr =
                searchStatus.currentHitIndex !== null
                    ? `${searchStatus.currentHitIndex + 1}`
                    : "-";
            const matchStr = searchStatus.matchCount.toLocaleString("ko-KR");
            const searchingSuffix = searchStatus.isSearching ? "+" : "";
            const cappedText = searchStatus.isCapped
                ? " (결과가 너무 많음(1,000만 건 상한))"
                : "";
            searchSummary = `검색 결과: ${currentStr} / ${matchStr}${searchingSuffix}${cappedText}`;
        }

        return (
            <footer className="h-7 shrink-0 px-3 flex items-center justify-between text-xs bg-zinc-100 dark:bg-zinc-900 border-t border-zinc-200 dark:border-zinc-800 text-zinc-600 dark:text-zinc-400 select-none font-mono">
                {/* 좌측: 파일 경로 */}
                <div className="flex items-center space-x-2 truncate mr-4">
                    <span className="truncate" title={file.path}>
                        {file.path}
                    </span>
                    {isFilterMode && (
                        <span className="px-1.5 py-0.2 rounded bg-indigo-100 dark:bg-indigo-900/60 text-indigo-700 dark:text-indigo-300 font-semibold text-[10px]">
                            필터 모드
                        </span>
                    )}
                </div>

                {/* 우측: 검색 결과, 크기, 줄 수, 진행률, 현재 줄 위치 */}
                <div className="flex items-center space-x-4 shrink-0">
                    {searchSummary && (
                        <>
                            <span
                                className={
                                    searchStatus?.isCapped
                                        ? "text-rose-600 dark:text-rose-400 font-medium"
                                        : "text-zinc-700 dark:text-zinc-300 font-medium"
                                }
                                title="검색 결과"
                            >
                                {searchSummary}
                            </span>
                            <span className="text-zinc-300 dark:text-zinc-700">
                                |
                            </span>
                        </>
                    )}
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
                        {displayCurrentLine} /{" "}
                        {lineCount.toLocaleString("ko-KR")}
                    </span>
                </div>
            </footer>
        );
    },
);

StatusBar.displayName = "StatusBar";
