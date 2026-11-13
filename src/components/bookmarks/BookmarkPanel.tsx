import React, { useEffect, useState } from "react";
import { getLines, type Bookmark } from "@/lib/ipc";
import { BOOKMARK_COLORS } from "@/lib/bookmarks";

/** 목록에 표시할 줄 텍스트 미리보기 최대 길이 */
const PREVIEW_MAX = 80;

interface BookmarkPanelProps {
    fileId: number;
    items: Bookmark[];
    onClose: () => void;
    /** 항목 클릭 시 해당 줄로 이동 (0-based 줄 번호) */
    onNavigate: (line: number) => void;
    onChangeNote: (line: number, note: string) => void;
    onChangeColor: (line: number, color: string) => void;
    onDelete: (line: number) => void;
}

interface BookmarkRowProps {
    bookmark: Bookmark;
    /** 줄 텍스트 미리보기. undefined면 아직 로딩 중 */
    preview: string | undefined;
    onNavigate: () => void;
    onChangeNote: (note: string) => void;
    onOpenColorPicker: (target: HTMLButtonElement) => void;
    onDelete: () => void;
}

/**
 * 북마크 한 항목. 메모는 Enter 또는 포커스 해제 시 저장한다.
 */
const BookmarkRow: React.FC<BookmarkRowProps> = ({
    bookmark,
    preview,
    onNavigate,
    onChangeNote,
    onOpenColorPicker,
    onDelete,
}) => {
    // 메모 입력 초안: 외부에서 note가 바뀌면(불러오기 등) 다시 동기화한다
    const [draft, setDraft] = useState(bookmark.note);
    const [prevNote, setPrevNote] = useState(bookmark.note);
    if (prevNote !== bookmark.note) {
        setPrevNote(bookmark.note);
        setDraft(bookmark.note);
    }

    return (
        <div className="px-2 py-1.5 border-b border-zinc-100 dark:border-zinc-800/60 last:border-b-0">
            <div className="flex items-center gap-1.5">
                {/* 색 점 — 클릭하면 색 팔레트 팝오버 */}
                <button
                    type="button"
                    onClick={(e) => {
                        e.stopPropagation();
                        onOpenColorPicker(e.currentTarget);
                    }}
                    className="w-3 h-3 rounded-full shrink-0 cursor-pointer border border-black/10"
                    style={{ backgroundColor: bookmark.color }}
                    title="색상 변경"
                />
                {/* 줄 번호 + 미리보기 — 클릭하면 그 줄로 이동 */}
                <button
                    type="button"
                    onClick={onNavigate}
                    className="flex-1 min-w-0 text-left cursor-pointer rounded px-1 -mx-1 hover:bg-zinc-100 dark:hover:bg-zinc-800/60"
                    title={`줄 ${(bookmark.line + 1).toLocaleString("ko-KR")}로 이동`}
                >
                    <span className="font-mono text-[10px] text-zinc-400 dark:text-zinc-500">
                        줄 {(bookmark.line + 1).toLocaleString("ko-KR")}
                    </span>
                    <div className="font-mono text-xs text-zinc-700 dark:text-zinc-300 truncate">
                        {preview === undefined
                            ? "불러오는 중…"
                            : preview === ""
                              ? "(빈 줄)"
                              : preview}
                    </div>
                </button>
                {/* 삭제 */}
                <button
                    type="button"
                    onClick={onDelete}
                    className="shrink-0 px-1 text-xs text-zinc-400 hover:text-rose-600 dark:hover:text-rose-400 cursor-pointer"
                    title="북마크 삭제"
                >
                    ✕
                </button>
            </div>
            {/* 메모 인라인 편집 (Enter 저장, Esc 취소) */}
            <input
                type="text"
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                    if (e.key === "Enter") {
                        e.preventDefault();
                        onChangeNote(draft);
                        e.currentTarget.blur();
                    } else if (e.key === "Escape") {
                        // 패널 닫기 단축키로 번지지 않게 전파를 막는다
                        e.preventDefault();
                        e.stopPropagation();
                        setDraft(bookmark.note);
                        e.currentTarget.blur();
                    }
                }}
                onBlur={() => {
                    if (draft !== bookmark.note) onChangeNote(draft);
                }}
                placeholder="메모 (Enter 저장)"
                className="mt-1 w-full h-6 px-1.5 text-xs bg-white dark:bg-zinc-900 border border-zinc-200 dark:border-zinc-700 rounded focus:outline-none focus:ring-1 focus:ring-blue-500 text-zinc-800 dark:text-zinc-200 placeholder:text-zinc-400 dark:placeholder:text-zinc-600"
            />
        </div>
    );
};

/**
 * 북마크 목록 오른쪽 사이드 패널 (긴 줄 패널과 같은 위치).
 * Esc로 닫고, 항목 클릭 시 해당 줄로 이동한다.
 */
