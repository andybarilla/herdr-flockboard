# Flockboard

A herdr plugin that shows a live dashboard of [Flock](https://github.com/andybarilla/flock)-managed
agent work: open issues by workflow state, which agents are running and where
each issue is in its workflow, and what is waiting on the human — across every
repo active in the herdr session.

Status: **workflow stage board**. The dashboard TUI shows every agent in the
herdr session with its status, plus per-repo open issues by Flock state label
— each with its derived workflow stage (dispatched, pr open, checks pending/
failing, mergeable, review clean/blocking, rework in progress, awaiting
merge, done, stopped, or died-mid-run unknown) — and open PRs with
check/review state, grouped by git-origin organization. Read-only; the
waiting-on-me inbox is still a roadmap item (below).

## Install

```sh
herdr plugin install andybarilla/herdr-flockboard --ref v0.1.0
```

The prebuilt binary is only used when the checkout is the commit that release
was built from, so installing from the default branch generally builds from
source with `cargo build --release`.

Either way, the install finishes by linking `~/.local/bin/herdr-flockboard` at
the binary it installed — the same entrypoint convention `herdr-scuttlebutt`
uses — so the CLI is on your shell `PATH` under the `herdr-*` name:

```sh
herdr-flockboard --help
```

Or, working on it locally:

```sh
cargo build --release
herdr plugin link .
```

`herdr plugin link` skips the build step entirely, so a local checkout gets no
`herdr-flockboard` link; run the checkout's `target/release/flockboard`
directly.

### Update

Herdr plugin v1 has no separate update command. To update a GitHub-managed
install, rerun this command with the desired release tag:

```sh
herdr plugin install andybarilla/herdr-flockboard --ref v0.1.0
```

Reinstalling replaces the managed checkout while preserving existing plugin
config and state. Update a locally linked development checkout through Git and
rebuild it instead; `plugin install` refuses to replace a local link.

The plugin exposes actions for opening the dashboard pane in a split or a tab.

## Data sources

- Live agents, statuses, panes, and cwds: `herdr agent list`, polled every 3s.
- Issues and PRs per repo: `gh`, cached with a 45s TTL and refreshed by a
  dedicated worker thread with a bounded per-command timeout, so a slow or
  hung `gh` never stalls the board.
- Repo grouping: git origin organization, the same per-company grouping
  `herdr-scuttlebutt` uses for its rooms. Multiple agent worktrees of the
  same GitHub repo collapse into one row (one `gh` fetch per owner/repo,
  agent counts aggregated), and a failed `herdr` poll clears the live repo
  set — and with it any GitHub eligibility — until the poll recovers.
  Repos without a usable GitHub origin (no origin, another host, or a
  broken/missing cwd) are keyed and rendered by full working-directory
  path, so same-named directories in different locations stay distinct.
- Workflow stage per issue: the Flock event journal (`<repo>/.flock/events.jsonl`,
  see [flock#45](https://github.com/andybarilla/flock/issues/45)) read from the
  repo's live checkouts on every agent poll, correlated with PR
  checks/mergeability (using the project-config green classifier:
  SKIPPED/NEUTRAL satisfied, unknown fail-closed) and herdr agent liveness.
  The reader is tolerant — a missing, truncated, or forward-versioned journal
  degrades to label/PR-inferred stages rather than blanking the board — and a
  run with no terminal event whose supervising agent is gone shows as
  `unknown (run may have died)`.

## Roadmap

1. ~~Scaffold: Cargo project, plugin manifest, build/install/link scripts, CI.~~
2. ~~Live status board: agents + per-repo issues/PRs, read-only.~~
3. ~~Per-issue workflow stage view (reads the Flock event journal).~~
4. "Waiting on me" inbox: blocking reviews, parked PRs, stopped runs,
   needs-info items.
5. Cross-repo activity feed (scuttlebutt-style).

## Development

```sh
cargo build
cargo test
scripts/check-versions.sh   # Cargo.toml, herdr-plugin.toml, Cargo.lock, README versions agree
```

CI runs fmt, clippy (`-D warnings`), tests on Linux and macOS, and a hermetic
smoke test of the install/link script (`scripts/test-install-link.sh`).
