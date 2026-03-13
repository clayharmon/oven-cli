# Oven CLI

Oven (`oven-cli` on crates.io, `oven` binary) is a Rust CLI that orchestrates Claude Code agent pipelines against GitHub issues. Kitchen theme throughout.

Read DECISIONS.md for the full design document. Read ROADMAP.md for the phased implementation plan.

## What this project does

Users label GitHub issues with `o-ready`. Oven's planner agent picks them up (oldest first), creates a draft PR, and runs a pipeline of Claude Code agents against that PR: implement -> review -> fix (up to 2 cycles) -> merge. All agent comments go on the PR, not the issue. The planner continuously polls for new issues and can parallelize work mid-run.

## Architecture

### CLI commands (clap)
- `oven prep` - scaffold project (recipe.toml, .claude/agents/, .oven/)
- `oven on [IDS]` - start pipeline. IDS are comma-separated issue numbers. Flags: `-d` (detached), `-m` (auto-merge). Prints a run ID (8 hex chars from uuid).
- `oven off` - kill detached process (reads .oven/oven.pid)
- `oven look [RUN_ID]` - view logs. Tails if active, dumps if done. `--agent <NAME>` filters.
- `oven report [RUN_ID]` - cost, runtime, summary. `--all` for history, `--json` for machine output.
- `oven clean` - remove worktrees, logs, merged branches. `--only-logs`, `--only-trees`, `--only-branches`.
- `oven ticket create|list|view|close` - local issue management in .oven/issues/

### Agents (5, all invoked via `claude -p --output-format stream-json`)
1. **Planner** - read-only. Decides batching/parallelization, creates draft PRs, continuously re-evaluates.
2. **Implementer** - full access. Writes code + tests in a worktree.
3. **Reviewer** - read-only. Code quality + security + simplify in one pass. Outputs structured findings (critical/warning/info).
4. **Fixer** - full access. Addresses critical + warning findings from reviewer.
5. **Merger** - gh CLI. Marks PR ready-for-review, merges if -m flag.

### Agent tool scoping
| Agent | Allowed tools |
|-------|---------------|
| Planner | Read, Glob, Grep |
| Implementer | Read, Write, Edit, Glob, Grep, Bash |
| Reviewer | Read, Glob, Grep |
| Fixer | Read, Write, Edit, Glob, Grep, Bash |
| Merger | Bash |

### Review-fix loop
Max 2 cycles: implement -> review -> fix -> review -> fix -> final review. If still broken, stop and comment on PR with what's unresolved. No resume - clean and start over.

### Config (TOML, two levels)
- User: `~/.config/oven/recipe.toml` - defaults, multi-repo mappings
- Project: `recipe.toml` in repo root - overrides user config

### State
- `.oven/oven.db` - SQLite. Pipeline state, cost tracking, agent run history.
- `.oven/logs/<run_id>/` - per-run log files
- `.oven/worktrees/` - git worktrees per issue
- `.oven/issues/` - local issue markdown files
- `.oven/oven.pid` - PID file for detached mode

### Labels
`o-ready`, `o-cooking`, `o-complete`, `o-failed`

## Tech stack
- Rust (edition 2024), tokio async runtime
- clap 4 derive API (CLI), rusqlite with bundled SQLite (state), toml (config)
- tracing + tracing-subscriber + tracing-appender (logging)
- gh CLI for all GitHub operations
- claude CLI for all agent invocations
- Git worktrees for isolation

## Project structure
```
src/
  main.rs                   thin entry point, clap parse + delegate
  lib.rs                    module declarations
  cli/
    mod.rs                  Cli struct, Commands enum (clap derive)
    prep.rs                 oven prep
    on.rs                   oven on
    off.rs                  oven off
    look.rs                 oven look
    report.rs               oven report
    clean.rs                oven clean
    ticket.rs               oven ticket subcommands
  config/
    mod.rs                  Config struct, layered loading
  db/
    mod.rs                  connection setup, migrations, pragmas
    runs.rs                 pipeline run CRUD
    agent_runs.rs           agent execution records
  git/
    mod.rs                  worktree management, branch ops
  process/
    mod.rs                  subprocess runner (tokio::process)
    stream.rs               claude stream-json parser, cost extraction
  github/
    mod.rs                  gh CLI wrapper
    labels.rs               label create/add/remove
    issues.rs               issue fetch/comment/transition
    prs.rs                  PR create/update/merge
  agents/
    mod.rs                  AgentRole enum, invocation logic
    planner.rs
    implementer.rs
    reviewer.rs             structured findings output
    fixer.rs
    merger.rs
  pipeline/
    mod.rs                  orchestration, polling loop
    state.rs                status transitions, state machine
    executor.rs             step execution, review-fix loop
  logging.rs                tracing setup (file + stderr)
  errors.rs                 thiserror types
tests/
  common/
    mod.rs                  shared test helpers, fixtures
  cli_tests.rs              assert_cmd integration tests
  pipeline_tests.rs         pipeline integration tests
  db_tests.rs               database tests
```

