//! The Flock event journal (`<repo>/.flock/events.jsonl`, schema v1 from
//! flock#45) and the per-issue workflow-stage derivation built on it. The
//! journal is advisory and written only by Flock supervisors, so everything
//! here is read-only and fail-tolerant: a missing file, a truncated final
//! line (crash mid-append), unknown event names (the schema is
//! additive-only), and events from other workflows all degrade to fewer
//! known facts, never to an error.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::github::CheckRollup;
use crate::state::LABEL_PRIORITY;

/// One parsed journal line. Fields the schema marks optional stay optional;
/// anything unrecognized (new event names, new `data` shapes) is preserved
/// but simply never matches a derivation rule. Ordering is by file position
/// (`parse_events` preserves line order): the journal is append-only, so
/// position order is time order even when a `ts` is malformed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    pub run_id: String,
    pub workflow: String,
    pub event: String,
    pub issue: Option<u64>,
    pub pr: Option<u64>,
    pub branch: Option<String>,
    pub data: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct RawEvent {
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    workflow: String,
    #[serde(default)]
    event: String,
    #[serde(default)]
    issue: Option<u64>,
    #[serde(default)]
    pr: Option<u64>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    data: Option<serde_json::Value>,
    // `v` and `ts` are tool-generated metadata; the reader keys ordering off
    // file position and tolerates any schema version, so neither is kept.
}

impl Event {
    fn is(&self, name: &str) -> bool {
        self.event == name
    }

    /// `data.verdict` from a `review_verdict` event ("clean"|"blocking"),
    /// verbatim; anything else is tolerated as unknown.
    fn verdict(&self) -> Option<&str> {
        self.data.as_ref()?.get("verdict")?.as_str()
    }

    /// `data.reason` from a `run_stopped` event, verbatim.
    fn reason(&self) -> String {
        self.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str())
            .unwrap_or("unknown")
            .to_string()
    }
}

/// Parses journal text tolerantly: blank lines and unparseable lines
/// (including a truncated final line from a crash mid-append) are skipped,
/// and unknown event names parse fine — they just match no derivation rule.
pub fn parse_events(text: &str) -> Vec<Event> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str::<RawEvent>(line).ok().map(|r| Event {
                run_id: r.run_id,
                workflow: r.workflow,
                event: r.event,
                issue: r.issue,
                pr: r.pr,
                branch: r.branch,
                data: r.data,
            })
        })
        .collect()
}

/// Reads one candidate journal file. `None` when the file is missing or
/// unreadable — an absent journal is a normal state (repo not under Flock
/// management), not an error.
fn read_file(path: &Path) -> Option<Vec<Event>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(parse_events(&text))
}

/// Reads the journal for a repo given its live local checkouts. Several
/// cwds can map to one repo (the operator's main checkout plus agent
/// worktrees), but supervisors only ever write the journal in the checkout
/// they run in, so the first cwd (in sorted order, for determinism) that
/// actually has a readable journal wins. No journal anywhere yields no
/// events, and stages fall back to label/PR inference.
pub fn read_repo_events(cwds: &BTreeSet<PathBuf>) -> Vec<Event> {
    cwds.iter()
        .find_map(|cwd| read_file(&cwd.join(".flock").join("events.jsonl")))
        .unwrap_or_default()
}

/// The green classifier for a PR's checks, mirroring the verified
/// check-wait semantics in `docs/flock/project.md`: SKIPPED/NEUTRAL
/// conclusions count as satisfied, and anything unrecognized (a rollup
/// entry with neither a check-run nor a status-context shape) is
/// fail-closed — it classifies as failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrGreen {
    /// `mergeable: MERGEABLE` and every rollup entry satisfied.
    Green,
    /// Nothing failing, but something still running or mergeability
    /// unresolved.
    Pending,
    /// Any failing/errored entry, or an entry shape we do not recognize.
    Failing,
}

