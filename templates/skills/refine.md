---
name: refine
description: Comprehensive codebase audit -- security, error handling, patterns, tests, data, dependencies
disable-model-invocation: true
---

# /refine -- Codebase Audit

You are the `/refine` skill. Your job is to run a comprehensive, multi-dimensional audit of the codebase and produce a prioritized findings report.

**Input:** The user may optionally specify a scope: `security`, `errors`, `patterns`, `tests`, `data`, `dependencies`, or leave it empty for all dimensions.

## Phase 1: Orientation

1. Read `CLAUDE.md` for architecture, conventions, and project structure
2. Map the project structure using Glob to understand directories and file organization
3. Identify the language, framework, and tooling

## Phase 2: Static Analysis

Run all available tools and capture output. Do NOT stop if a tool fails.

For Rust projects:
```bash
cargo clippy --all-targets 2>&1 || true
cargo +nightly fmt --check 2>&1 || true
cargo deny check 2>&1 || true
```

For non-Rust projects, detect and run appropriate tools based on what's in the project (rubocop, eslint, mypy, etc.).

## Phase 3: Manual Analysis

For each dimension below, perform targeted searches and reads. Only run dimensions matching the user's scope argument (or all if no argument was given).

### Security (`security`)
- **Auth gaps**: endpoints or handlers missing authentication
- **Unvalidated inputs**: user data used without sanitization or validation
- **SQL/command injection**: string interpolation in queries or shell commands (`format!` in SQL, unescaped args in `Command::new`)
- **Hardcoded secrets**: passwords, API keys, tokens in source code
- **`unsafe` blocks**: should be forbidden by lint, but verify
- **Path traversal**: user-controlled paths in file operations
- **TOCTOU race conditions**: check-then-act patterns on filesystem or shared state

### Error Handling (`errors`)
- **`unwrap()` or `expect()` in non-test code**: find with `Grep`
- **Swallowed errors**: caught with no re-raise, logging, or handling
- **Inconsistent error types**: mixing anyhow and thiserror incorrectly
- **Missing error handling**: external calls (HTTP, database, file I/O) without `?` or match
- **Leaking internals**: error messages that expose implementation details

### Bad Patterns (`patterns`)
- **God modules**: files over 300 lines
- **TODO/FIXME/HACK/XXX comments**: search with Grep
- **Dead code**: commented-out code, unused imports, unreachable branches
- **Code duplication**: repeated logic that should be shared
- **Tight coupling**: unrelated modules depending on each other's internals
- **Premature abstractions**: traits or generics with only one implementation

### Test Gaps (`tests`)
- **Untested public functions**: public functions/methods without corresponding tests
- **Meaningless tests**: tests that don't assert anything or only assert `true`
- **`#[ignore]` tests**: find and explain why they're ignored
- **Missing edge cases**: boundary values, empty inputs, max values
- **Missing error paths**: error conditions not tested

### Data Issues (`data`)
- **Unbounded queries**: no LIMIT on database queries
- **Missing indexes**: queries on columns without indexes
- **SQL without parameters**: string interpolation in SQL statements
- **Float for money**: floating point used for monetary values
- **Missing foreign keys**: related tables without FK constraints

### Dependencies (`dependencies`)
- **`cargo deny check` results**: advisories, licenses, sources
- **Unused dependencies**: `cargo machete` if available, otherwise manual check
- **Overly broad version constraints**: `*` or no upper bound
- **Outdated with known issues**: check for deprecated crates

## Phase 4: Report

Output findings grouped by severity. Every finding MUST have a specific file path and line number.

```
# Codebase Audit Report

**Scope**: [all | security | errors | patterns | tests | data | dependencies]

## Critical (fix immediately)

### [Finding title]
**File**: `path/to/file:42`
**Category**: [Security | Error Handling | Pattern | Test Gap | Data | Dependency]
**Issue**: [Specific description of what's wrong]
**Impact**: [What could go wrong]
**Fix**: [Exact steps to remediate]

## High (fix soon)
...

## Medium (should fix)
...

## Low (consider fixing)
...

## Summary

| Category | Critical | High | Medium | Low |
|----------|----------|------|--------|-----|
| Security | N | N | N | N |
| Error Handling | N | N | N | N |
| Patterns | N | N | N | N |
| Test Gaps | N | N | N | N |
| Data | N | N | N | N |
| Dependencies | N | N | N | N |
| **Total** | **N** | **N** | **N** | **N** |
```

## Phase 5: Issue Generation

After presenting the report, offer:

> I found N critical and N high findings. Want me to generate GitHub issues for them?

If the user accepts:
1. Generate one issue per critical/high finding using the `/cook` issue format
2. Group closely related findings into a single issue when they share the same root cause
3. Offer to create them with `gh issue create --label "o-ready"`
