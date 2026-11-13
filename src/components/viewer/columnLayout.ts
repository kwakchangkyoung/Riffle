import { useCallback, useMemo, useState } from "react";
import {
    loadColumnLayout,
    saveColumnLayout,
    type ColumnLayout,
} from "@/lib/columns";
import type { Columns, Line } from "@/lib/ipc";

/** 컬럼 폭 하한 (px) */
export const MIN_COL_WIDTH = 40;
/** 컬럼 폭 상한 (px) — 기본 폭 추정과 수동 조절 모두에 적용 */
export const MAX_COL_WIDTH = 400;
/** 컬럼 헤더 높이 (px) */
export const COLUMN_HEADER_HEIGHT = 24;
/** 기본 폭 추정 시 텍스트 폭에 더하는 여백: 셀 좌우 패딩(px-2=16px) + 오른쪽 보더(1px) + 여유 8px */
export const CELL_EXTRA = 16 + 1 + 8;

/** 본문 셀과 같은 고정폭 글꼴 스택 (Tailwind font-mono 기본값과 동일) */
export const MONO_FONT_STACK =
    'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace';

/** 고정폭 글꼴에서 2칸을 차지하는 문자(한글·CJK·전각)인지 판별한다 */
function isWideCodePoint(cp: number): boolean {
    return (
        (cp >= 0x1100 && cp <= 0x115f) || // Hangul Jamo
        (cp >= 0x2e80 && cp <= 0xa4cf) || // CJK Radicals ~ Yi
        (cp >= 0xac00 && cp <= 0xd7a3) || // Hangul Syllables
        (cp >= 0xf900 && cp <= 0xfaff) || // CJK Compat Ideographs
        (cp >= 0xfe30 && cp <= 0xfe6b) || // CJK Compat Forms
        (cp >= 0xff00 && cp <= 0xff60) || // Fullwidth Forms
        (cp >= 0xffe0 && cp <= 0xffe6) || // Fullwidth 기호
        (cp >= 0x20000 && cp <= 0x3fffd) // CJK Ext B 이후 평면
    );
}

/** 고정폭 셀 폭을 문자 수로 계산한다 (한글·CJK·전각은 2칸, 나머지는 1칸) */
function cellLen(s: string): number {
    let w = 0;
    for (const ch of s) {
        w += isWideCodePoint(ch.codePointAt(0) ?? 0) ? 2 : 1;
    }
    return w;
}

// 텍스트 실측용 캔버스 컨텍스트 (undefined = 미생성, null = 사용 불가)
let measureCtx: CanvasRenderingContext2D | null | undefined;

/**
 * 문자열의 실제 렌더 폭(px)을 잰다. font는 CSS font 단축값
 * (getComputedStyle(el).font 그대로 전달하는 것을 권장).
 * 캔버스를 쓸 수 없는 환경이면 셀 문자 수 × 0.6em으로 근사한다.
 */
function measureTextWidth(text: string, font: string): number {
    if (measureCtx === undefined) {
        measureCtx = document.createElement("canvas").getContext("2d");
    }
    if (measureCtx === null) {
        const m = /([\d.]+)px/.exec(font);
        const px = m !== null ? Number(m[1]) : 13;
        return cellLen(text) * px * 0.6;
    }
    measureCtx.font = font;
    return measureCtx.measureText(text).width;
}

/**
 * 기본 컬럼 폭을 추정한다: 컬럼 이름(헤더 폰트)과 캐시에 로드된 행의
 * 해당 컬럼 값(본문 폰트) 중 가장 넓은 것의 실제 텍스트 폭 + CELL_EXTRA.
 * 표본이 늘어나면 다시 호출해 더 넓은 추정을 얻을 수 있다.
 */
export function estimateColWidths(
    columns: Columns,
    cellFont: string,
    headerFont: string,
    lines: Line[],
): number[] {
    const lens = columns.names.map((n) => measureTextWidth(n, headerFont));
    for (const line of lines) {
        const fields = line.fields;
        if (fields === undefined) continue;
        for (let c = 0; c < lens.length; c++) {
            const len = measureTextWidth(fields[c] ?? "", cellFont);
            if (len > lens[c]) lens[c] = len;
        }
    }
    return lens.map((len) =>
        Math.min(
            MAX_COL_WIDTH,
            Math.max(MIN_COL_WIDTH, Math.ceil(len) + CELL_EXTRA),
        ),
    );
}

