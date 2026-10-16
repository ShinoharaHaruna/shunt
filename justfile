# shunt task runner. `just --list` shows all recipes.

# Auto-fix formatting: rustfmt + dprint (md/toml/json). The only
# write-to-source entry besides the editor.
fmt:
    cargo fmt --all
    dprint fmt

# Read-only format check (pre-commit hook, CI).
fmt-check:
    cargo fmt --all -- --check
    dprint check

# Rust static analysis (pre-push hook, CI).
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Tests (CI only — intentionally not in git hooks).
test:
    cargo test --workspace --all-features

# Full CI gate: format + lint + test.
check: fmt-check lint test

# One-time setup after clone: point git at the versioned hooks dir.
setup:
    git config core.hooksPath .githooks
    @echo "git hooks -> .githooks/ (pre-commit: fmt-check, pre-push: lint)"
