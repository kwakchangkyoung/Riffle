import React, {
    useCallback,
    useEffect,
    useMemo,
    useRef,
    useState,
} from "react";
import {
    getAgentHosts,
    listRemoteDir,
    listSshHosts,
    remoteHome,
    setAgentEnabled,
    type RemoteEntry,
    type SshHost,
} from "@/lib/ipc";
import { remoteUriOf } from "@/lib/remote";
import { formatBytes, formatMtime } from "@/lib/format";
import { S3Panel } from "@/components/remote/S3Panel";

/** 최근 원격 위치 최대 개수 */
const MAX_RECENT_LOCS = 10;
/** 최근 위치 localStorage 키 */
const RECENT_LOCS_KEY = "riffle.remoteRecentLocations";

interface RecentLoc {
    host: string;
    dir: string;
}

interface RemoteOpenDialogProps {
    onClose: () => void;
    /** 선택한 원격 파일들을 sftp:// URI 목록으로 넘겨 탭으로 연다 */
    onOpenPaths: (paths: string[]) => void;
}

function toErrorMessage(err: unknown): string {
    if (typeof err === "string") return err;
    if (err instanceof Error) return err.message;
    return "알 수 없는 오류가 발생했습니다.";
}

/** 경로의 상위 디렉터리를 반환한다. 루트("/")의 상위는 루프 방지로 "/" */
function parentDir(path: string): string {
    const norm = path.replace(/\/+$/, "");
    const idx = norm.lastIndexOf("/");
    if (idx <= 0) return "/";
    return norm.slice(0, idx);
}

/** 디렉터리 경로와 항목 이름을 결합한다 */
function joinPath(dir: string, name: string): string {
    return dir.endsWith("/") ? `${dir}${name}` : `${dir}/${name}`;
}

function loadRecentLocs(): RecentLoc[] {
    try {
        const raw = localStorage.getItem(RECENT_LOCS_KEY);
        if (raw === null) return [];
        const parsed: unknown = JSON.parse(raw);
        if (!Array.isArray(parsed)) return [];
        return parsed.filter(
            (v): v is RecentLoc =>
                typeof v === "object" &&
                v !== null &&
                typeof (v as RecentLoc).host === "string" &&
                typeof (v as RecentLoc).dir === "string",
        );
    } catch {
        return [];
    }
}

function saveRecentLocs(locs: RecentLoc[]): void {
    try {
        localStorage.setItem(RECENT_LOCS_KEY, JSON.stringify(locs));
    } catch {
        // 저장 실패는 무시한다
    }
}

/**
 * 원격(SFTP) 파일 열기 대화상자.
 * ~/.ssh/config 호스트 선택 또는 직접 입력으로 접속하고,
 * 디렉터리를 탐색해 파일을 탭으로 연다.
 */
