import React, { useCallback, useEffect, useRef, useState } from "react";
import {
    deleteFilterPreset,
    listFilterPresets,
    saveFilterPreset,
    validateFilter,
    type Columns,
    type FilterPreset,
    type FilterRule,
    type FilterSpec,
    type LevelName,
    type RuleKind,
} from "@/lib/ipc";
import { pickRuleColor } from "@/lib/ruleColor";

interface FilterBarProps {
    spec: FilterSpec;
    onSpecChange: (spec: FilterSpec) => void;
    /** 증가할 때마다 규칙 입력창에 포커스 (네이티브 메뉴 "필터 규칙 추가" 이벤트용) */
    focusSignal?: number;
    /** 활성 탭 여부 — false이면 Cmd+Shift+F 포커스 단축키를 무시한다 */
    isActive?: boolean;
    /** 열린 파일 ID — 필드 규칙의 컬럼명 검증(validate_filter)에 사용 */
    fileId?: number | null;
    /** 감지된 컬럼 정보 — 있으면 규칙 입력에 "필드" 선택 드롭다운을 표시한다 */
    columns?: Columns | null;
}

/** 레벨 칩 정의. 색은 LogViewport의 로그 레벨 색상과 동일하게 맞춘다 */
const LEVELS: { id: LevelName; label: string; dotClass: string }[] = [
    { id: "error", label: "ERROR", dotClass: "bg-red-500" },
    { id: "warn", label: "WARN", dotClass: "bg-amber-500" },
    { id: "info", label: "INFO", dotClass: "bg-blue-500" },
    { id: "debug", label: "DEBUG", dotClass: "bg-zinc-400" },
    { id: "trace", label: "TRACE", dotClass: "bg-zinc-400" },
];
const ALL_LEVELS: LevelName[] = LEVELS.map((l) => l.id);

/** 규칙 색상 팔레트. 라이트/다크 배경 모두에서 반투명 하이라이트로 사용 가능해야 한다 */
const PALETTE = [
    "#ef4444",
    "#f97316",
    "#eab308",
    "#22c55e",
    "#14b8a6",
    "#3b82f6",
    "#a855f7",
    "#ec4899",
];

const KINDS: { id: RuleKind; label: string }[] = [
    { id: "include", label: "포함" },
    { id: "exclude", label: "제외" },
];

/** levels 배열을 칩 표시 순서로 정규화한다 */
function normalizeLevels(levels: LevelName[]): LevelName[] {
    return ALL_LEVELS.filter((l) => levels.includes(l));
}

/** 프리셋 스펙을 깊은 복사해 외부 참조와 분리한다 */
function cloneSpec(spec: FilterSpec): FilterSpec {
    return { ...spec, rules: spec.rules.map((r) => ({ ...r })) };
}

interface DraftRule {
    pattern: string;
    regex: boolean;
    caseSensitive: boolean;
    kind: RuleKind;
    /** 필드 필터 대상 컬럼 이름 (null이면 줄 전체) */
    field: string | null;
}

const EMPTY_DRAFT: DraftRule = {
    pattern: "",
    regex: false,
    caseSensitive: false,
    kind: "include",
    field: null,
};

/**
 * 상단 헤더 아래에 표시되는 다중 필터 바.
 * 레벨 칩, 규칙 칩(추가/수정/삭제/색상), AND/OR 전환, 프리셋 관리를 담당한다.
 */