## Dependencies
```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
rusqlite = { version = "0.32", features = ["bundled"] }
rusqlite_migration = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
tracing-appender = "0.2"
uuid = { version = "1", features = ["v4"] }
dirs = "6"
chrono = { version = "0.4", features = ["serde"] }
tokio-util = "0.7"

[dev-dependencies]
assert_cmd = "2"
predicates = "3"
assert_fs = "1"
tempfile = "3"
rstest = "0.23"
mockall = "0.13"
proptest = "1"
```

## Formatting
`rustfmt.toml` at repo root. Run `cargo +nightly fmt` (import grouping requires nightly).
```toml
edition = "2024"
max_width = 100
tab_spaces = 4
use_small_heuristics = "Max"
imports_granularity = "Crate"
group_imports = "StdExternalCrate"
```

## Linting
Configured in `Cargo.toml`. Run `cargo clippy --all-targets -- -D warnings`.
```toml
[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
all = { level = "deny", priority = -1 }
pedantic = { level = "warn", priority = -1 }
nursery = { level = "warn", priority = -1 }
module_name_repetitions = "allow"
must_use_candidate = "allow"
missing_errors_doc = "allow"
missing_panics_doc = "allow"
```

## Code conventions
- No unnecessary abstractions. Three similar lines > premature helper function.
- Error handling: anyhow for application errors, thiserror for library errors.
- No unwrap() in non-test code. Use `.context("what you were doing")?` for rich errors.
- `unsafe` is forbidden via lint. No exceptions.
- All SQL queries use parameterized statements with `params![]`. Never interpolate.
- Keep modules focused. One responsibility per file.
- Run `cargo clippy` and `cargo +nightly fmt` before committing.

## Testing
- **Unit tests**: `#[cfg(test)] mod tests` inline in every module with logic.
- **Integration tests**: `tests/` directory using assert_cmd + predicates for CLI commands.
- **Database tests**: `Connection::open_in_memory()` with migrations applied. Real SQLite, no mocks.
- **Async tests**: `#[tokio::test]` for anything async.
- **External CLI mocking**: Define traits for gh/claude interactions, mock with mockall. Never call real CLIs in tests.
- **Property tests**: proptest for config parsing, ID generation, serialization roundtrips.
- **Filesystem tests**: assert_fs or tempfile for temp directories with auto-cleanup.
- **Test runner**: cargo-nextest.
- **Coverage**: cargo-llvm-cov, 85% line coverage minimum.
- **Shared helpers**: `tests/common/mod.rs` for fixtures and builders.

## Database conventions
SQLite with these pragmas on every connection:
```rust
conn.pragma_update(None, "journal_mode", "WAL")?;
conn.pragma_update(None, "synchronous", "NORMAL")?;
conn.pragma_update(None, "busy_timeout", "5000")?;
conn.pragma_update(None, "foreign_keys", "ON")?;
```
Migrations via rusqlite_migration (user_version based, no migration table). Test migrations with `MIGRATIONS.validate()`.

## Process management
- `tokio::process::Command` with `kill_on_drop(true)` for all subprocesses.
- Graceful shutdown: `CancellationToken` from tokio-util combined with `tokio::signal::ctrl_c()`.
- Agent invocation: `claude -p --output-format stream-json --allowedTools <TOOLS>`.
- Detached mode: spawn new process with `std::process::Command` (not fork). Write PID to `.oven/oven.pid`.
- Always `wait()` on child processes to prevent zombies.

## CI pipeline
GitHub Actions with dtolnay/rust-toolchain + Swatinem/rust-cache. Run locally with `just ci` (or `just check` for the quick subset without coverage/deny). Jobs:
- `fmt` - nightly rustfmt check
- `clippy` - lint with -D warnings
- `test` - cargo-nextest on stable + MSRV (1.85)
- `coverage` - cargo-llvm-cov with 85% threshold
- `deny` - cargo-deny for license/advisory/source audits

## Skills (Claude Code skills, not oven commands)
- `/cook` - interactive issue design (scaffolded into .claude/skills/ by `oven prep`)
- `/refine` - codebase audit (scaffolded into .claude/skills/ by `oven prep`)

## GitHub Action
JS/TS action (not Docker). GitHub App auth, SHA-pinned deps, per-issue concurrency groups. See DECISIONS.md for full details.
