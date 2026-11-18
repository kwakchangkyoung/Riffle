import React, { useState } from "react";
import type { JsonValue } from "@/lib/jsonLine";

interface JsonTreeProps {
    value: JsonValue;
    /** 값 클릭 시 복사할 텍스트를 부모에 전달 (클립보드 기록 + 토스트는 부모가 처리) */
    onCopy: (text: string) => void;
}

interface JsonNodeProps {
    /** 객체 키 이름 (루트/배열 원소는 undefined, 배열 원소는 index를 nameIdx로 전달) */
    name?: string;
    /** 배열 원소의 순번 (undefined면 배열 원소가 아님) */
    index?: number;
    value: JsonValue;
    depth: number;
    onCopy: (text: string) => void;
}

function scalarClass(v: JsonValue): string {
    if (typeof v === "string") {
        return "text-emerald-700 dark:text-emerald-400";
    }
    if (typeof v === "number") {
        return "text-teal-600 dark:text-teal-400";
    }
    if (typeof v === "boolean") {
        return "text-orange-600 dark:text-orange-400";
    }
    return "text-zinc-400 dark:text-zinc-500 italic";
}

function scalarText(v: JsonValue): string {
    if (typeof v === "string") return `"${v}"`;
    if (v === null) return "null";
    return String(v);
}

/** 값의 클립보드 복사 텍스트: 문자열은 내용만, 컨테이너는 들여쓴 JSON */
function copyTextOf(v: JsonValue): string {
    if (v !== null && typeof v === "object") {
        return JSON.stringify(v, null, 2);
    }
    if (typeof v === "string") return v;
    return String(v);
}

/** 객체/배열 노드의 접힌 상태 요약 */
function summaryOf(v: JsonValue): string {
    if (Array.isArray(v)) return `[${v.length.toLocaleString("ko-KR")}개]`;
    if (v !== null && typeof v === "object") {
        return `{${Object.keys(v).length.toLocaleString("ko-KR")}개}`;
    }
    return "";
}

const JsonNode: React.FC<JsonNodeProps> = ({
    name,
    index,
    value,
    depth,
    onCopy,
}) => {
    // 1단계(루트)까지만 기본 펼침
    const [open, setOpen] = useState(depth === 0);
    const isContainer = value !== null && typeof value === "object";

    const keyNode =
        name !== undefined ? (
            <span className="text-violet-600 dark:text-violet-400">
                "{name}"
            </span>
        ) : index !== undefined ? (
            <span className="text-zinc-400 dark:text-zinc-500">{index}</span>
        ) : null;
    const keyPrefix = keyNode !== null ? <>{keyNode}: </> : null;

    if (!isContainer) {
        return (
            <button
                type="button"
                onClick={(e) => {
                    e.stopPropagation();
                    onCopy(copyTextOf(value));
                }}
                title="클릭하면 값을 복사합니다"
                className="block text-left cursor-copy hover:bg-zinc-200/60 dark:hover:bg-zinc-700/40 rounded-xs px-0.5 -mx-0.5"
            >
                {keyPrefix}
                <span className={scalarClass(value)}>{scalarText(value)}</span>
            </button>
        );
    }

    const entries: [string | undefined, number | undefined, JsonValue][] =
        Array.isArray(value)
            ? value.map((v, i) => [undefined, i, v])
            : Object.entries(value).map(([k, v]) => [k, undefined, v]);

    const bracket = Array.isArray(value) ? ["[", "]"] : ["{", "}"];

    return (
        <div>
            <button
                type="button"
                onClick={() => setOpen((o) => !o)}
                title={
                    open ? "접기 (값 클릭 시 복사)" : "펼치기 (값 클릭 시 복사)"
                }
                className="block text-left cursor-pointer hover:bg-zinc-200/60 dark:hover:bg-zinc-700/40 rounded-xs px-0.5 -mx-0.5"
            >
                <span className="inline-block w-3 text-zinc-400 dark:text-zinc-500">
                    {open ? "▾" : "▸"}
                </span>
                {keyPrefix}
                <span className="text-zinc-500 dark:text-zinc-400">
                    {bracket[0]}
                </span>
                {!open && (
                    <span
                        className="text-zinc-400 dark:text-zinc-500"
                        onClick={(e) => {
                            e.stopPropagation();
                            onCopy(copyTextOf(value));
                        }}
                    >
                        {" "}
                        {summaryOf(value)} {bracket[1]}
                    </span>
                )}
            </button>
            {open && (
                <>
                    <div className="pl-4 border-l border-zinc-200 dark:border-zinc-700 ml-1">
                        {entries.map(([k, i, v], pos) => (
                            <JsonNode
                                key={pos}
                                name={k}
                                index={i}
                                value={v}
                                depth={depth + 1}
                                onCopy={onCopy}
                            />
                        ))}
                    </div>
                    <div className="text-zinc-500 dark:text-zinc-400">
                        {bracket[1]}
                    </div>
                </>
            )}
        </div>
    );
};

/**
 * JSON 값을 접을 수 있는 트리로 표시합니다.
 * 루트만 기본 펼침, 값 클릭 시 부모 콜백으로 복사 텍스트를 전달합니다.
 */
export const JsonTree: React.FC<JsonTreeProps> = ({ value, onCopy }) => {
    return (
        <div className="p-3 font-mono text-xs whitespace-pre-wrap break-all text-zinc-900 dark:text-zinc-100 select-text">
            <JsonNode value={value} depth={0} onCopy={onCopy} />
        </div>
    );
};