/// A rollup entry is check-run-shaped when it carries `status` or
/// `conclusion` (gh emits check runs with both keys, conclusion possibly
/// null while running), status-context-shaped when it carries `state`.
fn entry_ok(r: &CheckRollup) -> bool {
    if r.status.is_some() || r.conclusion.is_some() {
        r.status.as_deref() == Some("COMPLETED")
            && matches!(
                r.conclusion.as_deref(),
                Some("SUCCESS") | Some("SKIPPED") | Some("NEUTRAL")
            )
    } else if r.state.is_some() {
        r.state.as_deref() == Some("SUCCESS")
    } else {
        false
    }
}

fn entry_bad(r: &CheckRollup) -> bool {
    if r.status.is_some() || r.conclusion.is_some() {
        r.status.as_deref() == Some("COMPLETED")
            && matches!(
                r.conclusion.as_deref(),
                Some("FAILURE")
                    | Some("CANCELLED")
                    | Some("TIMED_OUT")
                    | Some("ACTION_REQUIRED")
                    | Some("STARTUP_FAILURE")
            )
    } else if r.state.is_some() {
        matches!(r.state.as_deref(), Some("ERROR") | Some("FAILURE"))
    } else {
        true // unrecognized shape: fail closed
    }
}

pub fn classify_green(checks: &[CheckRollup], mergeable: &str) -> PrGreen {
    if checks.iter().any(entry_bad) {
        return PrGreen::Failing;
    }
    // With no checks at all the "all satisfied" half is vacuous, matching
    // the project-config jq: mergeability alone decides.
    if mergeable == "MERGEABLE" && checks.iter().all(entry_ok) {
        PrGreen::Green
    } else {
        PrGreen::Pending
    }
}

/// The live herdr agent set, used to tell a died-mid-run issue from a live
/// one: the journal is advisory, herdr liveness is ground truth.
#[derive(Default)]
pub struct LiveSet {
    /// (workspace_id, pane_id) of every live agent.
    panes: HashSet<(String, String)>,
    /// cwds of every live agent (an agent working in a worktree has the
    /// worktree path as its cwd).
    cwds: HashSet<PathBuf>,
}

impl LiveSet {
    pub fn new(
        panes: impl IntoIterator<Item = (String, String)>,
        cwds: impl IntoIterator<Item = PathBuf>,
    ) -> Self {
        Self {
            panes: panes.into_iter().collect(),
            cwds: cwds.into_iter().collect(),
        }
    }

    /// Whether the agent named by a dispatch event's `data` is still live.
    /// `Some(true/false)` when the data carries a correlatable identity
    /// (pane/workspace IDs or a worktree path); `None` when it carries
    /// nothing we can check, in which case no died verdict is made.
    fn dispatch_alive(&self, data: Option<&serde_json::Value>) -> Option<bool> {
        let data = data?;
        let field = |keys: &[&str]| keys.iter().find_map(|k| data.get(*k)?.as_str());
        let pane = field(&["pane_id", "pane"]);
        let workspace = field(&["workspace_id", "workspace"]);
        if let Some(pane) = pane {
            return Some(match workspace {
                Some(ws) => self.panes.contains(&(ws.to_string(), pane.to_string())),
                None => self.panes.iter().any(|(_, p)| p == pane),
            });
        }
        if let Some(worktree) = field(&["worktree", "worktree_path", "worktree_root"]) {
            return Some(self.cwds.contains(Path::new(worktree)));
        }
        None
    }
}

/// Where an in-flight issue is in the Flock workflow. `label()` strings are
/// the operator-facing vocabulary; `stopped:` reasons and label-inferred
/// stages are verbatim from the journal / tracker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stage {
    /// ready-for-agent, no events yet.
    Queued,
    Dispatched,
    PrOpen,
    ChecksPending,
    ChecksFailing,
    Mergeable,
    ReviewClean,
    ReviewBlocking,
    ReworkInProgress,
    AwaitingMerge,
    Done,
    /// `stopped: <reason>` — the verbatim operator-run stop string.
    Stopped(String),
    /// No terminal event for the latest run and its supervising agent is
    /// gone from herdr: "unknown (run may have died)".
    Died,
    /// Label-inferred state for a tracked label with no workflow signal
    /// (e.g. `needs-triage`, `blocked`).
    Label(String),
    /// No journal events, no tracked label, no correlated PR.
    None,
}

