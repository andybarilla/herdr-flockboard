//! Dashboard state derivation. Everything here is pure data transformation
//! over the `herd`, `github`, and `journal` fetch layers — no process
//! spawning, no terminal, no filesystem — so agent grouping, label
//! bucketing, PR classification, workflow-stage assembly, cross-repo inbox
//! and activity-feed aggregation, and the GitHub TTL cache are all
//! unit-testable with fakes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::feed::{self, FeedEvent};
use crate::git_org::Remote;
use crate::github::{CheckRollup, Issue, PullRequest};
use crate::herd::AgentInfo;
use crate::inbox::{self, InboxItem};
use crate::journal::{self, LiveSet};

/// How often the agent board re-polls `herdr agent list`.
pub const AGENT_POLL: Duration = Duration::from_secs(3);

/// How long per-repo GitHub data is trusted before an asynchronous refetch.
/// Short enough that a queue change shows up within a minute, long enough
/// that a busy session does not hammer `gh`.
pub const GH_TTL: Duration = Duration::from_secs(45);

/// How long a repo slot is retained after its last live agent disappears.
/// A died sole agent must not take the repo's row — and with it the
/// `unknown (run may have died)` stage — off the board immediately, so a
/// freshly departed slot keeps its journal, cwds, and cached GitHub data
/// (rendered with zero live agents) until this bounded grace period
/// elapses or a terminal event supersedes the died heuristic.
pub const GONE_GRACE: Duration = Duration::from_secs(120);

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
/// names a slot key and pruned when nothing live has referenced it for
/// longer than `GONE_GRACE` (a died sole agent leaves the repo on the
/// board briefly so its died-mid-run stage can render); raw fetch results
/// (including errors) are cached so a flaky `gh` is
/// retried on the refresh TTL, not continuously. `generation` is bumped
/// on every creation or reclassification so a fetch that started against
/// a since-pruned or reclassified slot cannot write its result into the
/// replacement.
pub struct RepoSlot {
    pub remote: Remote,
    pub generation: u64,
    /// Live agents whose cwd resolves to this slot, aggregated across every
    /// worktree that shares the repo; refreshed on every successful sync.
    /// Zero on a retained slot whose agents have all disappeared.
    pub agent_count: usize,
    /// Every live cwd that resolves to this slot (main checkout plus any
    /// worktrees), sorted for deterministic journal reads. The Flock
    /// journal lives in whichever checkout a supervisor ran in. Kept from
    /// the last live sync on a retained slot, so its journal stays
    /// readable through the grace period.
    pub cwds: BTreeSet<PathBuf>,
    /// When the slot was first seen with no live agents, if it is
    /// currently in the `GONE_GRACE` retention window.
    pub gone_since: Option<Instant>,
    /// The repo's parsed Flock journal, re-read after each successful sync.
    /// Empty when no live checkout has a readable journal. Bounded at the
    /// reader: the production reader (`journal::read_repo_events`) retains
    /// only the newest `journal::MAX_JOURNAL_EVENTS` events, so this cache
    /// stays flat as append-only journals grow.
    pub journal: Arc<Vec<journal::Event>>,
    pub issues: Option<Result<Vec<Issue>, String>>,
    pub prs: Option<Result<Vec<PullRequest>, String>>,
    pub fetched_at: Option<Instant>,
}

