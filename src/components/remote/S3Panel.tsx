import React, {
    useCallback,
    useEffect,
    useMemo,
    useRef,
    useState,
} from "react";
import {
    getS3Bookmarks,
    getSettings,
    listAwsProfiles,
    s3ListBuckets,
    s3ListPrefix,
    setS3Bookmarks,
    setS3Profile,
    type S3Entry,
} from "@/lib/ipc";
import { s3UriOf } from "@/lib/remote";
import { formatBytes, formatMtime } from "@/lib/format";

/** 이 크기 이상의 객체는 열기 전 데이터 전송 안내를 표시한다 (1 GiB) */
const LARGE_OBJECT_BYTES = 1024 * 1024 * 1024;
/** "다시 묻지 않기" localStorage 키 */
const SKIP_CONFIRM_KEY = "riffle.s3SkipLargeConfirm";
/** 접두사 목록 잘림 안내 기준 (백엔드 상한과 동일) */
const MAX_PREFIX_ENTRIES = 10_000;

interface S3PanelProps {
    /** 선택한 S3 객체들을 s3:// URI 목록으로 넘겨 탭으로 연다 */
    onOpenPaths: (paths: string[]) => void;
    onClose: () => void;
}

function toErrorMessage(err: unknown): string {
    if (typeof err === "string") return err;
    if (err instanceof Error) return err.message;
    return "알 수 없는 오류가 발생했습니다.";
}

function skipLargeConfirm(): boolean {
    try {
        return localStorage.getItem(SKIP_CONFIRM_KEY) === "1";
    } catch {
        return false;
    }
}

/** "bucket/prefix/" 형태의 위치 문자열을 { bucket, prefix }로 분해한다 */
function parseLocation(item: string): { bucket: string; prefix: string } {
    const segs = item.split("/").filter((s) => s.length > 0);
    return {
        bucket: segs[0] ?? "",
        prefix: segs
            .slice(1)
            .map((s) => `${s}/`)
            .join(""),
    };
}

/**
 * S3 객체 탐색 패널 (원격 파일 열기 대화상자의 S3 탭).
 * AWS 기본 자격 증명 체인(또는 선택한 프로필)으로 버킷과 접두사를 탐색해
 * s3:// URI로 객체를 연다. 읽기 전용 API만 사용한다.
 */
