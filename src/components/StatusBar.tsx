import React from "react";
import type { FileInfo, IndexProgress } from "@/lib/ipc";
import { formatBytes } from "@/lib/format";
import { remoteHostOf, s3BucketOf } from "@/lib/remote";

export interface SearchStatusInfo {
    currentHitIndex: number | null;
    matchCount: number;
    isSearching: boolean;
    isCapped: boolean;
    query: string;
}

export interface FilterStatusInfo {
    matchCount: number;
    isSearching: boolean;
    isCapped: boolean;
    scannedBytes: number;
    totalBytes: number;
}

interface StatusBarProps {
    file: FileInfo | null;
    fileSize?: number | null; // tail 모드 등으로 갱신된 파일 크기 (미지정 시 file.size)
    lineCount: number;
    indexProgress: IndexProgress | null;
    topLine: number;
    searchStatus?: SearchStatusInfo | null;
    isFilterMode?: boolean; // 검색 결과만 보기 모드
    filterViewActive?: boolean; // 다중 필터 보기 모드
    filterStatus?: FilterStatusInfo | null; // 다중 필터 계산 상태
    actualTopLine?: number | null; // 인덱스 기반 보기 시 화면 상단 행의 실제 원본 파일 줄 번호 (1-based)
    tailOn?: boolean; // tail 모드 활성 여부
    tailPaused?: boolean; // tail 자동 스크롤 일시정지 여부 (사용자가 맨 아래에 있지 않음)
    copying?: boolean; // 줄 범위 복사 진행 중 여부
    selectionLines?: number; // 줄 단위 선택된 줄 수 (0이면 미표시)
    mergeSourceCount?: number | null; // 병합 뷰: 원본 개수 (병합 탭이 아니면 null/미지정)
    mergeSourcePaths?: string[]; // 병합 뷰: 원본 경로 목록 (툴팁용)
}

/**
 * 하단 상태바 컴포넌트
 */
