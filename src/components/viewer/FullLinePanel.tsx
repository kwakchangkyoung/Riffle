import React, { useEffect, useState } from "react";
import { getFullLine, type FullLine } from "@/lib/ipc";
import { formatBytes } from "@/lib/format";

/** get_full_line이 반환하는 최대 바이트 (10MB)를 넘으면 잘린 것으로 표시 */
const FULL_LINE_LIMIT_BYTES = 10 * 1024 * 1024;

interface FullLinePanelProps {
    fileId: number;
    lineNo: number; // 0-based 줄 번호
    onClose: () => void;
}

type LoadState =
    | { status: "loading" }
    | { status: "error"; message: string }
    | { status: "loaded"; line: FullLine };

/**
 * 잘린 줄의 원문 전체를 보여주는 오른쪽 사이드 패널.
 */
export const FullLinePanel: React.FC<FullLinePanelProps> = ({
    fileId,
    lineNo,
    onClose,
}) => {
    const [state, setState] = useState<LoadState>({ status: "loading" });

    // Esc로 패널 닫기
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.key === "Escape") {
                e.stopPropagation();
                onClose();
            }
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [onClose]);

    // 줄 번호가 바뀌면 부모가 key로 리마운트하므로 여기서는 조회만 수행한다
    useEffect(() => {
        let cancelled = false;
        getFullLine(fileId, lineNo)
            .then((line) => {
                if (cancelled) return;
                if (line === null) {
                    setState({
                        status: "error",
                        message: "해당 줄을 찾을 수 없습니다.",
                    });
                } else {
                    setState({ status: "loaded", line });
                }
            })
            .catch((err) => {
                if (cancelled) return;
                const msg =
                    typeof err === "string" ? err : "줄을 불러올 수 없습니다.";
                setState({ status: "error", message: msg });
            });
        return () => {
            cancelled = true;
        };
    }, [fileId, lineNo]);

    return (
        <aside className="w-[40%] shrink-0 h-full flex flex-col border-l border-zinc-200 dark:border-zinc-800 bg-zinc-50/60 dark:bg-zinc-900/40">
            {/* 헤더: 줄 번호, 크기, 잘림 안내, 닫기 버튼 */}
            <div className="shrink-0 px-3 py-2 border-b border-zinc-200 dark:border-zinc-800 flex items-center justify-between gap-2">
                <div className="text-xs font-mono text-zinc-600 dark:text-zinc-400 truncate">
                    줄 {lineNo + 1}
                    {state.status === "loaded" && (
                        <>
                            {" · "}
                            {formatBytes(state.line.byteLen)}
                        </>
                    )}
                </div>
                <button
                    type="button"
                    onClick={onClose}
                    className="shrink-0 px-2 py-0.5 text-xs rounded text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                    title="닫기 (Esc)"
                >
                    닫기
                </button>
            </div>

            {/* 본문 */}
            <div className="flex-1 min-h-0 overflow-auto">
                {state.status === "loading" && (
                    <div className="p-4 text-xs text-zinc-500 dark:text-zinc-400">
                        불러오는 중…
                    </div>
                )}
                {state.status === "error" && (
                    <div className="p-4 text-xs text-rose-600 dark:text-rose-400">
                        {state.message}
                    </div>
                )}
                {state.status === "loaded" && (
                    <>
                        {state.line.truncated && (
                            <div className="px-3 py-1.5 text-[11px] text-amber-700 dark:text-amber-400 bg-amber-50 dark:bg-amber-950/40 border-b border-amber-200 dark:border-amber-900">
                                원본 줄이 {formatBytes(FULL_LINE_LIMIT_BYTES)}를
                                넘어 앞부분만 표시됩니다.
                            </div>
                        )}
                        <pre className="p-3 font-mono text-xs whitespace-pre-wrap break-all text-zinc-900 dark:text-zinc-100">
                            {state.line.text}
                        </pre>
                    </>
                )}
            </div>
        </aside>
    );
};
