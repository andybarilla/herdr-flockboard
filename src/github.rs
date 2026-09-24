//! Per-repo issue and PR data from the `gh` CLI, behind the `IssueTracker`
//! trait so state derivation is testable without GitHub. Fetching is raw
//! data only; bucketing and status classification live in `state.rs`.

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::proc::run_with_timeout;

/// A hung network or API call becomes an inline per-repo error after this,
/// so one slow repo can never freeze the GitHub refresh worker.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub labels: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub draft: bool,
    /// Raw `reviewDecision` from gh: "", APPROVED, CHANGES_REQUESTED, or
    /// REVIEW_REQUIRED.
    pub review_decision: String,
    /// Raw `mergeable` from gh: MERGEABLE, CONFLICTING, or UNKNOWN ("" when
    /// gh did not report it).
    pub mergeable: String,
    /// Head branch name; correlates a PR back to its issue via the
    /// `flock/issue-<n>-<slug>` branch pattern and journal `branch` fields.
    pub head_ref_name: String,
    pub checks: Vec<CheckRollup>,
}

/// One `statusCheckRollup` entry. gh emits two shapes: check runs carry
/// `status`/`conclusion`, status contexts carry `state`; each is `None` for
/// the shape that does not have it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckRollup {
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub state: Option<String>,
}

pub trait IssueTracker {
    /// `repo` is "owner/name" as `gh --repo` expects it.
    fn open_issues(&self, repo: &str) -> Result<Vec<Issue>>;
    fn open_prs(&self, repo: &str) -> Result<Vec<PullRequest>>;
}

#[derive(Deserialize)]
struct RawIssue {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    labels: Vec<RawLabel>,
}

#[derive(Deserialize)]
struct RawLabel {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: String,
    #[serde(default)]
    mergeable: String,
    #[serde(default)]
    head_ref_name: String,
    // gh emits `null` (not `[]`) for a PR with no checks at all.
    #[serde(default)]
    status_check_rollup: Option<Vec<RawRollup>>,
}

#[derive(Deserialize)]
struct RawRollup {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

pub fn parse_issues(json: &str) -> Result<Vec<Issue>> {
    let raw: Vec<RawIssue> = serde_json::from_str(json).context("parsing gh issue list output")?;
    Ok(raw
        .into_iter()
        .map(|r| Issue {
            number: r.number,
            title: r.title,
            labels: r.labels.into_iter().map(|l| l.name).collect(),
        })
        .collect())
}

pub fn parse_prs(json: &str) -> Result<Vec<PullRequest>> {
    let raw: Vec<RawPr> = serde_json::from_str(json).context("parsing gh pr list output")?;
    Ok(raw
        .into_iter()
        .map(|r| PullRequest {
            number: r.number,
            title: r.title,
            draft: r.is_draft,
            review_decision: r.review_decision,
            mergeable: r.mergeable,
            head_ref_name: r.head_ref_name,
            checks: r
                .status_check_rollup
                .unwrap_or_default()
                .into_iter()
                .map(|c| CheckRollup {
                    status: c.status,
                    conclusion: c.conclusion,
                    state: c.state,
                })
                .collect(),
        })
        .collect())
}

/// Real implementation: shells out to `gh`. Failures (auth, network, repo
/// gone, timeout) are returned as errors so the board can render them
/// inline per repo.
pub struct GhCli;

impl GhCli {
    fn run(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("gh");
        cmd.args(args);
        // Defense in depth behind the null stdin `run_with_timeout` sets:
        // the board calls gh unattended, so gh must fail fast on anything
        // that would prompt rather than ever try to interact.
        cmd.env("GH_PROMPT_DISABLED", "1");
        let out = run_with_timeout(&mut cmd, GH_TIMEOUT).context("running gh")?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        String::from_utf8(out.stdout).context("gh output was not UTF-8")
    }
}

impl IssueTracker for GhCli {
    fn open_issues(&self, repo: &str) -> Result<Vec<Issue>> {
        let json = self.run(&[
            "issue",
            "list",
            "--repo",
            repo,
            "--state",
            "open",
            "--json",
            "number,title,labels",
            "--limit",
            "200",
        ])?;
        parse_issues(&json)
    }

    fn open_prs(&self, repo: &str) -> Result<Vec<PullRequest>> {
        let json = self.run(&[
            "pr",
            "list",
            "--repo",
            repo,
            "--state",
            "open",
            "--json",
            "number,title,isDraft,reviewDecision,mergeable,headRefName,statusCheckRollup",
            "--limit",
            "200",
        ])?;
        parse_prs(&json)
    }
}

/// Test double: replays canned issues/PRs (or errors) for any repo.
#[cfg(test)]
pub struct FakeTracker {
    pub issues: std::result::Result<Vec<Issue>, String>,
    pub prs: std::result::Result<Vec<PullRequest>, String>,
}

#[cfg(test)]
impl IssueTracker for FakeTracker {
    fn open_issues(&self, _repo: &str) -> Result<Vec<Issue>> {
        match &self.issues {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(anyhow::anyhow!(e.clone())),
        }
    }

    fn open_prs(&self, _repo: &str) -> Result<Vec<PullRequest>> {
        match &self.prs {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(anyhow::anyhow!(e.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_issues_with_labels() {
        let json = r#"[
            {"number": 1, "title": "Board", "labels": [{"name": "enhancement"}, {"name": "ready-for-agent"}]},
            {"number": 2, "title": "No labels", "labels": []}
        ]"#;
        let issues = parse_issues(json).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].labels, vec!["enhancement", "ready-for-agent"]);
        assert!(issues[1].labels.is_empty());
    }

    #[test]
    fn parses_prs_with_both_check_shapes() {
        let json = r#"[
            {"number": 7, "title": "Fix", "isDraft": false, "reviewDecision": "APPROVED",
             "mergeable": "MERGEABLE", "headRefName": "flock/issue-7-fix",
             "statusCheckRollup": [
                {"__typename": "CheckRun", "status": "COMPLETED", "conclusion": "SUCCESS", "name": "test"},
                {"__typename": "StatusContext", "state": "PENDING", "context": "ci"}
             ]},
            {"number": 8, "title": "WIP", "isDraft": true, "reviewDecision": "", "statusCheckRollup": null}
        ]"#;
        let prs = parse_prs(json).unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].review_decision, "APPROVED");
        assert_eq!(prs[0].mergeable, "MERGEABLE");
        assert_eq!(prs[0].head_ref_name, "flock/issue-7-fix");
        assert_eq!(
            prs[0].checks,
            vec![
                CheckRollup {
                    status: Some("COMPLETED".to_string()),
                    conclusion: Some("SUCCESS".to_string()),
                    state: None,
                },
                CheckRollup {
                    status: None,
                    conclusion: None,
                    state: Some("PENDING".to_string()),
                },
            ]
        );
        assert!(prs[1].draft);
        assert!(prs[1].checks.is_empty());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(parse_issues("not json").is_err());
    }
}