export const StatusBar: React.FC<StatusBarProps> = React.memo(
    ({
        file,
        fileSize,
        lineCount,
        indexProgress,
        topLine,
        searchStatus,
        isFilterMode,
        filterViewActive,
        filterStatus,
        actualTopLine,
        tailOn,
        tailPaused,
        copying,
        selectionLines = 0,
        mergeSourceCount = null,
        mergeSourcePaths,
    }) => {
        if (!file) {
            return (
                <footer className="h-7 shrink-0 px-3 flex items-center justify-between text-xs bg-zinc-100 dark:bg-zinc-900 border-t border-zinc-200 dark:border-zinc-800 text-zinc-500 dark:text-zinc-400 select-none">
                    <span>열린 파일 없음</span>
                    <span>Riffle</span>
                </footer>
            );
        }

        // 현재 보이는 줄 (일반 모드: 1-based topLine, 인덱스 기반 보기: 화면 상단 행의 실제 파일 줄 번호)
        const indexView = isFilterMode || filterViewActive;
        // 원격 파일의 호스트 (로컬 파일이면 null)
        const remoteHost = file.remote ? remoteHostOf(file.path) : null;
        // S3 객체의 버킷 (S3가 아니면 null)
        const s3Bucket = file.remote ? s3BucketOf(file.path) : null;
        const normalCurrentLine =
            lineCount === 0 ? 0 : Math.min(lineCount, Math.floor(topLine) + 1);
        const displayCurrentLine = indexView
            ? actualTopLine != null
                ? actualTopLine.toLocaleString("ko-KR")
                : "-"
            : normalCurrentLine.toLocaleString("ko-KR");

        // 병합 뷰 여부 (file.source가 "merge"인 가상 파일)
        const isMergeView = file.source === "merge";

        // 인덱싱/압축 해제/병합 구축 진행률 텍스트
        let progressText = "";
        let progressTitle = "인덱싱 진행률";
        if (isMergeView) {
            progressTitle = "병합 구축 진행률";
        }
        if (indexProgress) {
            if (indexProgress.done) {
                progressText = isMergeView
                    ? "병합 구축 완료 (100%)"
                    : "인덱싱 완료 (100%)";
            } else if (indexProgress.decompressing) {
                progressTitle = "압축 해제 진행률";
                if (indexProgress.totalBytes > 0) {
                    const pct = (
                        (indexProgress.indexedBytes /
                            indexProgress.totalBytes) *
                        100
                    ).toFixed(1);
                    progressText = `압축 해제 중 ${pct}%`;
                } else {
                    progressText = "압축 해제 중...";
                }
            } else if (indexProgress.totalBytes > 0) {
                const pct = (
                    (indexProgress.indexedBytes / indexProgress.totalBytes) *
                    100
                ).toFixed(1);
                progressText = isMergeView
                    ? `병합 구축 중 ${pct}%`
                    : `인덱싱 중 ${pct}%`;
            } else {
                progressText = isMergeView ? "병합 구축 중..." : "인덱싱 중...";
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

        // 다중 필터 상태 텍스트: "필터: n / 전체줄" (계산 중이면 +와 진행률)
        let filterSummary = "";
        let filterCapped = false;
        if (filterStatus) {
            filterCapped = filterStatus.isCapped;
            const countStr = filterStatus.matchCount.toLocaleString("ko-KR");
            const plus = filterStatus.isSearching ? "+" : "";
            const pct =
                filterStatus.isSearching && filterStatus.totalBytes > 0
                    ? ` (${Math.floor(
                          (filterStatus.scannedBytes /
                              filterStatus.totalBytes) *
                              100,
                      )}%)`
                    : "";
            filterSummary = `필터: ${countStr}${plus} / ${lineCount.toLocaleString("ko-KR")}줄${pct}`;
        }

        return (
            <footer className="h-7 shrink-0 px-3 flex items-center justify-between text-xs bg-zinc-100 dark:bg-zinc-900 border-t border-zinc-200 dark:border-zinc-800 text-zinc-600 dark:text-zinc-400 select-none font-mono">
                {/* 좌측: 파일 경로 (병합 뷰는 원본 개수) */}
                <div className="flex items-center space-x-2 truncate mr-4">
                    {isMergeView ? (
                        <>
                            <span
                                className="px-1.5 py-0.2 rounded bg-violet-100 dark:bg-violet-900/60 text-violet-700 dark:text-violet-300 font-semibold text-[10px] shrink-0"
                                title={
                                    mergeSourcePaths?.join("\n") ?? "병합 뷰"
                                }
                            >
                                병합
                            </span>
                            <span
                                className="truncate"
                                title={
                                    mergeSourcePaths?.join("\n") ??
                                    "여러 로그의 시간순 병합 뷰 (스냅샷)"
                                }
                            >
                                병합: 원본 {mergeSourceCount ?? 0}개 · 병합 시점
                                스냅샷
                            </span>
                        </>
                    ) : (
                        <span className="truncate" title={file.path}>
                            {file.path}
                        </span>
                    )}
                    {remoteHost !== null && (
                        <span
                            className="px-1.5 py-0.2 rounded bg-sky-100 dark:bg-sky-900/60 text-sky-700 dark:text-sky-300 font-semibold text-[10px] shrink-0"
                            title={`원격 호스트: ${remoteHost}`}
                        >
                            원격: {remoteHost}
                        </span>
                    )}
                    {s3Bucket !== null && (
                        <span
                            className="px-1.5 py-0.2 rounded bg-orange-100 dark:bg-orange-900/60 text-orange-700 dark:text-orange-300 font-semibold text-[10px] shrink-0"
                            title={`S3 버킷: ${s3Bucket}`}
                        >
                            S3: {s3Bucket}
                        </span>
                    )}
                    {isFilterMode && (
                        <span
                            className="px-1.5 py-0.2 rounded bg-indigo-100 dark:bg-indigo-900/60 text-indigo-700 dark:text-indigo-300 font-semibold text-[10px]"
                            title="검색 결과 줄만 표시 중"
                        >
                            검색 결과만
                        </span>
                    )}
                    {filterViewActive && (
                        <span
                            className="px-1.5 py-0.2 rounded bg-emerald-100 dark:bg-emerald-900/60 text-emerald-700 dark:text-emerald-300 font-semibold text-[10px]"
                            title="다중 필터를 통과한 줄만 표시 중"
                        >
                            필터
                        </span>
                    )}
                </div>

                {/* 우측: 검색 결과, 크기, 줄 수, 진행률, 현재 줄 위치 */}
                <div className="flex items-center space-x-4 shrink-0">
                    {selectionLines > 0 && (
                        <>
                            <span
                                className="text-sky-700 dark:text-sky-300 font-medium"
                                title="선택된 줄 범위 (Esc로 해제)"
                            >
                                선택: {selectionLines.toLocaleString("ko-KR")}줄
                                · ⌘C 복사
                            </span>
                            <span className="text-zinc-300 dark:text-zinc-700">
                                |
                            </span>
                        </>
                    )}
                    {copying && (
                        <>
                            <span className="text-sky-600 dark:text-sky-400 font-medium">
                                복사 중…
                            </span>
                            <span className="text-zinc-300 dark:text-zinc-700">
                                |
                            </span>
                        </>
                    )}
                    {tailOn && (
                        <>
                            <span
                                className={
                                    tailPaused
                                        ? "text-amber-600 dark:text-amber-400 font-semibold"
                                        : "text-emerald-600 dark:text-emerald-400 font-semibold"
                                }
                                title={
                                    tailPaused
                                        ? "tail 일시정지: 맨 아래로 스크롤하면 자동 추적이 재개됩니다"
                                        : "tail 모드: 파일 끝을 실시간으로 따라가는 중 (검색 결과는 검색 시작 시점까지)"
                                }
                            >
                                {tailPaused ? "TAIL 일시정지" : "● TAIL"}
                            </span>
                            <span className="text-zinc-300 dark:text-zinc-700">
                                |
                            </span>
                        </>
                    )}
                    {filterSummary && (
                        <>
                            <span
                                className={
                                    filterCapped
                                        ? "text-rose-600 dark:text-rose-400 font-medium"
                                        : "text-emerald-700 dark:text-emerald-300 font-medium"
                                }
                                title="필터를 통과한 줄 수 / 전체 줄 수"
                            >
                                {filterSummary}
                                {filterCapped && (
                                    <span
                                        className="ml-1 text-rose-500 font-bold"
                                        title="결과가 너무 많음 (상한 도달)"
                                    >
                                        !
                                    </span>
                                )}
                            </span>
                            <span className="text-zinc-300 dark:text-zinc-700">
                                |
                            </span>
                        </>
                    )}
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
                    <span
                        title={
                            file.compressed
                                ? "임시 공간 사용 중(파일을 닫으면 해제됨)"
                                : "파일 크기"
                        }
                    >
                        {file.compressed && indexProgress
                            ? `원본 ${formatBytes(file.size)} → 해제 ${formatBytes(indexProgress.contentBytes)}`
                            : formatBytes(fileSize ?? file.size)}
                    </span>
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
                                title={progressTitle}
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
