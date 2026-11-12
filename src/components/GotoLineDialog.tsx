import React, { useEffect, useRef, useState } from "react";

interface GotoLineDialogProps {
    totalLines: number;
    onClose: () => void;
    onNavigate: (lineNo: number) => void; // 1-based 줄 번호
}

/**
 * Cmd+L 줄 번호 이동 모달.
 * 입력한 줄 번호(1-based)가 화면 위 1/3 지점에 오도록 이동합니다.
 */
export const GotoLineDialog: React.FC<GotoLineDialogProps> = ({
    totalLines,
    onClose,
    onNavigate,
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
        if (!/^\d+$/.test(trimmed)) {
            setError("숫자를 입력하세요");
            return;
        }
        const lineNo = Number(trimmed);
        if (lineNo < 1 || lineNo > totalLines) {
            setError(
                `1 – ${totalLines.toLocaleString("ko-KR")} 범위의 줄 번호를 입력하세요`,
            );
            return;
        }
        onNavigate(lineNo);
    };

    const handleKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
        if (e.key === "Enter") {
            e.preventDefault();
            handleSubmit();
        } else if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onClose();
        }
    };

    return (
        <div
            className="fixed inset-0 z-50 flex items-start justify-center pt-[20vh] bg-black/30 dark:bg-black/50"
            onMouseDown={(e) => {
                if (e.target === e.currentTarget) onClose();
            }}
        >
            <div className="w-80 rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-xl p-3">
                <div className="text-xs font-medium text-zinc-500 dark:text-zinc-400 mb-2">
                    줄 번호 (1 – {totalLines.toLocaleString("ko-KR")})
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
                    placeholder="줄 번호"
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
