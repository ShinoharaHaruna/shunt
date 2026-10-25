# shunt design spec

shunt is an experiment operations layer for humans and AI agents across local workstations, lab servers, and cloud CPU or GPU machines. It combines machine and project registration, independent named queues, persistent task staging, execution and resource visibility, and project coordination through one CLI. Remote machines use SSH; local clients use direct process execution. Billing metadata and idle shutdown are optional capabilities for deployments that need them.

The queue originated on hourly-billed cloud GPU instances (AutoDL-style: no systemd, containers that may vanish, billing tied to wall-clock uptime). That environment shaped its recovery and shutdown semantics; the current interface also supports owned, shared, and always-on machines.

The name is the rail-yard metaphor: the server is the marshalling yard, each client box is a destination siding, and `pending/`, `running/`, `done/`, `failed/` are the four tracks. Moving a task between tracks is a filesystem `mv`.

This document is the behavioral spec for the Rust rewrite. The proven bash implementation is archived under `reference/bash-queue/` and is the oracle for every semantic in §3. When behavior is ambiguous, the bash source wins.

## 1. Positioning and non-goals

shunt provides FIFO + priority-band + at-least-once experiment queues within a machine-management and project-coordination workflow. Humans and agents operate the same CLI, locally or over SSH, with structured state for automation. Named queues separate execution settings on one machine; coordination rooms let projects sharing compute record agreements (§17).

FIFO is the _claim_ order, not the execution shape: the client runs a concurrent worker pool (`workers`, live-tunable), so several tasks run in parallel and the daemon's job is to keep the box saturated, not serialized.

Non-goals, by deliberate exclusion:

- No DAG dependencies between tasks. Use Airflow.
- No automatic retry of arbitrary failures. A task opts into bounded rescheduling explicitly (`# shunt:retry=N` for rc 75 or recognized OOM signatures, §3.9); failures outside those criteria, timeouts, and exhausted retries land in `failed/` and the agent decides. Declared resource estimates are annotations for the agent's estimation loop, never admission gates (§3.16).
- No user-facing HTTP API. The client CLI is the API; SSH carries it.
- No multi-machine load balancing or automatic client selection. Task submission targets a registered client (explicitly or through `default_client`); matching task requirements to machine facts is the operator's or agent's job, with facts exposed via `shunt status --all --json`.
- No resource reservations or tenant isolation. Declared estimates and room agreements inform participants; the queue does not enforce CPU, RAM, or GPU allocations.
- No remote power-on. A human or an upstream automation makes the machine available. On AutoDL-class vendors this includes the console "power on" action. Idle poweroff is explicit opt-in and is refused server-side for clients marked `shared`.
- No non-Linux dev side. The server role is specified against Linux semantics — `/proc` liveness (§3.4), SIGUSR2/SIGTERM lifecycle (§3.5a), unix permission bits, ssh ControlMaster muxing, ETXTBSY-safe self-upgrade — and the single binary also carries the Linux-only client machinery, so a port would mean a platform abstraction plus module-wide `cfg` gating for convenience only. A Windows or macOS operator runs shunt inside WSL2 or on a remote Linux host instead.

## 2. Architecture

```text
dev machine (server)                         experiment machine (client)
┌──────────────────────────────────┐           ┌────────────────────────────────┐
│ shunt                            │           │ shunt (same binary)            │
│  registry/  clients + projects   │ SSH/local │  pending/ running/ done/       │
│  spool/     server-side staging  │ ────────► │  failed/ workers STOP          │
│  dispatcher spool → client       │  exec +   │  one daemon per named queue    │
│  rooms/     project coordination │ ◄──────── │  workers, logs, usage, probe   │
└──────────────────────────────────┘  JSON     │  optional idle lifecycle       │
                                               └────────────────────────────────┘
```

- One Rust binary `shunt` runs on both sides. Server subcommands operate the registry and spool; client subcommands operate the local queue.
- The wire protocol is SSH exec: the server runs `ssh <endpoint> 'shunt <subcmd> --json'`. No agent port, no daemon on the server side beyond the dispatcher loop, no extra process on the client beyond `shuntd` (the queue daemon embedded in `shunt`).
- Machines ↔ projects is many-to-many. A client (machine) owns named queues (§4.1a), each with an independent qroot, worker pool, daemon, binary, shell, and environment setup. A project binds to `(client, queue)` pairs via `[[bind]]` entries; the binding supplies `workdir` and the env overlay applied when a task runs.
- Client lifecycle: the machine is reachable → the dispatcher revives a stopped queue daemon before delivering work → tasks are claimed → on drain the daemon stays resident, exits if `exit_on_idle` is configured, or powers off the machine if idle shutdown was explicitly armed. No server-to-machine wake path exists.

## 3. Client queue semantics

This section specifies the state machine ported from `reference/bash-queue/bin/queue_run.sh`. Every rule below names a failure the bash version already survived.

### 3.1 Filesystem layout

Under `$QROOT` (client-local; resolved as `--root` flag > `$SHUNT_ROOT` env. Every wire op passes `--root <qroot>` from the client row, so the registry stays authoritative and the remote never resolves its own queue root):

| Entry                | Meaning                                                                                                                              |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------ |
| `pending/`           | `NNNN_<slug>/` entry dirs holding `task.sh`; claimed in name sort order                                                              |
| `running/`           | in-flight `NNNN_<slug>/` with `task.sh`, `log`, `pid`                                                                                |
| `done/`              | `NNNN_<slug>/` with `task.sh` + `log` + `exit` for rc 0                                                                              |
| `failed/`            | `NNNN_<slug>/` with `task.sh` + `log` + `exit` for nonzero rc or killed tasks                                                        |
| `workers`            | pool size file, re-read every poll cycle                                                                                             |
| `.workers.last`      | last resolved pool size, written by the daemon on change — the last-known fallback `status`/`workers get` shares (§3.8)              |
| `env`                | runner env dotenv: `WORKDIR`, `PATH_PREPEND`, `TASK_THREADS`, passthrough keys                                                       |
| `STOP`               | sentinel file: drain in-flight tasks, then daemon exits                                                                              |
| `shutdown_on_idle`   | flag file: power off once queue fully drains                                                                                         |
| `exit_on_idle`       | flag file: daemon exits cleanly once queue fully drains (§3.11a) — pushed from the client row                                        |
| `.shunt-owner`       | claiming client's ssh pub key line; `bootstrap` refuses a foreign-owned qroot so two installs never merge into one queue (§4.1)      |
| `shutdown_grace_s`   | countdown seconds (default 60)                                                                                                       |
| `shutdown_cmd`       | command exec'd at countdown end                                                                                                      |
| `daemon.pid`         | daemon pid, written at startup                                                                                                       |
| `daemon.ver`         | running image's version string, written at startup — the handoff capability gate (§3.5a): absent = the daemon predates SIGUSR2       |
| `daemon.bin`         | absolute path the running image was exec'd from, written at startup — diagnosis for `restart`/upgrade verification                   |
| `daemon.log`         | structured JSONL event stream (see §3.9); rotated to `daemon.log.1` past `SHUNT_DAEMON_LOG_MAX` (default 1 MiB), one generation kept |
| `.seq` + `.seq.lock` | enqueue counter under flock; a corrupt `.seq` fails the enqueue with remediation, never reseeds silently (§3.2)                      |

Per-entry sidecars beyond `task.sh`/`log`/`pid`/`exit`: `spawned_at` (unix seconds at dispatch, §3.7 — lets a handoff-adopted entry reconstruct its `timeout_s` deadline, §3.5a/§3.6), `usage` (measured resource peaks, §3.16), `attempts` + `retry_after` (reschedule bookkeeping, §3.9; `retry_after` lives in pending/ only and is removed on claim), `timeout` (budget-death marker, §3.9), `shunt_error` (quarantine note).

Every queue entry is a directory named `NNNN_<slug>` containing `task.sh` plus the sidecars it accumulates (`log`, `pid`, `exit`). Readers also understand the legacy flat layout (`NNNN_<slug>.sh` with `.sh.log`/`.sh.pid`/`.sh.exit` sidecars) so pre-existing queues need no migration; writers always create dirs, and `finalize` normalizes any legacy entry it touches. A suffixless flat file is NOT an entry — it is ignored entirely.

Upgrading in place over a legacy-format qroot is a supported path (observed end to end against the bash-era layout): orphan requeue, `pending/*.sh` normalization on claim, `.seq` high-water, and legacy `done/`/`failed/` listing all behave.

The daemon only manages tasks it dequeued itself, tracked via `running/*/pid`. Other jobs on the box are never touched. This boundary is a hard safety rule: every signal the daemon or `shunt kill` sends goes to a pgid recorded in a `pid` file, never to a name match.

### 3.2 Sequence numbers and the priority band

- `pending/` entry names are `NNNN_<slug>`, `NNNN` zero-padded to 4.
- `.seq` starts at 99, so the first enqueued task is `0100`. Values `0000`–`0099` are the manual priority band: `shunt prio` renumbers into it (numbers outside the band are rejected — renumbering into the normal sequence would corrupt it), and requeued orphans land at `0000_requeued_*`.
- `.seq` is read/incremented/written under an flock on `.seq.lock` (30 s wait, matching `flock -w 30`). Concurrent enqueues must never produce duplicate numbers.
- A missing `.seq` is rebuilt implicitly: the mint is `max(SEQ_INIT, high-water) + 1`, so the counter is repairable by deletion. A `.seq` that exists but does not parse as `0`–`9999` is operator damage — the enqueue FAILS with a remediation message (`remove it to rebuild from the high-water mark, or write a valid counter`), never silently reseeds over it.
- Hand-staged `NNNN_<slug>` names (any track, band or normal) are honored, never renumbered: the high-water floor guarantees the allocator never mints a taken number. A duplicate `NNNN_` across tracks is caught loudly at claim/finalize time by the quarantine paths (§3.7, §3.9).
- Claim order is `LC_ALL=C` filename sort, so `0000_*` always runs ahead of `0100_*`. The 4-digit namespace tops out at `9999`; the next number issued is the larger of `.seq + 1` and the high-water NNNN prefix across all four tracks (and their `.archive/`s) + 1, so a new entry always sorts behind everything queued and can never collide at claim or finalize time. When `.seq` itself has reached `9999` the counter is treated as wrapped: the next number restarts at high-water + 1 rather than minting a 5-digit name. True exhaustion — the high-water prefix is already `9999` — fails the enqueue ("sequence space exhausted") instead of issuing a number that would leapfrog the queue.

### 3.3 Enqueue

`shunt enqueue` (or the stdin form used over the wire):

1. Slug is sanitized to `[A-Za-z0-9_.-]` (anything else → `-`); an empty result — or one starting with `-` — is an error (the slug rides in remote argv where a leading dash would parse as a flag). The same check runs at `submit`/`new`/`mktask`/`enqueue`.
2. Take `.seq.lock` once — the whole of steps 3–5 is one critical section, so concurrent pushes of the same delivery serialize instead of racing the dedup scan.
3. **Delivery dedup.** If the body's comment head carries `# shunt:id=<server task id>` (§3.13), scan the four live tracks — in state-machine order pending → running → done → failed — for an entry whose `task.sh` head carries the same key. Scanning follows forward claim/finalize transitions into not-yet-scanned tracks; reschedule's backward running → pending move (§3.9) takes the same `.seq.lock` across its collision check and mutation so it cannot slip behind the scan. `.archive/` is out of scope (delivered + finished + archived inside one retry window is pathological, and a miss degrades to a guard-absorbed rerun). On a match the enqueue acks `{ok, name: <existing>, deduped: true}` and installs nothing — a re-push after a lost ack never mints a second number. Bodies without the directive (hand-written tasks, pre-dedup peers) skip the scan and always allocate.
4. Mint `NNNN` from `.seq`, write it back.
5. Install a `NNNN_<slug>/` entry dir holding `task.sh` (mode 0755) into `pending/` via a temp dir + `mv` inside `pending/` (atomic on the same filesystem). A remote `scp` + `mv` is _not_ atomic across filesystems; never install directly to the final name from an off-queue temp.

### 3.4 Daemon liveness

- Liveness is pidfile + `/proc/<pid>/cmdline` content match: read `daemon.pid`, `kill -0`, then check cmdline mentions the daemon identity. **Never use pgrep/pkill -f** — a remote shell whose command line happens to mention the pattern self-matches and gives false positives. The cmdline match string is a single named constant in the codebase (the bash version duplicated it in three scripts; that was a defect).
- Stale pidfile → treated as dead, removed, daemon may start.
- `daemon.ver`/`daemon.bin` are written beside the pidfile at startup and removed on the same exit paths; `status` reports their contents (`daemon.ver`, `daemon.bin`) so `restart` and `upgrade` can verify the running image instead of trusting that a signal landed.

### 3.5 Daemon loop

Poll cycle is 2 s (`POLL` env overridable for tests). Each cycle, in order: `reap` → if `STOP` present, drain; else `dispatch`, then check idle-shutdown. When a STOP drain completes with `shutdown_on_idle` armed, the daemon honors it before exiting (§3.10).

On startup:

1. Create the four track dirs.
2. Write `daemon.pid`.
3. Register the qroot in the box-level registry (§3.11).
4. Log `daemon_start` (with `handoff: true` when the `SHUNT_HANDOFF` env marker says this image arrived via §3.5a re-exec; the marker is consumed at startup).
5. Write `daemon.ver` + `daemon.bin` beside the pidfile.
6. Run `adopt_orphans` (§3.6).

On TERM/INT/HUP: log, send TERM to every tracked task pgid, wait 2 s, send KILL to any survivors, reap them into `failed/`, then exit 143 after removing `daemon.pid`, `daemon.ver`, `daemon.bin` and the registry entry — with one exception mirroring §3.10: if `shutdown_on_idle` is armed and `pending/` is empty, the daemon first honors the flag (grace countdown, then fire), because exiting without firing would orphan it just as in the STOP path; the pidfile and registry entry are kept for the countdown, and a further signal during it cancels the countdown and exits promptly. If `pending/` is non-empty the flag stays armed — powering off would strand queued work — and a `signal_left_shutdown_armed` WARNING is logged so the orphaning is loud; a restarted daemon drains the queue and honors the flag naturally. (TERM arriving as part of an external poweroff needs no special case: firing the shutdown command again is harmless.)

### 3.5a Handoff re-exec (SIGUSR2)

TERM/INT/HUP is the destructive exit path; SIGUSR2 is the graceful one — the binary-upgrade mechanism (`restart` below and §16 build on it). On USR2, at the next poll boundary (or the next countdown second — the flag is observed at the same points TERM is): log `daemon_handoff` (`{pid, exe, runpids}`), set the `SHUNT_HANDOFF=1` env marker, then `execve` the daemon's own argv against its resolved image path. The image path is `/proc/self/exe` readlink minus a trailing ` (deleted)` marker — after a stage-then-mv install the link names the old inode as deleted, and stripping recovers the canonical path where the NEW binary now sits.

- Continuity is structural, not negotiated: the pid never changes so `daemon.pid` and the cmdline liveness check stay valid; `daemon.lock` is an fd and survives exec, so there is no window in which a second daemon could start; the qroot registry entry, `STOP`, `exit_on_idle`, `shutdown_on_idle`, `workers` and all per-task state are on disk and re-derived by the new image's ordinary startup.
- Tracked tasks are NEVER signaled on this path — the only exit that leaves `running/` entries alive and managed. The new image adopts them through §3.6's ppid gate: children keep their parent across `execve`, so `ppid == self.pid` distinguishes handoff survivors from crash orphans (reparented to init), and `waitpid` still collects real exit statuses.
- A USR2 mid-STOP-drain or mid-countdown still execs: the on-disk state re-drives the same course in the new image. One documented drift: an interrupted grace countdown re-arms from full — the countdown is in-memory, the arm is on disk.
- An `execve` failure logs `handoff_failed` WARNING and the daemon keeps running the old image — the handoff flag is consumed, never re-tried in a poll spin, and never falls back to the TERM drain. `restart` reports the daemon still on its old version rather than claiming a handoff.
- USR2 sent to an image that predates the handler kills it outright (default disposition), leaving orphans — so the signal is only ever sent through `restart` after the `daemon.ver` gate below. A USR2 arriving in the exec→handler-install gap stays pending (the daemon blocks the signal in `sigprocmask` across the exec and unblocks it after the new image registers) rather than default-killing the fresh image; the pending signal then re-fires one harmless convergent exec.
- `shunt restart --root <q>`: liveness check (§3.4) → `daemon.ver` present → `kill(pid, SIGUSR2)`, reporting the outgoing version; absent `daemon.ver` → refuse (`the daemon predates handoff support — sending SIGUSR2 would kill it; upgrade the binary with shunt upgrade and retry, or stop+start`). No live daemon → falls through to `start`, which already launches the new binary. The dispatcher loop honors SIGUSR2 identically (§16).

