// 앱 내 업데이트 확인/설치 래퍼 (@tauri-apps/plugin-updater, plugin-process)

import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

export { relaunch };
export type { DownloadEvent, Update } from "@tauri-apps/plugin-updater";

/**
 * 업데이트 엔드포인트에 새 버전이 있는지 확인한다.
 * 새 버전이 없으면 null을, 확인에 실패하면 예외를 던진다.
 * 네트워크가 느린 환경에서 다이얼로그가 무한 대기하지 않도록 타임아웃을 둔다.
 */
export function checkForUpdate() {
    return check({ timeout: 15000 });
}
