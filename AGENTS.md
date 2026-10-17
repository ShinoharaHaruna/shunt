# AGENTS.md

## shunt

Task-scheduling interface layer between AI agents and hourly-billed cloud GPU instances (AutoDL-style). One Rust binary runs on both sides: the dev machine (server: registry + spool + dispatcher) and each GPU box (client: filesystem queue daemon). Wire protocol is `ssh <box> 'shunt <cmd> --json'`.

## Tech stack

- Rust 2021, single crate, std-only (no async runtime)
- Deps: clap (derive CLI), serde/serde_json/toml (registry + wire), anyhow, thiserror, signal-hook, fs2 (flock)
- Build system: cargo

## Commands

Unified entry is `just` (`just --list`):

- `just fmt` — auto-fix: rustfmt + dprint (md/toml/json); the only write-to-source path besides the editor
- `just fmt-check` — read-only format gate (pre-commit hook)
- `just lint` — clippy `-D warnings` (pre-push hook)
- `just test` — cargo test (CI only, not in hooks)
- `just check` — full gate = fmt-check + lint + test
- `just setup` — after clone, points git at the versioned hooks dir (`core.hooksPath = .githooks`)

Raw cargo equivalents work too (`cargo build/test/clippy`).

## Repository structure

- `src/cli.rs` — full clap command tree; server vs client subcommands
- `src/agent/` — client side: daemon loop, FS queue ops, exec, probe, shutdown
- `src/server/` — server side: registry CRUD (real), spool, dispatcher, ssh transport
- `src/proto/` — shared types: task format, wire JSON (protocol version 1)
- `docs/design.md` — **authoritative behavioral spec**; when ambiguous, this file wins
- `etc/vendors/` — client templates (`autodl.toml` seeds AutoDL defaults)
- `task_template.sh` — guard-pattern task skeleton
- `justfile`, `dprint.json`, `rust-toolchain.toml`, `.githooks/` — toolchain
- `reference/` — bash prototype oracle (gitignored; contains deployment-specific details, never commit it)

## CI

Not configured — repo is unpublished. Git hooks + `just check` are the only gates for now; hooks and future CI only check, they never modify files.

## Conventions

- Spec-first: `docs/design.md` §3 catalogs the queue state machine's edge cases (orphan requeue, pgid isolation, idle-shutdown) ported from the bash oracle; implement to the spec, don't invent semantics.
- Skeleton phase: most handlers are stubs that print parsed intent and exit "not yet implemented". Registry CRUD and `mktask` are real.
- Client queue is a filesystem state machine: `pending/ running/ done/ failed/` under `$QROOT`; resolution order `--root` > `$SHUNT_ROOT` > configured qroot.
- Registry lives at `~/.config/shunt/` (`$SHUNT_HOME` overrides for tests), human-editable TOML, one file per client/project.
- Never put real hostnames, ports, or key paths in tracked files — use placeholders (see `etc/vendors/autodl.toml` FILL markers).
