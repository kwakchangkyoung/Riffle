import React from "react";
import { isRemotePath } from "@/lib/remote";

interface EmptyStateProps {
    recentFiles: string[];
    onOpenDialog: () => void;
    onOpenRemote: () => void;
    onOpenPath: (path: string) => void;
    onShowShortcuts: () => void;
}

/**
 * 탭이 하나도 없을 때 표시하는 빈 화면.
 * 클릭하면 파일 열기 다이얼로그를 열고, 최근 파일 목록을 보여준다.
 */
export const EmptyState: React.FC<EmptyStateProps> = ({
    recentFiles,
    onOpenDialog,
    onOpenRemote,
    onOpenPath,
    onShowShortcuts,
}) => {
    return (
        <div
            onClick={onOpenDialog}
            className="flex-1 flex flex-col items-center justify-center cursor-pointer hover:bg-zinc-50/50 dark:hover:bg-zinc-900/30 transition-colors min-h-0"
        >
            <div className="max-w-md p-8 rounded-2xl border-2 border-dashed border-zinc-300 dark:border-zinc-800 flex flex-col items-center text-center space-y-3">
                <div className="w-12 h-12 rounded-full bg-zinc-100 dark:bg-zinc-900 flex items-center justify-center text-zinc-400 dark:text-zinc-500">
                    <svg
                        xmlns="http://www.w3.org/2000/svg"
                        className="w-6 h-6"
                        fill="none"
                        viewBox="0 0 24 24"
                        stroke="currentColor"
                    >
                        <path
                            strokeLinecap="round"
                            strokeLinejoin="round"
                            strokeWidth={1.5}
                            d="M9 13h6m-3-3v6m5 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z"
                        />
                    </svg>
                </div>
                <div className="space-y-1">
                    <p className="text-sm font-medium text-zinc-800 dark:text-zinc-200">
                        Cmd+O 로 열거나 파일을 끌어다 놓으세요
                    </p>
                    <p className="text-xs text-zinc-500 dark:text-zinc-400">
                        대용량 텍스트 로그 파일을 빠르게 탐색할 수 있습니다
                    </p>
                </div>
                {/* 원격(SFTP) 파일 열기 — 카드 클릭(로컬 파일 열기)과 분리 */}
                <button
                    type="button"
                    onClick={(e) => {
                        e.stopPropagation();
                        onOpenRemote();
                    }}
                    className="mt-1 px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                >
                    원격 파일 열기 (⌘⇧O)
                </button>
                {/* 단축키 도움말 링크 */}
                <button
                    type="button"
                    onClick={(e) => {
                        e.stopPropagation();
                        onShowShortcuts();
                    }}
                    className="text-[11px] text-zinc-400 dark:text-zinc-500 hover:text-zinc-600 dark:hover:text-zinc-300 hover:underline cursor-pointer"
                >
                    단축키 보기 ⌘/
                </button>
            </div>

            {/* 최근 파일 목록 */}
            {recentFiles.length > 0 && (
                <div
                    className="mt-6 w-full max-w-md"
                    onClick={(e) => e.stopPropagation()}
                >
                    <div className="text-xs text-zinc-500 dark:text-zinc-400 mb-2 text-center">
                        최근 파일
                    </div>
                    <div className="rounded-lg border border-zinc-200 dark:border-zinc-800 bg-white/70 dark:bg-zinc-900/70 divide-y divide-zinc-100 dark:divide-zinc-800 overflow-hidden">
                        {recentFiles.slice(0, 10).map((path) => (
                            <button
                                key={path}
                                type="button"
                                onClick={() => onOpenPath(path)}
                                className="w-full px-3 py-1.5 text-left text-xs font-mono text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800 truncate cursor-pointer"
                                title={path}
                            >
                                {isRemotePath(path) ? (
                                    <span className="text-sky-600 dark:text-sky-400">
                                        {path}
                                    </span>
                                ) : (
                                    path
                                )}
                            </button>
                        ))}
                    </div>
                </div>
            )}
        </div>
    );
};