export const FilterBar: React.FC<FilterBarProps> = React.memo(
    ({ spec, onSpecChange, focusSignal, isActive = true, fileId, columns }) => {
        const ruleInputRef = useRef<HTMLInputElement>(null);

        const [draft, setDraft] = useState<DraftRule>(EMPTY_DRAFT);
        const [editingIndex, setEditingIndex] = useState<number | null>(null);
        const [inputError, setInputError] = useState<string | null>(null);

        const [presetOpen, setPresetOpen] = useState(false);
        const [presets, setPresets] = useState<FilterPreset[]>([]);
        const [presetName, setPresetName] = useState("");
        const [presetError, setPresetError] = useState<string | null>(null);
        const [deleteConfirm, setDeleteConfirm] = useState<string | null>(null);

        const [colorPicker, setColorPicker] = useState<{
            ruleIndex: number;
            x: number;
            y: number;
        } | null>(null);

        // 입력창에 입력 중인 규칙을 반영한 후보 스펙을 만든다
        const buildCandidate = useCallback(
            (d: DraftRule): FilterSpec | null => {
                if (d.pattern.length === 0) return null;
                if (editingIndex !== null) {
                    const base = spec.rules[editingIndex];
                    if (base === undefined) return null;
                    const rule: FilterRule = {
                        ...base,
                        pattern: d.pattern,
                        regex: d.regex,
                        caseSensitive: d.caseSensitive,
                        kind: d.kind,
                        field: d.field,
                    };
                    return {
                        ...spec,
                        rules: spec.rules.map((r, i) =>
                            i === editingIndex ? rule : r,
                        ),
                    };
                }
                const rule: FilterRule = {
                    pattern: d.pattern,
                    regex: d.regex,
                    caseSensitive: d.caseSensitive,
                    kind: d.kind,
                    enabled: true,
                    color: pickRuleColor(spec.rules),
                    field: d.field,
                };
                return { ...spec, rules: [...spec.rules, rule] };
            },
            [spec, editingIndex],
        );

        // 정규식·필드 선택 중 실시간 유효성 검사 (리터럴 줄 규칙은 항상 유효).
        // 필드 규칙은 컬럼명 존재 여부도 검증되므로 fileId를 함께 넘긴다.
        const validateSeqRef = useRef(0);
        useEffect(() => {
            const candidate = buildCandidate(draft);
            if ((!draft.regex && draft.field === null) || candidate === null) {
                setInputError(null);
                return;
            }
            const seq = ++validateSeqRef.current;
            const timer = setTimeout(() => {
                validateFilter(fileId ?? null, candidate)
                    .then(() => {
                        if (validateSeqRef.current === seq) {
                            setInputError(null);
                        }
                    })
                    .catch((err: unknown) => {
                        if (validateSeqRef.current !== seq) return;
                        const msg =
                            typeof err === "string"
                                ? err
                                : err instanceof Error
                                  ? err.message
                                  : "잘못된 규칙입니다.";
                        setInputError(msg);
                    });
            }, 300);
            return () => clearTimeout(timer);
        }, [draft, buildCandidate, fileId]);

        const resetDraft = useCallback(() => {
            setDraft(EMPTY_DRAFT);
            setEditingIndex(null);
            setInputError(null);
        }, []);

        // Enter로 규칙 추가/수정 확정
        const submitDraft = useCallback(async () => {
            const candidate = buildCandidate(draft);
            if (candidate === null) return;
            if (draft.regex || draft.field !== null) {
                try {
                    await validateFilter(fileId ?? null, candidate);
                } catch (err) {
                    const msg =
                        typeof err === "string"
                            ? err
                            : err instanceof Error
                              ? err.message
                              : "잘못된 규칙입니다.";
                    setInputError(msg);
                    return;
                }
            }
            setInputError(null);
            onSpecChange(candidate);
            resetDraft();
            ruleInputRef.current?.focus();
        }, [buildCandidate, draft, onSpecChange, resetDraft, fileId]);

        const handleDraftKeyDown = (e: React.KeyboardEvent<HTMLElement>) => {
            if (e.key === "Enter") {
                e.preventDefault();
                void submitDraft();
            } else if (e.key === "Escape") {
                e.preventDefault();
                resetDraft();
            }
        };

        // 메뉴 "필터 규칙 추가" 이벤트(focusSignal)로 입력창 포커스
        const lastFocusSignalRef = useRef(focusSignal ?? 0);
        useEffect(() => {
            const signal = focusSignal ?? 0;
            if (signal === lastFocusSignalRef.current) return;
            lastFocusSignalRef.current = signal;
            ruleInputRef.current?.focus();
            ruleInputRef.current?.select();
        }, [focusSignal]);

        // Cmd+Shift+F: 규칙 입력창 포커스.
        // 참고: 네이티브 메뉴가 이 단축키를 accelerator로 가로채면 WKWebView까지
        // keydown이 도달하지 않을 수 있으므로, App의 메뉴 이벤트 경로(focusSignal)도 함께 둔다.
        useEffect(() => {
            const handler = (e: KeyboardEvent) => {
                // 숨겨진 탭(비활성)은 포커스 단축키를 처리하지 않는다
                if (!isActive) return;
                if (e.metaKey && e.shiftKey && e.code === "KeyF") {
                    e.preventDefault();
                    ruleInputRef.current?.focus();
                    ruleInputRef.current?.select();
                }
            };
            window.addEventListener("keydown", handler);
            return () => window.removeEventListener("keydown", handler);
        }, [isActive]);

        // ===== 프리셋 =====

        const refreshPresets = useCallback(() => {
            listFilterPresets()
                .then(setPresets)
                .catch((err) => {
                    console.warn("필터 프리셋 목록 조회 실패:", err);
                });
        }, []);

        useEffect(() => {
            if (presetOpen) {
                refreshPresets();
            }
        }, [presetOpen, refreshPresets]);

        const handleSavePreset = useCallback(async () => {
            const name = presetName.trim();
            if (name.length === 0) return;
            try {
                await saveFilterPreset(name, spec);
                setPresetName("");
                setPresetError(null);
                refreshPresets();
            } catch (err) {
                const msg =
                    typeof err === "string"
                        ? err
                        : err instanceof Error
                          ? err.message
                          : "프리셋 저장에 실패했습니다.";
                setPresetError(msg);
            }
        }, [presetName, spec, refreshPresets]);

        const handleLoadPreset = useCallback(
            (preset: FilterPreset) => {
                onSpecChange(cloneSpec(preset.spec));
                setPresetOpen(false);
            },
            [onSpecChange],
        );

        // 삭제는 2단계 확인 (첫 클릭 후 3초 안에 다시 클릭해야 삭제)
        const handleDeletePreset = useCallback(
            async (name: string) => {
                if (deleteConfirm !== name) {
                    setDeleteConfirm(name);
                    setTimeout(() => {
                        setDeleteConfirm((cur) => (cur === name ? null : cur));
                    }, 3000);
                    return;
                }
                setDeleteConfirm(null);
                try {
                    await deleteFilterPreset(name);
                    refreshPresets();
                } catch (err) {
                    console.warn("필터 프리셋 삭제 실패:", err);
                }
            },
            [deleteConfirm, refreshPresets],
        );

        // ===== 스펙 변경 헬퍼 =====

        const updateRule = useCallback(
            (index: number, patch: Partial<FilterRule>) => {
                onSpecChange({
                    ...spec,
                    rules: spec.rules.map((r, i) =>
                        i === index ? { ...r, ...patch } : r,
                    ),
                });
            },
            [spec, onSpecChange],
        );

        const handleLevelClick = useCallback(
            (level: LevelName, only: boolean) => {
                if (only) {
                    onSpecChange({ ...spec, levels: [level] });
                    return;
                }
                const levels = spec.levels.includes(level)
                    ? spec.levels.filter((l) => l !== level)
                    : normalizeLevels([...spec.levels, level]);
                onSpecChange({ ...spec, levels });
            },
            [spec, onSpecChange],
        );

        const handleClearAll = useCallback(() => {
            onSpecChange({
                rules: [],
                combine: spec.combine,
                levels: [...ALL_LEVELS],
            });
        }, [spec.combine, onSpecChange]);

        const startEditRule = useCallback(
            (index: number) => {
                const rule = spec.rules[index];
                if (rule === undefined) return;
                setDraft({
                    pattern: rule.pattern,
                    regex: rule.regex,
                    caseSensitive: rule.caseSensitive,
                    kind: rule.kind,
                    field: rule.field ?? null,
                });
                setEditingIndex(index);
                setInputError(null);
                ruleInputRef.current?.focus();
                ruleInputRef.current?.select();
            },
            [spec.rules],
        );

        const enabledIncludeCount = spec.rules.filter(
            (r) => r.enabled && r.kind === "include",
        ).length;
        const showCombine = enabledIncludeCount >= 2;

        return (
            <div className="relative shrink-0 flex items-stretch h-9 border-b border-zinc-200 dark:border-zinc-800 bg-zinc-50/60 dark:bg-zinc-900/60">
                {/* 스크롤 가능한 필터 컨트롤 영역 */}
                <div className="flex-1 flex items-center gap-2 px-3 min-w-0 overflow-x-auto">
                    {/* 로그 레벨 칩: 모두 선택 = 레벨 조건 없음 */}
                    <div
                        className="flex items-center gap-1 shrink-0"
                        title="로그 레벨 필터 — 클릭으로 토글, Option+클릭으로 이 레벨만"
                    >
                        {LEVELS.map((lv) => {
                            const selected = spec.levels.includes(lv.id);
                            return (
                                <button
                                    key={lv.id}
                                    type="button"
                                    onClick={(e) =>
                                        handleLevelClick(lv.id, e.altKey)
                                    }
                                    className={`h-5 px-1.5 rounded text-[10px] font-mono font-semibold flex items-center gap-1 cursor-pointer transition-opacity ${
                                        selected
                                            ? "text-zinc-700 dark:text-zinc-300"
                                            : "text-zinc-400 dark:text-zinc-600 opacity-50"
                                    }`}
                                    title={`${lv.label} — 클릭 토글, Option+클릭 이 레벨만`}
                                >
                                    <span
                                        className={`w-1.5 h-1.5 rounded-full ${lv.dotClass}`}
                                    />
                                    {lv.label}
                                </button>
                            );
                        })}
                    </div>

                    <div className="w-px h-4 bg-zinc-300 dark:bg-zinc-700 shrink-0" />

                    {/* 규칙 칩 + 입력 */}
                    <div className="flex items-center gap-1">
                        {showCombine && (
                            <div
                                className="flex rounded overflow-hidden border border-zinc-300 dark:border-zinc-700 shrink-0"
                                title="포함 규칙 결합 방식"
                            >
                                {(["and", "or"] as const).map((c) => (
                                    <button
                                        key={c}
                                        type="button"
                                        onClick={() =>
                                            onSpecChange({
                                                ...spec,
                                                combine: c,
                                            })
                                        }
                                        className={`h-5 px-1.5 text-[10px] font-mono font-bold cursor-pointer ${
                                            spec.combine === c
                                                ? "bg-blue-600 text-white"
                                                : "text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                                        }`}
                                    >
                                        {c.toUpperCase()}
                                    </button>
                                ))}
                            </div>
                        )}

                        {spec.rules.map((rule, index) => (
                            <span
                                key={index}
                                className={`h-5 pl-1 pr-0.5 rounded flex items-center gap-1 border border-zinc-300 dark:border-zinc-700 bg-white dark:bg-zinc-900 text-[11px] font-mono max-w-44 ${
                                    rule.enabled
                                        ? "text-zinc-700 dark:text-zinc-300"
                                        : "opacity-40"
                                }`}
                            >
                                <button
                                    type="button"
                                    onClick={(e) => {
                                        e.stopPropagation();
                                        const rect =
                                            e.currentTarget.getBoundingClientRect();
                                        setColorPicker({
                                            ruleIndex: index,
                                            x: rect.left,
                                            y: rect.bottom,
                                        });
                                    }}
                                    className="w-2.5 h-2.5 rounded-full shrink-0 cursor-pointer border border-black/10"
                                    style={{ backgroundColor: rule.color }}
                                    title="규칙 색상 변경"
                                />
                                <button
                                    type="button"
                                    onClick={() =>
                                        updateRule(index, {
                                            enabled: !rule.enabled,
                                        })
                                    }
                                    onDoubleClick={() => startEditRule(index)}
                                    className="truncate cursor-pointer"
                                    title={`${
                                        rule.kind === "exclude"
                                            ? "제외 규칙"
                                            : "포함 규칙"
                                    }${
                                        rule.field != null && rule.field !== ""
                                            ? ` [필드: ${rule.field}]`
                                            : ""
                                    }${rule.regex ? " (정규식)" : ""}${
                                        rule.caseSensitive
                                            ? " (대소문자 구분)"
                                            : ""
                                    } — 클릭 토글, 더블클릭 편집`}
                                >
                                    {rule.kind === "exclude" && "−"}
                                    {rule.field != null &&
                                        rule.field !== "" && (
                                            <span className="text-blue-600 dark:text-blue-400 shrink-0">
                                                {rule.field}=
                                            </span>
                                        )}
                                    <span
                                        className={
                                            rule.kind === "exclude"
                                                ? "line-through"
                                                : undefined
                                        }
                                    >
                                        {rule.pattern}
                                    </span>
                                </button>
                                <button
                                    type="button"
                                    onClick={() =>
                                        onSpecChange({
                                            ...spec,
                                            rules: spec.rules.filter(
                                                (_, i) => i !== index,
                                            ),
                                        })
                                    }
                                    className="text-zinc-400 hover:text-rose-500 cursor-pointer px-0.5"
                                    title="규칙 삭제"
                                >
                                    ×
                                </button>
                            </span>
                        ))}

                        {/* 규칙 입력 (추가/수정 공용) */}
                        <div className="relative flex items-center">
                            {editingIndex !== null && (
                                <span className="text-[10px] text-zinc-400 mr-0.5 shrink-0">
                                    수정:
                                </span>
                            )}
                            <input
                                ref={ruleInputRef}
                                type="text"
                                value={draft.pattern}
                                onChange={(e) =>
                                    setDraft((d) => ({
                                        ...d,
                                        pattern: e.target.value,
                                    }))
                                }
                                onKeyDown={handleDraftKeyDown}
                                placeholder={
                                    editingIndex !== null
                                        ? "규칙 수정 후 Enter"
                                        : "규칙 추가 (⌘⇧F)"
                                }
                                className={`h-6 px-1.5 text-xs font-mono bg-white dark:bg-zinc-900 border rounded focus:outline-none w-36 text-zinc-900 dark:text-zinc-100 transition-colors ${
                                    inputError
                                        ? "border-rose-500 focus:ring-1 focus:ring-rose-500"
                                        : "border-zinc-300 dark:border-zinc-700 focus:ring-1 focus:ring-blue-500"
                                }`}
                            />
                            {draft.pattern.length > 0 &&
                                editingIndex === null && (
                                    <button
                                        type="button"
                                        onClick={() =>
                                            setDraft((d) => ({
                                                ...d,
                                                pattern: "",
                                            }))
                                        }
                                        className="absolute right-1.5 text-zinc-400 hover:text-zinc-600 dark:hover:text-zinc-200 cursor-pointer text-xs"
                                        title="입력 지우기"
                                    >
                                        ✕
                                    </button>
                                )}
                        </div>

                        {/* 필드 선택 드롭다운: 컬럼이 감지된 파일에서만 표시 (전체 줄 / 각 컬럼) */}
                        {columns !== null &&
                            columns !== undefined &&
                            columns.names.length > 0 && (
                                <select
                                    value={draft.field ?? ""}
                                    onChange={(e) =>
                                        setDraft((d) => ({
                                            ...d,
                                            field:
                                                e.target.value === ""
                                                    ? null
                                                    : e.target.value,
                                        }))
                                    }
                                    onKeyDown={handleDraftKeyDown}
                                    className={`h-6 px-1 text-[10px] font-mono bg-white dark:bg-zinc-900 border rounded focus:outline-none text-zinc-700 dark:text-zinc-300 cursor-pointer max-w-32 ${
                                        draft.field !== null
                                            ? "border-blue-500 text-blue-600 dark:text-blue-400"
                                            : "border-zinc-300 dark:border-zinc-700"
                                    }`}
                                    title="필드 — 규칙을 특정 컬럼 값에만 적용합니다"
                                >
                                    <option value="">전체 줄</option>
                                    {columns.names.map((name) => (
                                        <option key={name} value={name}>
                                            {name}
                                        </option>
                                    ))}
                                </select>
                            )}

                        {/* 정규식 토글 (.*) */}
                        <button
                            type="button"
                            onClick={() =>
                                setDraft((d) => ({ ...d, regex: !d.regex }))
                            }
                            className={`h-5 px-1 text-[10px] font-mono rounded cursor-pointer transition-colors ${
                                draft.regex
                                    ? "bg-blue-600 text-white font-bold"
                                    : "text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                            }`}
                            title="정규식 (.*)"
                        >
                            .*
                        </button>

                        {/* 대소문자 구분 토글 (Aa) */}
                        <button
                            type="button"
                            onClick={() =>
                                setDraft((d) => ({
                                    ...d,
                                    caseSensitive: !d.caseSensitive,
                                }))
                            }
                            className={`h-5 px-1 text-[10px] font-mono rounded cursor-pointer transition-colors ${
                                draft.caseSensitive
                                    ? "bg-blue-600 text-white font-bold"
                                    : "text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                            }`}
                            title="대소문자 구분 (Aa)"
                        >
                            Aa
                        </button>

                        {/* 포함/제외 세그먼트 */}
                        <div className="flex rounded overflow-hidden border border-zinc-300 dark:border-zinc-700 shrink-0">
                            {KINDS.map((k) => (
                                <button
                                    key={k.id}
                                    type="button"
                                    onClick={() =>
                                        setDraft((d) => ({ ...d, kind: k.id }))
                                    }
                                    className={`h-5 px-1.5 text-[10px] cursor-pointer ${
                                        draft.kind === k.id
                                            ? k.id === "include"
                                                ? "bg-emerald-600 text-white font-semibold"
                                                : "bg-rose-600 text-white font-semibold"
                                            : "text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                                    }`}
                                    title={
                                        k.id === "include"
                                            ? "매치된 줄을 보이게 하는 포함 규칙"
                                            : "매치된 줄을 숨기는 제외 규칙"
                                    }
                                >
                                    {k.label}
                                </button>
                            ))}
                        </div>
                    </div>
                </div>

                {/* 우측 고정 영역: 모두 해제 + 프리셋 */}
                <div className="flex items-center gap-1 px-2 shrink-0 border-l border-zinc-200 dark:border-zinc-800">
                    <button
                        type="button"
                        onClick={handleClearAll}
                        disabled={spec.rules.length === 0}
                        className="h-5 px-1.5 text-[10px] text-zinc-500 dark:text-zinc-400 hover:text-zinc-800 dark:hover:text-zinc-200 rounded cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                        title="모든 규칙과 레벨 조건 해제"
                    >
                        모두 해제
                    </button>
                    <button
                        type="button"
                        onClick={() => setPresetOpen((v) => !v)}
                        className={`h-5 px-1.5 text-[10px] rounded cursor-pointer ${
                            presetOpen
                                ? "bg-blue-600 text-white"
                                : "text-zinc-500 dark:text-zinc-400 hover:bg-zinc-200 dark:hover:bg-zinc-700"
                        }`}
                        title="필터 프리셋 저장/불러오기"
                    >
                        프리셋
                    </button>
                </div>

                {/* 규칙 입력 에러 팝오버 (검색바와 동일한 스타일) */}
                {inputError && (
                    <div
                        className="absolute top-full left-24 mt-1 z-50 max-w-md px-2.5 py-1 text-[11px] font-mono text-rose-600 dark:text-rose-300 bg-white dark:bg-zinc-900 border border-rose-500 rounded-md shadow-lg truncate pointer-events-auto"
                        title={inputError}
                    >
                        <span className="font-semibold text-rose-500 mr-1">
                            오류:
                        </span>
                        {inputError}
                    </div>
                )}

                {/* 프리셋 팝오버 */}
                {presetOpen && (
                    <>
                        <div
                            className="fixed inset-0 z-40"
                            onClick={() => setPresetOpen(false)}
                        />
                        <div className="absolute right-2 top-full mt-1 z-50 w-64 rounded-lg border border-zinc-200 dark:border-zinc-700 bg-white dark:bg-zinc-900 shadow-lg p-2">
                            <div className="flex items-center gap-1 mb-2">
                                <input
                                    type="text"
                                    value={presetName}
                                    onChange={(e) =>
                                        setPresetName(e.target.value)
                                    }
                                    onKeyDown={(e) => {
                                        if (e.key === "Enter") {
                                            e.preventDefault();
                                            void handleSavePreset();
                                        }
                                    }}
                                    placeholder="현재 필터를 프리셋으로 저장"
                                    className="flex-1 h-6 px-1.5 text-xs bg-zinc-50 dark:bg-zinc-800 border border-zinc-300 dark:border-zinc-700 rounded focus:outline-none focus:ring-1 focus:ring-blue-500 text-zinc-900 dark:text-zinc-100"
                                />
                                <button
                                    type="button"
                                    onClick={() => void handleSavePreset()}
                                    disabled={presetName.trim().length === 0}
                                    className="h-6 px-2 text-[11px] rounded bg-zinc-900 text-white dark:bg-zinc-100 dark:text-zinc-900 hover:opacity-90 cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                                >
                                    저장
                                </button>
                            </div>
                            {presetError && (
                                <div className="text-[10px] text-rose-500 mb-1 px-1">
                                    {presetError}
                                </div>
                            )}
                            <div className="max-h-44 overflow-y-auto">
                                {presets.length === 0 && (
                                    <div className="text-[11px] text-zinc-400 px-1.5 py-2">
                                        저장된 프리셋이 없습니다
                                    </div>
                                )}
                                {presets.map((preset) => (
                                    <div
                                        key={preset.name}
                                        className="flex items-center group"
                                    >
                                        <button
                                            type="button"
                                            onClick={() =>
                                                handleLoadPreset(preset)
                                            }
                                            className="flex-1 min-w-0 text-left px-1.5 py-1 text-xs rounded hover:bg-zinc-100 dark:hover:bg-zinc-800 cursor-pointer"
                                            title={`${preset.name} — 클릭하여 불러오기`}
                                        >
                                            <span className="text-zinc-800 dark:text-zinc-200 truncate block">
                                                {preset.name}
                                            </span>
                                            <span className="text-[10px] text-zinc-400">
                                                규칙 {preset.spec.rules.length}
                                                개
                                                {preset.spec.levels.length < 5
                                                    ? ` · 레벨 ${preset.spec.levels.length}개`
                                                    : ""}
                                            </span>
                                        </button>
                                        <button
                                            type="button"
                                            onClick={() =>
                                                void handleDeletePreset(
                                                    preset.name,
                                                )
                                            }
                                            className={`px-1.5 py-1 text-[10px] rounded cursor-pointer shrink-0 ${
                                                deleteConfirm === preset.name
                                                    ? "text-rose-600 dark:text-rose-400 font-bold"
                                                    : "text-zinc-400 hover:text-rose-500 opacity-0 group-hover:opacity-100"
                                            }`}
                                            title={
                                                deleteConfirm === preset.name
                                                    ? "한 번 더 클릭하면 삭제"
                                                    : "프리셋 삭제"
                                            }
                                        >
                                            {deleteConfirm === preset.name
                                                ? "삭제 확인"
                                                : "×"}
                                        </button>
                                    </div>
                                ))}
                            </div>
                        </div>
                    </>
                )}

                {/* 규칙 색상 팔레트 팝오버 */}
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
                            {PALETTE.map((color) => (
                                <button
                                    key={color}
                                    type="button"
                                    onClick={() => {
                                        updateRule(colorPicker.ruleIndex, {
                                            color,
                                        });
                                        setColorPicker(null);
                                    }}
                                    className={`w-4 h-4 rounded-full cursor-pointer border ${
                                        spec.rules[colorPicker.ruleIndex]
                                            ?.color === color
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
            </div>
        );
    },
);

FilterBar.displayName = "FilterBar";
