import React, {
    useCallback,
    useEffect,
    useMemo,
    useRef,
    useState,
} from "react";
import { cancelListing, type ListEvent } from "@/lib/ipc";
import { formatBytes, formatMtime } from "@/lib/format";

/** 고정 행 높이(px) — 가상 스크롤 기준 */
const ROW_HEIGHT = 26;
/** 보이는 구간 위아래로 추가로 그리는 행 수 */
const OVERSCAN = 8;
/** '시작 문자' 필터 입력 → 스트림 재시작 디바운스(ms) */
const FILTER_DEBOUNCE_MS = 300;
/** 페이지 도착 → 화면 반영 묶음 주기(ms) */
const FLUSH_MS = 100;
/** 항목이 이만큼 넘으면 반영 주기를 늘린다 (매 반영마다 전체 정렬을 다시 하므로) */
const LARGE_LIST = 20_000;
/** 큰 목록의 반영 주기(ms) */
const FLUSH_MS_LARGE = 500;
/** 정렬 상태 localStorage 키 */
const SORT_KEY = "riffle.remoteListSort";

/** 목록 항목의 공통 형태 (S3Entry / RemoteEntry 와 구조가 같다) */
export interface RemoteListEntry {
    name: string;
    isDir: boolean;
    size: number;
    /** 수정 시각 (unix 초), 없으면 null */
    mtime: number | null;
}

type SortCol = "name" | "size" | "mtime";
type SortDir = "asc" | "desc";

interface SortState {
    col: SortCol;
    dir: SortDir;
}

type FilterMode = "prefix" | "contains";

/** 스트림 진행 상태 (error 는 별도 error 문자열에 담긴다) */
type ListStatus = "loading" | "done" | "truncated" | "cancelled" | "error";

function toErrorMessage(err: unknown): string {
    if (typeof err === "string") return err;
    if (err instanceof Error) return err.message;
    return "알 수 없는 오류가 발생했습니다.";
}

function loadSort(): SortState {
    try {
        const raw = localStorage.getItem(SORT_KEY);
        if (raw !== null) {
            const s: unknown = JSON.parse(raw);
            if (
                typeof s === "object" &&
                s !== null &&
                ["name", "size", "mtime"].includes((s as SortState).col) &&
                ["asc", "desc"].includes((s as SortState).dir)
            ) {
                return s as SortState;
            }
        }
    } catch {
        // 읽기 실패는 기본값으로 무시한다
    }
    return { col: "name", dir: "asc" };
}

function saveSort(s: SortState): void {
    try {
        localStorage.setItem(SORT_KEY, JSON.stringify(s));
    } catch {
        // 저장 실패는 무시한다
    }
}

/**
 * 자연 정렬용 키: 소문자로 바꾸고 숫자 묶음을 20자리로 0 채움한다 (file2 < file10).
 * 20만 개를 Intl.Collator(numeric)로 매번 정렬하면 약 540ms 걸려 불러오는 중 화면이 멈추므로,
 * 키를 항목당 한 번만 만들고 단순 문자열 비교로 정렬한다 (같은 순서, 약 70ms).
 */
function naturalKey(name: string): string {
    return name
        .toLowerCase()
        .replace(/\d+/g, (d) => d.replace(/^0+/, "").padStart(20, "0"));
}

/** 항목 객체 → 자연 정렬 키 캐시. 항목이 버려지면 키도 함께 회수된다. */
const sortKeyCache = new WeakMap<RemoteListEntry, string>();

function sortKeyOf(e: RemoteListEntry): string {
    let k = sortKeyCache.get(e);
    if (k === undefined) {
        k = naturalKey(e.name);
        sortKeyCache.set(e, k);
    }
    return k;
}

