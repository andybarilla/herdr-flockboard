//! The "waiting on you" inbox: one prioritized, cross-repo list of
//! everything that needs the human — blocking reviews, parked PRs, green
//! PRs awaiting merge, stopped operator runs, and tracker items needing
//! input. Derivation is pure over the same per-repo facts the repo views
//! use (tracker issues, open PRs, the Flock event journal — stopped/parked
//! stages via `journal::derive_stage`, blocking-review verdicts from the
//! verdict events directly — and the project-config green classifier), so every
//! category and its clearing condition is unit-testable. Nothing here is
//! latched: items clear on the next refresh because their condition stops
//! deriving — a merged or closed PR drops off the open-PR list, a newer
//! run supersedes the stopped one in stage derivation, a removed label
//! leaves the issue's label set.

use std::cmp::Ordering;
use std::time::{Duration, SystemTime};

use crate::github::{Issue, PullRequest};
use crate::journal::{self, Event, LiveSet, PrGreen, Stage};
use crate::state::correlated_pr;

/// Open-issue labels that mean the tracker is waiting on human input,
/// most-urgent first (matching `state::LABEL_PRIORITY`'s relative order).
/// An issue carrying both lands in the first matching category so the
/// inbox lists it exactly once.
pub const NEEDS_INPUT_LABELS: [&str; 2] = ["needs-info", "needs-triage"];

/// Stop reasons (matched on the head before any ": detail" suffix) that
/// end a run normally, from the operator-run stop-reason vocabulary: the
/// run's work is done, its queue ran dry, or its configured limits were
/// reached, so nothing is waiting on the human. Every other reason — and
/// any reason this build does not know yet — counts as needing human
/// action (fail-closed, same ethos as the green classifier).
const BENIGN_STOPS: [&str; 5] = [
    "completed one-shot action",
    "dry-run plan completed",
    "queue empty",
    "focus queue empty",
    "limit reached",
];

/// Inbox categories. `group()` buckets them into the presentation order
/// from the issue: blocking/stopped first, then merge-ready, then
/// informational.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxKind {
    /// Latest `review_verdict` blocking (journal), or `CHANGES_REQUESTED`
    /// on an open PR (gh).
    BlockingReview,
    /// The issue's cycle ended `review blocking` and its PR is still open:
    /// parked, awaiting the human per Flock's parked-PR policy.
    ParkedPr,
    /// The latest run stopped on a reason that needs human action.
    StoppedRun,
    /// Checks green per the project-config classifier, `MERGEABLE`, no
    /// blocking review, and no still-unsatisfied required review
    /// (`reviewDecision: REVIEW_REQUIRED` is `pending` per the project
    /// config, not green): the human merges per policy.
    AwaitingMerge,
    /// Open issue labeled `needs-info` or `needs-triage`.
    TrackerInput,
}

impl InboxKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::BlockingReview => "blocking review",
            Self::ParkedPr => "parked PR",
            Self::StoppedRun => "stopped run",
            Self::AwaitingMerge => "awaiting merge",
            Self::TrackerInput => "needs input",
        }
    }

    /// Presentation group, lower first: blocking/stopped (0), merge-ready
    /// (1), informational (2).
    pub fn group(self) -> u8 {
        match self {
            Self::BlockingReview | Self::ParkedPr | Self::StoppedRun => 0,
            Self::AwaitingMerge => 1,
            Self::TrackerInput => 2,
        }
    }
}

/// What an inbox item points at: the issue a run/tracker item is about, or
/// the PR a review/merge item is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Issue(u64),
    Pr(u64),
}

impl Target {
    pub fn label(self) -> String {
        match self {
            Self::Issue(n) => format!("#{n}"),
            Self::Pr(n) => format!("PR #{n}"),
        }
    }

    fn number(self) -> u64 {
        match self {
            Self::Issue(n) | Self::Pr(n) => n,
        }
    }
}

/// One thing waiting on the human.
#[derive(Clone, Debug, PartialEq)]
pub struct InboxItem {
    /// "owner/name" of the repo the item belongs to.
    pub repo: String,
    pub kind: InboxKind,
    pub target: Target,
    pub title: String,
    /// Short qualifier for the kind (stop reason, tracker label, gh review
    /// state); empty when the kind label says everything.
    pub detail: String,
    /// When the triggering event happened: the journal event's `ts` for
    /// journal-derived items, gh's `updatedAt` for GitHub-state items.
    /// `None` when no usable timestamp exists; the item renders ageless
    /// and sorts after aged items within its group.
    pub since: Option<SystemTime>,
}

impl InboxItem {
    /// The operator-facing reason text: the category, qualified by the
    /// detail when there is one.
    pub fn reason(&self) -> String {
        match self.kind {
            InboxKind::StoppedRun => format!("stopped: {}", self.detail),
            InboxKind::TrackerInput => self.detail.clone(),
            _ if self.detail.is_empty() => self.kind.label().to_string(),
            _ => format!("{} ({})", self.kind.label(), self.detail),
        }
    }
}

/// Everything the inbox derivation knows about one repo: its open issues
/// and PRs (`None` when that fetch never ran or errored — GitHub-state
/// categories then simply derive nothing, same degradation as the repo
/// views; journal blocking facts still gate merge-ready PRs when only the
/// issue fetch failed, see `repo_items`) and its parsed journal events.
pub struct RepoFacts<'a> {
    /// "owner/name".
    pub repo: &'a str,
    pub issues: Option<&'a [Issue]>,
    pub prs: Option<&'a [PullRequest]>,
    pub events: &'a [Event],
}

/// The reason head: the stop reason up to any ": detail" suffix, trimmed.
/// Operator stop strings sometimes carry elaborations ("review blocking:
/// 3 Important findings…"); classification keys off the category head.
fn stop_head(reason: &str) -> &str {
    reason.split(':').next().unwrap_or(reason).trim()
}

