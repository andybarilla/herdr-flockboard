//! Repo discovery from agent working directories: each cwd's `origin` remote
//! classifies the repo (GitHub owner/repo, another host, no origin) and
//! supplies the organization used to group the board. Modeled on
//! herdr-scuttlebutt's `git_org.rs`; lookups are cached because remotes
//! change rarely while the agent poll runs every few seconds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::proc::run_with_timeout;

/// A wedged git (e.g. on a dead network mount) degrades the repo to the
/// inline git-error state after this rather than stalling the agent poll.
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a remote lookup is trusted. Worktrees come and go under a
/// long-lived dashboard, so entries expire rather than pinning the first
/// answer forever.
const TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Remote {
    /// GitHub-hosted. `repo` is "owner/name" as `gh --repo` expects it;
    /// `org` (the owner) is the board's grouping key.
    GitHub { org: String, repo: String },
    /// The directory is a repo (or git answered) but has no `origin` remote.
    NoOrigin,
    /// An origin exists but is not github.com: another host, or a local path
    /// (`host` is "local path" then). Rendered as an inline skip state; `gh`
    /// issue/PR data is github.com-only in v1.
    NonGitHub { host: String },
    /// The git lookup itself failed: not a repo, cwd deleted, git missing.
    GitError(String),
}

/// Resolves the `origin` remote of the repo containing `cwd`.
pub fn remote_for(cwd: &Path) -> Remote {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(["config", "--get", "remote.origin.url"]);
    let out = run_with_timeout(&mut cmd, GIT_TIMEOUT);
    match out {
        Err(e) => Remote::GitError(e.to_string()),
        Ok(o) if !o.status.success() => Remote::NoOrigin,
        Ok(o) => classify_url(String::from_utf8_lossy(&o.stdout).trim()),
    }
}

/// Classifies a remote URL. Both forge URLs (`scheme://host/owner/repo`) and
/// scp-style remotes (`user@host:owner/repo`) are recognized; anything
/// without a host is a local path, which has no owner to group by.
pub fn classify_url(url: &str) -> Remote {
    match host_path(url) {
        None => Remote::NonGitHub {
            host: "local path".to_string(),
        },
        Some((host, path)) => {
            if host.eq_ignore_ascii_case("github.com") {
                match owner_repo(&path) {
                    Some((owner, name)) => Remote::GitHub {
                        org: owner.clone(),
                        repo: format!("{owner}/{name}"),
                    },
                    None => Remote::NonGitHub { host },
                }
            } else {
                Remote::NonGitHub { host }
            }
        }
    }
}

/// Splits a remote URL into (host, path), or `None` for host-less local
/// paths. A colon only starts an scp-style path when it precedes any slash:
/// `/srv/a:b` is a local path, not `host:path`.
fn host_path(url: &str) -> Option<(String, String)> {
    if let Some((_, rest)) = url.split_once("://") {
        let (authority, path) = rest.split_once('/')?;
        // Strip userinfo (`git@host`) and any port (`host:22`).
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host);
        Some((host.to_string(), path.to_string()))
    } else {
        let colon = url.find(':')?;
        if url[..colon].contains('/') {
            return None;
        }
        let host = url[..colon].rsplit('@').next().unwrap_or(&url[..colon]);
        Some((host.to_string(), url[colon + 1..].to_string()))
    }
}

/// The (owner, repo) segments of a remote path, `.git` suffix stripped.
fn owner_repo(path: &str) -> Option<(String, String)> {
    let mut parts = path.trim_matches('/').split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if owner.is_empty() || name.is_empty() {
        return None;
    }
    let name = name.strip_suffix(".git").unwrap_or(name);
    Some((owner.to_string(), name.to_string()))
}

/// Memoizes `remote_for` per working directory so each agent poll does not
/// spawn one `git` per agent.
pub struct RemoteCache {
    ttl: Duration,
    entries: HashMap<PathBuf, (Instant, Remote)>,
}

impl Default for RemoteCache {
    fn default() -> Self {
        Self {
            ttl: TTL,
            entries: HashMap::new(),
        }
    }
}

impl RemoteCache {
    pub fn get(&mut self, cwd: &Path) -> Remote {
        if let Some((at, remote)) = self.entries.get(cwd) {
            if at.elapsed() < self.ttl {
                return remote.clone();
            }
        }
        let remote = remote_for(cwd);
        self.entries
            .insert(cwd.to_path_buf(), (Instant::now(), remote.clone()));
        remote
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_https_github_url() {
        assert_eq!(
            classify_url("https://github.com/andybarilla/herdr-flockboard"),
            Remote::GitHub {
                org: "andybarilla".to_string(),
                repo: "andybarilla/herdr-flockboard".to_string(),
            }
        );
    }

    #[test]
    fn classifies_scp_style_github_url_with_git_suffix() {
        assert_eq!(
            classify_url("git@github.com:andybarilla/flock.git"),
            Remote::GitHub {
                org: "andybarilla".to_string(),
                repo: "andybarilla/flock".to_string(),
            }
        );
    }

    #[test]
    fn classifies_ssh_scheme_url() {
        assert_eq!(
            classify_url("ssh://git@github.com/owner/repo.git"),
            Remote::GitHub {
                org: "owner".to_string(),
                repo: "owner/repo".to_string(),
            }
        );
    }

    #[test]
    fn classifies_other_host_as_non_github() {
        assert_eq!(
            classify_url("git@gitlab.com:owner/repo.git"),
            Remote::NonGitHub {
                host: "gitlab.com".to_string(),
            }
        );
    }

    #[test]
    fn classifies_local_path_as_non_github() {
        assert_eq!(
            classify_url("/srv/repos/mirror.git"),
            Remote::NonGitHub {
                host: "local path".to_string(),
            }
        );
        // A colon after a slash is still a local path, not scp syntax.
        assert_eq!(
            classify_url("/srv/a:b"),
            Remote::NonGitHub {
                host: "local path".to_string(),
            }
        );
    }

    #[test]
    fn trailing_slash_and_extra_segments_are_tolerated() {
        assert_eq!(
            classify_url("https://github.com/owner/repo/"),
            Remote::GitHub {
                org: "owner".to_string(),
                repo: "owner/repo".to_string(),
            }
        );
    }
}