export interface RemoteFileListProps<E extends RemoteListEntry> {
    /** 이 값이 바뀌면 필터를 비우고 목록을 새로 불러온다 (폴더 이동 신호) */
    location: string;
    /** 목록 스트림을 시작한다. namePrefix 는 '시작 문자' 필터. 반환: 취소용 listId */
    startStream: (
        namePrefix: string,
        onEvent: (e: ListEvent<E>) => void,
    ) => Promise<number>;
    /** 폴더 항목을 더블클릭해 들어간다 */
    onEnterDir: (name: string) => void;
    /** 파일을 연다 (더블클릭·열기 버튼 공용). entries 는 현재까지 받은 항목 전체 */
    onOpen: (names: string[], entries: E[]) => void;
    /** 대화상자 [취소] 버튼 */
    onCancel: () => void;
    /** 목록이 비었을 때의 안내 문구 */
    emptyText: string;
    /** 선택 안내 문구의 대상 이름 ("파일" / "객체") */
    itemName: string;
    /** 아무것도 선택하지 않았을 때 푸터에 보이는 안내 문구 */
    hintText: string;
    /**
     * 스트림이 끝날 때 한 번 호출된다.
     * ok=true 는 정상 종료(done/truncated), ok=false 는 오류. 취소는 호출하지 않는다.
     */
    onFinished?: (ok: boolean) => void;
}

/**
 * 원격 목록 공용 뷰 (S3 접두사 목록 / SFTP 디렉터리 목록).
 * 스트리밍으로 항목을 받아 가상 스크롤·정렬·'시작 문자'/'포함' 필터를 제공한다.
 * 항목이 많아도 보이는 행만 렌더링해 스크롤과 선택이 끊기지 않는다.
 */
