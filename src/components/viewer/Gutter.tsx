import React, { useCallback, useRef } from "react";
import type { Line } from "@/lib/ipc";

/** 거터 클릭 안내 툴팁을 보여준 횟수를 세는 localStorage 키 (처음 3회만 표시) */
const HINT_KEY = "riffle.gutterHintShown";
const HINT_LIMIT = 3;
const HINT_TEXT =
    "클릭: 줄 선택 · Shift+클릭: 범위 · Option+클릭: 북마크 · Cmd+C: 복사";

interface GutterProps {
    startLine: number; // 0-based 시작 인덱스
    rowCount: number; // 렌더링할 줄 수
    totalLines: number; // 전체 줄 수 (자릿수 폭 계산용)
    translateY: number; // 서브픽셀 수직 이동값 (px)
    rowHeight: number; // 행 높이 (px)
    fontSize: number; // 폰트 크기 (px)
    isFilterMode?: boolean; // 인덱스 기반 보기 여부 (줄 선택 미지원)
    getLine?: (index: number) => Line | undefined; // 줄 데이터 조회 함수
    cacheVersion?: number; // 캐시 갱신 시 거터 리렌더링 트리거
    onLineClick?: (index: number, shiftKey: boolean) => void; // 거터 줄 클릭 (줄 선택용)
    selectedRange?: { start: number; end: number } | null; // 줄 선택 범위 (인덱스 기준)
    onTruncatedClick?: (lineNo: number) => void; // 잘린 줄 아이콘 클릭 (실제 파일 줄 번호)
    bookmarks?: Map<number, string>; // 북마크: 실제 파일 줄 번호(0-based) → 색상
    onBookmarkToggle?: (lineNo: number) => void; // 거터 우클릭/Option+클릭 북마크 토글 (실제 파일 줄 번호)
    mergeMode?: boolean; // 병합 뷰: 줄 번호 대신 원본 줄 번호(sourceLine)를 표시
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
        selectedRange,
        onTruncatedClick,
        bookmarks,
        onBookmarkToggle,
        mergeMode = false,
    }) => {
        // 전체 줄 수의 자릿수에 비례하여 폭 계산 (최소 4글자 폭 확보)
        // 모노스페이스 글자 폭은 폰트 크기의 약 0.6em이므로 fontSize에 연동한다
        const digits = Math.max(4, String(totalLines || 1).length);
        const gutterWidthPx = Math.ceil((digits + 2) * fontSize * 0.6) + 16;

        // 줄 선택 안내 툴팁: 처음 3번의 거터 클릭까지 표시 후 영구히 숨긴다
        const hintShownRef = useRef<number | null>(null);
        const getHintShown = useCallback((): number => {
            if (hintShownRef.current !== null) return hintShownRef.current;
            let n: number;
            try {
                n = Number(window.localStorage.getItem(HINT_KEY) ?? "0");
            } catch {
                n = HINT_LIMIT;
            }
            if (!Number.isFinite(n)) n = HINT_LIMIT;
            hintShownRef.current = n;
            return n;
        }, []);

        const selectable = !isFilterMode && onLineClick !== undefined;
        const showHint = selectable && getHintShown() < HINT_LIMIT;

        const handleClick = (index: number, shiftKey: boolean) => {
            if (selectable && getHintShown() < HINT_LIMIT) {
                const next = getHintShown() + 1;
                hintShownRef.current = next;
                try {
                    window.localStorage.setItem(HINT_KEY, String(next));
                } catch {
                    // 저장 실패는 무시
                }
            }
            onLineClick?.(index, shiftKey);
        };

        const lines: React.ReactNode[] = [];
        for (let i = 0; i < rowCount; i++) {
            const index = startLine + i;
            const line = getLine ? getLine(index) : undefined;
            let displayNo: number | string;
            let rowTitle: string | undefined;

            if (mergeMode) {
                // 병합 뷰: 원본 줄 번호(1-based) 표시, 툴팁은 병합 위치
                if (
                    line !== undefined &&
                    line.sourceLine !== null &&
                    line.sourceLine !== undefined
                ) {
                    displayNo = line.sourceLine + 1;
                    rowTitle = `병합 위치 ${(line.no + 1).toLocaleString("ko-KR")}`;
                } else {
                    displayNo = "";
                }
            } else if (isFilterMode) {
                // 인덱스 기반 보기: 원본 파일의 실제 줄 번호(1-based) 표시
                displayNo = line !== undefined ? line.no + 1 : "";
            } else {
                // 일반 모드: 파일 내 유효한 줄 번호만 1-based 숫자로 표시
                displayNo = index < totalLines ? index + 1 : "";
            }

            // 줄 선택 범위 강조 (인덱스 기반 보기에서는 선택을 지원하지 않는다)
            const isSelected =
                !isFilterMode &&
                selectedRange != null &&
                index >= selectedRange.start &&
                index <= selectedRange.end;

            // 이 행의 실제 파일 줄 번호 (북마크 표시·토글 기준)
            const actualNo = isFilterMode
                ? line !== undefined
                    ? line.no
                    : null
                : index;
            const bookmarkColor =
                actualNo !== null ? bookmarks?.get(actualNo) : undefined;

            lines.push(
                <div
                    key={index}
                    onClick={(e) => {
                        // Option+클릭은 줄 선택 대신 북마크 토글
                        if (
                            e.altKey &&
                            onBookmarkToggle !== undefined &&
                            actualNo !== null
                        ) {
                            onBookmarkToggle(actualNo);
                            return;
                        }
                        handleClick(index, e.shiftKey);
                    }}
                    onContextMenu={(e) => {
                        // 거터 우클릭: 북마크 토글
                        if (
                            onBookmarkToggle === undefined ||
                            actualNo === null
                        ) {
                            return;
                        }
                        e.preventDefault();
                        onBookmarkToggle(actualNo);
                    }}
                    title={rowTitle ?? (showHint ? HINT_TEXT : undefined)}
                    className={`relative pr-3 text-right font-mono select-none ${
                        selectable ? "cursor-pointer" : ""
                    } ${
                        isSelected
                            ? "bg-sky-200/70 dark:bg-sky-800/50 text-zinc-700 dark:text-zinc-300"
                            : "text-zinc-400 dark:text-zinc-600"
                    }`}
                    style={{
                        height: rowHeight,
                        lineHeight: `${rowHeight}px`,
                        fontSize: `${fontSize}px`,
                    }}
                >
                    {displayNo}
                    {/* 북마크 색 점 (줄 번호 왼쪽) */}
                    {bookmarkColor !== undefined && (
                        <span
                            className="absolute left-1 top-1/2 -translate-y-1/2 w-1.5 h-1.5 rounded-full"
                            style={{ backgroundColor: bookmarkColor }}
                            title="북마크 (⌘B 토글 · F2 이동 · ⌘⇧B 목록)"
                        />
                    )}
                    {line !== undefined &&
                        line.truncated &&
                        onTruncatedClick !== undefined && (
                            <button
                                type="button"
                                onClick={(e) => {
                                    e.stopPropagation();
                                    onTruncatedClick(line.no);
                                }}
                                className="absolute right-0.5 top-1/2 -translate-y-1/2 px-0.5 text-zinc-400 dark:text-zinc-600 hover:text-zinc-700 dark:hover:text-zinc-300 font-bold cursor-pointer leading-none"
                                style={{ fontSize: `${fontSize - 2}px` }}
                                title="잘린 줄 — 클릭해 전체 보기(16KB 초과)"
                            >
                                ↔
                            </button>
                        )}
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
