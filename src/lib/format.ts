/**
 * 바이트 단위 파일 크기를 읽기 쉬운 단위 문자열로 변환합니다.
 */
export function formatBytes(bytes: number): string {
    if (bytes <= 0) return "0 B";
    if (bytes < 1024) return `${bytes} B`;

    const units = ["KB", "MB", "GB", "TB"];
    let u = -1;
    let value = bytes;

    while (value >= 1024 && u < units.length - 1) {
        value /= 1024;
        u++;
    }

    return `${value.toFixed(2)} ${units[u]}`;
}

/**
 * unix 초를 짧은 로컬 시각 문자열로 변환합니다.
 */
export function formatMtime(mtime: number | null): string {
    if (mtime === null) return "-";
    const d = new Date(mtime * 1000);
    if (Number.isNaN(d.getTime())) return "-";
    return d.toLocaleString("ko-KR", {
        month: "2-digit",
        day: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
    });
}