### 3.6 Orphan adoption and requeue (at-least-once core)

Any `running/` entry present at daemon start means a previous daemon died mid-flight — or handed off (§3.5a). For each:

1. **Adopt the handoff survivors.** `pid` file present AND the §3.9a leader check passes (start_time-verified, self-grouped — a zombie leader still satisfies this, its stat readable until reaped) AND `/proc/<pgid>/stat` shows `ppid == this daemon's pid` → the orphan is a survivor of a handoff: children keep their parent across `execve`, while a crashed daemon's orphans reparent to init and fail this check. Rebuild the tracked handle: directives re-parsed from `task.sh`; `deadline` reconstructed as `spawned_at + timeout_s` in wall-clock terms (a missing or unparseable `spawned_at` — pre-sidecar entries, flat legacy entries — re-times the remaining budget from adoption, the pre-adoption behavior); the `usage` accumulator seeds from the entry's `usage` sidecar with `last_jiffies` baselined at adoption so `cpu_seconds` accrues only post-handoff deltas and peaks are never double-counted; the `pid` file stays valid untouched. Log `orphan_adopted {task, pgid}`. Reaping and timeout enforcement then run the ordinary paths — `waitpid` still works because the child relationship survived exec. A duplicate pgid across two adopted entries is operator damage: the second cannot be tracked, so it falls through to requeue below.
2. If the group is live but NOT adoptable — reparented crash orphans, live pgids failing the leader check, leader-reaped remnant groups — the old rule stands: log an `orphan_live_pgid` WARNING and requeue anyway; the task's own guard makes the rerun a no-op. Fully dead groups requeue silently.
3. Strip any stacked `0000_requeued_` prefixes from the basename in a loop — a task requeued twice must not accumulate prefixes.
4. `mv` the task to `pending/0000_requeued_<clean>` (front of queue).
5. Its `log` moves to `failed/<clean>.interrupted.<unixts>.log`.
6. Remove the `pid` file.

If the requeue `mv` fails (the `0000_requeued_<clean>` target already exists, permissions, full disk) the entry must not be left in `running/`: it is moved to `failed/` with a `shunt_error` note and a `requeue_failed` event is logged, because a `running/` entry with no `pid` file is invisible to `shunt kill` and would ghost forever. If even the quarantine move fails, a `ghost_entry` WARNING is logged.

Then sweep legacy strays: any remaining `running/*.pid` files are removed; any remaining `running/*.log` moves to `failed/<name>.interrupted.<unixts>.log`. Dir entries carry their sidecars inside, so they cannot stray.

### 3.7 Dispatch

- Read desired workers (see §3.8). While `len(running) < workers`:
  1. Snapshot the `pending/` list once per cycle (`LC_ALL=C` name sort) and walk it — re-reading the head after every failed claim would hot-spin on the same unclaimable entry for the whole poll.
  2. A rescheduled entry is not claimable until its `retry_after` timestamp passes (§3.9): not-yet-due candidates are skipped this cycle; an expired or unparseable sidecar counts as due and is removed on the way through.
  3. Claim by `mv pending/<f> running/<f>`. If the entry vanished mid-race (a concurrent `shunt cancel` won), skip to the next candidate — the `mv` result is the recheck, not a prior existence test. If the `mv` fails but the entry still exists (a name collision with a leftover `running/<f>` dir, permissions), the entry can never be claimed: log `claim_failed` and quarantine it to `failed/` with a `shunt_error` note rather than letting it poison every future poll's head-of-queue.
  4. `chmod +x`, write log header (`=== task`, `=== start <iso>`).
  5. Spawn via `process_group(0)` (setsid) so the task's pgid equals its leader pid; stdout/stderr append to `running/<name>/log`.
  6. Write the leader pid **and its /proc start_time** to `running/<name>/pid` (`<pgid> <start_time>`), record pgid → entry name. The start_time pin lets `shunt kill` distinguish the leader it spawned from a recycled pid (§3.9a below). Write `spawned_at` (unix seconds) beside it — the sidecar that lets a §3.5a handoff reconstruct the `timeout_s` deadline instead of re-timing it (dir entries only; flat legacy entries carry no sidecars).
- Each task runs in its own process group precisely so `shunt kill` and the daemon's signal forwarding can address the whole task tree by pgid without touching the daemon or sibling tasks.

### 3.8 Worker pool resolution

Re-read every cycle. One resolution path serves the daemon loop and `status`/`workers get` alike: `workers` file > `QUEUE_WORKERS` env (set at daemon start) > last valid value > built-in default 3.

- Digits are extracted (`tr -cd '0-9'` semantics); a non-numeric or missing file/env falls through to the next source. "Last valid" is the value the daemon last resolved — persisted at `.workers.last` on every change — so a garbage `workers` file makes `status` report that same last-known (source `last-known`), never a divergent `default`.
- Clamped to ≥ 1.
- Raising takes effect on the next dispatch. Lowering only stops new dispatches — running tasks finish naturally ("drain"), so shrinking never kills work. To free a slot now, use `shunt kill`.
- Status output must report the _resolved_ value and its source; never hardcode the default (the bash `qstatus.sh` printed `3 (default)` as a literal — a defect).

### 3.9 Reap and finalize

- Reap: for each tracked pid, if `kill -0` fails the child is gone; collect its exit status and finalize.
- Leader-exit vs group-drain: a reaped leader whose process group still has live members (detached grandchildren) is NOT finalized immediately — finalize waits out the group for a bounded grace (`SHUNT_PGID_DRAIN_S`, default 5 s), so the `pid` file keeps the pgid addressable to `kill` and the log stays put while anyone still writes to it. If the group is still alive at grace expiry the entry is finalized anyway and a `finalize_lingering_pgid` WARNING names the residual pgid for audit: survivors keep writing to the moved log through their inherited fd and are unreachable by `kill` from then on.
- Finalize entry `<name>` with rc:
  - append `=== exit <rc> <iso>` to the log;
  - remove the entry's `pid` file;
  - `mv` the whole entry dir to `done/` if rc 0, else `failed/` (legacy flat entries are normalized into the dir layout here);
  - write `<rc>` to `<name>/exit`;
  - if the task exceeded its `# shunt:timeout_s=` budget (§3.13): it lands in `failed/` even on a clean-exit race, the log gains a `=== timeout: exceeded wall-clock limit <n>s` line, and a `timeout` marker file carrying the limit is written beside `exit` so agents can tell budget death from a plain non-zero rc;
  - log `finish` (`{task, rc, dest}`).
- If the finalize `mv` fails, the entry is quarantined to `failed/` with a `shunt_error` note and a `finalize_failed` event is logged — same rule as §3.6: no pid-less ghosts in `running/`. If quarantine itself fails, a `ghost_entry` WARNING is logged.
- **Reschedule (rc 75 / EX_TEMPFAIL) runs BEFORE finalize.** A task that declared `# shunt:retry=N` and dies with rc 75 — or, via the bounded OOM heuristic below, any nonzero rc whose log tail matches a fixed OOM-signature list (`out of memory`, `OutOfMemoryError`, `CUDA_ERROR_OUT_OF_MEMORY`, `ALLOC_FAILED`, `cannot allocate memory`, case-insensitive) — is moved back to `pending/` under its SAME `NNNN_<slug>` name instead of landing in `failed/`:
  - the entry's `attempts` sidecar is incremented; at `attempts > N` the entry finalizes normally (budget exhausted — `retry=N` means N extra runs).
  - a `retry_after` sidecar (`unix now + # shunt:retry_delay_s=`, default 60 s) holds it unclaimable until then (§3.7 step 2) — without it a still-busy resource would burn the whole budget in one poll burst.
  - the `pid` file is removed; the failed attempt's `log` is swept to `failed/<base>.attempt<K>.log` (spawn truncates `log` on each run, so keeping it inside would destroy the evidence), and a `task_rescheduled` event is logged `{task, rc, via: rc75|oom_heuristic, attempt, retry_after}`.
  - eligibility deliberately requires BOTH a declared budget AND a reschedule-shaped rc: the OOM heuristic never applies to undeclared tasks, so a plain failure can never turn into a silent retry of a real bug — and rc 75 without `# shunt:retry=` is just another nonzero rc.
  - a requeue that cannot complete (lock acquisition failure, rename collision, fs error) falls back to normal finalize with a `reschedule_failed` WARNING — the entry never ghosts in `running/`.
  - same-name requeue preserves the `task show` ledger mapping and the entry's sorted position; it also keeps `# shunt:id=` delivery dedup honest (a re-push still finds the entry).
- Terminal tracks are capped: after each finalize, when `done/` or `failed/` exceeds `SHUNT_TRACK_KEEP` (default 500) entries, the oldest by name sort (sequence order) move to `<track>/.archive/` — dot-prefixed, invisible to every listing, preserved on disk. Without the cap a long-lived queue makes every `status` readdir grow without bound.

### 3.9a Signaling discipline (`shunt kill`)

Every signal goes to a pgid read from an entry's `pid` file — never a name match — and is gated on a group-identity check with two accepted proofs: (a) the leader check — `/proc/<pgid>/stat` shows the leader pid alive, self-grouped (`pgrp == pgid`), and with the start_time recorded at spawn; or (b) the remnant case — the leader pid is dead **but the process group still holds members**. A pgid cannot be recycled while its group is live, so a live remnant is provably the task's own leftover group (a leader that exits on TERM leaves detached grandchildren from `cmd &` behind, still in its group) and signaling it hits only the task's members. The signal is skipped only when the group is fully dead — there the pgid may have been recycled to a stranger — and when a live pid at `pgid` fails the leader check (the recycled case: a foreign leader took the pid). `kill` reports `gone` and sends nothing in the refused cases. The gate is re-run before the KILL escalation too. A `kill` result is honest about what happened: `signaled` (TERM, optionally escalated) vs `gone` (nothing sent; the entry will finalize on the daemon's next reap).

### 3.9b Bulk cleanup (`shunt clean`)

`cancel`/`kill` are single-entry ops by design; a queue that has piled up needs a bulk verb. `shunt clean` removes whole entry sets selected by flag: `--pending`, `--done`, `--failed`, `--archive` (the contents of `done/.archive/` and `failed/.archive/` — the §3.9 cap overflow, which otherwise only grows), or `--all` for all four. `running/` is never a clean target: in-flight work is §3.9a's signaling discipline, not janitorial. Bare `clean` selects nothing and refuses, printing per-track counts so the operator sees what each flag would take — a silent no-op would read as success and a default-on verb would be a footgun.

The sweep never deletes an entry in place. Each entry is claimed by atomic `rename` into a per-run trash dir `<qroot>/.clean-<pid>/<set>/<name>`, and the trash tree is `remove_dir_all`'d once at the end — the same rename-is-the-recheck discipline as the §3.7 claim. An entry the daemon claimed first (or a concurrent `cancel` took) yields ENOENT, counts as `skipped`, and is left alone; the loser of that race is never a half-deleted `running/` entry, which is the failure mode an in-place `rm -rf` would open. A flat legacy entry's `name.sh.*` sidecars move with it. The daemon needs no awareness: a vanished pending entry is the §3.7 cancel-won race, terminal-track removals are invisible to it, and the §3.9 archiver's own rename into `.archive` races identically — an entry archived mid-sweep simply escapes that pass. There is no per-entry rollback; every state the queue can be left in (removed, skipped, or named in `errors`) is reported, and one stubborn entry never wedges the sweep.

A `clean` killed mid-delete strands a `.clean-<pid>` holding doomed entries; the next `clean` run removes stale `.clean-*` dirs first (they are trash by construction), and the dot-prefix keeps them invisible to every listing in the meantime. Staleness is probed by pid — a `.clean-<pid>` whose pid is a live `shunt clean` belongs to a concurrent run and is left alone; dead, foreign, and unparsable names are swept. Clean is a sweep, not a fence: entries enqueued or finalized during the run may or may not be caught — re-run to converge. It does not stop the daemon or disarm `shutdown_on_idle`; clearing `pending/` is precisely how an armed flag's §3.11 condition becomes true.

### 3.10 STOP sentinel

If `STOP` exists: no new dispatches. When the running set empties, the daemon removes `STOP`, logs, and exits 0 — with one exception: if `shutdown_on_idle` is still armed (and `pending/` is empty), the daemon first runs the §3.11 grace countdown and fires the shutdown command, because the daemon is the only process that can honor an armed flag; exiting without firing would orphan the flag and leave the box billing idle forever. In this path `STOP` is already consumed and no longer cancels the countdown; a flag removal or a new `pending/` entry still can, and the daemon then exits 0 without firing. A task in flight when STOP appears finishes normally and lands in `done/` or `failed/` first. Immediate stop = `shunt stop` + `shunt kill`: the killed task lands in `failed/`, the daemon exits at the next boundary. The pidfile is kept for the duration of the honoring countdown (so `shunt shutdown off` still reaches a live daemon) and removed before exit.

### 3.11 Idle shutdown

When `shutdown_on_idle` exists AND `running/` is empty AND `pending/` is empty:

1. Arm: log `idle_shutdown_armed` with the grace length.
2. Count down `shutdown_grace_s` (file > default 60) one second at a time. Each second, re-check: flag removed → cancel; a new `pending/` entry → cancel; `STOP` sentinel → cancel and disarm (flag removed — without the disarm the STOP-drain honoring path below would fire it anyway). Exception: when the countdown is running as part of STOP-drain honoring, §3.10, `STOP` is already consumed and does not apply. Also each second: a busy sibling daemon → abort the countdown (`idle_shutdown_blocked`) _without_ disarming — the poll loop re-arms and retries once the box drains. Log every 10 s and for the final 5 s.

   A signal during the countdown ends it early: the daemon returns to the poll loop and the normal signal path runs — which, with the flag still armed and `pending/` empty, re-enters honoring through §3.5.

   **Box-level qroot registry.** Several daemons with different qroots can share one box (a second `--root` started by hand, future local multi-queue); the idle check must not power off while a sibling still has work. At start the daemon registers its canonical qroot path in a shared registry dir (`$SHUNT_STATE/qroots/` > `$XDG_STATE_HOME/shunt/qroots/` > `~/.local/state/shunt/qroots/` > `/tmp/shunt-$USER/qroots/`), one file per qroot keyed by a hash of its path, and removes the entry on every exit path (drain exit, signal, fire). A registered peer _blocks_ firing only when its daemon is live (§3.4 pidfile + cmdline discipline) **and** its queue holds pending or running entries; an idle live daemon does not block — a forgotten idle daemon must not pin the box up forever — and a stale entry is swept when met. The residual race mirrors the existing one: a sibling enqueue landing after the final re-check can still lose to poweroff. The registry is per-USER (`shunt-$USER`): two shunt installs under the _same_ unix account (the common shared-root-container case) coordinate correctly across their separate qroots, but daemons under _different_ unix accounts cannot see each other's queues — on a multi-account box the only safe posture is `shared = true` in the client row, which refuses the arm server-side (§4.1).

   Registry uncertainty is not idleness: directory open/iteration failures, unreadable or malformed peer records, and unreadable or malformed peer pidfiles block shutdown without disarming it. The daemon logs `idle_shutdown_blocked` with `why = "peer registry unavailable"`, the failing path/error, and a warning to repair the registry path, permissions, or records. A missing registry is legitimate only for an initial read before registration; a running daemon treats its disappearance as a scan failure. Registration failure logs `qroot_registration_failed` and refuses daemon startup (removing its pidfile) rather than running an invisible queue. Proven-dead peers and peers with genuinely missing pidfiles can still be swept; idle live peers still do not block. This cannot coordinate a daemon that predates registration enforcement and already failed to register, or close the final enqueue/poweroff race.

3. After the countdown, re-check once more: flag still present, pending still empty, running set empty, no busy peers. Only then exec the shutdown command.
4. Resolve the command at the last moment: `shutdown_cmd` file > `$SHUTDOWN_CMD` env > `/usr/bin/shutdown`. Last-moment resolution is what lets a test redirect the command to a marker — a test must never reach the real `/usr/bin/shutdown`. The `shutdown_cmd` file is written by `shunt shutdown on --cmd`, and the server-side arm (`shutdown -c <name> on`) pushes the client row's `shutdown_cmd` field every time, so the registry stays the source of truth — copy a row to a test env and its placeholder command comes along. `shutdown status` reports the resolved command before anything fires.

