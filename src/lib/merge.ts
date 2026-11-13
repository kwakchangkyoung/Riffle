// 로그 시간순 병합 뷰(Phase 17) 프론트엔드 헬퍼

/** 병합 가상 파일 경로 접두사 (open_merge가 반환하는 FileInfo.path 형식) */
export const MERGE_SCHEME = "merge://";

/** 경로가 병합 가상 파일인지 확인합니다. */
export function isMergePath(path: string): boolean {
    return path.startsWith(MERGE_SCHEME);
}

/** 병합 원본 순번(index)별 고정 색상 팔레트 (#rrggbb) */
const SOURCE_COLORS = [
    "#2563eb", // 파랑
    "#059669", // 초록
    "#d97706", // 주황
    "#dc2626", // 빨강
    "#7c3aed", // 보라
    "#0891b2", // 청록
    "#db2777", // 분홍
    "#65a30d", // 연두
] as const;

/** 원본 순번의 고정 색상을 반환합니다. */
export function mergeSourceColor(index: number): string {
    return SOURCE_COLORS[index % SOURCE_COLORS.length];
}

/**
 * 경로의 표시 이름을 반환합니다 (마지막 경로 조각).
 * 원격 URI("sftp://host/dir/f.log", "s3://bucket/key")도 동일하게 동작합니다.
 */
export function baseNameOf(path: string): string {
    const trimmed = path.endsWith("/") ? path.slice(0, -1) : path;
    const last = trimmed.split("/").pop();
    return last !== undefined && last.length > 0 ? last : path;
}

/**
 * 원본 칩에 표시할 짧은 이름 (파일명 앞 12자, 길면 말줄임).
 */
export function mergeShortName(path: string): string {
    const name = baseNameOf(path);
    return name.length > 12 ? `${name.slice(0, 11)}…` : name;
}

/**
 * 병합 탭 제목. 원본이 2개까지면 "병합: a.log + b.log",
 * 그 이상이면 "병합: a.log 외 N개"로 줄입니다.
 */
export function mergeTabTitle(paths: string[]): string {
    const names = paths.map(baseNameOf);
    if (names.length <= 2) {
        return `병합: ${names.join(" + ")}`;
    }
    return `병합: ${names[0]} 외 ${names.length - 1}개`;
}

/**
 * Cmd+L 이동 대화상자의 입력을 해석합니다.
 * 순수 숫자면 줄 번호(1-based), 아니면 시각 문자열로 파싱합니다.
 * 시각은 로컬 시간대로 해석하며, 인식 형식:
 *   "2026-11-12 07:15:42[.SSS]", "2026-11-12T07:15:42[.SSS][Z|±hh:mm]",
 *   "2026-11-12", "07:15[:42]" (오늘 날짜, 초 생략 시 0초)
 */
export function parseGotoInput(
    input: string,
): { kind: "line"; line: number } | { kind: "time"; tsMs: number } | null {
    const trimmed = input.trim();
    if (trimmed.length === 0) return null;
    if (/^\d+$/.test(trimmed)) {
        return { kind: "line", line: Number(trimmed) };
    }

    // 날짜(+시각) 형식. 시간대 표기가 없으면 로컬 시간대로 해석한다
    const dateTime = trimmed.match(
        /^(\d{4})-(\d{1,2})-(\d{1,2})(?:[T ](\d{1,2}):(\d{1,2})(?::(\d{1,2})(?:[.,](\d{1,9}))?)?)?(?:\s*(Z|[+-]\d{2}:?\d{2}))?$/,
    );
    if (dateTime !== null) {
        const [, y, mo, d, hh, mm, ss, frac, tz] = dateTime;
        const ms =
            frac !== undefined ? Number(frac.padEnd(3, "0").slice(0, 3)) : 0;
        if (tz === undefined) {
            const t = new Date(
                Number(y),
                Number(mo) - 1,
                Number(d),
                hh !== undefined ? Number(hh) : 0,
                mm !== undefined ? Number(mm) : 0,
                ss !== undefined ? Number(ss) : 0,
                ms,
            );
            return Number.isNaN(t.getTime())
                ? null
                : { kind: "time", tsMs: t.getTime() };
        }
        const utcMs = Date.UTC(
            Number(y),
            Number(mo) - 1,
            Number(d),
            hh !== undefined ? Number(hh) : 0,
            mm !== undefined ? Number(mm) : 0,
            ss !== undefined ? Number(ss) : 0,
            ms,
        );
        let offsetMs = 0;
        if (tz !== "Z") {
            const sign = tz.startsWith("-") ? -1 : 1;
            const digits = tz.slice(1).replace(":", "");
            offsetMs =
                sign *
                (Number(digits.slice(0, 2)) * 60 + Number(digits.slice(2))) *
                60_000;
        }
        return { kind: "time", tsMs: utcMs - offsetMs };
    }

    // 시각만 (오늘 날짜, 로컬)
    const timeOnly = trimmed.match(/^(\d{1,2}):(\d{2})(?::(\d{1,2}))?$/);
    if (timeOnly !== null) {
        const now = new Date();
        const t = new Date(
            now.getFullYear(),
            now.getMonth(),
            now.getDate(),
            Number(timeOnly[1]),
            Number(timeOnly[2]),
            timeOnly[3] !== undefined ? Number(timeOnly[3]) : 0,
            0,
        );
        return Number.isNaN(t.getTime())
            ? null
            : { kind: "time", tsMs: t.getTime() };
    }

    return null;
}