export const BookmarkPanel: React.FC<BookmarkPanelProps> = ({
    fileId,
    items,
    onClose,
    onNavigate,
    onChangeNote,
    onChangeColor,
    onDelete,
}) => {
    // 줄 텍스트 미리보기 캐시 (줄 번호 → 앞 80자). fileId와 함께 보관해 파일 전환 시 자동 무효화한다
    const [previews, setPreviews] = useState<{
        fileId: number;
        map: Map<number, string>;
    }>({ fileId, map: new Map() });
    // 색 팔레트 팝오버가 열린 북마크 줄과 위치
    const [colorPicker, setColorPicker] = useState<{
        line: number;
        x: number;
        y: number;
    } | null>(null);

    // 파일이 바뀌면 이전 파일의 미리보기 캐시를 버린다 (렌더 중 상태 조정)
    if (previews.fileId !== fileId) {
        setPreviews({ fileId, map: new Map() });
    }

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

    // 아직 가져오지 않은 북마크 줄의 텍스트 앞부분을 조회한다
    useEffect(() => {
        const missing = items
            .map((b) => b.line)
            .filter((line) => !previews.map.has(line));
        if (missing.length === 0) return;
        let cancelled = false;
        void Promise.all(
            missing.map(async (line) => {
                try {
                    const lines = await getLines(fileId, line, 1);
                    const text = lines[0]?.text ?? "";
                    return [
                        line,
                        text.length > PREVIEW_MAX
                            ? `${text.slice(0, PREVIEW_MAX)}…`
                            : text,
                    ] as const;
                } catch {
                    return [line, ""] as const;
                }
            }),
        ).then((fetched) => {
            if (cancelled) return;
            setPreviews((prev) => {
                // 응답이 도착하는 사이 파일이 바뀌었으면 폐기한다
                const base =
                    prev.fileId === fileId
                        ? prev.map
                        : new Map<number, string>();
                const next = new Map(base);
                for (const [line, text] of fetched) {
                    next.set(line, text);
                }
                return { fileId, map: next };
            });
        });
        return () => {
            cancelled = true;
        };
    }, [fileId, items, previews]);

    return (
        <aside className="w-[40%] shrink-0 h-full flex flex-col border-l border-zinc-200 dark:border-zinc-800 bg-zinc-50/60 dark:bg-zinc-900/40">
            {/* 헤더: 항목 수, 닫기 버튼 */}
            <div className="shrink-0 px-3 py-2 border-b border-zinc-200 dark:border-zinc-800 flex items-center justify-between gap-2">
                <div className="text-xs font-medium text-zinc-600 dark:text-zinc-400">
                    북마크 {items.length.toLocaleString("ko-KR")}개
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

            {/* 목록 (비어 있으면 사용법 안내) */}
            <div className="flex-1 min-h-0 overflow-y-auto">
                {items.length === 0 ? (
                    <div className="p-4 text-xs text-zinc-500 dark:text-zinc-400 space-y-1.5 leading-relaxed">
                        <p className="font-medium text-zinc-600 dark:text-zinc-300">
                            북마크가 없습니다.
                        </p>
                        <p>⌘B — 현재 줄에 북마크 추가/제거</p>
                        <p>F2 / ⇧F2 — 다음/이전 북마크로 이동</p>
                        <p>
                            줄 번호 우클릭(또는 Option+클릭)으로도 토글할 수
                            있습니다.
                        </p>
                        <p>
                            북마크는 파일마다 저장되어 다음에 열 때 복원됩니다.
                        </p>
                    </div>
                ) : (
                    items.map((b) => (
                        <BookmarkRow
                            key={b.line}
                            bookmark={b}
                            preview={previews.map.get(b.line)}
                            onNavigate={() => onNavigate(b.line)}
                            onChangeNote={(note) => onChangeNote(b.line, note)}
                            onOpenColorPicker={(target) => {
                                const r = target.getBoundingClientRect();
                                setColorPicker((prev) =>
                                    prev !== null && prev.line === b.line
                                        ? null
                                        : {
                                              line: b.line,
                                              x: r.left,
                                              y: r.bottom,
                                          },
                                );
                            }}
                            onDelete={() => onDelete(b.line)}
                        />
                    ))
                )}
            </div>

            {/* 색 팔레트 팝오버 */}
            {colorPicker !== null && (
                <>
                    <div
                        className="fixed inset-0 z-40"
                        onClick={() => setColorPicker(null)}
                    />
                    <div
                        className="fixed z-50 flex items-center gap-1 p-1.5 rounded-lg border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-lg"
                        style={{
                            left: colorPicker.x,
                            top: colorPicker.y + 4,
                        }}
                    >
                        {BOOKMARK_COLORS.map((color) => (
                            <button
                                key={color}
                                type="button"
                                onClick={() => {
                                    onChangeColor(colorPicker.line, color);
                                    setColorPicker(null);
                                }}
                                className={`w-4 h-4 rounded-full cursor-pointer border ${
                                    items.find(
                                        (b) => b.line === colorPicker.line,
                                    )?.color === color
                                        ? "border-zinc-900 dark:border-zinc-100 ring-1 ring-offset-1 ring-zinc-400"
                                        : "border-black/10"
                                }`}
                                style={{ backgroundColor: color }}
                                title={color}
                            />
                        ))}
                    </div>
                </>
            )}
        </aside>
    );
};

BookmarkPanel.displayName = "BookmarkPanel";