Residual race, documented in the bash source and retained: an enqueue in flight during the final instant (or an scp mid-copy) can still lose to poweroff. The last re-check narrows but cannot close it.

### 3.11a Idle daemon exit (`exit_on_idle`)

`exit_on_idle` is the residency preference: the client row's `exit_on_idle = true` is pushed to `<qroot>/exit_on_idle` at bootstrap / `client env`. On the same drain condition as §3.11 — tracked `runpids` empty AND `pending/` empty — but only when `shutdown_on_idle` is not armed (an armed shutdown wins: its countdown ends the daemon anyway), the daemon logs `daemon_exit_idle`, removes `daemon.pid`, deregisters its qroot, and exits 0 instead of staying resident. No countdown and no multi-tenant gate: this daemon serves only its own queue, a busy sibling is no reason to stay, and the next `status`/dispatch revives it through the auto-heal `shunt start`. For boxes shared with other users where a resident process is unwanted.

### 3.12 Task environment (exec wrapper)

Every task runs under a fixed prologue (the bash `qexec.sh`). The queue-level `env` file under `$QROOT` supplies defaults; per-task `# shunt:` directives in the task's comment head override them for that task only:

- `cwd` = task's `# shunt:workdir=` directive, else `env`'s `WORKDIR`, else `$QROOT` itself. If the workdir is unreachable the task fails rc 97 without running.
- `PATH` prepended with `env`'s `PATH_PREPEND`.
- All proxy variables unset: `ALL_PROXY all_proxy http_proxy https_proxy HTTP_PROXY HTTPS_PROXY`. A task that needs a tunnel re-exports it inside itself.
- `OMP_NUM_THREADS` / `MKL_NUM_THREADS` set to `env`'s `TASK_THREADS` (`auto` → cgroup probe, see §8). Rationale: AutoDL containers are cgroup-capped while `nproc`/`free` report host values — on RW-A, 14 real cores vs 112 reported. An uncapped torch process claims all of them and N workers thrash.
- Remaining `env` keys pass through as environment; per-task `# shunt-env:K=V` directives apply last and win.
- The `env` file is literal `KEY=value`, not shell: no quote stripping, no `$VAR` expansion, no escapes — `K="a b"` makes the quote characters part of the value (write `K=a b`; embedded spaces are preserved verbatim). The first `=` splits key/value; the KEY is whitespace-trimmed but the VALUE is passed through verbatim, surrounding whitespace included. `#` lines and blanks are skipped. The same literal rule applies to `# shunt-env:` directives — and since the first `=` splits, an env key can never itself contain `=`; `validate_meta` rejects such keys at submit because `# shunt-env:K=X=v` would silently arrive as key `K` with value `X=v`.
- `<qroot>/env_setup` is a client-row `env_setup` snippet pushed by `client env`/`bootstrap`: sourced in the wrapper right before exec, so conda-activate/PATH-export land in the task env, and `--file` tasks get it too (the wrapper, not the render, owns it). The wrapper's shell is `<qroot>/shell` or bash — record `shell = "zsh"` when the snippet isn't POSIX (conda activate under bare `sh` fails silently). A nonzero snippet return doesn't block; `exit`/`exec` inside it kills the launch — same footgun as bashrc.
- Then `exec <shell> <taskfile>`.

A task may override any of these inside itself. `set -uo pipefail` is part of the generated task template, not enforced by the runner.

### 3.13 Task file format and the guard contract

Generated task (single generator — `client::exec::render_task` — shared by `new`/`mktask` and server-side `submit`):

```bash
#!/usr/bin/env bash
# task: <slug> (generated by shunt <ts>)
set -uo pipefail
if [ -e <guard> ]; then echo "guard hit: <guard> exists; skipping"; exit 0; fi
exec <cmd...>            # args shell-quoted
```

A submitted task carries its binding inside the body, as comment-head directives the runner parses at spawn (the wire carries only the body, so metadata cannot travel beside it):

```bash
# shunt:id=deadbeef-01           # delivery id (submit only); enqueue dedups on it, runner ignores it
# shunt:workdir=/abs/path        # runner cwd for this task only
# shunt-env:KEY=value            # env overlay; beats the queue env file
# shunt:timeout_s=3600           # wall-clock budget; 0/absent = unlimited
# shunt:retry=2                  # reschedule budget on rc 75 / OOM heuristic (§3.9)
# shunt:retry_delay_s=60         # wait before a requeued task is claimable (default 60)
# shunt:gpu_mem_mb=1500          # declared estimates — ANNOTATION ONLY (§3.16)
# shunt:ram_mb=2048              #   nothing is gated on these; the agent refines
# shunt:cpu_cores=4              #   them against measured `usage` after each run
```

All are optional (`env` is repeatable); scanning stops at the first non-comment line, so hand-written tasks can set them too. An unrecognized `# shunt:` line is just a comment to older runners — new directives degrade harmlessly. `# shunt:id=` is consumed by the enqueue allocator (§3.3), not the runner; `submit` always injects it above the file's own head so a stale embedded id can never shadow the current delivery's (first match wins).

