# Contributing

Thanks for your interest in oven. Here's what you need to know.

## Setup

1. Install Rust 1.85+ (nightly toolchain also needed for formatting)
2. Install [cargo-nextest](https://nexte.st/) for running tests
3. Clone the repo and run `cargo build`

## Development

Format, lint, and test before opening a PR:

```sh
cargo +nightly fmt
cargo clippy --all-targets -- -D warnings
cargo nextest run
```

Or run everything at once:

```sh
just ci
```

## Pull requests

- Keep changes focused. One fix or feature per PR.
- Add tests for new functionality.
- Make sure CI is green before requesting review.
- Write a clear description of what changed and why.

## Reporting bugs

Open an issue with:

- What you expected to happen
- What actually happened
- Steps to reproduce
- Oven version (`oven --version`) and OS

## Code style

- No `unwrap()` outside of tests. Use `.context("...")?` instead.
- No `unsafe`. It's forbidden at the lint level.
- Follow existing patterns in the codebase.
- See `CLAUDE.md` for the full conventions list.
