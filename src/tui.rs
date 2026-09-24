//! The read-only dashboard TUI. Two threads: a refresh worker owns all
//! blocking I/O (`herdr`, `git`, `gh`) and pushes whole `Dashboard`
//! snapshots over a channel; the UI thread only handles keys and draws, so
//! the board never stalls on a slow GitHub refresh.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::git_org::{Remote, RemoteCache};
use crate::github::GhCli;
use crate::herd::{HerdCli, HerdControl};
use crate::state::{self, AgentStatus, Checks, Dashboard, RepoView, Review, LABEL_PRIORITY};

/// How long the UI waits for a key before redrawing; also the cadence that
/// keeps "updated Ns ago" moving between agent polls.
const UI_TICK: Duration = Duration::from_millis(250);

/// PRs shown per repo before collapsing the tail into "+N more".
const MAX_PRS_SHOWN: usize = 8;

pub fn run() -> Result<()> {
    let (tx, rx) = mpsc::channel::<Dashboard>();
    std::thread::spawn(move || refresh_loop(tx));

    let mut guard = TerminalGuard::new()?;
    let mut board: Option<Dashboard> = None;
    loop {
        // Keep only the freshest snapshot; the worker outpaces the UI when
        // GitHub is slow.
        while let Ok(d) = rx.try_recv() {
            board = Some(d);
        }
        guard.terminal.draw(|frame| draw(frame, board.as_ref()))?;
        if event::poll(UI_TICK)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Owns all fetching. Runs on the agent-poll cadence; GitHub slots refetch
/// only when their TTL expires, and results — errors included — land in the
/// slots so the board renders what the last fetch actually said.
fn refresh_loop(tx: mpsc::Sender<Dashboard>) {
    let herd = HerdCli::default();
    let gh = GhCli;
    let mut slots: HashMap<PathBuf, state::RepoSlot> = HashMap::new();
    let mut remotes = RemoteCache::default();
    loop {
        let agents = herd
            .list_agents()
            .map_err(|e| e.to_string())
            .map(state::agent_rows);
        if let Ok(rows) = &agents {
            state::sync_slots(rows, &mut slots, &mut |cwd| remotes.get(cwd));
        }
        state::refresh_due(&gh, &mut slots, Instant::now(), state::GH_TTL);
        if tx.send(state::dashboard(agents, &slots)).is_err() {
            return; // UI exited
        }
        std::thread::sleep(state::AGENT_POLL);
    }
}

fn draw(frame: &mut Frame, board: Option<&Dashboard>) {
    let lines = match board {
        None => vec![Line::from(Span::styled(
            "connecting to herdr…",
            Style::default().fg(Color::DarkGray),
        ))],
        Some(b) => render(b),
    };
    let title = format!(
        " Flockboard — q quit · agents every {}s · github every {}s ",
        state::AGENT_POLL.as_secs(),
        state::GH_TTL.as_secs(),
    );
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        frame.area(),
    );
}

fn render(board: &Dashboard) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    render_agents(board, &mut lines);
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

fn render_repo(repo: &RepoView, lines: &mut Vec<Line<'static>>) {
    lines.push(Line::from(vec![
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
    ]));
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
            let mut spans = vec![
                Span::raw(format!("    issues: {} open", b.total)),
                Span::styled(
                    format!(" — {}", detail.join(" · ")),
                    Style::default().fg(Color::Yellow),
                ),
            ];
            spans.push(staleness_span(repo));
            Line::from(spans)
        }
    };
    lines.push(line);
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
            lines.push(Line::from(vec![
                Span::raw(format!("    prs: {} open", prs.len())),
                staleness_span(repo),
            ]));
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
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }
}