export const S3Panel: React.FC<S3PanelProps> = ({ onOpenPaths, onClose }) => {
    const [profiles, setProfiles] = useState<string[]>([]);
    // null = 기본 자격 증명 체인
    const [profile, setProfile] = useState<string | null>(null);
    const [bookmarks, setBookmarks] = useState<string[]>([]);

    const [buckets, setBuckets] = useState<string[]>([]);
    const [bucketsError, setBucketsError] = useState<string | null>(null);
    const [bucketInput, setBucketInput] = useState("");

    const [bucket, setBucket] = useState<string | null>(null);
    const [prefix, setPrefix] = useState("");
    const [entries, setEntries] = useState<S3Entry[]>([]);
    const [loading, setLoading] = useState(false);
    const [error, setError] = useState<string | null>(null);

    const [selected, setSelected] = useState<Set<string>>(new Set());
    const anchorRef = useRef<string | null>(null);

    // 1GB 이상 객체 열기 확인 창 상태
    const [confirm, setConfirm] = useState<{
        uris: string[];
        count: number;
        totalBytes: number;
    } | null>(null);
    const [dontAskAgain, setDontAskAgain] = useState(false);

    const objectEntries = useMemo(
        () => entries.filter((e) => !e.isDir),
        [entries],
    );

    // 버킷 목록을 불러온다. 실패(권한 없음 등) 시 수동 입력으로 진행할 수 있다.
    const loadBuckets = useCallback(async (p: string | null) => {
        setLoading(true);
        setError(null);
        setBucketsError(null);
        try {
            const list = await s3ListBuckets(p);
            setBuckets(list);
        } catch (err) {
            setBuckets([]);
            setBucketsError(toErrorMessage(err));
        } finally {
            setLoading(false);
        }
    }, []);

    // 접두사 목록을 불러오고 현재 위치를 갱신한다.
    const loadPrefix = useCallback(
        async (bkt: string, pfx: string) => {
            setLoading(true);
            setError(null);
            try {
                const list = await s3ListPrefix(profile, bkt, pfx);
                setBucket(bkt);
                setPrefix(pfx);
                setEntries(list);
                setSelected(new Set());
                anchorRef.current = null;
            } catch (err) {
                setError(toErrorMessage(err));
            } finally {
                setLoading(false);
            }
        },
        [profile],
    );

    // 초기화: 저장된 프로필·프로필 목록·즐겨찾기를 읽고 버킷 목록을 요청한다
    useEffect(() => {
        let cancelled = false;
        (async () => {
            let savedProfile: string | null = null;
            try {
                const [settings, profs, bms] = await Promise.all([
                    getSettings(),
                    listAwsProfiles(),
                    getS3Bookmarks(),
                ]);
                if (cancelled) return;
                setProfiles(profs);
                setBookmarks(bms);
                savedProfile = settings.s3Profile;
                setProfile(savedProfile);
            } catch (err) {
                console.warn("S3 설정 초기화 실패:", err);
            }
            if (!cancelled) {
                void loadBuckets(savedProfile);
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [loadBuckets]);

    // 프로필 변경: 설정에 저장하고 탐색 상태를 루트로 되돌린 뒤 버킷을 다시 읽는다
    const handleProfileChange = useCallback(
        (value: string) => {
            const next = value === "" ? null : value;
            setProfile(next);
            setS3Profile(next).catch((err) => {
                console.warn("S3 프로필 저장 실패:", err);
            });
            setBucket(null);
            setPrefix("");
            setEntries([]);
            setSelected(new Set());
            void loadBuckets(next);
        },
        [loadBuckets],
    );

    // "bucket" 또는 "bucket/prefix/" 형태의 위치로 이동한다
    const enterLocation = useCallback(
        (item: string) => {
            const loc = parseLocation(item);
            if (loc.bucket.length === 0) return;
            void loadPrefix(loc.bucket, loc.prefix);
        },
        [loadPrefix],
    );

    // 버킷 직접 입력 처리 (s3:// 접두사·슬래시는 관대하게 허용)
    const handleEnterBucketInput = useCallback(() => {
        const raw = bucketInput.trim().replace(/^s3:\/\//, "");
        enterLocation(raw);
    }, [bucketInput, enterLocation]);

    // 상위 위치로 이동: 접두사 안이면 한 단계 위, 접두사 루트면 버킷 목록으로
    const goUp = useCallback(() => {
        if (bucket === null) return;
        if (prefix.length === 0) {
            setBucket(null);
            setEntries([]);
            setSelected(new Set());
            setError(null);
            return;
        }
        const trimmed = prefix.replace(/\/+$/, "");
        const idx = trimmed.lastIndexOf("/");
        const parent = idx < 0 ? "" : `${trimmed.slice(0, idx + 1)}`;
        void loadPrefix(bucket, parent);
    }, [bucket, prefix, loadPrefix]);

    // 브레드크럼 세그먼트 이동: null이면 버킷 목록, ""이면 버킷 루트, 그 외 접두사
    const navigateTo = useCallback(
        (pfx: string | null) => {
            if (pfx === null) {
                if (bucket !== null) {
                    setBucket(null);
                    setEntries([]);
                    setSelected(new Set());
                    setError(null);
                }
                return;
            }
            if (bucket !== null) {
                void loadPrefix(bucket, pfx);
            }
        },
        [bucket, loadPrefix],
    );

    // 현재 위치의 즐겨찾기 항목 ("bucket" 또는 "bucket/prefix/")
    const currentBookmark = useMemo(() => {
        if (bucket === null) return null;
        return prefix.length === 0 ? bucket : `${bucket}/${prefix}`;
    }, [bucket, prefix]);

    const isBookmarked =
        currentBookmark !== null && bookmarks.includes(currentBookmark);

    // 즐겨찾기 토글: 목록을 갱신하고 백엔드(s3-bookmarks.json)에 저장한다
    const toggleBookmark = useCallback((item: string) => {
        setBookmarks((prev) => {
            const next = prev.includes(item)
                ? prev.filter((b) => b !== item)
                : [...prev, item].sort();
            setS3Bookmarks(next).catch((err) => {
                console.warn("S3 즐겨찾기 저장 실패:", err);
            });
            return next;
        });
    }, []);

    // 객체 항목 클릭: 일반=단일 선택, Cmd/Ctrl=토글 추가, Shift=범위 선택
    const handleEntryClick = useCallback(
        (entry: S3Entry, e: React.MouseEvent) => {
            if (entry.isDir) return;
            const name = entry.name;
            if (e.shiftKey && anchorRef.current !== null) {
                const names = objectEntries.map((f) => f.name);
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
        [objectEntries],
    );

    // 객체 열기. 1GB 이상 객체가 포함되고 "다시 묻지 않기"가 꺼져 있으면
    // 데이터 전송 요금 확인 창을 먼저 표시한다.
    const openObjects = useCallback(
        (names: string[]) => {
            if (bucket === null || names.length === 0) return;
            const uris = names.map((n) => s3UriOf(bucket, `${prefix}${n}`));
            const objs = entries.filter(
                (e) => !e.isDir && names.includes(e.name),
            );
            const hasLarge = objs.some((e) => e.size >= LARGE_OBJECT_BYTES);
            if (hasLarge && !skipLargeConfirm()) {
                setConfirm({
                    uris,
                    count: names.length,
                    totalBytes: objs.reduce((acc, e) => acc + e.size, 0),
                });
                return;
            }
            onOpenPaths(uris);
            onClose();
        },
        [bucket, prefix, entries, onOpenPaths, onClose],
    );

    const handleOpenSelected = useCallback(() => {
        openObjects([...selected]);
    }, [openObjects, selected]);

    // 확인 창에서 "열기" 확정
    const handleConfirmOpen = useCallback(() => {
        if (confirm === null) return;
        if (dontAskAgain) {
            try {
                localStorage.setItem(SKIP_CONFIRM_KEY, "1");
            } catch {
                // 저장 실패는 무시한다
            }
        }
        onOpenPaths(confirm.uris);
        setConfirm(null);
        onClose();
    }, [confirm, dontAskAgain, onOpenPaths, onClose]);

    // 에러 시 재시도: 버킷 안이면 현재 접두사, 버킷 목록이면 목록 재조회
    const handleRetry = useCallback(() => {
        if (bucket !== null) {
            void loadPrefix(bucket, prefix);
        } else {
            void loadBuckets(profile);
        }
    }, [bucket, prefix, loadPrefix, loadBuckets, profile]);

    const canOpen = selected.size > 0 && !loading;
    const prefixSegments = useMemo(
        () => prefix.split("/").filter((s) => s.length > 0),
        [prefix],
    );

    return (
        <>
            {/* AWS 프로필 선택 */}
            <div className="px-4 pt-3">
                <label
                    htmlFor="s3-profile-select"
                    className="block text-[11px] font-medium text-zinc-500 dark:text-zinc-400 mb-1"
                >
                    AWS 프로필 (~/.aws/config)
                </label>
                <select
                    id="s3-profile-select"
                    value={profile ?? ""}
                    onChange={(e) => handleProfileChange(e.target.value)}
                    className="w-full h-8 px-2 text-sm font-mono bg-white dark:bg-zinc-950 border border-zinc-300 dark:border-zinc-700 rounded focus:outline-none focus:ring-1 focus:ring-blue-500 text-zinc-900 dark:text-zinc-100"
                >
                    <option value="">기본 자격 증명 체인</option>
                    {profiles.map((p) => (
                        <option key={p} value={p}>
                            {p}
                        </option>
                    ))}
                </select>
            </div>

            {/* 위치 바: 브레드크럼 + 즐겨찾기 토글 + 상위로 */}
            <div className="px-4 pt-3 flex items-end gap-2">
                <div className="flex-1 min-w-0">
                    <span className="block text-[11px] font-medium text-zinc-500 dark:text-zinc-400 mb-1">
                        위치
                    </span>
                    <div className="h-8 px-2 flex items-center gap-1 text-sm font-mono border border-zinc-300 dark:border-zinc-700 rounded bg-zinc-50 dark:bg-zinc-950 overflow-x-auto whitespace-nowrap">
                        <button
                            type="button"
                            onClick={() => navigateTo(null)}
                            className="text-zinc-400 dark:text-zinc-500 hover:text-blue-600 dark:hover:text-blue-400 cursor-pointer shrink-0"
                            title="버킷 목록으로"
                        >
                            s3://
                        </button>
                        {bucket === null ? (
                            <span className="text-zinc-400 dark:text-zinc-500 text-xs">
                                버킷을 선택하세요
                            </span>
                        ) : (
                            <>
                                <button
                                    type="button"
                                    onClick={() => navigateTo("")}
                                    className="text-blue-600 dark:text-blue-400 hover:underline cursor-pointer shrink-0"
                                >
                                    {bucket}
                                </button>
                                {prefixSegments.map((seg, i) => {
                                    const target = `${prefixSegments
                                        .slice(0, i + 1)
                                        .join("/")}/`;
                                    const isLast =
                                        i === prefixSegments.length - 1;
                                    return (
                                        <React.Fragment key={target}>
                                            <span className="text-zinc-400 dark:text-zinc-500 shrink-0">
                                                /
                                            </span>
                                            <button
                                                type="button"
                                                onClick={() =>
                                                    navigateTo(target)
                                                }
                                                className={`shrink-0 cursor-pointer ${
                                                    isLast
                                                        ? "text-zinc-800 dark:text-zinc-200"
                                                        : "text-blue-600 dark:text-blue-400 hover:underline"
                                                }`}
                                            >
                                                {seg}
                                            </button>
                                        </React.Fragment>
                                    );
                                })}
                            </>
                        )}
                    </div>
                </div>
                {currentBookmark !== null && (
                    <button
                        type="button"
                        onClick={() => toggleBookmark(currentBookmark)}
                        className={`h-8 w-8 shrink-0 rounded-md text-sm flex items-center justify-center cursor-pointer ${
                            isBookmarked
                                ? "text-amber-500 hover:text-amber-400"
                                : "text-zinc-400 hover:text-amber-500"
                        }`}
                        title={
                            isBookmarked
                                ? "즐겨찾기에서 제거"
                                : "현재 위치를 즐겨찾기에 추가"
                        }
                    >
                        {isBookmarked ? "★" : "☆"}
                    </button>
                )}
                <button
                    type="button"
                    onClick={goUp}
                    disabled={loading || bucket === null}
                    className="h-8 px-3 shrink-0 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 disabled:opacity-40 cursor-pointer"
                    title="상위 위치로 이동"
                >
                    상위로
                </button>
            </div>

            {/* 버킷 목록 권한 없음: 직접 입력 안내 */}
            {bucket === null && bucketsError !== null && (
                <div className="mx-4 mt-3 px-3 py-2 rounded-md bg-amber-50 dark:bg-amber-950/50 border border-amber-200 dark:border-amber-900">
                    <div className="text-xs text-amber-800 dark:text-amber-200">
                        버킷 목록 권한이 없습니다 — 버킷 이름을 직접 입력하세요
                    </div>
                    <div className="mt-0.5 text-[11px] text-amber-600 dark:text-amber-400 whitespace-pre-wrap break-all">
                        {bucketsError}
                    </div>
                    <div className="mt-1.5 flex items-center gap-2">
                        <input
                            type="text"
                            value={bucketInput}
                            onChange={(e) => setBucketInput(e.target.value)}
                            onKeyDown={(e) => {
                                if (e.key === "Enter") {
                                    e.preventDefault();
                                    handleEnterBucketInput();
                                } else if (e.key === "Escape") {
                                    e.preventDefault();
                                    e.stopPropagation();
                                    (e.target as HTMLInputElement).blur();
                                }
                            }}
                            placeholder="bucket 또는 bucket/prefix/"
                            className="flex-1 h-7 px-2 text-xs font-mono bg-white dark:bg-zinc-950 border border-amber-300 dark:border-amber-800 rounded focus:outline-none focus:ring-1 focus:ring-amber-500 text-zinc-900 dark:text-zinc-100"
                            autoComplete="off"
                            spellCheck={false}
                        />
                        <button
                            type="button"
                            onClick={handleEnterBucketInput}
                            disabled={bucketInput.trim().length === 0}
                            className="h-7 px-2.5 text-[11px] font-medium rounded bg-amber-100 dark:bg-amber-900/60 text-amber-800 dark:text-amber-200 hover:bg-amber-200 dark:hover:bg-amber-800 disabled:opacity-40 cursor-pointer"
                        >
                            이동
                        </button>
                        <button
                            type="button"
                            onClick={handleRetry}
                            className="h-7 px-2.5 text-[11px] font-medium rounded bg-amber-100 dark:bg-amber-900/60 text-amber-800 dark:text-amber-200 hover:bg-amber-200 dark:hover:bg-amber-800 cursor-pointer"
                        >
                            다시 시도
                        </button>
                    </div>
                </div>
            )}

            {/* 일반 에러 표시 + 재시도 */}
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

            {/* 목록 영역: 버킷 목록 또는 접두사 항목 */}
            <div className="flex-1 min-h-0 mt-3 mx-4 mb-3 rounded-md border border-zinc-200 dark:border-zinc-800 overflow-hidden flex flex-col">
                <div className="shrink-0 grid grid-cols-[1fr_7rem_11rem] gap-2 px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wide text-zinc-400 dark:text-zinc-500 border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900">
                    <span>이름</span>
                    <span className="text-right">크기</span>
                    <span className="text-right">수정 시각</span>
                </div>
                <div className="flex-1 min-h-0 overflow-auto">
                    {loading && entries.length === 0 && buckets.length === 0 ? (
                        <div className="px-3 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
                            불러오는 중…
                        </div>
                    ) : bucket === null ? (
                        <>
                            {/* 즐겨찾기 (목록 상단) */}
                            {bookmarks.map((item) => (
                                <div
                                    key={`bm-${item}`}
                                    onDoubleClick={() => enterLocation(item)}
                                    onClick={() => enterLocation(item)}
                                    title={`s3://${item}`}
                                    className="grid grid-cols-[1fr_7rem_11rem] gap-2 px-3 items-center text-xs font-mono cursor-default select-none text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                    style={{ height: 26 }}
                                >
                                    <span className="truncate flex items-center">
                                        <button
                                            type="button"
                                            onClick={(e) => {
                                                e.stopPropagation();
                                                toggleBookmark(item);
                                            }}
                                            className="mr-1.5 shrink-0 text-amber-500 hover:text-amber-400 cursor-pointer"
                                            title="즐겨찾기에서 제거"
                                        >
                                            ★
                                        </button>
                                        <span className="truncate">{item}</span>
                                    </span>
                                    <span className="text-right text-zinc-500 dark:text-zinc-400 truncate">
                                        -
                                    </span>
                                    <span className="text-right text-zinc-500 dark:text-zinc-400 truncate">
                                        -
                                    </span>
                                </div>
                            ))}
                            {/* 버킷 목록 */}
                            {buckets
                                .filter((b) => !bookmarks.includes(b))
                                .map((b) => (
                                    <div
                                        key={`bucket-${b}`}
                                        onDoubleClick={() => enterLocation(b)}
                                        onClick={() => enterLocation(b)}
                                        title={`s3://${b}`}
                                        className="grid grid-cols-[1fr_7rem_11rem] gap-2 px-3 items-center text-xs font-mono cursor-default select-none text-zinc-700 dark:text-zinc-300 hover:bg-zinc-100 dark:hover:bg-zinc-800"
                                        style={{ height: 26 }}
                                    >
                                        <span className="truncate flex items-center">
                                            <button
                                                type="button"
                                                onClick={(e) => {
                                                    e.stopPropagation();
                                                    toggleBookmark(b);
                                                }}
                                                className="mr-1.5 shrink-0 text-zinc-400 hover:text-amber-500 cursor-pointer"
                                                title="즐겨찾기에 추가"
                                            >
                                                ☆
                                            </button>
                                            <svg
                                                className="w-3.5 h-3.5 mr-1.5 shrink-0 text-amber-500"
                                                fill="currentColor"
                                                viewBox="0 0 20 20"
                                            >
                                                <path d="M2 6a2 2 0 012-2h4l2 2h6a2 2 0 012 2v6a2 2 0 01-2 2H4a2 2 0 01-2-2V6z" />
                                            </svg>
                                            <span className="truncate">
                                                {b}
                                            </span>
                                        </span>
                                        <span className="text-right text-zinc-500 dark:text-zinc-400 truncate">
                                            -
                                        </span>
                                        <span className="text-right text-zinc-500 dark:text-zinc-400 truncate">
                                            -
                                        </span>
                                    </div>
                                ))}
                            {!loading &&
                                buckets.length === 0 &&
                                bookmarks.length === 0 &&
                                bucketsError === null && (
                                    <div className="px-3 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
                                        버킷이 없습니다
                                    </div>
                                )}
                        </>
                    ) : entries.length === 0 && error === null && !loading ? (
                        <div className="px-3 py-6 text-center text-xs text-zinc-400 dark:text-zinc-500">
                            빈 폴더입니다
                        </div>
                    ) : (
                        entries.map((entry) => {
                            const isSel = selected.has(entry.name);
                            return (
                                <div
                                    key={entry.name}
                                    onClick={(e) => handleEntryClick(entry, e)}
                                    onDoubleClick={() => {
                                        if (entry.isDir) {
                                            void loadPrefix(
                                                bucket,
                                                `${prefix}${entry.name}/`,
                                            );
                                        } else {
                                            openObjects([entry.name]);
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
                                            : formatBytes(entry.size)}
                                    </span>
                                    <span className="text-right text-zinc-500 dark:text-zinc-400 truncate">
                                        {formatMtime(entry.mtime)}
                                    </span>
                                </div>
                            );
                        })
                    )}
                </div>
                {bucket !== null && entries.length >= MAX_PREFIX_ENTRIES && (
                    <div className="shrink-0 px-3 py-1 text-[10px] text-amber-600 dark:text-amber-400 border-t border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900">
                        항목이 많아 처음{" "}
                        {MAX_PREFIX_ENTRIES.toLocaleString("ko-KR")}개만
                        표시합니다
                    </div>
                )}
                {loading && (entries.length > 0 || buckets.length > 0) && (
                    <div className="shrink-0 px-3 py-1 text-[10px] text-zinc-400 dark:text-zinc-500 border-t border-zinc-200 dark:border-zinc-800 bg-zinc-50 dark:bg-zinc-900">
                        불러오는 중…
                    </div>
                )}
            </div>

            {/* 푸터: 선택 정보 + 열기 */}
            <div className="shrink-0 flex items-center justify-between px-4 py-2.5 border-t border-zinc-200 dark:border-zinc-800">
                <span className="text-[11px] text-zinc-500 dark:text-zinc-400 truncate">
                    {selected.size > 0
                        ? `${selected.size}개 객체 선택됨 (Shift/Cmd+클릭으로 다중 선택)`
                        : "객체를 더블클릭하거나 선택 후 [열기]"}
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
                        열기{selected.size > 1 ? ` (${selected.size})` : ""}
                    </button>
                </div>
            </div>

            {/* 1GB 이상 객체 열기 확인 창 */}
            {confirm !== null && (
                <div
                    role="dialog"
                    aria-modal="true"
                    aria-label="대용량 S3 객체 열기 확인"
                    className="fixed inset-0 z-[60] flex items-center justify-center bg-black/30 dark:bg-black/50"
                    onMouseDown={(e) => {
                        if (e.target === e.currentTarget) setConfirm(null);
                    }}
                >
                    <div className="w-[26rem] max-w-[92vw] rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-xl p-4">
                        <div className="text-sm font-semibold text-zinc-900 dark:text-zinc-100 mb-2">
                            대용량 객체를 엽니다
                        </div>
                        <div className="text-xs text-zinc-700 dark:text-zinc-300 space-y-1.5">
                            <p>
                                {confirm.count > 1
                                    ? `선택한 ${confirm.count}개 객체의 총 크기는 ${formatBytes(confirm.totalBytes)}입니다.`
                                    : `객체 크기: ${formatBytes(confirm.totalBytes)}`}
                            </p>
                            <p>
                                전체 인덱싱·검색 시 AWS 외부로 약{" "}
                                {formatBytes(confirm.totalBytes)}를 전송합니다.
                                데이터 전송 요금이 발생할 수 있습니다.
                            </p>
                        </div>
                        <label className="mt-3 flex items-center gap-1.5 text-xs text-zinc-600 dark:text-zinc-400 cursor-pointer select-none">
                            <input
                                type="checkbox"
                                checked={dontAskAgain}
                                onChange={(e) =>
                                    setDontAskAgain(e.target.checked)
                                }
                                className="cursor-pointer"
                            />
                            다시 묻지 않기
                        </label>
                        <div className="mt-3.5 flex justify-end gap-2">
                            <button
                                type="button"
                                onClick={() => setConfirm(null)}
                                className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                            >
                                취소
                            </button>
                            <button
                                type="button"
                                onClick={handleConfirmOpen}
                                className="px-3 py-1.5 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 cursor-pointer"
                            >
                                열기
                            </button>
                        </div>
                    </div>
                </div>
            )}
        </>
    );
};