The timeout is a wall-clock budget counted from spawn, not a retry: at the deadline the daemon sends TERM to the task's verified pgid (§3.9a discipline), escalates to KILL after a 10 s grace, and finalizes per §3.9. A crash requeue re-times the rerun from its new spawn — at-least-once re-executes the whole task, budget included — but a handoff adoption reconstructs the ORIGINAL deadline from `spawned_at` (§3.6): a graceful restart must not become a way to stretch a task's budget. A guard-hit task exits before the budget can ever matter. No `cd` line — the runner already places cwd at `workdir` (the bash version's generated `cd` was redundant). The guard convention is the contract that makes at-least-once safe: tasks must be idempotent or guard on the artifact they produce.

### 3.14 Known defects in the bash source — fix in the port

| Defect                                                                                             | Required behavior                                                      |
| -------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `qprio.sh` uses `mv -f`: renumbering onto an existing `NNNN_slug` silently destroys a pending task | `shunt prio` must fail if the target name exists                       |
| `rwq_enqueue.sh` uses predictable remote tmp `/tmp/rwq_enqueue_$$.sh`                              | remote temp files come from `mktemp`                                   |
| `daemon_alive` duplicated in three scripts with the grep string inline                             | liveness check + identity string are single-sourced                    |
| `qstatus.sh` prints `3 (default)` as a literal                                                     | report the resolved workers value and source                           |
| task-wrap logic duplicated client-side (`rwq_enqueue`) and remote (`qnew`)                         | one generator: `shunt mktask` on the client; submit paths forward args |

### 3.16 Resource declarations and usage telemetry

The estimate/measure loop (the issue-#3 design): the server proposes a task with declared estimates (`# shunt:gpu_mem_mb=`/`ram_mb=`/`cpu_cores=`, set via `submit --gpu-mem-mb`/`--ram-mb`/`--cpu-cores`), the box runs it and measures actual peaks, `status` reports declared-vs-observed side by side, and the agent refines its next estimate against the diff. **Estimates are annotations, never gates** — an estimate that gated would be a wrong-guess amplifier (too low admits OOM anyway, too high waits forever); the protective mechanism is the reschedule signal (§3.9), which acts on the real failure, not the prediction.

- **Sampling.** Every poll cycle the daemon takes one Snapshot of `/proc` (one sweep, shared across the pool): pid → (pgid, utime+stime, rss) via `/proc/<pid>/stat` (comm may hold spaces/parens — parse from the last `)`), plus `nvidia-smi --query-compute-apps=pid,gpu_uuid,used_gpu_memory` mapped back to process groups for per-card VRAM attribution (unknown uuids are skipped, never guessed onto index 0). RAM peak = max summed group RSS; CPU = cumulative core-seconds (Σ group jiffies deltas, 100 Hz); VRAM peak = max per-card usage.
- **The `usage` sidecar.** Peaks are written into `running/<entry>/usage` (JSON `{peak_vram_mb:{<idx>:mb}, peak_ram_mb, cpu_seconds, samples}`, tmp+rename) every cycle, so the file travels with the entry to `done/`/`failed/` at finalize for free — the SAME file backs live status and terminal records. Peak-only, no time series: the agent needs the ceiling, not the curve.
- **status --json gains** `resources{gpus[]:{index,uuid,name,mem_total_mb,mem_free_mb,util_pct}, ram_total_mb, ram_free_mb, load1}` (host `/proc/meminfo` deliberately, not the cgroup view — on a shared box the contention that matters is foreign HOST use), `pending_detail`/`running_detail` (`{name, declared, usage}`; the `pending`/`running` name lists stay for order-only consumers), and `usage`+`declared` on `done_recent`/`failed_recent` ExitRecords. All serde-defaulted — older remotes simply omit the fields.
- **GPU placement** needs no machinery: `# shunt-env:CUDA_VISIBLE_DEVICES=<idx>` (or `submit --env`) already lands in the task's environment; the agent picks a card from `resources.gpus[]` at submit time. Multi-GPU attribution is per-card in `peak_vram_mb` — no `gpu_id` directive exists because placement is env data, not shunt policy.
- **Residual limits:** GPU SM% is not attributable per process (reported per card only); cpu_seconds loses children that die unsampled; usage files exist only on dir-layout entries; `.archive/` entries are never detail-scanned; a task rescheduled past its budget lands in `failed/` normally — reschedule is not infinite.

## 4. Server: registry, spool, dispatcher

### 4.1 Registry layout

`~/.config/shunt/` (XDG; `$SHUNT_HOME` overrides for tests):

```text
server.toml            # global defaults (poll interval, default client)
                       # default_client fills in -c/--client: required for client-only
                       # commands (submit, tunnel); for dual-mode commands it applies
                       # only when no local queue is in scope (--root/$SHUNT_ROOT unset)
clients/<name>.toml
projects/<name>.toml
keys/<name>[.pub]      # dedicated ed25519 keypair per ssh client
known_hosts/<name>     # shunt-scoped TOFU store, per client (never ~/.ssh)
cm/<name>.sock         # ControlMaster mux socket per client
state/<name>.fail      # consecutive ssh connect failures (endpoint-drift watchdog)
spool/                 # server-side staging queue
```

Registry files are human-editable TOML and safe to keep in git (minus key paths). "Registration" is server-local bookkeeping: `shunt client add` writes the row with zero network traffic. A client never needs to reach the server.

**Typed name resolution.** Wherever a name is typed to pick an existing registry identity — `-c`/`--client`, `--queue`, `--project`, `project bind <client>`, a room name, `room create --members`/`--clients` entries — it resolves through one ladder: an exact hit wins; a unique case-insensitive hit substitutes the canonical registered spelling; otherwise the name errors out carrying _suggestions_ — prefix and small-edit (Levenshtein, budgeted ≤1 for names up to four chars, ≤2 for longer) candidates — which are never auto-accepted. Several case-variant candidates are a loud ambiguity, never a coin flip. A canonical substitution announces itself on stderr (`note: resolved 'rwx' → client 'rw-x'`): picking a box to talk to deserves a spoken word. `room --as` labels are the exception — they canonicalize silently (§17.3). A configured `default_client` is data, not typed input — it stays exact-matched. Room names keep their older unique-prefix acceptance on top (§17.2); task slugs (`cancel`/`prio`/`kill`) and ledger/task ids keep their own substring and prefix matching — different semantics, deliberately outside this rule. A 0-match refusal still names the pattern's hits in the tracks the op does not act on (`pending` hits point at `cancel`, `running` hits at `kill`, terminal tracks are named plainly): the claim race moves a task out of `pending/` mid-typing, and a bare "no pending task matches" would read as a matcher defect rather than a moved task.

**Permissions are enforced, not assumed.** `ssh.password` is plaintext and `keys/` holds private keys, so every shunt-managed dir under `$SHUNT_HOME` is created/re-tightened to `0700` and every file (rows, spool entries, state, tunnel pid/port/log) to `0600` — umask never widens the registry, and saving re-tightens a file a user loosened. Paths outside `$SHUNT_HOME` (a user-supplied `ssh.key`) keep their own permissions.

**SSH is fully managed.** `client add`/`bootstrap` generate a dedicated ed25519 keypair at `keys/<name>` and fill `ssh.key` — registration alone makes the row usable. Deploying the public key is the one step that needs existing trust: when `ssh.password` is set, `bootstrap` appends the pubkey via a single password-only sshpass session (no key offers, idempotent `grep -qxF`); without it, bootstrap prints the `.pub` to install once via the vendor console or a manual `ssh-copy-id`. **A `shared` row never auto-deploys**: authorized_keys is the one write shunt makes outside `<qroot>`, and other tenants' consent is not ours to assume — point `ssh.key` at an already-authorized key or install the `.pub` manually. Host-key checking is TOFU (`StrictHostKeyChecking=accept-new`) against a per-client `known_hosts/<name>` file, so first contact never prompts and the user's own `~/.ssh` stays untouched. Per-client files are load-bearing: two rows may legitimately point at the same host:port (different boxes behind one NAT endpoint) while the boxes carry different host keys — a shared file would pin the first TOFU'd key and deadlock the second client on `REMOTE HOST IDENTIFICATION HAS CHANGED`.

**qroot ownership claim.** Two independent shunt installs pointed at the same `<qroot>` silently merge into one queue: sequence numbers race between two dispatchers and one daemon ends up serving both. Before any write, `bootstrap` runs a single remote claim script on `<qroot>/.shunt-owner` holding the client's ssh public key line (read from `{ssh.key}.pub` or derived via `ssh-keygen -y`): absent → create-exclusive via `set -C` (`O_EXCL`, so a simultaneously claiming bootstrap cannot interleave — a later install never preempts an earlier owner); present → `grep -qxF` compare. A foreign owner answers FOREIGN and bootstrap fails with "claimed by a different shunt install — pick a different qroot"; OWNED (self, the re-bootstrap/upgrade path) and CLAIMED proceed. Rows without a derivable pubkey skip the claim rather than weaken it. `client add` also warns when another row already targets the same host:port+user+qroot tuple, surfacing the collision at registration. This is an accident guard, not a security boundary — any tenant can `rm` the file; it exists to make collisions loud, not to fence out a deliberate claimer. Same-account multi-install is the supported topology (§3.11 registry coordinates it); cross-account sharing is the `shared` row's business.

**Host-key rotation** (rebuilt containers change their key): `shunt client hostkey <name>` runs `ssh-keygen -R` scoped to that client's `known_hosts/<name>` and kills the mux master, so the next connect re-TOFUs cleanly. Anything — agent or human — must never write to `~/.ssh/known_hosts`; a stale entry there surfaces as an unexplained REMOTE HOST IDENTIFICATION HAS CHANGED on the user's own connections.

**Endpoint drift** (vendors that reassign IP/port across restarts — `endpoint_rotates = true` in the row): `shunt client update <name> --host <h> --port <p>` rewrites the row AND clears the live state that would stay pinned to the dead endpoint — known_hosts entries under both the old and the new host:port spec (a reassigned tuple can carry a foreign key that hard-fails TOFU), the mux master (it answers for its original destination regardless of the requested target), the conn-fail streak, and any recorded tunnel (with a best-effort ALL_PROXY cleanup over the new endpoint). Hand-editing the row does none of this. Also updatable: `--user`, `--password` (empty clears), `--endpoint-rotates` (`--no-endpoint-rotates` clears a mislabeled row), `--pre-connect` (empty clears). Server-local: no reachability check; verify with `status -c`. Three reminders update emits rather than fixes: a `--user` change alone keeps the deployed pubkey working (authorized_keys is user-independent) but a rebuilt container resets it — re-run `bootstrap` in that case; a new endpoint already registered on another client row warns (legal behind NAT — per-client known_hosts keeps the keys apart — but a genuine duplicate should be `client rm`'d); and non-endpoint fields (`qroot`, `deploy_root`, `expire`, `env_setup`) are deliberately out of scope — hand-edit the row, remembering a moved `qroot` means relocating the remote queue, not just a string. Row writes are atomic (temp + rename), so a concurrent `dispatch` sweep never reads a torn TOML; the residual ordering hazard — a sweep connecting between the row save and the known_hosts/mux clearing — is why `update` should not run concurrently with a dispatch loop.

**Lifecycle exits.** `client rename <old> <new>` moves the whole identity (row, managed keypair, mux socket, conn-fail streak, tunnel state, queued spool refs, project `[[bind]]` rows). `client rm <name>` deregisters: remotely it stops the daemon and revokes our authorized_keys line — matched by the key blob, not its comment, so a renamed client still revokes cleanly — then locally removes row, keys, mux socket, state, spool entries, and bindings. `--purge` additionally `rm -rf <qroot>` on the box — the qroot is validated first (absolute, ≥2 components, no system dir / home dir / `~` / `..` / suspiciously short path) and a bad value aborts the deregistration; bound workdirs are reported, never deleted — task data is the user's call. Remote steps are best-effort: an unreachable box warns but never wedges the deregistration. Ad-hoc box probing goes through `client exec <name> -- <cmd>` (managed ssh: mux + retries + our key/known_hosts).

**Connection discipline.** Every ssh/scp exec rides a ControlMaster mux (`cm/<name>.sock`, `ControlPersist=60`) — one TCP session per client instead of one handshake per op. Boxes fronted by startup-limited proxies (observed: `Exceeded MaxStartups` on a mainland vendor's ssh gateway) flap hard under per-op connections; muxing removes the storm. rc-255 failures also retry 3× inside a call — exec and scp (`copy_to`) alike — so a single flap never surfaces; only an exhausted streak feeds the endpoint-drift counter. The deploy path deliberately keeps `ControlPath=none` — a password-auth session must never become the mux master.

**Reachability precondition (`pre_connect`).** A client may sit behind a local precondition ssh cannot express — a VPN, an overlay interface, a port-forward that must exist before the endpoint is routable at all. `pre_connect` is an optional per-client shell snippet (`sh -c`, stdin null, ambient env inherited) that runs **on the dev machine** to enforce it. It is failure-triggered, never on the healthy path: inside an exec/`copy_to` call it runs after the first rc-255 — once per call — and the remaining retries ride whatever it established; in `tunnel up`, whose `ssh -N -R` has no retry loop, it runs when the first spawn dies during setup and that spawn is retried once. The one eager case: a call that starts with a nonzero conn_fail streak already knows the endpoint is down, so the hook runs before the first attempt rather than after another wasted ConnectTimeout. The hook gets its own deadline (`SHUNT_PRE_CONNECT_TIMEOUT_S`, default 60 s — a bring-up legitimately blocks until connected) and is killed as a process group on expiry. Its result never gates the retries — a half-established precondition still deserves them — but it is reported: the final unreachable error carries `[pre_connect rc=N: <stderr tail>]` / `timed out`, so the remediation attempt and its complaint are visible instead of a bare 255. Contract: the snippet must be idempotent (an unreachable endpoint re-runs it once per sweep, and parallel `status --all` probes may run it concurrently) and non-interactive — stdin is null, so anything that prompts (passwords, OTP) is out of scope; for those, a hook that only CHECKS the precondition and fails fast with its own remediation message is the right shape. A precondition needing a resident process must daemonize or hand off to a supervisor (`nmcli`, `systemd`, `autossh`, `openconnect -b`, `setsid`): the snippet's job is ensure-and-return — arranging and supervising whatever keeps it alive is the user's own deployment, never shunt's, and a foreground keepalive is killed with the hook's process group at the deadline, accomplishing nothing. Non-goals: not a ProxyCommand or bastion mechanism (jump hosts are an ssh-layer `-J` concern — a different field if ever needed), not remote-side environment (`env_setup`, §3.12), and not fired on wire errors, remote nonzero rc, or deadline-hit hangs — strictly the ssh layer. Firing is tied to attempted contact — there is no idle watchdog: with nothing calling the client, a broken precondition is simply never observed until the next attempt; and a connectivity loss that surfaces as an exec deadline rather than rc-255 skips the hook for that call, firing normally on the next. Ignored on `local` rows. Set via `client add`/`update --pre-connect` (empty clears) or by hand.

**File-copy protocol.** `copy_to` explicitly uses legacy SCP (`scp -O`) with a shell-quoted remote path, rather than depending on the installed scp's default protocol. This preserves compatibility with older remote SCP peers and keeps spaces, shell substitutions, and apostrophes literal; shell quoting is not valid for default-SFTP scp, which preserves the quote characters as part of the path. The server-side `scp` must support `-O`, and the box must provide a legacy SCP sink and a POSIX-compatible login shell; local scp implementations without `-O` and SFTP-only endpoints are unsupported. There is no automatic protocol fallback: an unsupported option or sink fails the copy rather than weakening quoting. Mux, keepalives, hard deadlines, and rc-255 retries are unchanged.

**`localhost` is a pre-registered peer client, not a special case.** The dev machine is a real execution target (light tasks, tests, CPU-only work): it binds in projects, takes submissions, runs the daemon, and answers every wire op exactly like a remote box — the only difference is the transport. `local = true` in a client row selects the loopback transport (exec the local `shunt` binary instead of ssh); `localhost` is simply that transport's conventional reserved name, and `client add <name> --local` can mint more local queues. The first access materializes `clients/localhost.toml` with the defaults (`qroot=$SHUNT_HOME/queue`, `deploy_root=$HOME`), so a fresh install still runs the whole submit → dispatch → execute → finalize path with zero setup — and afterwards the row sits on disk, editable like any other: `local_bin`, `timeout_s` and friends override the loopback transport. The write is best-effort: an unwritable HOME still yields working defaults. `client rm localhost` resets to factory — the row regenerates on next use; a user who truly wants it gone writes `disabled = true` into the row (that file is never auto-rewritten, so the opt-out sticks).

`clients/<name>.toml`:

| field                                                         | meaning                                                                                                                                                                                                                                                            |
| ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `name`                                                        | registry key                                                                                                                                                                                                                                                       |
| `local`                                                       | loopback transport instead of ssh (localhost and any `client add --local` row)                                                                                                                                                                                     |
| `local_bin`                                                   | local transport: binary to exec (default: the running `shunt`)                                                                                                                                                                                                     |
| `bin_path`                                                    | explicit local path for the binary `bootstrap` ships (e.g. a musl cross-build for an old-glibc box); the auto source is `current_exe()` (§15). Settable at `client add --bin-path` or by hand                                                                      |
| `timeout_s`                                                   | local transport: exec timeout; on expiry the child is killed                                                                                                                                                                                                       |
| `ssh_timeout_s`                                               | ssh transport: ConnectTimeout override (default 15)                                                                                                                                                                                                                |
| `pre_connect`                                                 | dev-machine shell snippet enforcing a reachability precondition (§4.1): `sh -c`, fires once per call after the first rc-255 — or before the first attempt while the conn_fail streak is nonzero — never gates retries, outcome reported in the unreachable error   |
| `shutdown_cmd`                                                | poweroff command pushed to `<qroot>/shutdown_cmd` at `shutdown on` arm time — point at `true` or a marker script in test envs                                                                                                                                      |
| `disabled`                                                    | opt-out: row stays on disk but is not a usable client (skipped by listings, rejected by load)                                                                                                                                                                      |
| `ssh.host`, `ssh.port`, `ssh.user`, `ssh.key`, `ssh.password` | exec endpoint; `password` feeds the one-time `sshpass ssh-copy-id` key deploy                                                                                                                                                                                      |
| `deploy_root`                                                 | vendor work root (AutoDL: `/root/autodl-tmp`)                                                                                                                                                                                                                      |
| `facts`                                                       | table filled by `probe`/`bootstrap`: `gpu`, `cuda`, `driver`, `cores`, `mem_gb`, `disk_free_gb`, `python`, `can_root`, `shutdown_cmd` (poweroff candidate)                                                                                                         |
| `expire`                                                      | billing lifetime: `unlimited` (owned box, localhost), `metered` (pay-as-you-go until balance), or an ISO-8601 deadline — write it WITH an offset (`+08:00`/`Z`): a bare datetime has no zone, so `status` cannot turn it into `expires_in_s` (§9). Empty = unknown |
| `endpoint_rotates`                                            | vendor reassigns IP/port across restarts (AutoDL does not); steers the escalation hint after `CONN_FAIL_ESCALATE` consecutive ssh failures                                                                                                                         |
| `shared`                                                      | consent mark: the box hosts other people's work — server-side `shutdown on` refuses to arm idle poweroff while set (`client add --shared`); clear the field only when the user confirms sole use                                                                   |
| `billing.hourly`, `billing.currency`                          | cost metadata surfaced to the agent                                                                                                                                                                                                                                |
| `default_queue`                                               | execution default: which registered queue receives unqualified server queue ops and `submit`; absent = the sole queue, else explicit `--queue` is required                                                                                                         |
| `queues.<name>.qroot`                                         | queue dir on the box for named queue `<name>` — required, absolute, non-overlapping with sibling queues (legacy rows keep top-level `qroot` as the literal `default` queue)                                                                                        |
| `queues.<name>.env_setup`, `queues.<name>.shell`              | per-queue task preamble: snippet sourced before every task + the shell it runs under (default bash); pushed to `<qroot>/env_setup`/`shell` by `client env`/`bootstrap` or `queue env`/`queue bootstrap`                                                            |
| `queues.<name>.exit_on_idle`                                  | per-queue residency preference pushed to `<qroot>/exit_on_idle` (`client add --exit-on-idle` / `queue add --exit-on-idle`): daemon exits on drain instead of staying resident (§3.11a)                                                                             |
| `queues.<name>.workers`                                       | optional configured worker-pool preference pushed by `queue add --workers`/`bootstrap`; absent = the queue's runtime `workers` file stays authoritative                                                                                                            |

`projects/<name>.toml`:

```toml
name = "riboworld"
desc = "RiboWorld RNA-seq pipeline" # helps agents pick the right project
repo = "~/Code/RiboWorld" # canonical checkout on the dev machine
# (path or git URL; a future rsync source)

[defaults]
env = { TASK_THREADS = "4" } # merged UNDER every binding's env

[[bind]]
client = "rw-a"
queue = "train" # named queue on rw-a; omitted in old rows means "default"
workdir = "/root/autodl-tmp/RiboWorld" # repo location on the box
env = { PATH_PREPEND = "/root/.local/bin" }
[[bind]]
client = "rw-a"
queue = "eval"
workdir = "/data/RiboWorld"
```

`[defaults].env` merges beneath each binding's `env` at submit (binding wins per key). `repo`/`desc` are documentation fields today — `project add --repo --desc` fills them, `project ls` prints the desc column — and `repo` is where a future `shunt sync` would pull task code from. The remote checkout path stays a per-binding `workdir` concern; different boxes mount differently.

### 4.1a Named queues within a managed instance

A **client** identifies one managed execution instance and one SSH identity, not a queue. Endpoint, credentials, host-key store, mux socket, tunnel, connection-failure state, reachability precondition, billing, expiry, shared-box consent, and machine facts belong to the client. A **queue** is a named subresource with its own qroot, tracks, sequence allocator, workers, environment, binary, and daemon. A project binds to `(client, queue)`; several bindings may share a client but not the same pair. This section supersedes the single-queue assumptions in §4.1 and the older CLI examples below.

Canonical client rows store queues in `[queues.<name>]` tables and optionally set `default_queue`. Each queue owns `qroot`, `env_setup`, `shell`, `exit_on_idle`, and an optional configured `workers` preference. Existing flat rows remain readable as a literal queue named `default`; explicit queue mutations normalize them without moving any filesystem state or changing SSH keys or `.shunt-owner` claims. Canonical and nonempty legacy queue configuration must not coexist. Queue qroots within one client must not overlap. Queue selection is explicit `--queue` first, then configured default, then the sole queue; ambiguity and dangling references are errors rather than guesses. A typed `--queue` runs the §4.1 name ladder (case-folded hits substitute with a note). A missing queue in an old binding or task record always means the literal `default`, never a newly selected default.

`client bootstrap` and `client env` apply to all registered queues; `queue bootstrap` and `queue env` apply only to the named queue. `queue add` refuses an existing name; `queue set` is the single-field update path — patch semantics (only passed flags change; empty `--env-setup`/`--shell` clear, `--no-exit-on-idle`/`--no-workers` clear), server-local like `add`, whole-row revalidation before writing, and a byte-identical file on a no-diff call. Registry changes reach a live box only through `queue env` (env_setup/shell/exit_on_idle/workers), and a changed qroot retargets wire ops while a live daemon and staged work stay pinned to the old root — `set` names the push or relocate follow-up rather than implying the change propagated. Queue lifecycle operations never revoke the client key or destroy shared connection state. Removing a queue requires a different configured default, at least one remaining queue, and no project binding, staged work, or known unfinished task targeting it. Client removal decommissions all registered queues and revokes its key only once. Queue names are stable; automatic queue renaming, endpoint-based client merging, and automatic merging of previously independent owner identities are not supported.

Submission resolves a concrete queue and records its name and qroot in the spool and task ledger. Dispatch probes and heals daemons independently per `(client, queue)`, while connection backoff and tunnel lifecycle remain client-scoped. A changed qroot is reported and staged work is retained, never silently delivered elsewhere. Old records remain compatible with the literal `default` queue. Task ledger `queue_name` remains the task entry basename (`NNNN_<slug>`), distinct from the named queue identity. Aggregate status reports each client's machine metadata once and each queue's own reachability and runtime state.

One client tunnel reconciles `ALL_PROXY` across its registered queues, including queues added after tunnel creation. Workers and daemons remain independent; there is no instance-level supervisor or resource-aware admission. A server-side shutdown arm refuses a shared client and must preflight all registered sibling queues, refusing busy or unknown state, including stopped queues with pending work. This is **not** an atomic instance-wide drain guarantee: the existing remote registry covers live same-account daemons rather than a persistent authoritative inventory, enqueue may race shutdown, and other registries, users, and unmanaged workloads are outside this model. Direct `--root` queue behavior is unchanged. Operators must not infer permission to power off a shared instance from an empty set of managed queues.

### 4.2 Vendor templates

A vendor template is a plain client-row TOML used as a seed for `client add` — no special syntax, FILL markers are just comments. Search order, first hit wins: `$SHUNT_VENDOR_DIR`, then `$SHUNT_HOME/vendors/` (user-supplied templates live with the registry they seed), then the repo's `etc/vendors/` (a compile-time path that only resolves for builds from the source tree). `shunt client add --template autodl` seeds `ssh.user=root`, `deploy_root=/root/autodl-tmp`, and the initial queue's `qroot=/root/autodl-tmp/queue`, leaving only `ssh.host`, `ssh.port` (and optionally `ssh.password`, `billing`) to fill — `ssh.key` is generated automatically. `client add` normalizes the seeded flat fields into `[queues.default]` (or `--queue <name>`). Templates are defaults for the row file, nothing more — after `add`, the client row stands alone. A bad name errors out listing every template the search dirs actually contain.

### 4.3 Box-side SOCKS5 tunnel

Boxes with poor international connectivity (mainland-China GPU vendors) get their work done through a reverse dynamic forward: `ssh -N -R <port>` opens `127.0.0.1:<port>` as a SOCKS5 listener **on the box**, tunneled back to exit via the dev machine's network. `shunt tunnel up -c <name> [--port 1080]` spawns that ssh detached (pidfile + log under `tunnels/`), then upserts `ALL_PROXY=socks5h://127.0.0.1:<port>` into the box's `<qroot>/env` — tasks pick the proxy up with zero task-level edits, because the runner strips ambient proxy vars and then injects the env file. `tunnel down` kills the ssh and removes the env line; `tunnel status` reports liveness and the port. The `h` in `socks5h` makes DNS resolve remotely too, which is the point — a poisoned resolver stays unused. ssh-side prerequisites: OpenSSH ≥7.6 client and `AllowTcpForwarding` on the box's sshd (on by default).

**Transactions.** `up`, `down`, and `heal` serialize the complete local-state and remote-env transaction under a per-client `tunnels/.<name>.lock` flock (private `0600`, bounded 30 s acquisition). State readers take the same lock so they cannot combine a PID from one transaction with a port from another; list discovery is advisory, not a global snapshot. Lock files persist to keep their inodes stable for waiters. A repeated `up`, even with a different requested port, returns the existing identity-verified live tunnel without spawning another. Failed setup or state publication kills and reaps the newly spawned SSH before releasing the lock; a surviving port-only record remains eligible for self-heal. `down` verifies command identity before signalling and waits for exit (TERM, then KILL after 2 s) before removing records. These are cooperating-process transactions, not crash-atomic disk or remote-env commits; an env-write failure retains the existing warning/self-heal behavior.

**Self-heal.** The tunnel's own ServerAlive policy kills a half-open ssh in ~90s, which is the common death: the ssh dies while the box and its queue daemon stay up — and `ALL_PROXY` left in `<qroot>/env` then poisons every subsequent task into a dead proxy (failures surface as generic network errors). So the dispatcher sweep treats a recorded tunnel (pid or port state file) as a liveness responsibility alongside the daemon, even when the spool is empty; unused registry clients without a tunnel record and local clients without spooled work are not probed. On any sweep where the client probe succeeded (`Reach::Up`), a recorded-but-dead tunnel triggers one reconnect attempt via `tunnel up`; if the reconnect fails, ALL_PROXY is stripped from the env file (fail-over to direct egress) while the state files stay, so the next successful-probe sweep retries. Sweeps where the probe is skipped by conn-fail backoff also skip the tunnel check — the throttling is never bypassed — and a dead box is untouched entirely (an env edit needs ssh anyway, and no tasks can run). `tunnel status` reports this intermediate state as `stale`: pid dead, state files present, env possibly still poisoned.

### 4.4 Spool and dispatcher

`submit` lands a task in `spool/` as `spool/<task-id>.json` (metadata: project, client, slug, guard, argv, submitted_at) + `<task-id>.sh` (rendered task body). Persisted before returning, so submitting while the box is off is normal operation.

**`--file` submissions get the same binding injection as argv submissions.** The binding's `workdir`/`env` are merged into the file's comment head as `# shunt:workdir=` / `# shunt-env:` directives, injected _above_ the file's own directive block so the file's own directives win per key (the file is the task author speaking; the binding is context). An explicit `--guard` flag injects the guard line ahead of the file's first code line, so the flag always runs first — an embedded check still runs after it, harmlessly.

**The guard contract is enforced by warning, not by flag.** A staged task whose body carries no guard check (no `--guard`, no embedded `guard hit:` line in a `--file`) triggers a loud stderr warning at submit: at-least-once means a lost ack, an orphan requeue, or a live-pgid requeue reruns the body, and an unguarded task then double-executes (double GPU burn, duplicated result writes). Naturally idempotent tasks are a legitimate unguarded case, so submit still proceeds.

A second advisory fires the same way: a `-p`-less submit aimed at a `(client, queue)` pair that one or more projects bind carries no binding, hence no workdir/env — for `--file` and argv forms alike. Submit names the bound projects on stderr (issue #14's trap: the staged `--file` body then looks like it dropped `# shunt:workdir=` when the real cause is a missing `-p`); the note never gates, and a genuinely unbound pair stays quiet.

Two-phase dispatch, at-least-once end to end:

1. Dispatcher (server-side loop, poll interval from `server.toml`) scans `spool/` oldest-first.
2. For each entry bound to client C, over the row's transport: `shunt enqueue - <slug>-<shortid> --json --root <qroot>` with the body on stdin.
3. Only on a clean ack (exit 0 + JSON `{ok: true, name: ...}`) does the server remove the spool entry. Any failure leaves it for the next poll.
4. Dupes from a retry after a lost ack are deduped box-side: the rendered body carries `# shunt:id=<task id>` and the remote returns the existing entry (`{ok, name, deduped: true}`) instead of minting a second number (§3.3). The task guard remains the backstop for remotes that predate the directive, and the server task id is also embedded in the slug (`<slug>-<shortid>`) so a re-pushed task is recognizable in logs. A spool entry staged before this change carries no directive in its body — it simply allocates a fresh number per push, the pre-dedup behavior.

**Held entries are staged but undeliverable until released.** `submit --hold[=REASON]` and `spool hold <id> [reason]` set `hold` on the entry JSON (a free-form reason string, `""` = held without note); `spool release <id>` clears it — both accept a unique id prefix, same as `task show`. A held entry is excluded from every sweep — `dispatch --once` and the loop alike: no probe, no `pin_target`, no push — so deferred work can sit in the spool indefinitely without ever reaching the box. This is the answer to "stage now, decide later" (issue #13): the alternative, dispatch-time id filtering, would only shape `--once`, while the looping dispatcher still fires every staged entry on its next poll. The flag is spool-local state — it never enters the rendered task body or the §5 wire protocol. `spool ls` marks held entries; `spool rm` removes them like any other. Backward compat: the field is absent on pre-hold entries (`serde(default)` parses them as unheld), and a downgraded binary ignores the unknown field — a held entry read by an old binary is simply dispatchable again.

Fault isolation: a spool file that fails to read or parse is quarantined — `<id>.json`/`<id>.sh` renamed to `*.bad` — with a stderr warning, and the sweep continues; one corrupt entry must never stop delivery for any client. A client row that fails to load skips only that client's entries. In the looping dispatcher a failed sweep is logged and retried next poll; `dispatch --once` still propagates errors.

Auto-heal: if the box answers but the daemon liveness check fails, the dispatcher runs `shunt start --root <qroot>` once before pushing (this is the "power on → claims work" link; box-side autostart remains optional sugar). If the transport fails entirely (box off), the entry stays in spool.

Dead-box probe backoff: the sweep walks entries in order, so a single dead box (a full `ConnectTimeout` × retry stretch) would head-of-line block every live client behind it. Once a client's conn_fail streak reaches `CONN_FAIL_ESCALATE`, the dispatcher probes it only every 4th sweep and treats it as down in between; a recovered box clears the streak on its first periodic probe and resumes full service. `dispatch --once` (sweep 0) always probes.

### 4.5 Task ledger and events

A clean enqueue ack deletes the spool entry, so without a ledger the spool id → queue-name mapping would be lost and an agent could only grep `done/`/`failed/` by shortid. On every successful push the dispatcher writes `tasks/<id>.json` (one file per task, same write-temp-then-rename discipline as the spool): `{id, project, client, slug, queue_name, submitted_at, pushed_at, outcome}`. `queue_name` is the box-side handle (`NNNN_<slug>`) for `logs`/`kill`/`cancel`. A ledger write failure warns but never makes a delivered task look undelivered.

The box never reports back (there is no reverse channel), so outcomes are resolved **lazily**: `shunt task show <id>` re-queries the client's `status` and matches `queue_name` against `pending`/`running`/`done_recent`/`failed_recent`. A terminal sighting (`done`/`failed` with rc) is written back into the record's `outcome` exactly once and appended to `events.jsonl` as a `finish`/`fail` line; transient states are reported live but not recorded. A name absent from all lists reports `unknown` — it may have aged out of the recent-5 window, been cancelled, or renamed. `task ls` lists records; `task events` prints the raw stream (`{ts, event: pushed|finish|fail, id, client, queue, rc?}` — appends are single O_APPEND writes). `client rename` retargets ledger `client` fields; `client rm` leaves the ledger as history. This is deliberately the minimal spine — a ledger and event stream an agent can grep — not a notification system: nothing pushes, nothing polls on the agent's behalf.

## 5. Wire protocol

The remote interface is `shunt <subcmd> --json` on the client. Every response is a single compact JSON object line with `"protocol": 1`. Errors exit nonzero with `{"protocol":1,"ok":false,"error":"..."}`.

Remote stdout is not guaranteed clean: ssh runs the login shell, so `.bashrc` echoes, motd, or conda-init banners can surround the answer. The server never whole-parses stdout — it scans lines from the end and takes the first that parses as a JSON object carrying `"protocol"` (`wire::extract`). Non-JSON responses (`logs`, `mktask` bodies) are unaffected.

The `protocol` field is validated, not just located: an answer whose `protocol` differs from the build's version is rejected with a binary-drift error (`extract` fails; callers surface the message), and a JSON answer carrying no `protocol` field at all is treated the same way — a remote binary that predates the field is by definition a version mismatch, so "missing" is reported as drift, not as a format error. Both errors direct the operator to `shunt client bootstrap <name>`: bootstrap is idempotent (key deploy dedupes, `init`/`start` no-op on a live queue, the binary ships staged-then-`mv` so an scp never truncates a running executable) and therefore doubles as the binary upgrade path — no separate upgrade command exists.

Transport is chosen per client row: `ssh` (default) execs over SSH; `local` (`local = true` in a row, including the pre-registered `localhost`) spawns the same argv via the local `shunt` binary — the wire surface is identical and the loopback needs no network, no key. Every wire call appends `--root <qroot>` from the client row, so the registry is the single source of truth for where the queue lives; ops that read state also pass `--json`.

| op                                       | request                                         | response sketch                                                                                                                                                                                          |
| ---------------------------------------- | ----------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `enqueue`                                | task body on stdin; `--slug`                    | `{ok, name, deduped?}` — `deduped: true` when `# shunt:id=` matched an existing entry                                                                                                                    |
| `new`                                    | `--slug`, `--guard`, `--` argv                  | `{ok, name, deduped?}`                                                                                                                                                                                   |
| `mktask`                                 | same as `new`                                   | task body on stdout                                                                                                                                                                                      |
| `status`                                 | —                                               | `{daemon:{alive,pid,ver?,bin?}, workers:{value,source}, running[], pending[], done_recent[], failed_recent[], stop, shutdown:{armed,grace_s,cmd}, resources?, pending_detail?, running_detail?}` (§3.16) |
| `cancel`                                 | pattern                                         | `{ok, name}` or error on 0/multi match                                                                                                                                                                   |
| `clean`                                  | `--pending\|--done\|--failed\|--archive\|--all` | `{ok, cleaned:{pending,done,failed,archive}, skipped, errors[]}` — `running/` is never a target; bare flags refuse with per-track counts (§3.9b)                                                         |
| `prio`                                   | pattern, n                                      | `{ok, from, to}`; error if target exists                                                                                                                                                                 |
| `kill`                                   | pattern                                         | `{ok, pgid, escalated}`; `{ok, gone}` when the recorded leader is dead/recycled — nothing was signaled                                                                                                   |
| `workers get` / `workers set N`          | —                                               | `{workers, source}`                                                                                                                                                                                      |
| `shutdown on [grace]` / `off` / `status` | —                                               | `{armed, grace_s, running, pending}`                                                                                                                                                                     |
| `probe`                                  | —                                               | `{gpu, driver, cuda, cores, mem_gb, disk_free_gb, python}`                                                                                                                                               |
| `logs`                                   | `-n N` and/or `-- pat`; bare = daemon           | text stream (`-f` is local-only: ssh exec is collected under the §5 deadline, so wire follow is refused, not streamed)                                                                                   |
| `cat`                                    | `-- pat`                                        | task.sh body on stdout — a text op like `logs`/`mktask`; matches pending → running → done → failed, exact name wins over substring, several refuse                                                       |
| `start` / `stop` / `daemon`              | —                                               | `{ok, pid}` / `{ok}` / runs the loop (foreground; `start` daemonizes)                                                                                                                                    |
| `restart`                                | —                                               | `{ok, pid, from_ver}` — SIGUSR2 handoff (§3.5a); refuses when `daemon.ver` is absent, falls through to `start` when the daemon is dead                                                                   |
| `init`                                   | `--qroot`                                       | `{ok}` creates dirs                                                                                                                                                                                      |

All argv crossing SSH is shell-quoted with `%q`-equivalent escaping on the server side; quoting is centralized in the transport layer, never hand-rolled per call site.

`workers` is the one op with two spellings: the CLI form `shunt workers [N]` (positional) for interactive use, and the wire form `workers get` / `workers set N` the server builds when forwarding (`shunt workers -c x 4` → `workers set 4`), so remote argv stays unambiguous.

Every ssh exec is bounded twice, because `ConnectTimeout` covers only the handshake: `ServerAliveInterval=15`/`ServerAliveCountMax=2` kills a half-open TCP session in ~30s, and a 120s hard deadline (`SHUNT_SSH_EXEC_TIMEOUT_S` override) kills a live-but-hung remote command — a deadline hit bails without retry and without bumping the conn_fail streak (a hang is not a flap). The SOCKS5 tunnel sets its own ServerAlive policy and is unaffected. `status --all` probes clients in parallel (one std::thread each), so a dead box costs its own timeout rather than a serial slot; output order stays the registry's sorted order.

## 6. Rust mapping table

| bash construct                       | Rust mechanism                                                                                        |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| `setsid bash qexec.sh task &`        | `Command::spawn` + `process_group(0)` (pre-exec `setsid`); pid = pgid                                 |
| `flock -w 30` on `.seq.lock`         | `fs2::FileExt::lock_exclusive` + timeout retry                                                        |
| `trap on_term TERM INT HUP`          | `signal-hook` iterator; forward TERM→2 s→KILL to tracked pgids; exit 143                              |
| `wait $pid; rc=$?`                   | `Child::wait` / `try_wait` for the reaper                                                             |
| `find -printf '%f' \| LC_ALL=C sort` | `read_dir` + byte-wise sort on filenames                                                              |
| `kill -0 -- -pgid`                   | `kill(-pgid, 0)` via `nix`-style libc call                                                            |
| tmux `queue` session                 | none — `shunt start` self-daemonizes: `setsid`, redirect stdout/stderr to `daemon.log`, write pidfile |
| `env` file / inline exports          | parsed `KEY=value` map per binding, injected into task `Command` env                                  |
| `date -Is` log stamps                | `OffsetDateTime` RFC 3339                                                                             |
| `daemon.log` text lines              | JSONL: `{ts, event, ...}` per line; `shunt logs` renders human-readable                               |

## 7. End-to-end lifecycle

This dedicated-cloud example uses AutoDL paths. Local, owned, or shared machines use the same submit/dispatch flow with their own paths and daemon lifecycle settings.

1. `shunt client add rw-a --template autodl` — edit host/port/key in the row.
2. `shunt client bootstrap rw-a` — scp the binary, remote `shunt init` + `shunt start`, then wire `shunt probe` (the binary is already on the box, so there is exactly one probe implementation); facts merged into the row.
3. `shunt project add riboworld` + `shunt project bind riboworld --client rw-a --workdir /root/autodl-tmp/RiboWorld`.
4. `shunt submit -p riboworld -c rw-a --guard runs/x.json -- uv run python ...` → lands in `spool/`.
5. Dispatcher pushes when the machine is reachable; its queue daemon runs the task independently. On drain, the daemon stays resident or exits under `exit_on_idle`; only explicitly armed idle shutdown powers the machine off. Offline submissions stay in spool until the machine is available; the dispatcher's auto-heal restarts `shuntd` and delivers.

## 8. Probe spec

`shunt probe` collects, emitting JSON — it is the only probe implementation; bootstrap ships the binary before probing and `client probe` runs it over the wire (an unbootstrapped box answers with the binary-drift error from §5):

- `gpu`: `nvidia-smi --query-gpu=name,driver_version --format=csv,noheader`; absent → `null` (CPU-only boot is a valid state — AutoDL no-card boots drop the GPU).
- `cuda`: `nvidia-smi` header parse.
- `cores`: cgroup-aware — `cpu.max` quota/period (v2) or `cpu.cfs_quota_us`/`cpu.cfs_period_us` (v1); `nproc` only as fallback and marked suspect.
- `mem_gb`: `memory.max` / `memory.limit_in_bytes`; `free` only as fallback, same caveat. Never size off host-reported values.
- `disk_free_gb`: `df` on `deploy_root`.
- `python`: `python3 --version`.
- `libc`: `ldd --version` parsed to `glibc 2.35` / `musl 1.2.4`; absent when undeterminable. Deploy-relevant context — the §15 verdict is computed against it. `needs_glibc`: the binary's own max `GLIBC_x.y` ref, self-scanned — the other half of the verdict; absent on musl builds and remotes predating the field.
- `shutdown_cmd`: best-guess poweroff command — `systemctl poweroff` when `/run/systemd/system` exists and `systemctl` is found, else `shutdown`, else `poweroff`, else absent. Advisory: a container may carry the binary yet lack CAP_SYS_BOOT; the row's `shutdown_cmd` field and `--cmd` flag still override it at arm time.

Facts are advisory registry data for agent-side matching, not enforced constraints.

## 9. Billing and cost surfacing

`billing.hourly`/`billing.currency` in a client row flow into `shunt status --all --json` output so the agent can reason about cost. `expire` carries the billing lifetime — `unlimited` for owned compute (localhost, lab GPUs), `metered` for pay-as-you-go (no deadline; burns until the balance does), or an ISO-8601 timestamp for subscription boxes — and `status` adds `expires_in_s` when the timestamp carries an offset. shunt never makes the decision — "instance has 40 min of paid time left but the next task needs 3 h → hold and notify" is agent logic taught in the skill file. shunt's job is accurate exposure of queue state, uptime context, and rates.

## 10. CLI surface

| command                                                                                           | side   | purpose                                                                                                                                                      |
| ------------------------------------------------------------------------------------------------- | ------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `shunt submit -p proj -c client [--guard p] [--hold[=why]] -- cmd...`                             | server | spool a task; `--hold` stages it undispatchable until `spool release`                                                                                        |
| `shunt submit -c client file.sh`                                                                  | server | spool a script file                                                                                                                                          |
| `shunt status [--all] [--json]`                                                                   | server | aggregate client states                                                                                                                                      |
| `shunt spool ls\|rm\|hold\|release`                                                               | server | inspect staging; `hold`/`release` gate delivery (§4.4)                                                                                                       |
| `shunt task ls\|show <id>\|events`                                                                | server | task ledger + outcomes                                                                                                                                       |
| `shunt client add\|update\|ls\|show\|bootstrap\|probe\|env\|templates\|hostkey\|rename\|exec\|rm` | server | registry; `bootstrap`/`env` cover all queues                                                                                                                 |
| `shunt queue add\|ls\|show\|default\|set\|rm\|bootstrap\|env\|start\|stop\|restart -c client`     | server | named queue configuration + queue-scoped lifecycle                                                                                                           |
| `shunt submit -c client --queue <name>`                                                           | server | spool a task for a named queue (all queue ops accept `--queue`)                                                                                              |  |
| `shunt project add\|bind\|unbind\|ls\|show\|rm`                                                   | server | registry; `bind`/`unbind` take `CLIENT --queue <name>` pairs                                                                                                 |  |
| `shunt dispatch [--once]`                                                                         | server | run the dispatcher loop                                                                                                                                      |
| `shunt serve [--bind] [--port] [--token] [--interval-s]`                                          | server | read-only ops dashboard: embedded web UI + websocket snapshots (§18)                                                                                         |
| `shunt tunnel up\|down\|status -c client`                                                         | server | box-side SOCKS5 tunnel                                                                                                                                       |
| `shunt daemon`                                                                                    | client | run queue loop (foreground)                                                                                                                                  |
| `shunt init`                                                                                      | client | create queue dirs                                                                                                                                            |
| `shunt start` / `shunt stop` / `shunt restart`                                                    | client | daemon lifecycle; `restart` is the §3.5a SIGUSR2 handoff                                                                                                     |
| `shunt enqueue [file\|-] [slug]`                                                                  | client | install into pending                                                                                                                                         |
| `shunt new` / `shunt mktask`                                                                      | client | wrap argv as task (guard)                                                                                                                                    |
| `shunt status`                                                                                    | client | queue summary                                                                                                                                                |
| `shunt cancel` / `prio` / `kill`                                                                  | client | pending/running ops                                                                                                                                          |
| `shunt clean --pending\|--done\|--failed\|--archive\|--all`                                       | client | bulk-remove queue entries by set (§3.9b); bare invocation refuses with per-track counts                                                                      |
| `shunt workers [N]` (wire: `workers get`\|`set N`)                                                | client | pool size get/set                                                                                                                                            |
| `shunt shutdown on\|off\|status`                                                                  | client | idle poweroff                                                                                                                                                |
| `shunt room create\|ls\|say\|read\|decide\|members\|compact\|archive\|unarchive\|rm`              | server | project-membership coordination rooms (§17)                                                                                                                  |
| `shunt logs [-n N] [-f] [pat]` / `shunt dlog`                                                     | client | task/daemon logs                                                                                                                                             |
| `shunt cat <pat>`                                                                                 | client | print a task's task.sh verbatim — matches all four tracks, exact name wins over substring                                                                    |
| `shunt probe`                                                                                     | client | machine facts JSON                                                                                                                                           |
| `shunt completions <shell>`                                                                       | local  | shell completion script                                                                                                                                      |
| `shunt version [-c client] [--json]`                                                              | local  | version+build info; `-c` forwards the `--version` flag (works on remotes that predate this command)                                                          |
| `shunt upgrade [--check] [--force] [-c client] [--queue name] [--json]`                           | local  | self-upgrade from latest release; `-c` upgrades one queue binary — `--queue` is required on multi-queue clients; live daemons are handed off via §3.5a (§16) |
| `shunt feedback <title> ...` / `feedback ls\|retry`                                               | local  | file a GitHub issue (§14)                                                                                                                                    |

## 11. Testing strategy

Integration tests spawn a real daemon against a tmp `SHUNT_ROOT`; the poll interval is shortened via env, and `shutdown_cmd` is redirected to a script that touches a marker file. Must cover:

- dispatch order (filename sort; `0000_` band ahead of `0100_`)
- enqueue concurrency vs dispatch (`.seq` under lock; no dup numbers)
- delivery dedup: same `# shunt:id=` returns the existing entry across all four live tracks; concurrent same-key pushes mint exactly once; no-directive bodies always allocate
- `.seq` fault handling: corrupt counter bails with remediation; missing counter rebuilds from high-water
- `cancel` 0/1/many match; `prio` renumber incl. target-exists refusal
- `clean`: per-set selection incl. `.archive` contents; a pending entry lost to a mid-sweep claim counts as `skipped` and is never half-deleted; flat-entry `.sh.*` sidecars move with the body; bare invocation refuses with per-track counts; stale `.clean-*` trash is swept first
- `kill`: TERM → 10 s → KILL, rc recorded, lands in `failed/`; recycled-pgid refusal (leader start_time mismatch → `gone`, foreign group untouched)
- orphan requeue: dead daemon leaves `running/` entries → restart → front-of-queue, prefix strip, interrupted logs swept; requeue-target collision quarantines to `failed/`
- claim/finalize failure paths: unclaimable pending entry quarantined (no dispatch spin); failed finalize quarantines instead of ghosting in `running/`
- live-pgid orphan: WARNING + requeue anyway; crash orphans (reparented, ppid ≠ daemon) keep this path even when the group is still alive
- handoff adoption: USR2 re-exec keeps the pid, adopts every `running/` entry whose leader is still our child (real rc through `waitpid`), rebuilds `timeout_s` deadlines from `spawned_at` (budget survives restart, not re-timed), and seeds usage peaks from the `usage` sidecar; a `restart` against a daemon with no `daemon.ver` is refused; an `execve` failure logs `handoff_failed` and the old image keeps running
- STOP drain: running task finalizes, then daemon exits; armed shutdown_on_idle is honored (grace countdown → fire) instead of orphaned
- idle-shutdown: arm → fire (marker), cancel via flag rm / new pending / STOP, grace default, final re-check
- workers resize: raise dispatches immediately; lower drains only; invalid value falls back to last-known
- env file overlay, workdir, proxy unset, thread caps
- dispatcher: spool → push → ack → delete; unreachable client retains; auto-heal restarts a dead daemon
- `pre_connect`: fires once per call after the first rc-255 (fake-ssh → hook marker file), eagerly before the first attempt when the conn_fail streak is already nonzero; a hook failure or timeout is named in the unreachable error and never gates the remaining retries; wire errors and deadline-hit hangs do not trigger it; `tunnel up` retries a setup-dead spawn once after running it
- reschedule: rc 75 requeues same-name to pending/ with `attempts`/`retry_after`; budget exhaustion finalizes; OOM heuristic only fires for retry-declared tasks; `retry_after` gates dispatch until due
- usage telemetry: `usage` sidecar written per cycle, travels to terminal tracks; `status` reports resources + declared-vs-observed detail
- bootstrap compat (§15): stage → `<staged> --version 2>&1` → three-state classify (ok/warn/error); auto source rescues via same-sha musl release then falls back warn / refuses error; explicit path honors warn, refuses error; failed candidates are `rm`-ed, never `mv`-ed; live daemons hand off via wire `restart`; GLIBC byte-scan + ldd are diagnostics only; smoke `--version 2>&1` surfaces loader stderr
- feedback: sanitize redacts registry secrets (host/port/user/password/key/$HOME) and skips the common-name allowlist; persist-before-deliver leaves a pending artifact when delivery is disabled (`SHUNT_FEEDBACK_REPO=` empty); dedup returns the existing URL within 24 h; the hourly cap refuses but still persists; `retry` delivers pending entries; delivery itself runs against a stub `gh` on PATH (never the real API)
- rooms: concurrent `say` mints unique message numbers under `.seq.lock`; `read` prints only messages past the reader's `last_read_n` and marks it (`--all` shows the stream, still marks); `say`/`decide` mark their own message read; `ls`/`status` report `unread_msgs` without marking; a non-member `say` posts with `member:false` and names the roster on stderr; `decide` overwrites `meta.decision`; the 500-message cap archives oldest-first to `msgs/.archive/`; `compact` commits an in-stream `summary_through` digest and archives ≤N; `read --from/--to` reaches `.archive/`; `room ls` suggestions group projects sharing an ssh endpoint; a >16 KiB body is rejected

## 12. Skill deliverable

`skills/shunt/SKILL.md` ships in the repo (no `shunt skill` subcommand). Contents: **a deployment-consent interview before `client add`** — the agent asks the user, not decides: shared box or dedicated (`--shared`), ssh key strategy (dedicated keypair + password deploy / reuse an already-authorized `--key` / manual console install), qroot placement, daemon residency (`--exit-on-idle`), reachability precondition (`--pre-connect`) — then register/bootstrap, add/bind a project, submit and guard conventions, query and interpret queue state and JSONL events, cost-aware practices (arm idle-shutdown only on dedicated boxes, watch billing metadata), splitting long experiments into checkpointable tasks so at-least-once reruns are cheap, and the feedback channel (§14) — an agent that hits a shunt defect or UX wart should file it via `shunt feedback`, with a reminder never to paste credentials into the body.

## 13. Repo layout

```text
shunt/
  Cargo.toml
  src/
    main.rs          # clap entry; calls cli::run
    cli/             # clap command tree (args.rs), dispatch (mod.rs,
                     # server.rs, local.rs, feedback.rs, upgrade.rs — §16)
    server/          # registry/ (types, paths, state, io),
                     # fsutil (0700/0600 privacy), spool, dispatcher,
                     # tasks (ledger), tunnel, transport/ (ssh + local),
                     # status, room (coordination rooms — §17),
                     # client_ops/ (keys, lifecycle, update, hygiene, env,
                     # probe, bootstrap, compat + release fetch — §15)
    agent/           # FS queue ops, exec, shutdown flags, probe;
                     # daemon/ = poll loop (core), lifecycle, workers,
                     # orphan adopt+requeue (reap), idle countdown (idle)
    handoff.rs       # §3.5a SIGUSR2 re-exec shared by daemon + dispatcher
    feedback/        # GitHub-issue feedback channel: compose + sanitize,
                     # persist-first ledger, gh/curl delivery (§14)
    proto/           # task format, wire JSON types (shared)
  etc/vendors/autodl.toml
  docs/design.md
  skills/shunt/SKILL.md
  tests/             # tmp-SHUNT_ROOT integration suite
  reference/         # bash oracle (read-only archive)
  task_template.sh
  README.md  LICENSE(MIT)
```

## 14. Feedback channel

`shunt feedback` files a report as a GitHub issue. The usual reporter is an agent in another project that hit a shunt defect or UX wart — the command exists so that report is one non-interactive call, and so it cannot leak box credentials into a public repository.

It is a local command, not a wire op: whatever machine runs it files directly, and no registry is required — a bare box resolves the artifact dir under the XDG default, delivery simply fails there (no gh, no token), and the error names where the report landed. There is deliberately no box-side collection path (no qroot artifacts, no `status` field, no `feedback --collect`): agents live on the dev machine, so that machinery would serve a case that does not occur. If it ever does, collection is a follow-up over the existing wire/exec machinery.

### 14.1 Command surface

```text
shunt feedback <title> [--kind bug|ux|feature|docs]
                       [--body TEXT | --body-file F | <stdin>]
                       [--client NAME] [--dry-run] [--json]
shunt feedback ls                    # local ledger
shunt feedback retry [<id>]          # re-deliver pending artifacts
```

- The kind renders as a `[feedback/<kind>]` title prefix — labels would need repo-side setup, a prefix needs none.
- Body source precedence: `--body` > `--body-file` > stdin (when stdin is not a TTY). One of the three is required; a bare interactive call errors naming the options.
- `--client NAME` appends the row's `facts` (probe data), `billing`, and `expire` as a fenced JSON block — `ssh.*` is never included, and it costs no network. A row that fails to load fails the send.
- `--dry-run` prints the composed issue body and files nothing — the human review path.
- `retry`'s id resolves by exact filename stem first, otherwise by a unique filename-stem prefix across the complete ledger (including filed artifacts); omitted = all pending. Ambiguous prefixes list the candidate stems and fail before delivery or artifact updates. A resolved filed artifact is not retried; the error includes its URL when available. Missing ids point to `shunt feedback ls --json`.

### 14.2 Pipeline

1. **Compose**: `[feedback/<kind>] <title>` plus a body of the report text, a context block (shunt version, wire protocol version, os/arch, optional client facts — emitted as fenced JSON so an agent can parse it), and a `filed by shunt feedback` footer. The report text is truncated at 60,000 bytes with an explicit `… [truncated]` marker — GitHub rejects issue bodies past 64 KiB, and an oversized report would otherwise persist pending forever.
2. **Sanitize — mandatory, the default repo is public.** The sensitive-value set is read straight off disk: every `clients/*.toml` parsed as raw TOML (typed row loading is bypassed — a disabled or unparseable row still holds secrets), plucking `ssh.host`, `ssh.port`, `ssh.user`, `ssh.password`, `ssh.key`, plus the `$HOME` prefix. Every occurrence of a value ≥ 4 chars in title+body becomes `<redacted>` — except an allowlist of collision-prone common names (`root`, `ubuntu`, `debian`, `admin`, `user`, `git`, `shunt`, `localhost`), because redacting "root" would mangle "root cause". Redaction count is reported on stderr. This guards the realistic leak — an agent pasting `ssh root@1.2.3.4 -p 12345` errors verbatim; a body can still embed arbitrary secrets, and that residual is stated here rather than claimed solved.

   The same pass redacts registered **names** with kind-tagged placeholders: every client name (`clients/*.toml` stems — disabled rows list too, a parked box's name is still sensitive) → `<BOX_NAME>`, every project name (`projects/*.toml` stems) → `<PROJECT_NAME>`, every queue name (each client row's `[queues.<name>]` tables) → `<QUEUE_NAME>`. Applied to title and body alike — the composed body's context block rides inside it, so `--client` facts are covered. Matching is a case-insensitive substring scan, longest-name-first across all kinds, so a name that prefixes another still fully redacts (`gpu` vs `gpu-1`) and embeds keep their suffix (`riboworld-waves` → `<PROJECT_NAME>-waves`). `localhost`, `admin`, `default`, and the secrets allowlist's words are exempt — `localhost` is the loopback peer and `default` the canonical queue name every flat row resolves to, so scrubbing either would mangle legitimate prose while exposing nothing; single-character names are skipped (a lone letter is indistinguishable from prose). Names come from the live registry at compose (and `retry`) time — a name deleted beforehand cannot be redacted, which is accepted: the registry is the truth of what exists. Queue names load via `load_client_row`, not `load_client`, so disabled rows still contribute and no `localhost` row is materialized by a read-only scrub.
3. **Persist first (at-least-once):** `$SHUNT_HOME/feedback/<unixts>_<slug>.json` — `{kind, title, body, created_at, delivered_at, issue_url, issue_number, attempts}`, 0600, slug = sanitized title. New reports are fully written to an exclusive private staging file, then atomically hard-linked to an unoccupied JSON name so concurrent writers cannot overwrite each other and scans never see a partial new report. Same-second collisions use suffixes `_1` through `_99`; exhausting all 100 names reports an error with a wait-and-retry hint. Staging files are removed on normal success/failure; interruption can leave `.pending-*` files, which scans ignore. This installation requires hard-link support in the ledger filesystem; an unsupported filesystem surfaces a persist failure rather than falling back to clobbering writes. Delivery updates still use `write_private` in place. The artifact stores the post-sanitize text, so a `retry` can never deliver a pre-scrub draft — and `retry` re-scrubs anyway, because a client row registered between send and retry can have added secrets the persisted body still carries. Persist failure warns but does not stop delivery; if persist AND delivery both fail the composed body is dumped to stderr so the report survives in the caller's scrollback. Metadata scans (`ls`, the rate cap) read a header-only projection — bodies can be tens of KB and are retained only where needed (`find_dup`, `retry`).
4. **Deliver:** repo resolves `$SHUNT_FEEDBACK_REPO` > `server.toml` `feedback_repo` > compile-time `ShinoharaHaruna/shunt` — the env wins so a fork can redirect without editing files, and an **empty env value disables delivery deliberately** (test hook and user opt-out: the artifact persists, the error says so). Mechanism order: `gh` on PATH (`gh issue create --repo R --title T --body-file -`, body on stdin, URL parsed from stdout) → `GH_TOKEN`/`GITHUB_TOKEN` + `curl` POST to `api.github.com/repos/R/issues` (the token rides a 0600 `curl -K` config file, never argv — argv is world-readable via /proc) → fail with a remediation message (`install gh or export GH_TOKEN, then shunt feedback retry`). Each mechanism is tried in turn and the errors are reported together. No HTTP client is added — `std::process` shell-out keeps the dep list untouched.
5. **Report:** `feedback filed: <url>` on stdout; `--json` emits `{ok, url, number, deduped}`. Failures with `--json` emit `{ok:false, error, artifact}` before the nonzero exit — agents consume this command too.

### 14.3 Anti-spam

The reporter may be an agent sitting in a retry loop aimed at a public repo:

- **Dedup:** an artifact with identical `(kind, title, body)` delivered within the last 24 h short-circuits to the existing issue URL (`deduped: true`) — checked before the rate cap so a repeat report is never blocked.
- **Rate cap:** `SHUNT_FEEDBACK_MAX_PER_HOUR` (default 5) delivered artifacts in the last hour refuses delivery — the artifact still persists, and the error says to re-run `shunt feedback retry` later. `retry` runs the same resolve path (dedup, then cap): it is the cap's remediation, not a bypass — a looped caller cannot drain the ledger into a public repo. Only real deliveries count toward the cap (a dedup-hit copy consumed no API call).

## 15. Binary compatibility (three deploy states)

`bootstrap` ships a binary to a box whose libc may predate the dev machine's toolchain. The check OBSERVES rather than predicts — a glibc-linked dev build on an old-glibc box once ran only by weak-version luck, the loader warning buried in remote stderr that `exec` discards on success.

**Three deploy states** (`Compat`): `ok` — runs clean; `warn` — runs but prints loader noise on every invocation (weak-version refs the box's older glibc cannot serve, and the like); `error` — cannot start (hard version miss, wrong arch, missing interpreter, exec failure). Warn is a real state, not a euphemism: the smoke proves only that the process STARTS — the loader guarantees every hard ref resolved, but an unprobed weak symbol could still abort a task mid-run. The deploy ladder is therefore ok → warn → refuse: reach ok when possible, accept warn loudly, refuse error.

**Source order:** `--bin-path` > row `bin_path` > `current_exe()`. The row field exists because bootstrap doubles as the upgrade path — a musl build for an old-glibc box must survive re-bootstraps; when it is set, bootstrap prints both the local and the shipped version strings so drift is loud. An explicit path (flag or row) is the operator's choice and is never substituted: a warn-state explicit binary ships with the noise named loudly; an error-state one refuses with the musl recipe.

**Classify** — stage the candidate to `<bin>.new`, then exec `<staged> --version 2>&1`. The version line proves the process started; any other line is noise that recurs on every future call → `warn`; no version line, or an exec failure, → `error`. The staged path — never the install path — is what runs, so a bad binary cannot land; bail paths `rm -f` the staged file. Observing covers every failure class for free: a dynamic-musl or other-libc binary (no `GLIBC_x.y` refs for a byte-scan to find — the old predictive check shipped it "unconditionally" and it died at runtime), a wrong arch, a missing interpreter, a box without `ldd` at all. The `GLIBC_x.y` byte-scan of the local binary survives only as diagnostics — it names the need in reports and gates the rescue (a static binary has no release to swap for); the box's `ldd --version` is likewise report context, never the decider.

- `warn` on the auto source: **rescue first** — the same-code musl release (below) upgrades warn → ok; when no matching release exists, the warn binary ships with a named report: the noise recurs per remote call and lands in daemon.log, and the fix (`upgrade -c` or a musl `--bin-path`) is printed.
- `error` on the auto source: rescue is the only road to ok; a fetched musl that itself smokes non-ok means the box is broken beyond libc (arch, sandboxing) → refuse with both outputs; a rescue failure refuses with the musl build recipe (`rustup target add x86_64-unknown-linux-musl && cargo build --release --target x86_64-unknown-linux-musl`) and `--bin-path`.
- `warn`/`error` on an explicit path: warn ships (it runs — the choice is honored); error refuses (it cannot — substituting would be a silent override).

**Rescue** mechanics: the local `--version` string `x.y.z (<sha> <date>)` names a commit. Resolve the release tag by querying `v<x.y.z>` first, then `<x.y.z>` only when the API confirms the prefixed tag is absent (HTTP 404); other query errors fail rescue. Download `shunt-<tag>-x86_64-unknown-linux-musl.tar.gz` + `SHA256SUMS-<target>` from the releases of `SHUNT_RELEASE_REPO` > `server.toml release_repo` > upstream default (`gh release download` first, `curl -L` fallback — public assets need no auth), verify the checksum, then run the extracted binary's `--version`: the embedded sha must equal the local sha — the artifact self-certifies it is the same code. Checksum or identity failure never retries another tag. A local build with no embedded sha accepts on `x.y.z` alone; `-dirty` warns that the shipped build lacks the uncommitted changes. The rescued binary is staged and smoked like any candidate — a musl that cannot run means the box is broken beyond libc.

**Redeploy handoff:** bootstrap is the manual redeploy path, so the install must update what is RUNNING, not just what is installed. Before staging, a wire `status` probes daemon liveness; after install, a live daemon gets `restart` (§3.5a — SIGUSR2 re-execs the just-installed path, no drain) and a dead one `start` as before. A pre-handoff daemon (no `daemon.ver`) refuses the restart — reported named with the `queue restart` / stop+start remediation, never failed the install.

**Post-install smoke:** after the stage-then-mv install and before remote `init`, exec `<bin> --version 2>&1` — nonzero fails bootstrap with the output; the merged loader warnings print on stderr locally. Success echoes the remote's own version string (`remote shunt 0.2.0 (faae5d9 …)`).

**Reporting:** the box-side `status`/`probe` self-scans `current_exe` for `GLIBC_x.y` refs and reports `needs_glibc` (a local file read, no subprocess — safe on the status poll path). Because the reporting process is alive, a need the box's `libc` fact cannot serve can only be weak-missed → `warn`, never `error`; a glibc need against an undeterminable libc reads `warn` too — running, but unverifiably quiet. Server `status` computes the verdict per queue: the row's note cell carries `bin:warn`/`bin:error`, `status --json` surfaces `compat`, and a footnote names the `upgrade -c` remediation. Remotes predating `needs_glibc` report nothing — absent is honest, never synthesized.

The `libc` probe fact shares the ldd parser and lands in the row's `facts` for agent-side matching (§8). The release workflow packs both musl and gnu artifacts per tag; bootstrap only ever fetches the musl one — it runs on every libc the gnu one can and beyond.

## 16. Self-upgrade (`shunt upgrade`)

`shunt upgrade` fetches the newest non-prerelease GitHub release (gh → curl against the public releases API; repo from `$SHUNT_RELEASE_REPO` > `server.toml release_repo` > the built-in upstream) and installs it over `current_exe`:

- **Local** (default): `--check`/`--json` report `{current, latest, newer}` without touching anything. When the release is newer (semver triple compare; a release outranks its prereleases), the asset matching the _running binary's_ linkage is fetched — the §15 byte-scan decides: glibc-linked → `x86_64-unknown-linux-gnu`, static → musl. Integrity is `SHA256SUMS-<target>`; identity is the extracted binary's own `--version` reporting the tag's version (upgrade deliberately does NOT reuse rescue's same-sha check — a new version carries a new sha). Install is stage-then-rename (`<exe>.new` → `mv`), so replacing a running binary is ETXTBSY-safe and atomic. `--force` reinstalls the latest tag even when versions match (e.g. a dirty dev build reverting to the release). A non-writable install location fails with the package-manager remediation. After install, every live daemon in the box's qroot registry is handed off via `shunt restart` (§3.5a) — a registered qroot that no longer exists is swept as unambiguously stale (the daemon re-registers on start) rather than spawned into — the §3.5a `daemon.ver` gate is checked per daemon, so a pre-handoff image is named in the report rather than killed by an unhandled USR2. A daemon started from a different binary path re-execs ITS OWN image, not the just-upgraded `current_exe` — harmless but stale; the report names its `daemon.bin` path. A live `shunt serve` instance is handed off the same way via `serve restart` — its `serve.ver` sidecar is the §3.5a gate, so a pre-handler image is refused with the `serve down`+`serve up` remediation rather than sent an unhandled USR2.
- **Remote** (`-c <client>`): upgrades the BOX's binary to the latest release. A `local = true` row (incl. `localhost`) has no box binary — its daemon runs the dev machine's own image and re-execs `self_exe()` on restart — so `-c` against it is an explicit no-op naming `shunt upgrade` (no `-c`) as the covering verb (`--check`/`--json` still report the running image vs latest, with `local: true`), never a compat smoke against a `<qroot>/bin/shunt` nothing deployed. The box always gets the musl asset (static — runs on any libc, so §15 compat can't fail) delivered through `client bootstrap --bin-path` — owner claim, stage-then-mv, `--version 2>&1` smoke, init/start/probe all apply for free. A remote already reporting the target version is skipped UNLESS its own `--version 2>&1` smoke classified `warn`/`error` — the remote --version doubles as the §15 compat check, so a loader-noisy or non-starting box binary is relinked to musl at equal version (that IS the upgrade: same code, runnable linkage) without needing `--force`. `--check`/`--json` report `compat`; the install report carries `relinked`. Non-x86_64 boxes (checked via `uname -m`) are refused with the build-and-`--bin-path` recipe. The install is followed by a wire `restart` — the daemon re-execs `<qroot>/bin/shunt`, which is exactly the path bootstrap installed over, so queue-semantics changes take effect WITHOUT draining or killing in-flight tasks. Verification is the daemon's own `daemon.ver` flipping to the target version (polled via `status` for a few seconds), not "signal sent": a daemon that predates the handler refuses restart (§3.5a gate) and one started from a different binary path re-execs its own stale image — both surface as a named warning carrying the `daemon.bin` path and the manual `queue restart` remediation.
- **The dispatcher loop honors SIGUSR2 identically** — a running dispatcher re-execs its own argv at the next sweep boundary. Its in-flight state is entirely in `spool/` plus box-side `# shunt:id=` dedup, so even a hard restart loses nothing; there is no dispatcher pidfile, so `shunt upgrade` cannot discover running dispatchers and instead tells the operator to send USR2 (or restart normally). A hand-sent USR2 to a dispatcher that predates the handler still kills it — no `daemon.ver` gate exists there — but the outcome is a relaunched dispatcher, which spool + box-side dedup makes safe anyway.

`rescue_musl` (§15) and `upgrade` share one exact-tag fetch path — download, checksum gate, extract — with caller-specific identity verification on top. Both `v<x.y.z>` and bare `<x.y.z>` tags are supported: latest-release discovery retains the raw tag for download URLs, asset names, and extracted directory names, separately from the normalized version used for comparison and reports. The legacy `latest_version()` and `fetch_asset(ver, target)` helpers retain normalized output and v-prefixed asset lookup, respectively.

## 17. Coordination rooms (server-side)

Several projects can share one box — separate qroots, separate daemons, all pointed at the same GPUs (the §3.11 registry exists precisely because that topology is real). The queue deliberately gives those tenants no way to talk to each other, so resource coordination today happens by trial and error: one agent raises `workers`, the other's task OOMs into the reschedule path, and the negotiation is conducted entirely through failures. `shunt room` is the missing side channel: a long-lived named group chat whose members are the PROJECTS sharing compute, where agents propose and settle agreements (VRAM budgets, worker caps, card assignment) before burning GPU time on them. A room decides nothing by itself — a settled agreement takes effect only when the participants act on it (`workers set`, `# shunt-env:CUDA_VISIBLE_DEVICES=`); the queue never reads it.

Rooms live on the SERVER, in the registry — the headquarter view. Three reasons over a box-side board: (a) membership is derivable data, because the registry is the one place that knows every project↔client binding — "which projects share my box" is a query, not configuration; (b) a room can span boxes ("my peak overlaps yours — I'll shift tasks to box-2"), which a per-box board structurally cannot express; (c) the record survives the rented box it coordinates — a box-side board dies with the box's rebuild, and rooms get the registry's 0700/0600 discipline for free. Rooms are deliberately NOT wire ops: nothing here reaches the box, no remote binary version gates the feature, and no `--as` injection machinery is needed.

Non-goals, stated up front:

- One dev machine, one venue — agents on DIFFERENT dev machines run separate registries and cannot meet in one room (the earlier box-side design handled that topology; rooms trade it for the global view). The observed deployment — several agent sessions on one dev machine sharing one registry — is the supported case.
- No push, no notification — agents discover room activity through `room ls` / `status --as` polling (§17.5).
- Pure side channel — zero interaction with scheduling, dispatch, or the idle-shutdown gate; room traffic never counts as queue activity, and a full room never blocks a task.
- No authentication — `author`/`--as` is a self-declared label, consistent with the queue's own trust model. Rooms serve cooperating agents, not adversarial ones.

### 17.1 Storage and layout

`$SHUNT_HOME/rooms/` — inside the registry, so `fsutil` keeps it 0700 / files 0600 like every sibling dir:

```text
rooms/
  <name>/
    meta.json               # {created_at, members: [project...], clients: [client...], decision?, compacted_through?}
    .seq + .seq.lock        # message counter under flock (§3.2 discipline)
    msgs/
      NNNN.json             # {author, ts, body_md, member: bool, summary_through?}
      .archive/             # overflow past the §17.4 cap + compacted history (§17.6)
    readers/
      <label>.json          # {last_read_n} — high-water mark per identity (§17.5)
  .archive/
    <name>/                 # cold-stored rooms (§17.4)
```

- A room is a LINEAR stream, not threads: one `msgs/` sequence, `NNNN` minted under `.seq.lock` with the §3.2 rules (flock, bounded wait, missing counter rebuilt from high-water, corrupt counter an error with remediation), message dir created temp-then-rename. The topic/reply two-level structure was rejected for rigidity — a coordination stream does not know its topics in advance, and a single ordered log is what a group chat is.
- `meta.json.members` names projects, not client rows: row names are private dev-side bookkeeping — two registries can name the same box `riboworld-7032` and `pytorch-7032` — while the project name is the shared vocabulary a counterpart actually needs to read. `clients` is annotation context (which boxes this room coordinates), never a gate.
- Message bodies are markdown (`body_md`) hard-capped at 16 KiB — oversize rejected before anything is written; a coordination message has no business carrying logs.
- `summary_through` on a `msgs/NNNN.json` marks the message as a compaction digest, the value being the highest message number it covers (§17.6) — one optional field doubles as the kind tag, so a summary stays an ordinary stream item everywhere else. `meta.json.compacted_through` ratchets the highest covered N; it lives in meta rather than being derived from the stream because the 500-cap can archive the newest summary itself, leaving nothing live to derive from.
- A room dir is fully self-contained — `meta.json`, both message tiers (`msgs/` and `msgs/.archive/`), `readers/`, and `.seq` all live under `<name>/` — so room archiving is a single rename into `rooms/.archive/` and `unarchive` restores byte-for-byte, with no cross-references to break. The two `.archive/` levels coexist and are unrelated: `msgs/.archive/` ages individual messages (§17.6), `rooms/.archive/` cold-stores whole rooms (§17.4).

### 17.2 Command surface

```text
shunt room create <name> --members p1,p2 [--clients c1,c2]
shunt room ls [--as L] [--all]            # rooms + msgs + unread(<label>) + suggested groups (§17.4)
shunt room say <room> (--body T | --body-file F | -) [--as L]
shunt room read <room> [--as L] [--all | --digest | --from M --to N]  # unread (default), full stream, digests only, or a range incl. .archive/
shunt room decide <room> --note T [--as L]  # record the current agreed state in meta.decision
shunt room members <room>                 # roster
shunt room compact <room> [--ratio 0.8]               # plan: shunt picks the split, dumps the digest packet
shunt room compact <room> --through N (--body|--body-file|-)  # commit: archive ≤N, store the digest
shunt room archive <room>
shunt room unarchive <name>
shunt room rm <room>
```

All server-local — no `-c` forwarding exists because there is nothing to forward to. Body source precedence `--body` > `--body-file` > `-` (stdin), the §14.1 rule. `<room>` resolves through the §4.1 ladder — exact, then unique case-fold — plus the rooms-specific unique-prefix acceptance that predates the middleware; an ambiguous prefix bails listing the candidates, and a miss carries did-you-mean suggestions. `read` with no selector prints only messages past the reader's mark (§17.5); `say`, `read`, `decide`, and `compact` all mark. `archive` resolves through that same ladder — a room being put away is still a live name — while `unarchive`, and `rm` aimed at the cold side, match EXACT names only under `.archive/` (cold storage takes no prefixes), and every other op pointed at an archived room fails naming `shunt room unarchive` — one uniform remediation, because they all route through the same resolution.

`read`'s view selectors — `--all`, `--digest`, and the `--from`/`--to` range — are mutually exclusive; `--from` and `--to` may each appear alone as an open range, and a range reaching below the live floor reads straight into `.archive/`. `compact` runs two phases under one verb (§17.6): `--through` plus a body source is a commit, neither is a plan, and exactly one is an error naming the missing half; `--ratio` exists only in plan mode. A `read` by a label off the roster still returns the stream and marks — the same open trust as `say`/`decide` — but stderr names the roster, so a wrong-room or mis-cased label is loud instead of silently reading a foreign channel (§17.3). `decide` writes `meta.decision = {ts, author, note}` — latest wins, replacing the topic-close mechanism: what coordination needs is "the current agreement", not a closed thread; the stream retains how it was reached.

### 17.3 Identity: one `--as` label for author and reader

`--as <label>` (sanitized like a §3.3 slug) is both the `author` on messages and the key of the caller's `readers/` cursor — you read under the same name you speak under. Agents SHOULD use their project name (`riboworld`, `gerna`). Resolution: explicit `--as` > `admin` — no `--as` is the human operator, not `$USER`: an agent forgetting `--as` posts as a person, not under whatever account name the shell happens to export.

`admin` is a reserved identity, case-insensitive in every comparison. It names the operator overseeing the rooms — an IMPLICIT member of every room by role, not by roster: `say`/`read`/`decide` from it are exempt from the non-member warning (a human reading a foreign room is auditing, not leaking), its posts record `member: true`, and `room ls` — whose default scope is the caller's member rooms — lists the whole directory for it, since the overseer's inbox IS every room (`--all` is a no-op for `admin`). Membership here means legitimate participation; the roster stays project-only — `meta.members` names which projects a room coordinates, and `admin` is never written onto one: `project add admin` and `room create --members admin` are both rejected, so no registry row or roster can turn the operator into a project. `room members` prints `(+admin implicit)` beside the roster so the implicit seat is visible rather than hidden convention.

After sanitization the label runs the §4.1 name ladder against registered projects — silently, unlike `-c`: `admin` in any case folds to `admin`, and a unique case-insensitive hit substitutes its registered spelling (`--as riboworld` becomes `RiboWorld`). Membership checks, the message `author`, and the `readers/` cursor then all agree on who the caller is — a mis-cased `--as` no longer posts a phantom non-member duplicate, and read cursors stop fragmenting across case variants of what is one project. Two registered projects differing only by case make the match ambiguous and fail loudly; a label matching no project passes through unchanged (free-form labels stay legal — prefix/fuzzy hits never rewrite an identity, they only feed error text). The roster-membership check itself is case-insensitive too, covering rosters that hold non-registered member names.

A `say`/`decide`/`read` whose resolved label is not in `meta.members` still lands — open-trust model — but `say` records `member: false` and all three name the roster on stderr, so an agent that typed the wrong room learns where it went instead of leaking into (or silently reading) a foreign channel.

### 17.4 Membership, discovery, lifecycle

- `room create --members` is explicit; `room ls` additionally prints _suggested_ rooms: groups of projects whose bindings land on the same box (client rows sharing an `ssh.host:port`, plus `local` rows grouped together) with ≥2 distinct projects and no room yet. Suggestions are computed live from bindings — zero stored state, always accurate — and materialize only via `create`.
- Membership tracks reality loosely on purpose: `project bind`/`unbind` does NOT rewrite room rosters (a room may span boxes, may predate the binding, may outlive it); `room ls` flags members whose projects no longer share any suggested group as `stale` so drift is loud, not hidden.
- A room's stream is capped at 500 live messages; past that, oldest `NNNN.json` move to `msgs/.archive/` — the §3.9 track-cap discipline, dot-prefixed and invisible to the default `read`/`ls` view but no longer write-only: `read --from/--to` reaches it.
- Compaction is the second mover into `.archive/` and the cap is the first — a `compact` commit renames the covered non-summary messages there (§17.6). Both movers keep the same `NNNN` namespace, so a `--from`/`--to` range read spans live and archived seamlessly.
- Rooms themselves never auto-expire — the registry is cheap disk and the coordination record is the asset; cleanup is `room rm`. `readers/` is capped at 64 label files, oldest-mtime evicted — a label gone quiet loses only its read positions, never data.
- `room archive` is the ending that keeps the record: a coordination relationship can be over — box released, project retired — while `room rm` would destroy the asset named above and a dead room left live pollutes `ls`. Live and archived names form one disjoint namespace — `create` refuses an archived namesake and points at `unarchive`/`rm` — so neither direction of the rename can ever collide.
- Archived rooms stay visible but out of the way: `room ls` prints them in a separate trailing section (JSON: an `"archived"` field beside `rooms`), while `status --json` stays live-only — a cold room is a stored record, not an inbox anyone should poll.
- `room rm` resolves either namespace — exact-name only on the archived side (§17.2) — and reports which it removed, so destroying a cold room takes a deliberate exact name, never a lucky prefix.

### 17.5 Read state: per-label high-water marks

`readers/<label>.json` holds `{last_read_n}` — a single high-water counter, made trivial by the linear stream: `unread_msgs = count(NNNN > last_read_n)`, finer than the per-topic binary flag of the earlier board design and computable in one pass. Only real engagement marks: `say`, `decide`, and a `compact` commit acknowledge the current live maximum after the op (a `say` marks its own message read — you obviously saw what you posted; a commit likewise saw everything it condensed). `read` acknowledges only through the highest-numbered successfully loaded message it returns — `--all` and `--digest` alike; a concurrent arrival outside that loaded set remains unread. Audit reads are the exception: a `--from`/`--to` range is archaeology, not engagement, and never marks — else one look at history would forge currency. Archived messages are excluded. Unreadable/corrupt messages are skipped with a warning: a skipped tail does not advance the cursor, but a skipped hole below a returned message is crossed by the single high-water mark. An empty result does not advance the cursor. A label whose mark falls inside a compacted range simply sees the digest message as an ordinary unread item — placing the summary in-stream is precisely what lets the single high-water cursor survive compaction, where a sidecar summary store would silently skip the covered messages for stale readers: the skipped-hole semantics this section tolerates only for corruption. `room ls` and `status` report without marking — polling is awareness, not reading. The `ls` table prints `msgs` (live-stream total) beside `unread(<label>)` — the column header names the resolved `--as` label (default `admin`), so a count never poses as a reader's unread when it is really a total, and under `--all` the reader whose mark the column reflects is always visible.

`shunt status --json [--as L]` gains a top-level `rooms: [{name, members, unread_msgs, decision}]` for the `--as` label (default `admin`) — the aggregate view an agent polls anyway now carries its coordination inbox. Reader updates and reader-cap eviction serialize under the room's `.seq.lock`; files are written temp-then-rename and cursors only advance, so an older concurrent caller cannot regress a shared label's read position. Message loading stays outside that lock. A label shared by two callers shares one read set (intended: one identity, one read position); eviction still resets a quiet label as described in §17.4.

### 17.6 Compaction

A linear stream is the right shape for coordination, but raw catch-up degrades as the stream grows: an agent returning after a month owes the room a full re-read for a handful of still-relevant facts. Compaction condenses an old range into one in-stream digest message while keeping the newest ~20% raw — the pi-style 80/20 split: recent context stays verbatim because it is what agents quote and react to, while the long tail collapses into prose. `decision` and digests do different jobs and both survive: `meta.decision` is the normative what-we-agreed, a digest is the narrative what-was-said — losing either makes the other work harder than it should.

Shunt never summarizes — no LLM lives in the binary. The agent supplies the digest text; shunt supplies the split point, the archival mechanics, and the cursor-safe placement. That division is why `compact` is two-phase: plan picks the boundary and dumps the digest packet — the covered messages verbatim, for the agent to condense — and commit stores the result. `--through` is mandatory at commit and never recomputed: the digest must provably cover exactly what gets archived, and a split recomputed at commit time could silently swallow messages posted between plan and commit.

- **Split math is BYTES, not count.** Readability is a byte metric — the newest 20% by count can hold 90% of the bytes when recent messages run long. The eligible set is the non-summary live messages: summaries are never re-digested and never compact-moved — exempt movers, though the 500-cap still drains them. The boundary is the message minimizing `|cumulative bytes − ratio·total|`; ties break toward the earlier message. `--ratio` defaults to 0.8.
- **Floor.** Plan refuses below a minimum eligible count (default 100, `SHUNT_ROOM_COMPACT_MIN`) — compaction pays off on long streams, and on a short one it merely relocates the reading into a digest. An explicit `--through` commit is the manual override: the operator owns the floor, the default just declines to propose under it.
- **Commit mechanics.** Under `.seq.lock`: non-summary live `NNNN` ≤ `--through` rename into `.archive/` (summaries exempt), then the digest message mints the next `NNNN` — it lands at the live tail carrying `summary_through = N`, the placement §17.5's cursor relies on — then `meta.compacted_through = N`. The order is chosen so a mid-rename failure is indistinguishable from ordinary cap-archival and a re-run converges; a commit retried after the digest was written is idempotent — an existing live summary with the same `summary_through` is returned, not duplicated.
- **Commit validation.** `--through ≤ compacted_through` is rejected outright — that range is already covered. There is no requirement that N exist in live: cap-archival makes the live stream a contiguous suffix, and digesting already-archived history is legitimate. Before mutating, commit warns — loudly, never gating: the usual non-member roster note; covering the newest live message warns it may be unseen; every reader whose `last_read_n < N` is named on stderr.
- **Concurrency.** The critical section mirrors `say`'s mint+drain shape and serializes against it. Lock-free readers can observe a partially-moved range, so a NotFound during message load is a silent skip, not a corruption warning — a deliberate rule: moved ≠ corrupt, and §17.5's hole-crossing already covers the read side.

Non-goal, restated for this subsection: no auto-compaction and no threshold nudges — compaction triggers only on an agent's explicit `compact` call, because the digest-quality decision belongs to whoever writes the digest.

## 18. Ops dashboard (`shunt serve`)

A read-only browser view of the dev-side state — the same truth `status --all` computes, pushed over websocket instead of polled at a terminal. It observes; it never mutates. Every write path stays on the CLI, so the dashboard needs no auth model beyond reachability of the port.

`shunt serve` is a five-leaf command: bare `serve` prints its help; `serve run` is the foreground `std::net::TcpListener` (thread-per-conn — no async runtime, same discipline as the rest of the binary); `serve up` self-daemonizes exactly like `shunt start` (spawn `current_exe serve run …`, `process_group(0)`, stdio → `$SHUNT_HOME/serve.log`, wait for `$SHUNT_HOME/serve.pid`); `serve down` verifies the pidfile's `/proc` cmdline identity (`shunt* serve run`) before SIGTERM — the run handler removes the pidfile and `serve.ver` on TERM/INT. `serve restart` shares the identity check but sends SIGUSR2: the run handler re-execs its own argv in place (§3.5a) — the CLOEXEC listener drops at execve and the new image rebinds the port, a sub-second refusal window for new connections plus one dropped WS conn. `run` writes `serve.ver` beside the pidfile as the capability gate — `restart` (and `upgrade`'s serve handoff) refuses an image without it, since a USR2 there would default-kill, naming the `serve down`+`serve up` remediation. `serve status` reports liveness, recorded version, the endpoint parsed out of the running argv, handoff capability, and the log path (`--json`). The lifecycle verbs converge like the queue's: `up` on a live instance reports `already up`, `down` on nothing reports `not running` and exits 0, `restart` with nothing live falls through to `up`. Convergence never silently drops intent: an explicitly-passed flag that differs from the running argv makes `up` refuse (naming both values) and `restart` warn — re-exec preserves the running argv, so flag changes need `down`+`up`. Flags (`--bind`, `--port`, `--token`, `--interval-s`) live on `run`/`up`/`restart`. Routes:

| route                                  | what                                                                                                                                                     |
| -------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `GET /` + `/<asset>`                   | the embedded SPA — `web/dist` baked into the binary at build time (a missing dist embeds a placeholder page naming the pnpm recipe; the API still works) |
| `GET /api/snapshot`                    | the current merged snapshot as JSON — the same object ws pushes                                                                                          |
| `GET /api/room/<name>`                 | one room's roster view + newest 200 messages of the merged live+archive stream                                                                           |
| `GET /api/logs/<c>/<q>?pattern=&tail=` | a `logs -n <tail> -- <pattern>` wire op to the resolved queue — raw stdout, same as the CLI                                                              |
| `GET /api/script/<c>/<q>?name=`        | a `cat -- <name>` wire op to the resolved queue — the entry's task.sh verbatim; backs the pending-entry script pane (a queued entry has no log yet)      |
| `POST /api/exec`                       | `client exec` passthrough, `{client, cmd}` → `sh -c` — the sole non-GET route, scoped to read-only probes (ps/nvidia-smi/df); it writes no queue state   |
| `GET /ws`                              | websocket upgrade; server→client text frames only                                                                                                        |

The snapshot is `{type:"snapshot", protocol, server, ts, clients, local}` with two independently-ticking sections: `clients` is `status::probe_all` — the identical per-queue wire probe `--all` runs, including `resources` (GPU util/VRAM, RAM, load1) — recomputed every `--interval` seconds (min 2; one ssh exec per queue per tick, parallel as in `status --all`); `local` is spool entries, the task ledger's newest 100, and `room::list` computed for `admin`, every 2 s because fs reads are free. A section failure lands as `{error}` inside the payload — a dead box or unreadable dir degrades, the broadcast itself never dies. New connections receive the latest snapshot immediately, then every broadcast after; a conn that falls a 64-frame buffer behind is dropped rather than allowed to stall the fan-out.

Two rules the read promise depends on: room streams are served through the §17.5 audit path (`ReadMode::Range` — `admin`'s cursor never advances, so `status --all`'s unread counts stay honest), and the only non-GET route is `POST /api/exec`, an exec passthrough whose scope is process-level inspection — it mutates no queue state.

**Binding and auth.** Default `127.0.0.1` — reach a remote dev machine with `ssh -L 8747:127.0.0.1:8747 <dev>`, which keeps the port inside the existing SSH trust boundary and needs no credential at all. A non-loopback `--bind` is refused without `--token`; the token is then required on every route as `?token=`, `x-shunt-token`, or `Authorization: Bearer`. Token auth is a reachability gate, not an identity system — there is still exactly one implicit identity, `admin`.

**Frontend.** `web/` is a pnpm + Vite + React + Tailwind SPA built to `web/dist` (`just web`); `build.rs` embeds dist as a static table, so the shipped binary stays a single scp'd artifact — the frontend toolchain is a build-time concern only. The vite dev server proxies `/api` and `/ws` to a running `shunt serve` for the edit-refresh loop. Theming is token-driven (CSS vars under `[data-theme]`): a system/light/dark switcher lives in the header, persists to `localStorage`, and `index.html`'s inline init script applies it pre-paint — its sha256 rides in the CSP header, so editing the script means recomputing the hash. Navigation is hash-routed behind a sidebar (`#/machines`, `#/queues/<client>/<queue>`, `#/tasks`, `#/rooms/<name>`) with live badges (running total, spool depth, unread msgs): Machines is the fleet glance — reachability, facts, expiry/billing, queue chips that deep-link; the queue page owns detail — gauges with rolling sparklines, declared-vs-measured entries, per-entry log tails re-fetched each tick, the probe box; room deep links survive refresh and sharing, `?token=` included.
