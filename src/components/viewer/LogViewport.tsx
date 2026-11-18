import React, {
    useCallback,
    useEffect,
    useLayoutEffect,
    useRef,
    useState,
} from "react";
import type { Columns, FileInfo, Line, RuleKind } from "@/lib/ipc";
import type { LineCache } from "@/lib/lineCache";
import {
    findWordSpans,
    lineLevel,
    tokenClassName,
    tokenizeLine,
    wordAt,
    type Token,
} from "@/lib/syntax";
import { ColumnHeader, FieldFilterMenu, type CellMenu } from "./ColumnView";
import {
    estimateColWidths,
    MIN_COL_WIDTH,
    MONO_FONT_STACK,
    useColumnLayout,
} from "./columnLayout";
import { Gutter } from "./Gutter";
import { Scrollbar } from "./Scrollbar";
import { MergeChipColumn, type MergeChipInfo } from "./MergeChipColumn";

const OVERSCAN = 3; // 뷰포트 위아래 추가 렌더링 행 수

/** columns가 없을 때 useColumnLayout에 넘기는 공용 빈 배열 (참조 안정성용) */
const EMPTY_NAMES: string[] = [];

/** 컬럼 뷰가 꺼져 있을 때 기본 폭으로 넘기는 공용 빈 배열 (참조 안정성용) */
const EMPTY_WIDTHS: number[] = [];

/** 로그 레벨 토큰 탐색: 줄 앞 200자 안의 첫 번째 매치만 사용 */
const LEVEL_TOKEN_RE = /\b(ERROR|FATAL|WARN(?:ING)?|INFO|DEBUG|TRACE)\b/;
const LEVEL_SEARCH_CHARS = 200;

/** #rrggbb 색상 문자열에 알파를 붙인 8자리 hex를 반환한다 (규칙 하이라이트 배경용) */
function withAlpha(hex: string): string {
    return hex.length === 7 && hex.startsWith("#") ? `${hex}40` : hex;
}

/**
 * 구문 강조가 꺼져 있을 때의 토큰 목록: 레거시 LEVEL_TOKEN_RE와 같은 대상의
 * 첫 레벨 토큰 하나만 만든다 (배지 없이 텍스트 색만 적용한다).
 */
function legacyLevelTokens(text: string): Token[] {
    const head = text.slice(0, LEVEL_SEARCH_CHARS);
    const match = LEVEL_TOKEN_RE.exec(head);
    if (!match) return [];
    return [
        {
            start: match.index,
            end: match.index + match[0].length,
            kind: "level",
        },
    ];
}

/**
 * 검색 하이라이트, 필터 규칙 하이라이트, 더블클릭 단어 강조, 구문 토큰 범위를
 * 경계로 잘라 각 조각에 적절한 스타일을 입혀 렌더링합니다.
 * 우선순위(바깥 래퍼): 검색 하이라이트 > 규칙 배경 > 단어 강조 > 구문 색(안쪽).
 */
