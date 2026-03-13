# Oven CLI - Design Decisions

## Identity
- **Package name:** `oven-cli`
- **Binary name:** `oven`
- **Theme:** Kitchen
- **Language:** Rust
- **Inspired by:** [ocak](~/dev/ocak) - rewrite with cleaner design

## Agents (5 total, all Opus)
| Agent | Role | Tools | Model |
|-------|------|-------|-------|
| **Planner** | Decides what to do, batches/parallelizes, keeps re-evaluating during pipeline runs. Creates draft PRs early. | Read-only | opus |
| **Implementer** | Writes code + tests, runs test suite | Full access | opus (may switch to sonnet) |
| **Reviewer** | Code review + security review + simplify (rolled into one) | Read-only | opus |
| **Fixer** | Addresses reviewer findings | Full access | opus |
| **Merger** | Marks PR ready, merges if `--merge` flag | gh CLI | opus |

## Core Workflow
```
Issue labeled `o-ready`
  → Planner picks it up (oldest first, FIFO)
  → Planner creates draft PR + worktree
  → Implementer writes code in worktree, pushes to PR branch
  → Reviewer reviews (code quality + security + simplify)
  → If findings: Fixer addresses critical/warning items
  → Reviewer re-reviews (max 2 fix cycles)
  → If still issues: stop, comment what's unresolved, human takes over
  → If clean: Merger marks PR ready-for-review
  → If --merge: Merger merges the PR
  → Issue labeled `o-complete`

Meanwhile: Planner continuously polls for new `o-ready` issues
           and starts parallel pipelines if safe to do so.
```

- **PR-centric:** All agent activity (comments, status) goes on the PR, not the issue
- **Planner is continuous:** Background loop. Picks up new issues mid-run if parallelizable
- **Manual merge by default:** `-m` opts into auto-merge
- **Oldest issues first:** FIFO ordering
- **No resume:** If something fails, clean and start over

## Review-Fix Loop Strategy

Based on research across SWE-agent, OpenHands, Aider, Cursor, CodeRabbit, Devin, Google ADK:

- **Max 2 review-fix cycles** (implement → review → fix → review → fix → final review)
- **Each review is full-scope** - catches regressions from fixes
- **Fixer gets structured findings** - only critical + warning items
- **Hard caps at 3 layers:**
  1. Per-cycle cap: 2 fix rounds max
  2. Cost cap: configurable per-pipeline budget
  3. Turn cap: max turns per agent invocation
- **Escalation:** Stop, leave PR open with clear comment. Human takes over.
- **No retry on format/parse errors** - treat as hard failure

## Skills
| Skill | Purpose |
|-------|---------|
| `/chop` | Interactive issue design. Asks before adding `o-ready` label. |
| `/taste-test` | Codebase audit (security, patterns, tests, data, deps) |

## CLI API

```
oven prep [--force]
    Scaffold project: recipe.toml, .claude/agents/, .oven/ directory + SQLite db.
    --force    Overwrite existing config

oven on [ISSUE_IDS] [-d] [-m]
    Start the pipeline. Outputs a run ID (8 hex chars, e.g. a3f9b2c1).
    Without ISSUE_IDS, enters continuous polling mode.
    ISSUE_IDS are comma-separated: oven on 123,245

    -d, --detached     Run in background (daemonize)
    -m, --merge        Auto-merge PRs (default: manual)

oven off
    Stop a detached pipeline process.

oven look [RUN_ID]
    View pipeline logs. Tails in real-time if the run is active, dumps if complete.
    Without RUN_ID, shows the current/most recent run.

    --agent <NAME>     Filter to specific agent

oven report [RUN_ID]
    Show run details: cost breakdown, run time, agent summaries, outcomes.
    Without RUN_ID, shows the most recent run.

    --all              Show all runs
    --json             Output as JSON

oven clean
    Clean worktrees, logs, and merged branches.
    --only-logs        Only remove logs
    --only-trees       Only remove worktrees
    --only-branches    Only remove merged branches

oven ticket create <TITLE>
    Create a local issue (.oven/issues/).
    --body <TEXT>      Issue body (or opens $EDITOR)
    --ready            Add o-ready label immediately

oven ticket list
    List local issues.
    --label <LABEL>    Filter by label

oven ticket view <ID>
    Display a local issue.

oven ticket close <ID>
    Mark a local issue as completed.
```

### Global Flags
```
--verbose, -v      Verbose output
--quiet, -q        Suppress non-essential output
```

## Run IDs
- 8 hex characters from UUID v4 (e.g. `a3f9b2c1`)
- Printed on `oven on` startup
- Used in logs directory: `.oven/logs/<run_id>/`
- Used in `oven report <run_id>` and `oven look --run <run_id>`
- Stored in SQLite for querying

