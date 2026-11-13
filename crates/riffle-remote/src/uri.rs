//! `sftp://host/abs/path` 형태의 원격 파일 URI.

use std::fmt;

/// 원격 파일 위치. `host` 는 ssh 가 이해하는 대상 그대로
/// (별칭, `user@host` 등) 보관하고, `path` 는 원격 절대 경로다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUri {
    pub host: String,
    pub path: String,
}

impl RemoteUri {
    /// `"sftp://host/절대/경로"` 문자열을 파싱한다.
    /// 경로에 공백·한글 등 비ASCII 문자가 있어도 그대로 받는다 (퍼센트 디코딩 없음).
    pub fn parse(s: &str) -> Option<RemoteUri> {
        let rest = s.strip_prefix("sftp://")?;
        let (host, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if host.is_empty() {
            return None;
        }
        Some(RemoteUri {
            host: host.to_string(),
            path: path.to_string(),
        })
    }
}

/// `"sftp://host/path"` 형태로 표시한다 (`to_string()` 은 `Display` 경유).
impl fmt::Display for RemoteUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sftp://{}{}", self.host, self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let uri = RemoteUri::parse("sftp://myhost/var/log/app.log").unwrap();
        assert_eq!(uri.host, "myhost");
        assert_eq!(uri.path, "/var/log/app.log");
        assert_eq!(uri.to_string(), "sftp://myhost/var/log/app.log");
    }

    #[test]
    fn roundtrip_space_and_korean() {
        let s = "sftp://nas-server/로그 모음/2026년 11월/app.log";
        let uri = RemoteUri::parse(s).unwrap();
        assert_eq!(uri.to_string(), s);
    }

    #[test]
    fn user_at_host() {
        let uri = RemoteUri::parse("sftp://deploy@bastion/etc/nginx/nginx.conf").unwrap();
        assert_eq!(uri.host, "deploy@bastion");
        assert_eq!(uri.path, "/etc/nginx/nginx.conf");
    }

    #[test]
    fn no_path_defaults_root() {
        let uri = RemoteUri::parse("sftp://myhost").unwrap();
        assert_eq!(uri.path, "/");
    }

    #[test]
    fn rejects_non_sftp() {
        assert!(RemoteUri::parse("http://host/path").is_none());
        assert!(RemoteUri::parse("/local/path").is_none());
        assert!(RemoteUri::parse("").is_none());
    }

    #[test]
    fn rejects_empty_host() {
        assert!(RemoteUri::parse("sftp:///abs/path").is_none());
        assert!(RemoteUri::parse("sftp://").is_none());
    }
}
