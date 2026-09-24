//! Dashboard state derivation. Everything here is pure data transformation
//! over the `herd` and `github` fetch layers — no process spawning, no
//! terminal — so agent grouping, label bucketing, PR classification, and the
//! GitHub TTL cache are all unit-testable with fakes.

use std::collections::BTreeMap;
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

/// Identity of a cached repo slot. GitHub repos key by "owner/name" so
/// several agent cwds/worktrees for the same repo share one cache entry,
/// one `gh` fetch, and one rendered row with an aggregated agent count.
/// Everything else keys by cwd: without a GitHub identity there is nothing
/// stable to dedupe by, and distinct local paths must never collapse into
/// one another just because their basenames match.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SlotKey {
    GitHub(String),
    Local(PathBuf),
}

impl SlotKey {
    fn for_remote(cwd: &Path, remote: &Remote) -> Self {
        match remote {
            Remote::GitHub { repo, .. } => Self::GitHub(repo.clone()),
            _ => Self::Local(cwd.to_path_buf()),
        }
    }
}

/// Cached per-repo GitHub state. Created when the live agent set first
/// names a slot key and pruned when nothing live references it anymore;
/// raw fetch results (including errors) are cached so a flaky `gh` is
/// retried on the refresh TTL, not continuously. `generation` is bumped
/// on every creation or reclassification so a fetch that started against
/// a since-pruned or reclassified slot cannot write its result into the
/// replacement.
pub struct RepoSlot {
    pub remote: Remote,
    pub generation: u64,
    /// Live agents whose cwd resolves to this slot, aggregated across every
    /// worktree that shares the repo; refreshed on every successful sync.
    pub agent_count: usize,
    pub issues: Option<Result<Vec<Issue>, String>>,
    pub prs: Option<Result<Vec<PullRequest>, String>>,
    pub fetched_at: Option<Instant>,
}

impl RepoSlot {
    fn new(remote: Remote, generation: u64, agent_count: usize) -> Self {
        Self {
            remote,
            generation,
            agent_count,
            issues: None,
            prs: None,
            fetched_at: None,
        }
    }
}

/// A unit of due GitHub work: which slot to write back to, which repo to
/// fetch, and the slot generation the fetch started against. The identity
/// plus generation make a completion that lands after the slot was pruned
/// and re-created — or after the whole cache was invalidated — recognizably
/// stale, so an old result can never overwrite a current entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchTicket {
    pub key: SlotKey,
    /// "owner/name", as `gh --repo` expects it.
    pub repo: String,
    pub generation: u64,
}

/// The live repo cache shared between the agent worker (syncs on each
/// successful herdr poll, invalidates on failure) and the GitHub worker
/// (reads due tickets, writes results back). Iteration is ordered so the
/// rendered board is deterministic. GitHub work and cache only ever track
/// repos currently in use.
#[derive(Default)]
pub struct SlotCache {
    slots: BTreeMap<SlotKey, RepoSlot>,
    generation: u64,
}

impl SlotCache {
    /// Reconciles the cache with the live agent set: resolves every live
    /// cwd through `lookup` (a `RemoteCache` in production, so this is a
    /// map hit after first sight), refreshes per-slot agent counts, prunes
    /// slots nothing live references anymore, and creates slots for newly
    /// seen keys with a fresh generation. A cwd reclassified to or from a
    /// GitHub identity maps to a different key, so its old slot is pruned
    /// and the new one starts empty with a new generation — an in-flight
    /// fetch for the old identity is rejected on writeback. A local cwd
    /// that keeps its key but moves among NoOrigin/NonGitHub/GitError is
    /// reclassified in place: the slot is replaced wholesale (fresh
    /// generation, no cached state) so nothing from the old
    /// classification survives.
    pub fn sync(&mut self, rows: &[AgentRow], lookup: &mut dyn FnMut(&Path) -> Remote) {
        let mut live: BTreeMap<SlotKey, (Remote, usize)> = BTreeMap::new();
        for row in rows {
            let remote = lookup(&row.cwd);
            let entry = live
                .entry(SlotKey::for_remote(&row.cwd, &remote))
                .or_insert_with(|| (remote, 0));
            entry.1 += 1;
        }
        self.slots.retain(|key, _| live.contains_key(key));
        for (key, (remote, count)) in live {
            match self.slots.get_mut(&key) {
                Some(slot) if slot.remote == remote => slot.agent_count = count,
                Some(slot) => {
                    self.generation += 1;
                    *slot = RepoSlot::new(remote, self.generation, count);
                }
                None => {
                    self.generation += 1;
                    self.slots
                        .insert(key, RepoSlot::new(remote, self.generation, count));
                }
            }
        }
    }

