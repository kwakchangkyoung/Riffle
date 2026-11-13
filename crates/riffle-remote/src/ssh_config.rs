//! `~/.ssh/config` 파서. 호스트 별칭 목록과 HostName/User 값을 읽는다.
//!
//! 지원 범위: `Host`/`Match` 블록, `HostName`, `User`, `Include`
//! (`~` 확장, 포함 파일 기준 상대 경로, 파일명 부분의 `*`/`?` 글롭).
//! `*`·`?`·`!` 를 포함한 패턴은 별칭으로 취급하지 않는다.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// `~/.ssh/config` 의 `Host` 블록 하나에서 얻은 별칭 정보.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshHost {
    /// `Host` 줄의 패턴 토큰 (와일드카드·부정 패턴 제외).
    pub alias: String,
    /// `HostName` 값. 없으면 `None` (ssh 는 별칭 자체를 호스트명으로 사용).
    pub hostname: Option<String>,
    /// `User` 값.
    pub user: Option<String>,
}

/// 사용자 `~/.ssh/config` 에서 호스트 별칭 목록을 읽는다.
pub fn list_ssh_hosts() -> Vec<SshHost> {
    let Some(home) = std::env::home_dir() else {
        return Vec::new();
    };
    list_ssh_hosts_inner(&home.join(".ssh").join("config"), &home)
}

/// 임의 경로의 ssh config 파일에서 호스트 별칭 목록을 읽는다 (테스트용).
pub fn list_ssh_hosts_from(config_path: &Path) -> Vec<SshHost> {
    let home = std::env::home_dir().unwrap_or_default();
    list_ssh_hosts_inner(config_path, &home)
}

fn list_ssh_hosts_inner(config_path: &Path, home: &Path) -> Vec<SshHost> {
    let mut lines = Vec::new();
    flatten_config(config_path, home, 0, &mut lines);
    parse_hosts(&lines)
}

const MAX_INCLUDE_DEPTH: usize = 16;

/// Include 를 텍스트 수준에서 펼쳐 `(키워드 줄)` 의 단일 스트림을 만든다.
/// ssh 와 마찬가지로 먼저 나온 값이 우선하므로 순서를 유지해야 한다.
fn flatten_config(path: &Path, home: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    for raw in text.lines() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        let keyword = line.split_whitespace().next().unwrap_or("");
        if keyword.eq_ignore_ascii_case("include") {
            for pat in line.split_whitespace().skip(1) {
                expand_include(pat, base_dir, home, depth, out);
            }
        } else {
            out.push(line);
        }
    }
}

/// `#` 주석을 제거한다. `#` 가 줄 처음이거나 공백 뒤에 올 때만 주석으로 본다.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

/// `~` 와 `~/` 를 홈 디렉터리로 확장한다. `~user` 형태는 지원하지 않는다.
fn expand_tilde(pat: &str, home: &Path) -> PathBuf {
    if pat == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = pat.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(pat)
}

/// Include 패턴 하나를 펼쳐 매칭된 파일들을 순서대로 처리한다.
/// 글롭은 마지막 경로 구성요소(파일명)에만 적용한다.
fn expand_include(pat: &str, base_dir: &Path, home: &Path, depth: usize, out: &mut Vec<String>) {
    let unquoted = unquote(pat);
    let expanded = expand_tilde(&unquoted, home);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    };

    let file_part = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let has_glob = file_part.contains('*') || file_part.contains('?');
    if !has_glob {
        flatten_config(&path, home, depth + 1, out);
        return;
    }

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| p.is_file() && glob_match(file_part.as_bytes(), n.as_bytes()))
        })
        .collect();
    names.sort();
    for p in names {
        flatten_config(&p, home, depth + 1, out);
    }
}

/// `"..."` 로 감싼 값을 벗긴다.
fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

