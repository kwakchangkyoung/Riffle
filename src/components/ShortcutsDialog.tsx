import React, { useEffect } from "react";

interface ShortcutsDialogProps {
    onClose: () => void;
}

/** 단축키 그룹/항목 정의 — README의 단축키 표와 동일한 내용을 유지한다 */
const GROUPS: { title: string; rows: [string, string][] }[] = [
    {
        title: "파일과 탭",
        rows: [
            ["⌘O", "파일 열기 (여러 파일 선택 가능)"],
            ["⌘⇧O", "원격 파일 열기 (SFTP / S3)"],
            ["⌘W", "탭 닫기"],
            ["⌘⇧M", "열린 탭 로그 병합 (2~8개)"],
            ["⌘1 ~ ⌘8", "해당 순번 탭으로 이동"],
            ["⌘9", "마지막 탭으로 이동"],
            ["⌃Tab / ⌃⇧Tab", "다음 / 이전 탭"],
            ["⌘⇧[ / ⌘⇧]", "이전 / 다음 탭"],
        ],
    },
    {
        title: "검색과 필터",
        rows: [
            ["⌘F", "검색창 포커스"],
            ["⌘⇧F", "필터 규칙 입력창 포커스"],
            ["Enter / ⇧Enter", "다음 / 이전 검색 결과"],
        ],
    },
    {
        title: "이동과 보기",
        rows: [
            ["↑ ↓ / PageUp PageDown", "한 줄 / 한 화면 스크롤"],
            ["⌘↑ / ⌘↓ (Home / End)", "파일 처음 / 끝"],
            ["⌘L", "줄 번호로 이동"],
            ["⌘T", "tail 모드 토글"],
            ["⌘⇧C", "컬럼 뷰 토글 (CSV / JSONL)"],
            ["⌘= / ⌘- / ⌘0", "글자 크게 / 작게 / 기본 크기"],
            ["트랙패드 핀치", "글자 크기 조절"],
        ],
    },
    {
        title: "북마크",
        rows: [
            ["⌘B", "현재 줄 북마크 추가/제거"],
            ["F2 / ⇧F2", "다음 / 이전 북마크"],
            ["⌘⌥↓ / ⌘⌥↑", "다음 / 이전 북마크"],
            ["⌘⇧B", "북마크 목록 패널"],
            ["줄 번호 우클릭 / ⌥클릭", "북마크 토글"],
        ],
    },
    {
        title: "선택과 복사",
        rows: [
            ["줄 번호 클릭 / ⇧클릭", "줄 선택 / 범위 선택"],
            ["⌘A", "전체 줄 선택 (일반 보기)"],
            ["⌘C", "선택한 줄 복사"],
            ["Esc", "검색창 해제, 줄 선택 해제, 패널·모달 닫기"],
        ],
    },
    {
        title: "서버 측 가속 (에이전트)",
        rows: [
            [
                "켜기",
                "원격 열기(⌘⇧O) SSH 탭에서 호스트 선택 후 체크박스로 호스트별 opt-in (기본 꺼짐)",
            ],
            [
                "설치 위치",
                "원격 ~/.cache/riffle/ 에 실행 파일(약 2MB) 업로드 — Linux x86_64/aarch64만 지원",
            ],
            ["제거", "원격에서 rm -rf ~/.cache/riffle"],
        ],
    },
    {
        title: "기타",
        rows: [["⌘/", "이 단축키 도움말"]],
    },
];

/**
 * ⌘/ 단축키 도움말 모달. Esc 또는 배경 클릭으로 닫습니다.
 */
export const ShortcutsDialog: React.FC<ShortcutsDialogProps> = ({
    onClose,
}) => {
    useEffect(() => {
        const handleKeyDown = (e: KeyboardEvent) => {
            if (e.key === "Escape") {
                e.preventDefault();
                e.stopPropagation();
                onClose();
            }
        };
        window.addEventListener("keydown", handleKeyDown);
        return () => window.removeEventListener("keydown", handleKeyDown);
    }, [onClose]);

    return (
        <div
            role="dialog"
            aria-modal="true"
            aria-label="단축키 도움말"
            className="fixed inset-0 z-50 flex items-start justify-center pt-[10vh] bg-black/30 dark:bg-black/50"
            onMouseDown={(e) => {
                if (e.target === e.currentTarget) onClose();
            }}
        >
            <div className="w-[26rem] max-h-[70vh] overflow-y-auto rounded-lg border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-xl p-4">
                <div className="flex items-center justify-between mb-3">
                    <div className="text-sm font-semibold text-zinc-800 dark:text-zinc-200">
                        단축키
                    </div>
                    <button
                        type="button"
                        onClick={onClose}
                        className="px-2 py-0.5 text-xs rounded text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700 cursor-pointer"
                        title="닫기 (Esc)"
                    >
                        닫기
                    </button>
                </div>
                {GROUPS.map((group) => (
                    <div key={group.title} className="mb-3 last:mb-0">
                        <div className="text-[11px] font-medium text-zinc-500 dark:text-zinc-400 mb-1">
                            {group.title}
                        </div>
                        <table className="w-full text-xs">
                            <tbody>
                                {group.rows.map(([keys, desc]) => (
                                    <tr
                                        key={keys}
                                        className="border-t border-zinc-100 dark:border-zinc-800 first:border-t-0"
                                    >
                                        <td className="py-1 pr-3 font-mono text-zinc-700 dark:text-zinc-300 whitespace-nowrap w-[45%]">
                                            {keys}
                                        </td>
                                        <td className="py-1 text-zinc-600 dark:text-zinc-400">
                                            {desc}
                                        </td>
                                    </tr>
                                ))}
                            </tbody>
                        </table>
                    </div>
                ))}
            </div>
        </div>
    );
};
