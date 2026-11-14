import React, { useState } from "react";
import type { FileInfo, IndexProgress } from "@/lib/ipc";
import { remoteHostOf, s3BucketOf, s3DisplayPath } from "@/lib/remote";

/** App이 관리하는 탭 메타데이터 (TabBar 표시용) */
export interface TabMeta {
    id: string;
    /** 사용자가 요청한 경로 (openFile 인자). 병합 탭은 "merge://<tabId>" */
    path: string;
    /** Rust가 canonicalize한 경로를 포함한 파일 정보 (열리기 전 null) */
    file: FileInfo | null;
    /** 인덱싱/해제/병합 구축 진행률 (탭 진행률 바용) */
    progress: IndexProgress | null;
    /** tail 모드 활성 여부 (탭 표시용) */
    tailOn: boolean;
    /** 탭 표시 이름 (병합 탭 "병합: a.log + b.log"). 없으면 경로 basename */
    title?: string;
    /** 병합 탭이면 원본 파일 id 목록 (이 필드가 있으면 openMerge로 연다) */
    mergeSourceIds?: number[];
}

interface TabBarProps {
    tabs: TabMeta[];
    activeId: string | null;
    onSelect: (id: string) => void;
    onClose: (id: string) => void;
    /** HTML5 드래그 앤 드롭 순서 변경 (from 인덱스의 탭을 to 위치로 이동) */
    onReorder: (from: number, to: number) => void;
    onOpenDialog: () => void;
    /** 병합 대화상자 열기 (병합 가능한 탭이 2개 미만이면 비활성) */
    mergeable: boolean;
    onMergeClick: () => void;
}

/** 진행률 퍼센트. 완료/미시작/크기 미상이면 null (바를 표시하지 않음) */
function progressPct(p: IndexProgress | null): number | null {
    if (p === null || p.done || p.totalBytes <= 0) return null;
    return Math.min(100, (p.indexedBytes / p.totalBytes) * 100);
}

/** 탭에 표시할 파일 이름 (명시 제목 우선, 없으면 경로 basename) */
function tabName(tab: TabMeta): string {
    if (tab.title !== undefined) return tab.title;
    const p = tab.file?.path ?? tab.path;
    return p.split("/").pop() || p;
}

/**
 * 상단 탭 바. 탭 클릭으로 전환, ×/가운데 클릭으로 닫기,
 * 드래그로 순서 변경, 하단 얇은 바로 인덱싱 진행률을 표시한다.
 */