    /// Drops every slot. Called when the herdr poll fails: without a live
    /// agent set there is no current repo set, so nothing from the last
    /// good poll may stay eligible for GitHub work, and completions of
    /// fetches already in flight find no slot to land in. The dashboard
    /// still renders the herdr error itself from the snapshot.
    pub fn invalidate(&mut self) {
        self.slots.clear();
    }

    /// GitHub slots due for a refetch, as tickets the GitHub worker can
    /// fetch without holding the lock. Non-GitHub slots are never listed —
    /// their inline state explains why — and a slot inside its TTL keeps
    /// its cached data, errors included, so a flaky `gh` is not retried on
    /// every agent poll.
    pub fn due_github_repos(&self, now: Instant, ttl: Duration) -> Vec<FetchTicket> {
        self.slots
            .iter()
            .filter_map(|(key, slot)| {
                let Remote::GitHub { repo, .. } = &slot.remote else {
                    return None;
                };
                let due = slot
                    .fetched_at
                    .is_none_or(|at| now.duration_since(at) >= ttl);
                due.then(|| FetchTicket {
                    key: key.clone(),
                    repo: repo.clone(),
                    generation: slot.generation,
                })
            })
            .collect()
    }

    /// Records a finished fetch — errors included — so the board shows what
    /// `gh` actually said rather than a spinner forever. Returns false and
    /// writes nothing when the ticket is stale: the slot is gone (pruned or
    /// invalidated mid-fetch) or was re-created since the fetch started
    /// (generation bumped or repo identity changed), because then the
    /// result belongs to a repo set that no longer exists.
    pub fn record_fetch(
        &mut self,
        ticket: &FetchTicket,
        issues: Result<Vec<Issue>, String>,
        prs: Result<Vec<PullRequest>, String>,
        at: Instant,
    ) -> bool {
        let Some(slot) = self.slots.get_mut(&ticket.key) else {
            return false;
        };
        let current = slot.generation == ticket.generation
            && matches!(&slot.remote, Remote::GitHub { repo, .. } if *repo == ticket.repo);
        if !current {
            return false;
        }
        slot.issues = Some(issues);
        slot.prs = Some(prs);
        slot.fetched_at = Some(at);
        true
    }
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

pub fn repo_view(key: &SlotKey, slot: &RepoSlot) -> RepoView {
    // Display name for local states, where no owner/repo identity exists:
    // the full cwd, not the basename. Local slots are keyed by complete
    // path precisely because /a/work and /b/work are unrelated repos, and
    // rendering only the basename would make them identical.
    let dir = match key {
        SlotKey::Local(cwd) => cwd.display().to_string(),
        SlotKey::GitHub(repo) => repo.clone(),
    };
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
        agent_count: slot.agent_count,
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

/// The full board: agent rows (or the herdr error) plus the live repo
/// slots, grouped by org. A herdr failure yields no repo section at all —
/// the agent worker invalidates the slot cache on error, and without
/// agents there is nothing to discover from — and the UI renders the
/// error in place of the whole board.
pub struct Dashboard {
    pub agents: Result<Vec<AgentRow>, String>,
    pub groups: Vec<OrgGroup>,
}

pub fn dashboard(agents: Result<Vec<AgentRow>, String>, cache: &SlotCache) -> Dashboard {
    let rows = match agents {
        Err(e) => {
            return Dashboard {
                agents: Err(e),
                groups: Vec::new(),
            }
        }
        Ok(rows) => rows,
    };
    let views: Vec<RepoView> = cache
        .slots
        .iter()
        .map(|(key, slot)| repo_view(key, slot))
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

    fn github_remote(repo: &str) -> Remote {
        Remote::GitHub {
            org: repo.split('/').next().unwrap().to_string(),
            repo: repo.to_string(),
        }
    }

    fn github_key(repo: &str) -> SlotKey {
        SlotKey::GitHub(repo.to_string())
    }

    /// The single due ticket for `repo`, panicking unless there is exactly
    /// one — which is itself the one-fetch-per-repo invariant.
    fn due_ticket(cache: &SlotCache, repo: &str, now: Instant) -> FetchTicket {
        let due = cache.due_github_repos(now, GH_TTL);
        let matching: Vec<_> = due.iter().filter(|t| t.repo == repo).collect();
        assert_eq!(matching.len(), 1, "exactly one due ticket for {repo}");
        matching[0].clone()
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
    fn sync_registers_new_cwds_and_keeps_fetch_state_across_polls() {
        let rows = agent_rows(vec![
            info("working", "/dev/a/one"),
            info("idle", "/dev/a/two"),
        ]);
        let mut cache = SlotCache::default();
        let mut lookup = |cwd: &Path| match cwd.to_str().unwrap() {
            "/dev/a/one" => github_remote("o/one"),
            _ => Remote::NoOrigin,
        };
        cache.sync(&rows, &mut lookup);
        assert_eq!(cache.slots.len(), 2);
        let ticket = due_ticket(&cache, "o/one", Instant::now());
        assert!(cache.record_fetch(&ticket, Ok(vec![]), Ok(vec![]), Instant::now()));
        // Re-polling the same live set keeps the slot, its generation, and
        // its cached fetch data; the count is simply refreshed.
        cache.sync(&rows, &mut lookup);
        let slot = &cache.slots[&github_key("o/one")];
        assert_eq!(slot.generation, ticket.generation);
        assert!(slot.fetched_at.is_some());
    }

    #[test]
    fn sync_prunes_repos_no_longer_live() {
        let mut cache = SlotCache::default();
        let mut lookup = |cwd: &Path| match cwd.to_str().unwrap() {
            "/dev/a/gone" => github_remote("o/gone"),
            _ => github_remote("o/kept"),
        };
        cache.sync(
            &agent_rows(vec![
                info("working", "/dev/a/gone"),
                info("idle", "/dev/a/kept"),
            ]),
            &mut lookup,
        );
        assert!(cache.slots.contains_key(&github_key("o/gone")));
        // Only /dev/a/kept is live now.
        cache.sync(
            &agent_rows(vec![info("working", "/dev/a/kept")]),
            &mut lookup,
        );
        assert!(!cache.slots.contains_key(&github_key("o/gone")));
        assert!(cache.slots.contains_key(&github_key("o/kept")));
    }

    #[test]
    fn two_cwds_for_one_repo_share_one_slot_fetch_and_row() {
        // A main checkout and a worktree of the same GitHub repo.
        let rows = agent_rows(vec![
            info("working", "/dev/a/repo"),
            info("working", "/dev/wt/repo-feature"),
            info("idle", "/dev/wt/repo-review"),
        ]);
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("acme/repo");
        cache.sync(&rows, &mut lookup);
        assert_eq!(cache.slots.len(), 1, "one cache entry for the repo");
        assert_eq!(cache.slots[&github_key("acme/repo")].agent_count, 3);
        // Exactly one due fetch for the repo, not one per cwd.
        let due = cache.due_github_repos(Instant::now(), GH_TTL);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].repo, "acme/repo");
        // And exactly one rendered row, with the aggregated agent count.
        let dash = dashboard(Ok(rows), &cache);
        assert_eq!(dash.groups.len(), 1);
        assert_eq!(dash.groups[0].repos.len(), 1);
        let view = &dash.groups[0].repos[0];
        assert_eq!(view.key, "acme/repo");
        assert_eq!(view.agent_count, 3);
    }

    #[test]
    fn local_slot_tracks_remote_reclassification_with_new_generation() {
        let remote = std::cell::RefCell::new(Remote::NoOrigin);
        let mut lookup = |_: &Path| remote.borrow().clone();
        let rows = agent_rows(vec![info("working", "/dev/a/work")]);
        let key = SlotKey::Local(PathBuf::from("/dev/a/work"));
        let mut cache = SlotCache::default();
        cache.sync(&rows, &mut lookup);
        let first = cache.slots[&key].generation;
        assert_eq!(cache.slots[&key].remote, Remote::NoOrigin);
        // State cached under the old classification (injected: production
        // never fetches for a local slot, but the reset must not depend on
        // that) must not survive reclassification.
        let slot = cache.slots.get_mut(&key).unwrap();
        slot.issues = Some(Ok(vec![]));
        slot.fetched_at = Some(Instant::now());
        // The same cwd's origin now points at a non-GitHub host (RemoteCache
        // TTL expired and git answered differently): the slot must follow.
        *remote.borrow_mut() = Remote::NonGitHub {
            host: "gitlab.com".to_string(),
        };
        cache.sync(&rows, &mut lookup);
        let slot = &cache.slots[&key];
        assert_eq!(
            slot.remote,
            Remote::NonGitHub {
                host: "gitlab.com".to_string(),
            }
        );
        assert_ne!(slot.generation, first, "reclassification bumps generation");
        assert!(slot.issues.is_none(), "old classification's state cleared");
        assert!(slot.fetched_at.is_none());
        assert_eq!(slot.agent_count, 1);
        // A further move to a git-error state is tracked the same way.
        *remote.borrow_mut() = Remote::GitError("not a repo".to_string());
        cache.sync(&rows, &mut lookup);
        assert_eq!(
            cache.slots[&key].remote,
            Remote::GitError("not a repo".to_string())
        );
        // An unchanged classification keeps slot, generation, and state.
        let stable = cache.slots[&key].generation;
        cache.sync(&rows, &mut lookup);
        assert_eq!(cache.slots[&key].generation, stable);
    }

    #[test]
    fn invalidate_clears_live_set_and_rejects_in_flight_completions() {
        let rows = agent_rows(vec![info("working", "/dev/a/one")]);
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(&rows, &mut lookup);
        let ticket = due_ticket(&cache, "o/one", Instant::now());
        // The herdr poll fails: the live repo set is invalidated.
        cache.invalidate();
        assert!(
            cache.due_github_repos(Instant::now(), GH_TTL).is_empty(),
            "no repo stays eligible for gh work"
        );
        // A fetch that was in flight when the poll failed has nowhere to land.
        assert!(!cache.record_fetch(&ticket, Ok(vec![]), Ok(vec![]), Instant::now()));
        // The dashboard renders the herdr error with no repo section.
        let dash = dashboard(Err("server not running".to_string()), &cache);
        assert_eq!(dash.agents.unwrap_err(), "server not running");
        assert!(dash.groups.is_empty());
        // The next successful poll re-populates and re-fetches from scratch.
        cache.sync(&rows, &mut lookup);
        assert_eq!(cache.due_github_repos(Instant::now(), GH_TTL).len(), 1);
    }

    #[test]
    fn stale_ticket_after_remove_and_readd_is_rejected() {
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(
            &agent_rows(vec![info("working", "/dev/a/one")]),
            &mut lookup,
        );
        let old = due_ticket(&cache, "o/one", Instant::now());
        // The repo leaves the session entirely...
        cache.sync(&[], &mut lookup);
        assert!(cache.slots.is_empty());
        // ...then a different worktree brings the same repo back.
        cache.sync(&agent_rows(vec![info("idle", "/dev/wt/one")]), &mut lookup);
        // The old in-flight completion must not overwrite the new entry.
        assert!(!cache.record_fetch(
            &old,
            Ok(vec![issue(&["blocked"])]),
            Ok(vec![]),
            Instant::now()
        ));
        let slot = &cache.slots[&github_key("o/one")];
        assert!(slot.issues.is_none(), "stale result was not recorded");
        assert_ne!(slot.generation, old.generation);
        // The new generation's own fetch is accepted.
        let current = due_ticket(&cache, "o/one", Instant::now());
        assert!(cache.record_fetch(&current, Ok(vec![]), Ok(vec![]), Instant::now()));
        assert!(cache.slots[&github_key("o/one")].fetched_at.is_some());
    }

    #[test]
    fn stale_ticket_after_reclassification_is_rejected() {
        let remote = std::cell::RefCell::new(github_remote("o/before"));
        let mut lookup = |_: &Path| remote.borrow().clone();
        let rows = agent_rows(vec![info("working", "/dev/a/one")]);
        let mut cache = SlotCache::default();
        cache.sync(&rows, &mut lookup);
        let old = due_ticket(&cache, "o/before", Instant::now());
        // The cwd's origin now points at a different repo (RemoteCache TTL
        // expired and git answered differently).
        *remote.borrow_mut() = github_remote("o/after");
        cache.sync(&rows, &mut lookup);
        assert!(!cache.slots.contains_key(&github_key("o/before")));
        assert!(cache.slots.contains_key(&github_key("o/after")));
        assert!(!cache.record_fetch(&old, Ok(vec![]), Ok(vec![]), Instant::now()));
        let due = cache.due_github_repos(Instant::now(), GH_TTL);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].repo, "o/after");
    }

    #[test]
    fn due_github_repos_selects_only_due_github_slots() {
        let now = Instant::now();
        let rows = agent_rows(vec![
            info("working", "/dev/a/new"),
            info("working", "/dev/a/plain"),
            info("working", "/dev/a/fresh"),
            info("working", "/dev/a/stale"),
        ]);
        let mut cache = SlotCache::default();
        let mut lookup = |cwd: &Path| match cwd.to_str().unwrap() {
            "/dev/a/new" => github_remote("o/new"),
            "/dev/a/fresh" => github_remote("o/fresh"),
            "/dev/a/stale" => github_remote("o/stale"),
            _ => Remote::NoOrigin, // never due
        };
        cache.sync(&rows, &mut lookup);
        // Fresh: completed inside the TTL. Stale: completed past it.
        let fresh = due_ticket(&cache, "o/fresh", now);
        assert!(cache.record_fetch(
            &fresh,
            Ok(vec![]),
            Ok(vec![]),
            now - Duration::from_secs(10)
        ));
        let stale = due_ticket(&cache, "o/stale", now);
        assert!(cache.record_fetch(
            &stale,
            Ok(vec![]),
            Ok(vec![]),
            now - GH_TTL - Duration::from_secs(1),
        ));
        let mut due: Vec<String> = cache
            .due_github_repos(now, GH_TTL)
            .into_iter()
            .map(|t| t.repo)
            .collect();
        due.sort();
        assert_eq!(due, vec!["o/new".to_string(), "o/stale".to_string()]);
    }

    #[test]
    fn record_fetch_stores_tracker_results_errors_included() {
        let gh = FakeTracker {
            issues: Err("gh: auth required".to_string()),
            prs: Ok(vec![]),
        };
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(
            &agent_rows(vec![info("working", "/dev/a/one")]),
            &mut lookup,
        );
        let ticket = due_ticket(&cache, "o/one", Instant::now());
        let issues = gh.open_issues(&ticket.repo).map_err(|e| e.to_string());
        let prs = gh.open_prs(&ticket.repo).map_err(|e| e.to_string());
        // A current-generation completion is accepted.
        assert!(cache.record_fetch(&ticket, issues, prs, Instant::now()));
        let view = repo_view(&ticket.key, &cache.slots[&ticket.key]);
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
        let mut cache = SlotCache::default();
        let mut lookup = |cwd: &Path| match cwd.to_str().unwrap() {
            "/dev/a/zeta" => github_remote("acme/zeta"),
            "/dev/a/beta" => github_remote("acme/beta"),
            _ => Remote::NoOrigin,
        };
        cache.sync(&rows, &mut lookup);
        let dash = dashboard(Ok(rows), &cache);
        assert_eq!(dash.agents.unwrap().len(), 4);
        let orgs: Vec<&str> = dash.groups.iter().map(|g| g.org.as_str()).collect();
        assert_eq!(orgs, vec!["acme", NO_ORG_GROUP]);
        let acme = &dash.groups[0];
        let keys: Vec<&str> = acme.repos.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["acme/beta", "acme/zeta"]);
        assert_eq!(acme.repos[0].agent_count, 2);
        assert_eq!(dash.groups[1].repos[0].key, "/dev/a/local");
    }

    #[test]
    fn non_github_states_do_not_collapse_by_basename() {
        // Distinct local paths with the same basename are unrelated repos
        // and must each stay visible; git-error and non-GitHub states keep
        // their own rows and grouping too.
        let rows = agent_rows(vec![
            info("working", "/dev/x/work"),
            info("idle", "/dev/y/work"),
            info("working", "/dev/z/broken"),
            info("idle", "/dev/w/mirror"),
        ]);
        let mut cache = SlotCache::default();
        let mut lookup = |cwd: &Path| match cwd.to_str().unwrap() {
            "/dev/z/broken" => Remote::GitError("not a repo".to_string()),
            "/dev/w/mirror" => Remote::NonGitHub {
                host: "gitlab.com".to_string(),
            },
            _ => Remote::NoOrigin,
        };
        cache.sync(&rows, &mut lookup);
        assert_eq!(cache.slots.len(), 4, "no collapse by basename");
        let dash = dashboard(Ok(rows), &cache);
        let orgs: Vec<&str> = dash.groups.iter().map(|g| g.org.as_str()).collect();
        assert_eq!(orgs, vec!["gitlab.com", NO_ORG_GROUP]);
        let no_org = &dash.groups[1].repos;
        let keys: Vec<&str> = no_org.iter().map(|r| r.key.as_str()).collect();
        // Rendered by full cwd: the same-basename repos stay visibly
        // distinct, not two identical "work" rows.
        assert_eq!(keys, vec!["/dev/x/work", "/dev/y/work", "/dev/z/broken"]);
        assert_eq!(dash.groups[0].repos[0].key, "/dev/w/mirror (gitlab.com)");
    }

    #[test]
    fn dashboard_with_herdr_error_has_no_repo_section() {
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(
            &agent_rows(vec![info("working", "/dev/a/one")]),
            &mut lookup,
        );
        let dash = dashboard(Err("server not running".to_string()), &cache);
        assert_eq!(dash.agents.unwrap_err(), "server not running");
        assert!(dash.groups.is_empty());
    }

    #[test]
    fn dashboard_with_zero_agents_is_an_explicit_empty_state() {
        let dash = dashboard(Ok(vec![]), &SlotCache::default());
        assert!(dash.agents.unwrap().is_empty());
        assert!(dash.groups.is_empty());
    }
}
