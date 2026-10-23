# AGENTS.md

## shunt

Experiment operations layer for humans and AI agents across local workstations, lab servers, and cloud CPU or GPU machines. One Rust binary runs on both sides: the dev machine (server: registry + spool + dispatcher + coordination rooms) and each experiment machine (client: independent named filesystem queues and daemons). Remote wire protocol is `ssh <machine> 'shunt <cmd> --json'`; local clients use direct process execution. Billing metadata and idle shutdown are optional capabilities.

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

- `src/cli/` — full clap command tree (`args/`); server vs local-queue subcommand dispatch (`mod.rs`, `server/`, `local/`)
- `src/client/` — client (box) side: daemon loop, FS queue ops, exec, probe, shutdown
- `src/server/` — server side: registry CRUD (real; clients own named `[queues.<name>]` subresources — a queue holds qroot/env_setup/shell/exit_on_idle/workers while the client holds the SSH identity, tunnel, billing), spool, dispatcher, ssh transport, SOCKS5 tunnel manager, `room.rs` coordination rooms (design.md §17); `queue_ops.rs` = queue-scoped teardown + shutdown preflight, `registry/queues.rs` = resolution/mutation, `registry/names.rs` = typed-name resolution middleware (exact > case-fold canonical > suggestion-only)
- `src/feedback/` — GitHub-issue feedback channel: compose + sanitize, persist-first ledger, gh/curl delivery (design.md §14)
- `src/proto/` — shared types: task format, wire JSON (protocol version 1)
- `docs/design.md` — **authoritative behavioral spec**; when ambiguous, this file wins
- `etc/vendors/` — client templates (`autodl.toml` seeds AutoDL defaults)
- `task_template.sh` — guard-pattern task skeleton
- `justfile`, `dprint.json`, `rust-toolchain.toml`, `.githooks/` — toolchain
- `reference/` — bash prototype oracle (gitignored; contains deployment-specific details, never commit it)

## CI

`.github/workflows/release.yml` runs on pushed SemVer tags (`v0.1.0` or `0.1.0`), validates the tag against `Cargo.toml`, checks formatting and Clippy, tests both musl and GNU targets, and publishes Linux x86_64 binary archives with per-target checksums and generated notes. Prerelease tags create GitHub prereleases. Ordinary branch/PR CI is not configured. Git hooks and workflow checks never modify source files.

## Conventions

- Spec-first: `docs/design.md` §3 catalogs the queue state machine's edge cases (orphan requeue, pgid isolation, idle-shutdown) ported from the bash oracle; implement to the spec, don't invent semantics.
- Implementation phase: the tree is implemented end to end — registry CRUD, `mktask`, spool, dispatcher, ssh/local transports, tunnel, and the full client queue state machine (design.md §3). No stubbed handlers remain.
- Client queue is a filesystem state machine: `pending/ running/ done/ failed/` under `$QROOT`; resolution order `--root` > `$SHUNT_ROOT` > configured qroot. Server-side queue selection is `--queue` > `default_queue` > sole queue (design.md §4.1a); staged tasks pin `(queue, qroot)` — never retarget after the fact.
- Registry lives at `~/.config/shunt/` (`$SHUNT_HOME` overrides for tests), human-editable TOML, one file per client/project.
- Never put real hostnames, ports, or key paths in tracked files — use placeholders (see `etc/vendors/autodl.toml` FILL markers).
