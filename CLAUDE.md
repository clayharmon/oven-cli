# Oven CLI

Oven (`oven-cli` on crates.io, `oven` binary) is a Rust CLI that orchestrates Claude Code agent pipelines against GitHub issues. Kitchen theme throughout.

Read DECISIONS.md for the full design document. This file covers what you need to build and maintain the codebase.

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
- Rust, tokio async runtime
- clap (CLI), rusqlite (state), toml (config)
- gh CLI for all GitHub operations
- claude CLI for all agent invocations
- Git worktrees for isolation

## Code conventions
- No unnecessary abstractions. Three similar lines > premature helper function.
- Test coverage for all core logic. Integration tests hit real sqlite.
- Error handling: use anyhow for application errors, thiserror for library errors.
- No unwrap() in non-test code. Use context-rich error messages.
- Keep modules focused. One responsibility per file.
- Run `cargo clippy` and `cargo fmt` before committing.

## Skills (Claude Code skills, not oven commands)
- `/chop` - interactive issue design (scaffolded into .claude/skills/ by `oven prep`)
- `/taste-test` - codebase audit (scaffolded into .claude/skills/ by `oven prep`)

## GitHub Action
JS/TS action (not Docker). GitHub App auth, SHA-pinned deps, per-issue concurrency groups. See DECISIONS.md for full details.
