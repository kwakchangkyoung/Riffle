//! 원격 접속 오류 타입과 오류 분류.

use std::io;

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;

/// 원격 파일 접근 중 발생하는 오류.
#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    /// ssh 가 호스트 키를 확인할 수 없어 접속을 거부했다.
    #[error(
        "호스트 키가 확인되지 않았습니다. 터미널에서 `ssh {0}` 로 한 번 접속해 호스트 키를 등록하세요."
    )]
    HostKeyUnknown(String),
    /// 공개키 인증에 실패했다.
    #[error(
        "인증에 실패했습니다. 키에 암호가 있다면 `ssh-add --apple-use-keychain <키 파일>` 로 agent 에 등록하세요. ({0})"
    )]
    AuthFailed(String),
    /// ssh 프로세스가 stderr 메시지와 함께 종료했다.
    #[error("연결 실패: {0}")]
    Connection(String),
    /// 연결 수립이 제한 시간을 넘겼다.
    #[error("연결 시간 초과 ({0}초)")]
    Timeout(u64),
    /// 원격 경로가 존재하지 않는다.
    #[error("파일을 찾을 수 없습니다: {0}")]
    NotFound(String),
    /// 원격 측 권한 부족.
    #[error("권한이 없습니다: {0}")]
    PermissionDenied(String),
    /// SFTP 프로토콜 수준의 오류.
    #[error("SFTP 오류: {0}")]
    Sftp(String),
    /// S3 접근 권한 부족 (HTTP 403).
    #[error("권한이 없습니다(403). 프로필/버킷 정책을 확인하세요: {0}")]
    S3AccessDenied(String),
    /// S3 버킷·객체가 없다 (HTTP 404).
    #[error("찾을 수 없습니다: {0}")]
    S3NotFound(String),
    /// AWS 자격 증명을 찾지 못했다.
    #[error(
        "자격 증명을 찾을 수 없습니다. `aws login` 또는 `aws sso login --profile …` 후 다시 시도하세요 ({0})"
    )]
    S3Credentials(String),
    /// 그 외 S3 오류 요약.
    #[error("S3 오류: {0}")]
    S3(String),
    /// 원격 에이전트 준비·통신 오류. 호출자는 항상 SFTP 방식으로 폴백하면 된다.
    #[error("에이전트를 사용할 수 없습니다: {0}")]
    Agent(String),
    /// 그 외 I/O 오류.
    #[error("I/O 오류: {0}")]
    Io(#[from] io::Error),
}

/// ssh 의 stderr 내용을 보고 접속 실패 원인을 분류한다.
///
/// `stderr` 는 수집된 stderr 전체(또는 꼬리)다.
pub(crate) fn classify_ssh_failure(host: &str, stderr: &str) -> RemoteError {
    if stderr.contains("Host key verification failed") {
        return RemoteError::HostKeyUnknown(host.to_string());
    }
    if stderr.contains("Too many authentication failures") || stderr.contains("Permission denied") {
        return RemoteError::AuthFailed(summarize_stderr(stderr));
    }
    RemoteError::Connection(summarize_stderr(stderr))
}

/// stderr 에서 사용자에게 보여줄 한 줄 요약을 뽑는다.
/// 의미 있는 마지막 줄을 우선 사용하고, 없으면 일반 문구를 반환한다.
fn summarize_stderr(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match lines.last() {
        Some(line) => truncate(line, 200),
        None => "알 수 없는 연결 오류".to_string(),
    }
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// SFTP 요청 오류가 연결 단절성 오류인지 판별한다.
/// true 이면 재연결 후 재시도가 의미가 있다.
pub(crate) fn sftp_is_conn_error(e: &SftpError) -> bool {
    match e {
        SftpError::IO(_) | SftpError::UnexpectedBehavior(_) => true,
        SftpError::Status(status) => matches!(
            status.status_code,
            StatusCode::NoConnection | StatusCode::ConnectionLost
        ),
        _ => false,
    }
}

/// 요청 시간 초과는 연결 자체는 살아 있을 수 있어 재연결 없이 재시도한다.
pub(crate) fn sftp_is_timeout(e: &SftpError) -> bool {
    matches!(e, SftpError::Timeout)
}

/// SFTP 오류를 RemoteError 로 변환한다. `path` 는 NotFound/PermissionDenied 메시지에 쓴다.
pub(crate) fn map_sftp_error(e: SftpError, path: &str) -> RemoteError {
    match e {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => RemoteError::NotFound(path.to_string()),
            StatusCode::PermissionDenied => RemoteError::PermissionDenied(path.to_string()),
            StatusCode::NoConnection | StatusCode::ConnectionLost => {
                RemoteError::Connection(format!("{}: {}", status.status_code, status.error_message))
            }
            _ => RemoteError::Sftp(format!("{}: {}", status.status_code, status.error_message)),
        },
        SftpError::Timeout => RemoteError::Sftp("SFTP 요청 시간 초과".to_string()),
        SftpError::IO(msg) | SftpError::UnexpectedBehavior(msg) => RemoteError::Connection(msg),
        other => RemoteError::Sftp(other.to_string()),
    }
}

