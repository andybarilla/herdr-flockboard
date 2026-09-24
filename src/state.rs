//! Dashboard state derivation. Everything here is pure data transformation
//! over the `herd` and `github` fetch layers — no process spawning, no
//! terminal — so agent grouping, label bucketing, PR classification, and the
//! GitHub TTL cache are all unit-testable with fakes.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::git_org::Remote;
use crate::github::{CheckRollup, Issue, PullRequest};
use crate::herd::AgentInfo;

/// How often the agent board re-polls `herdr agent list`.
pub const AGENT_POLL: Duration = Duration::from_secs(3);

/// How long per-repo GitHub data is trusted before an asynchronous refetch.
/// Short enough that a queue change shows up within a minute, long enough
/// that a busy session does not hammer `gh`.
pub const GH_TTL: Duration = Duration::from_secs(45);

/// Tracked Flock state labels, most-urgent first. An issue carrying several
/// tracked labels lands in the first matching bucket so the board counts each
/// issue exactly once, and the bucket that needs the human soonest wins.
pub const LABEL_PRIORITY: [&str; 5] = [
    "blocked",
    "needs-info",
    "needs-triage",
    "ready-for-agent",
    "ready-for-human",
];

/// Grouping key for repos whose origin gives no organization.
pub const NO_ORG_GROUP: &str = "(no origin)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    /// A status this build does not know yet; rendered as `unknown` rather
    /// than failing the board.
    Unknown,
}

impl AgentStatus {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            "done" => Self::Done,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }
}

/// One row of the agent board: the herdr agent plus the repo its cwd implies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRow {
    pub name: String,
    pub status: AgentStatus,
    pub cwd: PathBuf,
    /// Basename of the cwd, shown as the repo column; the full owner/repo
    /// lives on the repo view once the remote is resolved.
    pub repo_dir: String,
    pub workspace: String,
    pub pane: String,
    pub title: String,
    pub focused: bool,
}

pub fn agent_rows(infos: Vec<AgentInfo>) -> Vec<AgentRow> {
    let mut rows: Vec<AgentRow> = infos
        .into_iter()
        .map(|i| {
            let cwd = PathBuf::from(&i.cwd);
            let repo_dir = cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| i.cwd.clone());
            AgentRow {
                name: i.agent,
                status: AgentStatus::parse(&i.status),
                cwd,
                repo_dir,
                workspace: i.workspace_id,
                pane: i.pane_id,
                title: i.terminal_title,
                focused: i.focused,
            }
        })
        .collect();
    // Working agents first, then by repo, so the busy part of the session is
    // at the top of the board.
    rows.sort_by(|a, b| {
        rank(a.status)
            .cmp(&rank(b.status))
            .then_with(|| a.repo_dir.cmp(&b.repo_dir))
            .then_with(|| a.pane.cmp(&b.pane))
    });
    rows
}

fn rank(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Blocked => 0,
        AgentStatus::Working => 1,
        AgentStatus::Idle => 2,
        AgentStatus::Done => 3,
        AgentStatus::Unknown => 4,
    }
}

/// Open-issue counts by Flock state label. `by_label` only has entries for
/// tracked labels; everything else (untracked labels, no labels) is `other`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueBuckets {
    pub by_label: BTreeMap<String, usize>,
    pub other: usize,
    pub total: usize,
}

