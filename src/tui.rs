//! The read-only dashboard TUI. Three threads: an agent worker polls
//! `herdr` on the agent cadence and pushes whole `Dashboard` snapshots over
//! a channel; a GitHub worker independently refreshes due repo slots in the
//! shared cache (bounded per-command timeouts); the UI thread only handles
//! keys and draws. Neither worker blocks the other, so a slow or hung `gh`
//! shows up as older slot data, never as a stalled board.

use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::feed::{self, FeedEvent};
use crate::git_org::{Remote, RemoteCache};
use crate::github::{GhCli, IssueTracker};
use crate::herd::{HerdCli, HerdControl};
use crate::inbox::{format_age, InboxItem, InboxKind};
use crate::journal::{self, Stage};
use crate::state::{self, AgentStatus, Checks, Dashboard, RepoView, Review, LABEL_PRIORITY};

/// How long the UI waits for a key before redrawing; also the cadence that
/// keeps "updated Ns ago" moving between agent polls.
const UI_TICK: Duration = Duration::from_millis(250);

/// PRs shown per repo before collapsing the tail into "+N more".
const MAX_PRS_SHOWN: usize = 8;

/// Issue rows shown per repo before collapsing the tail into "+N more".
const MAX_ISSUES_SHOWN: usize = 8;

/// Inbox items shown before collapsing the tail into "+N more". The inbox
/// is the top-priority section, so it gets more rows than a per-repo
/// table, but a long needs-triage backlog still must not flood the board.
const MAX_INBOX_SHOWN: usize = 12;

/// Activity-feed events shown before collapsing the tail into "+N more".
/// The feed is a rolling stream, so a busy session would otherwise push
/// the repo sections off the screen; the full bounded window per repo
/// still lives in `board.feed`.
const MAX_FEED_SHOWN: usize = 10;

/// How often the GitHub worker checks the slot cache for repos whose TTL
/// expired; short so a newly seen repo is fetched promptly.
const GH_WORKER_TICK: Duration = Duration::from_secs(1);

/// Slot cache shared between the agent worker (syncs it on each successful
/// herdr poll, invalidates it on failure, reads it for snapshots) and the
/// GitHub worker (writes fetch results back by ticket).
type SharedSlots = Arc<Mutex<state::SlotCache>>;

