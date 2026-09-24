//! The "waiting on you" inbox: one prioritized, cross-repo list of
//! everything that needs the human — blocking reviews, parked PRs, green
//! PRs awaiting merge, stopped operator runs, and tracker items needing
//! input. Derivation is pure over the same per-repo facts the repo views
//! use (tracker issues, open PRs, the Flock event journal via
//! `journal::derive_stage`, the project-config green classifier), so every
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
    /// blocking review: the human merges per policy.
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
/// views) and its parsed journal events.
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

/// Derives every inbox item for one repo, sorted by `sort_items` order.
///
/// The journal-derived categories ride `journal::derive_stage` so they
/// share its precedence and clearing semantics exactly: a blocking verdict
/// superseded by a rework dispatch, a newer verdict, or a newer run no
/// longer derives `ReviewBlocking`; a stopped run's item disappears as
/// soon as a newer run starts (the stage is no longer `Stopped`). A
/// `review blocking` stop with the PR still open is the parked-PR
/// category, which subsumes both the blocking-review and the stopped-run
/// representation of that stop — one row per underlying state.
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
        match &stage {
            Stage::ReviewBlocking => {
                // The stage guarantees the latest verdict is the blocking
                // one, so the last review_verdict event is the trigger.
                let since = events
                    .iter()
                    .rev()
                    .find(|e| e.is("review_verdict"))
                    .and_then(|e| e.ts.as_deref())
                    .and_then(parse_ts);
                let item = match pr {
                    Some(p) => {
                        blocked_prs.push(p.number);
                        make_item(facts.repo, InboxKind::BlockingReview, p, "", since)
                    }
                    None => InboxItem {
                        repo: facts.repo.to_string(),
                        kind: InboxKind::BlockingReview,
                        target: Target::Issue(issue.number),
                        title: issue.title.clone(),
                        detail: String::new(),
                        since,
                    },
                };
                items.push(item);
            }
            Stage::Stopped(reason) => {
                let since = events
                    .iter()
                    .rev()
                    .find(|e| e.is("run_stopped"))
                    .and_then(|e| e.ts.as_deref())
                    .and_then(parse_ts);
                if let ("review blocking", Some(p)) = (stop_head(reason), pr) {
                    // Cycle ended review-blocking and the PR is still open:
                    // parked per Flock's parked-PR policy. This subsumes the
                    // stopped-run item for the same stop.
                    blocked_prs.push(p.number);
                    items.push(make_item(
                        facts.repo,
                        InboxKind::ParkedPr,
                        p,
                        stop_head(reason),
                        since,
                    ));
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
            _ => {}
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
                && journal::classify_green(&pr.checks, &pr.mergeable) == PrGreen::Green
            {
                // Drafts are excluded: a draft is by definition not
                // awaiting merge. The classifier already excludes
                // conflicts (CONFLICTING is failing), pending checks, and
                // unknown shapes (fail-closed).
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
    if dp.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let (hms, offset) = match time.strip_suffix('Z') {
        Some(t) => (t, 0),
        None => {
            let idx = time.find(['+', '-'])?;
            let (t, sign) = (&time[..idx], &time[idx..]);
            let (oh, om) = sign[1..].split_once(':')?;
            let secs: i64 = oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60;
            (t, if sign.starts_with('-') { -secs } else { secs })
        }
    };
    let hms = hms.split('.').next()?;
    let mut tp = hms.split(':');
    let hh: i64 = tp.next()?.parse().ok()?;
    let mm: i64 = tp.next()?.parse().ok()?;
    let ss: i64 = tp.next()?.parse().ok()?;
    if tp.next().is_some() || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let secs = days_from_civil(y, m, d) * 86_400 + hh * 3600 + mm * 60 + ss - offset;
    u64::try_from(secs)
        .ok()
        .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
}

/// Days since the Unix epoch for a Gregorian date (Howard Hinnant's
/// days-from-civil algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
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

        // Clears when the PR is merged/closed (no longer open). The stop
        // itself still needs a human, so it falls back to a stopped-run
        // item until a newer run starts.
        let items = repo_items(&facts(Some(&issues), Some(&[]), &events), &no_live());
        assert!(!kinds(&items).contains(&InboxKind::ParkedPr));
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