/// Whether a stop reason needs human action. Fail-closed: only the known
/// benign completions are excluded.
fn needs_human(reason: &str) -> bool {
    !BENIGN_STOPS.contains(&stop_head(reason))
}

/// The blocking-review fact for one issue, derived from the journal
/// events directly rather than from `Stage::ReviewBlocking`:
/// `derive_stage` evaluates its died heuristic before the verdict ladder,
/// so a latest blocking verdict on a run that later appears dead never
/// surfaces as a stage — but per the issue it is still a blocking-review
/// inbox item. Returns the triggering verdict's timestamp when the latest
/// `review_verdict` is blocking and nothing supersedes it: a newer verdict
/// (the latest one wins), a `rework_dispatched` or `merge_verified` after
/// it, or a newer run (the verdict is not in the latest run) all clear it,
/// mirroring the stage derivation's clearing semantics.
fn latest_blocking_verdict(events: &[Event]) -> Option<Option<SystemTime>> {
    let v = events.iter().rposition(|e| e.is("review_verdict"))?;
    if events[v].verdict() != Some("blocking") {
        return None;
    }
    let superseded = events[v + 1..]
        .iter()
        .any(|e| e.is("rework_dispatched") || e.is("merge_verified"));
    let newer_run = events
        .last()
        .is_some_and(|last| last.run_id != events[v].run_id);
    if superseded || newer_run {
        return None;
    }
    Some(events[v].ts.as_deref().and_then(parse_ts))
}

/// Derives every inbox item for one repo, sorted by `sort_items` order.
///
/// The stopped-run and parked-PR categories ride `journal::derive_stage`'s
/// `Stopped` stage, so they share its clearing semantics exactly: a
/// stopped run's item disappears as soon as a newer run starts (the stage
/// is no longer `Stopped`). The blocking-review category comes from
/// `latest_blocking_verdict` (see its doc comment), with the same
/// supersession rules the stage would apply. A `review blocking` stop
/// subsumes the blocking-review row for the same verdict — one row per
/// underlying state — and is the parked-PR category while the PR is still
/// open.
///
/// Journal-derived PR items (blocking review, parked PR) require a
/// correlated open PR whenever the PR fetch succeeded: a merged or closed
/// PR drops off the open-PR list and the item clears on the next refresh
/// rather than lingering as an issue-keyed row or being reclassified as a
/// stopped run. Only with no PR data at all (fetch never ran or errored)
/// do they degrade to issue-keyed/stopped-run rows, matching the repo
/// views' degradation.
///
/// When the issue fetch failed but PR data is available, the per-issue
/// loop below never runs, so its journal-derived blocking facts would be
/// lost and a green PR the journal says is blocked could surface as
/// awaiting merge. A fallback pass re-derives just those facts straight
/// from the journal — events carry issue/pr/branch keys independently of
/// the issue fetch — and adds each correlated open PR to `blocked_prs`
/// before the PR-state pass (suppression only; no rows are emitted in
/// this degraded mode).
pub fn repo_items(facts: &RepoFacts, live: &LiveSet) -> Vec<InboxItem> {
    let mut items = Vec::new();
    let issues = facts.issues.unwrap_or(&[]);
    // PRs that already carry a blocking-family item (journal verdict or
    // parked): they are excluded from the merge-ready category and must
    // not get a second blocking item from the gh leg.
    let mut blocked_prs: Vec<u64> = Vec::new();

    for issue in issues {
        let events: Vec<Event> = facts
            .events
            .iter()
            .filter(|e| e.issue == Some(issue.number))
            .cloned()
            .collect();
        let pr = correlated_pr(issue.number, &events, facts.prs);
        let stage = journal::derive_stage(
            &journal::IssueFacts {
                labels: issue.labels.clone(),
                events: events.clone(),
                pr_green: pr.map(|p| journal::classify_green(&p.checks, &p.mergeable)),
            },
            live,
        );
        // Derived from the verdict events, not the stage: a died run
        // masks `Stage::ReviewBlocking`, but the verdict is still a
        // blocking-review inbox item (see `latest_blocking_verdict`).
        let blocking = latest_blocking_verdict(&events);
        // A `review blocking` stop subsumes the blocking-review row for
        // the same verdict (one row per underlying state).
        let mut review_blocking_stop = false;
        if let Stage::Stopped(reason) = &stage {
            let since = events
                .iter()
                .rev()
                .find(|e| e.is("run_stopped"))
                .and_then(|e| e.ts.as_deref())
                .and_then(parse_ts);
            if stop_head(reason) == "review blocking" {
                review_blocking_stop = true;
                if let Some(p) = pr {
                    // Cycle ended review-blocking and the PR is still
                    // open: parked per Flock's parked-PR policy. This
                    // subsumes the stopped-run item for the same stop.
                    blocked_prs.push(p.number);
                    items.push(make_item(
                        facts.repo,
                        InboxKind::ParkedPr,
                        p,
                        stop_head(reason),
                        since,
                    ));
                } else if facts.prs.is_none() {
                    // No PR data at all: degrade to the plain
                    // stopped-run row. With PR data available and no
                    // correlated open PR, the parked PR merged or
                    // closed — the state cleared, and a closed parked
                    // PR is not reclassified as a stopped run.
                    items.push(InboxItem {
                        repo: facts.repo.to_string(),
                        kind: InboxKind::StoppedRun,
                        target: Target::Issue(issue.number),
                        title: issue.title.clone(),
                        detail: reason.clone(),
                        since,
                    });
                }
            } else if needs_human(reason) {
                items.push(InboxItem {
                    repo: facts.repo.to_string(),
                    kind: InboxKind::StoppedRun,
                    target: Target::Issue(issue.number),
                    title: issue.title.clone(),
                    detail: reason.clone(),
                    since,
                });
            }
        }
        if let (Some(since), false) = (blocking, review_blocking_stop) {
            if let Some(p) = pr {
                blocked_prs.push(p.number);
                items.push(make_item(
                    facts.repo,
                    InboxKind::BlockingReview,
                    p,
                    "",
                    since,
                ));
            } else if facts.prs.is_none() {
                // No PR data at all: degrade to an issue-keyed row. With
                // PR data available and no correlated open PR, the PR the
                // verdict was about merged or closed and the item clears.
                items.push(InboxItem {
                    repo: facts.repo.to_string(),
                    kind: InboxKind::BlockingReview,
                    target: Target::Issue(issue.number),
                    title: issue.title.clone(),
                    detail: String::new(),
                    since,
                });
            }
        }
        if let Some(label) = NEEDS_INPUT_LABELS
            .iter()
            .find(|l| issue.labels.iter().any(|have| have == *l))
        {
            items.push(InboxItem {
                repo: facts.repo.to_string(),
                kind: InboxKind::TrackerInput,
                target: Target::Issue(issue.number),
                title: issue.title.clone(),
                detail: label.to_string(),
                since: parse_ts(&issue.updated_at),
            });
        }
    }

    // Issue fetch failed but PR data is available: the issue loop above
    // never ran, so journal-derived blocking facts would be lost and a
    // green PR the journal says is blocked could surface as awaiting
    // merge. Re-derive just those facts per journal issue cluster — the
    // same verdict/stop rules as the issue loop — and push each
    // correlated open PR onto `blocked_prs` before the PR-state pass.
    if facts.issues.is_none() && facts.prs.is_some() {
        let mut numbers: Vec<u64> = facts.events.iter().filter_map(|e| e.issue).collect();
        numbers.sort_unstable();
        numbers.dedup();
        for number in numbers {
            let events: Vec<Event> = facts
                .events
                .iter()
                .filter(|e| e.issue == Some(number))
                .cloned()
                .collect();
            let Some(pr) = correlated_pr(number, &events, facts.prs) else {
                // No correlated open PR: merged/closed (cleared) or not
                // yet opened — nothing to suppress.
                continue;
            };
            let verdict_blocking = latest_blocking_verdict(&events).is_some();
            // Same stop semantics as the issue loop: a latest-run
            // `review blocking` stop parks a still-open PR. Labels are
            // unavailable without the issue fetch, but they never feed
            // the Stopped stage.
            let parked = matches!(
                journal::derive_stage(
                    &journal::IssueFacts {
                        labels: Vec::new(),
                        events,
                        pr_green: None,
                    },
                    live,
                ),
                Stage::Stopped(reason) if stop_head(&reason) == "review blocking"
            );
            if (verdict_blocking || parked) && !blocked_prs.contains(&pr.number) {
                blocked_prs.push(pr.number);
            }
        }
    }

    // GitHub-state categories over the open PR list. A merged or closed PR
    // is simply absent here, which is what clears these items on refresh.
    if let Some(prs) = facts.prs {
        for pr in prs {
            if blocked_prs.contains(&pr.number) {
                continue;
            }
            if pr.review_decision == "CHANGES_REQUESTED" {
                items.push(make_item(
                    facts.repo,
                    InboxKind::BlockingReview,
                    pr,
                    "changes requested",
                    parse_ts(&pr.updated_at),
                ));
            } else if !pr.draft
                && pr.review_decision != "REVIEW_REQUIRED"
                && journal::classify_green(&pr.checks, &pr.mergeable) == PrGreen::Green
            {
                // Drafts are excluded: a draft is by definition not
                // awaiting merge. REVIEW_REQUIRED means branch protection
                // still requires a review, which the project config
                // classifies as `pending`, not green; an empty decision
                // means no review is required, so only REVIEW_REQUIRED is
                // excluded here (CHANGES_REQUESTED is the blocking-review
                // leg above). The classifier already excludes conflicts
                // (CONFLICTING is failing), pending checks, and unknown
                // shapes (fail-closed).
                items.push(make_item(
                    facts.repo,
                    InboxKind::AwaitingMerge,
                    pr,
                    "",
                    parse_ts(&pr.updated_at),
                ));
            }
        }
    }

    sort_items(&mut items);
    items
}