/// RandomRead 구현체가 io::Result 로 반환해야 할 때 쓰는 SFTP 오류 → io::Error 변환.
pub(crate) fn sftp_to_io(e: SftpError) -> io::Error {
    match e {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => io::Error::new(io::ErrorKind::NotFound, status.error_message),
            StatusCode::PermissionDenied => {
                io::Error::new(io::ErrorKind::PermissionDenied, status.error_message)
            }
            StatusCode::Eof => io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"),
            _ => io::Error::other(format!("{}: {}", status.status_code, status.error_message)),
        },
        SftpError::Timeout => io::Error::new(io::ErrorKind::TimedOut, "SFTP 요청 시간 초과"),
        SftpError::IO(msg) | SftpError::UnexpectedBehavior(msg) => {
            io::Error::new(io::ErrorKind::ConnectionReset, msg)
        }
        other => io::Error::other(other.to_string()),
    }
}

/// S3 SDK 오류를 RemoteError 로 분류한다. `what` 은 대상(s3 URI, 버킷명 등).
pub(crate) fn map_s3_error<E: ProvideErrorMetadata>(e: &SdkError<E>, what: &str) -> RemoteError {
    if let Some(resp) = e.raw_response() {
        let status = resp.status().as_u16();
        let code = e
            .as_service_error()
            .and_then(|se| se.meta().code().map(str::to_string));
        return match (status, code.as_deref()) {
            (403, _) | (_, Some("AccessDenied")) => RemoteError::S3AccessDenied(what.to_string()),
            (404, _) | (_, Some("NoSuchKey" | "NoSuchBucket" | "NotFound" | "404")) => {
                RemoteError::S3NotFound(what.to_string())
            }
            _ => {
                let detail = code.map(|c| format!(" ({c})")).unwrap_or_default();
                RemoteError::S3(truncate(&format!("{what}: HTTP {status}{detail}"), 300))
            }
        };
    }
    // 응답 자체가 없는 오류: 자격 증명 해석 실패는 안내 문구로 분류한다.
    let msg = e.to_string();
    if msg.to_lowercase().contains("credential") {
        return RemoteError::S3Credentials(what.to_string());
    }
    RemoteError::S3(truncate(&format!("{what}: {msg}"), 300))
}

/// 오류가 리전 리다이렉트(301/PermanentRedirect)면 응답 헤더의 올바른 리전을 반환한다.
/// 그 외엔 `None` — 호출자가 원래 오류로 처리한다.
pub(crate) fn s3_redirect_region<E: ProvideErrorMetadata>(e: &SdkError<E>) -> Option<String> {
    let resp = e.raw_response()?;
    let is_redirect = resp.status().as_u16() == 301
        || e.as_service_error().and_then(|se| se.meta().code()) == Some("PermanentRedirect");
    if !is_redirect {
        return None;
    }
    resp.headers()
        .get("x-amz-bucket-region")
        .map(str::to_string)
}

/// 범위 오류(416 / InvalidRange) 여부. 요청 범위가 전부 객체 밖일 때 온다.
pub(crate) fn s3_is_invalid_range<E: ProvideErrorMetadata>(e: &SdkError<E>) -> bool {
    e.as_service_error().and_then(|se| se.meta().code()) == Some("InvalidRange")
        || e.raw_response().is_some_and(|r| r.status().as_u16() == 416)
}

/// 풀 작업 실패(RemoteError)를 RandomRead 의 io::Result 로 변환한다.
pub(crate) fn remote_to_io(e: RemoteError) -> io::Error {
    match e {
        RemoteError::NotFound(path) => io::Error::new(io::ErrorKind::NotFound, path),
        RemoteError::PermissionDenied(path) => {
            io::Error::new(io::ErrorKind::PermissionDenied, path)
        }
        RemoteError::S3AccessDenied(what) => io::Error::new(io::ErrorKind::PermissionDenied, what),
        RemoteError::S3NotFound(what) => io::Error::new(io::ErrorKind::NotFound, what),
        RemoteError::Timeout(secs) => io::Error::new(
            io::ErrorKind::TimedOut,
            format!("연결 시간 초과 ({secs}초)"),
        ),
        RemoteError::Io(io_err) => io_err,
        other => io::Error::other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_host_key() {
        let err = classify_ssh_failure(
            "myhost",
            "Warning: Permanently added 'myhost' (ED25519) to the list of known hosts.\r\nHost key verification failed.\r\n",
        );
        assert!(matches!(err, RemoteError::HostKeyUnknown(ref h) if h == "myhost"));
    }

    #[test]
    fn classify_auth_publickey() {
        let err = classify_ssh_failure(
            "myhost",
            "myhost: Permission denied (publickey,password,keyboard-interactive).\r\n",
        );
        assert!(matches!(err, RemoteError::AuthFailed(_)));
    }

    #[test]
    fn classify_too_many_auth() {
        let err = classify_ssh_failure("myhost", "Too many authentication failures\r\n");
        assert!(matches!(err, RemoteError::AuthFailed(_)));
    }

    #[test]
    fn classify_connection_refused() {
        let err = classify_ssh_failure(
            "myhost",
            "ssh: connect to host myhost port 22: Connection refused\r\n",
        );
        assert!(matches!(err, RemoteError::Connection(ref m) if m.contains("Connection refused")));
    }

    #[test]
    fn classify_empty_stderr() {
        let err = classify_ssh_failure("myhost", "");
        assert!(matches!(err, RemoteError::Connection(_)));
    }
}
