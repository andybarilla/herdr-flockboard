//! Repo discovery from agent working directories: each cwd's `origin` remote
//! classifies the repo (GitHub owner/repo, another host, no origin) or the
//! lookup itself fails (non-repo, missing cwd, git error). The classification
//! supplies the organization used to group the board. Modeled on
//! herdr-scuttlebutt's `git_org.rs`; lookups are cached because remotes
//! change rarely while the agent poll runs every few seconds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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
    /// A valid git work tree that has no `origin` remote.
    NoOrigin,
    /// An origin exists but is not github.com: another host, or a local path
    /// (`host` is "local path" then). Rendered as an inline skip state; `gh`
    /// issue/PR data is github.com-only in v1.
    NonGitHub { host: String },
    /// The git lookup itself failed: not a repo, cwd deleted, git missing.
    GitError(String),
}

/// Resolves the `origin` remote of the repo containing `cwd`. Only a valid
/// work tree that simply lacks an origin is `NoOrigin`; everything broken —
/// non-repo, missing or deleted cwd, unreadable `.git`, a git failure — is
/// `GitError`, never conflated with it.
pub fn remote_for(cwd: &Path) -> Remote {
    // Preflight: `NoOrigin` may only be reached from a real work tree.
    // `git -C` also surfaces a missing/deleted cwd as a failure here.
    let mut preflight = Command::new("git");
    preflight
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--is-inside-work-tree"]);
    match run_with_timeout(&mut preflight, GIT_TIMEOUT) {
        Err(e) => return Remote::GitError(e.to_string()),
        Ok(o) if !o.status.success() => return Remote::GitError(git_failure(&o)),
        Ok(_) => {}
    }
    // `--local` reads only the repo's own config, so a `remote.origin.url`
    // set in global or system config cannot masquerade as this repo's
    // origin.
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(["config", "--local", "--get", "remote.origin.url"]);
    match run_with_timeout(&mut cmd, GIT_TIMEOUT) {
        Err(e) => Remote::GitError(e.to_string()),
        Ok(o) if o.status.success() => classify_url(String::from_utf8_lossy(&o.stdout).trim()),
        // In a valid repo (preflight passed), exit 1 with no output is
        // git-config's "key not set": the repo simply has no origin.
        Ok(o) if o.status.code() == Some(1) && o.stdout.is_empty() && o.stderr.is_empty() => {
            Remote::NoOrigin
        }
        Ok(o) => Remote::GitError(git_failure(&o)),
    }
}

/// The first line of git's stderr, or the exit status when it said nothing.
fn git_failure(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    match stderr.lines().next().map(str::trim) {
        Some(msg) if !msg.is_empty() => msg.to_string(),
        _ => format!("git exited with {}", out.status),
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

    /// Runs git in `dir`, asserting success; test fixture setup only.
    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A fresh git repo in a hermetic temporary directory.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        dir
    }

    #[test]
    fn repo_with_github_origin_classifies_github() {
        let dir = repo();
        git(
            dir.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        assert_eq!(
            remote_for(dir.path()),
            Remote::GitHub {
                org: "owner".to_string(),
                repo: "owner/repo".to_string(),
            }
        );
    }

    #[test]
    fn repo_without_origin_is_no_origin() {
        let dir = repo();
        assert_eq!(remote_for(dir.path()), Remote::NoOrigin);
    }

    #[test]
    fn global_config_origin_does_not_leak_into_repo_without_origin() {
        let dir = repo();
        let global = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            global.path(),
            "[remote \"origin\"]\n\turl = https://github.com/x/y\n",
        )
        .unwrap();
        // `--local` never reads global config, so the env override cannot
        // change any other test's classification even while briefly set.
        std::env::set_var("GIT_CONFIG_GLOBAL", global.path());
        let remote = remote_for(dir.path());
        std::env::remove_var("GIT_CONFIG_GLOBAL");
        assert_eq!(remote, Remote::NoOrigin);
    }

    #[test]
    fn existing_non_repo_dir_is_git_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(remote_for(dir.path()), Remote::GitError(_)));
    }

    #[test]
    fn deleted_cwd_is_git_error() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("worktree");
        std::fs::create_dir(&gone).unwrap();
        std::fs::remove_dir(&gone).unwrap();
        assert!(matches!(remote_for(&gone), Remote::GitError(_)));
    }

    #[test]
    fn non_github_origin_is_non_github() {
        let dir = repo();
        git(
            dir.path(),
            &["remote", "add", "origin", "git@gitlab.com:owner/repo.git"],
        );
        assert_eq!(
            remote_for(dir.path()),
            Remote::NonGitHub {
                host: "gitlab.com".to_string(),
            }
        );
    }

    #[test]
    fn local_path_origin_is_non_github() {
        let dir = repo();
        git(
            dir.path(),
            &["remote", "add", "origin", "/srv/repos/mirror.git"],
        );
        assert_eq!(
            remote_for(dir.path()),
            Remote::NonGitHub {
                host: "local path".to_string(),
            }
        );
    }

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
