import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWebview, type DragDropEvent } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen, type Event as TauriEvent } from "@tauri-apps/api/event";
import {
    getRecentFiles,
    getSettings,
    loadSession,
    saveSession,
    takePendingOpen,
    type FileInfo,
    type IndexProgress,
    type MenuEvent,
} from "@/lib/ipc";
import { checkForUpdate } from "@/lib/updater";
import { useTauriListener } from "@/lib/useTauriListener";
import {
    clampFontSize,
    DEFAULT_FONT_SIZE,
    loadFontSize,
    saveFontSize,
} from "@/lib/fontSize";
import { FileTab } from "@/components/FileTab";
import { TabBar, type TabMeta } from "@/components/TabBar";
import { EmptyState } from "@/components/EmptyState";
import { ShortcutsDialog } from "@/components/ShortcutsDialog";
import { MergeDialog } from "@/components/MergeDialog";
import { isMergePath, mergeTabTitle } from "@/lib/merge";
import { RemoteOpenDialog } from "@/components/remote/RemoteOpenDialog";
import {
    UpdateDialog,
    type UpdateDialogState,
} from "@/components/UpdateDialog";

/**
 * Riffle 메인 애플리케이션 컴포넌트.
 * 탭 목록/활성 탭과 전역 상태(글자 크기, 업데이트, 메뉴/열기 진입점, 세션)만 관리하고,
 * 파일별 상태와 뷰어는 각 FileTab이 소유한다.
 */