function renderLineContent(
    text: string,
    highlights: [number, number][] | undefined,
    ruleHighlights: [number, number, number][] | undefined,
    ruleColors: string[] | undefined,
    isCurrentHit: boolean,
    syntaxOn: boolean,
    word: string | null,
): React.ReactNode {
    const tokens = syntaxOn ? tokenizeLine(text) : legacyLevelTokens(text);
    const hl = highlights ?? [];
    const rl = ruleHighlights ?? [];
    const ws = word !== null && word !== "" ? findWordSpans(text, word) : [];
    if (
        tokens.length === 0 &&
        hl.length === 0 &&
        rl.length === 0 &&
        ws.length === 0
    ) {
        return text;
    }

    // 하이라이트·단어·구문 토큰의 모든 경계를 모아 조각 단위로 분할한다.
    // 토큰 경계도 포함되므로 각 조각은 어떤 토큰 안에 온전히 들어가거나 밖에 있다.
    const bounds = new Set<number>([0, text.length]);
    for (const t of tokens) {
        bounds.add(t.start);
        bounds.add(t.end);
    }
    for (const [s, e] of hl) {
        if (s >= 0 && s <= text.length) bounds.add(s);
        if (e >= 0 && e <= text.length) bounds.add(e);
    }
    for (const [s, e] of rl) {
        if (s >= 0 && s <= text.length) bounds.add(s);
        if (e >= 0 && e <= text.length) bounds.add(e);
    }
    for (const [s, e] of ws) {
        bounds.add(s);
        bounds.add(e);
    }
    const sorted = Array.from(bounds).sort((a, b) => a - b);

    const nodes: React.ReactNode[] = [];
    let tokIdx = 0;
    for (let i = 0; i < sorted.length - 1; i++) {
        const s = sorted[i];
        const e = sorted[i + 1];
        if (e <= s) continue;
        const seg = text.slice(s, e);
        const inHighlight = hl.some(([hs, he]) => s >= hs && e <= he);
        const ruleEntry = rl.find(([rs, re]) => s >= rs && e <= re);
        const inWord = ws.some(([ws0, we0]) => s >= ws0 && e <= we0);

        // 이 조각을 온전히 감싸는 구문 토큰 (경계 분할 덕분에 최대 하나)
        while (tokIdx < tokens.length && tokens[tokIdx].end <= s) tokIdx++;
        const tok = tokens[tokIdx];
        const inTok =
            tok !== undefined && s >= tok.start && e <= tok.end ? tok : null;

        let content: React.ReactNode = inTok ? (
            <span className={tokenClassName(inTok, text, syntaxOn)}>{seg}</span>
        ) : (
            seg
        );

        if (inHighlight) {
            content = (
                <mark
                    className={
                        isCurrentHit
                            ? "bg-amber-400 text-zinc-950 dark:bg-amber-400 dark:text-zinc-950 font-semibold rounded-xs px-0.5"
                            : "bg-amber-200/90 text-zinc-900 dark:bg-amber-500/40 dark:text-zinc-100 rounded-xs px-0.5"
                    }
                >
                    {content}
                </mark>
            );
        } else if (ruleEntry !== undefined) {
            const color = ruleColors?.[ruleEntry[2]];
            content = (
                <span
                    style={
                        color !== undefined
                            ? { backgroundColor: withAlpha(color) }
                            : undefined
                    }
                >
                    {content}
                </span>
            );
        } else if (inWord) {
            // 더블클릭 단어 강조: 노란 배경 (검색의 amber와 구분).
            // 여백·테두리를 주면 고정폭 글자 위치가 밀리고 토큰 경계마다 상자가 끊기므로 배경만 쓴다.
            content = (
                <span className="bg-yellow-200/80 text-zinc-900 dark:bg-yellow-400/30 dark:text-zinc-100">
                    {content}
                </span>
            );
        }
        nodes.push(<React.Fragment key={i}>{content}</React.Fragment>);
    }

    return nodes;
}

interface LogViewportProps {
    file: FileInfo;
    lineCount: number; // 현재 뷰포트가 표시하는 총 줄 수 (일반 모드: 파일 줄 수, 필터 모드: 매치 건수)
    totalFileLines: number; // 원본 파일의 전체 줄 수 (거터 폭 계산용)
    lineCache: LineCache;
    topLine: number;
    onTopLineChange: (topLine: number) => void;
    currentHitLine: number | null; // 현재 검색 결과 줄 번호 (0-based, 실제 파일 줄 번호)
    isFilterMode: boolean; // 검색 결과만 보기 모드 여부
    filterViewActive?: boolean; // 다중 필터 보기 모드 여부 (인덱스 기반 보기)
    onLineDoubleClick?: (lineNo: number) => void; // 필터 모드에서 줄 더블클릭 시 호출
    densityBins?: number[]; // 스크롤바 미니맵용 밀도 배열
    ruleColors?: string[]; // 필터 규칙 색상 (spec.rules 인덱스 → #rrggbb)
    onViewportHeightChange?: (height: number) => void;
    onTrackHeightChange?: (height: number) => void;
    fontSize: number; // 본문 폰트 크기 (px)
    rowHeight: number; // 행 높이 (px)
    selectedRange?: { start: number; end: number } | null; // 줄 단위 선택 범위 (0-based, 일반 모드만)
    onGutterLineClick?: (index: number, shiftKey: boolean) => void;
    onTruncatedLineClick?: (lineNo: number) => void; // 잘린 줄 표시 클릭 시 원문 줄 번호(0-based)
    onPinchZoom?: (delta: number) => void; // 트랙패드 핀치로 글자 크기 조절 (+1/-1 단계)
    isActive?: boolean; // 활성 탭 여부 — false이면 뷰어 단축키를 무시한다
    bookmarks?: Map<number, string>; // 북마크: 실제 파일 줄 번호 → 색상 (거터 표시용)
    bookmarkMarkers?: { line: number; color: string }[]; // 스크롤바 북마크 마커
    onBookmarkToggle?: (lineNo: number) => void; // 거터 우클릭/Option+클릭 북마크 토글
    columns?: Columns | null; // 감지된 컬럼 정보 (plain이면 null 또는 format "plain")
    columnView?: boolean; // 컬럼 뷰 모드 여부
    onFieldFilter?: (field: string, value: string, kind: RuleKind) => void; // 셀 우클릭 필드 필터 추가
    mergeSources?: MergeChipInfo[]; // 병합 뷰 원본 칩 정보 (비어 있지 않으면 원본 칩 열 표시)
    onJumpToSource?: (sourceIndex: number, sourceLine: number) => void; // 병합 행/칩 → 원본 탭 이동
    syntaxEnabled?: boolean; // 구문 강조 토글 (레벨 배지·줄 레벨 배경·거터 막대도 함께 제어)
    highlightWord?: string | null; // 더블클릭으로 선택된 강조 단어 (탭별 상태)
    onWordHighlight?: (word: string) => void; // 본문 단어 더블클릭 → 단어 강조 설정
}

