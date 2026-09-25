//! The cross-repo activity feed: one reverse-chronological stream of
//! workflow events built from every discovered repo's Flock journal
//! (`<repo>/.flock/events.jsonl`). Derivation is pure over the parsed
//! journal events the slot cache already holds — they are re-read on
//! every agent poll by the same tolerant reader the stage derivation
//! uses (missing files, truncated tails, and unknown event names degrade
//! to fewer events, never to an error), so the feed tails the journals
//! on the normal refresh cycle with no state of its own. The journal
//! file is append-only and never touched; the slot cache holds only the
//! bounded newest tail (`journal::MAX_JOURNAL_EVENTS`, applied at the
//! reader so retained memory and per-poll parse work stay flat), and the
//! per-repo bound below drops old events from the view only.

use std::cmp::Ordering;
use std::time::SystemTime;

use crate::inbox::parse_ts;
use crate::journal::Event;

/// Most recent journal events shown per repo. Journals are append-only,
/// so the newest events are the file's tail; older events are dropped
/// from the view (never from the file) to keep the rendered feed small.
/// The slot cache itself is bounded separately and much more loosely at
/// the reader (`journal::MAX_JOURNAL_EVENTS`), so stage/inbox derivation
/// keeps history far beyond this window.
pub const MAX_EVENTS_PER_REPO: usize = 500;

/// One feed line: a journal event plus the repo it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedEvent {
    /// Display name of the repo, matching the rendered repo row's key
    /// ("owner/name", or the full working-directory path for repos
    /// without a GitHub identity).
    pub repo: String,
    /// The event's `ts` parsed; `None` when missing or malformed.
    pub ts: Option<SystemTime>,
    /// The event's `ts` verbatim, for display.
    pub ts_text: Option<String>,
    pub workflow: String,
    /// The event name verbatim; unknown names (the schema is
    /// additive-only) render like any other, just with no extracted
    /// detail.
    pub event: String,
    pub issue: Option<u64>,
    pub pr: Option<u64>,
    /// The salient field for the event type (stop reason, verdict, gate
    /// result, dispatch target, PR branch); empty for event types
    /// without one.
    pub detail: String,
    /// The event's line index within its repo's journal: the order
    /// tie-break among same-timestamp events (file position is time
    /// order within a journal).
    pos: usize,
}

impl FeedEvent {
    /// The issue/PR the event is about: "#7", "PR #5", "#7 PR #5" when it
    /// carries both, empty when it names neither.
    pub fn target(&self) -> String {
        match (self.issue, self.pr) {
            (Some(i), Some(p)) => format!("#{i} PR #{p}"),
            (Some(i), None) => format!("#{i}"),
            (None, Some(p)) => format!("PR #{p}"),
            (None, None) => String::new(),
        }
    }
}

/// The newest `MAX_EVENTS_PER_REPO` events of one repo as feed events, in
/// journal (file-position) order. The bound is what keeps the view's
/// memory flat as journals grow.
pub fn repo_feed(repo: &str, events: &[Event]) -> Vec<FeedEvent> {
    let start = events.len().saturating_sub(MAX_EVENTS_PER_REPO);
    events[start..]
        .iter()
        .enumerate()
        .map(|(i, e)| FeedEvent {
            repo: repo.to_string(),
            ts: e.ts.as_deref().and_then(parse_ts),
            ts_text: e.ts.clone(),
            workflow: e.workflow.clone(),
            event: e.event.clone(),
            issue: e.issue,
            pr: e.pr,
            detail: salient(e),
            pos: start + i,
        })
        .collect()
}

/// Merges per-repo journals into one reverse-chronological feed ordered
/// by event `ts` (newest first), which stays correct across repos even
/// when their clocks or append rates differ. Events whose `ts` is missing
/// or unparseable sort after every timestamped event; ties fall back to
/// repo name, then to journal position (file position is time order
/// within a repo) for full determinism.
pub fn build_feed<'a>(repos: impl IntoIterator<Item = (String, &'a [Event])>) -> Vec<FeedEvent> {
    let mut feed: Vec<FeedEvent> = repos
        .into_iter()
        .flat_map(|(repo, events)| repo_feed(&repo, events))
        .collect();
    feed.sort_by(|a, b| {
        newest_first(a.ts, b.ts)
            .then_with(|| a.repo.cmp(&b.repo))
            .then_with(|| b.pos.cmp(&a.pos))
    });
    feed
}

