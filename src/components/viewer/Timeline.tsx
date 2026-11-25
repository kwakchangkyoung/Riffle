import React, {
    useCallback,
    useEffect,
    useMemo,
    useRef,
    useState,
} from "react";
import { getTimeline, type Timeline } from "@/lib/ipc";
import type { OverviewInfo } from "@/lib/overviewState";

/** 패널 폭 1버킷당 최소 픽셀과 버킷 수 범위 */
const MIN_BUCKET_PX = 4;
const MIN_BUCKETS = 20;
const MAX_BUCKETS = 400;
/** 폭 변경 시 get_timeline 재요청 디바운스 (ms) */
const REFETCH_DELAY = 180;

interface TimelinePanelProps {
    fileId: number;
    /** 개요 스캔 상태 (null: 아직 시작 안 함) */
    overview: OverviewInfo | null;
    /** 개요 done 이후 get_timeline 재요청을 유발하는 세대 번호 */
    overviewVersion: number;
    /** 현재 파일 줄 수 (tail로 늘어나면 개요보다 커질 수 있음) */
    lineCount: number;
    /** 뷰포트 맨 위의 실제 파일 줄 번호 (0-based) — 현재 구간 띠 표시용 */
    topActualLine: number;
    /** 수동 시작이 필요한 파일(에이전트 없는 원격) 여부 */
    needsManualStart: boolean;
    /** 막대 클릭 시 해당 막대의 첫 줄(1-based)로 이동 */
    onNavigate: (lineNo: number) => void;
    /** 개요 다시 계산 요청 */
    onRecompute: () => void;
    /** 개요 계산 수동 시작 */
    onStartOverview: () => void;
    /** 패널 닫기 (보기 메뉴 토글과 동일 효과) */
    onClose: () => void;
}

const pad2 = (n: number) => String(n).padStart(2, "0");

