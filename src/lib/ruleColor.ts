import type { FilterRule } from "./ipc";

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

/** 기존 규칙이 쓰지 않는 팔레트 색을 고른다 (모두 사용 중이면 순환) */
export function pickRuleColor(rules: FilterRule[]): string {
    const used = new Set(rules.map((r) => r.color));
    return (
        PALETTE.find((c) => !used.has(c)) ??
        PALETTE[rules.length % PALETTE.length]
    );
}
