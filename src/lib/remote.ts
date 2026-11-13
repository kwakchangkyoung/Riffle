// 원격(sftp://) 경로 판별 및 호스트 추출 헬퍼

/** 원격 파일 URI 접두사 */
export const REMOTE_SCHEME = "sftp://";

/**
 * 경로가 원격(sftp://) URI인지 확인합니다.
 */
export function isRemotePath(path: string): boolean {
    return path.startsWith(REMOTE_SCHEME);
}

/**
 * 원격 URI에서 호스트 부분을 추출합니다. 로컬 경로이거나 파싱 불가면 null.
 * 예: "sftp://myhost/var/log/app.log" → "myhost"
 */
export function remoteHostOf(path: string): string | null {
    if (!isRemotePath(path)) return null;
    const rest = path.slice(REMOTE_SCHEME.length);
    const slash = rest.indexOf("/");
    const host = slash >= 0 ? rest.slice(0, slash) : rest;
    return host.length > 0 ? host : null;
}

/**
 * 호스트와 원격 절대 경로로 sftp:// URI를 만듭니다.
 * 경로가 "/"로 시작하지 않으면 "/"를 붙입니다.
 */
export function remoteUriOf(host: string, path: string): string {
    const normalized = path.startsWith("/") ? path : `/${path}`;
    return `${REMOTE_SCHEME}${host}${normalized}`;
}
