import React, { useCallback, useEffect, useState } from "react";
import type { RuleKind } from "@/lib/ipc";
import { COLUMN_HEADER_HEIGHT, MIN_COL_WIDTH } from "./columnLayout";

interface ColumnHeaderProps {
    /** 전체 컬럼 이름 */
    names: string[];
    /** 전체 컬럼 기준 유효 폭 */
    widths: number[];
    /** 보이는 컬럼 인덱스 */
    visible: number[];
    /** 숨긴 컬럼 인덱스 집합 */
    hidden: ReadonlySet<number>;
    /** 왼쪽 줄번호 거터 폭 — 헤더와 본문 정렬용 (px) */
    gutterWidth: number;
    /** 본문 콘텐츠 min-width와 같은 값 — 헤더/본문 가로 범위 일치용 (px) */
    contentMinWidth: number;
    /** 헤더 내용 요소 ref — 본문 가로 스크롤 위치를 transform으로 미러링한다 */
    contentRef: React.RefObject<HTMLDivElement | null>;
    onResize: (index: number, width: number) => void;
    onToggleHidden: (index: number) => void;
}

/**
 * 컬럼 뷰 고정 헤더.
 * 컬럼 이름 표시, 폭 드래그 조절(최소 40px), 우클릭/▾ 버튼으로 컬럼 숨기기·보이기.
 * 본문 스크롤은 부모가 contentRef의 transform을 갱신해 동기화한다.
 */
export const ColumnHeader: React.FC<ColumnHeaderProps> = ({
    names,
    widths,
    visible,
    hidden,
    gutterWidth,
    contentMinWidth,
    contentRef,
    onResize,
    onToggleHidden,
}) => {
    const [menuPos, setMenuPos] = useState<{ x: number; y: number } | null>(
        null,
    );

    // 컬럼 폭 드래그: mousedown 지점부터의 이동량을 시작 폭에 더한다
    const startResize = useCallback(
        (e: React.MouseEvent, index: number) => {
            e.preventDefault();
            e.stopPropagation();
            const startX = e.clientX;
            const startWidth = widths[index] ?? MIN_COL_WIDTH;
            const onMove = (ev: MouseEvent) => {
                onResize(index, startWidth + ev.clientX - startX);
            };
            const onUp = () => {
                window.removeEventListener("mousemove", onMove);
                window.removeEventListener("mouseup", onUp);
            };
            window.addEventListener("mousemove", onMove);
            window.addEventListener("mouseup", onUp);
        },
        [widths, onResize],
    );

    // Esc로 컬럼 표시 메뉴 닫기
    useEffect(() => {
        if (menuPos === null) return;
        const onKey = (e: KeyboardEvent) => {
            if (e.key === "Escape") {
                e.preventDefault();
                setMenuPos(null);
            }
        };
        window.addEventListener("keydown", onKey);
        return () => window.removeEventListener("keydown", onKey);
    }, [menuPos]);

    const openMenu = (x: number, y: number) => {
        // 메뉴가 창 밖으로 나가지 않게 위치를 제한한다
        setMenuPos({
            x: Math.min(x, window.innerWidth - 180),
            y: Math.min(
                y,
                window.innerHeight - 40 * Math.min(10, names.length),
            ),
        });
    };

    return (
        <div
            className="flex shrink-0 items-stretch border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900 text-zinc-500 dark:text-zinc-400 select-none"
            style={{ height: COLUMN_HEADER_HEIGHT }}
            onContextMenu={(e) => {
                e.preventDefault();
                openMenu(e.clientX, e.clientY);
            }}
        >
            {/* 거터 자리: 본문 줄번호 거터와 같은 폭으로 비워 둔다 */}
            <div
                className="shrink-0 border-r border-zinc-200 dark:border-zinc-800"
                style={{ width: gutterWidth }}
            />
            <div className="relative flex-1 overflow-hidden">
                <div
                    ref={contentRef}
                    className="flex h-full"
                    style={{
                        minWidth: `max(100%, ${contentMinWidth}px)`,
                        // 우측 "컬럼 ▾" 버튼이 마지막 컬럼을 덮지 않게 여백을 둔다
                        paddingRight: 56,
                    }}
                >
                    {visible.map((ci) => (
                        <div
                            key={ci}
                            className="relative flex h-full items-center shrink-0 px-2 font-mono text-[11px] font-semibold border-r border-zinc-200 dark:border-zinc-800"
                            style={{ width: widths[ci] }}
                            title={names[ci]}
                        >
                            <span className="truncate">{names[ci]}</span>
                            <div
                                className="absolute top-0 right-0 h-full w-1.5 cursor-col-resize hover:bg-blue-400/50"
                                onMouseDown={(e) => startResize(e, ci)}
                                onContextMenu={(e) => e.stopPropagation()}
                            />
                        </div>
                    ))}
                </div>
                {/* 컬럼 숨기기/보이기 메뉴 버튼 (헤더 우클릭으로도 열린다) */}
                <button
                    type="button"
                    onClick={(e) => {
                        const rect = e.currentTarget.getBoundingClientRect();
                        openMenu(rect.left, rect.bottom + 2);
                    }}
                    onContextMenu={(e) => e.stopPropagation()}
                    className="absolute right-0 top-0 h-full px-1.5 text-[10px] bg-zinc-50 dark:bg-zinc-900 border-l border-zinc-200 dark:border-zinc-800 hover:bg-zinc-200 dark:hover:bg-zinc-800 cursor-pointer"
                    title="컬럼 숨기기 / 보이기"
                >
                    컬럼 ▾
                </button>
            </div>
            {/* 스크롤바 자리: 본문 커스텀 스크롤바(w-3.5)와 같은 폭으로 비워 둔다 */}
            <div className="w-3.5 shrink-0 border-l border-zinc-200 dark:border-zinc-800" />

            {/* 컬럼 표시 메뉴 */}
            {menuPos !== null && (
                <>
                    <div
                        className="fixed inset-0 z-40"
                        onClick={() => setMenuPos(null)}
                        onContextMenu={(e) => {
                            e.preventDefault();
                            setMenuPos(null);
                        }}
                    />
                    <div
                        className="fixed z-50 min-w-40 max-h-64 overflow-y-auto rounded-lg border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-lg py-1"
                        style={{ left: menuPos.x, top: menuPos.y }}
                    >
                        <div className="px-2.5 py-1 text-[10px] text-zinc-400">
                            컬럼 표시
                        </div>
                        {names.map((name, i) => (
                            <button
                                key={i}
                                type="button"
                                onClick={() => onToggleHidden(i)}
                                className="w-full flex items-center gap-2 px-2.5 py-1 text-xs text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800 cursor-pointer"
                            >
                                <span className="w-3 shrink-0 text-center text-blue-600 dark:text-blue-400">
                                    {hidden.has(i) ? "" : "✓"}
                                </span>
                                <span className="truncate font-mono">
                                    {name}
                                </span>
                            </button>
                        ))}
                    </div>
                </>
            )}
        </div>
    );
};

