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
 * S3 URI의 첫 "/" 이전 구간(버킷 또는 "<연결id>@버킷")을 반환합니다.
 */
function s3AuthorityOf(path: string): string | null {
    if (!isS3Path(path)) return null;
    const rest = path.slice(S3_SCHEME.length);
    const slash = rest.indexOf("/");
    const authority = slash >= 0 ? rest.slice(0, slash) : rest;
    return authority.length > 0 ? authority : null;
}

/**
 * S3 URI에서 저장된 연결 id를 추출합니다. 연결 URI가 아니면 null.
 * 예: "s3://c0123abcd@my-bucket/logs/app.log" → "c0123abcd"
 * (버킷 이름에는 '@'가 올 수 없으므로 첫 '/' 이전의 '@'는 연결 구분자다)
 */
export function s3ConnectionIdOf(path: string): string | null {
    const authority = s3AuthorityOf(path);
    if (authority === null) return null;
    const at = authority.indexOf("@");
    if (at <= 0) return null;
    return authority.slice(0, at);
}

/**
 * S3 URI에서 버킷 이름을 추출합니다. S3 URI가 아니거나 파싱 불가면 null.
 * 예: "s3://my-bucket/logs/app.log" → "my-bucket"
 * 예: "s3://conn-id@my-bucket/logs/app.log" → "my-bucket"
 */
export function s3BucketOf(path: string): string | null {
    const authority = s3AuthorityOf(path);
    if (authority === null) return null;
    const at = authority.indexOf("@");
    const bucket = at >= 0 ? authority.slice(at + 1) : authority;
    return bucket.length > 0 ? bucket : null;
}

/**
 * 표시용으로 S3 URI의 "<연결id>@" 부분을 제거한 경로를 반환합니다.
 * 연결 id를 알아도 의미가 없으므로 툴팁·제목에는 숨긴다.
 * 예: "s3://c0123abcd@my-bucket/app.log" → "s3://my-bucket/app.log"
 */
export function s3HideConn(path: string): string {
    const conn = s3ConnectionIdOf(path);
    if (conn === null) return path;
    return path.replace(`${S3_SCHEME}${conn}@`, S3_SCHEME);
}

/**
 * 표시용 S3 경로: "<연결id>@"를 연결 이름으로 치환하고,
 * 이름을 모르면 id 부분을 숨긴다.
 * 예: "s3://c0123abcd@my-bucket/app.log" → "s3://prod@my-bucket/app.log"
 */
export function s3DisplayPath(path: string, connName: string | null): string {
    const conn = s3ConnectionIdOf(path);
    if (conn === null) return path;
    const head =
        connName !== null && connName.length > 0
            ? `${S3_SCHEME}${connName}@`
            : S3_SCHEME;
    return path.replace(`${S3_SCHEME}${conn}@`, head);
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

/**
 * 저장된 연결 id·버킷·객체 키로 "s3://<연결id>@버킷/키" URI를 만듭니다.
 */
export function s3ConnUriOf(
    connId: string,
    bucket: string,
    key: string,
): string {
    return `${S3_SCHEME}${connId}@${bucket}/${key}`;
}