impl Stage {
    pub fn label(&self) -> String {
        match self {
            Self::Queued => "queued".to_string(),
            Self::Dispatched => "dispatched".to_string(),
            Self::PrOpen => "pr open".to_string(),
            Self::ChecksPending => "checks pending".to_string(),
            Self::ChecksFailing => "checks failing".to_string(),
            Self::Mergeable => "mergeable".to_string(),
            Self::ReviewClean => "review clean".to_string(),
            Self::ReviewBlocking => "review blocking".to_string(),
            Self::ReworkInProgress => "rework in progress".to_string(),
            Self::AwaitingMerge => "awaiting merge".to_string(),
            Self::Done => "done".to_string(),
            Self::Stopped(reason) => format!("stopped: {reason}"),
            Self::Died => "unknown (run may have died)".to_string(),
            Self::Label(label) => label.clone(),
            Self::None => "—".to_string(),
        }
    }

    /// Display ordering: actively worked first, then waiting-on-automation,
    /// then terminal/attention states, then the dormant queue. Lower sorts
    /// earlier on the board.
    pub fn rank(&self) -> u8 {
        match self {
            Self::ReworkInProgress | Self::Dispatched => 0,
            Self::ChecksPending | Self::ChecksFailing | Self::Mergeable | Self::PrOpen => 1,
            Self::ReviewBlocking | Self::ReviewClean | Self::AwaitingMerge => 2,
            Self::Died | Self::Stopped(_) => 3,
            Self::Queued => 4,
            Self::Label(_) | Self::None => 5,
            Self::Done => 6,
        }
    }
}

/// Everything the stage derivation knows about one issue: its tracker
/// labels, its journal events in file order, and the green state of its
/// correlated open PR (`None` when there is no correlated open PR or PR
/// data has not been fetched).
pub struct IssueFacts {
    pub labels: Vec<String>,
    pub events: Vec<Event>,
    pub pr_green: Option<PrGreen>,
}

/// Derives the workflow stage for one issue. Latest event wins; `run_id`
/// ties events to a run. The precedence is: terminal facts (closed,
/// stopped) first, then the died heuristic, then the review/rework ladder,
/// then PR/check progress, then bare dispatch, and finally — with no
/// usable events at all — label/PR inference.
pub fn derive_stage(facts: &IssueFacts, live: &LiveSet) -> Stage {
    let events = &facts.events;
    if events.is_empty() {
        return infer_stage(&facts.labels, facts.pr_green);
    }
    if events.iter().any(|e| e.is("issue_closed")) {
        return Stage::Done;
    }
    // The latest event names the latest run; a `run_stopped` in that run is
    // its terminal event and carries the verbatim stop reason.
    let last_run = &events.last().map(|e| e.run_id.clone()).unwrap_or_default();
    let run: Vec<&Event> = events.iter().filter(|e| &e.run_id == last_run).collect();
    if let Some(stop) = run.iter().rev().find(|e| e.is("run_stopped")) {
        return Stage::Stopped(stop.reason());
    }
    // No terminal event for the latest run: if its dispatch named an agent
    // and that agent is gone from herdr, the run likely died. No
    // correlatable dispatch data means no died verdict — fall through to
    // whatever the events do say.
    if let Some(dispatch) = run
        .iter()
        .rev()
        .find(|e| e.is("issue_dispatched") || e.is("rework_dispatched"))
    {
        if live.dispatch_alive(dispatch.data.as_ref()) == Some(false) {
            return Stage::Died;
        }
    }
    let last_verdict = events.iter().rposition(|e| e.is("review_verdict"));
    let last_rework = events.iter().rposition(|e| e.is("rework_dispatched"));
    // A rework dispatched after the latest verdict supersedes it.
    if let Some(rework) = last_rework {
        if last_verdict.is_none_or(|v| rework > v) {
            return Stage::ReworkInProgress;
        }
    }
    if let Some(v) = last_verdict {
        match events[v].verdict() {
            Some("blocking") => return Stage::ReviewBlocking,
            Some("clean") => {
                // Review clean: green checks (or an already-verified merge)
                // leave only the operator's merge/close step; ungreen checks
                // still gate it. With no PR data to classify, the clean
                // verdict itself is the freshest known state.
                if events.iter().any(|e| e.is("merge_verified")) {
                    return Stage::AwaitingMerge;
                }
                return match facts.pr_green {
                    Some(PrGreen::Green) => Stage::AwaitingMerge,
                    Some(PrGreen::Failing) => Stage::ChecksFailing,
                    Some(PrGreen::Pending) => Stage::ChecksPending,
                    None => Stage::ReviewClean,
                };
            }
            // Unknown verdict value: additive schema change; fall through.
            _ => {}
        }
    }
    if events.iter().any(|e| e.is("pr_opened")) || facts.pr_green.is_some() {
        return match facts.pr_green {
            Some(PrGreen::Failing) => Stage::ChecksFailing,
            Some(PrGreen::Pending) => Stage::ChecksPending,
            Some(PrGreen::Green) => Stage::Mergeable,
            None => Stage::PrOpen,
        };
    }
    if events.iter().any(|e| e.is("issue_dispatched")) {
        return Stage::Dispatched;
    }
    infer_stage(&facts.labels, facts.pr_green)
}