/// Timestamped events newest first; an event without a usable timestamp
/// sorts after every timestamped one rather than erroring the feed.
fn newest_first(a: Option<SystemTime>, b: Option<SystemTime>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Filters a built feed to one repo; `None` is the "all" filter and keeps
/// every repo.
pub fn filter_repo(feed: &[FeedEvent], repo: Option<&str>) -> Vec<FeedEvent> {
    feed.iter()
        .filter(|e| repo.is_none_or(|r| e.repo == r))
        .cloned()
        .collect()
}

/// The one field worth showing for a known event type: stop reason for
/// `run_stopped`, verdict for `review_verdict`, passed/failed for
/// `gate_result`, the supervising workspace/pane (or worktree) for
/// dispatches, the branch for `pr_opened`. Unknown event types and
/// missing or unexpectedly shaped fields degrade to no detail — the
/// event line still renders.
fn salient(e: &Event) -> String {
    let field = |keys: &[&str]| {
        e.data
            .as_ref()
            .and_then(|d| keys.iter().find_map(|k| d.get(*k)?.as_str()))
    };
    match e.event.as_str() {
        "run_stopped" => field(&["reason"]).unwrap_or("unknown").to_string(),
        "review_verdict" => field(&["verdict"]).unwrap_or_default().to_string(),
        "gate_result" => match e
            .data
            .as_ref()
            .and_then(|d| d.get("passed"))
            .and_then(|p| p.as_bool())
        {
            Some(true) => "passed".to_string(),
            Some(false) => "failed".to_string(),
            // A string-shaped `passed` (schema drift) renders verbatim; a
            // missing one renders no detail.
            None => field(&["passed"]).unwrap_or_default().to_string(),
        },
        "issue_dispatched" | "rework_dispatched" => {
            let pane = field(&["pane_id", "pane"]);
            match (field(&["workspace_id", "workspace"]), pane) {
                (Some(ws), Some(p)) => format!("{ws}/{p}"),
                (None, Some(p)) => p.to_string(),
                _ => field(&["worktree", "worktree_path", "worktree_root"])
                    .unwrap_or_default()
                    .to_string(),
            }
        }
        "pr_opened" => e.branch.clone().unwrap_or_default(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(run: &str, event: &str, ts: &str) -> Event {
        Event {
            run_id: run.to_string(),
            workflow: "operator-run".to_string(),
            event: event.to_string(),
            ts: Some(ts.to_string()),
            ..Default::default()
        }
    }

    fn ev_data(run: &str, event: &str, data: serde_json::Value) -> Event {
        Event {
            data: Some(data),
            ..ev(run, event, "2026-09-24T18:00:00Z")
        }
    }

    fn feed_lines(feed: &[FeedEvent]) -> Vec<(&str, &str)> {
        feed.iter()
            .map(|e| (e.repo.as_str(), e.event.as_str()))
            .collect()
    }

    // --- ordering ---

    #[test]
    fn orders_by_event_ts_across_repos() {
        // Repo o/a's clock runs behind repo o/b's appends; the merged
        // order must follow each event's own timestamp, not file order,
        // repo order, or arrival order.
        let a = vec![
            ev("r1", "run_started", "2026-09-24T18:00:00Z"),
            ev("r1", "issue_closed", "2026-09-24T18:30:00Z"),
        ];
        let b = vec![
            ev("r2", "issue_dispatched", "2026-09-24T18:10:00Z"),
            ev("r2", "pr_opened", "2026-09-24T18:40:00Z"),
        ];
        let feed = build_feed(vec![
            ("o/a".to_string(), a.as_slice()),
            ("o/b".to_string(), b.as_slice()),
        ]);
        assert_eq!(
            feed_lines(&feed),
            vec![
                ("o/b", "pr_opened"),
                ("o/a", "issue_closed"),
                ("o/b", "issue_dispatched"),
                ("o/a", "run_started"),
            ]
        );
    }

    #[test]
    fn unparseable_and_missing_timestamps_sort_last_without_error() {
        let mut no_ts = ev("r1", "run_started", "2026-09-24T18:00:00Z");
        no_ts.ts = None;
        let bad_ts = ev("r1", "pr_opened", "not a timestamp");
        let good = ev("r1", "issue_closed", "2026-09-24T17:00:00Z");
        let feed = build_feed(vec![("o/a".to_string(), [no_ts, bad_ts, good].as_slice())]);
        // The one parseable timestamp leads even though it is the oldest
        // line in the journal; the rest fall back to journal position,
        // later line first (file position is time order).
        assert_eq!(
            feed_lines(&feed),
            vec![
                ("o/a", "issue_closed"),
                ("o/a", "pr_opened"),
                ("o/a", "run_started"),
            ]
        );
    }

    #[test]
    fn malformed_numeric_timestamps_never_panic_and_sort_last() {
        // Corrupt journal `ts` values whose components are numeric but
        // absurd (huge year, huge offset, out-of-range fields) parse to
        // None via checked arithmetic — no overflow panic in debug
        // builds, no wrapped bogus SystemTime in release — and sort
        // after every timestamped event.
        let events = vec![
            ev("r1", "run_started", "2026-09-24T18:00:00Z"),
            ev("r1", "gate_result", "999999999-09-24T18:00:00Z"),
            ev(
                "r1",
                "pr_opened",
                "2026-09-24T18:00:00+9999999999999999999:00",
            ),
            ev("r1", "run_stopped", "2026-13-32T99:99:99Z"),
            ev("r1", "issue_closed", "2026-09-24T18:30:00Z"),
        ];
        let feed = build_feed(vec![("o/a".to_string(), events.as_slice())]);
        assert_eq!(feed.len(), 5);
        assert_eq!(
            feed_lines(&feed),
            vec![
                ("o/a", "issue_closed"),
                ("o/a", "run_started"),
                // The malformed lines keep journal order among
                // themselves, later line first.
                ("o/a", "run_stopped"),
                ("o/a", "pr_opened"),
                ("o/a", "gate_result"),
            ]
        );
    }

    #[test]
    fn equal_timestamps_tie_break_deterministically() {
        let ts = "2026-09-24T18:00:00Z";
        let a = vec![ev("r1", "run_started", ts), ev("r1", "run_stopped", ts)];
        let b = vec![ev("r2", "run_started", ts)];
        let feed = build_feed(vec![
            ("o/b".to_string(), b.as_slice()),
            ("o/a".to_string(), a.as_slice()),
        ]);
        // Same ts: repo name first, then later journal position is newer.
        assert_eq!(
            feed_lines(&feed),
            vec![
                ("o/a", "run_stopped"),
                ("o/a", "run_started"),
                ("o/b", "run_started"),
            ]
        );
    }

    // --- per-repo memory bound ---

    #[test]
    fn per_repo_bound_keeps_only_the_newest_n_events() {
        let events: Vec<Event> = (0..MAX_EVENTS_PER_REPO + 100)
            .map(|_| ev("r1", "run_started", "2026-09-24T18:00:00Z"))
            .collect();
        let feed = repo_feed("o/a", &events);
        assert_eq!(feed.len(), MAX_EVENTS_PER_REPO);
        // The dropped events are the oldest (the file head); the newest
        // line of the journal always survives.
        assert_eq!(feed.first().unwrap().pos, 100);
        assert_eq!(feed.last().unwrap().pos, MAX_EVENTS_PER_REPO + 99);
    }

    // --- filtering ---

    #[test]
    fn filter_selects_one_repo_or_all() {
        let a = vec![ev("r1", "run_started", "2026-09-24T18:00:00Z")];
        let b = vec![ev("r2", "run_started", "2026-09-24T18:01:00Z")];
        let feed = build_feed(vec![
            ("o/a".to_string(), a.as_slice()),
            ("o/b".to_string(), b.as_slice()),
        ]);
        assert_eq!(filter_repo(&feed, None).len(), 2, "None is the all filter");
        let only_a = filter_repo(&feed, Some("o/a"));
        assert_eq!(feed_lines(&only_a), vec![("o/a", "run_started")]);
        assert!(filter_repo(&feed, Some("o/missing")).is_empty());
    }

    // --- tolerant reading (rides the shared journal reader) ---

    #[test]
    fn truncated_journal_tail_shows_the_killed_runs_last_valid_event() {
        // A supervisor killed mid-append leaves a partial final line; the
        // tolerant reader skips it and the feed still shows the killed
        // run's last complete event without error.
        let text = concat!(
            r#"{"v":1,"ts":"2026-09-24T18:52:28Z","run_id":"r1","workflow":"operator-run","event":"issue_dispatched","repo":"o/a","issue":7}"#,
            "\n",
            r#"{"v":1,"ts":"2026-09-24T18:53:01Z","run_id":"r1","workflow":"operator-run","event":"pr_ope"#,
        );
        let events = crate::journal::parse_events(text);
        let feed = repo_feed("o/a", &events);
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0].event, "issue_dispatched");
        assert_eq!(feed[0].issue, Some(7));
    }

    #[test]
    fn unknown_event_names_render_with_no_detail() {
        let events = vec![ev("r1", "future_event_v2", "2026-09-24T18:00:00Z")];
        let feed = repo_feed("o/a", &events);
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0].event, "future_event_v2");
        assert_eq!(feed[0].detail, "");
    }

    #[test]
    fn missing_journal_yields_an_empty_feed() {
        assert!(repo_feed("o/a", &[]).is_empty());
        assert!(build_feed(vec![("o/a".to_string(), [].as_slice())]).is_empty());
    }

    // --- salient detail and target ---

    #[test]
    fn salient_detail_per_event_type() {
        // run_stopped → the verbatim stop reason.
        let stop = ev_data(
            "r1",
            "run_stopped",
            serde_json::json!({"reason": "PR not green"}),
        );
        assert_eq!(salient(&stop), "PR not green");
        // A stop without a reason degrades to "unknown", matching the
        // stage derivation.
        assert_eq!(
            salient(&ev("r1", "run_stopped", "2026-09-24T18:00:00Z")),
            "unknown"
        );
        // review_verdict → the verdict.
        let verdict = ev_data(
            "r1",
            "review_verdict",
            serde_json::json!({"verdict": "blocking"}),
        );
        assert_eq!(salient(&verdict), "blocking");
        // gate_result → passed/failed from the boolean.
        let passed = ev_data("r1", "gate_result", serde_json::json!({"passed": true}));
        assert_eq!(salient(&passed), "passed");
        let failed = ev_data("r1", "gate_result", serde_json::json!({"passed": false}));
        assert_eq!(salient(&failed), "failed");
        // dispatches → workspace/pane, or the worktree when there are no
        // pane ids.
        let dispatch = ev_data(
            "r1",
            "issue_dispatched",
            serde_json::json!({"workspace_id": "w1", "pane_id": "w1:p1"}),
        );
        assert_eq!(salient(&dispatch), "w1/w1:p1");
        let worktree = ev_data(
            "r1",
            "rework_dispatched",
            serde_json::json!({"worktree": "/wt/7"}),
        );
        assert_eq!(salient(&worktree), "/wt/7");
        // pr_opened → the branch.
        let mut opened = ev("r1", "pr_opened", "2026-09-24T18:00:00Z");
        opened.branch = Some("flock/issue-7-thing".to_string());
        assert_eq!(salient(&opened), "flock/issue-7-thing");
        // Unexpectedly shaped fields yield no detail, not a crash.
        let odd = ev_data(
            "r1",
            "gate_result",
            serde_json::json!({"passed": {"unexpected": "shape"}}),
        );
        assert_eq!(salient(&odd), "");
        // Events with no salient field.
        assert_eq!(
            salient(&ev("r1", "merge_verified", "2026-09-24T18:00:00Z")),
            ""
        );
        assert_eq!(
            salient(&ev("r1", "issue_closed", "2026-09-24T18:00:00Z")),
            ""
        );
    }

    #[test]
    fn target_combines_issue_and_pr() {
        let mut e = ev("r1", "pr_opened", "2026-09-24T18:00:00Z");
        e.issue = Some(7);
        e.pr = Some(20);
        assert_eq!(repo_feed("o/a", &[e])[0].target(), "#7 PR #20");
        let mut e = ev("r1", "issue_dispatched", "2026-09-24T18:00:00Z");
        e.issue = Some(7);
        assert_eq!(repo_feed("o/a", &[e])[0].target(), "#7");
        let e = ev("r1", "run_started", "2026-09-24T18:00:00Z");
        assert_eq!(repo_feed("o/a", &[e])[0].target(), "");
    }
}
