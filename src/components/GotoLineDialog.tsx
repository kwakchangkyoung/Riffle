import React, { useEffect, useRef, useState } from "react";
import { parseGotoInput } from "@/lib/merge";

interface GotoLineDialogProps {
    totalLines: number;
    indexing?: boolean; // 아직 인덱싱 중 (totalLines는 지금까지 확인된 줄 수)
    mergeMode?: boolean; // 병합 뷰: 줄 번호 외에 시각 입력도 허용
    onClose: () => void;
    onNavigate: (lineNo: number) => void; // 1-based 줄 번호
    onNavigateTime?: (tsMs: number) => void; // 병합 뷰 시각 이동 (epoch ms)
}

/**
 * Cmd+L 줄 번호 이동 모달.
 * 입력한 줄 번호(1-based)가 화면 위 1/3 지점에 오도록 이동합니다.
 * 병합 뷰에서는 "2026-11-12 07:15:42", "07:15" 같은 시각(로컬 시간대)도
 * 입력할 수 있습니다.
 */
export const GotoLineDialog: React.FC<GotoLineDialogProps> = ({
    totalLines,
    indexing = false,
    mergeMode = false,
    onClose,
    onNavigate,
    onNavigateTime,
}) => {
    const inputRef = useRef<HTMLInputElement>(null);
    const [value, setValue] = useState("");
    const [error, setError] = useState<string | null>(null);

    useEffect(() => {
        inputRef.current?.focus();
        inputRef.current?.select();
    }, []);

    const handleSubmit = () => {
        const trimmed = value.trim();
        if (mergeMode) {
            // 병합 뷰: 숫자면 줄 번호, 아니면 시각으로 해석한다
            const parsed = parseGotoInput(trimmed);
            if (parsed === null) {
                setError(
                    "줄 번호 또는 시각(예: 2026-11-12 07:15:42, 07:15)을 입력하세요",
                );
                return;
            }
            if (parsed.kind === "time") {
                if (onNavigateTime !== undefined) {
                    onNavigateTime(parsed.tsMs);
                } else {
                    setError("이 탭에서는 시각 이동을 지원하지 않습니다");
                }
                return;
            }
            if (parsed.line < 1 || parsed.line > totalLines) {
                setError(
                    indexing && parsed.line > totalLines
                        ? `아직 구축 중입니다 (현재 ${totalLines.toLocaleString("ko-KR")}줄까지 확인됨)`
                        : `1 – ${totalLines.toLocaleString("ko-KR")} 범위의 줄 번호를 입력하세요`,
                );
                return;
            }
            onNavigate(parsed.line);
            return;
        }
        if (!/^\d+$/.test(trimmed)) {
            setError("숫자를 입력하세요");
            return;
        }
        const lineNo = Number(trimmed);
        if (lineNo < 1 || lineNo > totalLines) {
            setError(
                indexing && lineNo > totalLines
                    ? `아직 인덱싱 중입니다 (현재 ${totalLines.toLocaleString("ko-KR")}줄까지 확인됨)`
                    : `1 – ${totalLines.toLocaleString("ko-KR")} 범위의 줄 번호를 입력하세요`,
            );
            return;
        }
        onNavigate(lineNo);
    };

    const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
        if (e.key === "Enter") {
            e.preventDefault();
            // 전역 Enter 핸들러(검색 결과 이동)가 다시 반응하지 않게 차단
            e.stopPropagation();
            handleSubmit();
        } else if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onClose();
        }
    };

    return (
        <div
            role="dialog"
            aria-modal="true"
            className="fixed inset-0 z-50 flex items-start justify-center pt-[20vh] bg-black/30 dark:bg-black/50"
            onMouseDown={(e) => {
                if (e.target === e.currentTarget) onClose();
            }}
        >
            <div className="w-80 rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-xl p-3">
                <div className="text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-2">
                    {mergeMode
                        ? "줄 번호 또는 시각 (로컬 시간)"
                        : `줄 번호 (1 – ${totalLines.toLocaleString("ko-KR")})`}
                </div>
                <input
                    ref={inputRef}
                    type="text"
                    value={value}
                    onChange={(e) => {
                        setValue(e.target.value);
                        setError(null);
                    }}
                    onKeyDown={handleKeyDown}
                    className={`w-full h-8 px-2 text-sm font-mono bg-white dark:bg-zinc-950 border rounded focus:outline-none focus:ring-1 ${
                        error
                            ? "border-rose-500 focus:ring-rose-500 text-zinc-900 dark:text-zinc-100"
                            : "border-zinc-300 dark:border-zinc-700 focus:ring-blue-500 text-zinc-900 dark:text-zinc-100"
                    }`}
                    placeholder={
                        mergeMode ? "12345 또는 2026-11-12 07:15:42" : "줄 번호"
                    }
                />
                {error && (
                    <div className="mt-1.5 text-[11px] font-mono text-rose-600 dark:text-rose-400">
                        {error}
                    </div>
                )}
            </div>
        </div>
    );
};