export const TabBar: React.FC<TabBarProps> = ({
    tabs,
    activeId,
    onSelect,
    onClose,
    onReorder,
    onOpenDialog,
    mergeable,
    onMergeClick,
}) => {
    const [dragIndex, setDragIndex] = useState<number | null>(null);

    return (
        <div
            className="h-10 shrink-0 flex items-center border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50/80 dark:bg-zinc-900/80 backdrop-blur relative z-20"
            // 탭 사이 빈 공간에 놓아도 끝으로 이동되게 컨테이너에서도 드롭을 받는다
            onDragOver={(e) => e.preventDefault()}
            onDrop={(e) => {
                e.preventDefault();
                if (dragIndex !== null) {
                    onReorder(dragIndex, tabs.length);
                    setDragIndex(null);
                }
            }}
        >
            <span className="font-semibold text-sm tracking-tight text-zinc-800 dark:text-zinc-200 px-3 shrink-0 select-none">
                Riffle
            </span>

            {/* 탭 목록 (많으면 가로 스크롤) */}
            <div className="flex-1 flex items-center min-w-0 overflow-x-auto">
                {tabs.map((tab, i) => {
                    const active = tab.id === activeId;
                    const pct = progressPct(tab.progress);
                    const fullPath = tab.file?.path ?? tab.path;
                    // 병합 탭 배지와 원격 탭 배지(sftp 호스트, s3 버킷)
                    const isMerge =
                        tab.mergeSourceIds !== undefined ||
                        tab.file?.source === "merge";
                    const remoteHost = isMerge ? null : remoteHostOf(fullPath);
                    const s3Bucket = isMerge ? null : s3BucketOf(fullPath);
                    // 저장된 연결로 연 S3 파일이면 연결 이름 (배지에 버킷 대신 표시)
                    const s3ConnName = tab.file?.s3ConnectionName ?? null;
                    return (
                        <div
                            key={tab.id}
                            draggable
                            onDragStart={(e) => {
                                setDragIndex(i);
                                e.dataTransfer.effectAllowed = "move";
                            }}
                            onDragEnd={() => setDragIndex(null)}
                            onDragOver={(e) => e.preventDefault()}
                            onDrop={(e) => {
                                e.preventDefault();
                                e.stopPropagation();
                                if (dragIndex !== null) {
                                    onReorder(dragIndex, i);
                                    setDragIndex(null);
                                }
                            }}
                            onClick={() => onSelect(tab.id)}
                            onMouseDown={(e) => {
                                // 가운데 클릭 자동 스크롤 방지
                                if (e.button === 1) e.preventDefault();
                            }}
                            onAuxClick={(e) => {
                                if (e.button === 1) {
                                    e.preventDefault();
                                    onClose(tab.id);
                                }
                            }}
                            title={s3DisplayPath(fullPath, s3ConnName)}
                            className={`group relative flex items-center h-8 mx-0.5 px-2.5 rounded-md text-xs cursor-default select-none shrink-0 max-w-[220px] ${
                                active
                                    ? "bg-white dark:bg-zinc-800 text-zinc-900 dark:text-zinc-100 shadow-sm font-medium"
                                    : "text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60"
                            } ${dragIndex === i ? "opacity-40" : ""}`}
                        >
                            {isMerge && (
                                <span
                                    className="mr-1.5 text-[9px] font-bold px-1 py-px rounded bg-violet-100 dark:bg-violet-900/60 text-violet-700 dark:text-violet-300 shrink-0"
                                    title="여러 로그의 시간순 병합 뷰"
                                >
                                    merge
                                </span>
                            )}
                            {tab.file?.compressed === true && (
                                <span
                                    className="mr-1.5 text-[9px] font-bold px-1 py-px rounded border border-zinc-300 dark:border-zinc-600 text-zinc-400 dark:text-zinc-500 shrink-0"
                                    title="gzip 압축 파일"
                                >
                                    gz
                                </span>
                            )}
                            {remoteHost !== null && (
                                <span
                                    className="mr-1 text-[9px] font-bold px-1 py-px rounded bg-sky-100 dark:bg-sky-900/60 text-sky-700 dark:text-sky-300 shrink-0"
                                    title={`원격 호스트: ${remoteHost}`}
                                >
                                    {remoteHost}:
                                </span>
                            )}
                            {remoteHost !== null &&
                                tab.file?.agent === true && (
                                    <span
                                        className="mr-1 text-[10px] leading-none text-sky-600 dark:text-sky-400 shrink-0"
                                        title="서버 측 가속(에이전트) 사용 중 — 원격 서버에서 인덱싱·검색 실행"
                                    >
                                        ⚡
                                    </span>
                                )}
                            {s3Bucket !== null && (
                                <span
                                    className="mr-1 text-[9px] font-bold px-1 py-px rounded bg-orange-100 dark:bg-orange-900/60 text-orange-700 dark:text-orange-300 shrink-0"
                                    title={
                                        s3ConnName !== null
                                            ? `S3 연결: ${s3ConnName} · 버킷: ${s3Bucket}`
                                            : `S3 버킷: ${s3Bucket}`
                                    }
                                >
                                    s3:{s3ConnName ?? s3Bucket}
                                </span>
                            )}
                            <span className="truncate">{tabName(tab)}</span>
                            {tab.tailOn && (
                                <span
                                    className="ml-1.5 w-1.5 h-1.5 rounded-full bg-emerald-500 shrink-0"
                                    title="tail 모드 켜짐"
                                />
                            )}
                            <button
                                type="button"
                                onClick={(e) => {
                                    e.stopPropagation();
                                    onClose(tab.id);
                                }}
                                className="ml-1.5 -mr-0.5 w-4 h-4 rounded flex items-center justify-center text-zinc-400 hover:text-zinc-700 dark:hover:text-zinc-200 hover:bg-zinc-300/70 dark:hover:bg-zinc-600/70 opacity-0 group-hover:opacity-100 transition-opacity cursor-pointer shrink-0"
                                title="탭 닫기 (⌘W)"
                            >
                                <span className="text-[11px] leading-none">
                                    ×
                                </span>
                            </button>
                            {/* 인덱싱/해제 진행률 얇은 바 */}
                            {pct !== null && (
                                <div className="absolute bottom-0 left-1 right-1 h-0.5 rounded-full bg-zinc-200 dark:bg-zinc-700 overflow-hidden">
                                    <div
                                        className="h-full bg-blue-500 transition-[width] duration-150"
                                        style={{ width: `${pct}%` }}
                                    />
                                </div>
                            )}
                        </div>
                    );
                })}
            </div>

            {/* 로그 병합 버튼 (병합 가능한 탭이 2개 이상일 때만 활성) */}
            <button
                type="button"
                onClick={onMergeClick}
                disabled={!mergeable}
                title={
                    mergeable
                        ? "열린 탭 2~8개를 타임스탬프 순으로 병합합니다"
                        : "병합하려면 파일이 2개 이상 열려 있어야 합니다"
                }
                className={`mr-1.5 px-2.5 py-1 text-xs font-medium rounded-md transition-colors flex items-center space-x-1.5 shrink-0 ${
                    mergeable
                        ? "bg-zinc-100 text-zinc-700 hover:bg-zinc-200 dark:bg-zinc-800 dark:text-zinc-300 dark:hover:bg-zinc-700 cursor-pointer"
                        : "bg-zinc-50 text-zinc-400 dark:bg-zinc-900 dark:text-zinc-600 cursor-not-allowed"
                }`}
            >
                <span>병합…</span>
                <kbd className="text-[10px] px-1 py-0.5 rounded bg-zinc-200 dark:bg-zinc-700 text-zinc-500 dark:text-zinc-400 font-mono">
                    ⌘⇧M
                </kbd>
            </button>

            {/* 파일 열기 버튼 */}
            <button
                type="button"
                onClick={onOpenDialog}
                className="mx-2 px-2.5 py-1 text-xs font-medium rounded-md bg-zinc-900 text-zinc-50 hover:bg-zinc-800 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-200 transition-colors flex items-center space-x-1.5 cursor-pointer shadow-xs shrink-0"
            >
                <span>파일 열기</span>
                <kbd className="text-[10px] px-1 py-0.5 rounded bg-zinc-800 dark:bg-zinc-200 text-zinc-300 dark:text-zinc-700 font-mono">
                    ⌘O
                </kbd>
            </button>
        </div>
    );
};
