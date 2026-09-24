# Flock Project Config

This file tells Flock skills how this repository handles tracked work. Keep repository policy here, not machine-specific commands or personal preferences.

## Tracker

System: GitHub Issues
Repository: `andybarilla/herdr-flockboard`
URL: `https://github.com/andybarilla/herdr-flockboard`
Default branch: `main`

Read issue: `gh issue view <number> --comments`
List ready issues: `gh issue list --state open --label ready-for-agent --json number,title,labels,updatedAt,url --limit 50`
Read PR: `gh pr view <number> --json number,title,body,state,author,url,headRefName,baseRefName,mergeable,reviewDecision,statusCheckRollup`
Read PR diff: `gh pr diff <number>`

## Labels

State labels:
- ready-for-agent: `ready-for-agent`
- needs-triage: `needs-triage`
- needs-info: `needs-info`
- ready-for-human: `ready-for-human`
- blocked: `blocked`
- wontfix: `wontfix`

Size labels:
- epic: `epic`

Other labels of interest:
- bug: `bug`
- enhancement: `enhancement`
- documentation: `documentation`

## Branch

Default base branch: `main`
Issue branch pattern: `flock/issue-<number>-<short-slug>`
PR branch pattern: same as issue branch pattern unless a user chooses otherwise.

Before starting issue work:

```bash
git fetch origin
git checkout main
git pull --ff-only
git checkout -b flock/issue-<number>-<short-slug>
```

## Gate

Commands that prove a worker's change locally:

```bash
scripts/check-versions.sh && cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked && sh scripts/test-install-link.sh
```

Notes:
- This mirrors the CI workflow (`.github/workflows/ci.yml`); a local green that CI would reject is a false green — run the full gate, not just `cargo test`.
- `cargo clippy --locked` requires an up-to-date `Cargo.lock`; check-versions catches a stale one.
- `scripts/test-install-link.sh` is hermetic (temp HOME, file:// fixture release) and needs no Rust toolchain.

## PR

Open PRs with:

```bash
git push -u origin HEAD
gh pr create --fill
```

Tracker completion policy:
- Use neutral PR references such as `Refs #<number>`; do not rely on PR auto-close wording to complete issues.
- After successful issue work, explicitly comment with completion evidence and close the tracker item.
- Close only after implementation, observed validation, PR/update preparation when applicable, and required review have succeeded with no blocking verdict.
- Leave the tracker item open when work is blocked, validation fails, review is blocking, or required review cannot run.
- Check commit messages for accidental closing keywords before opening the PR.

## Review

Default review tier: standard
Advanced review available: yes
Escalate to advanced review when:
- concurrency/state machines are touched (dashboard polling, caches, event readers)
- the install/build/link scripts or plugin manifest change (they run on user machines via `herdr plugin install`)
- public CLI surface or journal/consumer compatibility is touched
- tests are weak or acceptance criteria are unclear

Blocking bar:
- correctness bugs
- missing required behavior
- meaningful test gaps
- security/data-safety risk
- install/link script changes that could overwrite files the plugin does not own

## Merge

Policy: human merges

Notes:
- Default: human merges. Flock workflows merge nothing without explicit policy here.
- This repository may later switch to conditional operator merge following the `andybarilla/flock` Merge policy; record the exact green definition, max PR wait, and merge method here when enabling it.

## Retry

Policy: no retry

Notes:
- Flock v1 issue-loop stops on blocked/failed work.

## Operator Approval Policy

Mutating operator automation requires explicit approval policy here. When this section is absent, operator workflows must use dry-run only and ask before any mutation.

Approval categories:
- Issue selection for queued work: ask
- Grooming labels/comments: ask
- Triage labels/comments: ask
- Branch creation: ask
- Commits: ask
- PR creation/update: ask
- Tracker completion/issue close: ask
- Merge: never

Limits:
- Max cycles per operator run: 1
- Max issues worked per operator run: 1
- Max grooming batches per operator run: 0
- Max triage issues per operator run: 0
- Max runtime: ask

Stop conditions:
- missing or insufficient project config
- dirty or unexpected worktree state
- auth, branch, validation, PR, review, or tracker failure
- blocking product or technical question
- ambiguous, too broad, already complete, or non-dispatchable issue
- failed validation or blocking review
- configured limits reached

## Workflow Defaults

Ready queue label: ready-for-agent
Groom target depth: 6
Groom batch size: 10
Issue loop default limit: 1
Confirm before starting queued issue: yes

## Project Notes

- Rust binary plugin for herdr, modeled on `andybarilla/herdr-scuttlebutt` (manifest, build/link scripts, CI shape).
- `scripts/fetch-or-build.sh` links `~/.local/bin/herdr-flockboard`; it must never overwrite anything it does not own (see the ownership rules in the script).
- The dashboard reads the Flock event journal (`<repo>/.flock/events.jsonl`) produced by flock#45's `flock_event` extension; treat that schema (v1, additive-only) as a compatibility boundary.
- Release workflow (tag-driven prebuilt binaries, like scuttlebutt's `release.yml`) is intentionally not part of the scaffold; until then installs build from source via the fetch-or-build fallback.