/** 컬럼 레이아웃 상태 (useColumnLayout 반환값) */
export interface ColumnLayoutState {
    /** 전체 컬럼 기준 유효 폭 (수동 저장값 없으면 최신 기본 폭) */
    widths: number[];
    /** 숨긴 컬럼 인덱스 집합 */
    hidden: ReadonlySet<number>;
    /** 보이는 컬럼 인덱스 (순서 유지) */
    visible: number[];
    /** 보이는 컬럼 폭 합 */
    totalWidth: number;
    /** 컬럼 폭 변경 (경로별 저장) */
    setWidth: (index: number, width: number) => void;
    /** 컬럼 숨김/표시 토글 (경로별 저장). 마지막 보이는 컬럼은 숨기지 않는다 */
    toggleHidden: (index: number) => void;
}

interface LayoutEntry {
    path: string;
    layout: ColumnLayout;
}

/**
 * 컬럼 폭·숨김 상태를 경로별 localStorage와 동기화하는 훅.
 * 저장되는 폭은 사용자가 드래그로 정한 것뿐이며, 수동 폭이 없는 컬럼은
 * defaultWidths(캐시 표본이 늘면 재계산되는 최신 추정값)를 매번 따른다.
 */
export function useColumnLayout(
    path: string,
    names: string[],
    defaultWidths: number[],
): ColumnLayoutState {
    const [entry, setEntry] = useState<LayoutEntry>(() => ({
        path,
        layout: loadColumnLayout(path),
    }));

    // 경로가 바뀌면 렌더 중에 저장 상태를 다시 읽는다
    // (effect 안의 동기 setState는 react-hooks/set-state-in-effect 위반)
    if (entry.path !== path) {
        setEntry({ path, layout: loadColumnLayout(path) });
    }
    const stored = entry.path === path ? entry.layout : loadColumnLayout(path);

    // 레이아웃 갱신과 localStorage 저장을 한 곳에서 처리한다
    const persist = useCallback(
        (updater: (prev: ColumnLayout) => ColumnLayout) => {
            setEntry((prev) => {
                const next = updater(prev.layout);
                saveColumnLayout(prev.path, next);
                return { path: prev.path, layout: next };
            });
        },
        [],
    );

    // 유효 폭: 수동 저장값이 있는 컬럼만 그것을 쓰고, 나머지는 기본 폭.
    // 기본 폭은 저장하지 않으므로 표본이 늘어 재계산되면 그대로 반영된다.
    const widths = useMemo(
        () =>
            names.map(
                (_, i) => stored.widths[i] ?? defaultWidths[i] ?? MIN_COL_WIDTH,
            ),
        [names, stored.widths, defaultWidths],
    );
    const hidden = useMemo(
        () => new Set(stored.hidden.filter((i) => i >= 0 && i < names.length)),
        [stored.hidden, names.length],
    );
    const visible = useMemo(
        () => names.map((_, i) => i).filter((i) => !hidden.has(i)),
        [names, hidden],
    );
    const totalWidth = visible.reduce((sum, i) => sum + widths[i], 0);

    const setWidth = useCallback(
        (index: number, width: number) => {
            const w = Math.min(
                MAX_COL_WIDTH,
                Math.max(MIN_COL_WIDTH, Math.round(width)),
            );
            persist((prev) => {
                // 드래그한 컬럼의 폭만 수동값으로 저장한다
                const widths: (number | null)[] = Array.from(
                    { length: names.length },
                    (_, i) => prev.widths[i] ?? null,
                );
                widths[index] = w;
                return { ...prev, widths };
            });
        },
        [persist, names.length],
    );

    const toggleHidden = useCallback(
        (index: number) => {
            persist((prev) => {
                const cur = new Set(
                    prev.hidden.filter((i) => i >= 0 && i < names.length),
                );
                if (cur.has(index)) {
                    cur.delete(index);
                } else {
                    // 마지막으로 보이는 컬럼 하나는 숨길 수 없다
                    if (names.length - cur.size <= 1) return prev;
                    cur.add(index);
                }
                return {
                    ...prev,
                    hidden: [...cur].sort((a, b) => a - b),
                };
            });
        },
        [persist, names.length],
    );

    return { widths, hidden, visible, totalWidth, setWidth, toggleHidden };
}
