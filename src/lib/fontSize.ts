export const DEFAULT_FONT_SIZE = 12;
export const MIN_FONT_SIZE = 9;
export const MAX_FONT_SIZE = 24;

const STORAGE_KEY = "riffle.fontSize";

/**
 * 폰트 크기를 허용 범위로 제한합니다.
 */
export function clampFontSize(size: number): number {
    return Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(size)));
}

/**
 * 폰트 크기에 대응하는 행 높이(px)를 계산합니다.
 */
export function rowHeightFor(fontSize: number): number {
    return Math.round(fontSize * 1.67);
}

/**
 * localStorage에 저장된 폰트 크기를 읽습니다. 실패 시 기본값을 반환합니다.
 */
export function loadFontSize(): number {
    try {
        const raw = window.localStorage.getItem(STORAGE_KEY);
        if (raw === null) return DEFAULT_FONT_SIZE;
        const size = Number(raw);
        return Number.isFinite(size) ? clampFontSize(size) : DEFAULT_FONT_SIZE;
    } catch {
        return DEFAULT_FONT_SIZE;
    }
}

/**
 * 폰트 크기를 localStorage에 저장합니다.
 */
export function saveFontSize(size: number): void {
    try {
        window.localStorage.setItem(STORAGE_KEY, String(size));
    } catch {
        // 저장 실패는 무시
    }
}
