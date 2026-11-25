/**
 * 파일 개요(오류 분포 지도·타임라인) 표시 설정.
 * 두 설정 모두 모든 탭 공통이며 localStorage에 유지된다.
 */

const ERROR_MAP_KEY = "riffle.errorMap";
const TIMELINE_KEY = "riffle.timeline";

/** 스크롤바 오류 분포 지도 표시 여부를 읽습니다 (기본 true). */
export function loadErrorMapEnabled(): boolean {
    try {
        const raw = window.localStorage.getItem(ERROR_MAP_KEY);
        return raw === null ? true : raw === "1";
    } catch {
        return true;
    }
}

/** 스크롤바 오류 분포 지도 표시 여부를 저장합니다. */
export function saveErrorMapEnabled(enabled: boolean): void {
    try {
        window.localStorage.setItem(ERROR_MAP_KEY, enabled ? "1" : "0");
    } catch {
        // 저장 실패는 무시
    }
}

/** 타임라인 패널 표시 여부를 읽습니다 (기본 false). */
export function loadTimelineOpen(): boolean {
    try {
        const raw = window.localStorage.getItem(TIMELINE_KEY);
        return raw === "1";
    } catch {
        return false;
    }
}

/** 타임라인 패널 표시 여부를 저장합니다. */
export function saveTimelineOpen(open: boolean): void {
    try {
        window.localStorage.setItem(TIMELINE_KEY, open ? "1" : "0");
    } catch {
        // 저장 실패는 무시
    }
}