/** 셀 우클릭 필드 필터 메뉴의 대상 정보 */
export interface CellMenu {
    x: number;
    y: number;
    /** 대상 컬럼 이름 (필터 규칙의 field) */
    field: string;
    /** 셀 값 (필터 패턴으로 쓴다) */
    value: string;
}

interface FieldFilterMenuProps {
    menu: CellMenu;
    /** "이 값만 보기"(include) / "이 값 제외"(exclude) 선택 */
    onFilter: (field: string, value: string, kind: RuleKind) => void;
    onClose: () => void;
}

/**
 * 셀 우클릭 메뉴: 해당 컬럼 값으로 포함/제외 필드 필터를 추가한다.
 */
export const FieldFilterMenu: React.FC<FieldFilterMenuProps> = ({
    menu,
    onFilter,
    onClose,
}) => {
    useEffect(() => {
        const onKey = (e: KeyboardEvent) => {
            if (e.key === "Escape") {
                e.preventDefault();
                onClose();
            }
        };
        window.addEventListener("keydown", onKey);
        return () => window.removeEventListener("keydown", onKey);
    }, [onClose]);

    const left = Math.min(menu.x, window.innerWidth - 200);
    const top = Math.min(menu.y, window.innerHeight - 110);

    return (
        <>
            <div
                className="fixed inset-0 z-40"
                onClick={onClose}
                onContextMenu={(e) => {
                    e.preventDefault();
                    onClose();
                }}
            />
            <div
                className="fixed z-50 min-w-36 rounded-lg border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-lg py-1"
                style={{ left, top }}
            >
                <div
                    className="px-2.5 py-1 text-[10px] text-zinc-400 truncate max-w-64"
                    title={`${menu.field} = ${menu.value}`}
                >
                    {menu.field} = {menu.value}
                </div>
                <button
                    type="button"
                    onClick={() => {
                        onFilter(menu.field, menu.value, "include");
                        onClose();
                    }}
                    className="w-full text-left px-2.5 py-1 text-xs text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800 cursor-pointer"
                >
                    이 값만 보기
                </button>
                <button
                    type="button"
                    onClick={() => {
                        onFilter(menu.field, menu.value, "exclude");
                        onClose();
                    }}
                    className="w-full text-left px-2.5 py-1 text-xs text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800 cursor-pointer"
                >
                    이 값 제외
                </button>
            </div>
        </>
    );
};