fn lock(slots: &SharedSlots) -> MutexGuard<'_, state::SlotCache> {
    // A poisoned lock means the other worker panicked mid-update; recover
    // the data rather than freeze the board.
    slots.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn run() -> Result<()> {
    let slots: SharedSlots = Arc::new(Mutex::new(state::SlotCache::default()));
    let (tx, rx) = mpsc::channel::<Dashboard>();
    std::thread::spawn({
        let slots = Arc::clone(&slots);
        move || agent_loop(tx, slots)
    });
    std::thread::spawn(move || github_loop(slots));

    let mut guard = TerminalGuard::new()?;
    let mut board: Option<Dashboard> = None;
    // Activity-feed repo filter: `None` is "all". Session state of the
    // pane, kept across snapshots — the board itself is rebuilt from the
    // journals on every poll, so the filter lives here.
    let mut feed_filter: Option<String> = None;
    loop {
        // Keep only the freshest snapshot; the worker outpaces the UI when
        // GitHub is slow.
        while let Ok(d) = rx.try_recv() {
            board = Some(d);
        }
        guard.terminal.draw(|frame| {
            draw(frame, board.as_ref(), feed_filter.as_deref());
        })?;
        if event::poll(UI_TICK)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    // Cycle the feed filter: all → each repo on the board
                    // → all.
                    KeyCode::Char('f') => {
                        if let Some(b) = &board {
                            let mut repos: Vec<String> = b
                                .groups
                                .iter()
                                .flat_map(|g| g.repos.iter().map(|r| r.key.clone()))
                                .collect();
                            repos.sort();
                            repos.dedup();
                            feed_filter = next_filter(&feed_filter, &repos);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Polls `herdr` on the agent-poll cadence and publishes snapshots built
/// from whatever the slot cache currently holds. Never calls `gh`, so the
/// initial agent (or zero-agent) render is not gated on GitHub at all; the
/// per-repo Flock journals it re-reads after each sync are small local
/// files with a tolerant parser. A failed poll invalidates the cache: without a live agent set there is no
/// current repo set, so nothing stale stays eligible for GitHub work (and
/// in-flight completions find no slot to land in); the error itself still
/// renders, and the next successful poll repopulates from scratch. A repo
/// whose agents all disappeared is retained for `state::GONE_GRACE` so a
/// died-mid-run issue stays visibly distinct before the row drops.
fn agent_loop(tx: mpsc::Sender<Dashboard>, slots: SharedSlots) {
    let herd = HerdCli::default();
    let mut remotes = RemoteCache::default();
    loop {
        let agents = herd
            .list_agents()
            .map_err(|e| e.to_string())
            .map(state::agent_rows);
        let dash = {
            let mut slots = lock(&slots);
            match &agents {
                Ok(rows) => {
                    slots.sync(rows, &mut |cwd| remotes.get(cwd), Instant::now());
                    // Journals are tiny and the reader is tolerant, so a
                    // fresh read each poll keeps issue stages and the
                    // activity feed current.
                    slots.refresh_journals(&journal::read_repo_events);
                }
                Err(_) => slots.invalidate(),
            }
            state::dashboard(agents, &slots)
        };
        if tx.send(dash).is_err() {
            return; // UI exited
        }
        std::thread::sleep(state::AGENT_POLL);
    }
}

/// Refreshes due GitHub slots on its own cadence, independent of the agent
/// poll. Snapshots the due list as tickets under the lock, fetches outside
/// it (each `gh` call bounded by `GH_TIMEOUT`), then writes results back —
/// errors included, so the board renders what the last fetch actually said.
/// A ticket whose slot was pruned, re-created, or invalidated mid-fetch is
/// stale: `record_fetch` rejects it by generation and repo identity.
fn github_loop(slots: SharedSlots) {
    let gh = GhCli;
    loop {
        let due = lock(&slots).due_github_repos(Instant::now(), state::GH_TTL);
        for ticket in due {
            let issues = gh.open_issues(&ticket.repo).map_err(|e| e.to_string());
            let prs = gh.open_prs(&ticket.repo).map_err(|e| e.to_string());
            lock(&slots).record_fetch(&ticket, issues, prs, Instant::now());
        }
        std::thread::sleep(GH_WORKER_TICK);
    }
}

fn draw(frame: &mut Frame, board: Option<&Dashboard>, feed_filter: Option<&str>) {
    let lines = match board {
        None => vec![Line::from(Span::styled(
            "connecting to herdr…",
            Style::default().fg(Color::DarkGray),
        ))],
        Some(b) => render(b, feed_filter),
    };
    let title = format!(
        " Flockboard — q quit · f filter feed · agents every {}s · github every {}s ",
        state::AGENT_POLL.as_secs(),
        state::GH_TTL.as_secs(),
    );
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        frame.area(),
    );
}

fn render(board: &Dashboard, feed_filter: Option<&str>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    render_agents(board, &mut lines);
    render_inbox(board, &mut lines);
    render_feed(board, &mut lines, feed_filter);
    for group in &board.groups {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("── {} ──", group.org),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for repo in &group.repos {
            render_repo(repo, &mut lines);
        }
    }
    lines
}

fn render_agents(board: &Dashboard, lines: &mut Vec<Line<'static>>) {
    match &board.agents {
        Err(e) => {
            lines.push(Line::from(Span::styled(
                format!("herdr unreachable: {e}"),
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::from(Span::styled(
                "start the herdr server (or set HERDR_BIN_PATH); retrying automatically",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Ok(rows) if rows.is_empty() => {
            lines.push(Line::from(Span::styled(
                "no agents in this herdr session",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Ok(rows) => {
            lines.push(Line::from(Span::styled(
                format!("AGENTS ({})", rows.len()),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for row in rows {
                let focused = if row.focused { " ← focused" } else { "" };
                lines.push(Line::from(vec![
                    status_span(row.status),
                    Span::raw(format!(" {:<5}", row.name)),
                    Span::styled(
                        format!(" {:<30}", truncate(&row.repo_dir, 30)),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(
                        format!(" {}/{}", row.workspace, row.pane),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!(" {}{focused}", row.title),
                        Style::default().fg(Color::Gray),
                    ),
                ]));
            }
        }
    }
}

/// The cross-repo "waiting on you" inbox: blocking reviews, parked PRs,
/// and stopped runs first, then merge-ready PRs, then informational
/// tracker items — the one section that answers "what needs me right
/// now". Placed between the agent board and the per-repo groups so the
/// highest-priority information is on top. A herdr error renders in the
/// agent section and leaves the inbox empty by construction; claiming
/// "nothing waiting on you" then would be false, so the section is
/// skipped entirely on error.
fn render_inbox(board: &Dashboard, lines: &mut Vec<Line<'static>>) {
    if board.agents.is_err() {
        return;
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("── WAITING ON YOU ({}) ──", board.inbox.len()),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    if board.inbox.is_empty() {
        lines.push(Line::from(Span::styled(
            "  nothing waiting on you",
            Style::default().fg(Color::DarkGray),
        )));
        return;
    }
    for item in board.inbox.iter().take(MAX_INBOX_SHOWN) {
        lines.push(inbox_line(item));
    }
    if board.inbox.len() > MAX_INBOX_SHOWN {
        lines.push(Line::from(Span::styled(
            format!("  +{} more", board.inbox.len() - MAX_INBOX_SHOWN),
            Style::default().fg(Color::DarkGray),
        )));
    }
}

fn inbox_line(item: &InboxItem) -> Line<'static> {
    // Group colors mirror the stage palette: attention red, forward-ready
    // green, informational yellow.
    let (symbol, color) = match item.kind.group() {
        0 => ("!", Color::Red),
        1 => ("✓", Color::Green),
        _ => ("?", Color::Yellow),
    };
    let age = item
        .since
        .map(|t| format!("{} ago", format_age(t.elapsed().unwrap_or(Duration::ZERO))));
    let reason = item.reason();
    let reason_color = match item.kind {
        InboxKind::TrackerInput => Color::Yellow,
        _ => color,
    };
    Line::from(vec![
        Span::styled(format!(" {symbol}"), Style::default().fg(color)),
        Span::styled(
            format!(" {:<24}", truncate(&item.repo, 24)),
            Style::default().fg(Color::White),
        ),
        Span::raw(format!(" {:<8}", item.target.label())),
        Span::styled(
            format!(" {:<36}", truncate(&reason, 36)),
            Style::default().fg(reason_color),
        ),
        Span::styled(
            format!(" {:<9}", age.unwrap_or_default()),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(" {}", truncate(&item.title, 50)),
            Style::default().fg(Color::Gray),
        ),
    ])
}

/// The cross-repo activity feed: workflow events from every discovered
/// repo's Flock journal, newest first by event timestamp. Placed between
/// the inbox and the per-repo groups: below the "what needs me" summary,
/// above the detail. The `f` key cycles a per-repo filter (including
/// "all") that persists for the pane session. Skipped entirely on a
/// herdr error, like the inbox: with no live repo set there is nothing
/// to feed from, and claiming "no events yet" would be false.
fn render_feed(board: &Dashboard, lines: &mut Vec<Line<'static>>, filter: Option<&str>) {
    if board.agents.is_err() {
        return;
    }
    let shown = feed::filter_repo(&board.feed, filter);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("── ACTIVITY ({}) ──", filter.unwrap_or("all repos")),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    if shown.is_empty() {
        let msg = match filter {
            Some(repo) => format!("  no journal events for {repo}"),
            None => "  no journal events yet".to_string(),
        };
        lines.push(Line::from(Span::styled(
            msg,
            Style::default().fg(Color::DarkGray),
        )));
        return;
    }
    for event in shown.iter().take(MAX_FEED_SHOWN) {
        lines.push(feed_line(event));
    }
    if shown.len() > MAX_FEED_SHOWN {
        lines.push(Line::from(Span::styled(
            format!("  +{} more", shown.len() - MAX_FEED_SHOWN),
            Style::default().fg(Color::DarkGray),
        )));
    }
}

fn feed_line(event: &FeedEvent) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {:<15}", format_ts(event.ts_text.as_deref())),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(" {:<24}", truncate(&event.repo, 24)),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!(" {:<16}", truncate(&event.workflow, 16)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(" {:<20}", truncate(&event.event, 20)),
            Style::default().fg(event_color(event)),
        ),
        Span::raw(format!(" {:<12}", truncate(&event.target(), 12))),
        Span::styled(
            format!(" {}", truncate(&event.detail, 40)),
            Style::default().fg(Color::Gray),
        ),
    ])
}

/// Feed event colors mirror the stage palette: in-flight dispatches and
/// runs cyan, forward progress green, stops and blocking/failed outcomes
/// red, unrecognized verdicts or gate shapes yellow, unknown event types
/// gray.
fn event_color(event: &FeedEvent) -> Color {
    match event.event.as_str() {
        "run_started" | "issue_dispatched" | "rework_dispatched" => Color::Cyan,
        "pr_opened" | "merge_verified" | "issue_closed" => Color::Green,
        "run_stopped" => Color::Red,
        "review_verdict" => match event.detail.as_str() {
            "clean" => Color::Green,
            "blocking" => Color::Red,
            _ => Color::Yellow,
        },
        "gate_result" => match event.detail.as_str() {
            "passed" => Color::Green,
            "failed" => Color::Red,
            _ => Color::Yellow,
        },
        _ => Color::Gray,
    }
}

/// Compact timestamp for the feed: "MM-DD HH:MM:SS" taken from the
/// journal's RFC3339 `ts`. A malformed `ts` renders verbatim (truncated)
/// and a missing one as "—", never as a wrong time.
fn format_ts(raw: Option<&str>) -> String {
    let Some(raw) = raw else {
        return "—".to_string();
    };
    if let Some((date, time)) = raw.split_once('T') {
        // Slice only after validating an ASCII RFC3339-like shape
        // (YYYY-MM-DD / HH:MM:SS), and then only via `.get()` ranges,
        // so a malformed multibyte `ts` degrades to the verbatim render
        // instead of panicking the feed on a non-char-boundary slice.
        let date_ok = date.len() == 10
            && date.bytes().enumerate().all(|(i, b)| match i {
                4 | 7 => b == b'-',
                _ => b.is_ascii_digit(),
            });
        let time_ok = time.len() >= 8
            && time.bytes().take(8).enumerate().all(|(i, b)| match i {
                2 | 5 => b == b':',
                _ => b.is_ascii_digit(),
            });
        if date_ok && time_ok {
            if let (Some(d), Some(t)) = (date.get(5..), time.get(..8)) {
                return format!("{d} {t}");
            }
        }
    }
    truncate(raw, 15)
}

/// Cycles the activity-feed repo filter: all → first repo → … → last
/// repo → all. A filter naming a repo that has since left the board
/// resets to all on the next cycle.
fn next_filter(current: &Option<String>, repos: &[String]) -> Option<String> {
    match current {
        None => repos.first().cloned(),
        Some(r) => match repos.iter().position(|x| x == r) {
            Some(i) => repos.get(i + 1).cloned(),
            None => None,
        },
    }
}

fn render_repo(repo: &RepoView, lines: &mut Vec<Line<'static>>) {
    let mut header = vec![
        Span::styled(
            format!("  {}", repo.key),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " ({} agent{})",
                repo.agent_count,
                if repo.agent_count == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    // Repo-level freshness, shown exactly once on the header whenever a
    // fetch has completed — in success (empty or not) and error states
    // alike — rather than duplicated on individual issue/PR rows.
    if repo.fetched_at.is_some() {
        header.push(staleness_span(repo));
    }
    lines.push(Line::from(header));
    match &repo.remote {
        Remote::GitHub { .. } => {
            render_issues(repo, lines);
            render_prs(repo, lines);
        }
        Remote::NoOrigin => lines.push(skip_line("no git origin — issue/PR data unavailable")),
        Remote::NonGitHub { host } => lines.push(skip_line(&format!(
            "not a github.com remote ({host}) — skipped"
        ))),
        Remote::GitError(e) => lines.push(Line::from(Span::styled(
            format!("    git error: {e}"),
            Style::default().fg(Color::Red),
        ))),
    }
}

fn skip_line(msg: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("    {msg}"),
        Style::default().fg(Color::Yellow),
    ))
}

fn render_issues(repo: &RepoView, lines: &mut Vec<Line<'static>>) {
    let line = match &repo.issues {
        None => Line::from(Span::styled(
            "    issues: fetching…".to_string(),
            Style::default().fg(Color::DarkGray),
        )),
        Some(Err(e)) => Line::from(Span::styled(
            format!("    issues: gh error: {}", first_line(e)),
            Style::default().fg(Color::Red),
        )),
        Some(Ok(b)) if b.total == 0 => Line::from(Span::styled(
            "    issues: none open".to_string(),
            Style::default().fg(Color::DarkGray),
        )),
        Some(Ok(b)) => {
            let mut detail: Vec<String> = LABEL_PRIORITY
                .iter()
                .filter_map(|l| b.by_label.get(*l).map(|n| format!("{l} {n}")))
                .collect();
            if b.other > 0 {
                detail.push(format!("other {}", b.other));
            }
            let spans = vec![
                Span::raw(format!("    issues: {} open", b.total)),
                Span::styled(
                    format!(" — {}", detail.join(" · ")),
                    Style::default().fg(Color::Yellow),
                ),
            ];
            Line::from(spans)
        }
    };
    lines.push(line);
    // Per-issue rows carry the Flock workflow stage derived from the repo's
    // event journal, labels, and PR state. Rows follow the issue fetch
    // state: nothing renders until the first fetch, and a gh error shows
    // only on the summary line above.
    if let Some(Ok(rows)) = &repo.issue_rows {
        for row in rows.iter().take(MAX_ISSUES_SHOWN) {
            lines.push(Line::from(vec![
                Span::raw(format!("      #{:<5}", row.number)),
                stage_span(&row.stage),
                Span::styled(
                    format!(" {}", truncate(&row.title, 60)),
                    Style::default().fg(Color::Gray),
                ),
            ]));
        }
        if rows.len() > MAX_ISSUES_SHOWN {
            lines.push(Line::from(Span::styled(
                format!("      +{} more", rows.len() - MAX_ISSUES_SHOWN),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
}

fn render_prs(repo: &RepoView, lines: &mut Vec<Line<'static>>) {
    match &repo.prs {
        None => lines.push(Line::from(Span::styled(
            "    prs: fetching…".to_string(),
            Style::default().fg(Color::DarkGray),
        ))),
        Some(Err(e)) => lines.push(Line::from(Span::styled(
            format!("    prs: gh error: {}", first_line(e)),
            Style::default().fg(Color::Red),
        ))),
        Some(Ok(prs)) if prs.is_empty() => lines.push(Line::from(Span::styled(
            "    prs: none open".to_string(),
            Style::default().fg(Color::DarkGray),
        ))),
        Some(Ok(prs)) => {
            lines.push(Line::from(vec![Span::raw(format!(
                "    prs: {} open",
                prs.len()
            ))]));
            for pr in prs.iter().take(MAX_PRS_SHOWN) {
                let draft = if pr.draft { " (draft)" } else { "" };
                lines.push(Line::from(vec![
                    Span::raw(format!("      #{:<4}", pr.number)),
                    checks_span(pr.checks),
                    review_span(pr.review),
                    Span::styled(
                        format!(" {}{draft}", truncate(&pr.title, 70)),
                        Style::default().fg(Color::Gray),
                    ),
                ]));
            }
            if prs.len() > MAX_PRS_SHOWN {
                lines.push(Line::from(Span::styled(
                    format!("      +{} more", prs.len() - MAX_PRS_SHOWN),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
}

/// Stage colors: in-flight work cyan, waiting yellow, attention red,
/// forward-ready green, done blue, dormant gray. A died-mid-run issue is
/// magenta so it reads as neither live work nor a clean stop.
fn stage_span(stage: &Stage) -> Span<'static> {
    let color = match stage {
        Stage::ReworkInProgress | Stage::Dispatched => Color::Cyan,
        Stage::PrOpen | Stage::ChecksPending => Color::Yellow,
        Stage::ChecksFailing | Stage::ReviewBlocking | Stage::Stopped(_) => Color::Red,
        Stage::Died => Color::Magenta,
        Stage::Mergeable | Stage::ReviewClean | Stage::AwaitingMerge | Stage::Merged => {
            Color::Green
        }
        Stage::Done => Color::Blue,
        Stage::Queued | Stage::Label(_) | Stage::None => Color::DarkGray,
    };
    let label = truncate(&stage.label(), 40);
    Span::styled(format!(" {label:<40}"), Style::default().fg(color))
}

fn status_span(status: AgentStatus) -> Span<'static> {
    let (symbol, color) = match status {
        AgentStatus::Working => ("●", Color::Green),
        AgentStatus::Idle => ("○", Color::Yellow),
        AgentStatus::Blocked => ("▲", Color::Red),
        AgentStatus::Done => ("✔", Color::Blue),
        AgentStatus::Unknown => ("?", Color::DarkGray),
    };
    Span::styled(
        format!("{symbol} {:<8}", status.label()),
        Style::default().fg(color),
    )
}

fn checks_span(checks: Checks) -> Span<'static> {
    let (symbol, color) = match checks {
        Checks::Pass => ("✓", Color::Green),
        Checks::Fail => ("✗", Color::Red),
        Checks::Pending => ("…", Color::Yellow),
        Checks::None => ("·", Color::DarkGray),
    };
    Span::styled(format!(" {symbol}"), Style::default().fg(color))
}

fn review_span(review: Review) -> Span<'static> {
    let (symbol, color) = match review {
        Review::Approved => ("R✓", Color::Green),
        Review::ChangesRequested => ("R✗", Color::Red),
        Review::Required => ("R?", Color::Yellow),
        Review::None => ("  ", Color::DarkGray),
    };
    Span::styled(format!(" {symbol}"), Style::default().fg(color))
}

fn staleness_span(repo: &RepoView) -> Span<'static> {
    match repo.fetched_at {
        Some(at) => Span::styled(
            format!("  (github data {}s ago)", at.elapsed().as_secs()),
            Style::default().fg(Color::DarkGray),
        ),
        None => Span::raw(""),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Restores the terminal on drop, including on the `?` error path.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        // Every failure from here on must roll back raw mode (and the
        // alternate screen, harmlessly if it was never entered) so the
        // caller's terminal is never left in a broken state.
        let terminal = init_or_rollback(enter_terminal, || {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            let _ = disable_raw_mode();
        })?;
        Ok(Self { terminal })
    }
}

/// Alternate-screen entry plus terminal construction: everything that runs
/// after raw mode is on and can still fail.
fn enter_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

/// Runs `setup`, and on failure runs `rollback` before propagating the
/// error. The sequencing is extracted so the rollback-on-failure discipline
/// is unit-testable without a tty.
fn init_or_rollback<T>(setup: impl FnOnce() -> Result<T>, rollback: impl FnOnce()) -> Result<T> {
    match setup() {
        Ok(value) => Ok(value),
        Err(e) => {
            rollback();
            Err(e)
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Checks, IssueBuckets, PrRow, Review};
    use std::cell::Cell;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn github_repo(
        issues: Option<Result<IssueBuckets, String>>,
        prs: Option<Result<Vec<PrRow>, String>>,
        fetched_at: Option<Instant>,
    ) -> RepoView {
        RepoView {
            key: "o/repo".to_string(),
            org: "o".to_string(),
            remote: Remote::GitHub {
                org: "o".to_string(),
                repo: "o/repo".to_string(),
            },
            agent_count: 1,
            issues,
            issue_rows: None,
            prs,
            fetched_at,
        }
    }

    fn pr(number: u64) -> PrRow {
        PrRow {
            number,
            title: "t".to_string(),
            checks: Checks::Pass,
            review: Review::Approved,
            draft: false,
        }
    }

    fn staleness_lines(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(line_text)
            .filter(|t| t.contains("github data"))
            .collect()
    }

    #[test]
    fn staleness_on_header_for_success_nonempty() {
        let mut lines = Vec::new();
        let buckets = IssueBuckets {
            total: 2,
            ..Default::default()
        };
        let repo = github_repo(
            Some(Ok(buckets)),
            Some(Ok(vec![pr(1)])),
            Some(Instant::now()),
        );
        render_repo(&repo, &mut lines);
        let stale = staleness_lines(&lines);
        assert_eq!(stale.len(), 1, "exactly one freshness indicator");
        assert!(stale[0].contains("o/repo"), "it is the repo header");
    }

    #[test]
    fn staleness_on_header_for_success_empty() {
        let mut lines = Vec::new();
        let repo = github_repo(
            Some(Ok(IssueBuckets::default())),
            Some(Ok(vec![])),
            Some(Instant::now()),
        );
        render_repo(&repo, &mut lines);
        let stale = staleness_lines(&lines);
        assert_eq!(stale.len(), 1);
        assert!(stale[0].contains("o/repo"));
        // The empty states themselves still render.
        let all: String = lines.iter().map(line_text).collect();
        assert!(all.contains("issues: none open"));
        assert!(all.contains("prs: none open"));
    }

    #[test]
    fn staleness_on_header_for_gh_error() {
        let mut lines = Vec::new();
        let repo = github_repo(
            Some(Err("gh: auth required".to_string())),
            Some(Err("gh: auth required".to_string())),
            Some(Instant::now()),
        );
        render_repo(&repo, &mut lines);
        let stale = staleness_lines(&lines);
        assert_eq!(stale.len(), 1);
        assert!(stale[0].contains("o/repo"));
        let all: String = lines.iter().map(line_text).collect();
        assert!(all.contains("gh error"));
    }

    #[test]
    fn no_staleness_before_first_fetch() {
        let mut lines = Vec::new();
        let repo = github_repo(None, None, None);
        render_repo(&repo, &mut lines);
        assert!(staleness_lines(&lines).is_empty());
        let all: String = lines.iter().map(line_text).collect();
        assert!(all.contains("issues: fetching"));
    }

    #[test]
    fn issue_rows_render_with_stage_labels() {
        let mut repo = github_repo(
            Some(Ok(IssueBuckets {
                total: 2,
                ..Default::default()
            })),
            Some(Ok(vec![])),
            Some(Instant::now()),
        );
        repo.issue_rows = Some(Ok(vec![
            crate::state::IssueRow {
                number: 2,
                title: "Workflow stage".to_string(),
                stage: Stage::ReworkInProgress,
            },
            crate::state::IssueRow {
                number: 3,
                title: "Queued thing".to_string(),
                stage: Stage::Queued,
            },
        ]));
        let mut lines = Vec::new();
        render_repo(&repo, &mut lines);
        let all: Vec<String> = lines.iter().map(line_text).collect();
        let row2 = all.iter().find(|t| t.contains("#2")).expect("issue 2 row");
        assert!(row2.contains("rework in progress"));
        let row3 = all.iter().find(|t| t.contains("#3")).expect("issue 3 row");
        assert!(row3.contains("queued"));
    }

    #[test]
    fn issue_row_tail_collapses_beyond_cap() {
        let mut repo = github_repo(
            Some(Ok(IssueBuckets {
                total: MAX_ISSUES_SHOWN + 2,
                ..Default::default()
            })),
            Some(Ok(vec![])),
            Some(Instant::now()),
        );
        repo.issue_rows = Some(Ok((1..=(MAX_ISSUES_SHOWN + 2) as u64)
            .map(|n| crate::state::IssueRow {
                number: n,
                title: "t".to_string(),
                stage: Stage::None,
            })
            .collect()));
        let mut lines = Vec::new();
        render_repo(&repo, &mut lines);
        let all: String = lines.iter().map(line_text).collect();
        assert!(all.contains("+2 more"));
        assert!(!all.contains(&format!("#{}", MAX_ISSUES_SHOWN + 2)));
    }

    #[test]
    fn init_or_rollback_rolls_back_on_setup_failure() {
        let rolled_back = Cell::new(false);
        let err = init_or_rollback(
            || -> Result<u32> { Err(anyhow::anyhow!("enter failed")) },
            || rolled_back.set(true),
        )
        .unwrap_err();
        assert!(rolled_back.get(), "rollback ran on failure");
        assert_eq!(err.to_string(), "enter failed");
    }

    #[test]
    fn init_or_rollback_does_not_roll_back_on_success() {
        let rolled_back = Cell::new(false);
        let value: u32 = init_or_rollback(|| Ok(42), || rolled_back.set(true)).unwrap();
        assert_eq!(value, 42);
        assert!(!rolled_back.get());
    }

    fn inbox_item(kind: InboxKind, target: crate::inbox::Target) -> InboxItem {
        InboxItem {
            repo: "o/repo".to_string(),
            kind,
            target,
            title: "the title".to_string(),
            detail: String::new(),
            since: None,
        }
    }

    #[test]
    fn empty_inbox_renders_an_explicit_nothing_waiting_state() {
        let board = Dashboard {
            agents: Ok(vec![]),
            groups: vec![],
            inbox: vec![],
            feed: vec![],
        };
        let all: String = render(&board, None).iter().map(line_text).collect();
        assert!(all.contains("WAITING ON YOU (0)"));
        assert!(all.contains("nothing waiting on you"));
    }

    #[test]
    fn inbox_items_render_repo_target_reason_age_and_title() {
        let mut parked = inbox_item(InboxKind::ParkedPr, crate::inbox::Target::Pr(20));
        parked.detail = "review blocking".to_string();
        parked.since = Some(std::time::SystemTime::now() - Duration::from_secs(2 * 3600));
        let board = Dashboard {
            agents: Ok(vec![]),
            groups: vec![],
            inbox: vec![parked],
            feed: vec![],
        };
        let lines = render(&board, None);
        let all: Vec<String> = lines.iter().map(line_text).collect();
        assert!(all.iter().any(|t| t.contains("WAITING ON YOU (1)")));
        let row = all.iter().find(|t| t.contains("PR #20")).expect("item row");
        assert!(row.contains("o/repo"));
        assert!(row.contains("parked PR (review blocking)"));
        assert!(row.contains("2h ago"), "age renders: {row}");
        assert!(row.contains("the title"));
        // No empty-state line when there are items.
        assert!(!all.iter().any(|t| t.contains("nothing waiting on you")));
    }

    #[test]
    fn herdr_error_renders_no_inbox_section() {
        let board = Dashboard {
            agents: Err("server not running".to_string()),
            groups: vec![],
            inbox: vec![],
            feed: vec![],
        };
        let all: String = render(&board, None).iter().map(line_text).collect();
        assert!(all.contains("herdr unreachable"));
        // With no live repo set, claiming an empty inbox would be false.
        assert!(!all.contains("WAITING ON YOU"));
        assert!(!all.contains("nothing waiting on you"));
        // Same for the feed: no section at all.
        assert!(!all.contains("ACTIVITY"));
    }

    #[test]
    fn inbox_tail_collapses_beyond_cap() {
        let board = Dashboard {
            agents: Ok(vec![]),
            groups: vec![],
            inbox: (1..=(MAX_INBOX_SHOWN + 3) as u64)
                .map(|n| inbox_item(InboxKind::TrackerInput, crate::inbox::Target::Issue(n)))
                .collect(),
            feed: vec![],
        };
        let all: String = render(&board, None).iter().map(line_text).collect();
        assert!(all.contains("+3 more"));
        assert_eq!(
            all.matches("the title").count(),
            MAX_INBOX_SHOWN,
            "only the capped number of item rows render"
        );
    }

    // --- activity feed ---

    fn jev(event: &str, issue: u64, ts: &str) -> journal::Event {
        journal::Event {
            run_id: "r1".to_string(),
            workflow: "operator-run".to_string(),
            event: event.to_string(),
            issue: Some(issue),
            ts: Some(ts.to_string()),
            ..Default::default()
        }
    }

    fn feed_board(events: Vec<(String, Vec<journal::Event>)>) -> Dashboard {
        Dashboard {
            agents: Ok(vec![]),
            groups: vec![],
            inbox: vec![],
            feed: feed::build_feed(
                events
                    .iter()
                    .map(|(repo, ev)| (repo.clone(), ev.as_slice())),
            ),
        }
    }

    #[test]
    fn feed_renders_ts_repo_workflow_event_target_and_detail() {
        let mut stop = jev("run_stopped", 7, "2026-09-24T18:52:28.183Z");
        stop.pr = Some(20);
        stop.data = Some(serde_json::json!({"reason": "PR not green"}));
        let board = feed_board(vec![("o/repo".to_string(), vec![stop])]);
        let lines = render(&board, None);
        let all: Vec<String> = lines.iter().map(line_text).collect();
        assert!(all.iter().any(|t| t.contains("ACTIVITY (all repos)")));
        let row = all
            .iter()
            .find(|t| t.contains("run_stopped"))
            .expect("feed row");
        assert!(row.contains("09-24 18:52:28"), "compact timestamp: {row}");
        assert!(row.contains("o/repo"));
        assert!(row.contains("operator-run"));
        assert!(row.contains("#7 PR #20"));
        assert!(row.contains("PR not green"), "salient detail: {row}");
    }

    #[test]
    fn feed_newest_first_across_repos_with_explicit_empty_state() {
        let a = vec![jev("run_started", 1, "2026-09-24T18:00:00Z")];
        let b = vec![jev("pr_opened", 2, "2026-09-24T18:10:00Z")];
        let board = feed_board(vec![("o/a".to_string(), a), ("o/b".to_string(), b)]);
        let lines = render(&board, None);
        let all: Vec<String> = lines.iter().map(line_text).collect();
        let opened = all.iter().position(|t| t.contains("pr_opened")).unwrap();
        let started = all.iter().position(|t| t.contains("run_started")).unwrap();
        assert!(opened < started, "newest event renders first");
        // A repo with an empty journal contributes no lines and no error.
        let board = feed_board(vec![("o/a".to_string(), vec![])]);
        let all: String = render(&board, None).iter().map(line_text).collect();
        assert!(all.contains("no journal events yet"));
    }

    #[test]
    fn feed_filter_shows_one_repo_and_names_it_in_the_header() {
        let a = vec![jev("run_started", 1, "2026-09-24T18:00:00Z")];
        let b = vec![jev("pr_opened", 2, "2026-09-24T18:10:00Z")];
        let board = feed_board(vec![("o/a".to_string(), a), ("o/b".to_string(), b)]);
        let all: String = render(&board, Some("o/a")).iter().map(line_text).collect();
        assert!(all.contains("ACTIVITY (o/a)"));
        assert!(all.contains("run_started"));
        assert!(!all.contains("pr_opened"), "other repo filtered out");
        // A filter naming a repo with no events says so explicitly.
        let all: String = render(&board, Some("o/missing"))
            .iter()
            .map(line_text)
            .collect();
        assert!(all.contains("no journal events for o/missing"));
    }

    #[test]
    fn feed_tail_collapses_beyond_cap() {
        let events: Vec<journal::Event> = (0..(MAX_FEED_SHOWN + 3) as u64)
            .map(|n| jev("run_started", n, "2026-09-24T18:00:00Z"))
            .collect();
        let board = feed_board(vec![("o/a".to_string(), events)]);
        let all: String = render(&board, None).iter().map(line_text).collect();
        assert!(all.contains("+3 more"));
    }

    #[test]
    fn filter_cycles_all_through_each_repo_and_back() {
        let repos = vec!["o/a".to_string(), "o/b".to_string()];
        let mut filter = None;
        filter = next_filter(&filter, &repos);
        assert_eq!(filter.as_deref(), Some("o/a"));
        filter = next_filter(&filter, &repos);
        assert_eq!(filter.as_deref(), Some("o/b"));
        filter = next_filter(&filter, &repos);
        assert_eq!(filter, None, "cycles back to all");
        // A filter naming a departed repo resets to all.
        let stale = Some("o/gone".to_string());
        assert_eq!(next_filter(&stale, &repos), None);
        // No repos on the board: the filter stays at all.
        assert_eq!(next_filter(&None, &[]), None);
    }

    #[test]
    fn format_ts_renders_compact_or_degrades() {
        assert_eq!(
            format_ts(Some("2026-09-24T18:52:28.183Z")),
            "09-24 18:52:28"
        );
        assert_eq!(
            format_ts(Some("2026-09-24T18:52:28+02:00")),
            "09-24 18:52:28"
        );
        assert_eq!(format_ts(None), "—");
        // Bare minimum well-formed shape: time of exactly 8 bytes.
        assert_eq!(format_ts(Some("2026-09-24T18:52:28")), "09-24 18:52:28");
        // A malformed ts renders verbatim, truncated, never as a wrong time.
        assert_eq!(format_ts(Some("not a timestamp")), "not a timestamp");
    }

    #[test]
    fn format_ts_tolerates_malformed_and_multibyte_ts() {
        // Five é before 'T' is a 10-byte, 5-char date: byte-slicing it
        // at [5..] panics. It must degrade to the verbatim render.
        assert_eq!(format_ts(Some("éééééT18:52:28")), "éééééT18:52:28");
        // Multibyte bytes inside the time half also fall back safely.
        assert_eq!(format_ts(Some("2026-09-24T18:5é:28")), "2026-09-24T18:…");
        // Boundary lengths: 9-byte date and 7-byte time miss the shape.
        assert_eq!(format_ts(Some("026-09-24T18:52:28")), "026-09-24T18:5…");
        assert_eq!(format_ts(Some("2026-09-24T18:52:2")), "2026-09-24T18:…");
        // Right lengths but wrong separators/digits are not a timestamp.
        assert_eq!(format_ts(Some("2026/09/24T18:52:28")), "2026/09/24T18:…");
        assert_eq!(format_ts(Some("2026-09-24T18-52-28")), "2026-09-24T18-…");
        // A 16-char malformed string truncates with an ellipsis.
        assert_eq!(format_ts(Some("not a timestamp!")), "not a timestam…");
    }
}