/**
 * 가상 스크롤 로그 뷰포트 컴포넌트
 */
export const LogViewport: React.FC<LogViewportProps> = React.memo(
    ({
        file,
        lineCount,
        totalFileLines,
        lineCache,
        topLine,
        onTopLineChange,
        currentHitLine,
        isFilterMode,
        filterViewActive = false,
        onLineDoubleClick,
        densityBins,
        ruleColors,
        onViewportHeightChange,
        onTrackHeightChange,
        fontSize,
        rowHeight,
        selectedRange,
        onGutterLineClick,
        onTruncatedLineClick,
        onPinchZoom,
        isActive = true,
        bookmarks,
        bookmarkMarkers,
        onBookmarkToggle,
        columns = null,
        columnView = false,
        onFieldFilter,
        mergeSources,
        onJumpToSource,
        syntaxEnabled = true,
        highlightWord = null,
        onWordHighlight,
    }) => {
        const containerRef = useRef<HTMLDivElement>(null);
        const bodyWrapRef = useRef<HTMLDivElement>(null);
        const bodyScrollRef = useRef<HTMLDivElement>(null);
        const contentRef = useRef<HTMLDivElement>(null);
        // 컬럼 헤더 내용 요소 — 본문 가로 스크롤과 transform으로 동기화한다
        const headerContentRef = useRef<HTMLDivElement | null>(null);

        const [viewportHeight, setViewportHeight] = useState(0);

        // 컬럼 뷰 활성 여부: 토글이 켜져 있고 형식이 plain이 아닐 때만
        const colViewActive =
            columnView && columns !== null && columns.format !== "plain";

        // 병합 뷰 여부: 원본 칩 정보가 있으면 원본 칩 열을 표시한다
        const mergeView = mergeSources !== undefined && mergeSources.length > 0;

        // 본문 영역(헤더 아래) 크기 관찰 — 컬럼 헤더 높이는 행 수 계산에서 제외된다
        useEffect(() => {
            const wrap = bodyWrapRef.current;
            if (!wrap) return;

            const observer = new ResizeObserver((entries) => {
                for (const entry of entries) {
                    const h = entry.contentRect.height;
                    setViewportHeight(h);
                    onViewportHeightChange?.(h);
                }
            });
            observer.observe(wrap);
            return () => observer.disconnect();
        }, [onViewportHeightChange]);

        // 화면에 완전히/부분적으로 들어오는 행 수
        const visibleRows = viewportHeight > 0 ? viewportHeight / rowHeight : 0;

        // 줄 수나 크기 변화로 topLine이 범위를 벗어난 경우 보정
        useEffect(() => {
            if (viewportHeight <= 0) return;
            const maxTop = Math.max(0, lineCount - viewportHeight / rowHeight);
            if (topLine > maxTop) {
                onTopLineChange(maxTop);
            }
        }, [lineCount, viewportHeight, rowHeight, topLine, onTopLineChange]);

        // 캐시 업데이트 시 리렌더링 트리거
        const [cacheVersion, setCacheVersion] = useState(0);
        useEffect(() => {
            return lineCache.subscribe(() => {
                setCacheVersion((v) => v + 1);
            });
        }, [lineCache]);

        // 가로 스크롤 안정화: 지금까지 렌더된 행 중 가장 넓은 폭을 min-width로 유지한다.
        // 세로 스크롤로 짧은 줄만 보일 때 scrollWidth가 줄어 scrollLeft가 튀는 것을 막는다.
        const [minContentWidth, setMinContentWidth] = useState(0);

        // 최대 폭 재설정: 파일/보기(캐시)/글자 크기가 바뀔 때만 초기화한다
        useLayoutEffect(() => {
            setMinContentWidth(0);
            if (bodyScrollRef.current) {
                bodyScrollRef.current.scrollLeft = 0;
            }
        }, [file.id, lineCache, fontSize]);

        // 서브픽셀 렌더링 계산
        const intTopLine = Math.floor(topLine);
        const frac = topLine - intTopLine;

        const renderStart = Math.max(0, intTopLine - OVERSCAN);
        const overscanAbove = intTopLine - renderStart;
        const renderEnd = Math.min(
            lineCount,
            intTopLine + Math.ceil(visibleRows) + OVERSCAN,
        );
        const rowCount = Math.max(0, renderEnd - renderStart);

        // ===== 컬럼 뷰 =====

        // 거터 폭 (Gutter.tsx의 계산식과 동일 — 헤더 왼쪽 여백을 본문 거터와 맞춘다)
        const gutterDigits = Math.max(4, String(totalFileLines || 1).length);
        const gutterWidth = Math.ceil((gutterDigits + 2) * fontSize * 0.6) + 16;

        // 셀/헤더와 동일 클래스의 숨은 프로브에서 계산된 글꼴 단축값을 읽는다.
        // ref 콜백은 커밋 후 실행되므로 프로브 장착 시 한 번 재렌더돼
        // 실제 폰트로 추정을 다시 계산한다.
        const [cellFont, setCellFont] = useState<string | null>(null);
        const [headerFont, setHeaderFont] = useState<string | null>(null);
        const cellProbeRef = useCallback((el: HTMLSpanElement | null) => {
            if (el !== null) setCellFont(getComputedStyle(el).font);
        }, []);
        const headerProbeRef = useCallback((el: HTMLSpanElement | null) => {
            if (el !== null) setHeaderFont(getComputedStyle(el).font);
        }, []);
        const effCellFont = cellFont ?? `${fontSize}px ${MONO_FONT_STACK}`;
        const effHeaderFont = headerFont ?? `600 11px ${MONO_FONT_STACK}`;

        // 기본 컬럼 폭: 컬럼 이름과 캐시에 처음 로드된 행(최대 200행)의 값의
        // 실제 텍스트 폭으로 추정한다. measureText 비용 때문에 캐시나
        // 측정 입력이 바뀐 시점에만 재추정하고, 수동 저장 폭이 없는 컬럼은
        // 이 추정값이 늘어나면 그대로 따라간다.
        const estRef = useRef<{
            key: number;
            columns: Columns | null;
            cellFont: string;
            headerFont: string;
            active: boolean;
            widths: number[];
        }>({
            key: -1,
            columns: null,
            cellFont: "",
            headerFont: "",
            active: false,
            widths: EMPTY_WIDTHS,
        });
        const est = estRef.current;
        if (
            est.key !== cacheVersion ||
            est.columns !== columns ||
            est.cellFont !== effCellFont ||
            est.headerFont !== effHeaderFont ||
            est.active !== colViewActive
        ) {
            estRef.current = {
                key: cacheVersion,
                columns,
                cellFont: effCellFont,
                headerFont: effHeaderFont,
                active: colViewActive,
                widths:
                    colViewActive && columns !== null
                        ? estimateColWidths(
                              columns,
                              effCellFont,
                              effHeaderFont,
                              lineCache.sampleLines(200),
                          )
                        : EMPTY_WIDTHS,
            };
        }
        const defaultColWidths = estRef.current.widths;

        const colNames = columns !== null ? columns.names : EMPTY_NAMES;
        const colLayout = useColumnLayout(
            file.path,
            colNames,
            defaultColWidths,
        );

        // 컬럼 뷰 본문/헤더의 최소 폭 — 단조 폭 규칙(Phase 14)에 컬럼 합계를 합산한다
        const contentMinWidth = colViewActive
            ? Math.max(minContentWidth, colLayout.totalWidth)
            : minContentWidth;

        // 셀 우클릭 필드 필터 메뉴 상태
        const [cellMenu, setCellMenu] = useState<CellMenu | null>(null);

        // 본문 가로 스크롤 → 컬럼 헤더 동기화 (리렌더 없이 transform만 갱신)
        const handleBodyScroll = useCallback(
            (e: React.UIEvent<HTMLDivElement>) => {
                const el = headerContentRef.current;
                if (el !== null) {
                    el.style.transform = `translateX(${-e.currentTarget.scrollLeft}px)`;
                }
            },
            [],
        );

        // 헤더가 다시 그려진 후(컬럼 뷰 토글/폭 변경) 스크롤 위치를 재동기화한다
        useLayoutEffect(() => {
            const el = headerContentRef.current;
            const body = bodyScrollRef.current;
            if (el !== null && body !== null) {
                el.style.transform = `translateX(${-body.scrollLeft}px)`;
            }
        }, [colViewActive, colLayout.totalWidth]);

        // 렌더 후 실제 행 폭을 측정해 최대값을 누적한다 (단조 증가).
        // 행 내용이 바뀌는 조건(보이는 구간/캐시/행 높이)이 변할 때만 측정한다.
        useLayoutEffect(() => {
            const el = contentRef.current;
            if (!el) return;
            let max = 0;
            for (const child of Array.from(el.children)) {
                const w = (child as HTMLElement).scrollWidth;
                if (w > max) max = w;
            }
            if (max > 0) {
                setMinContentWidth((prev) => (max > prev ? max : prev));
            }
        }, [
            renderStart,
            renderEnd,
            cacheVersion,
            rowHeight,
            file.id,
            lineCache,
        ]);

        // 컨테이너 이동값: 행의 서브픽셀 오프셋 반영
        const translateY = -(overscanAbove + frac) * rowHeight;

        // 보이는 구간 데이터 요청
        useEffect(() => {
            lineCache.requestRange(renderStart, renderEnd);
        }, [lineCache, file.id, lineCount, renderStart, renderEnd]);

        // 휠 스크롤 상태 및 RAF 묶기
        const accumulatedDeltaYRef = useRef(0);
        const rafIdRef = useRef<number | null>(null);
        const topLineRef = useRef(topLine);
        topLineRef.current = topLine;

        const flushScroll = useCallback(() => {
            rafIdRef.current = null;
            const deltaY = accumulatedDeltaYRef.current;
            accumulatedDeltaYRef.current = 0;

            const deltaLines = deltaY / rowHeight;
            const maxTop = Math.max(0, lineCount - viewportHeight / rowHeight);
            const nextTop = Math.max(
                0,
                Math.min(maxTop, topLineRef.current + deltaLines),
            );
            onTopLineChange(nextTop);
        }, [lineCount, viewportHeight, rowHeight, onTopLineChange]);

        const flushScrollRef = useRef(flushScroll);
        flushScrollRef.current = flushScroll;

        // 핀치 줌 콜백/누적 상태: 제스처당 단계 변화를 만들기 위해 deltaY를 누적한다
        const pinchZoomRef = useRef(onPinchZoom);
        pinchZoomRef.current = onPinchZoom;
        const pinchAccRef = useRef(0);
        const pinchResetTimerRef = useRef<ReturnType<typeof setTimeout> | null>(
            null,
        );

        // 휠 이벤트 등록 (passive: false)
        useEffect(() => {
            const container = containerRef.current;
            if (!container) return;

            const handleWheel = (e: WheelEvent) => {
                e.preventDefault();

                // Ctrl+휠: macOS 트랙패드 핀치 제스처 → 스크롤 대신 글자 크기 조절.
                // 누적 deltaY가 임계값을 넘을 때마다 한 단계씩 바꾼다.
                if (e.ctrlKey) {
                    const PINCH_STEP = 60;
                    pinchAccRef.current += e.deltaY;
                    while (pinchAccRef.current <= -PINCH_STEP) {
                        pinchAccRef.current += PINCH_STEP;
                        pinchZoomRef.current?.(1);
                    }
                    while (pinchAccRef.current >= PINCH_STEP) {
                        pinchAccRef.current -= PINCH_STEP;
                        pinchZoomRef.current?.(-1);
                    }
                    // 제스처가 끝나면 잔여 누적값을 버려 다음 핀치가 튀지 않게 한다
                    if (pinchResetTimerRef.current !== null) {
                        clearTimeout(pinchResetTimerRef.current);
                    }
                    pinchResetTimerRef.current = setTimeout(() => {
                        pinchResetTimerRef.current = null;
                        pinchAccRef.current = 0;
                    }, 300);
                    return;
                }

                // 가로 스크롤: 본문 영역만 가로 스크롤
                if (bodyScrollRef.current && e.deltaX !== 0) {
                    bodyScrollRef.current.scrollLeft += e.deltaX;
                }

                // 세로 스크롤: deltaMode(픽셀/줄/페이지) 정규화 후 누적
                let dy = e.deltaY;
                if (e.deltaMode === 1) {
                    dy *= rowHeight;
                } else if (e.deltaMode === 2) {
                    dy *= viewportHeight;
                }

                accumulatedDeltaYRef.current += dy;

                if (rafIdRef.current === null) {
                    rafIdRef.current = requestAnimationFrame(() => {
                        flushScrollRef.current();
                    });
                }
            };

            container.addEventListener("wheel", handleWheel, {
                passive: false,
            });
            return () => {
                container.removeEventListener("wheel", handleWheel);
                if (rafIdRef.current !== null) {
                    cancelAnimationFrame(rafIdRef.current);
                    rafIdRef.current = null;
                }
                if (pinchResetTimerRef.current !== null) {
                    clearTimeout(pinchResetTimerRef.current);
                    pinchResetTimerRef.current = null;
                }
            };
        }, [viewportHeight, rowHeight]);

        // 키보드 내비게이션
        useEffect(() => {
            const handleKeyDown = (e: KeyboardEvent) => {
                // 숨겨진 탭(비활성)은 스크롤 단축키를 처리하지 않는다
                if (!isActive) return;
                // 모달(aria-modal)이 열려 있으면 뷰어 단축키를 무시한다
                // (SearchBar의 전역 Enter 핸들러와 같은 가드)
                if (document.querySelector('[aria-modal="true"]') !== null) {
                    return;
                }
                if (
                    document.activeElement instanceof HTMLInputElement ||
                    document.activeElement instanceof HTMLTextAreaElement
                ) {
                    return;
                }

                const maxTop = Math.max(
                    0,
                    lineCount - viewportHeight / rowHeight,
                );
                const pageLines = Math.floor(visibleRows) || 1;

                // 참고: macOS WKWebView는 Cmd+화살표를 텍스트 커서 이동 명령으로
                // 먼저 소비해 네이티브 메뉴 accelerator(go_top/go_bottom)가
                // 발화하지 않는 경우가 있으므로 여기서도 처음/끝 이동을 처리한다.
                // 두 경로 모두 같은 동작이라 중복 실행돼도 문제없다 (멱등).
                // Option 조합(Cmd+Option+↑/↓)은 북마크 이동이므로 여기서 제외한다.
                if (
                    (e.metaKey &&
                        !e.altKey &&
                        !e.shiftKey &&
                        e.key === "ArrowUp") ||
                    e.key === "Home"
                ) {
                    e.preventDefault();
                    onTopLineChange(0);
                } else if (
                    (e.metaKey &&
                        !e.altKey &&
                        !e.shiftKey &&
                        e.key === "ArrowDown") ||
                    e.key === "End"
                ) {
                    e.preventDefault();
                    onTopLineChange(maxTop);
                } else if (e.key === "ArrowUp" && !e.metaKey) {
                    e.preventDefault();
                    onTopLineChange(Math.max(0, topLineRef.current - 1));
                } else if (e.key === "ArrowDown" && !e.metaKey) {
                    e.preventDefault();
                    onTopLineChange(Math.min(maxTop, topLineRef.current + 1));
                } else if (e.key === "PageUp") {
                    e.preventDefault();
                    onTopLineChange(
                        Math.max(0, topLineRef.current - pageLines),
                    );
                } else if (e.key === "PageDown") {
                    e.preventDefault();
                    onTopLineChange(
                        Math.min(maxTop, topLineRef.current + pageLines),
                    );
                }
            };

            window.addEventListener("keydown", handleKeyDown);
            return () => window.removeEventListener("keydown", handleKeyDown);
        }, [
            lineCount,
            viewportHeight,
            visibleRows,
            rowHeight,
            onTopLineChange,
            isActive,
        ]);

        const getLineCallback = useCallback(
            (idx: number): Line | undefined => lineCache.getLine(idx),
            [lineCache],
        );

        // 스크롤바 드래그 상태를 LineCache에 전달 (드래그 중 프리페치 생략)
        const handleScrollbarDragging = useCallback(
            (dragging: boolean) => lineCache.setDragging(dragging),
            [lineCache],
        );

        // 인덱스 기반 보기(검색 결과만 / 다중 필터) 여부
        const indexView = isFilterMode || filterViewActive;

        // 더블클릭 위치의 단어를 골라 강조 콜백으로 넘긴다.
        // 더블클릭 시점에 브라우저가 이미 단어를 선택해 두므로 selection 앵커의
        // 문자 오프셋을 줄 텍스트 좌표로 환산한 뒤 우리 구분자 규칙으로 확장한다.
        const pickWordAt = useCallback(
            (e: React.MouseEvent, line: Line) => {
                if (onWordHighlight === undefined) return;
                const sel = window.getSelection();
                if (sel === null || sel.anchorNode === null) return;
                const el = e.currentTarget as HTMLElement;
                const node = sel.anchorNode;
                if (!el.contains(node)) return;
                const abs = absOffsetIn(el, node, sel.anchorOffset);
                if (abs === null) return;
                const w = wordAt(line.text, abs);
                if (w !== null) onWordHighlight(w.word);
            },
            [onWordHighlight],
        );

        // 본문 행 렌더링
        const rows: React.ReactNode[] = [];
        for (let i = 0; i < rowCount; i++) {
            const index = renderStart + i;
            const line = lineCache.getLine(index);

            // 현재 검색 결과 줄인지 확인 (line.no는 모든 모드에서 실제 파일 줄 번호)
            const isCurrentHit =
                currentHitLine !== null && line?.no === currentHitLine;

            // 줄 단위 선택 범위에 포함되는지 확인 (일반 모드에서만 index == 줄 번호)
            const isSelected =
                !indexView &&
                selectedRange != null &&
                index >= selectedRange.start &&
                index <= selectedRange.end;

            // CSV 헤더 감지 파일의 0번 줄(헤더 행)은 흐리게 표시한다
            const isCsvHeaderRow =
                colViewActive &&
                columns !== null &&
                columns.format === "csv" &&
                columns.hasHeader &&
                line?.no === 0;

            // 줄 단위 레벨 표시 (구문 강조 설정과 함께 켜지고 꺼진다)
            const lvl =
                syntaxEnabled && line !== undefined
                    ? lineLevel(line.text)
                    : null;

            rows.push(
                <div
                    key={index}
                    onDoubleClick={(e) => {
                        if (line === undefined) return;
                        // Option+더블클릭: 어떤 보기에서든 단어 강조만 수행
                        if (e.altKey && !colViewActive) {
                            pickWordAt(e, line);
                            return;
                        }
                        // 병합 뷰: 더블클릭은 원본 탭의 같은 줄로 이동 (모든 보기 모드)
                        if (
                            line.sourceLine !== null &&
                            line.sourceLine !== undefined &&
                            onJumpToSource !== undefined
                        ) {
                            onJumpToSource(line.source ?? 0, line.sourceLine);
                            return;
                        }
                        if (isFilterMode && onLineDoubleClick) {
                            onLineDoubleClick(line.no);
                        } else if (!indexView && line.truncated) {
                            // 잘린 줄 더블클릭: 전체 줄 패널 열기
                            onTruncatedLineClick?.(line.no);
                        } else if (!indexView && !colViewActive) {
                            // 일반 본문 더블클릭: 커서 아래 단어 강조
                            pickWordAt(e, line);
                        }
                    }}
                    className={`flex font-mono select-text cursor-default ${
                        colViewActive
                            ? "items-stretch"
                            : "items-center whitespace-pre px-2"
                    } ${
                        isCurrentHit
                            ? "bg-amber-100/80 dark:bg-amber-950/50 text-zinc-950 dark:text-zinc-50 border-l-2 border-amber-500"
                            : isSelected
                              ? "bg-sky-200/70 dark:bg-sky-800/50 text-zinc-900 dark:text-zinc-100"
                              : lvl === "error"
                                ? "bg-red-50/70 dark:bg-red-950/30 text-zinc-900 dark:text-zinc-100"
                                : lvl === "warn"
                                  ? "bg-orange-50/70 dark:bg-orange-950/30 text-zinc-900 dark:text-zinc-100"
                                  : "text-zinc-900 dark:text-zinc-100 hover:bg-zinc-100/70 dark:hover:bg-zinc-800/40"
                    } ${isCsvHeaderRow ? "opacity-45" : ""}`}
                    style={{
                        height: rowHeight,
                        lineHeight: `${rowHeight}px`,
                        fontSize: `${fontSize}px`,
                    }}
                >
                    {colViewActive ? (
                        line === undefined && lineCache.isLineFailed(index) ? (
                            <span className="flex items-center px-2 text-zinc-400 dark:text-zinc-600 italic">
                                불러오기 실패
                            </span>
                        ) : (
                            // 컬럼 뷰: 행을 고정 폭 셀로 렌더링 (셀 단위 검색 하이라이트는 생략)
                            colLayout.visible.map((ci) => {
                                const val = line?.fields?.[ci] ?? "";
                                return (
                                    <div
                                        key={ci}
                                        className="flex h-full items-center shrink-0 overflow-hidden whitespace-nowrap px-2 border-r border-zinc-200/70 dark:border-zinc-800/70"
                                        style={{
                                            width:
                                                colLayout.widths[ci] ??
                                                MIN_COL_WIDTH,
                                        }}
                                        title={val}
                                        onContextMenu={(e) => {
                                            if (
                                                onFieldFilter === undefined ||
                                                line === undefined
                                            ) {
                                                return;
                                            }
                                            e.preventDefault();
                                            setCellMenu({
                                                x: e.clientX,
                                                y: e.clientY,
                                                field: colNames[ci] ?? "",
                                                value: val,
                                            });
                                        }}
                                    >
                                        <span className="truncate">{val}</span>
                                    </div>
                                );
                            })
                        )
                    ) : (
                        <span>
                            {line ? (
                                renderLineContent(
                                    line.text,
                                    line.highlights,
                                    line.ruleHighlights,
                                    ruleColors,
                                    isCurrentHit,
                                    syntaxEnabled,
                                    highlightWord,
                                )
                            ) : lineCache.isLineFailed(index) ? (
                                <span className="text-zinc-400 dark:text-zinc-600 italic">
                                    불러오기 실패
                                </span>
                            ) : (
                                ""
                            )}
                        </span>
                    )}
                </div>,
            );
        }

        return (
            <div
                ref={containerRef}
                className="relative flex-1 flex flex-col h-full w-full overflow-hidden select-none bg-white dark:bg-zinc-950"
            >
                {/* 컬럼 뷰 고정 헤더 (본문과 가로 스크롤 동기화) */}
                {colViewActive && columns !== null && (
                    <ColumnHeader
                        names={colNames}
                        widths={colLayout.widths}
                        visible={colLayout.visible}
                        hidden={colLayout.hidden}
                        gutterWidth={gutterWidth}
                        contentMinWidth={contentMinWidth}
                        contentRef={headerContentRef}
                        onResize={colLayout.setWidth}
                        onToggleHidden={colLayout.toggleHidden}
                    />
                )}

                {/* 본문 영역: 거터 + 가상 행 + 스크롤바 */}
                <div ref={bodyWrapRef} className="flex-1 flex min-h-0">
                    {/* 줄 번호 거터 (잘린 줄 표시·줄 선택 강조 포함) */}
                    <Gutter
                        startLine={renderStart}
                        rowCount={rowCount}
                        totalLines={totalFileLines}
                        translateY={translateY}
                        rowHeight={rowHeight}
                        fontSize={fontSize}
                        isFilterMode={indexView}
                        getLine={getLineCallback}
                        cacheVersion={cacheVersion}
                        onLineClick={onGutterLineClick}
                        selectedRange={selectedRange}
                        onTruncatedClick={onTruncatedLineClick}
                        bookmarks={bookmarks}
                        onBookmarkToggle={onBookmarkToggle}
                        mergeMode={mergeView}
                        levelBars={syntaxEnabled}
                    />

                    {/* 병합 뷰: 원본 표시 칩 열 (거터 옆 고정 폭) */}
                    {mergeView && (
                        <MergeChipColumn
                            startLine={renderStart}
                            rowCount={rowCount}
                            translateY={translateY}
                            rowHeight={rowHeight}
                            fontSize={fontSize}
                            getLine={getLineCallback}
                            cacheVersion={cacheVersion}
                            sources={mergeSources}
                            onJumpToSource={onJumpToSource}
                        />
                    )}

                    {/* 본문 영역 (가로 스크롤 가능) */}
                    <div
                        ref={bodyScrollRef}
                        className="flex-1 h-full overflow-x-auto overflow-y-hidden"
                        style={{ scrollbarWidth: "thin" }}
                        onScroll={handleBodyScroll}
                    >
                        <div
                            ref={contentRef}
                            className="inline-block"
                            style={{
                                // 렌더된 최장 폭을 유지해 가로 스크롤 위치가 튀지 않게 한다
                                minWidth: `max(100%, ${contentMinWidth}px)`,
                                transform: `translateY(${translateY}px)`,
                                willChange: "transform",
                            }}
                        >
                            {rows}
                        </div>
                    </div>

                    {/* 커스텀 수직 스크롤바 (인덱스 기반 보기에서는 밀도·북마크 마커 숨김) */}
                    <Scrollbar
                        topLine={topLine}
                        lineCount={lineCount}
                        visibleRows={visibleRows}
                        onScroll={onTopLineChange}
                        densityBins={indexView ? undefined : densityBins}
                        bookmarkMarkers={
                            indexView ? undefined : bookmarkMarkers
                        }
                        onTrackHeightChange={onTrackHeightChange}
                        onDraggingChange={handleScrollbarDragging}
                    />
                </div>

                {/* 폰트 실측용 프로브: 셀/헤더와 동일한 클래스·크기 (비가시) */}
                {colViewActive && (
                    <>
                        <span
                            ref={cellProbeRef}
                            aria-hidden="true"
                            className="font-mono absolute invisible whitespace-nowrap"
                            style={{ fontSize: `${fontSize}px` }}
                        >
                            {"\u00a0"}
                        </span>
                        <span
                            ref={headerProbeRef}
                            aria-hidden="true"
                            className="font-mono text-[11px] font-semibold absolute invisible whitespace-nowrap"
                        >
                            {"\u00a0"}
                        </span>
                    </>
                )}

                {/* 셀 우클릭 필드 필터 메뉴 */}
                {cellMenu !== null && onFieldFilter !== undefined && (
                    <FieldFilterMenu
                        menu={cellMenu}
                        onFilter={onFieldFilter}
                        onClose={() => setCellMenu(null)}
                    />
                )}
            </div>
        );
    },
);

LogViewport.displayName = "LogViewport";

/**
 * 요소 안에서 (node, offset) 위치의 전체 텍스트 기준 문자 오프셋을 계산합니다.
 * 구문 토큰 span으로 나뉜 행 내용에서 더블클릭 지점의 줄 텍스트 좌표를 구하는 데 쓴다.
 */
function absOffsetIn(
    el: HTMLElement,
    node: Node,
    offset: number,
): number | null {
    if (node.nodeType === Node.TEXT_NODE) {
        const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
        let total = 0;
        let cur = walker.nextNode();
        while (cur !== null) {
            if (cur === node) return total + offset;
            total += cur.nodeValue?.length ?? 0;
            cur = walker.nextNode();
        }
        return null;
    }
    if (node.nodeType === Node.ELEMENT_NODE) {
        // 앵커가 요소 노드면 offset은 자식 인덱스 — 앞쪽 형제의 텍스트 길이 합
        let total = 0;
        const kids = node.childNodes;
        const limit = Math.min(offset, kids.length);
        for (let i = 0; i < limit; i++) {
            total += kids[i].textContent?.length ?? 0;
        }
        return total;
    }
    return null;
}
