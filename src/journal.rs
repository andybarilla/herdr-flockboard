//! The Flock event journal (`<repo>/.flock/events.jsonl`, schema v1 from
//! flock#45) and the per-issue workflow-stage derivation built on it. The
//! journal is advisory and written only by Flock supervisors, so everything
//! here is read-only and fail-tolerant: a missing file, a truncated final
//! line (crash mid-append), non-UTF-8 bytes in a corrupt tail, unknown event
//! names (the schema is additive-only), and events from other workflows all
//! degrade to fewer known facts, never to an error. Journals are append-only
//! and can grow unbounded over a long-lived repo's life, so the reader
//! retains and parses only the newest `MAX_JOURNAL_EVENTS` lines per file —
//! enough history that an in-flight issue's events survive (see the
//! constant), never so much that cached memory or per-poll parse work grows
//! with the file.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::github::CheckRollup;
use crate::state::LABEL_PRIORITY;

/// Newest journal lines read, parsed, and retained per repo. The journal is
/// append-only, so the newest events are the file's tail; capping the reader
/// here keeps every consumer's memory and per-poll parse work flat no matter
/// how large `.flock/events.jsonl` grows, instead of re-reading the whole
/// file into the slot cache every agent poll. The bound is deliberately far
/// above the activity feed's view cap (`feed::MAX_EVENTS_PER_REPO` = 500):
/// the same cache also backs per-issue stage derivation and the inbox, whose
/// facts (a parked PR's blocking verdict, a stopped run's reason) can be far
/// older than the feed's window. 10_000 lines covers hundreds of issue
/// cycles at a few dozen events each, while worst-case retained memory stays
/// at a few MB per repo. Residual edge, accepted and documented: an issue
/// whose *entire* journal history has scrolled past the tail (only possible
/// after 10_000 newer events in the same repo) loses its event-derived
/// facts and degrades to label/PR inference — fewer known facts, never an
/// error, same as every other journal degradation.
pub const MAX_JOURNAL_EVENTS: usize = 10_000;

/// One parsed journal line. Fields the schema marks optional stay optional;
/// anything unrecognized (new event names, new `data` shapes) is preserved
/// but simply never matches a derivation rule. Ordering is by file position
/// (the parser preserves line order): the journal is append-only, so
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
    /// The event's `ts`, kept verbatim (never used for ordering — file
    /// position is time order). Consumers parse it leniently; a malformed
    /// timestamp degrades only their age display.
    pub ts: Option<String>,
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
    #[serde(default)]
    ts: Option<String>,
    // `v` is tool-generated metadata and never read; the reader tolerates
    // any schema version.
}

impl Event {
    pub(crate) fn is(&self, name: &str) -> bool {
        self.event == name
    }

    /// `data.verdict` from a `review_verdict` event ("clean"|"blocking"),
    /// verbatim; anything else is tolerated as unknown.
    pub(crate) fn verdict(&self) -> Option<&str> {
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

/// Parses journal lines tolerantly: blank lines and unparseable lines
/// (including a truncated final line from a crash mid-append) are skipped,
/// and unknown event names parse fine — they just match no derivation rule.
fn parse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<Event> {
    lines
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
                ts: r.ts,
            })
        })
        .collect()
}

/// Parses journal text tolerantly (see `parse_lines`). Test-only: the
/// production reader goes through `read_file`, which skips the over-bound
/// head before parsing.
#[cfg(test)]
pub fn parse_events(text: &str) -> Vec<Event> {
    parse_lines(text.lines())
}

/// Reads one candidate journal file. `None` when the file is missing or
/// unreadable — an absent journal is a normal state (repo not under Flock
/// management), not an error. Bytes are decoded lossily: a corrupt tail
/// with invalid UTF-8 ruins only the line it lands on (the replacement
/// character makes it unparseable), so every valid prior line survives.
/// Only the newest `MAX_JOURNAL_EVENTS` lines are parsed and returned: the
/// head is skipped as whole lines without deserializing, so both retained
/// memory and per-poll parse work stay bounded as the file grows. (Corrupt
/// or blank lines inside the tail count toward the line cap, so the event
/// count can land slightly below it — harmless.)
fn read_file(path: &Path) -> Option<Vec<Event>> {
    let bytes = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let skip = text.lines().count().saturating_sub(MAX_JOURNAL_EVENTS);
    Some(parse_lines(text.lines().skip(skip)))
}