fn make_item(
    repo: &str,
    kind: InboxKind,
    pr: &PullRequest,
    detail: &str,
    since: Option<SystemTime>,
) -> InboxItem {
    InboxItem {
        repo: repo.to_string(),
        kind,
        target: Target::Pr(pr.number),
        title: pr.title.clone(),
        detail: detail.to_string(),
        since,
    }
}

/// Presentation order: blocking/stopped first, then merge-ready, then
/// informational; oldest first within a group (ageless items last); then
/// repo and number for determinism.
pub fn sort_items(items: &mut [InboxItem]) {
    items.sort_by(|a, b| {
        a.kind
            .group()
            .cmp(&b.kind.group())
            .then_with(|| oldest_first(a.since, b.since))
            .then_with(|| a.repo.cmp(&b.repo))
            .then_with(|| a.target.number().cmp(&b.target.number()))
    });
}

fn oldest_first(a: Option<SystemTime>, b: Option<SystemTime>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Compact age text for the dashboard: seconds under a minute, minutes
/// under an hour, hours under a day, then days.
pub fn format_age(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86_400)
    }
}

/// Parses the RFC3339 timestamps the journal (`ts`) and gh (`updatedAt`)
/// emit: `YYYY-MM-DDTHH:MM:SS[.fff](Z|±HH:MM)`. Fractional seconds are
/// ignored (ages render at second granularity); anything unrecognized
/// yields `None` so the item renders without an age rather than with a
/// wrong one.
pub fn parse_ts(s: &str) -> Option<SystemTime> {
    let (date, time) = s.split_once('T')?;
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;
    // Range-check every component before any arithmetic so corrupt
    // journal input (a huge year, a day of 99) degrades to "no
    // timestamp" instead of overflowing. RFC3339 years are four
    // digits, and the epoch result must fit a u64 second count, so
    // pre-epoch dates degrade the same way — every timestamp gh or a
    // well-formed journal can emit is far inside that window.
    if dp.next().is_some()
        || !(1..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&d)
    {
        return None;
    }
    let (hms, offset) = match time.strip_suffix('Z') {
        Some(t) => (t, 0),
        None => {
            let idx = time.find(['+', '-'])?;
            let (t, sign) = (&time[..idx], &time[idx..]);
            let (oh, om) = sign[1..].split_once(':')?;
            let oh: i64 = oh.parse().ok()?;
            let om: i64 = om.parse().ok()?;
            if !(0..=23).contains(&oh) || !(0..=59).contains(&om) {
                return None;
            }
            let secs = oh.checked_mul(3600)?.checked_add(om.checked_mul(60)?)?;
            (t, if sign.starts_with('-') { -secs } else { secs })
        }
    };
    let hms = hms.split('.').next()?;
    let mut tp = hms.split(':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mm: i64 = tp.next()?.parse().ok()?;
    let ss: i64 = tp.next()?.parse().ok()?;
    if tp.next().is_some() || hh > 23 || mm > 59 || ss > 60 || hh < 0 || mm < 0 || ss < 0 {
        return None;
    }
    // Checked arithmetic throughout: any overflow yields None rather
    // than panicking in debug builds or wrapping into a bogus SystemTime
    // in release.
    let secs = days_from_civil(y, m, d)?
        .checked_mul(86_400)?
        .checked_add(hh.checked_mul(3600)?)?
        .checked_add(mm.checked_mul(60)?)?
        .checked_add(ss)?
        .checked_sub(offset)?;
    u64::try_from(secs)
        .ok()
        .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
}

/// Days since the Unix epoch for a Gregorian date (Howard Hinnant's
/// days-from-civil algorithm). The caller bounds the year to four
/// digits, which keeps the intermediate products far inside i64; the
/// checked arithmetic stays as defense-in-depth so no future caller
/// can overflow it.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    let y = if m <= 2 { y.checked_sub(1)? } else { y };
    let era = y.checked_div_euclid(400)?;
    let yoe = y.checked_sub(era.checked_mul(400)?)?;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe / 4)?
        .checked_sub(yoe / 100)?
        .checked_add(doy)?;
    era.checked_mul(146_097)?
        .checked_add(doe)?
        .checked_sub(719_468)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::CheckRollup;

    const REPO: &str = "o/repo";

    fn issue(number: u64, labels: &[&str]) -> Issue {
        Issue {
            number,
            title: format!("issue {number}"),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            updated_at: "2026-09-24T18:00:00Z".to_string(),
        }
    }

    fn pr(number: u64, branch: &str, mergeable: &str, review: &str) -> PullRequest {
        PullRequest {
            number,
            title: format!("pr {number}"),
            draft: false,
            review_decision: review.to_string(),
            mergeable: mergeable.to_string(),
            head_ref_name: branch.to_string(),
            updated_at: "2026-09-24T18:00:00Z".to_string(),
            checks: vec![CheckRollup {
                status: Some("COMPLETED".to_string()),
                conclusion: Some("SUCCESS".to_string()),
                state: None,
            }],
        }
    }

    fn ev(run: &str, event: &str, issue: u64) -> Event {
        Event {
            run_id: run.to_string(),
            workflow: "operator-run".to_string(),
            event: event.to_string(),
            issue: Some(issue),
            ts: Some("2026-09-24T18:52:28.183Z".to_string()),
            ..Default::default()
        }
    }

    fn ev_data(run: &str, event: &str, issue: u64, data: serde_json::Value) -> Event {
        Event {
            data: Some(data),
            ..ev(run, event, issue)
        }
    }

    fn no_live() -> LiveSet {
        LiveSet::default()
    }

    fn facts<'a>(
        issues: Option<&'a [Issue]>,
        prs: Option<&'a [PullRequest]>,
        events: &'a [Event],
    ) -> RepoFacts<'a> {
        RepoFacts {
            repo: REPO,
            issues,
            prs,
            events,
        }
    }

    fn kinds(items: &[InboxItem]) -> Vec<InboxKind> {
        items.iter().map(|i| i.kind).collect()
    }

    // --- blocking reviews ---

    #[test]
    fn blocking_review_from_journal_verdict_and_its_clearing() {
        let issues = vec![issue(7, &[])];
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "")];
        let verdict =
            |v: &str| ev_data("r1", "review_verdict", 7, serde_json::json!({"verdict": v}));

        // Latest verdict blocking → item, keyed on the correlated PR, aged
        // from the verdict event's ts.
        let events = vec![ev("r1", "pr_opened", 7), verdict("blocking")];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        let blocking: Vec<_> = items
            .iter()
            .filter(|i| i.kind == InboxKind::BlockingReview)
            .collect();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].target, Target::Pr(20));
        assert_eq!(blocking[0].title, "pr 20");
        assert_eq!(blocking[0].reason(), "blocking review");
        assert_eq!(
            blocking[0].since,
            parse_ts("2026-09-24T18:52:28.183Z"),
            "aged from the verdict event"
        );
        // The PR is not also offered as merge-ready.
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));

        // Clears when a newer verdict is clean...
        let events = vec![
            ev("r1", "pr_opened", 7),
            verdict("blocking"),
            verdict("clean"),
        ];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::BlockingReview));
        // ...and when a rework is dispatched after the verdict.
        let events = vec![
            ev("r1", "pr_opened", 7),
            verdict("blocking"),
            ev("r1", "rework_dispatched", 7),
        ];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::BlockingReview));
    }

    #[test]
    fn blocking_review_from_gh_changes_requested_and_its_clearing() {
        // No journal signal at all: gh's reviewDecision drives the item.
        let issues = vec![issue(7, &[])];
        let prs = vec![pr(
            20,
            "flock/issue-7-thing",
            "MERGEABLE",
            "CHANGES_REQUESTED",
        )];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &[]), &no_live());
        let blocking: Vec<_> = items
            .iter()
            .filter(|i| i.kind == InboxKind::BlockingReview)
            .collect();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].target, Target::Pr(20));
        assert_eq!(blocking[0].reason(), "blocking review (changes requested)");
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));

        // Clears when the review state resolves (decision changes)...
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "APPROVED")];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &[]), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::BlockingReview));
        assert!(kinds(&items).contains(&InboxKind::AwaitingMerge));
        // ...or when the PR is merged/closed (absent from the open list).
        let items = repo_items(&facts(Some(&issues), Some(&[]), &[]), &no_live());
        assert!(items.is_empty());
    }

    #[test]
    fn gh_and_journal_blocking_legs_do_not_double_list_a_pr() {
        let issues = vec![issue(7, &[])];
        let prs = vec![pr(
            20,
            "flock/issue-7-thing",
            "MERGEABLE",
            "CHANGES_REQUESTED",
        )];
        let events = vec![
            ev("r1", "pr_opened", 7),
            ev_data(
                "r1",
                "review_verdict",
                7,
                serde_json::json!({"verdict": "blocking"}),
            ),
        ];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        let blocking: Vec<_> = items
            .iter()
            .filter(|i| i.kind == InboxKind::BlockingReview)
            .collect();
        assert_eq!(blocking.len(), 1, "one row for the PR, not one per leg");
    }

    #[test]
    fn journal_blocking_verdict_clears_when_the_pr_is_merged_or_closed() {
        let issues = vec![issue(7, &[])];
        let events = vec![
            ev("r1", "pr_opened", 7),
            ev_data(
                "r1",
                "review_verdict",
                7,
                serde_json::json!({"verdict": "blocking"}),
            ),
        ];
        // Open PR: the item keys on it.
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "")];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::BlockingReview]);

        // PR merged/closed (absent from the open list) with PR data
        // available: the item clears on refresh instead of lingering as
        // an issue-keyed blocking-review row.
        let items = repo_items(&facts(Some(&issues), Some(&[]), &events), &no_live());
        assert!(items.is_empty());

        // With no PR data at all (fetch never ran or errored) the verdict
        // degrades to an issue-keyed row rather than disappearing.
        let items = repo_items(&facts(Some(&issues), None, &events), &no_live());
        let blocking: Vec<_> = items
            .iter()
            .filter(|i| i.kind == InboxKind::BlockingReview)
            .collect();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].target, Target::Issue(7));
    }

    // --- parked PRs ---

    #[test]
    fn parked_pr_from_review_blocking_stop_and_its_clearing() {
        let issues = vec![issue(7, &[])];
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "")];
        let events = vec![
            ev("r1", "pr_opened", 7),
            ev_data(
                "r1",
                "run_stopped",
                7,
                serde_json::json!({"reason": "review blocking: 2 findings on PR #20"}),
            ),
        ];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        let parked: Vec<_> = items
            .iter()
            .filter(|i| i.kind == InboxKind::ParkedPr)
            .collect();
        assert_eq!(parked.len(), 1);
        assert_eq!(parked[0].target, Target::Pr(20));
        // The reason category is the stop head, not the long elaboration.
        assert_eq!(parked[0].reason(), "parked PR (review blocking)");
        assert_eq!(parked[0].since, parse_ts("2026-09-24T18:52:28.183Z"));
        // The parked row subsumes the stopped-run item for the same stop.
        assert!(!kinds(&items).contains(&InboxKind::StoppedRun));

        // Clears when the PR is merged/closed (no longer open): a closed
        // parked PR is done, not reclassified as a stopped run.
        let items = repo_items(&facts(Some(&issues), Some(&[]), &events), &no_live());
        assert!(items.is_empty());
        // Only with no PR data at all (fetch never ran or errored) does
        // the stop degrade to a plain stopped-run row until a newer run
        // starts.
        let items = repo_items(&facts(Some(&issues), None, &events), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::StoppedRun]);
    }

    // --- green PRs awaiting merge ---

    #[test]
    fn awaiting_merge_requires_green_mergeable_no_blocking_review() {
        let issues = vec![];
        let green = || pr(20, "flock/issue-7-thing", "MERGEABLE", "APPROVED");

        // Green checks + MERGEABLE + non-blocking review → item.
        let items = repo_items(&facts(Some(&issues), Some(&[green()]), &[]), &no_live());
        let merge: Vec<_> = items
            .iter()
            .filter(|i| i.kind == InboxKind::AwaitingMerge)
            .collect();
        assert_eq!(merge.len(), 1);
        assert_eq!(merge[0].target, Target::Pr(20));
        assert_eq!(merge[0].reason(), "awaiting merge");
        assert_eq!(merge[0].since, parse_ts("2026-09-24T18:00:00Z"));

        // Not drafts...
        let mut draft = green();
        draft.draft = true;
        let items = repo_items(&facts(Some(&issues), Some(&[draft]), &[]), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));
        // ...not conflicting (the classifier's hard stop)...
        let items = repo_items(
            &facts(
                Some(&issues),
                Some(&[pr(20, "flock/issue-7-thing", "CONFLICTING", "APPROVED")]),
                &[],
            ),
            &no_live(),
        );
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));
        // ...not with checks pending...
        let mut pending = green();
        pending.checks[0].conclusion = None;
        pending.checks[0].status = Some("IN_PROGRESS".to_string());
        let items = repo_items(&facts(Some(&issues), Some(&[pending]), &[]), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));
        // ...and not with a blocking review decision.
        let items = repo_items(
            &facts(
                Some(&issues),
                Some(&[pr(
                    20,
                    "flock/issue-7-thing",
                    "MERGEABLE",
                    "CHANGES_REQUESTED",
                )]),
                &[],
            ),
            &no_live(),
        );
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));

        // Clears when the PR is merged (absent from the open list).
        let items = repo_items(&facts(Some(&issues), Some(&[]), &[]), &no_live());
        assert!(items.is_empty());
    }

    #[test]
    fn awaiting_merge_excludes_review_required() {
        let issues = vec![];
        // Green checks, MERGEABLE, not a draft — but branch protection
        // still requires a review: the project config classifies this as
        // `pending`, not green, so the PR is not offered as merge-ready
        // (and it is not a blocking review either).
        let prs = vec![pr(
            20,
            "flock/issue-7-thing",
            "MERGEABLE",
            "REVIEW_REQUIRED",
        )];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &[]), &no_live());
        assert!(items.is_empty());

        // An approval, or no required review at all (empty decision),
        // stays eligible.
        for decision in ["APPROVED", ""] {
            let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", decision)];
            let items = repo_items(&facts(Some(&issues), Some(&prs), &[]), &no_live());
            assert_eq!(
                kinds(&items),
                vec![InboxKind::AwaitingMerge],
                "reviewDecision {decision:?} stays merge-ready"
            );
        }
    }

    #[test]
    fn awaiting_merge_excluded_when_journal_verdict_blocking() {
        // gh shows no blocking decision, but the journal's latest verdict
        // is blocking: "no blocking review" fails and the PR is not
        // offered as merge-ready.
        let issues = vec![issue(7, &[])];
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "")];
        let events = vec![
            ev("r1", "pr_opened", 7),
            ev_data(
                "r1",
                "review_verdict",
                7,
                serde_json::json!({"verdict": "blocking"}),
            ),
        ];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));
        assert!(kinds(&items).contains(&InboxKind::BlockingReview));
    }

    #[test]
    fn latest_blocking_verdict_survives_a_died_run() {
        let issues = vec![issue(7, &[])];
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "")];
        let dispatch = serde_json::json!({"workspace_id": "w1", "pane_id": "w1:p1"});
        let base = || {
            vec![
                ev_data("r1", "issue_dispatched", 7, dispatch.clone()),
                ev("r1", "pr_opened", 7),
                ev_data(
                    "r1",
                    "review_verdict",
                    7,
                    serde_json::json!({"verdict": "blocking"}),
                ),
            ]
        };
        // The run appears dead (its agent is gone from the live set), so
        // derive_stage yields Died before ever evaluating the verdict —
        // but the latest blocking verdict is still a blocking-review
        // inbox item, keyed on the correlated PR and aged from the
        // verdict event.
        let items = repo_items(&facts(Some(&issues), Some(&prs), &base()), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::BlockingReview]);
        assert_eq!(items[0].target, Target::Pr(20));
        assert_eq!(items[0].since, parse_ts("2026-09-24T18:52:28.183Z"));

        // Superseded by a newer clean verdict: the blocking item clears
        // (the green PR becomes merge-ready instead).
        let mut events = base();
        events.push(ev_data(
            "r1",
            "review_verdict",
            7,
            serde_json::json!({"verdict": "clean"}),
        ));
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::BlockingReview));
        // ...by a rework dispatched after the verdict...
        let mut events = base();
        events.push(ev("r1", "rework_dispatched", 7));
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::BlockingReview));
        // ...or by a newer run (the verdict is no longer in the latest
        // run, even though that run also appears dead).
        let mut events = base();
        events.push(ev("r2", "run_started", 7));
        events.push(ev_data("r2", "issue_dispatched", 7, dispatch.clone()));
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::BlockingReview));
    }

    // --- stopped runs ---

    #[test]
    fn stopped_run_categories_and_benign_exclusions() {
        let issues = vec![issue(7, &[])];
        let stop = |run: &str, reason: &str| {
            ev_data(run, "run_stopped", 7, serde_json::json!({"reason": reason}))
        };
        // Human-action reasons derive items, keyed on the issue with the
        // verbatim reason.
        for reason in [
            "PR not green",
            "worker blocked",
            "merge conflict",
            "checks failing",
            "dirty worktree",
            "human confirmation required",
            "some future stop reason",
        ] {
            let events = vec![ev("r1", "pr_opened", 7), stop("r1", reason)];
            let items = repo_items(&facts(Some(&issues), Some(&[]), &events), &no_live());
            let stopped: Vec<_> = items
                .iter()
                .filter(|i| i.kind == InboxKind::StoppedRun)
                .collect();
            assert_eq!(stopped.len(), 1, "{reason} needs a human");
            assert_eq!(stopped[0].target, Target::Issue(7));
            assert_eq!(stopped[0].reason(), format!("stopped: {reason}"));
        }
        // Benign completions derive nothing.
        for reason in [
            "completed one-shot action",
            "dry-run plan completed",
            "queue empty",
            "focus queue empty",
            "limit reached",
            "limit reached: max cycles 3",
        ] {
            let events = vec![ev("r1", "run_started", 7), stop("r1", reason)];
            let items = repo_items(&facts(Some(&issues), Some(&[]), &events), &no_live());
            assert!(
                !kinds(&items).contains(&InboxKind::StoppedRun),
                "{reason} is benign"
            );
        }
    }

    #[test]
    fn stopped_run_clears_when_a_newer_run_starts() {
        let issues = vec![issue(7, &[])];
        let events = vec![
            ev("r1", "pr_opened", 7),
            ev_data(
                "r1",
                "run_stopped",
                7,
                serde_json::json!({"reason": "PR not green"}),
            ),
        ];
        let items = repo_items(&facts(Some(&issues), None, &events), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::StoppedRun]);
        // A newer run for the same issue supersedes the stop in stage
        // derivation, so the item is gone.
        let mut events = events;
        events.push(ev("r2", "run_started", 7));
        events.push(ev("r2", "issue_dispatched", 7));
        let items = repo_items(&facts(Some(&issues), None, &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::StoppedRun));
    }

    // --- tracker items needing input ---

    #[test]
    fn tracker_items_from_needs_input_labels_and_their_clearing() {
        let issues = vec![
            issue(7, &["needs-info"]),
            issue(8, &["needs-triage"]),
            issue(9, &["needs-info", "needs-triage"]), // listed once
            issue(10, &["ready-for-agent"]),           // not an input label
        ];
        let items = repo_items(&facts(Some(&issues), None, &[]), &no_live());
        let tracker: Vec<_> = items
            .iter()
            .filter(|i| i.kind == InboxKind::TrackerInput)
            .collect();
        assert_eq!(tracker.len(), 3);
        assert_eq!(tracker[0].target, Target::Issue(7));
        assert_eq!(tracker[0].reason(), "needs-info");
        assert_eq!(tracker[1].reason(), "needs-triage");
        // Both labels: the more urgent one names the item.
        assert_eq!(tracker[2].target, Target::Issue(9));
        assert_eq!(tracker[2].reason(), "needs-info");
        assert_eq!(tracker[0].since, parse_ts("2026-09-24T18:00:00Z"));

        // Clears when the label is removed...
        let issues = vec![issue(7, &["enhancement"])];
        let items = repo_items(&facts(Some(&issues), None, &[]), &no_live());
        assert!(items.is_empty());
        // ...or when the issue is closed (absent from the open list).
        let items = repo_items(&facts(Some(&[]), None, &[]), &no_live());
        assert!(items.is_empty());
    }

    // --- ordering, empty state, degradation ---

    #[test]
    fn ordering_is_blocking_then_merge_ready_then_informational_oldest_first() {
        let issues = vec![issue(7, &["needs-info"]), issue(8, &[])];
        let prs = vec![
            pr(20, "flock/issue-8-thing", "MERGEABLE", "APPROVED"),
            pr(21, "flock/issue-8-other", "MERGEABLE", "CHANGES_REQUESTED"),
        ];
        let mut older_pr = pr(22, "flock/issue-8-third", "MERGEABLE", "CHANGES_REQUESTED");
        older_pr.updated_at = "2026-09-01T00:00:00Z".to_string();
        let prs = [prs, vec![older_pr]].concat();
        let events = vec![ev_data(
            "r1",
            "run_stopped",
            8,
            serde_json::json!({"reason": "worker blocked"}),
        )];
        let items = repo_items(&facts(Some(&issues), Some(&prs), &events), &no_live());
        let got = kinds(&items);
        assert_eq!(
            got,
            vec![
                InboxKind::BlockingReview, // PR #22, oldest (2026-09-01)
                InboxKind::BlockingReview, // PR #21
                InboxKind::StoppedRun,     // issue 8 (2026-09-24T18:52)
                InboxKind::AwaitingMerge,  // PR #20
                InboxKind::TrackerInput,   // issue 7
            ]
        );
        let targets: Vec<Target> = items.iter().map(|i| i.target).collect();
        assert_eq!(
            targets,
            vec![
                Target::Pr(22),
                Target::Pr(21),
                Target::Issue(8),
                Target::Pr(20),
                Target::Issue(7),
            ]
        );
    }

    #[test]
    fn empty_facts_yield_an_empty_inbox() {
        assert!(repo_items(&facts(None, None, &[]), &no_live()).is_empty());
        assert!(repo_items(&facts(Some(&[]), Some(&[]), &[]), &no_live()).is_empty());
    }

    #[test]
    fn unfetched_github_data_degrades_to_journal_only_categories() {
        // Issues fetched, PR fetch failed: journal categories still
        // derive (a stopped run needs no PR data); PR-state categories
        // simply derive nothing rather than erroring.
        let issues = vec![issue(7, &[])];
        let events = vec![ev_data(
            "r1",
            "run_stopped",
            7,
            serde_json::json!({"reason": "PR not green"}),
        )];
        let items = repo_items(&facts(Some(&issues), None, &events), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::StoppedRun]);
    }

    // --- issue fetch failed: journal blocking facts still gate merge-ready ---

    #[test]
    fn unfetched_issues_do_not_unlock_a_journal_blocked_pr() {
        // Issue fetch failed (issues=None) but PR data is available and
        // the journal's latest verdict is blocking: the green PR must not
        // be offered as merge-ready (the spec's "no blocking review" gate
        // cannot depend on the issue fetch succeeding).
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "APPROVED")];
        let events = vec![
            ev("r1", "pr_opened", 7),
            ev_data(
                "r1",
                "review_verdict",
                7,
                serde_json::json!({"verdict": "blocking"}),
            ),
        ];
        let items = repo_items(&facts(None, Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));

        // Correlation rides the events' own pr/branch fields too, not
        // just the flock/issue-<n>- branch pattern.
        let prs = vec![pr(20, "feature/other", "MERGEABLE", "APPROVED")];
        let mut opened = ev("r1", "pr_opened", 7);
        opened.pr = Some(20);
        let events = vec![
            opened,
            ev_data(
                "r1",
                "review_verdict",
                7,
                serde_json::json!({"verdict": "blocking"}),
            ),
        ];
        let items = repo_items(&facts(None, Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));
    }

    #[test]
    fn unfetched_issues_do_not_unlock_a_parked_pr() {
        // Same degraded fetch, but the blocking signal is a parked
        // `review blocking` stop: the still-open PR is not merge-ready.
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "APPROVED")];
        let events = vec![
            ev("r1", "pr_opened", 7),
            ev_data(
                "r1",
                "run_stopped",
                7,
                serde_json::json!({"reason": "review blocking: 2 findings on PR #20"}),
            ),
        ];
        let items = repo_items(&facts(None, Some(&prs), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::AwaitingMerge));

        // A newer run supersedes the stop in stage derivation, so the
        // suppression clears with it.
        let mut events = events;
        events.push(ev("r2", "run_started", 7));
        events.push(ev("r2", "issue_dispatched", 7));
        let items = repo_items(&facts(None, Some(&prs), &events), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::AwaitingMerge]);
    }

    #[test]
    fn unfetched_issues_superseded_verdict_stays_merge_ready() {
        // The same supersession rules as the issue-loop path apply: a
        // blocking verdict that is no longer the operative state does not
        // suppress the merge-ready row.
        let prs = vec![pr(20, "flock/issue-7-thing", "MERGEABLE", "APPROVED")];
        let verdict =
            |v: &str| ev_data("r1", "review_verdict", 7, serde_json::json!({"verdict": v}));
        let base = || vec![ev("r1", "pr_opened", 7), verdict("blocking")];

        // Superseded by a newer clean verdict...
        let mut events = base();
        events.push(verdict("clean"));
        let items = repo_items(&facts(None, Some(&prs), &events), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::AwaitingMerge]);
        // ...by a rework dispatched after the verdict...
        let mut events = base();
        events.push(ev("r1", "rework_dispatched", 7));
        let items = repo_items(&facts(None, Some(&prs), &events), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::AwaitingMerge]);
        // ...or by a newer run (the verdict is no longer in the latest
        // run).
        let mut events = base();
        events.push(ev("r2", "run_started", 7));
        events.push(ev("r2", "issue_dispatched", 7));
        let items = repo_items(&facts(None, Some(&prs), &events), &no_live());
        assert_eq!(kinds(&items), vec![InboxKind::AwaitingMerge]);
    }

    // --- timestamps and ages ---

    #[test]
    fn parse_ts_handles_journal_and_gh_shapes() {
        assert_eq!(
            parse_ts("1970-01-01T00:00:00Z"),
            Some(SystemTime::UNIX_EPOCH)
        );
        let expected = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_275_948);
        assert_eq!(parse_ts("2026-09-24T18:52:28Z"), Some(expected));
        // Fractional seconds are ignored.
        assert_eq!(parse_ts("2026-09-24T18:52:28.183Z"), Some(expected));
        // Offsets normalize to UTC.
        assert_eq!(parse_ts("2026-09-24T20:52:28+02:00"), Some(expected));
        assert_eq!(parse_ts("2026-09-24T16:52:28-02:00"), Some(expected));
        // Leap day.
        assert_eq!(
            parse_ts("2000-02-29T12:00:00Z"),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(951_825_600))
        );
        // Anything unrecognized yields no timestamp, not a wrong one.
        for bad in [
            "",
            "not a timestamp",
            "2026-09-24",
            "2026-09-24 18:52:28Z",
            "2026-13-24T18:52:28Z",
            "2026-09-24T25:52:28Z",
            "2026-09-24T18:52Z",
        ] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
    }

    #[test]
    fn parse_ts_rejects_out_of_range_and_overflowing_components() {
        // Malformed-but-parseable components must yield None, never an
        // overflow panic (debug builds) or a wrapped bogus SystemTime
        // (release builds).
        for bad in [
            // Huge year: overflows days-from-civil and the epoch seconds.
            "999999999-09-24T18:52:28Z",
            "9999999999999999999-01-01T00:00:00Z",
            // Pre-epoch dates cannot become a u64 second count.
            "0000-01-01T00:00:00Z",
            "1969-12-31T23:59:59Z",
            // Month/day out of range.
            "2026-00-24T18:52:28Z",
            "2026-99-24T18:52:28Z",
            "2026-09-00T18:52:28Z",
            "2026-09-32T18:52:28Z",
            // Hour/minute/second out of range.
            "2026-09-24T99:52:28Z",
            "2026-09-24T18:99:28Z",
            "2026-09-24T18:52:99Z",
            // Huge or out-of-range offsets: overflow the i64 multiply,
            // or exceed the widest real zone.
            "2026-09-24T18:52:28+9999999999999999999:00",
            "2026-09-24T18:52:28+24:00",
            "2026-09-24T18:52:28-99:59",
            "2026-09-24T18:52:28+02:99",
            // A valid offset that pushes the instant below the epoch.
            "1970-01-01T00:00:00+01:00",
        ] {
            assert_eq!(parse_ts(bad), None, "{bad}");
        }
    }

    #[test]
    fn format_age_scales_with_magnitude() {
        assert_eq!(format_age(Duration::from_secs(0)), "0s");
        assert_eq!(format_age(Duration::from_secs(45)), "45s");
        assert_eq!(format_age(Duration::from_secs(60)), "1m");
        assert_eq!(format_age(Duration::from_secs(3599)), "59m");
        assert_eq!(format_age(Duration::from_secs(3600)), "1h");
        assert_eq!(format_age(Duration::from_secs(86_399)), "23h");
        assert_eq!(format_age(Duration::from_secs(86_400)), "1d");
        assert_eq!(format_age(Duration::from_secs(86_400 * 9)), "9d");
    }

    #[test]
    fn stop_head_strips_detail_suffixes() {
        assert_eq!(stop_head("review blocking"), "review blocking");
        assert_eq!(
            stop_head("review blocking: 3 Important findings on PR #5"),
            "review blocking"
        );
        assert_eq!(stop_head("limit reached: max cycles 3"), "limit reached");
        assert!(needs_human("PR not green"));
        assert!(!needs_human("limit reached: max cycles 3"));
    }
}