/// `*`(임의 문자열)와 `?`(임의 1바이트)를 지원하는 최소 glob 매칭.
fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    let (mut star_p, mut star_i) = (usize::MAX, 0usize);
    while i < s.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star_p = p;
            star_i = i;
            p += 1;
        } else if star_p != usize::MAX {
            p = star_p + 1;
            star_i += 1;
            i = star_i;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

struct Stanza {
    aliases: Vec<String>,
    hostname: Option<String>,
    user: Option<String>,
}

fn parse_hosts(lines: &[String]) -> Vec<SshHost> {
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cur: Option<Stanza> = None;

    for line in lines {
        let mut it = line.split_whitespace();
        let keyword = it.next().unwrap_or("").to_ascii_lowercase();
        match keyword.as_str() {
            "host" => {
                flush_stanza(&mut cur, &mut out, &mut seen);
                cur = Some(Stanza {
                    aliases: it
                        .filter(|&t| is_valid_alias(t))
                        .map(str::to_string)
                        .collect(),
                    hostname: None,
                    user: None,
                });
            }
            // Match 블록 안의 조건부 설정은 호스트 목록과 무관하므로 별칭 컨텍스트를 닫는다.
            "match" => {
                flush_stanza(&mut cur, &mut out, &mut seen);
                cur = None;
            }
            "hostname" => {
                if let Some(s) = cur.as_mut()
                    && s.hostname.is_none()
                {
                    s.hostname = it.next().map(unquote);
                }
            }
            "user" => {
                if let Some(s) = cur.as_mut()
                    && s.user.is_none()
                {
                    s.user = it.next().map(unquote);
                }
            }
            _ => {}
        }
    }
    flush_stanza(&mut cur, &mut out, &mut seen);

    out.sort_by(|a, b| {
        a.alias
            .to_lowercase()
            .cmp(&b.alias.to_lowercase())
            .then_with(|| a.alias.cmp(&b.alias))
    });
    out
}

fn flush_stanza(cur: &mut Option<Stanza>, out: &mut Vec<SshHost>, seen: &mut HashSet<String>) {
    let Some(s) = cur.take() else { return };
    for alias in s.aliases {
        // ssh 호스트명 매칭은 대소문자를 구분하지 않으므로 첫 등장만 남긴다.
        if seen.insert(alias.to_lowercase()) {
            out.push(SshHost {
                alias,
                hostname: s.hostname.clone(),
                user: s.user.clone(),
            });
        }
    }
}

/// `*`·`?` 글롭과 `!` 부정 패턴은 별칭이 아니므로 제외한다.
fn is_valid_alias(token: &str) -> bool {
    !token.is_empty() && !token.contains('*') && !token.contains('?') && !token.starts_with('!')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("riffle-sshcfg-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn multiple_hosts_and_aliases() {
        let dir = tempdir("basic");
        let cfg = write_config(
            &dir,
            "config",
            "Host web1 web2\n    HostName 10.0.0.1\n    User deploy\n\nHost db\n    HostName db.internal\n",
        );
        let hosts = list_ssh_hosts_from(&cfg);
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].alias, "db");
        assert_eq!(hosts[0].hostname.as_deref(), Some("db.internal"));
        assert_eq!(hosts[0].user, None);
        assert_eq!(hosts[1].alias, "web1");
        assert_eq!(hosts[1].hostname.as_deref(), Some("10.0.0.1"));
        assert_eq!(hosts[1].user.as_deref(), Some("deploy"));
        assert_eq!(hosts[2].alias, "web2");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wildcard_and_negation_excluded() {
        let dir = tempdir("wild");
        let cfg = write_config(
            &dir,
            "config",
            "Host *\n    User everyone\nHost *.example.com\n    User x\nHost !bastion good1 good2\n    HostName h\nHost prod-?\n    HostName y\n",
        );
        let hosts = list_ssh_hosts_from(&cfg);
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, vec!["good1", "good2"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn comments_and_case() {
        let dir = tempdir("comment");
        let cfg = write_config(
            &dir,
            "config",
            "# 전체 주석\nHOST MyHost   # trailing\n\thOsTnAmE real.host\n\tUSER bob # 코멘트\n",
        );
        let hosts = list_ssh_hosts_from(&cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "MyHost");
        assert_eq!(hosts[0].hostname.as_deref(), Some("real.host"));
        assert_eq!(hosts[0].user.as_deref(), Some("bob"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_relative_and_glob() {
        let dir = tempdir("include");
        write_config(&dir, "extra1.conf", "Host inc1\n    HostName e1\n");
        write_config(&dir, "extra2.conf", "Host inc2\n    HostName e2\n");
        write_config(&dir, "skip.txt", "Host notme\n");
        let cfg = write_config(
            &dir,
            "config",
            "Include extra*.conf\nInclude missing.conf\nHost main\n    HostName m\n",
        );
        let hosts = list_ssh_hosts_from(&cfg);
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, vec!["inc1", "inc2", "main"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_tilde() {
        let dir = tempdir("tilde");
        let fake_home = dir.join("home");
        write_config(
            &fake_home,
            ".ssh/keys.conf",
            "Host tildehost\n    User tildeuser\n",
        );
        let cfg = write_config(&dir, "config", "Include ~/.ssh/keys.conf\n");
        let hosts = list_ssh_hosts_inner(&cfg, &fake_home);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "tildehost");
        assert_eq!(hosts[0].user.as_deref(), Some("tildeuser"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn match_block_ignored() {
        let dir = tempdir("match");
        let cfg = write_config(
            &dir,
            "config",
            "Host real\n    HostName r\nMatch host *.internal\n    User nobody\n    HostName fake\nHost after\n    User u2\n",
        );
        let hosts = list_ssh_hosts_from(&cfg);
        let aliases: Vec<&str> = hosts.iter().map(|h| h.alias.as_str()).collect();
        assert_eq!(aliases, vec!["after", "real"]);
        // Match 블록 안의 HostName/User 는 반영되지 않는다.
        let real = hosts.iter().find(|h| h.alias == "real").unwrap();
        assert_eq!(real.hostname.as_deref(), Some("r"));
        assert_eq!(real.user, None);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dedup_first_wins() {
        let dir = tempdir("dedup");
        let cfg = write_config(
            &dir,
            "config",
            "Host dup\n    HostName first\nHost dup other\n    HostName second\n",
        );
        let hosts = list_ssh_hosts_from(&cfg);
        // "dup" 은 첫 등장만 남고, "other" 는 별개 별칭으로 유지된다.
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].alias, "dup");
        assert_eq!(hosts[0].hostname.as_deref(), Some("first"));
        assert_eq!(hosts[1].alias, "other");
        assert_eq!(hosts[1].hostname.as_deref(), Some("second"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_empty() {
        let hosts = list_ssh_hosts_from(Path::new("/nonexistent/ssh_config_xyz"));
        assert!(hosts.is_empty());
    }
}