/// Reads the journal for a repo given its live local checkouts. Several
/// cwds can map to one repo (the operator's main checkout plus agent
/// worktrees), but supervisors only ever write the journal in the checkout
/// they run in, so the first cwd (in sorted order, for determinism) that
/// actually has a readable journal wins. No journal anywhere yields no
/// events, and stages fall back to label/PR inference. The result is the
/// bounded newest tail (`MAX_JOURNAL_EVENTS`), never the whole file.
pub fn read_repo_events(cwds: &BTreeSet<PathBuf>) -> Vec<Event> {
    cwds.iter()
        .find_map(|cwd| read_file(&cwd.join(".flock").join("events.jsonl")))
        .unwrap_or_default()
}

/// The green classifier for a PR's checks, mirroring the verified
/// check-wait semantics in `docs/flock/project.md`: SKIPPED/NEUTRAL
/// conclusions count as satisfied, and anything unrecognized — a rollup
/// entry with neither a check-run nor a status-context shape, an unknown
/// check-run conclusion, an unknown status-context state — is fail-closed:
/// it classifies as failing, never as merely pending. `mergeable:
/// CONFLICTING` is the config's hard-stop `conflict` classification, so
/// with only three states here it renders as failing/attention rather than
/// pending; `UNKNOWN` mergeability stays pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrGreen {
    /// `mergeable: MERGEABLE` and every rollup entry satisfied.
    Green,
    /// Nothing failing, but something still running or mergeability
    /// unresolved (`UNKNOWN`).
    Pending,
    /// Any failing/errored entry, an entry shape we do not recognize, or
    /// `mergeable: CONFLICTING` (the config's hard-stop `conflict`).
    Failing,
}

/// Where one rollup entry lands. `Bad` is deliberately broad: the
/// classifier fails closed, so every shape or value not explicitly known
/// to be satisfied or still-running counts as failing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckState {
    Ok,
    Pending,
    Bad,
}

/// A rollup entry is check-run-shaped when it carries `status` or
/// `conclusion` (gh emits check runs with both keys, conclusion possibly
/// null while running), status-context-shaped when it carries `state`.
fn check_state(r: &CheckRollup) -> CheckState {
    if r.status.is_some() || r.conclusion.is_some() {
        // A set conclusion means the run completed: only the
        // known-satisfying conclusions pass; known failures and any
        // unrecognized conclusion (additive schema change) fail closed.
        if let Some(conclusion) = r.conclusion.as_deref() {
            return match conclusion {
                "SUCCESS" | "SKIPPED" | "NEUTRAL" => CheckState::Ok,
                _ => CheckState::Bad,
            };
        }
        match r.status.as_deref() {
            // Known still-running check-run statuses.
            Some("QUEUED") | Some("IN_PROGRESS") | Some("WAITING") | Some("PENDING")
            | Some("REQUESTED") => CheckState::Pending,
            // COMPLETED with no conclusion, or an unrecognized status:
            // fail closed.
            _ => CheckState::Bad,
        }
    } else if let Some(state) = r.state.as_deref() {
        match state {
            "SUCCESS" => CheckState::Ok,
            // Known still-running status-context states.
            "PENDING" | "EXPECTED" => CheckState::Pending,
            // Known failures and anything unrecognized fail closed.
            _ => CheckState::Bad,
        }
    } else {
        CheckState::Bad // unrecognized shape: fail closed
    }
}