export function RemoteFileList<E extends RemoteListEntry>({
    location,
    startStream,
    onEnterDir,
    onOpen,
    onCancel,
    emptyText,
    itemName,
    hintText,
    onFinished,
}: RemoteFileListProps<E>) {
    const [entries, setEntries] = useState<E[]>([]);
    const [status, setStatus] = useState<ListStatus>("loading");
    const [error, setError] = useState<string | null>(null);
    const [filterMode, setFilterMode] = useState<FilterMode>("prefix");
    const [filterText, setFilterText] = useState("");
    const [sort, setSort] = useState<SortState>(loadSort);
    const [selected, setSelected] = useState<Set<string>>(new Set());
    const anchorRef = useRef<string | null>(null);

    // 이벤트 콜백이 쓰는 최신 상태는 ref 에 둔다 (페이지마다 setState 하지 않기 위함)
    const entriesRef = useRef<E[]>([]);
    const genRef = useRef(0);
    const listIdRef = useRef<number | null>(null);
    const flushTimerRef = useRef<number | null>(null);
    const debounceRef = useRef<number | null>(null);
    /** 현재 스트림에 적용된 '시작 문자' 필터 */
    const activePrefixRef = useRef("");
    const startStreamRef = useRef(startStream);
    const onFinishedRef = useRef(onFinished);

    // 이벤트 콜백·지연 호출이 항상 최신 props를 쓰도록 커밋 후 ref를 갱신한다
    useEffect(() => {
        startStreamRef.current = startStream;
        onFinishedRef.current = onFinished;
    });

    // 가상 스크롤 상태
    const scrollRef = useRef<HTMLDivElement | null>(null);
    const [scrollTop, setScrollTop] = useState(0);
    const [viewHeight, setViewHeight] = useState(0);

    /** 버퍼에 쌓인 항목을 화면 상태로 반영한다 */
    const flush = useCallback(() => {
        if (flushTimerRef.current !== null) {
            clearTimeout(flushTimerRef.current);
            flushTimerRef.current = null;
        }
        setEntries(entriesRef.current);
    }, []);

    /** 페이지가 도착할 때마다가 아니라 FLUSH_MS 단위로 화면을 갱신한다 */
    const scheduleFlush = useCallback(() => {
        if (flushTimerRef.current === null) {
            const delay =
                entriesRef.current.length > LARGE_LIST
                    ? FLUSH_MS_LARGE
                    : FLUSH_MS;
            flushTimerRef.current = window.setTimeout(() => {
                flushTimerRef.current = null;
                setEntries(entriesRef.current);
            }, delay);
        }
    }, []);

    /** 진행 중 스트림을 취소하고, 세대를 올려 늦게 오는 이벤트를 무시하게 한다 */
    const stopStream = useCallback(() => {
        genRef.current += 1;
        if (flushTimerRef.current !== null) {
            clearTimeout(flushTimerRef.current);
            flushTimerRef.current = null;
        }
        const id = listIdRef.current;
        listIdRef.current = null;
        if (id !== null) void cancelListing(id);
    }, []);

    /** namePrefix(시작 문자 필터)로 새 스트림을 시작한다 */
    const start = useCallback(
        (namePrefix: string) => {
            if (debounceRef.current !== null) {
                clearTimeout(debounceRef.current);
                debounceRef.current = null;
            }
            stopStream();
            const gen = genRef.current;
            activePrefixRef.current = namePrefix;
            entriesRef.current = [];
            setEntries([]);
            setError(null);
            setSelected(new Set());
            anchorRef.current = null;
            setStatus("loading");
            setScrollTop(0);
            scrollRef.current?.scrollTo(0, 0);
            let finished = false;
            Promise.resolve()
                .then(() =>
                    startStreamRef.current(namePrefix, (ev) => {
                        if (gen !== genRef.current) return;
                        if (ev.kind === "page") {
                            // 정렬 키는 도착할 때 한 번만 만든다
                            for (const e of ev.entries) sortKeyOf(e);
                            entriesRef.current = entriesRef.current.concat(
                                ev.entries,
                            );
                            scheduleFlush();
                            return;
                        }
                        finished = true;
                        listIdRef.current = null;
                        flush();
                        if (ev.kind === "error") {
                            setStatus("error");
                            setError(ev.message);
                            onFinishedRef.current?.(false);
                        } else {
                            setStatus(
                                ev.cancelled
                                    ? "cancelled"
                                    : ev.truncated
                                      ? "truncated"
                                      : "done",
                            );
                            if (!ev.cancelled) onFinishedRef.current?.(true);
                        }
                    }),
                )
                .then((id) => {
                    if (gen !== genRef.current) return;
                    // 이벤트가 invoke 반환보다 먼저 끝까지 도착한 경우는 무시한다
                    if (!finished) listIdRef.current = id;
                })
                .catch((err: unknown) => {
                    if (gen !== genRef.current) return;
                    setStatus("error");
                    setError(toErrorMessage(err));
                    onFinishedRef.current?.(false);
                });
        },
        [flush, scheduleFlush, stopStream],
    );

    /** [중지]: 진행 중 스트림을 취소하고 지금까지 받은 항목을 그대로 보여준다 */
    const handleStop = useCallback(() => {
        // 세대를 올려 취소 후 도착하는 늦은 페이지·done 이벤트를 무시한다
        stopStream();
        setEntries(entriesRef.current);
        setStatus("cancelled");
    }, [stopStream]);

    // 위치가 바뀌면 필터 입력을 비운다 (렌더 중 상태 조정 패턴)
    const [prevLocation, setPrevLocation] = useState(location);
    if (prevLocation !== location) {
        setPrevLocation(location);
        setFilterText("");
    }

    // 위치가 바뀌면 목록을 처음부터 다시 불러온다
    // (스트림 시작은 비동기 작업이므로 다음 틱에 실행한다)
    useEffect(() => {
        const t = window.setTimeout(() => start(""), 0);
        return () => clearTimeout(t);
    }, [location, start]);

    // 언마운트 시 진행 중 스트림 취소 + 타이머 정리
    useEffect(
        () => () => {
            const id = listIdRef.current;
            if (id !== null) void cancelListing(id);
            if (flushTimerRef.current !== null) {
                clearTimeout(flushTimerRef.current);
            }
            if (debounceRef.current !== null) {
                clearTimeout(debounceRef.current);
            }
        },
        [],
    );

    // 필터 입력 반영. '시작 문자' 모드는 서버 측 필터라 디바운스 후 스트림을 재시작한다.
    // '포함' 모드는 불러온 항목을 즉시 걸러내므로 스트림은 그대로 둔다.
    const handleFilterText = (text: string) => {
        setFilterText(text);
        if (filterMode !== "prefix") return;
        if (debounceRef.current !== null) {
            clearTimeout(debounceRef.current);
        }
        debounceRef.current = window.setTimeout(() => {
            debounceRef.current = null;
            start(text);
        }, FILTER_DEBOUNCE_MS);
    };

    /** 필터 모드 전환. 접두사 필터가 걸린 목록에선 '포함' 적용을 위해 전체를 다시 불러온다 */
    const handleModeChange = (mode: FilterMode) => {
        if (mode === filterMode) return;
        setFilterMode(mode);
        if (mode === "prefix") {
            start(filterText);
        } else if (activePrefixRef.current !== "") {
            start("");
        }
    };

    /** 현재 필터·정렬이 적용된 표시용 항목 */
    const visible = useMemo(() => {
        let list = entries;
        if (filterMode === "contains" && filterText.length > 0) {
            const q = filterText.toLowerCase();
            list = list.filter((e) => e.name.toLowerCase().includes(q));
        }
        const { col, dir } = sort;
        const sign = dir === "asc" ? 1 : -1;
        const nameCmp = (a: E, b: E): number => {
            const ka = sortKeyOf(a);
            const kb = sortKeyOf(b);
            return ka < kb ? -1 : ka > kb ? 1 : 0;
        };
        const cmp = (a: E, b: E): number => {
            // 폴더는 항상 파일보다 위
            if (a.isDir !== b.isDir) return a.isDir ? -1 : 1;
            let r: number;
            switch (col) {
                case "size":
                    r = a.size - b.size;
                    break;
                case "mtime":
                    r = (a.mtime ?? 0) - (b.mtime ?? 0);
                    break;
                default:
                    r = nameCmp(a, b);
            }
            // 같은 값이면 이름순으로 정렬을 안정시킨다
            if (r === 0 && col !== "name") {
                r = nameCmp(a, b);
            }
            return r * sign;
        };
        return [...list].sort(cmp);
    }, [entries, filterMode, filterText, sort]);

    const fileEntries = useMemo(
        () => visible.filter((e) => !e.isDir),
        [visible],
    );

    /** 열 헤더 클릭: 같은 열이면 방향 전환, 다른 열이면 오름차순으로 */
    const toggleSort = useCallback((col: SortCol) => {
        setSort((prev) => {
            const next: SortState =
                prev.col === col
                    ? { col, dir: prev.dir === "asc" ? "desc" : "asc" }
                    : { col, dir: "asc" };
            saveSort(next);
            return next;
        });
    }, []);

    // 항목 클릭: 일반=단일 선택, Cmd/Ctrl=토글 추가, Shift=범위 선택
    const handleEntryClick = useCallback(
        (entry: E, e: React.MouseEvent) => {
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

    // 뷰포트 크기 추적
    useEffect(() => {
        const el = scrollRef.current;
        if (el === null) return;
        const ro = new ResizeObserver(() => {
            setViewHeight(el.clientHeight);
        });
        ro.observe(el);
        setViewHeight(el.clientHeight);
        return () => ro.disconnect();
    }, []);

    const startIdx = Math.max(0, Math.floor(scrollTop / ROW_HEIGHT) - OVERSCAN);
    const endIdx = Math.min(
        visible.length,
        Math.ceil((scrollTop + viewHeight) / ROW_HEIGHT) + OVERSCAN,
    );
    const rows = visible.slice(startIdx, endIdx);

    const sortMark = (col: SortCol): string =>
        sort.col === col ? (sort.dir === "asc" ? " ▲" : " ▼") : "";

    const clearFilter = () => {
        handleFilterText("");
    };

    return (
        <>
            {/* 필터 바: '시작 문자'는 스트림 재시작, '포함'은 불러온 항목에서 즉시 필터 */}
            <div className="px-4 pt-3 flex items-center gap-2">
                <div className="flex rounded-md border border-zinc-300 dark:border-zinc-700 overflow-hidden text-xs font-medium shrink-0">
                    {(
                        [
                            ["prefix", "시작 문자"],
                            ["contains", "포함"],
                        ] as const
                    ).map(([mode, label]) => (
                        <button
                            key={mode}
                            type="button"
                            onClick={() => handleModeChange(mode)}
                            className={`px-2.5 py-1 cursor-pointer ${
                                filterMode === mode
                                    ? "bg-zinc-900 text-zinc-50 dark:bg-zinc-100 dark:text-zinc-900"
                                    : "bg-white text-zinc-600 hover:bg-zinc-100 dark:bg-zinc-900 dark:text-zinc-400 dark:hover:bg-zinc-800"
                            }`}
                        >
                            {label}
                        </button>
                    ))}
                </div>
                <div className="relative flex-1 min-w-0">
                    <input
                        type="text"
                        value={filterText}
                        onChange={(e) => handleFilterText(e.target.value)}
                        onKeyDown={(e) => {
                            if (e.key === "Escape") {
                                e.preventDefault();
                                e.stopPropagation();
                                if (filterText.length > 0) {
                                    handleFilterText("");
                                } else {
                                    (e.target as HTMLInputElement).blur();
                                }
                            }
                        }}
                        placeholder="파일 이름으로 찾기"
                        className="w-full h-8 px-2 pr-7 text-sm font-mono bg-white dark:bg-zinc-950 border border-zinc-300 dark:border-zinc-700 rounded focus:outline-none focus:ring-1 focus:ring-blue-500 text-zinc-900 dark:text-zinc-100"
                        autoComplete="off"
                        spellCheck={false}
                    />
                    {filterText.length > 0 && (
                        <button
                            type="button"
                            onClick={clearFilter}
                            className="absolute right-1.5 top-1/2 -translate-y-1/2 w-5 h-5 rounded flex items-center justify-center text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                            title="필터 지우기"
                        >
                            <span className="text-sm leading-none">×</span>
                        </button>
                    )}
                </div>
            </div>

            {/* 에러 표시 + 재시도 */}
            {error !== null && (
                <div className="mx-4 mt-3 px-3 py-2 rounded-md bg-rose-50 dark:bg-rose-950/50 border border-rose-200 dark:border-rose-900">
                    <div className="text-xs text-rose-700 dark:text-rose-300 whitespace-pre-wrap break-all">
                        {error}
                    </div>
                    <button
                        type="button"
                        onClick={() => start(activePrefixRef.current)}
                        className="mt-1.5 px-2.5 py-1 text-[11px] font-medium rounded bg-rose-100 dark:bg-rose-900/60 text-rose-800 dark:text-rose-200 hover:bg-rose-200 dark:hover:bg-rose-800 cursor-pointer"
                    >
                        다시 시도
                    </button>
                </div>
            )}

            {/* 목록 영역 */}
            <div className="flex-1 min-h-0 mt-3 mx-4 mb-3 rounded-md border border-zinc-200 dark:border-zinc-800 overflow-hidden flex flex-col">
                {/* 열 헤더 (클릭 정렬) */}
                <div className="shrink-0 grid grid-cols-[1fr_7rem_11rem] gap-2 px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wide text-zinc-400 dark:text-zinc-500 border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900">
                    <button
                        type="button"
                        onClick={() => toggleSort("name")}
                        className="text-left cursor-pointer hover:text-zinc-600 dark:hover:text-zinc-300"
                    >
                        이름{sortMark("name")}
                    </button>
                    <button
                        type="button"
                        onClick={() => toggleSort("size")}
                        className="text-right cursor-pointer hover:text-zinc-600 dark:hover:text-zinc-300"
                    >
                        크기{sortMark("size")}
                    </button>
                    <button
                        type="button"
                        onClick={() => toggleSort("mtime")}
                        className="text-right cursor-pointer hover:text-zinc-600 dark:hover:text-zinc-300"
                    >
                        수정 시각{sortMark("mtime")}
                    </button>
                </div>
                {/* 가상 스크롤 본문 */}
                <div
                    ref={scrollRef}
                    onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}
                    className="flex-1 min-h-0 overflow-auto"
                >
                    {status === "loading" && entries.length === 0 ? (
                        <div className="px-3 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
                            불러오는 중…
                        </div>
                    ) : visible.length === 0 ? (
                        <div className="px-3 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
                            {filterMode === "contains" &&
                            filterText.length > 0 &&
                            entries.length > 0
                                ? "일치하는 항목이 없습니다"
                                : emptyText}
                        </div>
                    ) : (
                        <div
                            style={{
                                height: visible.length * ROW_HEIGHT,
                                position: "relative",
                            }}
                        >
                            {rows.map((entry, i) => {
                                const isSel = selected.has(entry.name);
                                return (
                                    <div
                                        key={
                                            entry.isDir
                                                ? `d:${entry.name}`
                                                : `f:${entry.name}`
                                        }
                                        onClick={(e) =>
                                            handleEntryClick(entry, e)
                                        }
                                        onDoubleClick={() => {
                                            if (entry.isDir) {
                                                onEnterDir(entry.name);
                                            } else {
                                                onOpen(
                                                    [entry.name],
                                                    entriesRef.current,
                                                );
                                            }
                                        }}
                                        title={
                                            entry.isDir
                                                ? `${entry.name}/`
                                                : entry.name
                                        }
                                        className={`absolute left-0 right-0 grid grid-cols-[1fr_7rem_11rem] gap-2 px-3 items-center text-xs font-mono cursor-default select-none ${
                                            entry.isDir
                                                ? "text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                                : isSel
                                                  ? "bg-blue-100 dark:bg-blue-900/50 text-zinc-900 dark:text-zinc-100"
                                                  : "text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                        }`}
                                        style={{
                                            top: (startIdx + i) * ROW_HEIGHT,
                                            height: ROW_HEIGHT,
                                        }}
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
                                                : formatBytes(entry.size)}
                                        </span>
                                        <span className="text-right text-zinc-500 dark:text-zinc-400 truncate">
                                            {formatMtime(entry.mtime)}
                                        </span>
                                    </div>
                                );
                            })}
                        </div>
                    )}
                </div>
                {/* 스트리밍 상태 줄 */}
                <div
                    className={`shrink-0 flex items-center gap-2 px-3 py-1 text-[10px] border-t border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900 ${
                        status === "truncated"
                            ? "text-amber-600 dark:text-amber-400"
                            : "text-zinc-400 dark:text-zinc-500"
                    }`}
                >
                    {status === "loading" && (
                        <>
                            <span>
                                불러오는 중…{" "}
                                {entries.length.toLocaleString("ko-KR")}개
                            </span>
                            <button
                                type="button"
                                onClick={handleStop}
                                className="px-1.5 rounded text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                            >
                                중지
                            </button>
                        </>
                    )}
                    {status === "done" && (
                        <span>
                            {filterMode === "contains" && filterText.length > 0
                                ? `${entries.length.toLocaleString("ko-KR")}개 중 ${visible.length.toLocaleString("ko-KR")}개 일치`
                                : `전체 ${entries.length.toLocaleString("ko-KR")}개`}
                        </span>
                    )}
                    {status === "truncated" && (
                        <span>
                            {entries.length.toLocaleString("ko-KR")}
                            개까지만 불러왔습니다 — '시작 문자' 필터로 좁히세요
                        </span>
                    )}
                    {status === "cancelled" && (
                        <span>
                            중지됨 — {entries.length.toLocaleString("ko-KR")}
                            개까지 표시
                        </span>
                    )}
                    {status === "error" && (
                        <span>
                            {entries.length > 0
                                ? `오류 발생 — ${entries.length.toLocaleString("ko-KR")}개까지 표시`
                                : "목록을 불러오지 못했습니다"}
                        </span>
                    )}
                </div>
            </div>

            {/* 푸터: 선택 정보 + 열기 */}
            <div className="shrink-0 flex items-center justify-between px-4 py-2.5 border-t border-zinc-200 dark:border-zinc-800">
                <span className="text-[11px] text-zinc-500 dark:text-zinc-400 truncate">
                    {selected.size > 0
                        ? `${selected.size}개 ${itemName} 선택됨 (Shift/Cmd+클릭으로 다중 선택)`
                        : hintText}
                </span>
                <div className="flex items-center gap-2">
                    <button
                        type="button"
                        onClick={onCancel}
                        className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                    >
                        취소
                    </button>
                    <button
                        type="button"
                        onClick={() =>
                            onOpen([...selected], entriesRef.current)
                        }
                        disabled={selected.size === 0}
                        className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 disabled:opacity-40 cursor-pointer"
                    >
                        열기
                        {selected.size > 1 ? ` (${selected.size})` : ""}
                    </button>
                </div>
            </div>
        </>
    );
}
