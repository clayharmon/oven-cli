# Run the full CI pipeline locally (mirrors .github/workflows/ci.yml)
ci: fmt clippy test coverage deny
    @echo "all checks passed"

# Format check (nightly required for import grouping)
fmt:
    cargo +nightly fmt --all --check

# Format and fix
fmt-fix:
    cargo +nightly fmt --all

# Lint
clippy:
    cargo clippy --all-targets --all-features -- -D warnings

# Run tests
test:
    cargo nextest run --all-features

# Run tests with coverage (85% threshold)
coverage:
    cargo llvm-cov nextest --lcov --output-path lcov.info --fail-under-lines 85

# Dependency audit (licenses, advisories, sources)
deny:
    cargo deny check

# Quick check: fmt + clippy + test (skip coverage and deny for speed)
check: fmt clippy test
    @echo "quick checks passed"