## Config

Two-level config with project overriding user defaults:

### User config: `~/.config/oven/recipe.toml`
```toml
# Machine-level defaults and multi-repo mappings

[pipeline]
cost_budget = 15.0            # Default USD budget per run
turn_limit = 50               # Default max turns per agent

# Multi-repo support (god repo)
[repos]
my-service = "/Users/clay/dev/my-service"
other-repo = "/Users/clay/dev/other-repo"
```

### Project config: `recipe.toml`
```toml
# Project-specific config. Overrides user config.

[project]
name = "my-project"           # Auto-detected from git remote
test = "cargo test"           # Test command
lint = "cargo clippy"         # Lint command

[pipeline]
max_parallel = 2              # Max concurrent issue pipelines
cost_budget = 10.0            # Override user default
poll_interval = 60            # Seconds between issue polls

[labels]
ready = "o-ready"
cooking = "o-cooking"
complete = "o-complete"
failed = "o-failed"
```

## Labels
| Label | Meaning |
|-------|---------|
| `o-ready` | Issue ready for pipeline pickup |
| `o-cooking` | Pipeline is actively working on it |
| `o-complete` | Pipeline finished successfully |
| `o-failed` | Pipeline failed |

## State & Storage
- **Directory:** `.oven/`
- **Database:** SQLite (`.oven/oven.db`) - pipeline state, cost tracking, agent runs
- **Worktrees:** `.oven/worktrees/`
- **Logs:** `.oven/logs/<run_id>/`
- **Local issues:** `.oven/issues/`
- **PID file:** `.oven/oven.pid` (for detached mode, `oven off` reads this)

## GitHub Action Strategy

Enterprise-grade approach based on research of Renovate, CodeRabbit, OpenHands, Claude Code Action:

### Architecture: JavaScript/TypeScript Action
- JS action (not Docker, not composite) - fastest cold start, cross-platform
- Installs `claude` CLI + `oven` binary at runtime (not bundled)
- Posts progress updates to PR comments during long runs

### Authentication: GitHub App + OIDC (never PATs)
- GitHub App via `actions/create-github-app-token` for repo access
  - Short-lived tokens (1hr), fine-grained permissions, bot identity
- `ANTHROPIC_API_KEY` as repository secret
- Optional OIDC for Bedrock/Vertex

### Security: Defense in Depth
- Pin all actions to commit SHAs (not tags - supply chain attack prevention)
- `step-security/harden-runner` for network egress monitoring
- Minimal permissions: read-only default, escalate per-job
- Secrets via env vars only (never CLI args)

### Concurrency
- Per-issue concurrency groups: `group: oven-${{ github.event.issue.number }}`
- `cancel-in-progress: false` - don't kill running pipelines
- GitHub-hosted runners fine for 10-30 min runs

### Triggers
- `issues: [labeled]` - primary (issue gets `o-ready`)
- `workflow_dispatch` - manual with issue number input
- `schedule` - optional cron

### Graceful Shutdown
- SIGTERM → post partial results to PR → push in-progress branch

## Technical Stack
- **Runtime:** tokio (async)
- **Claude invocation:** `claude -p --output-format stream-json`
- **Git isolation:** worktrees per issue
- **Config:** TOML (`toml` crate)
- **State:** SQLite (`rusqlite`)
- **CLI framework:** `clap`
- **Templating:** TBD for `oven prep`
- **GitHub:** `gh` CLI for all interactions
- **Models:** All opus (implementer may move to sonnet later)

## Improvements over ocak
- **Continuous planner:** Picks up new issues during pipeline runs
- **PR-centric:** All activity on the PR, not the issue
- **Oldest-first:** FIFO issue ordering
- **SQLite state:** Queryable, reliable, no JSON file sprawl
- **Unified reviewer:** One agent instead of three
- **Simpler config:** Fewer knobs, better defaults
- **Rust:** Type safety, performance, single binary
- **GH Action:** First-class, enterprise-grade GitHub Action support
- **Run IDs:** Every run is trackable with an 8-char hex ID

## Features Dropped from ocak
- hiz (fast mode)
- Resume from failure - just clean and start over
- Audit agent - folded into reviewer
- Separate security reviewer - folded into reviewer
- Heavy configurability
- `allowed_authors` / `require_comment` safety config - revisit later

## Open Questions
- Default cost budget per pipeline run? ($10? $15? $20?)
- `oven prep` template engine choice (askama? tera?)
- Exact PR comment format / structure
- How planner communicates parallelization decisions to the runtime