pub fn bucket_issues(issues: &[Issue]) -> IssueBuckets {
    let mut buckets = IssueBuckets {
        total: issues.len(),
        ..Default::default()
    };
    for issue in issues {
        match LABEL_PRIORITY
            .iter()
            .find(|tracked| issue.labels.iter().any(|l| l == **tracked))
        {
            Some(label) => *buckets.by_label.entry(label.to_string()).or_default() += 1,
            None => buckets.other += 1,
        }
    }
    buckets
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Checks {
    Pass,
    Fail,
    Pending,
    /// No check rollup at all — not the same as passing.
    None,
}

pub fn classify_checks(rollups: &[CheckRollup]) -> Checks {
    if rollups.is_empty() {
        return Checks::None;
    }
    let mut pending = false;
    for r in rollups {
        let conclusion = r.conclusion.as_deref().unwrap_or("");
        let state = r.state.as_deref().unwrap_or("");
        let status = r.status.as_deref().unwrap_or("");
        if matches!(
            conclusion,
            "FAILURE" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
        ) || matches!(state, "FAILURE" | "ERROR")
        {
            return Checks::Fail;
        }
        if matches!(state, "PENDING" | "EXPECTED") || (!status.is_empty() && status != "COMPLETED")
        {
            pending = true;
        }
    }
    if pending {
        Checks::Pending
    } else {
        Checks::Pass
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Review {
    Approved,
    ChangesRequested,
    Required,
    /// gh reported no review decision (""), e.g. a draft or unreviewable PR.
    None,
}

pub fn classify_review(decision: &str) -> Review {
    match decision {
        "APPROVED" => Review::Approved,
        "CHANGES_REQUESTED" => Review::ChangesRequested,
        "REVIEW_REQUIRED" => Review::Required,
        _ => Review::None,
    }
}

/// One row of a repo's PR table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrRow {
    pub number: u64,
    pub title: String,
    pub checks: Checks,
    pub review: Review,
    pub draft: bool,
}

pub fn pr_rows(prs: &[PullRequest]) -> Vec<PrRow> {
    let mut rows: Vec<PrRow> = prs
        .iter()
        .map(|p| PrRow {
            number: p.number,
            title: p.title.clone(),
            checks: classify_checks(&p.checks),
            review: classify_review(&p.review_decision),
            draft: p.draft,
        })
        .collect();
    rows.sort_by_key(|r| r.number);
    rows
}

/// Cached per-repo GitHub state. Created when an agent's cwd first names a
/// repo and pruned when no live agent references it anymore; raw fetch
/// results (including errors) are cached so a flaky `gh` is retried on the
/// refresh TTL, not continuously.
pub struct RepoSlot {
    pub cwd: PathBuf,
    pub remote: Remote,
    pub issues: Option<Result<Vec<Issue>, String>>,
    pub prs: Option<Result<Vec<PullRequest>, String>>,
    pub fetched_at: Option<Instant>,
}

impl RepoSlot {
    pub fn new(cwd: PathBuf, remote: Remote) -> Self {
        Self {
            cwd,
            remote,
            issues: None,
            prs: None,
            fetched_at: None,
        }
    }
}

/// Registers a slot for every cwd the live agents named, resolving the
/// remote through `lookup` (a `RemoteCache` in production) for new cwds
/// only, and drops slots whose cwd no live agent named anymore — GitHub
/// work and cache only ever track repos currently in use.
pub fn sync_slots(
    rows: &[AgentRow],
    slots: &mut HashMap<PathBuf, RepoSlot>,
    lookup: &mut dyn FnMut(&Path) -> Remote,
) {
    slots.retain(|cwd, _| rows.iter().any(|row| row.cwd == *cwd));
    for row in rows {
        slots
            .entry(row.cwd.clone())
            .or_insert_with(|| RepoSlot::new(row.cwd.clone(), lookup(&row.cwd)));
    }
}

/// GitHub-hosted slots due for a refetch, as (cwd, "owner/repo") pairs the
/// GitHub worker can fetch without holding the slots lock. Non-GitHub
/// remotes are never listed — their inline state explains why — and a slot
/// inside its TTL keeps its cached data, errors included, so a flaky `gh`
/// is not retried on every agent poll.
pub fn due_github_repos(
    slots: &HashMap<PathBuf, RepoSlot>,
    now: Instant,
    ttl: Duration,
) -> Vec<(PathBuf, String)> {
    slots
        .iter()
        .filter_map(|(cwd, slot)| {
            let Remote::GitHub { repo, .. } = &slot.remote else {
                return None;
            };
            let due = slot
                .fetched_at
                .is_none_or(|at| now.duration_since(at) >= ttl);
            due.then(|| (cwd.clone(), repo.clone()))
        })
        .collect()
}

/// Stores a finished fetch — errors included — so the board shows what `gh`
/// actually said rather than a spinner forever.
pub fn record_fetch(
    slot: &mut RepoSlot,
    issues: Result<Vec<Issue>, String>,
    prs: Result<Vec<PullRequest>, String>,
    at: Instant,
) {
    slot.issues = Some(issues);
    slot.prs = Some(prs);
    slot.fetched_at = Some(at);
}

/// What the board renders for one repo: identity, grouping, fetch state.
pub struct RepoView {
    pub key: String,
    pub org: String,
    pub remote: Remote,
    pub agent_count: usize,
    pub issues: Option<Result<IssueBuckets, String>>,
    pub prs: Option<Result<Vec<PrRow>, String>>,
    pub fetched_at: Option<Instant>,
}

pub fn repo_view(slot: &RepoSlot, agent_count: usize) -> RepoView {
    let dir = slot
        .cwd
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| slot.cwd.display().to_string());
    let (key, org) = match &slot.remote {
        Remote::GitHub { org, repo } => (repo.clone(), org.clone()),
        Remote::NoOrigin | Remote::GitError(_) => (dir.clone(), NO_ORG_GROUP.to_string()),
        Remote::NonGitHub { host } => (format!("{dir} ({host})"), host.clone()),
    };
    let issues = slot.issues.as_ref().map(|r| match r {
        Ok(v) => Ok(bucket_issues(v)),
        Err(e) => Err(e.clone()),
    });
    let prs = slot.prs.as_ref().map(|r| match r {
        Ok(v) => Ok(pr_rows(v)),
        Err(e) => Err(e.clone()),
    });
    RepoView {
        key,
        org,
        remote: slot.remote.clone(),
        agent_count,
        issues,
        prs,
        fetched_at: slot.fetched_at,
    }
}

pub struct OrgGroup {
    pub org: String,
    pub repos: Vec<RepoView>,
}

/// Groups repos by organization, orgs sorted alphabetically with the
/// no-origin fallback last, repos sorted by key within each org.
pub fn group_repos(views: Vec<RepoView>) -> Vec<OrgGroup> {
    let mut by_org: BTreeMap<String, Vec<RepoView>> = BTreeMap::new();
    for view in views {
        by_org.entry(view.org.clone()).or_default().push(view);
    }
    let mut groups: Vec<OrgGroup> = by_org
        .into_iter()
        .map(|(org, mut repos)| {
            repos.sort_by(|a, b| a.key.cmp(&b.key));
            OrgGroup { org, repos }
        })
        .collect();
    groups.sort_by(|a, b| {
        (a.org == NO_ORG_GROUP)
            .cmp(&(b.org == NO_ORG_GROUP))
            .then_with(|| a.org.cmp(&b.org))
    });
    groups
}

/// The full board: agent rows (or the herdr error) plus repos discovered
/// from live agent cwds, grouped by org. A herdr failure yields no repo
/// section at all — without agents there is nothing to discover from — and
/// the UI renders the error in place of the whole board.
pub struct Dashboard {
    pub agents: Result<Vec<AgentRow>, String>,
    pub groups: Vec<OrgGroup>,
}

pub fn dashboard(
    agents: Result<Vec<AgentRow>, String>,
    slots: &HashMap<PathBuf, RepoSlot>,
) -> Dashboard {
    let rows = match agents {
        Err(e) => {
            return Dashboard {
                agents: Err(e),
                groups: Vec::new(),
            }
        }
        Ok(rows) => rows,
    };
    let mut counts: HashMap<&Path, usize> = HashMap::new();
    for row in &rows {
        *counts.entry(row.cwd.as_path()).or_default() += 1;
    }
    let views: Vec<RepoView> = counts
        .iter()
        .filter_map(|(cwd, n)| slots.get(*cwd).map(|slot| repo_view(slot, *n)))
        .collect();
    Dashboard {
        agents: Ok(rows),
        groups: group_repos(views),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{FakeTracker, IssueTracker};

    fn info(status: &str, cwd: &str) -> AgentInfo {
        AgentInfo {
            agent: "pi".to_string(),
            status: status.to_string(),
            cwd: cwd.to_string(),
            focused: false,
            workspace_id: "w1".to_string(),
            pane_id: format!("w1:p-{status}"),
            terminal_title: format!("π - {cwd}"),
        }
    }

    fn issue(labels: &[&str]) -> Issue {
        Issue {
            number: 1,
            title: "t".to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn rollup(status: Option<&str>, conclusion: Option<&str>, state: Option<&str>) -> CheckRollup {
        CheckRollup {
            status: status.map(str::to_string),
            conclusion: conclusion.map(str::to_string),
            state: state.map(str::to_string),
        }
    }

    fn github_slot(cwd: &str, repo: &str) -> RepoSlot {
        RepoSlot::new(
            PathBuf::from(cwd),
            Remote::GitHub {
                org: repo.split('/').next().unwrap().to_string(),
                repo: repo.to_string(),
            },
        )
    }

    #[test]
    fn agent_status_maps_known_and_unknown() {
        assert_eq!(AgentStatus::parse("idle"), AgentStatus::Idle);
        assert_eq!(AgentStatus::parse("working"), AgentStatus::Working);
        assert_eq!(AgentStatus::parse("blocked"), AgentStatus::Blocked);
        assert_eq!(AgentStatus::parse("done"), AgentStatus::Done);
        assert_eq!(AgentStatus::parse("paused"), AgentStatus::Unknown);
        assert_eq!(AgentStatus::parse(""), AgentStatus::Unknown);
    }

    #[test]
    fn agent_rows_sort_blocked_then_working_then_idle() {
        let rows = agent_rows(vec![
            info("idle", "/dev/a/zeta"),
            info("working", "/dev/a/beta"),
            info("blocked", "/dev/a/alpha"),
        ]);
        let order: Vec<&str> = rows.iter().map(|r| r.repo_dir.as_str()).collect();
        assert_eq!(order, vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn bucket_issues_counts_each_issue_once_by_priority() {
        let buckets = bucket_issues(&[
            issue(&["blocked", "ready-for-agent"]), // blocked wins
            issue(&["ready-for-agent"]),
            issue(&["enhancement"]), // untracked label
            issue(&[]),              // no labels
        ]);
        assert_eq!(buckets.total, 4);
        assert_eq!(buckets.by_label.get("blocked"), Some(&1));
        assert_eq!(buckets.by_label.get("ready-for-agent"), Some(&1));
        assert_eq!(buckets.by_label.get("needs-triage"), None);
        assert_eq!(buckets.other, 2);
    }

    #[test]
    fn classify_checks_covers_rollup_combinations() {
        assert_eq!(classify_checks(&[]), Checks::None);
        assert_eq!(
            classify_checks(&[rollup(Some("COMPLETED"), Some("SUCCESS"), None)]),
            Checks::Pass
        );
        assert_eq!(
            classify_checks(&[rollup(None, None, Some("SUCCESS"))]),
            Checks::Pass
        );
        assert_eq!(
            classify_checks(&[rollup(Some("COMPLETED"), Some("FAILURE"), None)]),
            Checks::Fail
        );
        assert_eq!(
            classify_checks(&[rollup(None, None, Some("ERROR"))]),
            Checks::Fail
        );
        assert_eq!(
            classify_checks(&[rollup(Some("IN_PROGRESS"), None, None)]),
            Checks::Pending
        );
        assert_eq!(
            classify_checks(&[rollup(None, None, Some("PENDING"))]),
            Checks::Pending
        );
        // A failure beats a still-running check: the PR is not mergeable.
        assert_eq!(
            classify_checks(&[
                rollup(Some("IN_PROGRESS"), None, None),
                rollup(Some("COMPLETED"), Some("CANCELLED"), None),
            ]),
            Checks::Fail
        );
        // Skipped/neutral conclusions count as passing.
        assert_eq!(
            classify_checks(&[
                rollup(Some("COMPLETED"), Some("SUCCESS"), None),
                rollup(Some("COMPLETED"), Some("SKIPPED"), None),
            ]),
            Checks::Pass
        );
    }

    #[test]
    fn classify_review_maps_gh_decisions() {
        assert_eq!(classify_review("APPROVED"), Review::Approved);
        assert_eq!(
            classify_review("CHANGES_REQUESTED"),
            Review::ChangesRequested
        );
        assert_eq!(classify_review("REVIEW_REQUIRED"), Review::Required);
        assert_eq!(classify_review(""), Review::None);
    }

    #[test]
    fn sync_slots_registers_new_cwds_and_keeps_existing() {
        let rows = agent_rows(vec![
            info("working", "/dev/a/one"),
            info("idle", "/dev/a/two"),
        ]);
        let mut slots = HashMap::new();
        let mut lookup = |_: &Path| Remote::NoOrigin;
        sync_slots(&rows, &mut slots, &mut lookup);
        assert_eq!(slots.len(), 2);
        // A second sync must not re-resolve (the cache policy's job, but the
        // entry guard is what makes re-resolution impossible here).
        slots.get_mut(Path::new("/dev/a/one")).unwrap().remote = Remote::GitHub {
            org: "o".to_string(),
            repo: "o/one".to_string(),
        };
        sync_slots(&rows, &mut slots, &mut lookup);
        assert!(matches!(
            slots[Path::new("/dev/a/one")].remote,
            Remote::GitHub { .. }
        ));
    }

    #[test]
    fn sync_slots_prunes_repos_no_longer_live() {
        let mut slots = HashMap::new();
        let mut gone = github_slot("/dev/a/gone", "o/gone");
        record_fetch(
            &mut gone,
            Ok(vec![issue(&["blocked"])]),
            Ok(vec![]),
            Instant::now(),
        );
        slots.insert(PathBuf::from("/dev/a/gone"), gone);
        slots.insert(
            PathBuf::from("/dev/a/kept"),
            github_slot("/dev/a/kept", "o/kept"),
        );
        // Only /dev/a/kept is live now.
        let rows = agent_rows(vec![info("working", "/dev/a/kept")]);
        let mut lookup = |_: &Path| Remote::NoOrigin;
        sync_slots(&rows, &mut slots, &mut lookup);
        assert!(!slots.contains_key(Path::new("/dev/a/gone")));
        assert!(slots.contains_key(Path::new("/dev/a/kept")));
    }

    #[test]
    fn due_github_repos_selects_only_due_github_slots() {
        let now = Instant::now();
        let mut slots = HashMap::new();
        // Never fetched: due.
        slots.insert(
            PathBuf::from("/dev/a/new"),
            github_slot("/dev/a/new", "o/new"),
        );
        // Non-GitHub: never due.
        slots.insert(
            PathBuf::from("/dev/a/plain"),
            RepoSlot::new(PathBuf::from("/dev/a/plain"), Remote::NoOrigin),
        );
        // Fresh: inside the TTL, not due.
        let mut fresh = github_slot("/dev/a/fresh", "o/fresh");
        record_fetch(
            &mut fresh,
            Ok(vec![]),
            Ok(vec![]),
            now - Duration::from_secs(10),
        );
        slots.insert(PathBuf::from("/dev/a/fresh"), fresh);
        // Stale: past the TTL, due.
        let mut stale = github_slot("/dev/a/stale", "o/stale");
        record_fetch(
            &mut stale,
            Ok(vec![]),
            Ok(vec![]),
            now - GH_TTL - Duration::from_secs(1),
        );
        slots.insert(PathBuf::from("/dev/a/stale"), stale);

        let mut due = due_github_repos(&slots, now, GH_TTL);
        due.sort();
        assert_eq!(
            due,
            vec![
                (PathBuf::from("/dev/a/new"), "o/new".to_string()),
                (PathBuf::from("/dev/a/stale"), "o/stale".to_string()),
            ]
        );
    }

    #[test]
    fn record_fetch_stores_tracker_results_errors_included() {
        let gh = FakeTracker {
            issues: Err("gh: auth required".to_string()),
            prs: Ok(vec![]),
        };
        let mut slot = github_slot("/dev/a/one", "o/one");
        let issues = gh.open_issues("o/one").map_err(|e| e.to_string());
        let prs = gh.open_prs("o/one").map_err(|e| e.to_string());
        record_fetch(&mut slot, issues, prs, Instant::now());
        let view = repo_view(&slot, 1);
        assert_eq!(view.issues, Some(Err("gh: auth required".to_string())));
        assert_eq!(view.prs, Some(Ok(vec![])));
        assert!(view.fetched_at.is_some());
    }

    #[test]
    fn dashboard_groups_repos_by_org_with_fallback_last() {
        let rows = agent_rows(vec![
            info("working", "/dev/a/zeta"),
            info("idle", "/dev/a/beta"),
            info("working", "/dev/a/beta"), // two agents share a repo
            info("working", "/dev/a/local"),
        ]);
        let mut slots = HashMap::new();
        slots.insert(
            PathBuf::from("/dev/a/zeta"),
            github_slot("/dev/a/zeta", "acme/zeta"),
        );
        slots.insert(
            PathBuf::from("/dev/a/beta"),
            github_slot("/dev/a/beta", "acme/beta"),
        );
        slots.insert(
            PathBuf::from("/dev/a/local"),
            RepoSlot::new(PathBuf::from("/dev/a/local"), Remote::NoOrigin),
        );
        let dash = dashboard(Ok(rows), &slots);
        assert_eq!(dash.agents.unwrap().len(), 4);
        let orgs: Vec<&str> = dash.groups.iter().map(|g| g.org.as_str()).collect();
        assert_eq!(orgs, vec!["acme", NO_ORG_GROUP]);
        let acme = &dash.groups[0];
        let keys: Vec<&str> = acme.repos.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["acme/beta", "acme/zeta"]);
        assert_eq!(acme.repos[0].agent_count, 2);
        assert_eq!(dash.groups[1].repos[0].key, "local");
    }

    #[test]
    fn dashboard_with_herdr_error_has_no_repo_section() {
        let mut slots = HashMap::new();
        slots.insert(
            PathBuf::from("/dev/a/one"),
            github_slot("/dev/a/one", "o/one"),
        );
        let dash = dashboard(Err("server not running".to_string()), &slots);
        assert_eq!(dash.agents.unwrap_err(), "server not running");
        assert!(dash.groups.is_empty());
    }

    #[test]
    fn dashboard_with_zero_agents_is_an_explicit_empty_state() {
        let dash = dashboard(Ok(vec![]), &HashMap::new());
        assert!(dash.agents.unwrap().is_empty());
        assert!(dash.groups.is_empty());
    }
}