/// The stage when the journal says nothing usable about an issue: an open
/// PR from a flock branch means `pr open` (refined by its checks), a
/// ready-for-agent label means `queued`, any other tracked label is shown
/// verbatim, and anything else has no stage at all.
fn infer_stage(labels: &[String], pr_green: Option<PrGreen>) -> Stage {
    if let Some(green) = pr_green {
        return match green {
            PrGreen::Failing => Stage::ChecksFailing,
            PrGreen::Pending => Stage::ChecksPending,
            PrGreen::Green => Stage::Mergeable,
        };
    }
    if labels.iter().any(|l| l == "ready-for-agent") {
        return Stage::Queued;
    }
    LABEL_PRIORITY
        .iter()
        .find(|tracked| labels.iter().any(|l| l == **tracked))
        .map(|l| Stage::Label(l.to_string()))
        .unwrap_or(Stage::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(run: &str, event: &str) -> Event {
        Event {
            run_id: run.to_string(),
            workflow: "operator-run".to_string(),
            event: event.to_string(),
            issue: Some(7),
            ..Default::default()
        }
    }

    fn ev_data(run: &str, event: &str, data: serde_json::Value) -> Event {
        Event {
            data: Some(data),
            ..ev(run, event)
        }
    }

    fn facts(events: Vec<Event>) -> IssueFacts {
        IssueFacts {
            labels: vec![],
            events,
            pr_green: None,
        }
    }

    fn no_live() -> LiveSet {
        LiveSet::default()
    }

    fn rollup(status: Option<&str>, conclusion: Option<&str>, state: Option<&str>) -> CheckRollup {
        CheckRollup {
            status: status.map(str::to_string),
            conclusion: conclusion.map(str::to_string),
            state: state.map(str::to_string),
        }
    }

    // --- parsing tolerance ---

    #[test]
    fn parses_schema_v1_lines() {
        let text = concat!(
            r#"{"v":1,"ts":"2026-09-24T18:52:28.183Z","run_id":"r1","workflow":"github-issue-worker","event":"run_started","repo":"o/r","issue":1}"#,
            "\n",
            r#"{"v":1,"ts":"2026-09-24T19:05:01.524Z","run_id":"r1","workflow":"github-issue-worker","event":"pr_opened","repo":"o/r","issue":1,"pr":5,"branch":"flock/issue-1-x","data":{"k":"v"}}"#,
            "\n"
        );
        let events = parse_events(text);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].pr, Some(5));
        assert_eq!(events[1].branch.as_deref(), Some("flock/issue-1-x"));
        assert!(events[1].data.is_some());
    }

    #[test]
    fn truncated_last_line_is_skipped() {
        let text = concat!(
            r#"{"run_id":"r1","workflow":"w","event":"run_started","repo":"o/r","issue":1}"#,
            "\n",
            r#"{"run_id":"r1","workflow":"w","event":"pr_opened","repo":"o/r","iss"#, // crash mid-append
        );
        let events = parse_events(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "run_started");
    }

    #[test]
    fn corrupt_and_blank_lines_are_skipped() {
        let text = "\nnot json at all\n{\"run_id\":\"r1\",\"workflow\":\"w\",\"event\":\"issue_closed\",\"repo\":\"o/r\",\"issue\":1}\n{\"partial\":\n";
        let events = parse_events(text);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "issue_closed");
    }

    #[test]
    fn unknown_event_names_are_kept_but_match_no_rule() {
        let events = parse_events(
            r#"{"run_id":"r1","workflow":"w","event":"future_event_v2","repo":"o/r","issue":1}"#,
        );
        assert_eq!(events.len(), 1);
        // Unknown events contribute no stage facts; with nothing else known
        // the issue falls back to inference.
        assert_eq!(derive_stage(&facts(events), &no_live()), Stage::None);
    }

    #[test]
    fn missing_journal_yields_no_events() {
        let dir = tempfile::tempdir().unwrap();
        let cwds = BTreeSet::from([dir.path().to_path_buf()]);
        assert!(read_repo_events(&cwds).is_empty());
    }

    #[test]
    fn reads_first_checkout_that_has_a_journal() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a-worktree"); // no journal here
        let b = dir.path().join("b-main");
        std::fs::create_dir_all(b.join(".flock")).unwrap();
        std::fs::write(
            b.join(".flock").join("events.jsonl"),
            "{\"run_id\":\"r1\",\"workflow\":\"w\",\"event\":\"issue_closed\",\"repo\":\"o/r\",\"issue\":1}\n",
        )
        .unwrap();
        std::fs::create_dir_all(&a).unwrap();
        let cwds = BTreeSet::from([a, b]);
        let events = read_repo_events(&cwds);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "issue_closed");
    }

    // --- green classifier (project-config semantics) ---

    #[test]
    fn green_classifier_mirrors_project_config() {
        // All satisfied (SUCCESS/SKIPPED/NEUTRAL) + MERGEABLE → green.
        assert_eq!(
            classify_green(
                &[
                    rollup(Some("COMPLETED"), Some("SUCCESS"), None),
                    rollup(Some("COMPLETED"), Some("SKIPPED"), None),
                    rollup(Some("COMPLETED"), Some("NEUTRAL"), None),
                    rollup(None, None, Some("SUCCESS")),
                ],
                "MERGEABLE"
            ),
            PrGreen::Green
        );
        // Still running → pending.
        assert_eq!(
            classify_green(&[rollup(Some("IN_PROGRESS"), None, None)], "MERGEABLE"),
            PrGreen::Pending
        );
        assert_eq!(
            classify_green(&[rollup(None, None, Some("PENDING"))], "MERGEABLE"),
            PrGreen::Pending
        );
        // Failing conclusions/states → failing.
        assert_eq!(
            classify_green(
                &[rollup(Some("COMPLETED"), Some("FAILURE"), None)],
                "MERGEABLE"
            ),
            PrGreen::Failing
        );
        assert_eq!(
            classify_green(&[rollup(None, None, Some("ERROR"))], "MERGEABLE"),
            PrGreen::Failing
        );
        // Unknown shape → fail closed, even when mergeable.
        assert_eq!(
            classify_green(&[rollup(None, None, None)], "MERGEABLE"),
            PrGreen::Failing
        );
        // Mergeability unresolved or conflicting → not green.
        assert_eq!(classify_green(&[], "UNKNOWN"), PrGreen::Pending);
        assert_eq!(classify_green(&[], "CONFLICTING"), PrGreen::Pending);
        // No checks at all, mergeable → green (vacuous satisfaction).
        assert_eq!(classify_green(&[], "MERGEABLE"), PrGreen::Green);
    }

    // --- stage derivation from event sequences ---

    #[test]
    fn dispatched_then_pr_open_then_done() {
        assert_eq!(
            derive_stage(&facts(vec![ev("r1", "issue_dispatched")]), &no_live()),
            Stage::Dispatched
        );
        let mut f = facts(vec![ev("r1", "issue_dispatched"), ev("r1", "pr_opened")]);
        assert_eq!(derive_stage(&f, &no_live()), Stage::PrOpen);
        f.pr_green = Some(PrGreen::Pending);
        assert_eq!(derive_stage(&f, &no_live()), Stage::ChecksPending);
        f.pr_green = Some(PrGreen::Failing);
        assert_eq!(derive_stage(&f, &no_live()), Stage::ChecksFailing);
        f.pr_green = Some(PrGreen::Green);
        assert_eq!(derive_stage(&f, &no_live()), Stage::Mergeable);
        f.events.push(ev("r1", "merge_verified"));
        f.events.push(ev("r1", "issue_closed"));
        assert_eq!(derive_stage(&f, &no_live()), Stage::Done);
    }

    #[test]
    fn latest_run_stopped_wins_with_verbatim_reason() {
        let events = vec![
            ev("r1", "issue_dispatched"),
            ev("r1", "pr_opened"),
            ev_data(
                "r1",
                "run_stopped",
                serde_json::json!({"reason": "PR not green"}),
            ),
        ];
        assert_eq!(
            derive_stage(&facts(events), &no_live()),
            Stage::Stopped("PR not green".to_string())
        );
    }

    #[test]
    fn stop_in_an_earlier_run_does_not_mask_a_newer_run() {
        let events = vec![
            ev("r1", "issue_dispatched"),
            ev_data(
                "r1",
                "run_stopped",
                serde_json::json!({"reason": "review blocking"}),
            ),
            ev("r2", "run_started"),
            ev("r2", "issue_dispatched"),
        ];
        // r2 has no terminal event; without correlatable dispatch data no
        // died verdict either, so dispatch is the freshest known state.
        assert_eq!(derive_stage(&facts(events), &no_live()), Stage::Dispatched);
    }

    #[test]
    fn review_verdict_drives_review_stages() {
        let blocking = facts(vec![
            ev("r1", "pr_opened"),
            ev_data(
                "r1",
                "review_verdict",
                serde_json::json!({"verdict": "blocking"}),
            ),
        ]);
        assert_eq!(derive_stage(&blocking, &no_live()), Stage::ReviewBlocking);

        let mut clean = facts(vec![
            ev("r1", "pr_opened"),
            ev_data(
                "r1",
                "review_verdict",
                serde_json::json!({"verdict": "clean"}),
            ),
        ]);
        // No PR data to classify: the clean verdict is the freshest state.
        assert_eq!(derive_stage(&clean, &no_live()), Stage::ReviewClean);
        clean.pr_green = Some(PrGreen::Green);
        assert_eq!(derive_stage(&clean, &no_live()), Stage::AwaitingMerge);
        clean.pr_green = Some(PrGreen::Pending);
        assert_eq!(derive_stage(&clean, &no_live()), Stage::ChecksPending);
        // Latest verdict wins over an older blocking one.
        let mut events = vec![
            ev_data(
                "r1",
                "review_verdict",
                serde_json::json!({"verdict": "blocking"}),
            ),
            ev_data(
                "r1",
                "review_verdict",
                serde_json::json!({"verdict": "clean"}),
            ),
        ];
        let f = IssueFacts {
            labels: vec![],
            events: events.clone(),
            pr_green: Some(PrGreen::Green),
        };
        assert_eq!(derive_stage(&f, &no_live()), Stage::AwaitingMerge);
        events.push(ev("r1", "merge_verified"));
        let f = IssueFacts {
            labels: vec![],
            events,
            pr_green: Some(PrGreen::Green),
        };
        assert_eq!(derive_stage(&f, &no_live()), Stage::AwaitingMerge);
    }

    #[test]
    fn rework_newer_than_verdict_is_rework_in_progress() {
        let events = vec![
            ev("r1", "pr_opened"),
            ev_data(
                "r1",
                "review_verdict",
                serde_json::json!({"verdict": "blocking"}),
            ),
            ev("r1", "rework_dispatched"),
        ];
        assert_eq!(
            derive_stage(&facts(events), &no_live()),
            Stage::ReworkInProgress
        );
        // A verdict newer than the rework supersedes it again.
        let events = vec![
            ev("r1", "rework_dispatched"),
            ev_data(
                "r1",
                "review_verdict",
                serde_json::json!({"verdict": "blocking"}),
            ),
        ];
        assert_eq!(
            derive_stage(&facts(events), &no_live()),
            Stage::ReviewBlocking
        );
    }

    #[test]
    fn died_run_is_distinct_from_live_and_stopped() {
        let dispatch =
            serde_json::json!({"workspace_id": "w1", "pane_id": "w1:p1", "worktree": "/wt/7"});
        let events = vec![ev_data("r1", "issue_dispatched", dispatch)];
        // Supervising agent still live: normal in-flight stage.
        let live = LiveSet::new(
            vec![("w1".to_string(), "w1:p1".to_string())],
            vec![PathBuf::from("/wt/7")],
        );
        assert_eq!(
            derive_stage(&facts(events.clone()), &live),
            Stage::Dispatched
        );
        // Agent gone, no terminal event: visibly unknown.
        assert_eq!(derive_stage(&facts(events), &no_live()), Stage::Died);
    }

    #[test]
    fn died_detection_uses_worktree_when_no_pane_ids() {
        let events = vec![ev_data(
            "r1",
            "issue_dispatched",
            serde_json::json!({"worktree": "/wt/7"}),
        )];
        let live = LiveSet::new(vec![], vec![PathBuf::from("/wt/7")]);
        assert_eq!(
            derive_stage(&facts(events.clone()), &live),
            Stage::Dispatched
        );
        assert_eq!(derive_stage(&facts(events), &no_live()), Stage::Died);
    }

    #[test]
    fn died_detection_needs_correlatable_dispatch_data() {
        // A run_started with no dispatch data says nothing about liveness.
        let events = vec![ev("r1", "run_started")];
        assert_eq!(derive_stage(&facts(events), &no_live()), Stage::None);
    }

    // --- inference without events ---

    #[test]
    fn inferred_stages_without_events() {
        let mut f = facts(vec![]);
        f.labels = vec!["ready-for-agent".to_string()];
        assert_eq!(derive_stage(&f, &no_live()), Stage::Queued);

        f.labels = vec!["needs-triage".to_string()];
        assert_eq!(
            derive_stage(&f, &no_live()),
            Stage::Label("needs-triage".to_string())
        );

        f.labels = vec![];
        assert_eq!(derive_stage(&f, &no_live()), Stage::None);

        // An open flock-branch PR with no events: PR state alone.
        f.pr_green = Some(PrGreen::Pending);
        assert_eq!(derive_stage(&f, &no_live()), Stage::ChecksPending);
        f.pr_green = Some(PrGreen::Green);
        assert_eq!(derive_stage(&f, &no_live()), Stage::Mergeable);
    }

    #[test]
    fn stage_labels_match_the_vocabulary() {
        assert_eq!(Stage::Queued.label(), "queued");
        assert_eq!(Stage::ReworkInProgress.label(), "rework in progress");
        assert_eq!(Stage::Died.label(), "unknown (run may have died)");
        assert_eq!(
            Stage::Stopped("review blocking".to_string()).label(),
            "stopped: review blocking"
        );
        assert_eq!(Stage::AwaitingMerge.label(), "awaiting merge");
    }
}
