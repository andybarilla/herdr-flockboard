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
- Conditional operator merge is enabled (see `## Merge`): issue closure is deferred to post-merge. The delegated workflow leaves the issue open with a completion-evidence comment; the operator closes it (`gh issue close <number> --reason completed`) only after the merge is verified on the default branch.
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

Policy: conditional operator merge

Method: `gh pr merge <number> --squash`

Scope (either must be observed, never anything else):
- a green PR opened by the operator in the current run, or
- a green PR meeting the resume criteria: head branch matches `flock/issue-<number>-<short-slug>`, author is the authenticated account, linked issue open, self-authored `flock-operator-run` provenance marker comment on the PR, the gate re-run against the PR head passed in the resuming run, and a fresh non-blocking `pr-review` verdict in the resuming run.

Green definition (all observed from `gh` output):
- every `statusCheckRollup` entry satisfied (`SUCCESS`, `SKIPPED`, or `NEUTRAL`); none pending or failing
- `mergeable: MERGEABLE`
- Flock review verdict non-blocking
- `reviewDecision: APPROVED` when required by branch protection; not required otherwise

Verified check-wait command (green classifier; fail-closed — unrecognized typenames, conclusions, or states classify as `failing`):

```bash
gh pr view <number> --json state,mergeable,reviewDecision,statusCheckRollup --jq 'def ok: if has("conclusion") then (.status=="COMPLETED") and (.conclusion=="SUCCESS" or .conclusion=="SKIPPED" or .conclusion=="NEUTRAL") elif has("state") then .state=="SUCCESS" else false end; def bad: if has("conclusion") then (.status=="COMPLETED") and (.conclusion=="FAILURE" or .conclusion=="CANCELLED" or .conclusion=="TIMED_OUT" or .conclusion=="ACTION_REQUIRED" or .conclusion=="STARTUP_FAILURE") elif has("state") then (.state=="ERROR" or .state=="FAILURE") else true end; if .state!="OPEN" then "unexpected-state:\(.state)" elif .reviewDecision=="CHANGES_REQUESTED" then "blocking-review" elif .mergeable=="CONFLICTING" then "conflict" elif .mergeable=="UNKNOWN" or .reviewDecision=="REVIEW_REQUIRED" then "pending" elif ([.statusCheckRollup[]?|bad]|any) then "failing" elif .mergeable=="MERGEABLE" and ([.statusCheckRollup[]?|ok]|all) then "green" else "pending" end'
```

Classifications: `green` | `pending` | `failing` | `conflict` | `blocking-review` | `unexpected-state:<state>`. Poll every 60s; merge only on `green`.

Max PR wait: 15m (stop with `PR not green` on timeout; leave PR, issue, and branch in place for a human or a later resume)

Notes:
- All other PRs remain human-merged. Failing checks, conflicts, blocking review, or ambiguous ownership stop the run before any merge command.
- The issue is closed completed only after the merge is verified on the default branch (see Tracker completion policy in `## PR`).

## Retry

Policy: up to 2 retries

Notes:
- A failed or blocked issue may be retried up to 2 times within an operator run, re-dispatched through the normal `issue-loop` workflow.
- Retry only transient failures (tooling, network, flaky gate). Never retry blocking review (the PR parks under the operator's parked-PR rules), ambiguous or too-broad scope, or an issue that is already complete — those stop for a human.

## Operator Approval Policy

Mutating operator automation requires explicit approval policy here. When this section is absent, operator workflows must use dry-run only and ask before any mutation.

Approval categories:
- Issue selection for queued work: auto-approve
- Grooming labels/comments: auto-approve
- Triage labels/comments: auto-approve
- Branch creation: auto-approve
- Commits: auto-approve
- PR creation/update: auto-approve
- Tracker completion/issue close: auto-approve
- Merge: auto-approve — conditional only: green (per `## Merge`), run-opened or provenance-marked resume-target PRs only, squash. All other PRs remain human-merged.

Limits:
- Max cycles per operator run: 3
- Max issues worked per operator run: 2
- Max grooming batches per operator run: 1
- Max triage issues per operator run: 2
- Max runtime: 60m
- Max PR wait: 15m

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