impl RepoSlot {
    fn new(remote: Remote, generation: u64, agent_count: usize, cwds: BTreeSet<PathBuf>) -> Self {
        Self {
            remote,
            generation,
            agent_count,
            cwds,
            gone_since: None,
            journal: Arc::new(Vec::new()),
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
/// rendered board is deterministic. GitHub work and cache track repos in
/// use plus recently departed ones inside the `GONE_GRACE` window.
#[derive(Default)]
pub struct SlotCache {
    slots: BTreeMap<SlotKey, RepoSlot>,
    generation: u64,
}

impl SlotCache {
    /// Reconciles the cache with the live agent set: resolves every live
    /// cwd through `lookup` (a `RemoteCache` in production, so this is a
    /// map hit after first sight), refreshes per-slot agent counts and cwd
    /// sets, and creates slots for newly seen keys with a fresh
    /// generation. Slots nothing live references anymore are not pruned
    /// immediately: they enter the `GONE_GRACE` retention window (agent
    /// count zeroed, journal/cwds/fetch state kept) so a repo whose sole
    /// supervising agent just died still renders — including its
    /// died-mid-run issue stages — and only once the grace period has
    /// elapsed are they pruned. A cwd reclassified to or from a
    /// GitHub identity maps to a different key; its cwds are still live,
    /// so its old slot is pruned immediately (no retention) and the new
    /// one starts empty with a new generation — an in-flight fetch for
    /// the old identity is rejected on writeback. A local cwd
    /// that keeps its key but moves among NoOrigin/NonGitHub/GitError is
    /// reclassified in place: the slot is replaced wholesale (fresh
    /// generation, no cached state) so nothing from the old
    /// classification survives.
    pub fn sync(
        &mut self,
        rows: &[AgentRow],
        lookup: &mut dyn FnMut(&Path) -> Remote,
        now: Instant,
    ) {
        let mut live: BTreeMap<SlotKey, (Remote, usize, BTreeSet<PathBuf>)> = BTreeMap::new();
        let mut live_cwds: BTreeSet<PathBuf> = BTreeSet::new();
        for row in rows {
            let remote = lookup(&row.cwd);
            let entry = live
                .entry(SlotKey::for_remote(&row.cwd, &remote))
                .or_insert_with(|| (remote, 0, BTreeSet::new()));
            entry.1 += 1;
            entry.2.insert(row.cwd.clone());
            live_cwds.insert(row.cwd.clone());
        }
        // Departed slots enter the grace window on first absence and are
        // pruned only once it has fully elapsed. A slot whose cwds are all
        // still live under a different key was reclassified, not
        // abandoned, and is pruned immediately.
        self.slots.retain(|key, slot| {
            if live.contains_key(key) {
                return true;
            }
            if slot.cwds.iter().any(|cwd| live_cwds.contains(cwd)) {
                return false;
            }
            match slot.gone_since {
                None => {
                    slot.gone_since = Some(now);
                    slot.agent_count = 0;
                    true
                }
                Some(since) => now.duration_since(since) < GONE_GRACE,
            }
        });
        for (key, (remote, count, cwds)) in live {
            match self.slots.get_mut(&key) {
                Some(slot) if slot.remote == remote => {
                    slot.agent_count = count;
                    slot.cwds = cwds;
                    slot.gone_since = None;
                }
                Some(slot) => {
                    self.generation += 1;
                    *slot = RepoSlot::new(remote, self.generation, count, cwds);
                }
                None => {
                    self.generation += 1;
                    self.slots
                        .insert(key, RepoSlot::new(remote, self.generation, count, cwds));
                }
            }
        }
    }

    /// Drops every slot, retained ones included. Called when the herdr
    /// poll fails: without a live
    /// agent set there is no current repo set, so nothing from the last
    /// good poll may stay eligible for GitHub work, and completions of
    /// fetches already in flight find no slot to land in. The dashboard
    /// still renders the herdr error itself from the snapshot.
    pub fn invalidate(&mut self) {
        self.slots.clear();
    }

    /// Re-reads every slot's Flock journal from its live checkouts.
    /// Called after each successful sync: journals are small and the reader
    /// is tolerant (missing file, truncated tail, unknown events), so a
    /// fresh read per agent poll keeps issue stages and the activity feed
    /// current without a second cache/TTL layer. Retained (grace-window)
    /// slots keep reading from their last live checkouts, so a died run's
    /// terminal event is picked up as soon as a supervisor writes it.
    /// Non-GitHub slots have no issue/PR surface, but their journals still
    /// feed the cross-repo activity feed, so they are read too. Memory and
    /// parse cost stay bounded as journals grow: the production reader
    /// (`journal::read_repo_events`) parses and retains only the newest
    /// `journal::MAX_JOURNAL_EVENTS` lines per repo — far above the feed's
    /// per-repo view cap, so in-flight issues keep their stage/inbox facts
    /// (see the constant's doc for the residual edge).
    pub fn refresh_journals(&mut self, reader: &dyn Fn(&BTreeSet<PathBuf>) -> Vec<journal::Event>) {
        for slot in self.slots.values_mut() {
            slot.journal = Arc::new(reader(&slot.cwds));
        }
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

/// One rendered issue row: the tracker issue plus its derived Flock
/// workflow stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssueRow {
    pub number: u64,
    pub title: String,
    pub stage: journal::Stage,
}

/// The open PR that belongs to an issue, if any: an explicit `pr` reference
/// from the issue's journal events first, then branches named in its
/// events, then the configured `flock/issue-<n>-<slug>` branch pattern. The
/// pattern leg is what gives issues with no journal events their inferred
/// `pr open` stage. Shared with the inbox derivation, which keys
/// PR-state categories off the same correlation.
pub(crate) fn correlated_pr<'a>(
    number: u64,
    events: &[journal::Event],
    prs: Option<&'a [PullRequest]>,
) -> Option<&'a PullRequest> {
    let prs = prs?;
    if let Some(n) = events.iter().filter_map(|e| e.pr).next_back() {
        if let Some(pr) = prs.iter().find(|p| p.number == n) {
            return Some(pr);
        }
    }
    for branch in events.iter().filter_map(|e| e.branch.as_deref()) {
        if let Some(pr) = prs.iter().find(|p| p.head_ref_name == branch) {
            return Some(pr);
        }
    }
    let prefix = format!("flock/issue-{number}-");
    prs.iter().find(|p| p.head_ref_name.starts_with(&prefix))
}

/// Per-issue rows for a repo: each open issue's workflow stage derived from
/// its journal events, its correlated open PR (checks/mergeability via the
/// project-config green classifier), and the live agent set. Sorted
/// active-work-first, then by issue number. `prs` is `None` when PR data
/// was never fetched or errored — stages then derive from events and
/// labels alone, same as a missing journal degrades rather than blanks.
pub fn issue_rows(
    issues: &[Issue],
    prs: Option<&[PullRequest]>,
    events: &[journal::Event],
    live: &LiveSet,
) -> Vec<IssueRow> {
    let mut rows: Vec<IssueRow> = issues
        .iter()
        .map(|issue| {
            let ev: Vec<journal::Event> = events
                .iter()
                .filter(|e| e.issue == Some(issue.number))
                .cloned()
                .collect();
            let pr = correlated_pr(issue.number, &ev, prs);
            let facts = journal::IssueFacts {
                labels: issue.labels.clone(),
                events: ev,
                pr_green: pr.map(|p| journal::classify_green(&p.checks, &p.mergeable)),
            };
            IssueRow {
                number: issue.number,
                title: issue.title.clone(),
                stage: journal::derive_stage(&facts, live),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.stage
            .rank()
            .cmp(&b.stage.rank())
            .then_with(|| a.number.cmp(&b.number))
    });
    rows
}

/// What the board renders for one repo: identity, grouping, fetch state.
pub struct RepoView {
    pub key: String,
    pub org: String,
    pub remote: Remote,
    pub agent_count: usize,
    pub issues: Option<Result<IssueBuckets, String>>,
    /// Per-issue rows with workflow stages; follows the same fetch-state
    /// shape as `issues` (None until the first fetch, Err mirrored inline).
    pub issue_rows: Option<Result<Vec<IssueRow>, String>>,
    pub prs: Option<Result<Vec<PrRow>, String>>,
    pub fetched_at: Option<Instant>,
}

/// A slot's display name, matching the rendered repo row's key:
/// "owner/name" for GitHub repos, the full working-directory path (not
/// the basename — /a/work and /b/work are unrelated repos) for repos
/// without a usable origin, annotated with the host for non-GitHub
/// remotes. The activity feed names its events with it so feed lines and
/// repo rows refer to repos identically.
fn slot_display_key(key: &SlotKey, slot: &RepoSlot) -> String {
    let dir = match key {
        SlotKey::Local(cwd) => cwd.display().to_string(),
        SlotKey::GitHub(repo) => repo.clone(),
    };
    match &slot.remote {
        Remote::GitHub { repo, .. } => repo.clone(),
        Remote::NoOrigin | Remote::GitError(_) => dir,
        Remote::NonGitHub { host } => format!("{dir} ({host})"),
    }
}

pub fn repo_view(key: &SlotKey, slot: &RepoSlot, live: &LiveSet) -> RepoView {
    let org = match &slot.remote {
        Remote::GitHub { org, .. } => org.clone(),
        Remote::NoOrigin | Remote::GitError(_) => NO_ORG_GROUP.to_string(),
        Remote::NonGitHub { host } => host.clone(),
    };
    let key = slot_display_key(key, slot);
    let issues = slot.issues.as_ref().map(|r| match r {
        Ok(v) => Ok(bucket_issues(v)),
        Err(e) => Err(e.clone()),
    });
    // PR fetch failures degrade stage derivation to events + labels; they
    // never blank the issue rows.
    let prs_ok = slot.prs.as_ref().and_then(|r| r.as_ref().ok());
    let issue_rows = slot.issues.as_ref().map(|r| match r {
        Ok(v) => Ok(issue_rows(
            v,
            prs_ok.map(Vec::as_slice),
            &slot.journal,
            live,
        )),
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
        issue_rows,
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
    /// The cross-repo "waiting on you" inbox, priority-sorted. Empty on a
    /// herdr error: with no live repo set there is nothing to derive from.
    pub inbox: Vec<InboxItem>,
    /// The cross-repo activity feed, newest first. Empty on a herdr
    /// error, same as the inbox.
    pub feed: Vec<FeedEvent>,
}

pub fn dashboard(agents: Result<Vec<AgentRow>, String>, cache: &SlotCache) -> Dashboard {
    let rows = match agents {
        Err(e) => {
            return Dashboard {
                agents: Err(e),
                groups: Vec::new(),
                inbox: Vec::new(),
                feed: Vec::new(),
            }
        }
        Ok(rows) => rows,
    };
    // Herdr liveness is the ground truth behind the journal's advisory
    // view; it is what lets a died-mid-run issue show as unknown.
    let live = LiveSet::new(
        rows.iter().map(|r| (r.workspace.clone(), r.pane.clone())),
        rows.iter().map(|r| r.cwd.clone()),
    );
    let views: Vec<RepoView> = cache
        .slots
        .iter()
        .map(|(key, slot)| repo_view(key, slot, &live))
        .collect();
    // The inbox aggregates every discovered GitHub repo; unfetched or
    // errored per-repo data degrades to the categories that do not need
    // it, same as the repo views.
    let mut inbox = Vec::new();
    for slot in cache.slots.values() {
        if let Remote::GitHub { repo, .. } = &slot.remote {
            inbox.extend(inbox::repo_items(
                &inbox::RepoFacts {
                    repo,
                    issues: slot
                        .issues
                        .as_ref()
                        .and_then(|r| r.as_ref().ok().map(Vec::as_slice)),
                    prs: slot
                        .prs
                        .as_ref()
                        .and_then(|r| r.as_ref().ok().map(Vec::as_slice)),
                    events: &slot.journal,
                },
                &live,
            ));
        }
    }
    inbox::sort_items(&mut inbox);
    // The activity feed spans every discovered repo: local slots have no
    // issue/PR surface but can still carry a Flock journal. Bounded per
    // repo inside `build_feed`, ordered by event `ts`.
    let feed = feed::build_feed(
        cache
            .slots
            .iter()
            .map(|(key, slot)| (slot_display_key(key, slot), slot.journal.as_slice())),
    );
    Dashboard {
        agents: Ok(rows),
        groups: group_repos(views),
        inbox,
        feed,
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
            updated_at: String::new(),
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
        cache.sync(&rows, &mut lookup, Instant::now());
        assert_eq!(cache.slots.len(), 2);
        let ticket = due_ticket(&cache, "o/one", Instant::now());
        assert!(cache.record_fetch(&ticket, Ok(vec![]), Ok(vec![]), Instant::now()));
        // Re-polling the same live set keeps the slot, its generation, and
        // its cached fetch data; the count is simply refreshed.
        cache.sync(&rows, &mut lookup, Instant::now());
        let slot = &cache.slots[&github_key("o/one")];
        assert_eq!(slot.generation, ticket.generation);
        assert!(slot.fetched_at.is_some());
    }

    #[test]
    fn sync_retains_recently_gone_repos_then_prunes_after_grace() {
        let t0 = Instant::now();
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
            t0,
        );
        assert!(cache.slots.contains_key(&github_key("o/gone")));
        // Only /dev/a/kept is live now: o/gone enters the grace window —
        // kept (so a died run stays visible) but with zero live agents.
        cache.sync(
            &agent_rows(vec![info("working", "/dev/a/kept")]),
            &mut lookup,
            t0 + Duration::from_secs(3),
        );
        let gone = &cache.slots[&github_key("o/gone")];
        assert_eq!(gone.agent_count, 0);
        assert!(gone.gone_since.is_some());
        assert!(cache.slots.contains_key(&github_key("o/kept")));
        // Once the grace period has fully elapsed the slot is pruned.
        cache.sync(
            &agent_rows(vec![info("working", "/dev/a/kept")]),
            &mut lookup,
            t0 + Duration::from_secs(3) + GONE_GRACE + Duration::from_secs(1),
        );
        assert!(!cache.slots.contains_key(&github_key("o/gone")));
        assert!(cache.slots.contains_key(&github_key("o/kept")));
    }

    #[test]
    fn disappeared_sole_agent_keeps_died_stage_visible_through_grace() {
        let t0 = Instant::now();
        let rows = agent_rows(vec![info("working", "/dev/a/one")]);
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(&rows, &mut lookup, t0);
        // Issue 7's run was dispatched to the sole agent's pane.
        let mut dispatched = jev("r1", "issue_dispatched", 7);
        dispatched.data =
            Some(serde_json::json!({"workspace_id": "w1", "pane_id": "w1:p-working"}));
        cache.refresh_journals(&|_| vec![dispatched.clone()]);
        let ticket = due_ticket(&cache, "o/one", t0);
        assert!(cache.record_fetch(&ticket, Ok(vec![numbered_issue(7, &[])]), Ok(vec![]), t0));
        // The agent dies: the next poll sees an empty live set. Inside the
        // grace window the repo row stays on the dashboard, and the issue
        // renders died-mid-run rather than vanishing.
        cache.sync(&[], &mut lookup, t0 + Duration::from_secs(3));
        let dash = dashboard(Ok(vec![]), &cache);
        assert_eq!(dash.groups.len(), 1);
        let view = &dash.groups[0].repos[0];
        assert_eq!(view.key, "o/one");
        assert_eq!(view.agent_count, 0);
        let rows = view.issue_rows.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(rows[0].stage, journal::Stage::Died);
        // A terminal event written during the window supersedes the died
        // heuristic on the next journal refresh...
        let mut closed = jev("r1", "issue_dispatched", 7);
        closed.data = Some(serde_json::json!({"workspace_id": "w1", "pane_id": "w1:p-working"}));
        cache.refresh_journals(&|_| vec![closed.clone(), jev("r1", "issue_closed", 7)]);
        let dash = dashboard(Ok(vec![]), &cache);
        let view = &dash.groups[0].repos[0];
        let rows = view.issue_rows.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(rows[0].stage, journal::Stage::Done);
        // ...and past the grace period the repo row drops entirely.
        cache.sync(
            &[],
            &mut lookup,
            t0 + Duration::from_secs(3) + GONE_GRACE + Duration::from_secs(1),
        );
        assert!(dashboard(Ok(vec![]), &cache).groups.is_empty());
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
        cache.sync(&rows, &mut lookup, Instant::now());
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
        cache.sync(&rows, &mut lookup, Instant::now());
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
        cache.sync(&rows, &mut lookup, Instant::now());
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
        cache.sync(&rows, &mut lookup, Instant::now());
        assert_eq!(
            cache.slots[&key].remote,
            Remote::GitError("not a repo".to_string())
        );
        // An unchanged classification keeps slot, generation, and state.
        let stable = cache.slots[&key].generation;
        cache.sync(&rows, &mut lookup, Instant::now());
        assert_eq!(cache.slots[&key].generation, stable);
    }

    #[test]
    fn invalidate_clears_live_set_and_rejects_in_flight_completions() {
        let rows = agent_rows(vec![info("working", "/dev/a/one")]);
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(&rows, &mut lookup, Instant::now());
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
        cache.sync(&rows, &mut lookup, Instant::now());
        assert_eq!(cache.due_github_repos(Instant::now(), GH_TTL).len(), 1);
    }

    #[test]
    fn stale_ticket_after_remove_and_readd_is_rejected() {
        let t0 = Instant::now();
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(
            &agent_rows(vec![info("working", "/dev/a/one")]),
            &mut lookup,
            t0,
        );
        let old = due_ticket(&cache, "o/one", t0);
        // The repo leaves the session entirely (past the grace window)...
        cache.sync(&[], &mut lookup, t0 + Duration::from_secs(1));
        cache.sync(
            &[],
            &mut lookup,
            t0 + Duration::from_secs(1) + GONE_GRACE + Duration::from_secs(1),
        );
        assert!(cache.slots.is_empty());
        // ...then a different worktree brings the same repo back.
        cache.sync(
            &agent_rows(vec![info("idle", "/dev/wt/one")]),
            &mut lookup,
            t0 + GONE_GRACE + Duration::from_secs(2),
        );
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
        cache.sync(&rows, &mut lookup, Instant::now());
        let old = due_ticket(&cache, "o/before", Instant::now());
        // The cwd's origin now points at a different repo (RemoteCache TTL
        // expired and git answered differently).
        *remote.borrow_mut() = github_remote("o/after");
        cache.sync(&rows, &mut lookup, Instant::now());
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
        cache.sync(&rows, &mut lookup, now);
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
            Instant::now(),
        );
        let ticket = due_ticket(&cache, "o/one", Instant::now());
        let issues = gh.open_issues(&ticket.repo).map_err(|e| e.to_string());
        let prs = gh.open_prs(&ticket.repo).map_err(|e| e.to_string());
        // A current-generation completion is accepted.
        assert!(cache.record_fetch(&ticket, issues, prs, Instant::now()));
        let view = repo_view(&ticket.key, &cache.slots[&ticket.key], &LiveSet::default());
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
        cache.sync(&rows, &mut lookup, Instant::now());
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
        cache.sync(&rows, &mut lookup, Instant::now());
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

    fn numbered_issue(number: u64, labels: &[&str]) -> Issue {
        Issue {
            number,
            title: format!("issue {number}"),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            updated_at: String::new(),
        }
    }

    fn pr_on_branch(number: u64, branch: &str, mergeable: &str) -> PullRequest {
        PullRequest {
            number,
            title: format!("pr {number}"),
            draft: false,
            review_decision: String::new(),
            mergeable: mergeable.to_string(),
            head_ref_name: branch.to_string(),
            updated_at: String::new(),
            checks: vec![rollup(Some("COMPLETED"), Some("SUCCESS"), None)],
        }
    }

    fn jev(run: &str, event: &str, issue: u64) -> journal::Event {
        journal::Event {
            run_id: run.to_string(),
            workflow: "operator-run".to_string(),
            event: event.to_string(),
            issue: Some(issue),
            ..Default::default()
        }
    }

    #[test]
    fn issue_rows_derive_stages_from_events_labels_and_prs() {
        let issues = vec![
            numbered_issue(1, &[]),                  // done via events
            numbered_issue(2, &[]),                  // pr open via events
            numbered_issue(3, &["ready-for-agent"]), // queued via label
            numbered_issue(4, &[]),                  // flock-branch PR, no events
            numbered_issue(5, &["enhancement"]),     // no signal at all
        ];
        let prs = vec![
            pr_on_branch(20, "flock/issue-2-thing", "UNKNOWN"),
            pr_on_branch(40, "flock/issue-4-other", "MERGEABLE"),
        ];
        let events = vec![
            jev("r1", "issue_closed", 1),
            jev("r2", "issue_dispatched", 2),
            journal::Event {
                pr: Some(20),
                ..jev("r2", "pr_opened", 2)
            },
        ];
        let rows = issue_rows(&issues, Some(&prs), &events, &LiveSet::default());
        let stage = |n: u64| rows.iter().find(|r| r.number == n).unwrap().stage.clone();
        assert_eq!(stage(1), journal::Stage::Done);
        // pr_opened seen; the correlated PR's checks are green but
        // mergeability is unresolved → checks pending.
        assert_eq!(stage(2), journal::Stage::ChecksPending);
        assert_eq!(stage(3), journal::Stage::Queued);
        // No events, but the branch pattern correlates PR #40: green +
        // mergeable → mergeable, all inferred.
        assert_eq!(stage(4), journal::Stage::Mergeable);
        assert_eq!(stage(5), journal::Stage::None);
        // Active work sorts ahead of dormant rows.
        assert_eq!(rows[0].number, 2);
        assert_eq!(rows[1].number, 4);
    }

    #[test]
    fn issue_rows_tolerate_missing_pr_data() {
        let issues = vec![numbered_issue(2, &[])];
        let events = vec![jev("r1", "pr_opened", 2)];
        // PR fetch failed or never ran: events still carry the stage.
        let rows = issue_rows(&issues, None, &events, &LiveSet::default());
        assert_eq!(rows[0].stage, journal::Stage::PrOpen);
    }

    #[test]
    fn issue_rows_mark_died_run_via_live_set() {
        let issues = vec![numbered_issue(7, &[])];
        let mut dispatched = jev("r1", "issue_dispatched", 7);
        dispatched.data = Some(serde_json::json!({"workspace_id": "w1", "pane_id": "w1:p1"}));
        let events = vec![dispatched];
        // Agent gone: no terminal event, no live pane → died.
        let rows = issue_rows(&issues, None, &events, &LiveSet::default());
        assert_eq!(rows[0].stage, journal::Stage::Died);
        // Agent live: normal in-flight stage.
        let live = LiveSet::new(vec![("w1".to_string(), "w1:p1".to_string())], vec![]);
        let rows = issue_rows(&issues, None, &events, &live);
        assert_eq!(rows[0].stage, journal::Stage::Dispatched);
    }

    #[test]
    fn refresh_journals_reads_every_slots_checkouts() {
        let rows = agent_rows(vec![
            info("working", "/dev/a/gh"),
            info("working", "/dev/a/local"),
        ]);
        let mut cache = SlotCache::default();
        let mut lookup = |cwd: &Path| match cwd.to_str().unwrap() {
            "/dev/a/gh" => github_remote("o/gh"),
            _ => Remote::NoOrigin,
        };
        cache.sync(&rows, &mut lookup, Instant::now());
        // Local slots have no issue/PR surface, but their journals still
        // feed the cross-repo activity feed, so the reader sees their
        // checkouts too.
        let reader = |cwds: &BTreeSet<PathBuf>| {
            let cwd = cwds.iter().next().expect("one live checkout");
            let issue = if cwd == Path::new("/dev/a/gh") { 3 } else { 4 };
            vec![jev("r1", "issue_closed", issue)]
        };
        cache.refresh_journals(&reader);
        let slot = &cache.slots[&github_key("o/gh")];
        assert_eq!(slot.journal.len(), 1);
        assert_eq!(slot.journal[0].issue, Some(3));
        assert!(slot.cwds.contains(Path::new("/dev/a/gh")));
        let local = &cache.slots[&SlotKey::Local(PathBuf::from("/dev/a/local"))];
        assert_eq!(local.journal.len(), 1, "local slots read journals too");
        assert_eq!(local.journal[0].issue, Some(4));
    }

    /// Writes `text` as the Flock journal of a fresh tempdir checkout,
    /// returning the dir guard (keeps the files alive) and the checkout
    /// path for use as an agent cwd.
    fn journal_checkout(text: String) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".flock")).unwrap();
        std::fs::write(dir.path().join(".flock/events.jsonl"), text).unwrap();
        let cwd = dir.path().to_str().unwrap().to_string();
        (dir, cwd)
    }

    fn journal_line(run: &str, event: &str, issue: u64, extra: &str) -> String {
        format!(
            "{{\"run_id\":\"{run}\",\"workflow\":\"operator-run\",\"event\":\"{event}\",\"repo\":\"o/one\",\"issue\":{issue}{extra}}}\n"
        )
    }

    #[test]
    fn journal_cache_is_bounded_and_feed_view_stays_capped() {
        // A long-lived repo whose journal has grown past the reader bound:
        // the slot cache must not retain the whole file, and the dashboard
        // feed stays at its per-repo view cap.
        let total = journal::MAX_JOURNAL_EVENTS + 100;
        let mut text = String::new();
        for i in 0..total {
            text.push_str(&journal_line(
                &format!("r{i}"),
                "run_started",
                1,
                ",\"ts\":\"2026-09-24T18:00:00Z\"",
            ));
        }
        let (_dir, cwd) = journal_checkout(text);
        let rows = agent_rows(vec![info("working", &cwd)]);
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(&rows, &mut lookup, Instant::now());
        // The production reader, not a test fake: the bound must hold at
        // the reader/cache boundary refresh_journals calls through.
        cache.refresh_journals(&journal::read_repo_events);
        let slot = &cache.slots[&github_key("o/one")];
        assert_eq!(
            slot.journal.len(),
            journal::MAX_JOURNAL_EVENTS,
            "the cache retains only the bounded newest tail, not the whole {total}-event journal"
        );
        assert_eq!(
            slot.journal.last().unwrap().run_id,
            format!("r{}", total - 1)
        );
        let dash = dashboard(Ok(rows), &cache);
        assert_eq!(
            dash.feed.len(),
            feed::MAX_EVENTS_PER_REPO,
            "the feed view keeps its own per-repo cap"
        );
    }

    #[test]
    fn in_flight_issue_facts_survive_the_bounded_journal_tail() {
        // Regression for the cache bound: an older in-flight issue whose
        // events sit just inside the retained tail must keep its stage and
        // inbox facts even though newer events (from other issues) follow
        // them and the file head was dropped.
        let total = journal::MAX_JOURNAL_EVENTS + 100;
        let mut text = String::new();
        // The dropped head: an ancient closed issue's noise.
        for i in 0..100 {
            text.push_str(&journal_line(&format!("old{i}"), "run_started", 1, ""));
        }
        // The oldest retained lines: issue 7's in-flight facts — dispatch
        // to the live agent's pane, PR opened, latest verdict blocking.
        text.push_str(&journal_line(
            "r7",
            "issue_dispatched",
            7,
            ",\"data\":{\"workspace_id\":\"w1\",\"pane_id\":\"w1:p-working\"}",
        ));
        text.push_str(&journal_line(
            "r7",
            "pr_opened",
            7,
            ",\"pr\":20,\"branch\":\"flock/issue-7-thing\"",
        ));
        text.push_str(&journal_line(
            "r7",
            "review_verdict",
            7,
            ",\"data\":{\"verdict\":\"blocking\"}",
        ));
        // Newer filler from a different issue: it must neither push issue
        // 7's facts out of the tail nor supersede its verdict.
        for i in 103..total {
            text.push_str(&journal_line(&format!("r{i}"), "run_started", 9, ""));
        }
        let (_dir, cwd) = journal_checkout(text);
        let rows = agent_rows(vec![info("working", &cwd)]);
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(&rows, &mut lookup, Instant::now());
        cache.refresh_journals(&journal::read_repo_events);
        let slot = &cache.slots[&github_key("o/one")];
        assert_eq!(slot.journal.len(), journal::MAX_JOURNAL_EVENTS);
        let ticket = due_ticket(&cache, "o/one", Instant::now());
        assert!(cache.record_fetch(
            &ticket,
            Ok(vec![numbered_issue(7, &[]), numbered_issue(9, &[])]),
            Ok(vec![pr_on_branch(20, "flock/issue-7-thing", "MERGEABLE")]),
            Instant::now(),
        ));
        let dash = dashboard(Ok(rows), &cache);
        let view = &dash.groups[0].repos[0];
        let issue_rows = view.issue_rows.as_ref().unwrap().as_ref().unwrap();
        let stage = |n: u64| {
            issue_rows
                .iter()
                .find(|r| r.number == n)
                .unwrap()
                .stage
                .clone()
        };
        // Issue 7's blocking verdict survived the bound: the stage and the
        // blocking-review inbox item derive exactly as without a cap.
        assert_eq!(stage(7), journal::Stage::ReviewBlocking);
        assert_eq!(stage(9), journal::Stage::None);
        let blocking: Vec<_> = dash
            .inbox
            .iter()
            .filter(|i| i.kind == inbox::InboxKind::BlockingReview)
            .collect();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].target, inbox::Target::Pr(20));
    }

    #[test]
    fn dashboard_feed_spans_repos_newest_first_and_names_local_slots_by_path() {
        let rows = agent_rows(vec![
            info("working", "/dev/a/one"),
            info("working", "/dev/a/two"),
            info("working", "/dev/a/plain"),
        ]);
        let mut cache = SlotCache::default();
        let mut lookup = |cwd: &Path| match cwd.to_str().unwrap() {
            "/dev/a/one" => github_remote("o/one"),
            "/dev/a/two" => github_remote("o/two"),
            _ => Remote::NoOrigin,
        };
        cache.sync(&rows, &mut lookup, Instant::now());
        cache.refresh_journals(&|cwds| {
            let cwd = cwds.iter().next().expect("one live checkout");
            let mut e = jev("r1", "run_started", 1);
            e.ts = Some(
                if cwd == Path::new("/dev/a/one") {
                    "2026-09-24T18:00:00Z"
                } else if cwd == Path::new("/dev/a/two") {
                    "2026-09-24T18:05:00Z"
                } else {
                    "2026-09-24T18:02:00Z"
                }
                .to_string(),
            );
            vec![e]
        });
        let dash = dashboard(Ok(rows), &cache);
        let got: Vec<&str> = dash.feed.iter().map(|e| e.repo.as_str()).collect();
        // Newest first by event ts across repos; the local slot's events
        // are named by its full path, matching its rendered repo row.
        assert_eq!(got, vec!["o/two", "/dev/a/plain", "o/one"]);
    }

    #[test]
    fn dashboard_with_herdr_error_has_no_repo_section() {
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(
            &agent_rows(vec![info("working", "/dev/a/one")]),
            &mut lookup,
            Instant::now(),
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
        assert!(dash.inbox.is_empty());
    }

    #[test]
    fn dashboard_aggregates_inbox_across_slots_and_error_clears_it() {
        let t0 = Instant::now();
        let rows = agent_rows(vec![info("working", "/dev/a/one")]);
        let mut cache = SlotCache::default();
        let mut lookup = |_: &Path| github_remote("o/one");
        cache.sync(&rows, &mut lookup, t0);
        // Issue 7: latest verdict blocking. Issue 8: stopped on a benign
        // reason (no inbox item). Issue 9: needs-info.
        cache.refresh_journals(&|_| {
            vec![
                journal::Event {
                    data: Some(serde_json::json!({"verdict": "blocking"})),
                    ..jev("r1", "review_verdict", 7)
                },
                journal::Event {
                    data: Some(serde_json::json!({"reason": "queue empty"})),
                    ..jev("r2", "run_stopped", 8)
                },
            ]
        });
        let ticket = due_ticket(&cache, "o/one", t0);
        assert!(cache.record_fetch(
            &ticket,
            Ok(vec![
                numbered_issue(7, &[]),
                numbered_issue(8, &[]),
                numbered_issue(9, &["needs-info"]),
            ]),
            Ok(vec![pr_on_branch(20, "flock/issue-7-thing", "MERGEABLE")]),
            t0,
        ));
        let dash = dashboard(Ok(rows.clone()), &cache);
        let kinds: Vec<_> = dash.inbox.iter().map(|i| i.kind).collect();
        // Blocking review (PR #20) first, then the informational item;
        // the benign stop derived nothing.
        assert_eq!(
            kinds,
            vec![
                inbox::InboxKind::BlockingReview,
                inbox::InboxKind::TrackerInput,
            ]
        );
        assert!(dash.inbox.iter().all(|i| i.repo == "o/one"));
        // A herdr failure invalidates the live repo set: no inbox either.
        cache.invalidate();
        let dash = dashboard(Err("server not running".to_string()), &cache);
        assert!(dash.inbox.is_empty());
    }
}