export const RemoteOpenDialog: React.FC<RemoteOpenDialogProps> = ({
    onClose,
    onOpenPaths,
}) => {
    // 상단 세그먼트: SSH(SFTP) / S3 탭 전환
    const [mode, setMode] = useState<"ssh" | "s3">("ssh");
    const [hosts, setHosts] = useState<SshHost[]>([]);
    const [hostInput, setHostInput] = useState("");
    const [hostFocused, setHostFocused] = useState(false);
    const [connectedHost, setConnectedHost] = useState<string | null>(null);
    // 서버 측 가속(에이전트)이 켜진 호스트 목록 (settings.json의 agentHosts)
    const [agentHosts, setAgentHosts] = useState<Set<string>>(new Set());

    const [dirInput, setDirInput] = useState("");
    const [dir, setDir] = useState<string | null>(null);
    const [entries, setEntries] = useState<RemoteEntry[]>([]);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const [selected, setSelected] = useState<Set<string>>(new Set());
    const anchorRef = useRef<string | null>(null);
    // 최근 위치는 localStorage에서 동기적으로 읽어 초기값으로 사용한다
    const [recentLocs, setRecentLocs] = useState<RecentLoc[]>(loadRecentLocs);

    const fileEntries = useMemo(
        () => entries.filter((e) => !e.isDir),
        [entries],
    );

    // ~/.ssh/config 호스트 목록 로드
    useEffect(() => {
        let cancelled = false;
        listSshHosts()
            .then((list) => {
                if (!cancelled) setHosts(list);
            })
            .catch((err) => {
                console.warn("SSH 호스트 목록 조회 실패:", err);
            });
        getAgentHosts()
            .then((list) => {
                if (!cancelled) setAgentHosts(new Set(list));
            })
            .catch((err) => {
                console.warn("에이전트 호스트 목록 조회 실패:", err);
            });
        return () => {
            cancelled = true;
        };
    }, []);

    // 디렉터리 목록 조회. 성공 시 현재 경로/입력창/선택/최근 위치를 갱신한다.
    const loadDir = useCallback(async (host: string, path: string) => {
        setLoading(true);
        setError(null);
        try {
            const list = await listRemoteDir(host, path);
            setDir(path);
            setDirInput(path);
            setEntries(list);
            setSelected(new Set());
            anchorRef.current = null;
            setRecentLocs((prev) => {
                const next = [
                    { host, dir: path },
                    ...prev.filter((l) => !(l.host === host && l.dir === path)),
                ].slice(0, MAX_RECENT_LOCS);
                saveRecentLocs(next);
                return next;
            });
        } catch (err) {
            setError(toErrorMessage(err));
        } finally {
            setLoading(false);
        }
    }, []);

    // 호스트 접속: 홈 디렉터리를 시작 경로로 잡고 목록을 불러온다.
    // 성공 시 접속된 호스트를 반환한다 (실패 시 null).
    const connect = useCallback(
        async (host: string): Promise<string | null> => {
            const h = host.trim();
            if (h.length === 0) return null;
            setError(null);
            setLoading(true);
            setEntries([]);
            setSelected(new Set());
            try {
                const home = await remoteHome(h);
                setConnectedHost(h);
                setHostInput(h);
                setDir(home);
                setDirInput(home);
                await loadDir(h, home);
                return h;
            } catch (err) {
                setError(toErrorMessage(err));
                setLoading(false);
                return null;
            }
        },
        [loadDir],
    );

    // 경로 입력창의 경로로 이동. 미접속 상태면 먼저 호스트 접속을 시도하고
    // 접속에 성공하면 입력한 경로로 바로 이동한다.
    const navigateTo = useCallback(
        async (rawPath: string) => {
            const p = rawPath.trim();
            if (p.length === 0) return;
            const path = p.startsWith("/") ? p : `/${p}`;
            if (connectedHost === null) {
                const h = await connect(hostInput);
                if (h !== null) {
                    await loadDir(h, path);
                }
                return;
            }
            await loadDir(connectedHost, path);
        },
        [connectedHost, connect, hostInput, loadDir],
    );

    // 서버 측 가속(에이전트) 체크 변경: 체크 상태를 먼저 반영하고 저장한다.
    // 저장에 실패하면 체크 상태를 되돌린다.
    const handleAgentToggle = useCallback(
        (enabled: boolean) => {
            const host = connectedHost;
            if (host === null) return;
            setAgentHosts((prev) => {
                const next = new Set(prev);
                if (enabled) next.add(host);
                else next.delete(host);
                return next;
            });
            setAgentEnabled(host, enabled).catch((err) => {
                console.warn("에이전트 설정 저장 실패:", err);
                setAgentHosts((prev) => {
                    const next = new Set(prev);
                    if (enabled) next.delete(host);
                    else next.add(host);
                    return next;
                });
            });
        },
        [connectedHost],
    );

    // 에러 시 재시도: 접속된 호스트가 있으면 현재 경로를 다시 읽고,
    // 아니면 호스트 접속부터 다시 시도한다.
    const handleRetry = useCallback(() => {
        if (connectedHost !== null && dir !== null) {
            void loadDir(connectedHost, dir);
        } else {
            void connect(hostInput);
        }
    }, [connectedHost, dir, loadDir, connect, hostInput]);

    // 호스트 자동완성 후보 (입력 텍스트로 alias/hostname/user 부분 일치 필터)
    const hostSuggestions = useMemo(() => {
        if (!hostFocused || hosts.length === 0) return [];
        const q = hostInput.trim().toLowerCase();
        if (q.length === 0) return hosts;
        return hosts.filter(
            (h) =>
                h.alias.toLowerCase().includes(q) ||
                (h.hostname?.toLowerCase().includes(q) ?? false) ||
                (h.user?.toLowerCase().includes(q) ?? false),
        );
    }, [hostFocused, hosts, hostInput]);

    // 파일 항목 클릭: 일반=단일 선택, Cmd/Ctrl=토글 추가, Shift=범위 선택
    const handleEntryClick = useCallback(
        (entry: RemoteEntry, e: React.MouseEvent) => {
            if (entry.isDir) return;
            const name = entry.name;
            if (e.shiftKey && anchorRef.current !== null) {
                const names = fileEntries.map((f) => f.name);
                const a = names.indexOf(anchorRef.current);
                const b = names.indexOf(name);
                if (a >= 0 && b >= 0) {
                    const [lo, hi] = a < b ? [a, b] : [b, a];
                    setSelected(new Set(names.slice(lo, hi + 1)));
                    return;
                }
            }
            if (e.metaKey || e.ctrlKey) {
                setSelected((prev) => {
                    const next = new Set(prev);
                    if (next.has(name)) {
                        next.delete(name);
                    } else {
                        next.add(name);
                    }
                    return next;
                });
            } else {
                setSelected(new Set([name]));
            }
            anchorRef.current = name;
        },
        [fileEntries],
    );

    // 선택한 파일(또는 더블클릭한 파일)을 sftp:// URI로 열기 요청한다
    const openFiles = useCallback(
        (names: string[]) => {
            if (connectedHost === null || dir === null || names.length === 0) {
                return;
            }
            onOpenPaths(
                names.map((n) => remoteUriOf(connectedHost, joinPath(dir, n))),
            );
            onClose();
        },
        [connectedHost, dir, onOpenPaths, onClose],
    );

    const handleOpenSelected = useCallback(() => {
        openFiles([...selected]);
    }, [openFiles, selected]);

    // Esc로 닫기 (입력창에서는 각 입력이 우선 처리)
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.key !== "Escape") return;
            e.stopPropagation();
            onClose();
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [onClose]);

    const canOpen = selected.size > 0 && !loading;

    return (
        <div
            role="dialog"
            aria-modal="true"
            aria-label="원격 파일 열기"
            className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh] bg-black/30 dark:bg-black/50"
            onMouseDown={(e) => {
                if (e.target === e.currentTarget) onClose();
            }}
        >
            <div className="w-[46rem] max-w-[94vw] max-h-[80vh] rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-xl flex flex-col">
                {/* 헤더 */}
                <div className="flex items-center justify-between px-4 py-2.5 border-b border-zinc-200 dark:border-zinc-800">
                    <div className="flex items-center gap-3">
                        <div className="text-sm font-semibold text-zinc-900 dark:text-zinc-100">
                            원격 파일 열기
                        </div>
                        {/* SSH / S3 세그먼트 전환 */}
                        <div className="flex rounded-md border border-zinc-300 dark:border-zinc-700 overflow-hidden text-xs font-medium">
                            {(["ssh", "s3"] as const).map((m) => (
                                <button
                                    key={m}
                                    type="button"
                                    onClick={() => setMode(m)}
                                    className={`px-2.5 py-1 cursor-pointer ${
                                        mode === m
                                            ? "bg-zinc-900 text-zinc-50 dark:bg-zinc-100 dark:text-zinc-900"
                                            : "bg-white text-zinc-600 hover:bg-zinc-100 dark:bg-zinc-900 dark:text-zinc-400 dark:hover:bg-zinc-800"
                                    }`}
                                >
                                    {m === "ssh" ? "SSH" : "S3"}
                                </button>
                            ))}
                        </div>
                    </div>
                    <button
                        type="button"
                        onClick={onClose}
                        className="w-6 h-6 rounded flex items-center justify-center text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                        title="닫기 (Esc)"
                    >
                        <span className="text-sm leading-none">×</span>
                    </button>
                </div>

                {mode === "s3" ? (
                    <S3Panel onOpenPaths={onOpenPaths} onClose={onClose} />
                ) : (
                    <>
                        {/* 호스트 입력 + 자동완성 */}
                        <div className="px-4 pt-3 relative">
                            <label
                                htmlFor="remote-host-input"
                                className="block text-[11px] font-medium text-zinc-500 dark:text-zinc-400 mb-1"
                            >
                                호스트 (~/.ssh/config)
                            </label>
                            <input
                                id="remote-host-input"
                                type="text"
                                value={hostInput}
                                onChange={(e) => setHostInput(e.target.value)}
                                onFocus={() => setHostFocused(true)}
                                onBlur={() =>
                                    // 후보 클릭이 blur보다 먼저 처리되도록 약간 지연
                                    setTimeout(() => setHostFocused(false), 150)
                                }
                                onKeyDown={(e) => {
                                    if (e.key === "Enter") {
                                        e.preventDefault();
                                        void connect(hostInput);
                                    } else if (e.key === "Escape") {
                                        e.preventDefault();
                                        e.stopPropagation();
                                        (e.target as HTMLInputElement).blur();
                                    }
                                }}
                                placeholder="호스트 별칭 또는 주소"
                                className="w-full h-8 px-2 text-sm font-mono bg-white dark:bg-zinc-950 border border-zinc-300 dark:border-zinc-700 rounded focus:outline-none focus:ring-1 focus:ring-blue-500 text-zinc-900 dark:text-zinc-100"
                                autoComplete="off"
                                spellCheck={false}
                            />
                            {hostSuggestions.length > 0 && (
                                <div className="absolute left-4 right-4 top-full mt-1 z-10 rounded-md border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-lg max-h-48 overflow-auto">
                                    {hostSuggestions.map((h) => (
                                        <button
                                            key={h.alias}
                                            type="button"
                                            // mousedown으로 blur 전에 선택되게 한다
                                            onMouseDown={(e) => {
                                                e.preventDefault();
                                                setHostInput(h.alias);
                                                setHostFocused(false);
                                                void connect(h.alias);
                                            }}
                                            className="w-full px-3 py-1.5 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800 cursor-pointer"
                                        >
                                            <span className="text-xs font-mono font-medium text-zinc-800 dark:text-zinc-200">
                                                {h.alias}
                                            </span>
                                            {(h.hostname !== null ||
                                                h.user !== null) && (
                                                <span className="ml-2 text-[11px] text-zinc-500 dark:text-zinc-400">
                                                    {[h.user, h.hostname]
                                                        .filter(Boolean)
                                                        .join("@")}
                                                </span>
                                            )}
                                        </button>
                                    ))}
                                </div>
                            )}
                        </div>

                        {/* 서버 측 가속(에이전트) — 호스트에 접속하면 표시 */}
                        {connectedHost !== null && (
                            <div className="px-4 pt-3">
                                <label className="flex items-center gap-2 cursor-pointer select-none">
                                    <input
                                        type="checkbox"
                                        checked={agentHosts.has(connectedHost)}
                                        onChange={(e) =>
                                            handleAgentToggle(e.target.checked)
                                        }
                                        className="accent-zinc-900 dark:accent-zinc-100"
                                    />
                                    <span className="text-xs font-medium text-zinc-700 dark:text-zinc-300">
                                        서버 측 가속(에이전트) 사용
                                    </span>
                                </label>
                                <p className="mt-1 text-[11px] leading-relaxed text-zinc-500 dark:text-zinc-400">
                                    켜면 원격 ~/.cache/riffle 에 Riffle 에이전트
                                    실행 파일(약 2MB, Linux x86_64/aarch64)을
                                    올리고 서버에서 인덱싱·검색·필터를
                                    실행합니다. 대용량 원격 로그가 훨씬
                                    빨라집니다. 원격의 다른 파일은 수정하지
                                    않습니다.
                                </p>
                            </div>
                        )}

                        {/* 경로 입력 + 이동/상위로 */}
                        <div className="px-4 pt-3 flex items-end gap-2">
                            <div className="flex-1">
                                <label
                                    htmlFor="remote-path-input"
                                    className="block text-[11px] font-medium text-zinc-500 dark:text-zinc-400 mb-1"
                                >
                                    경로
                                </label>
                                <input
                                    id="remote-path-input"
                                    type="text"
                                    value={dirInput}
                                    onChange={(e) =>
                                        setDirInput(e.target.value)
                                    }
                                    onKeyDown={(e) => {
                                        if (e.key === "Enter") {
                                            e.preventDefault();
                                            void navigateTo(dirInput);
                                        } else if (e.key === "Escape") {
                                            e.preventDefault();
                                            e.stopPropagation();
                                            (
                                                e.target as HTMLInputElement
                                            ).blur();
                                        }
                                    }}
                                    placeholder={
                                        connectedHost === null
                                            ? "호스트를 먼저 선택하세요"
                                            : "/원격/경로"
                                    }
                                    disabled={connectedHost === null}
                                    className="w-full h-8 px-2 text-sm font-mono bg-white dark:bg-zinc-950 border border-zinc-300 dark:border-zinc-700 rounded focus:outline-none focus:ring-1 focus:ring-blue-500 text-zinc-900 dark:text-zinc-100 disabled:opacity-50"
                                    autoComplete="off"
                                    spellCheck={false}
                                />
                            </div>
                            <button
                                type="button"
                                onClick={() => void navigateTo(dirInput)}
                                disabled={
                                    loading || hostInput.trim().length === 0
                                }
                                className="h-8 px-3 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 disabled:opacity-40 cursor-pointer"
                            >
                                이동
                            </button>
                            <button
                                type="button"
                                onClick={() => {
                                    if (dir !== null) {
                                        void loadDir(
                                            connectedHost ?? hostInput.trim(),
                                            parentDir(dir),
                                        );
                                    }
                                }}
                                disabled={loading || dir === null}
                                className="h-8 px-3 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 disabled:opacity-40 cursor-pointer"
                                title="상위 디렉터리로 이동"
                            >
                                상위로
                            </button>
                        </div>

                        {/* 최근 원격 위치 */}
                        {recentLocs.length > 0 && (
                            <div className="px-4 pt-2 flex flex-wrap gap-1.5">
                                {recentLocs.map((loc) => (
                                    <button
                                        key={`${loc.host}${loc.dir}`}
                                        type="button"
                                        onClick={() => {
                                            setHostInput(loc.host);
                                            setConnectedHost(loc.host);
                                            void loadDir(loc.host, loc.dir);
                                        }}
                                        className="px-2 py-0.5 rounded-full border border-zinc-200 dark:border-zinc-700 text-[11px] font-mono text-zinc-600 dark:text-zinc-400 hover:bg-zinc-100 dark:hover:bg-zinc-800 cursor-pointer truncate max-w-56"
                                        title={`${loc.host}:${loc.dir}`}
                                    >
                                        {loc.host}
                                        {loc.dir}
                                    </button>
                                ))}
                            </div>
                        )}

                        {/* 에러 표시 + 재시도 */}
                        {error !== null && (
                            <div className="mx-4 mt-3 px-3 py-2 rounded-md bg-rose-50 dark:bg-rose-950/50 border border-rose-200 dark:border-rose-900">
                                <div className="text-xs text-rose-700 dark:text-rose-300 whitespace-pre-wrap break-all">
                                    {error}
                                </div>
                                <button
                                    type="button"
                                    onClick={handleRetry}
                                    className="mt-1.5 px-2.5 py-1 text-[11px] font-medium rounded bg-rose-100 dark:bg-rose-900/60 text-rose-800 dark:text-rose-200 hover:bg-rose-200 dark:hover:bg-rose-800 cursor-pointer"
                                >
                                    다시 시도
                                </button>
                            </div>
                        )}

                        {/* 디렉터리 목록 */}
                        <div className="flex-1 min-h-0 mt-3 mx-4 mb-3 rounded-md border border-zinc-200 dark:border-zinc-800 overflow-hidden flex flex-col">
                            {/* 열 헤더 */}
                            <div className="shrink-0 grid grid-cols-[1fr_7rem_11rem] gap-2 px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wide text-zinc-400 dark:text-zinc-500 border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900">
                                <span>이름</span>
                                <span className="text-right">크기</span>
                                <span className="text-right">수정 시각</span>
                            </div>
                            {/* 목록 본문 */}
                            <div className="flex-1 min-h-0 overflow-auto">
                                {loading && entries.length === 0 ? (
                                    <div className="px-3 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
                                        불러오는 중…
                                    </div>
                                ) : entries.length === 0 && error === null ? (
                                    <div className="px-3 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
                                        {connectedHost === null
                                            ? "호스트를 선택하면 디렉터리를 표시합니다"
                                            : "빈 디렉터리입니다"}
                                    </div>
                                ) : (
                                    entries.map((entry) => {
                                        const isSel = selected.has(entry.name);
                                        return (
                                            <div
                                                key={entry.name}
                                                onClick={(e) =>
                                                    handleEntryClick(entry, e)
                                                }
                                                onDoubleClick={() => {
                                                    if (
                                                        entry.isDir &&
                                                        dir !== null
                                                    ) {
                                                        const host =
                                                            connectedHost ??
                                                            hostInput.trim();
                                                        if (host.length > 0) {
                                                            void loadDir(
                                                                host,
                                                                joinPath(
                                                                    dir,
                                                                    entry.name,
                                                                ),
                                                            );
                                                        }
                                                    } else if (!entry.isDir) {
                                                        openFiles([entry.name]);
                                                    }
                                                }}
                                                title={
                                                    entry.isDir
                                                        ? `${entry.name}/`
                                                        : entry.name
                                                }
                                                className={`grid grid-cols-[1fr_7rem_11rem] gap-2 px-3 items-center text-xs font-mono cursor-default select-none ${
                                                    entry.isDir
                                                        ? "text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                                        : isSel
                                                          ? "bg-blue-100 dark:bg-blue-900/50 text-zinc-900 dark:text-zinc-100"
                                                          : "text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                                }`}
                                                style={{ height: 26 }}
                                            >
                                                <span className="truncate flex items-center">
                                                    {entry.isDir ? (
                                                        <svg
                                                            className="w-3.5 h-3.5 mr-1.5 shrink-0 text-amber-500"
                                                            fill="currentColor"
                                                            viewBox="0 0 20 20"
                                                        >
                                                            <path d="M2 6a2 2 0 012-2h4l2 2h6a2 2 0 012 2v6a2 2 0 01-2 2H4a2 2 0 01-2-2V6z" />
                                                        </svg>
                                                    ) : (
                                                        <span className="w-3.5 mr-1.5 shrink-0" />
                                                    )}
                                                    <span className="truncate">
                                                        {entry.name}
                                                        {entry.isDir ? "/" : ""}
                                                    </span>
                                                </span>
                                                <span className="text-right text-zinc-500 dark:text-zinc-400 truncate">
                                                    {entry.isDir
                                                        ? "-"
                                                        : formatBytes(
                                                              entry.size,
                                                          )}
                                                </span>
                                                <span className="text-right text-zinc-500 dark:text-zinc-400 truncate">
                                                    {formatMtime(entry.mtime)}
                                                </span>
                                            </div>
                                        );
                                    })
                                )}
                            </div>
                            {/* 하단 로딩 표시 (목록 갱신 중) */}
                            {loading && entries.length > 0 && (
                                <div className="shrink-0 px-3 py-1 text-[10px] text-zinc-400 dark:text-zinc-500 border-t border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900">
                                    불러오는 중…
                                </div>
                            )}
                        </div>

                        {/* 푸터: 선택 정보 + 열기 */}
                        <div className="shrink-0 flex items-center justify-between px-4 py-2.5 border-t border-zinc-200 dark:border-zinc-800">
                            <span className="text-[11px] text-zinc-500 dark:text-zinc-400 truncate">
                                {selected.size > 0
                                    ? `${selected.size}개 파일 선택됨 (Shift/Cmd+클릭으로 다중 선택)`
                                    : "파일을 더블클릭하거나 선택 후 [열기]"}
                            </span>
                            <div className="flex items-center gap-2">
                                <button
                                    type="button"
                                    onClick={onClose}
                                    className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                                >
                                    취소
                                </button>
                                <button
                                    type="button"
                                    onClick={handleOpenSelected}
                                    disabled={!canOpen}
                                    className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 disabled:opacity-40 cursor-pointer"
                                >
                                    열기
                                    {selected.size > 1
                                        ? ` (${selected.size})`
                                        : ""}
                                </button>
                            </div>
                        </div>
                    </>
                )}
            </div>
        </div>
    );
};