pub fn classify_green(checks: &[CheckRollup], mergeable: &str) -> PrGreen {
    // A conflict is a hard stop in the project config (`conflict`), not
    // something merely pending; the three-state UI renders it as the
    // failing/attention state.
    if mergeable == "CONFLICTING" {
        return PrGreen::Failing;
    }
    if checks.iter().any(|r| check_state(r) == CheckState::Bad) {
        return PrGreen::Failing;
    }
    // With no checks at all the "all satisfied" half is vacuous, matching
    // the project-config jq: mergeability alone decides.
    if mergeable == "MERGEABLE" && checks.iter().all(|r| check_state(r) == CheckState::Ok) {
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
    /// `merge_verified` seen but the issue not yet closed: the post-merge,
    /// pre-close window. Small spec-consistent extension of the issue #2
    /// vocabulary — `awaiting merge` would be false (the merge already
    /// happened) and `done` is reserved for `issue_closed`.
    Merged,
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
            Self::Merged => "merged".to_string(),
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
            Self::ReviewBlocking | Self::ReviewClean | Self::AwaitingMerge | Self::Merged => 2,
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
/// stopped) first, then a verified merge on the latest run, then the died
/// heuristic, then the review/rework ladder, then PR/check progress, then
/// bare dispatch, and finally — with no usable events at all — label/PR
/// inference.
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
    // A verified merge on the latest run is an explicit terminal-adjacent
    // fact: it outranks the died heuristic (a merged run's agent exiting
    // is normal, not a death) and needs no clean review verdict — the
    // merge event speaks for itself. Only `issue_closed` and a latest-run
    // `run_stopped` (above) outrank it.
    if run.iter().any(|e| e.is("merge_verified")) {
        return Stage::Merged;
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
                // Review clean (and no `merge_verified` — that is
                // recognized earlier, before the died heuristic): green
                // checks leave only the operator's merge/close step;
                // ungreen checks still gate it. With no PR data to
                // classify, the clean verdict itself is the freshest
                // known state.
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
        assert_eq!(events[1].ts.as_deref(), Some("2026-09-24T19:05:01.524Z"));
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
    fn non_utf8_corrupt_tail_preserves_prior_valid_lines() {
        let dir = tempfile::tempdir().unwrap();
        let flock = dir.path().join(".flock");
        std::fs::create_dir_all(&flock).unwrap();
        let mut bytes =
            b"{\"run_id\":\"r1\",\"workflow\":\"w\",\"event\":\"pr_opened\",\"repo\":\"o/r\",\"issue\":1}\n"
                .to_vec();
        bytes.extend_from_slice(b"{\"run_id\":\"r1\",\"event\":\"iss");
        bytes.extend_from_slice(&[0xff, 0xfe]); // invalid UTF-8 in the tail
        std::fs::write(flock.join("events.jsonl"), bytes).unwrap();
        let cwds = BTreeSet::from([dir.path().to_path_buf()]);
        let events = read_repo_events(&cwds);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "pr_opened");
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
    fn reader_retains_only_the_bounded_newest_tail() {
        // A long-lived repo's journal grows past the reader bound: the
        // reader parses and returns only the newest MAX_JOURNAL_EVENTS
        // lines, dropping the file head, so retained memory and per-poll
        // parse work stay flat no matter how large the file gets.
        let dir = tempfile::tempdir().unwrap();
        let flock = dir.path().join(".flock");
        std::fs::create_dir_all(&flock).unwrap();
        let total = MAX_JOURNAL_EVENTS + 100;
        let mut text = String::new();
        for i in 0..total {
            let event = if i == total - 1 {
                "issue_closed"
            } else {
                "run_started"
            };
            text.push_str(&format!(
                "{{\"run_id\":\"r{i}\",\"workflow\":\"w\",\"event\":\"{event}\",\"repo\":\"o/r\",\"issue\":1}}\n"
            ));
        }
        std::fs::write(flock.join("events.jsonl"), text).unwrap();
        let cwds = BTreeSet::from([dir.path().to_path_buf()]);
        let events = read_repo_events(&cwds);
        assert_eq!(events.len(), MAX_JOURNAL_EVENTS);
        // The dropped events are the oldest (the file head); the newest
        // line of the journal always survives.
        assert_eq!(events.first().unwrap().run_id, "r100");
        assert_eq!(events.last().unwrap().event, "issue_closed");
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
        // Unknown completed conclusions and status-context states fail
        // closed too — an additive schema change must not understate an
        // unsafe PR as merely pending.
        assert_eq!(
            classify_green(
                &[rollup(Some("COMPLETED"), Some("STALE"), None)],
                "MERGEABLE"
            ),
            PrGreen::Failing
        );
        assert_eq!(
            classify_green(&[rollup(None, None, Some("FUNKY"))], "MERGEABLE"),
            PrGreen::Failing
        );
        assert_eq!(
            classify_green(&[rollup(Some("COMPLETED"), None, None)], "MERGEABLE"),
            PrGreen::Failing
        );
        // Known still-running states stay pending, not failing.
        assert_eq!(
            classify_green(&[rollup(Some("QUEUED"), None, None)], "MERGEABLE"),
            PrGreen::Pending
        );
        assert_eq!(
            classify_green(&[rollup(None, None, Some("EXPECTED"))], "MERGEABLE"),
            PrGreen::Pending
        );
        // Mergeability unresolved stays pending; a conflict is the
        // config's hard-stop `conflict` classification, rendered here as
        // the failing/attention state.
        assert_eq!(classify_green(&[], "UNKNOWN"), PrGreen::Pending);
        assert_eq!(classify_green(&[], "CONFLICTING"), PrGreen::Failing);
        // No checks at all, mergeable → green (vacuous satisfaction).
        assert_eq!(classify_green(&[], "MERGEABLE"), PrGreen::Green);
    }

    #[test]
    fn conflicting_mergeable_is_failing_not_pending() {
        // Checks otherwise satisfied, but the PR conflicts: the project
        // config classifies this as the hard stop `conflict`, never as
        // `pending`, so the three-state UI must show the
        // failing/attention state rather than "checks pending".
        assert_eq!(
            classify_green(
                &[rollup(Some("COMPLETED"), Some("SUCCESS"), None)],
                "CONFLICTING"
            ),
            PrGreen::Failing
        );
        assert_eq!(classify_green(&[], "CONFLICTING"), PrGreen::Failing);
        // UNKNOWN mergeability stays pending, per the config.
        assert_eq!(
            classify_green(
                &[rollup(Some("COMPLETED"), Some("SUCCESS"), None)],
                "UNKNOWN"
            ),
            PrGreen::Pending
        );
        assert_eq!(classify_green(&[], "UNKNOWN"), PrGreen::Pending);
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
        assert_eq!(derive_stage(&f, &no_live()), Stage::Merged);
    }

    #[test]
    fn merge_verified_is_post_merge_not_awaiting_merge() {
        // Clean review + green checks, merge not yet verified: the
        // operator's merge step is all that remains.
        let mut events = vec![
            ev("r1", "pr_opened"),
            ev_data(
                "r1",
                "review_verdict",
                serde_json::json!({"verdict": "clean"}),
            ),
        ];
        let mut f = IssueFacts {
            labels: vec![],
            events: events.clone(),
            pr_green: Some(PrGreen::Green),
        };
        assert_eq!(derive_stage(&f, &no_live()), Stage::AwaitingMerge);
        // After the merge is verified the dashboard must not claim it is
        // still awaiting merge; the post-merge, pre-close window shows
        // `merged` until `issue_closed` makes it `done`.
        events.push(ev("r1", "merge_verified"));
        f.events = events.clone();
        assert_eq!(derive_stage(&f, &no_live()), Stage::Merged);
        events.push(ev("r1", "issue_closed"));
        f.events = events;
        assert_eq!(derive_stage(&f, &no_live()), Stage::Done);
    }

    #[test]
    fn merge_verified_outranks_died_heuristic_regardless_of_verdict() {
        // Real dispatch data naming a pane/worktree, the supervising agent
        // gone from herdr, merge verified, issue not yet closed: the
        // explicit merge event must win over the died heuristic, and must
        // not require a clean review verdict.
        let dispatch =
            serde_json::json!({"workspace_id": "w1", "pane_id": "w1:p1", "worktree": "/wt/7"});
        let verdicts = [
            None,
            Some(serde_json::json!({"verdict": "clean"})),
            Some(serde_json::json!({"verdict": "blocking"})),
        ];
        for verdict in verdicts {
            let mut events = vec![
                ev_data("r1", "issue_dispatched", dispatch.clone()),
                ev("r1", "pr_opened"),
            ];
            if let Some(v) = verdict {
                events.push(ev_data("r1", "review_verdict", v));
            }
            events.push(ev("r1", "merge_verified"));
            assert_eq!(derive_stage(&facts(events), &no_live()), Stage::Merged);
        }
    }

    #[test]
    fn closed_and_stopped_still_outrank_merge_verified() {
        let dispatch = serde_json::json!({"pane_id": "w1:p1"});
        let mut events = vec![
            ev_data("r1", "issue_dispatched", dispatch),
            ev("r1", "merge_verified"),
            ev("r1", "issue_closed"),
        ];
        assert_eq!(
            derive_stage(&facts(events.clone()), &no_live()),
            Stage::Done
        );
        events.pop();
        events.push(ev_data(
            "r1",
            "run_stopped",
            serde_json::json!({"reason": "PR not green"}),
        ));
        assert_eq!(
            derive_stage(&facts(events), &no_live()),
            Stage::Stopped("PR not green".to_string())
        );
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
        assert_eq!(Stage::Merged.label(), "merged");
    }
}
