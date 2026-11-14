import React, { useEffect, useState } from "react";
import type { TabMeta } from "@/components/TabBar";
import { baseNameOf } from "@/lib/merge";
import { remoteHostOf, s3BucketOf, s3DisplayPath } from "@/lib/remote";

/** 병합에 참여할 수 있는 원본 탭 수 범위 (Rust MIN/MAX_MERGE_SOURCES와 동일) */
export const MIN_MERGE_SOURCES = 2;
export const MAX_MERGE_SOURCES = 8;

interface MergeDialogProps {
    /** 병합 가능한 탭 목록 (이미 열린 비병합 탭, 탭 바 순서) */
    tabs: TabMeta[];
    onClose: () => void;
    /** 확인 시 선택된 탭 id 목록을 병합 순서(=동시각 우선순위)대로 넘긴다 */
    onMerge: (tabIds: string[]) => void;
}

/**
 * ⌘⇧M 로그 병합 대화상자.
 * 열린 탭에서 원본 2~8개를 체크로 고르고, 선택 목록에서 순서를 바꿉니다.
 * 선택 순서가 같은 타임스탬프를 가진 줄의 우선순위가 됩니다.
 */
export const MergeDialog: React.FC<MergeDialogProps> = ({
    tabs,
    onClose,
    onMerge,
}) => {
    // 선택된 탭 id를 병합 순서대로 보관한다 (체크한 순서로 추가됨)
    const [ordered, setOrdered] = useState<string[]>([]);

    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                onClose();
            }
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [onClose]);

    const toggle = (id: string) => {
        setOrdered((prev) =>
            prev.includes(id)
                ? prev.filter((t) => t !== id)
                : prev.length < MAX_MERGE_SOURCES
                  ? [...prev, id]
                  : prev,
        );
    };

    const move = (index: number, delta: -1 | 1) => {
        setOrdered((prev) => {
            const to = index + delta;
            if (to < 0 || to >= prev.length) return prev;
            const next = [...prev];
            const [moved] = next.splice(index, 1);
            next.splice(to, 0, moved);
            return next;
        });
    };

    const tabById = new Map(tabs.map((t) => [t.id, t]));
    const ok =
        ordered.length >= MIN_MERGE_SOURCES &&
        ordered.length <= MAX_MERGE_SOURCES;

    return (
        <div
            role="dialog"
            aria-modal="true"
            aria-label="로그 병합"
            className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh] bg-black/30 dark:bg-black/50"
            onMouseDown={(e) => {
                if (e.target === e.currentTarget) onClose();
            }}
        >
            <div className="w-[28rem] max-h-[75vh] flex flex-col rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-xl p-4">
                <div className="flex items-center justify-between mb-1">
                    <div className="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
                        로그 병합
                    </div>
                    <button
                        type="button"
                        onClick={onClose}
                        className="px-2 py-0.5 text-xs rounded text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                        title="닫기 (Esc)"
                    >
                        닫기
                    </button>
                </div>
                <p className="text-[11px] text-zinc-500 dark:text-zinc-400 mb-2">
                    열린 탭 {MIN_MERGE_SOURCES}~{MAX_MERGE_SOURCES}개를 골라
                    타임스탬프 순으로 섞은 가상 파일을 새 탭으로 엽니다. 위에
                    있을수록 같은 시각 줄이 먼저 옵니다.
                </p>

                {/* 열린 탭 목록 (체크로 선택) */}
                <div className="flex-1 min-h-0 overflow-y-auto border border-zinc-200 dark:border-zinc-700 rounded-md">
                    {tabs.map((tab) => {
                        const path = tab.file?.path ?? tab.path;
                        const checked = ordered.includes(tab.id);
                        const host = remoteHostOf(path);
                        const bucket = s3BucketOf(path);
                        return (
                            <label
                                key={tab.id}
                                title={s3DisplayPath(path, null)}
                                className="flex items-center gap-2 px-2.5 py-1.5 text-xs cursor-pointer border-b border-zinc-100 dark:border-zinc-800 last:border-b-0 hover:bg-zinc-50 dark:hover:bg-zinc-800/50"
                            >
                                <input
                                    type="checkbox"
                                    checked={checked}
                                    onChange={() => toggle(tab.id)}
                                    disabled={
                                        !checked &&
                                        ordered.length >= MAX_MERGE_SOURCES
                                    }
                                    className="accent-blue-600"
                                />
                                {host !== null && (
                                    <span className="text-[9px] font-bold px-1 py-px rounded bg-sky-100 dark:bg-sky-900/60 text-sky-700 dark:text-sky-300 shrink-0">
                                        {host}:
                                    </span>
                                )}
                                {bucket !== null && (
                                    <span className="text-[9px] font-bold px-1 py-px rounded bg-orange-100 dark:bg-orange-900/60 text-orange-700 dark:text-orange-300 shrink-0">
                                        s3:{bucket}
                                    </span>
                                )}
                                <span className="truncate text-zinc-800 dark:text-zinc-200">
                                    {baseNameOf(path)}
                                </span>
                                {checked && (
                                    <span className="ml-auto text-[10px] font-mono text-blue-600 dark:text-blue-400 shrink-0">
                                        {ordered.indexOf(tab.id) + 1}번
                                    </span>
                                )}
                            </label>
                        );
                    })}
                </div>

                {/* 병합 순서 (선택된 탭만, 위/아래로 조정) */}
                {ordered.length > 0 && (
                    <div className="mt-2 border border-zinc-200 dark:border-zinc-700 rounded-md max-h-40 overflow-y-auto">
                        {ordered.map((id, i) => {
                            const tab = tabById.get(id);
                            const path =
                                tab !== undefined
                                    ? (tab.file?.path ?? tab.path)
                                    : id;
                            return (
                                <div
                                    key={id}
                                    className="flex items-center gap-1.5 px-2.5 py-1 text-xs border-b border-zinc-100 dark:border-zinc-800 last:border-b-0"
                                    title={s3DisplayPath(path, null)}
                                >
                                    <span className="w-4 text-center font-mono text-[10px] text-zinc-400">
                                        {i + 1}
                                    </span>
                                    <span className="flex-1 truncate text-zinc-800 dark:text-zinc-200">
                                        {baseNameOf(path)}
                                    </span>
                                    <button
                                        type="button"
                                        onClick={() => move(i, -1)}
                                        disabled={i === 0}
                                        className="w-5 h-5 rounded text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-30 cursor-pointer"
                                        title="위로"
                                    >
                                        ↑
                                    </button>
                                    <button
                                        type="button"
                                        onClick={() => move(i, 1)}
                                        disabled={i === ordered.length - 1}
                                        className="w-5 h-5 rounded text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 disabled:opacity-30 cursor-pointer"
                                        title="아래로"
                                    >
                                        ↓
                                    </button>
                                    <button
                                        type="button"
                                        onClick={() => toggle(id)}
                                        className="w-5 h-5 rounded text-zinc-500 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                                        title="선택 해제"
                                    >
                                        ×
                                    </button>
                                </div>
                            );
                        })}
                    </div>
                )}

                <div className="flex items-center justify-between mt-3">
                    <span className="text-[11px] text-zinc-500 dark:text-zinc-400">
                        {ordered.length}개 선택됨
                    </span>
                    <button
                        type="button"
                        disabled={!ok}
                        onClick={() => onMerge(ordered)}
                        className={`px-3 py-1.5 text-xs font-medium rounded-md transition-colors ${
                            ok
                                ? "bg-blue-600 text-white hover:bg-blue-500 cursor-pointer shadow-xs"
                                : "bg-zinc-100 text-zinc-400 dark:bg-zinc-800 dark:text-zinc-600 cursor-not-allowed"
                        }`}
                    >
                        병합
                    </button>
                </div>
            </div>
        </div>
    );
};
