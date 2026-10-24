# shunt task runner. `just --list` shows all recipes.

# Auto-fix formatting: rustfmt + dprint (md/toml/json) + prettier
# (web/). The only write-to-source entry besides the editor.
fmt:
    cargo fmt --all
    dprint fmt
    pnpm -C web exec prettier --write .

# Read-only format check (pre-commit hook, CI).
fmt-check:
    cargo fmt --all -- --check
    dprint check
    test -d web/node_modules || { echo "web deps missing — run `pnpm -C web install` (or just web)" >&2; exit 1; }
    pnpm -C web exec prettier --check .

# Rust static analysis (pre-push hook, CI).
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Tests (CI only — intentionally not in git hooks).
test:
    cargo test --workspace --all-features

# Build the serve dashboard frontend into web/dist (embedded by
# build.rs on the next cargo build). pnpm install is idempotent.
web:
    pnpm -C web install
    pnpm -C web build

# Regenerate a shell completion script — the refresh path for
# installed files that go stale whenever the CLI grows.
# `just completions <shell> [dest]`: stdout, or install to DEST
completions shell dest="":
    #!/usr/bin/env sh
    set -eu
    if [ -n "{{dest}}" ]; then
        cargo run --quiet -- completions "{{shell}}" > "{{dest}}"
        echo "wrote {{dest}}"
    else
        cargo run --quiet -- completions "{{shell}}"
    fi

# Full CI gate: format + lint + test.
check: fmt-check lint test

# One-time setup after clone: point git at the versioned hooks dir.
setup:
    git config core.hooksPath .githooks
    @echo "git hooks -> .githooks/ (pre-commit: fmt-check, pre-push: lint)"
