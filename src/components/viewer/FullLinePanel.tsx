import React, { useCallback, useEffect, useMemo, useState } from "react";
import { getFullLine, type FullLine } from "@/lib/ipc";
import { formatBytes } from "@/lib/format";
import { parseLineJson } from "@/lib/jsonLine";
import { JsonTree } from "./JsonTree";

/** get_full_line이 반환하는 최대 바이트 (10MB)를 넘으면 잘린 것으로 표시 */
const FULL_LINE_LIMIT_BYTES = 10 * 1024 * 1024;

interface FullLinePanelProps {
    fileId: number;
    lineNo: number; // 0-based 줄 번호
    onClose: () => void;
    /** 값 복사 등의 짧은 안내를 상태바 토스트로 표시 */
    onToast?: (message: string) => void;
}

type LoadState =
    | { status: "loading" }
    | { status: "error"; message: string }
    | { status: "loaded"; line: FullLine };

/**
 * 잘린 줄의 원문 전체를 보여주는 오른쪽 사이드 패널.
 * 줄 안에 JSON이 있으면 [원문 | 트리] 전환이 표시된다.
 */
export const FullLinePanel: React.FC<FullLinePanelProps> = ({
    fileId,
    lineNo,
    onClose,
    onToast,
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

    // 줄에서 첫 { 또는 [ 부터 끝까지가 JSON으로 파싱되면 트리 보기를 제공한다.
    // 줄 텍스트 기준으로만 계산하므로 같은 줄을 다시 열어도 캐시가 재사용된다.
    const json = useMemo(
        () =>
            state.status === "loaded" ? parseLineJson(state.line.text) : null,
        [state],
    );

    // JSON이 있으면 트리를 기본으로 표시 (패널은 줄마다 리마운트되므로 초기값만 둔다)
    const [view, setView] = useState<"raw" | "tree">("tree");
    const treeView = json !== null && view === "tree";

    // 트리 값 클릭 → 클립보드 복사 (성공/실패를 토스트로 안내)
    const handleCopyValue = useCallback(
        (text: string) => {
            void (async () => {
                try {
                    await navigator.clipboard.writeText(text);
                    onToast?.("값을 복사했습니다");
                } catch {
                    try {
                        const ta = document.createElement("textarea");
                        ta.value = text;
                        ta.style.position = "fixed";
                        ta.style.opacity = "0";
                        document.body.appendChild(ta);
                        ta.select();
                        document.execCommand("copy");
                        ta.remove();
                        onToast?.("값을 복사했습니다");
                    } catch {
                        onToast?.("복사에 실패했습니다");
                    }
                }
            })();
        },
        [onToast],
    );

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
                <div className="flex items-center gap-2 shrink-0">
                    {/* JSON 줄 전용 [원문 | 트리] 전환 */}
                    {json !== null && (
                        <div className="flex rounded-md border border-zinc-300 dark:border-zinc-600 overflow-hidden text-xs">
                            <button
                                type="button"
                                onClick={() => setView("raw")}
                                className={`px-2 py-0.5 cursor-pointer ${
                                    !treeView
                                        ? "bg-zinc-700 text-white dark:bg-zinc-300 dark:text-zinc-900"
                                        : "text-zinc-600 dark:text-zinc-300 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                                }`}
                            >
                                원문
                            </button>
                            <button
                                type="button"
                                onClick={() => setView("tree")}
                                className={`px-2 py-0.5 cursor-pointer border-l border-zinc-300 dark:border-zinc-600 ${
                                    treeView
                                        ? "bg-zinc-700 text-white dark:bg-zinc-300 dark:text-zinc-900"
                                        : "text-zinc-600 dark:text-zinc-300 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                                }`}
                            >
                                트리
                            </button>
                        </div>
                    )}
                    <button
                        type="button"
                        onClick={onClose}
                        className="px-2 py-0.5 text-xs rounded text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                        title="닫기 (Esc)"
                    >
                        닫기
                    </button>
                </div>
            </div>

            {/* 본문 (select-text: Cmd+C 시 DOM 선택 내용을 복사할 수 있게) */}
            <div className="flex-1 min-h-0 overflow-auto select-text">
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
                        {treeView ? (
                            <JsonTree value={json} onCopy={handleCopyValue} />
                        ) : (
                            <pre className="p-3 font-mono text-xs whitespace-pre-wrap break-all text-zinc-900 dark:text-zinc-100">
                                {state.line.text}
                            </pre>
                        )}
                    </>
                )}
            </div>
        </aside>
    );
};
