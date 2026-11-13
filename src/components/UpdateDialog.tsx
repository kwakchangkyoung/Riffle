import React, { useEffect, useState } from "react";
import { relaunch, type DownloadEvent, type Update } from "@/lib/updater";
import { formatBytes } from "@/lib/format";

/**
 * 업데이트 확인 결과 상태. App이 checkForUpdate 결과를 여기에 담아 넘긴다.
 */
export type UpdateDialogState =
    | { status: "checking" }
    | { status: "upToDate" }
    | { status: "available"; update: Update }
    | { status: "error"; message: string };

interface UpdateDialogProps {
    state: UpdateDialogState;
    onClose: () => void;
}

type InstallState =
    | { status: "idle" }
    | { status: "downloading"; loaded: number; total: number | null }
    | { status: "installed" }
    | { status: "error"; message: string };

const IDLE_INSTALL: InstallState = { status: "idle" };

/**
 * 앱 업데이트 확인/설치 모달.
 * 확인 결과(최신/새 버전/오류) 표시, 다운로드 진행률, 재시작 안내를 담당한다.
 */
export const UpdateDialog: React.FC<UpdateDialogProps> = ({
    state,
    onClose,
}) => {
    const [install, setInstall] = useState<InstallState>(IDLE_INSTALL);

    // 확인 상태가 바뀔 때마다 부모가 key를 바꿔 리마운트하므로
    // 설치 진행 상태는 새 확인마다 자동으로 초기화된다.
    const update = state.status === "available" ? state.update : null;
    // 다운로드 중에는 실수로 닫히지 않게 한다
    const closable = install.status !== "downloading";

    // Esc로 닫기 (다운로드 중에는 무시)
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.key !== "Escape") return;
            e.stopPropagation();
            if (closable) onClose();
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [closable, onClose]);

    // 업데이트 다운로드 및 설치. macOS는 설치 후 재시작해야 적용된다.
    const handleInstall = () => {
        if (update === null) return;
        let loaded = 0;
        let total: number | null = null;
        setInstall({ status: "downloading", loaded, total });
        update
            .downloadAndInstall((event: DownloadEvent) => {
                if (event.event === "Started") {
                    total = event.data.contentLength ?? null;
                } else if (event.event === "Progress") {
                    loaded += event.data.chunkLength;
                }
                setInstall({ status: "downloading", loaded, total });
            })
            .then(() => {
                setInstall({ status: "installed" });
            })
            .catch((err: unknown) => {
                const msg =
                    err instanceof Error
                        ? err.message
                        : "업데이트 설치에 실패했습니다.";
                setInstall({ status: "error", message: msg });
            });
    };

    const handleRestart = () => {
        relaunch().catch((err: unknown) => {
            console.error("앱 재시작 실패:", err);
        });
    };

    // 상태별 제목
    let title = "업데이트";
    if (state.status === "checking") title = "업데이트 확인 중";
    else if (state.status === "upToDate") title = "업데이트";
    else if (state.status === "available") title = "새 버전 알림";
    else if (state.status === "error") title = "업데이트 확인 실패";
    if (install.status === "installed") title = "업데이트 설치 완료";

    // 새 버전 정보의 문자열을 표시용으로 변환 (파싱 불가 시 원문)
    const releaseDate = (() => {
        if (update?.date === undefined) return null;
        const d = new Date(update.date);
        return Number.isNaN(d.getTime())
            ? update.date
            : d.toLocaleDateString("ko-KR");
    })();

    const percent =
        install.status === "downloading" &&
        install.total !== null &&
        install.total > 0
            ? Math.min(100, (install.loaded / install.total) * 100)
            : null;

    return (
        <div
            role="dialog"
            aria-modal="true"
            className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] bg-black/30 dark:bg-black/50"
            onMouseDown={(e) => {
                if (e.target === e.currentTarget && closable) onClose();
            }}
        >
            <div className="w-[30rem] max-w-[92vw] rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-xl p-4 flex flex-col gap-3">
                <div className="text-sm font-semibold text-zinc-900 dark:text-zinc-100">
                    {title}
                </div>

                {/* 확인 중 */}
                {state.status === "checking" && (
                    <div className="text-xs text-zinc-500 dark:text-zinc-400">
                        업데이트를 확인하는 중입니다…
                    </div>
                )}

                {/* 최신 버전 */}
                {state.status === "upToDate" && (
                    <div className="text-xs text-zinc-500 dark:text-zinc-400">
                        최신 버전을 사용 중입니다.
                    </div>
                )}

                {/* 확인 오류 */}
                {state.status === "error" && (
                    <div className="text-xs text-rose-600 dark:text-rose-400 whitespace-pre-wrap">
                        {state.message}
                    </div>
                )}

                {/* 새 버전 정보 + 설치 진행 */}
                {update !== null && (
                    <>
                        <div className="text-xs text-zinc-600 dark:text-zinc-400">
                            현재 v{update.currentVersion} → 새 버전 v
                            {update.version}
                            {releaseDate !== null && ` (${releaseDate})`}
                        </div>

                        {update.body !== undefined &&
                            update.body.length > 0 &&
                            install.status !== "installed" && (
                                <div className="max-h-44 overflow-auto rounded border border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-950/60 p-2">
                                    <pre className="text-[11px] font-mono whitespace-pre-wrap break-words text-zinc-700 dark:text-zinc-300">
                                        {update.body}
                                    </pre>
                                </div>
                            )}

                        {install.status === "downloading" && (
                            <div className="flex flex-col gap-1.5">
                                <div className="h-1.5 rounded-full bg-zinc-200 dark:bg-zinc-800 overflow-hidden">
                                    {percent !== null && (
                                        <div
                                            className="h-full bg-blue-500 transition-[width]"
                                            style={{ width: `${percent}%` }}
                                        />
                                    )}
                                </div>
                                <div className="text-[11px] text-zinc-500 dark:text-zinc-400">
                                    다운로드 중… {formatBytes(install.loaded)}
                                    {install.total !== null &&
                                        ` / ${formatBytes(install.total)}`}
                                    {percent !== null &&
                                        ` (${Math.floor(percent)}%)`}
                                </div>
                            </div>
                        )}

                        {install.status === "installed" && (
                            <div className="text-xs text-emerald-700 dark:text-emerald-400">
                                업데이트가 설치되었습니다. 앱을 다시 시작하면 새
                                버전이 적용됩니다.
                            </div>
                        )}

                        {install.status === "error" && (
                            <div className="text-xs text-rose-600 dark:text-rose-400 whitespace-pre-wrap">
                                {install.message}
                            </div>
                        )}
                    </>
                )}

                {/* 버튼 영역 */}
                <div className="flex justify-end gap-2 pt-1">
                    {state.status === "available" &&
                        install.status === "idle" && (
                            <>
                                <button
                                    type="button"
                                    onClick={onClose}
                                    className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                                >
                                    나중에
                                </button>
                                <button
                                    type="button"
                                    onClick={handleInstall}
                                    className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 cursor-pointer"
                                >
                                    업데이트
                                </button>
                            </>
                        )}

                    {state.status === "available" &&
                        install.status === "error" && (
                            <>
                                <button
                                    type="button"
                                    onClick={onClose}
                                    className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                                >
                                    나중에
                                </button>
                                <button
                                    type="button"
                                    onClick={handleInstall}
                                    className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 cursor-pointer"
                                >
                                    다시 시도
                                </button>
                            </>
                        )}

                    {state.status === "available" &&
                        install.status === "installed" && (
                            <>
                                <button
                                    type="button"
                                    onClick={onClose}
                                    className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                                >
                                    나중에 재시작
                                </button>
                                <button
                                    type="button"
                                    onClick={handleRestart}
                                    className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 cursor-pointer"
                                >
                                    지금 재시작
                                </button>
                            </>
                        )}

                    {(state.status === "checking" ||
                        state.status === "upToDate" ||
                        state.status === "error") &&
                        closable && (
                            <button
                                type="button"
                                onClick={onClose}
                                className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                            >
                                닫기
                            </button>
                        )}
                </div>
            </div>
        </div>
    );
};