/** epoch 초 → "YYYY-MM-DD HH:MM" (로컬) */
function fmtDateTime(sec: number): string {
    const d = new Date(sec * 1000);
    return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())} ${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** 같은 날이면 "HH:MM", 아니면 날짜 포함 */
function fmtEnd(sec: number, startSec: number): string {
    const d = new Date(sec * 1000);
    const s = new Date(startSec * 1000);
    const sameDay =
        d.getFullYear() === s.getFullYear() &&
        d.getMonth() === s.getMonth() &&
        d.getDate() === s.getDate();
    return sameDay
        ? `${pad2(d.getHours())}:${pad2(d.getMinutes())}`
        : fmtDateTime(sec);
}

/** 막대 단위 레이블: "30초" / "10분" / "3시간" / "2일" */
function fmtBucketSecs(secs: number): string {
    if (secs < 60) return `${secs}초`;
    if (secs < 3600) {
        const m = secs / 60;
        return `${Number.isInteger(m) ? m : m.toFixed(1)}분`;
    }
    if (secs < 86400) {
        const h = secs / 3600;
        return `${Number.isInteger(h) ? h : h.toFixed(1)}시간`;
    }
    const d = secs / 86400;
    return `${Number.isInteger(d) ? d : d.toFixed(1)}일`;
}

/**
 * 시간대 히스토그램 패널 (본문 위, 필터 바 아래 ~72px).
 * 막대 = total(회색) 위에 warn(주황)·error(빨강)를 아래부터 쌓은 SVG.
 */
export const TimelinePanel: React.FC<TimelinePanelProps> = React.memo(
    ({
        fileId,
        overview,
        overviewVersion,
        lineCount,
        topActualLine,
        needsManualStart,
        onNavigate,
        onRecompute,
        onStartOverview,
        onClose,
    }) => {
        const chartRef = useRef<HTMLDivElement>(null);
        const [chartWidth, setChartWidth] = useState(0);
        const [chartHeight, setChartHeight] = useState(0);
        const [buckets, setBuckets] = useState(0);
        const [timeline, setTimeline] = useState<Timeline | null>(null);
        const [logScale, setLogScale] = useState(false);
        const [hoverIdx, setHoverIdx] = useState<number | null>(null);

        // 패널 폭 관측 → 버킷 수 계산 (디바운스는 fetch 쪽에서)
        useEffect(() => {
            const el = chartRef.current;
            if (!el) return;
            const observer = new ResizeObserver((entries) => {
                const r = entries[0].contentRect;
                setChartWidth(r.width);
                setChartHeight(r.height);
                setBuckets(
                    Math.max(
                        MIN_BUCKETS,
                        Math.min(
                            MAX_BUCKETS,
                            Math.floor(r.width / MIN_BUCKET_PX),
                        ),
                    ),
                );
            });
            observer.observe(el);
            return () => observer.disconnect();
        }, []);

        // 개요 완료 후/폭 변경 시 디바운스로 get_timeline 요청
        useEffect(() => {
            if (
                overview?.status !== "done" ||
                !overview.hasTime ||
                buckets === 0
            )
                return;
            const timer = setTimeout(() => {
                getTimeline(fileId, buckets)
                    .then(setTimeline)
                    .catch(() => setTimeline(null));
            }, REFETCH_DELAY);
            return () => clearTimeout(timer);
        }, [fileId, buckets, overview, overviewVersion]);

        const bars = useMemo(() => timeline?.buckets ?? [], [timeline]);

        // 세로 스케일 기준 (선형/로그)
        const maxTotal = useMemo(() => {
            let m = 0;
            for (const b of bars) if (b.total > m) m = b.total;
            return m;
        }, [bars]);
        const scaleOf = useCallback(
            (v: number): number =>
                logScale
                    ? maxTotal > 0
                        ? Math.log1p(v) / Math.log1p(maxTotal)
                        : 0
                    : maxTotal > 0
                      ? v / maxTotal
                      : 0,
            [logScale, maxTotal],
        );

        // 현재 뷰포트가 속한 막대: firstLine <= topActualLine 인 마지막 막대
        const currentIdx = useMemo(() => {
            let idx = -1;
            for (let i = 0; i < bars.length; i++) {
                if (bars[i].firstLine <= topActualLine) idx = i;
                else break;
            }
            return idx;
        }, [bars, topActualLine]);

        // 개요가 스캔 시점보다 줄이 늘어나면 stale 안내
        const stale =
            overview?.status === "done" && lineCount > overview.lineCount;

        const chartH = chartHeight;
        const barW =
            bars.length > 0 && chartWidth > 0 ? chartWidth / bars.length : 0;

        // 막대 클릭 → 그 막대 첫 줄로 이동
        const handleBarClick = useCallback(
            (idx: number) => {
                const b = bars[idx];
                if (b !== undefined) onNavigate(b.firstLine + 1);
            },
            [bars, onNavigate],
        );

        // 시간 축 눈금: 시작/끝과 중간 1~3개
        const axisTicks = useMemo(() => {
            if (timeline === null || bars.length === 0) return [];
            const n = chartWidth > 600 ? 5 : chartWidth > 300 ? 3 : 2;
            const start = timeline.startSec;
            const span = timeline.bucketSecs * bars.length;
            const ticks: { x: number; label: string }[] = [];
            for (let i = 0; i < n; i++) {
                const sec = start + (span * i) / (n - 1);
                ticks.push({
                    x: (chartWidth * i) / (n - 1),
                    label: fmtDateTime(sec),
                });
            }
            return ticks;
        }, [timeline, bars.length, chartWidth]);

        const hovered = hoverIdx !== null ? bars[hoverIdx] : undefined;

        return (
            <div className="h-[72px] shrink-0 flex flex-col border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50/60 dark:bg-zinc-900/40 select-none">
                {/* 헤더: 제목·막대 단위·stale 안내·스케일 전환·닫기 */}
                <div className="h-5 shrink-0 px-2 flex items-center gap-2 text-[10px] text-zinc-500 dark:text-zinc-400">
                    <span className="font-semibold text-zinc-600 dark:text-zinc-300">
                        타임라인
                    </span>
                    {timeline !== null && (
                        <span>{fmtBucketSecs(timeline.bucketSecs)} 단위</span>
                    )}
                    {stale && (
                        <span className="text-amber-600 dark:text-amber-400">
                            개요: {overview.lineCount.toLocaleString("ko-KR")}줄
                            기준
                            <button
                                type="button"
                                onClick={onRecompute}
                                className="ml-1 underline hover:text-amber-800 dark:hover:text-amber-200 cursor-pointer"
                            >
                                다시 계산
                            </button>
                        </span>
                    )}
                    <span className="flex-1" />
                    <button
                        type="button"
                        onClick={() => setLogScale((v) => !v)}
                        className="px-1.5 rounded border border-zinc-300 dark:border-zinc-600 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                        aria-label={
                            logScale
                                ? "선형 스케일로 전환"
                                : "로그 스케일로 전환"
                        }
                        title={
                            logScale
                                ? "로그 스케일 (클릭하면 선형)"
                                : "선형 스케일 (클릭하면 로그)"
                        }
                    >
                        {logScale ? "로그" : "선형"}
                    </button>
                    <button
                        type="button"
                        onClick={onClose}
                        className="px-1 rounded hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                        aria-label="타임라인 닫기"
                        title="닫기 (보기 → 타임라인)"
                    >
                        ✕
                    </button>
                </div>

                {/* 차트 영역 */}
                <div ref={chartRef} className="flex-1 min-h-0 relative px-1">
                    {overview?.status === "running" && (
                        <div className="h-full flex items-center justify-center text-[11px] text-zinc-400 dark:text-zinc-500">
                            개요 계산 중… {Math.floor(overview.fraction * 100)}%
                        </div>
                    )}
                    {overview === null && needsManualStart && (
                        <div className="h-full flex items-center justify-center">
                            <button
                                type="button"
                                onClick={onStartOverview}
                                className="px-2 py-0.5 text-[11px] rounded border border-zinc-300 dark:border-zinc-600 hover:bg-zinc-200 dark:hover:bg-zinc-700 text-zinc-600 dark:text-zinc-300 cursor-pointer"
                            >
                                개요 계산(전체 다운로드)
                            </button>
                        </div>
                    )}
                    {overview === null && !needsManualStart && (
                        <div className="h-full flex items-center justify-center text-[11px] text-zinc-400 dark:text-zinc-500">
                            개요를 아직 계산하지 않았습니다
                        </div>
                    )}
                    {overview?.status === "error" && (
                        <div className="h-full flex items-center justify-center gap-2 text-[11px] text-rose-500 dark:text-rose-400">
                            개요 계산 실패: {overview.message}
                            <button
                                type="button"
                                onClick={onRecompute}
                                className="underline cursor-pointer"
                            >
                                다시 시도
                            </button>
                        </div>
                    )}
                    {overview?.status === "done" && !overview.hasTime && (
                        <div className="h-full flex items-center justify-center text-[11px] text-zinc-400 dark:text-zinc-500">
                            타임스탬프를 인식하지 못했습니다
                        </div>
                    )}
                    {overview?.status === "done" &&
                        overview.hasTime &&
                        timeline === null && (
                            <div className="h-full flex items-center justify-center text-[11px] text-zinc-400 dark:text-zinc-500">
                                불러오는 중…
                            </div>
                        )}
                    {overview?.status === "done" &&
                        overview.hasTime &&
                        timeline !== null && (
                            <svg
                                width={chartWidth}
                                height={chartH}
                                className="block"
                                role="img"
                                aria-label="시간대별 줄 수 히스토그램"
                                onMouseLeave={() => setHoverIdx(null)}
                            >
                                {bars.map((b, i) => {
                                    const x = i * barW;
                                    const totalH = scaleOf(b.total) * chartH;
                                    const errH = scaleOf(b.error) * chartH;
                                    const warnH = scaleOf(b.warn) * chartH;
                                    // 아래부터 error(빨강) → warn(주황) → 나머지(회색)
                                    const restH = Math.max(
                                        0,
                                        totalH - errH - warnH,
                                    );
                                    const cur = i === currentIdx;
                                    return (
                                        <g
                                            key={i}
                                            onMouseEnter={() => setHoverIdx(i)}
                                            onClick={() => handleBarClick(i)}
                                            className="cursor-pointer"
                                        >
                                            <rect
                                                x={x}
                                                y={chartH - totalH}
                                                width={Math.max(1, barW - 0.5)}
                                                height={restH}
                                                className="fill-zinc-300 dark:fill-zinc-600"
                                            />
                                            {warnH > 0 && (
                                                <rect
                                                    x={x}
                                                    y={chartH - errH - warnH}
                                                    width={Math.max(
                                                        1,
                                                        barW - 0.5,
                                                    )}
                                                    height={warnH}
                                                    className="fill-orange-400 dark:fill-orange-500"
                                                />
                                            )}
                                            {errH > 0 && (
                                                <rect
                                                    x={x}
                                                    y={chartH - errH}
                                                    width={Math.max(
                                                        1,
                                                        barW - 0.5,
                                                    )}
                                                    height={errH}
                                                    className="fill-red-500 dark:fill-red-400"
                                                />
                                            )}
                                            {cur && (
                                                <rect
                                                    x={x}
                                                    y={0}
                                                    width={Math.max(
                                                        1,
                                                        barW - 0.5,
                                                    )}
                                                    height={chartH}
                                                    className="fill-sky-500/25 dark:fill-sky-400/25"
                                                />
                                            )}
                                            {/* 호버 히트 영역 */}
                                            <rect
                                                x={x}
                                                y={0}
                                                width={barW}
                                                height={chartH}
                                                fill="transparent"
                                            />
                                        </g>
                                    );
                                })}
                            </svg>
                        )}
                    {/* 호버 툴팁 */}
                    {hovered !== undefined && timeline !== null && (
                        <div
                            className="absolute top-0 z-10 pointer-events-none px-1.5 py-0.5 rounded bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900 text-[10px] whitespace-nowrap shadow"
                            style={{
                                left: Math.min(
                                    Math.max(0, (hoverIdx ?? 0) * barW - 60),
                                    Math.max(0, chartWidth - 240),
                                ),
                            }}
                        >
                            {fmtDateTime(hovered.startSec)} ~{" "}
                            {fmtEnd(
                                hovered.startSec + timeline.bucketSecs,
                                hovered.startSec,
                            )}{" "}
                            · {hovered.total.toLocaleString("ko-KR")}줄 · ERROR{" "}
                            {hovered.error.toLocaleString("ko-KR")} · WARN{" "}
                            {hovered.warn.toLocaleString("ko-KR")}
                        </div>
                    )}
                </div>

                {/* 시간 축 눈금 */}
                {axisTicks.length > 0 && (
                    <div className="h-4 shrink-0 relative text-[9px] text-zinc-400 dark:text-zinc-500">
                        {axisTicks.map((t, i) => (
                            <span
                                key={i}
                                className="absolute whitespace-nowrap"
                                style={{
                                    left:
                                        i === 0
                                            ? 4
                                            : i === axisTicks.length - 1
                                              ? undefined
                                              : t.x,
                                    right:
                                        i === axisTicks.length - 1
                                            ? 4
                                            : undefined,
                                    transform:
                                        i === 0 || i === axisTicks.length - 1
                                            ? "none"
                                            : "translateX(-50%)",
                                }}
                            >
                                {t.label}
                            </span>
                        ))}
                    </div>
                )}
            </div>
        );
    },
);

TimelinePanel.displayName = "TimelinePanel";
