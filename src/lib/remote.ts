// 원격(sftp://) 및 S3(s3://) 경로 판별·추출 헬퍼

/** SFTP 원격 파일 URI 접두사 */
export const REMOTE_SCHEME = "sftp://";
/** S3 객체 URI 접두사 */
export const S3_SCHEME = "s3://";

/**
 * 경로가 원격(sftp:// 또는 s3://) URI인지 확인합니다.
 */
export function isRemotePath(path: string): boolean {
    return isSftpPath(path) || isS3Path(path);
}

/**
 * 경로가 SFTP 원격 URI인지 확인합니다.
 */
export function isSftpPath(path: string): boolean {
    return path.startsWith(REMOTE_SCHEME);
}

/**
 * 경로가 S3 객체 URI인지 확인합니다.
 */
export function isS3Path(path: string): boolean {
    return path.startsWith(S3_SCHEME);
}

/**
 * SFTP 원격 URI에서 호스트 부분을 추출합니다. SFTP URI가 아니거나 파싱 불가면 null.
 * 예: "sftp://myhost/var/log/app.log" → "myhost"
 */
export function remoteHostOf(path: string): string | null {
    if (!isSftpPath(path)) return null;
    const rest = path.slice(REMOTE_SCHEME.length);
    const slash = rest.indexOf("/");
    const host = slash >= 0 ? rest.slice(0, slash) : rest;
    return host.length > 0 ? host : null;
}

/**
 * S3 URI에서 버킷 이름을 추출합니다. S3 URI가 아니거나 파싱 불가면 null.
 * 예: "s3://my-bucket/logs/app.log" → "my-bucket"
 */
export function s3BucketOf(path: string): string | null {
    if (!isS3Path(path)) return null;
    const rest = path.slice(S3_SCHEME.length);
    const slash = rest.indexOf("/");
    const bucket = slash >= 0 ? rest.slice(0, slash) : rest;
    return bucket.length > 0 ? bucket : null;
}

/**
 * 호스트와 원격 절대 경로로 sftp:// URI를 만듭니다.
 * 경로가 "/"로 시작하지 않으면 "/"를 붙입니다.
 */
export function remoteUriOf(host: string, path: string): string {
    const normalized = path.startsWith("/") ? path : `/${path}`;
    return `${REMOTE_SCHEME}${host}${normalized}`;
}

/**
 * 버킷과 객체 키로 s3:// URI를 만듭니다.
 */
export function s3UriOf(bucket: string, key: string): string {
    return `${S3_SCHEME}${bucket}/${key}`;
}