export default function App() {
    // ===== 탭 상태 =====
    // tabsRef는 setState와 동기적으로 갱신해 이벤트 핸들러에서 항상 최신 목록을 읽게 한다
    const [tabs, setTabs] = useState<TabMeta[]>([]);
    const tabsRef = useRef<TabMeta[]>([]);
    const [activeTabId, setActiveTabId] = useState<string | null>(null);
    const activeTabIdRef = useRef<string | null>(null);
    const tabSeqRef = useRef(0);

    const commitTabs = useCallback((next: TabMeta[]) => {
        tabsRef.current = next;
        setTabs(next);
    }, []);

    const activateTab = useCallback((id: string | null) => {
        activeTabIdRef.current = id;
        setActiveTabId(id);
    }, []);

    // 경로 목록을 탭으로 추가한다. 이미 같은 경로(요청 경로 또는 해석된 경로)가
    // 열려 있으면 새 탭을 만들지 않고 기존 탭 id를 재사용한다.
    // 반환값은 입력 경로 순서대로 대응하는 탭 id 목록이다.
    const addTabsForPaths = useCallback(
        (paths: string[]): string[] => {
            const base = tabsRef.current;
            const next = [...base];
            const ids: string[] = [];
            for (const path of paths) {
                const found = next.find(
                    (t) => t.path === path || t.file?.path === path,
                );
                if (found !== undefined) {
                    ids.push(found.id);
                    continue;
                }
                const id = `tab-${++tabSeqRef.current}`;
                next.push({
                    id,
                    path,
                    file: null,
                    progress: null,
                    tailOn: false,
                });
                ids.push(id);
            }
            if (next.length !== base.length) {
                commitTabs(next);
            }
            return ids;
        },
        [commitTabs],
    );

    // 공통 열기 경로: 각 경로마다 중복이 아니면 새 탭을 만들고 마지막 탭을 활성화한다.
    const openPaths = useCallback(
        (paths: string[]) => {
            const ids = addTabsForPaths(paths);
            if (ids.length > 0) {
                activateTab(ids[ids.length - 1]);
            }
        },
        [addTabsForPaths, activateTab],
    );

    // 탭 닫기: 닫힌 위치의 오른쪽(없으면 왼쪽) 탭을 활성화한다.
    // 마지막 탭을 닫으면 빈 화면(최근 파일)이 표시된다.
    const closeTab = useCallback(
        (id: string) => {
            const cur = tabsRef.current;
            const idx = cur.findIndex((t) => t.id === id);
            if (idx < 0) return;
            const next = cur.filter((t) => t.id !== id);
            commitTabs(next);
            if (activeTabIdRef.current === id) {
                const neighbor =
                    next.length > 0
                        ? next[Math.min(idx, next.length - 1)]
                        : undefined;
                activateTab(neighbor !== undefined ? neighbor.id : null);
            }
        },
        [commitTabs, activateTab],
    );

    // 다음/이전 탭으로 순환 이동
    const activateRelativeTab = useCallback(
        (dir: 1 | -1) => {
            const cur = tabsRef.current;
            if (cur.length === 0) return;
            const idx = cur.findIndex((t) => t.id === activeTabIdRef.current);
            const base = idx < 0 ? (dir === 1 ? -1 : 0) : idx;
            const nextIdx =
                (((base + dir) % cur.length) + cur.length) % cur.length;
            activateTab(cur[nextIdx].id);
        },
        [activateTab],
    );

    // 순번 탭 선택 (Cmd+1~8, Cmd+9=마지막)
    const activateTabAt = useCallback(
        (index: number) => {
            const cur = tabsRef.current;
            if (index >= 0 && index < cur.length) {
                activateTab(cur[index].id);
            }
        },
        [activateTab],
    );

    // 탭 순서 변경 (TabBar 드래그 앤 드롭)
    const reorderTabs = useCallback(
        (from: number, to: number) => {
            const cur = tabsRef.current;
            if (from === to || from < 0 || to < 0 || from >= cur.length) {
                return;
            }
            const next = cur.slice();
            const [moved] = next.splice(from, 1);
            next.splice(Math.min(to, next.length), 0, moved);
            commitTabs(next);
        },
        [commitTabs],
    );

    // 탭 메타 일부 갱신 (FileTab → App 상태 보고)
    const updateTab = useCallback(
        (id: string, patch: Partial<TabMeta>) => {
            const cur = tabsRef.current;
            if (!cur.some((t) => t.id === id)) return;
            commitTabs(cur.map((t) => (t.id === id ? { ...t, ...patch } : t)));
        },
        [commitTabs],
    );

    // FileTab이 파일 열기를 완료하면 보고한다.
    // 같은 canonical 경로가 이미 열려 있으면(심볼릭 링크 등) 새 탭을 닫고 기존 탭으로 전환한다.
    const handleTabFileInfo = useCallback(
        (tabId: string, info: FileInfo) => {
            const dup = tabsRef.current.find(
                (t) =>
                    t.id !== tabId &&
                    (t.path === info.path || t.file?.path === info.path),
            );
            if (dup !== undefined) {
                closeTab(tabId);
                activateTab(dup.id);
                return;
            }
            updateTab(tabId, { file: info });
        },
        [closeTab, activateTab, updateTab],
    );

    const handleTabProgress = useCallback(
        (tabId: string, progress: IndexProgress | null) => {
            const cur = tabsRef.current.find((t) => t.id === tabId);
            // 동일 값 재보고(열기 시작 시 null 등)는 상태 갱신을 유발하지 않는다
            if (cur === undefined || cur.progress === progress) return;
            updateTab(tabId, { progress });
        },
        [updateTab],
    );

    const handleTabTailChange = useCallback(
        (tabId: string, tailOn: boolean) => {
            const cur = tabsRef.current.find((t) => t.id === tabId);
            if (cur === undefined || cur.tailOn === tailOn) return;
            updateTab(tabId, { tailOn });
        },
        [updateTab],
    );

    // ===== 로그 병합 (Phase 17) =====

    // 로그 병합 대화상자 표시 여부 (⌘⇧M)
    const [mergeDialogOpen, setMergeDialogOpen] = useState(false);

    // 병합 가능한 탭 = 파일이 열려 있고 병합 뷰가 아닌 탭 (병합 탭은 원본이 될 수 없다)
    const mergeableTabs = useMemo(
        () => tabs.filter((t) => t.file !== null && t.file.source !== "merge"),
        [tabs],
    );

    // 병합 대화상자 확인: 선택 순서대로 원본 id/이름을 모아 병합 탭을 만든다
    const handleMergeConfirm = useCallback(
        (tabIds: string[]) => {
            const cur = tabsRef.current;
            const files: { id: number; path: string }[] = [];
            for (const tid of tabIds) {
                const file = cur.find((t) => t.id === tid)?.file;
                if (file !== null && file !== undefined) {
                    files.push({ id: file.id, path: file.path });
                }
            }
            if (files.length < 2 || files.length > 8) return;
            const id = `tab-${++tabSeqRef.current}`;
            commitTabs([
                ...cur,
                {
                    id,
                    // 실제 파일이 아닌 표시용 경로 — FileTab은 mergeSourceIds로 openMerge를 호출한다
                    path: `merge://${id}`,
                    file: null,
                    progress: null,
                    tailOn: false,
                    title: mergeTabTitle(files.map((f) => f.path)),
                    mergeSourceIds: files.map((f) => f.id),
                },
            ]);
            activateTab(id);
            setMergeDialogOpen(false);
        },
        [commitTabs, activateTab],
    );

    // 병합 탭 → 원본 탭 줄 이동 요청 (탭 id → 요청). key는 같은 줄 재요청도 구분하기 위한 순번
    const [gotoRequests, setGotoRequests] = useState<
        Record<string, { line: number; key: number }>
    >({});
    const gotoSeqRef = useRef(0);

    // 병합 탭에서 원본 탭으로 이동: 탭을 찾거나(닫혀 있으면 다시 열고) 활성화한 뒤
    // 해당 원본 줄로 이동시킨다 (이동 자체는 대상 FileTab이 gotoRequest를 받아 수행)
    const handleJumpToSource = useCallback(
        (path: string, line: number) => {
            const ids = addTabsForPaths([path]);
            const tid = ids[0];
            gotoSeqRef.current += 1;
            setGotoRequests((prev) => ({
                ...prev,
                [tid]: { line, key: gotoSeqRef.current },
            }));
            activateTab(tid);
        },
        [addTabsForPaths, activateTab],
    );

    // ===== 전역 상태 =====
    const [fontSize, setFontSize] = useState<number>(loadFontSize);
    const [isDragOver, setIsDragOver] = useState(false);
    const [recentFiles, setRecentFiles] = useState<string[]>([]);
    // 원격 파일 열기 대화상자 표시 여부
    const [remoteDialogOpen, setRemoteDialogOpen] = useState(false);
    // 단축키 도움말 모달 표시 여부 (⌘/)
    const [shortcutsOpen, setShortcutsOpen] = useState(false);

    // 업데이트 다이얼로그 상태 (null이면 닫힘)와 관련 ref들
    const [updateDialog, setUpdateDialog] = useState<UpdateDialogState | null>(
        null,
    );
    const updateDialogRef = useRef<UpdateDialogState | null>(null);
    // 확인 요청 시퀀스: 다이얼로그를 닫으면 진행 중이던 확인 결과를 폐기한다
    const updateCheckSeqRef = useRef(0);
    // 같은 세션에서 "나중에"로 닫은 뒤 자동 확인이 다시 띄우지 않게 하는 플래그
    const suppressAutoUpdateRef = useRef(false);

    // 비동기 업데이트 확인 콜백에서 최신 다이얼로그 상태를 읽기 위한 ref 동기화
    useEffect(() => {
        updateDialogRef.current = updateDialog;
    }, [updateDialog]);

    // 폰트 크기 조절 (localStorage에 저장, 모든 탭에 공통 적용)
    const adjustFontSize = useCallback((delta: number) => {
        setFontSize((prev) => {
            const next = clampFontSize(prev + delta);
            saveFontSize(next);
            return next;
        });
    }, []);

    const handleFontReset = useCallback(() => {
        setFontSize(DEFAULT_FONT_SIZE);
        saveFontSize(DEFAULT_FONT_SIZE);
    }, []);

    // 파일 선택 다이얼로그 호출 (여러 파일 동시 선택 가능)
    const handleOpenDialog = useCallback(async () => {
        try {
            const selected = await open({
                multiple: true,
                directory: false,
            });
            if (selected === null) return;
            const paths = Array.isArray(selected) ? selected : [selected];
            openPaths(paths);
        } catch (err) {
            console.error("파일 열기 다이얼로그 호출 실패:", err);
        }
    }, [openPaths]);

    // ===== 앱 업데이트 =====

    // 업데이트 확인 실행.
    // visible=true(메뉴 수동 확인)는 결과를 항상 다이얼로그로 표시하고,
    // visible=false(시작 시 자동 확인)는 새 버전이 있을 때만 조용히 표시한다.
    const runUpdateCheck = useCallback((visible: boolean) => {
        const seq = ++updateCheckSeqRef.current;
        if (visible) {
            setUpdateDialog({ status: "checking" });
        }
        checkForUpdate()
            .then((update) => {
                const stale = seq !== updateCheckSeqRef.current;
                if (update === null) {
                    if (!stale && visible) {
                        setUpdateDialog({ status: "upToDate" });
                    }
                    return;
                }
                // 표시하지 않기로 한 업데이트는 Rust 측 리소스를 해제한다
                const suppress =
                    !visible &&
                    (updateDialogRef.current !== null ||
                        suppressAutoUpdateRef.current);
                if (stale || suppress) {
                    update.close().catch(() => {});
                    return;
                }
                setUpdateDialog({ status: "available", update });
            })
            .catch((err: unknown) => {
                if (seq !== updateCheckSeqRef.current) return;
                if (visible) {
                    const msg =
                        err instanceof Error
                            ? err.message
                            : typeof err === "string"
                              ? err
                              : "업데이트 확인에 실패했습니다.";
                    setUpdateDialog({ status: "error", message: msg });
                } else {
                    // 자동 확인의 오류는 조용히 콘솔에만 남긴다
                    console.warn("업데이트 자동 확인 실패:", err);
                }
            });
    }, []);

    // 업데이트 다이얼로그 닫기 ("나중에"/Esc/배경 클릭 공통)
    const handleUpdateDialogClose = useCallback(() => {
        // 진행 중인 확인의 늦은 결과가 다이얼로그를 다시 열지 못하게 무효화한다
        updateCheckSeqRef.current += 1;
        suppressAutoUpdateRef.current = true;
        const current = updateDialogRef.current;
        if (current?.status === "available") {
            current.update.close().catch(() => {});
        }
        setUpdateDialog(null);
    }, []);

    // 시작 시 자동 업데이트 확인: 로드 3초 후, 설정이 켜져 있고 dev 모드가 아닐 때만.
    // 업데이트가 없거나 네트워크 오류면 아무것도 표시하지 않는다.
    useEffect(() => {
        if (import.meta.env.DEV) return;
        const timer = setTimeout(() => {
            getSettings()
                .then((settings) => {
                    if (settings.autoCheckUpdates) {
                        runUpdateCheck(false);
                    }
                })
                .catch((err) => {
                    console.warn("설정 조회 실패:", err);
                });
        }, 3000);
        return () => clearTimeout(timer);
    }, [runUpdateCheck]);

    // ===== 리스너/이벤트 =====

    // 네이티브 메뉴 이벤트 수신.
    // 파일 범위 명령(copy, find, goto_line 등)은 각 FileTab이 자체 리스너로 받아
    // 활성 탭에서만 처리한다. 여기서는 전역 명령만 처리한다.
    // (메뉴 accelerator가 키를 먼저 가로채므로 해당 단축키는 프론트 keydown이 아니라
    // 이 이벤트로만 처리한다)
    useTauriListener(
        (handler) => listen<MenuEvent>("menu", handler),
        (event: TauriEvent<MenuEvent>) => {
            const { id, path } = event.payload;
            switch (id) {
                case "open":
                    void handleOpenDialog();
                    break;
                case "open_remote":
                    setRemoteDialogOpen(true);
                    break;
                case "open_recent":
                    if (path) openPaths([path]);
                    break;
                case "close_tab":
                    if (activeTabIdRef.current !== null) {
                        closeTab(activeTabIdRef.current);
                    }
                    break;
                case "next_tab":
                    activateRelativeTab(1);
                    break;
                case "prev_tab":
                    activateRelativeTab(-1);
                    break;
                case "check_updates":
                    runUpdateCheck(true);
                    break;
                case "font_inc":
                    adjustFontSize(1);
                    break;
                case "font_dec":
                    adjustFontSize(-1);
                    break;
                case "font_reset":
                    handleFontReset();
                    break;
                default:
                    break;
            }
        },
    );

    // Finder "다음으로 열기"/더블클릭 등으로 전달된 여러 파일 경로
    useTauriListener(
        (handler) => listen<string[]>("open-files", handler),
        (event: TauriEvent<string[]>) => {
            if (event.payload && event.payload.length > 0) {
                openPaths(event.payload);
            }
        },
    );

    // Tauri 윈도우 드래그 앤 드롭 이벤트 등록 (여러 파일 동시 드롭 가능)
    useTauriListener(
        (handler) => {
            try {
                return getCurrentWebview().onDragDropEvent(handler);
            } catch (err) {
                console.warn("getCurrentWebview 호출 실패:", err);
                return Promise.resolve(() => {});
            }
        },
        (event: TauriEvent<DragDropEvent>) => {
            const payload = event.payload;
            if (payload.type === "enter" || payload.type === "over") {
                setIsDragOver(true);
            } else if (payload.type === "leave") {
                setIsDragOver(false);
            } else if (payload.type === "drop") {
                setIsDragOver(false);
                if (payload.paths && payload.paths.length > 0) {
                    openPaths(payload.paths);
                }
            }
        },
    );

    // 시작 시 세션 복원 → Finder 대기 경로 순서로 처리한다.
    // pending open은 복원된 탭 위에 추가로 열리고 마지막 것이 활성화된다.
    // 복원 로드가 끝날 때까지 sessionReady를 false로 두어
    // 디바운스 저장이 기존 세션을 빈 값으로 덮어쓰지 않게 한다.
    const [sessionReady, setSessionReady] = useState(false);
    useEffect(() => {
        let cancelled = false;
        (async () => {
            try {
                const session = await loadSession();
                if (
                    !cancelled &&
                    session !== null &&
                    session.paths.length > 0
                ) {
                    const ids = addTabsForPaths(session.paths);
                    const ai = session.activeIndex;
                    const idx =
                        ai !== null && ai >= 0 && ai < ids.length
                            ? ai
                            : ids.length - 1;
                    activateTab(ids[idx]);
                }
            } catch (err) {
                console.warn("세션 복원 실패:", err);
            }
            if (cancelled) return;
            try {
                const pending = await takePendingOpen();
                if (!cancelled && pending.length > 0) {
                    openPaths(pending);
                }
            } catch (err) {
                console.warn("대기 파일 경로 조회 실패:", err);
            }
            if (!cancelled) {
                setSessionReady(true);
            }
        })();
        return () => {
            cancelled = true;
        };
    }, [addTabsForPaths, activateTab, openPaths]);

    // 탭 목록/활성 탭 변경을 500ms 디바운스로 세션에 저장한다.
    // 병합 탭은 재현할 수 없으므로 세션에 저장하지 않는다 (원본 탭만 저장).
    // 진행률 같은 표시용 필드 변경은 키에 포함하지 않아 저장을 유발하지 않는다.
    const sessionKey = useMemo(
        () =>
            `${activeTabId ?? ""}#${tabs
                .map((t) => t.file?.path ?? t.path)
                .join("\n")}`,
        [tabs, activeTabId],
    );
    useEffect(() => {
        // 시작 시 세션 로드가 끝나기 전에는 저장하지 않는다
        if (!sessionReady) return;
        const timer = setTimeout(() => {
            const cur = tabsRef.current;
            const restorable = cur.filter(
                (t) =>
                    t.mergeSourceIds === undefined &&
                    !isMergePath(t.file?.path ?? t.path),
            );
            const paths = restorable.map((t) => t.file?.path ?? t.path);
            const ai = restorable.findIndex(
                (t) => t.id === activeTabIdRef.current,
            );
            saveSession({
                paths,
                activeIndex: ai >= 0 ? ai : null,
            }).catch((err) => {
                console.warn("세션 저장 실패:", err);
            });
        }, 500);
        return () => clearTimeout(timer);
    }, [sessionKey, sessionReady]);

    // 탭이 하나도 없으면 최근 파일 목록을 불러와 빈 화면에 표시한다
    useEffect(() => {
        if (tabs.length > 0) return;
        getRecentFiles()
            .then(setRecentFiles)
            .catch((err) => {
                console.warn("최근 파일 목록 조회 실패:", err);
                setRecentFiles([]);
            });
    }, [tabs]);

    // 창 제목: "활성 파일명 — Riffle" (병합 탭은 "병합: a.log + b.log" 제목)
    const windowTitle = useMemo(() => {
        const active = tabs.find((t) => t.id === activeTabId);
        const p =
            active !== undefined ? (active.file?.path ?? active.path) : null;
        const name =
            active?.title ?? (p !== null ? p.split("/").pop() || p : null);
        return name !== null ? `${name} — Riffle` : "Riffle";
    }, [tabs, activeTabId]);
    useEffect(() => {
        document.title = windowTitle;
        getCurrentWindow()
            .setTitle(windowTitle)
            .catch((err) => {
                console.warn("창 제목 설정 실패:", err);
            });
    }, [windowTitle]);

    // 탭 단축키: Cmd+1~8(순번 탭), Cmd+9(마지막 탭), Ctrl+Tab/Ctrl+Shift+Tab(다음/이전).
    // 참고: Cmd+Shift+[ ] (이전/다음 탭)는 네이티브 메뉴 accelerator가 처리하므로
    // 여기서 중복 처리하지 않는다 (비멱등 동작의 이중 실행 방지).
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            // Cmd+Shift+O: 원격 파일 열기 (탭 유무와 무관하게 동작).
            // 네이티브 메뉴 accelerator가 먼저 가로채면 menu 이벤트로도 도착하므로 둘 다 처리한다.
            if (e.metaKey && e.shiftKey && !e.ctrlKey && !e.altKey) {
                if (e.code === "KeyO") {
                    e.preventDefault();
                    setRemoteDialogOpen(true);
                    return;
                }
                // Cmd+Shift+M: 로그 병합 대화상자 (병합 가능한 탭이 2개 이상일 때)
                if (e.code === "KeyM") {
                    e.preventDefault();
                    const count = tabsRef.current.filter(
                        (t) => t.file !== null && t.file.source !== "merge",
                    ).length;
                    if (count >= 2) {
                        setMergeDialogOpen(true);
                    }
                    return;
                }
            }

            // ⌘/ (또는 ⌘?): 단축키 도움말 토글 (탭 유무와 무관하게 동작)
            if (
                e.metaKey &&
                !e.ctrlKey &&
                !e.altKey &&
                (e.key === "/" || e.key === "?")
            ) {
                e.preventDefault();
                setShortcutsOpen((v) => !v);
                return;
            }

            const count = tabsRef.current.length;
            if (count === 0) return;

            if (e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) {
                if (e.key >= "1" && e.key <= "8") {
                    e.preventDefault();
                    activateTabAt(Number(e.key) - 1);
                } else if (e.key === "9") {
                    e.preventDefault();
                    activateTabAt(count - 1);
                }
            } else if (
                e.ctrlKey &&
                !e.metaKey &&
                !e.altKey &&
                e.key === "Tab"
            ) {
                e.preventDefault();
                activateRelativeTab(e.shiftKey ? -1 : 1);
            }
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [activateTabAt, activateRelativeTab]);

    return (
        <div className="flex flex-col h-screen w-screen overflow-hidden bg-white text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100 select-none relative">
            {/* 상단 탭 바 */}
            <TabBar
                tabs={tabs}
                activeId={activeTabId}
                onSelect={activateTab}
                onClose={closeTab}
                onReorder={reorderTabs}
                onOpenDialog={() => void handleOpenDialog()}
                mergeable={mergeableTabs.length >= 2}
                onMergeClick={() => setMergeDialogOpen(true)}
            />

            {/* 탭 스택: 활성 탭만 표시, 나머지는 숨긴 채 마운트 유지(백그라운드 작업 계속) */}
            {tabs.length === 0 ? (
                <EmptyState
                    recentFiles={recentFiles}
                    onOpenDialog={() => void handleOpenDialog()}
                    onOpenRemote={() => setRemoteDialogOpen(true)}
                    onOpenPath={(path) => openPaths([path])}
                    onShowShortcuts={() => setShortcutsOpen(true)}
                />
            ) : (
                tabs.map((tab) => (
                    <FileTab
                        key={tab.id}
                        tabId={tab.id}
                        path={tab.path}
                        mergeSourceIds={tab.mergeSourceIds ?? null}
                        isActive={tab.id === activeTabId}
                        fontSize={fontSize}
                        onFileInfo={handleTabFileInfo}
                        onProgress={handleTabProgress}
                        onTailChange={handleTabTailChange}
                        onFontSizeAdjust={adjustFontSize}
                        onJumpToSource={handleJumpToSource}
                        gotoRequest={gotoRequests[tab.id] ?? null}
                    />
                ))
            )}

            {/* 창 드래그 오버 안내 오버레이 */}
            {isDragOver && (
                <div className="absolute inset-0 bg-blue-500/10 dark:bg-blue-500/20 backdrop-blur-xs border-2 border-dashed border-blue-500 flex items-center justify-center z-50 pointer-events-none">
                    <div className="px-4 py-2 rounded-lg bg-blue-600 text-white font-medium text-sm shadow-lg">
                        여기에 놓아 파일 열기
                    </div>
                </div>
            )}

            {/* 원격 파일 열기 모달 */}
            {remoteDialogOpen && (
                <RemoteOpenDialog
                    onClose={() => setRemoteDialogOpen(false)}
                    onOpenPaths={(paths) => openPaths(paths)}
                />
            )}

            {/* 로그 병합 모달 (⌘⇧M / 탭 바 "병합…") */}
            {mergeDialogOpen && (
                <MergeDialog
                    tabs={mergeableTabs}
                    onClose={() => setMergeDialogOpen(false)}
                    onMerge={handleMergeConfirm}
                />
            )}

            {/* 앱 업데이트 모달 (status가 바뀌면 리마운트해 설치 상태를 초기화) */}
            {updateDialog !== null && (
                <UpdateDialog
                    key={updateDialog.status}
                    state={updateDialog}
                    onClose={handleUpdateDialogClose}
                />
            )}

            {/* 단축키 도움말 모달 (⌘/) */}
            {shortcutsOpen && (
                <ShortcutsDialog onClose={() => setShortcutsOpen(false)} />
            )}
        </div>
    );
}
