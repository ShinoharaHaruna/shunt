# shunt design spec

shunt is the task-scheduling interface layer between an AI agent and hourly-billed cloud GPU instances (AutoDL-style: no systemd, containers that may vanish, billing tied to wall-clock uptime). It exists so that an agent — or a human — can submit an experiment, let a rented box run it, and have the box power itself off when the work is done, without anyone watching the queue.

The name is the rail-yard metaphor: the server is the marshalling yard, each client box is a destination siding, and `pending/`, `running/`, `done/`, `failed/` are the four tracks. Moving a task between tracks is a filesystem `mv`.

This document is the behavioral spec for the Rust rewrite. The proven bash implementation is archived under `reference/bash-queue/` and is the oracle for every semantic in §3. When behavior is ambiguous, the bash source wins.

## 1. Positioning and non-goals

shunt is a FIFO + priority-band + at-least-once experiment queue for rented GPU boxes, operated headlessly by agents over SSH. It is not a CI system, not Slurm, not a task spooler clone.

FIFO is the _claim_ order, not the execution shape: the client runs a concurrent worker pool (`workers`, live-tunable), so several tasks run in parallel and the daemon's job is to keep the box saturated, not serialized.

Non-goals, by deliberate exclusion:

- No DAG dependencies between tasks. Use Airflow.
- No automatic retry of arbitrary failures. A task opts into rescheduling explicitly (rc 75 + `# shunt:retry=N`, §3.9); everything else still lands in `failed/` and the agent decides. Declared resource estimates are annotations for the agent's estimation loop, never admission gates (§3.16).
- No user-facing HTTP API. The client CLI is the API; SSH carries it.
- No multi-machine load balancing or automatic client selection. Task submission always names the client explicitly; matching task requirements to machine facts is the agent's job, with facts exposed via `shunt clients --json`.
- No remote power-on. Billing on AutoDL-class vendors is bound to the console "power on" action a human (or an upstream automation outside shunt's scope) performs. shunt only ever powers a box _off_.

## 2. Architecture

```text
dev machine (server, always on)                experiment box (client, on-demand)
┌──────────────────────────────────┐           ┌────────────────────────────────┐
│ shunt                            │           │ shunt (same binary)            │
│  registry/  clients + projects   │   SSH     │  pending/ running/ done/       │
│  spool/     server-side staging  │ ────────► │  failed/ workers STOP          │
│  dispatcher spool → client       │  exec +   │  shutdown_on_idle ...          │
│  status    aggregate --json      │ ◄──────── │  shunt daemon (worker pool,    │
└──────────────────────────────────┘  JSON     │  idle poweroff, probe)         │
                                               └────────────────────────────────┘
```

- One Rust binary `shunt` runs on both sides. Server subcommands operate the registry and spool; client subcommands operate the local queue.
- The wire protocol is SSH exec: the server runs `ssh <endpoint> 'shunt <subcmd> --json'`. No agent port, no daemon on the server side beyond the dispatcher loop, no extra process on the client beyond `shuntd` (the queue daemon embedded in `shunt`).
- Machines ↔ projects is many-to-many. A client (machine) has one queue. A project binds to one or more clients via `[[bind]]` entries. A submitted task names a project and a client; the binding supplies `workdir` and the env overlay applied when the task runs.
- Client lifecycle: a human pays and powers the box on in the vendor console → `shuntd` starts (dispatcher auto-heal: if SSH is reachable but the daemon pidfile is dead, the server runs `shunt start` before pushing) → tasks are claimed → when the queue drains, the client's own idle-shutdown powers the box off. No server-to-box wake path exists.

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
| `daemon.log`         | structured JSONL event stream (see §3.9); rotated to `daemon.log.1` past `SHUNT_DAEMON_LOG_MAX` (default 1 MiB), one generation kept |
| `.seq` + `.seq.lock` | enqueue counter under flock; a corrupt `.seq` fails the enqueue with remediation, never reseeds silently (§3.2)                      |

Per-entry sidecars beyond `task.sh`/`log`/`pid`/`exit`: `usage` (measured resource peaks, §3.16), `attempts` + `retry_after` (reschedule bookkeeping, §3.9; `retry_after` lives in pending/ only and is removed on claim), `timeout` (budget-death marker, §3.9), `shunt_error` (quarantine note).

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

### 3.5 Daemon loop

Poll cycle is 2 s (`POLL` env overridable for tests). Each cycle, in order: `reap` → if `STOP` present, drain; else `dispatch`, then check idle-shutdown. When a STOP drain completes with `shutdown_on_idle` armed, the daemon honors it before exiting (§3.10).

On startup:

1. Create the four track dirs.
2. Write `daemon.pid`.
3. Register the qroot in the box-level registry (§3.11).
4. Log `daemon_start`.
5. Run `requeue_orphans` (§3.6).

On TERM/INT/HUP: log, send TERM to every tracked task pgid, wait 2 s, send KILL to any survivors, reap them into `failed/`, then exit 143 after removing `daemon.pid` and the registry entry — with one exception mirroring §3.10: if `shutdown_on_idle` is armed and `pending/` is empty, the daemon first honors the flag (grace countdown, then fire), because exiting without firing would orphan it just as in the STOP path; the pidfile and registry entry are kept for the countdown, and a further signal during it cancels the countdown and exits promptly. If `pending/` is non-empty the flag stays armed — powering off would strand queued work — and a `signal_left_shutdown_armed` WARNING is logged so the orphaning is loud; a restarted daemon drains the queue and honors the flag naturally. (TERM arriving as part of an external poweroff needs no special case: firing the shutdown command again is harmless.)

### 3.6 Orphan requeue (at-least-once core)

Any `running/` entry present at daemon start means a previous daemon died mid-flight. For each:

1. If its `pid` file exists and `kill -0 -- -<pgid>` succeeds, the task process group is _still alive_ — log an `orphan_live_pgid` WARNING and requeue anyway. The task's own guard makes the rerun a no-op. (This happens when the daemon dies but a detached task survives.)
2. Strip any stacked `0000_requeued_` prefixes from the basename in a loop — a task requeued twice must not accumulate prefixes.
3. `mv` the task to `pending/0000_requeued_<clean>` (front of queue).
4. Its `log` moves to `failed/<clean>.interrupted.<unixts>.log`.
5. Remove the `pid` file.

If the requeue `mv` fails (the `0000_requeued_<clean>` target already exists, permissions, full disk) the entry must not be left in `running/`: it is moved to `failed/` with a `shunt_error` note and a `requeue_failed` event is logged, because a `running/` entry with no `pid` file is invisible to `shunt kill` and would ghost forever. If even the quarantine move fails, a `ghost_entry` WARNING is logged.

Then sweep legacy strays: any remaining `running/*.pid` files are removed; any remaining `running/*.log` moves to `failed/<name>.interrupted.<unixts>.log`. Dir entries carry their sidecars inside, so they cannot stray.

### 3.7 Dispatch

- Read desired workers (see §3.8). While `len(running) < workers`:
  1. Snapshot the `pending/` list once per cycle (`LC_ALL=C` name sort) and walk it — re-reading the head after every failed claim would hot-spin on the same unclaimable entry for the whole poll.
  2. A rescheduled entry is not claimable until its `retry_after` timestamp passes (§3.9): not-yet-due candidates are skipped this cycle; an expired or unparseable sidecar counts as due and is removed on the way through.
  3. Claim by `mv pending/<f> running/<f>`. If the entry vanished mid-race (a concurrent `shunt cancel` won), skip to the next candidate — the `mv` result is the recheck, not a prior existence test. If the `mv` fails but the entry still exists (a name collision with a leftover `running/<f>` dir, permissions), the entry can never be claimed: log `claim_failed` and quarantine it to `failed/` with a `shunt_error` note rather than letting it poison every future poll's head-of-queue.
  4. `chmod +x`, write log header (`=== task`, `=== start <iso>`).
  5. Spawn via `process_group(0)` (setsid) so the task's pgid equals its leader pid; stdout/stderr append to `running/<name>/log`.
  6. Write the leader pid **and its /proc start_time** to `running/<name>/pid` (`<pgid> <start_time>`), record pgid → entry name. The start_time pin lets `shunt kill` distinguish the leader it spawned from a recycled pid (§3.9a below).
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

Generated task (single generator — `agent::exec::render_task` — shared by `new`/`mktask` and server-side `submit`):

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

The timeout is a wall-clock budget counted from spawn, not a retry: at the deadline the daemon sends TERM to the task's verified pgid (§3.9a discipline), escalates to KILL after a 10 s grace, and finalizes per §3.9. A daemon restart re-times the requeued rerun from its new spawn — at-least-once re-executes the whole task, budget included. A guard-hit task exits before the budget can ever matter. No `cd` line — the runner already places cwd at `workdir` (the bash version's generated `cd` was redundant). The guard convention is the contract that makes at-least-once safe: tasks must be idempotent or guard on the artifact they produce.

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

**Permissions are enforced, not assumed.** `ssh.password` is plaintext and `keys/` holds private keys, so every shunt-managed dir under `$SHUNT_HOME` is created/re-tightened to `0700` and every file (rows, spool entries, state, tunnel pid/port/log) to `0600` — umask never widens the registry, and saving re-tightens a file a user loosened. Paths outside `$SHUNT_HOME` (a user-supplied `ssh.key`) keep their own permissions.

**SSH is fully managed.** `client add`/`bootstrap` generate a dedicated ed25519 keypair at `keys/<name>` and fill `ssh.key` — registration alone makes the row usable. Deploying the public key is the one step that needs existing trust: when `ssh.password` is set, `bootstrap` appends the pubkey via a single password-only sshpass session (no key offers, idempotent `grep -qxF`); without it, bootstrap prints the `.pub` to install once via the vendor console or a manual `ssh-copy-id`. **A `shared` row never auto-deploys**: authorized_keys is the one write shunt makes outside `<qroot>`, and other tenants' consent is not ours to assume — point `ssh.key` at an already-authorized key or install the `.pub` manually. Host-key checking is TOFU (`StrictHostKeyChecking=accept-new`) against a per-client `known_hosts/<name>` file, so first contact never prompts and the user's own `~/.ssh` stays untouched. Per-client files are load-bearing: two rows may legitimately point at the same host:port (different boxes behind one NAT endpoint) while the boxes carry different host keys — a shared file would pin the first TOFU'd key and deadlock the second client on `REMOTE HOST IDENTIFICATION HAS CHANGED`.

**qroot ownership claim.** Two independent shunt installs pointed at the same `<qroot>` silently merge into one queue: sequence numbers race between two dispatchers and one daemon ends up serving both. Before any write, `bootstrap` runs a single remote claim script on `<qroot>/.shunt-owner` holding the client's ssh public key line (read from `{ssh.key}.pub` or derived via `ssh-keygen -y`): absent → create-exclusive via `set -C` (`O_EXCL`, so a simultaneously claiming bootstrap cannot interleave — a later install never preempts an earlier owner); present → `grep -qxF` compare. A foreign owner answers FOREIGN and bootstrap fails with "claimed by a different shunt install — pick a different qroot"; OWNED (self, the re-bootstrap/upgrade path) and CLAIMED proceed. Rows without a derivable pubkey skip the claim rather than weaken it. `client add` also warns when another row already targets the same host:port+user+qroot tuple, surfacing the collision at registration. This is an accident guard, not a security boundary — any tenant can `rm` the file; it exists to make collisions loud, not to fence out a deliberate claimer. Same-account multi-install is the supported topology (§3.11 registry coordinates it); cross-account sharing is the `shared` row's business.

**Host-key rotation** (rebuilt containers change their key): `shunt client hostkey <name>` runs `ssh-keygen -R` scoped to that client's `known_hosts/<name>` and kills the mux master, so the next connect re-TOFUs cleanly. Anything — agent or human — must never write to `~/.ssh/known_hosts`; a stale entry there surfaces as an unexplained REMOTE HOST IDENTIFICATION HAS CHANGED on the user's own connections.

**Endpoint drift** (vendors that reassign IP/port across restarts — `endpoint_rotates = true` in the row): `shunt client update <name> --host <h> --port <p>` rewrites the row AND clears the live state that would stay pinned to the dead endpoint — known_hosts entries under both the old and the new host:port spec (a reassigned tuple can carry a foreign key that hard-fails TOFU), the mux master (it answers for its original destination regardless of the requested target), the conn-fail streak, and any recorded tunnel (with a best-effort ALL_PROXY cleanup over the new endpoint). Hand-editing the row does none of this. Also updatable: `--user`, `--password` (empty clears), `--endpoint-rotates` (`--no-endpoint-rotates` clears a mislabeled row). Server-local: no reachability check; verify with `status -c`. Three reminders update emits rather than fixes: a `--user` change alone keeps the deployed pubkey working (authorized_keys is user-independent) but a rebuilt container resets it — re-run `bootstrap` in that case; a new endpoint already registered on another client row warns (legal behind NAT — per-client known_hosts keeps the keys apart — but a genuine duplicate should be `client rm`'d); and non-endpoint fields (`qroot`, `deploy_root`, `expire`, `env_setup`) are deliberately out of scope — hand-edit the row, remembering a moved `qroot` means relocating the remote queue, not just a string. Row writes are atomic (temp + rename), so a concurrent `dispatch` sweep never reads a torn TOML; the residual ordering hazard — a sweep connecting between the row save and the known_hosts/mux clearing — is why `update` should not run concurrently with a dispatch loop.

**Lifecycle exits.** `client rename <old> <new>` moves the whole identity (row, managed keypair, mux socket, conn-fail streak, tunnel state, queued spool refs, project `[[bind]]` rows). `client rm <name>` deregisters: remotely it stops the daemon and revokes our authorized_keys line — matched by the key blob, not its comment, so a renamed client still revokes cleanly — then locally removes row, keys, mux socket, state, spool entries, and bindings. `--purge` additionally `rm -rf <qroot>` on the box — the qroot is validated first (absolute, ≥2 components, no system dir / home dir / `~` / `..` / suspiciously short path) and a bad value aborts the deregistration; bound workdirs are reported, never deleted — task data is the user's call. Remote steps are best-effort: an unreachable box warns but never wedges the deregistration. Ad-hoc box probing goes through `client exec <name> -- <cmd>` (managed ssh: mux + retries + our key/known_hosts).

**Connection discipline.** Every ssh/scp exec rides a ControlMaster mux (`cm/<name>.sock`, `ControlPersist=60`) — one TCP session per client instead of one handshake per op. Boxes fronted by startup-limited proxies (observed: `Exceeded MaxStartups` on a mainland vendor's ssh gateway) flap hard under per-op connections; muxing removes the storm. rc-255 failures also retry 3× inside a call — exec and scp (`copy_to`) alike — so a single flap never surfaces; only an exhausted streak feeds the endpoint-drift counter. The deploy path deliberately keeps `ControlPath=none` — a password-auth session must never become the mux master.

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
| `shutdown_cmd`                                                | poweroff command pushed to `<qroot>/shutdown_cmd` at `shutdown on` arm time — point at `true` or a marker script in test envs                                                                                                                                      |
| `disabled`                                                    | opt-out: row stays on disk but is not a usable client (skipped by listings, rejected by load)                                                                                                                                                                      |
| `ssh.host`, `ssh.port`, `ssh.user`, `ssh.key`, `ssh.password` | exec endpoint; `password` feeds the one-time `sshpass ssh-copy-id` key deploy                                                                                                                                                                                      |
| `qroot`                                                       | queue dir on the box                                                                                                                                                                                                                                               |
| `deploy_root`                                                 | vendor work root (AutoDL: `/root/autodl-tmp`)                                                                                                                                                                                                                      |
| `facts`                                                       | table filled by `probe`/`bootstrap`: `gpu`, `cuda`, `driver`, `cores`, `mem_gb`, `disk_free_gb`, `python`, `can_root`, `shutdown_cmd` (poweroff candidate)                                                                                                         |
| `expire`                                                      | billing lifetime: `unlimited` (owned box, localhost), `metered` (pay-as-you-go until balance), or an ISO-8601 deadline — write it WITH an offset (`+08:00`/`Z`): a bare datetime has no zone, so `status` cannot turn it into `expires_in_s` (§9). Empty = unknown |
| `env_setup`, `shell`                                          | task preamble: snippet sourced before every task + the shell it runs under (default bash); pushed to `<qroot>/env_setup`/`shell` by `client env`/`bootstrap`                                                                                                       |
| `endpoint_rotates`                                            | vendor reassigns IP/port across restarts (AutoDL does not); steers the escalation hint after `CONN_FAIL_ESCALATE` consecutive ssh failures                                                                                                                         |
| `shared`                                                      | consent mark: the box hosts other people's work — server-side `shutdown on` refuses to arm idle poweroff while set (`client add --shared`); clear the field only when the user confirms sole use                                                                   |
| `exit_on_idle`                                                | residency preference pushed to `<qroot>/exit_on_idle` by `client env`/`bootstrap` (`client add --exit-on-idle`): daemon exits on drain instead of staying resident (§3.11a)                                                                                        |
| `billing.hourly`, `billing.currency`                          | cost metadata surfaced to the agent                                                                                                                                                                                                                                |

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
workdir = "/root/autodl-tmp/RiboWorld" # repo location on the box
env = { PATH_PREPEND = "/root/.local/bin" }
[[bind]]
client = "gerna-b"
workdir = "/data/RiboWorld"
```

`[defaults].env` merges beneath each binding's `env` at submit (binding wins per key). `repo`/`desc` are documentation fields today — `project add --repo --desc` fills them, `project ls` prints the desc column — and `repo` is where a future `shunt sync` would pull task code from. The remote checkout path stays a per-binding `workdir` concern; different boxes mount differently.

### 4.2 Vendor templates

A vendor template is a plain client-row TOML used as a seed for `client add` — no special syntax, FILL markers are just comments. Search order, first hit wins: `$SHUNT_VENDOR_DIR`, then `$SHUNT_HOME/vendors/` (user-supplied templates live with the registry they seed), then the repo's `etc/vendors/` (a compile-time path that only resolves for builds from the source tree). `shunt client add --template autodl` seeds `ssh.user=root`, `deploy_root=/root/autodl-tmp`, `qroot=/root/autodl-tmp/queue`, leaving only `ssh.host`, `ssh.port` (and optionally `ssh.password`, `billing`) to fill — `ssh.key` is generated automatically. Templates are defaults for the row file, nothing more — after `add`, the client row stands alone. A bad name errors out listing every template the search dirs actually contain.

### 4.3 Box-side SOCKS5 tunnel

Boxes with poor international connectivity (mainland-China GPU vendors) get their work done through a reverse dynamic forward: `ssh -N -R <port>` opens `127.0.0.1:<port>` as a SOCKS5 listener **on the box**, tunneled back to exit via the dev machine's network. `shunt tunnel up -c <name> [--port 1080]` spawns that ssh detached (pidfile + log under `tunnels/`), then upserts `ALL_PROXY=socks5h://127.0.0.1:<port>` into the box's `<qroot>/env` — tasks pick the proxy up with zero task-level edits, because the runner strips ambient proxy vars and then injects the env file. `tunnel down` kills the ssh and removes the env line; `tunnel status` reports liveness and the port. The `h` in `socks5h` makes DNS resolve remotely too, which is the point — a poisoned resolver stays unused. ssh-side prerequisites: OpenSSH ≥7.6 client and `AllowTcpForwarding` on the box's sshd (on by default).

**Self-heal.** The tunnel's own ServerAlive policy kills a half-open ssh in ~90s, which is the common death: the ssh dies while the box and its queue daemon stay up — and `ALL_PROXY` left in `<qroot>/env` then poisons every subsequent task into a dead proxy (failures surface as generic network errors). So the dispatcher sweep treats a recorded tunnel (pid or port state file) as a liveness responsibility alongside the daemon, even when the spool is empty; unused registry clients without a tunnel record and local clients without spooled work are not probed. On any sweep where the client probe succeeded (`Reach::Up`), a recorded-but-dead tunnel triggers one reconnect attempt via `tunnel up`; if the reconnect fails, ALL_PROXY is stripped from the env file (fail-over to direct egress) while the state files stay, so the next successful-probe sweep retries. Sweeps where the probe is skipped by conn-fail backoff also skip the tunnel check — the throttling is never bypassed — and a dead box is untouched entirely (an env edit needs ssh anyway, and no tasks can run). `tunnel status` reports this intermediate state as `stale`: pid dead, state files present, env possibly still poisoned.

### 4.4 Spool and dispatcher

`submit` lands a task in `spool/` as `spool/<task-id>.json` (metadata: project, client, slug, guard, argv, submitted_at) + `<task-id>.sh` (rendered task body). Persisted before returning, so submitting while the box is off is normal operation.

**`--file` submissions get the same binding injection as argv submissions.** The binding's `workdir`/`env` are merged into the file's comment head as `# shunt:workdir=` / `# shunt-env:` directives, injected _above_ the file's own directive block so the file's own directives win per key (the file is the task author speaking; the binding is context). An explicit `--guard` flag injects the guard line ahead of the file's first code line, so the flag always runs first — an embedded check still runs after it, harmlessly.

**The guard contract is enforced by warning, not by flag.** A staged task whose body carries no guard check (no `--guard`, no embedded `guard hit:` line in a `--file`) triggers a loud stderr warning at submit: at-least-once means a lost ack, an orphan requeue, or a live-pgid requeue reruns the body, and an unguarded task then double-executes (double GPU burn, duplicated result writes). Naturally idempotent tasks are a legitimate unguarded case, so submit still proceeds.

Two-phase dispatch, at-least-once end to end:

1. Dispatcher (server-side loop, poll interval from `server.toml`) scans `spool/` oldest-first.
2. For each entry bound to client C, over the row's transport: `shunt enqueue - <slug>-<shortid> --json --root <qroot>` with the body on stdin.
3. Only on a clean ack (exit 0 + JSON `{ok: true, name: ...}`) does the server remove the spool entry. Any failure leaves it for the next poll.
4. Dupes from a retry after a lost ack are deduped box-side: the rendered body carries `# shunt:id=<task id>` and the remote returns the existing entry (`{ok, name, deduped: true}`) instead of minting a second number (§3.3). The task guard remains the backstop for remotes that predate the directive, and the server task id is also embedded in the slug (`<slug>-<shortid>`) so a re-pushed task is recognizable in logs. A spool entry staged before this change carries no directive in its body — it simply allocates a fresh number per push, the pre-dedup behavior.

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

| op                                       | request                               | response sketch                                                                                                                                                                                |
| ---------------------------------------- | ------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `enqueue`                                | task body on stdin; `--slug`          | `{ok, name, deduped?}` — `deduped: true` when `# shunt:id=` matched an existing entry                                                                                                          |
| `new`                                    | `--slug`, `--guard`, `--` argv        | `{ok, name, deduped?}`                                                                                                                                                                         |
| `mktask`                                 | same as `new`                         | task body on stdout                                                                                                                                                                            |
| `status`                                 | —                                     | `{daemon:{alive,pid}, workers:{value,source}, running[], pending[], done_recent[], failed_recent[], stop, shutdown:{armed,grace_s,cmd}, resources?, pending_detail?, running_detail?}` (§3.16) |
| `cancel`                                 | pattern                               | `{ok, name}` or error on 0/multi match                                                                                                                                                         |
| `prio`                                   | pattern, n                            | `{ok, from, to}`; error if target exists                                                                                                                                                       |
| `kill`                                   | pattern                               | `{ok, pgid, escalated}`; `{ok, gone}` when the recorded leader is dead/recycled — nothing was signaled                                                                                         |
| `workers get` / `workers set N`          | —                                     | `{workers, source}`                                                                                                                                                                            |
| `shutdown on [grace]` / `off` / `status` | —                                     | `{armed, grace_s, running, pending}`                                                                                                                                                           |
| `probe`                                  | —                                     | `{gpu, driver, cuda, cores, mem_gb, disk_free_gb, python}`                                                                                                                                     |
| `logs`                                   | `-n N` and/or `-- pat`; bare = daemon | text stream (`-f` is local-only: ssh exec is collected under the §5 deadline, so wire follow is refused, not streamed)                                                                         |
| `start` / `stop` / `daemon`              | —                                     | `{ok, pid}` / `{ok}` / runs the loop (foreground; `start` daemonizes)                                                                                                                          |
| `init`                                   | `--qroot`                             | `{ok}` creates dirs                                                                                                                                                                            |

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

1. `shunt client add rw-a --template autodl` — edit host/port/key in the row.
2. `shunt client bootstrap rw-a` — scp the binary, remote `shunt init` + `shunt start`, then wire `shunt probe` (the binary is already on the box, so there is exactly one probe implementation); facts merged into the row.
3. `shunt project add riboworld` + `shunt project bind riboworld --client rw-a --workdir /root/autodl-tmp/RiboWorld`.
4. `shunt submit -p riboworld -c rw-a --guard runs/x.json -- uv run python ...` → lands in `spool/`.
5. Dispatcher pushes when the box is reachable; box runs it; on drain, idle-shutdown powers off. Overnight submissions sit in spool until the human powers the box on; the dispatcher's auto-heal restarts `shuntd` and delivers.

## 8. Probe spec

`shunt probe` collects, emitting JSON — it is the only probe implementation; bootstrap ships the binary before probing and `client probe` runs it over the wire (an unbootstrapped box answers with the binary-drift error from §5):

- `gpu`: `nvidia-smi --query-gpu=name,driver_version --format=csv,noheader`; absent → `null` (CPU-only boot is a valid state — AutoDL no-card boots drop the GPU).
- `cuda`: `nvidia-smi` header parse.
- `cores`: cgroup-aware — `cpu.max` quota/period (v2) or `cpu.cfs_quota_us`/`cpu.cfs_period_us` (v1); `nproc` only as fallback and marked suspect.
- `mem_gb`: `memory.max` / `memory.limit_in_bytes`; `free` only as fallback, same caveat. Never size off host-reported values.
- `disk_free_gb`: `df` on `deploy_root`.
- `python`: `python3 --version`.
- `libc`: `ldd --version` parsed to `glibc 2.35` / `musl 1.2.4`; absent when undeterminable. The most deploy-relevant fact — it decides whether a shipped binary can even start (§15).
- `shutdown_cmd`: best-guess poweroff command — `systemctl poweroff` when `/run/systemd/system` exists and `systemctl` is found, else `shutdown`, else `poweroff`, else absent. Advisory: a container may carry the binary yet lack CAP_SYS_BOOT; the row's `shutdown_cmd` field and `--cmd` flag still override it at arm time.

Facts are advisory registry data for agent-side matching, not enforced constraints.

## 9. Billing and cost surfacing

`billing.hourly`/`billing.currency` in a client row flow into `shunt status --all --json` output so the agent can reason about cost. `expire` carries the billing lifetime — `unlimited` for owned compute (localhost, lab GPUs), `metered` for pay-as-you-go (no deadline; burns until the balance does), or an ISO-8601 timestamp for subscription boxes — and `status` adds `expires_in_s` when the timestamp carries an offset. shunt never makes the decision — "instance has 40 min of paid time left but the next task needs 3 h → hold and notify" is agent logic taught in the skill file. shunt's job is accurate exposure of queue state, uptime context, and rates.

## 10. CLI surface

| command                                                                                           | side   | purpose                                                                                             |
| ------------------------------------------------------------------------------------------------- | ------ | --------------------------------------------------------------------------------------------------- |
| `shunt submit -p proj -c client [--guard p] -- cmd...`                                            | server | spool a task                                                                                        |
| `shunt submit -c client file.sh`                                                                  | server | spool a script file                                                                                 |
| `shunt status [--all] [--json]`                                                                   | server | aggregate client states                                                                             |
| `shunt spool ls\|rm`                                                                              | server | inspect staging                                                                                     |
| `shunt task ls\|show <id>\|events`                                                                | server | task ledger + outcomes                                                                              |
| `shunt client add\|update\|ls\|show\|bootstrap\|probe\|env\|templates\|hostkey\|rename\|exec\|rm` | server | registry                                                                                            |
| `shunt project add\|bind\|unbind\|ls\|show\|rm`                                                   | server | registry                                                                                            |
| `shunt dispatch [--once]`                                                                         | server | run the dispatcher loop                                                                             |
| `shunt tunnel up\|down\|status -c client`                                                         | server | box-side SOCKS5 tunnel                                                                              |
| `shunt daemon`                                                                                    | client | run queue loop (foreground)                                                                         |
| `shunt init`                                                                                      | client | create queue dirs                                                                                   |
| `shunt start` / `shunt stop`                                                                      | client | daemon lifecycle                                                                                    |
| `shunt enqueue [file\|-] [slug]`                                                                  | client | install into pending                                                                                |
| `shunt new` / `shunt mktask`                                                                      | client | wrap argv as task (guard)                                                                           |
| `shunt status`                                                                                    | client | queue summary                                                                                       |
| `shunt cancel` / `prio` / `kill`                                                                  | client | pending/running ops                                                                                 |
| `shunt workers [N]` (wire: `workers get`\|`set N`)                                                | client | pool size get/set                                                                                   |
| `shunt shutdown on\|off\|status`                                                                  | client | idle poweroff                                                                                       |
| `shunt room create\|ls\|say\|read\|decide\|members\|rm`                                           | server | project-membership coordination rooms (§17)                                                         |
| `shunt logs [-n N] [-f] [pat]` / `shunt dlog`                                                     | client | task/daemon logs                                                                                    |
| `shunt probe`                                                                                     | client | machine facts JSON                                                                                  |
| `shunt completions <shell>`                                                                       | local  | shell completion script                                                                             |
| `shunt version [-c client] [--json]`                                                              | local  | version+build info; `-c` forwards the `--version` flag (works on remotes that predate this command) |
| `shunt upgrade [--check] [--force] [-c client] [--json]`                                          | local  | self-upgrade from latest release; `-c` ships the latest musl build to the box via bootstrap (§16)   |
| `shunt feedback <title> ...` / `feedback ls\|retry`                                               | local  | file a GitHub issue (§14)                                                                           |

## 11. Testing strategy

Integration tests spawn a real daemon against a tmp `SHUNT_ROOT`; the poll interval is shortened via env, and `shutdown_cmd` is redirected to a script that touches a marker file. Must cover:

- dispatch order (filename sort; `0000_` band ahead of `0100_`)
- enqueue concurrency vs dispatch (`.seq` under lock; no dup numbers)
- delivery dedup: same `# shunt:id=` returns the existing entry across all four live tracks; concurrent same-key pushes mint exactly once; no-directive bodies always allocate
- `.seq` fault handling: corrupt counter bails with remediation; missing counter rebuilds from high-water
- `cancel` 0/1/many match; `prio` renumber incl. target-exists refusal
- `kill`: TERM → 10 s → KILL, rc recorded, lands in `failed/`; recycled-pgid refusal (leader start_time mismatch → `gone`, foreign group untouched)
- orphan requeue: dead daemon leaves `running/` entries → restart → front-of-queue, prefix strip, interrupted logs swept; requeue-target collision quarantines to `failed/`
- claim/finalize failure paths: unclaimable pending entry quarantined (no dispatch spin); failed finalize quarantines instead of ghosting in `running/`
- live-pgid orphan: WARNING + requeue anyway
- STOP drain: running task finalizes, then daemon exits; armed shutdown_on_idle is honored (grace countdown → fire) instead of orphaned
- idle-shutdown: arm → fire (marker), cancel via flag rm / new pending / STOP, grace default, final re-check
- workers resize: raise dispatches immediately; lower drains only; invalid value falls back to last-known
- env file overlay, workdir, proxy unset, thread caps
- dispatcher: spool → push → ack → delete; unreachable client retains; auto-heal restarts a dead daemon
- reschedule: rc 75 requeues same-name to pending/ with `attempts`/`retry_after`; budget exhaustion finalizes; OOM heuristic only fires for retry-declared tasks; `retry_after` gates dispatch until due
- usage telemetry: `usage` sidecar written per cycle, travels to terminal tracks; `status` reports resources + declared-vs-observed detail
- bootstrap compat (§15): GLIBC byte-scan (no refs → ship unconditionally); ldd parse over `2>&1`; refuse on older-glibc/musl/undeterminable box with musl-build remediation; release rescue verifies checksum + embedded sha; smoke `--version 2>&1` surfaces loader stderr
- feedback: sanitize redacts registry secrets (host/port/user/password/key/$HOME) and skips the common-name allowlist; persist-before-deliver leaves a pending artifact when delivery is disabled (`SHUNT_FEEDBACK_REPO=` empty); dedup returns the existing URL within 24 h; the hourly cap refuses but still persists; `retry` delivers pending entries; delivery itself runs against a stub `gh` on PATH (never the real API)
- rooms: concurrent `say` mints unique message numbers under `.seq.lock`; `read` prints only messages past the reader's `last_read_n` and marks it (`--all` shows the stream, still marks); `say`/`decide` mark their own message read; `ls`/`status` report `unread_msgs` without marking; a non-member `say` posts with `member:false` and names the roster on stderr; `decide` overwrites `meta.decision`; the 500-message cap archives oldest-first to `msgs/.archive/`; `room ls` suggestions group projects sharing an ssh endpoint; a >16 KiB body is rejected

## 12. Skill deliverable

`skills/shunt/SKILL.md` ships in the repo (no `shunt skill` subcommand). Contents: **a deployment-consent interview before `client add`** — the agent asks the user, not decides: shared box or dedicated (`--shared`), ssh key strategy (dedicated keypair + password deploy / reuse an already-authorized `--key` / manual console install), qroot placement, daemon residency (`--exit-on-idle`) — then register/bootstrap, add/bind a project, submit and guard conventions, query and interpret queue state and JSONL events, cost-aware practices (arm idle-shutdown only on dedicated boxes, watch billing metadata), splitting long experiments into checkpointable tasks so at-least-once reruns are cheap, and the feedback channel (§14) — an agent that hits a shunt defect or UX wart should file it via `shunt feedback`, with a reminder never to paste credentials into the body.

## 13. Repo layout

```text
shunt/
  Cargo.toml
  src/
    main.rs          # clap entry; calls cli::run
    cli/             # clap command tree (args.rs), dispatch (mod.rs,
                     # server.rs, agent.rs, feedback.rs, upgrade.rs — §16)
    server/          # registry/ (types, paths, state, io),
                     # fsutil (0700/0600 privacy), spool, dispatcher,
                     # tasks (ledger), tunnel, transport/ (ssh + local),
                     # status, room (coordination rooms — §17),
                     # client_ops/ (keys, lifecycle, update, hygiene, env,
                     # probe, bootstrap, compat + release fetch — §15)
    agent/           # FS queue ops, exec, shutdown flags, probe;
                     # daemon/ = poll loop (core), lifecycle, workers,
                     # orphan requeue (reap), idle-shutdown countdown (idle)
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
- `retry`'s id is a unique filename-stem prefix, like `task show`; omitted = all pending.

### 14.2 Pipeline

1. **Compose**: `[feedback/<kind>] <title>` plus a body of the report text, a context block (shunt version, wire protocol version, os/arch, optional client facts — emitted as fenced JSON so an agent can parse it), and a `filed by shunt feedback` footer. The report text is truncated at 60,000 bytes with an explicit `… [truncated]` marker — GitHub rejects issue bodies past 64 KiB, and an oversized report would otherwise persist pending forever.
2. **Sanitize — mandatory, the default repo is public.** The sensitive-value set is read straight off disk: every `clients/*.toml` parsed as raw TOML (typed row loading is bypassed — a disabled or unparseable row still holds secrets), plucking `ssh.host`, `ssh.port`, `ssh.user`, `ssh.password`, `ssh.key`, plus the `$HOME` prefix. Every occurrence of a value ≥ 4 chars in title+body becomes `<redacted>` — except an allowlist of collision-prone common names (`root`, `ubuntu`, `debian`, `admin`, `user`, `git`, `shunt`), because redacting "root" would mangle "root cause". Redaction count is reported on stderr. This guards the realistic leak — an agent pasting `ssh root@1.2.3.4 -p 12345` errors verbatim; a body can still embed arbitrary secrets, and that residual is stated here rather than claimed solved.
3. **Persist first (at-least-once):** `$SHUNT_HOME/feedback/<unixts>_<slug>.json` — `{kind, title, body, created_at, delivered_at, issue_url, issue_number, attempts}`, 0600 via `write_private`, slug = sanitized title. The artifact stores the post-sanitize text, so a `retry` can never deliver a pre-scrub draft — and `retry` re-scrubs anyway, because a client row registered between send and retry can have added secrets the persisted body still carries. Persist failure warns but does not stop delivery; if persist AND delivery both fail the composed body is dumped to stderr so the report survives in the caller's scrollback. Metadata scans (`ls`, the rate cap) read a header-only projection — bodies can be tens of KB and are retained only where needed (`find_dup`, `retry`).
4. **Deliver:** repo resolves `$SHUNT_FEEDBACK_REPO` > `server.toml` `feedback_repo` > compile-time `ShinoharaHaruna/shunt` — the env wins so a fork can redirect without editing files, and an **empty env value disables delivery deliberately** (test hook and user opt-out: the artifact persists, the error says so). Mechanism order: `gh` on PATH (`gh issue create --repo R --title T --body-file -`, body on stdin, URL parsed from stdout) → `GH_TOKEN`/`GITHUB_TOKEN` + `curl` POST to `api.github.com/repos/R/issues` (the token rides a 0600 `curl -K` config file, never argv — argv is world-readable via /proc) → fail with a remediation message (`install gh or export GH_TOKEN, then shunt feedback retry`). Each mechanism is tried in turn and the errors are reported together. No HTTP client is added — `std::process` shell-out keeps the dep list untouched.
5. **Report:** `feedback filed: <url>` on stdout; `--json` emits `{ok, url, number, deduped}`. Failures with `--json` emit `{ok:false, error, artifact}` before the nonzero exit — agents consume this command too.

### 14.3 Anti-spam

The reporter may be an agent sitting in a retry loop aimed at a public repo:

- **Dedup:** an artifact with identical `(kind, title, body)` delivered within the last 24 h short-circuits to the existing issue URL (`deduped: true`) — checked before the rate cap so a repeat report is never blocked.
- **Rate cap:** `SHUNT_FEEDBACK_MAX_PER_HOUR` (default 5) delivered artifacts in the last hour refuses delivery — the artifact still persists, and the error says to re-run `shunt feedback retry` later. `retry` runs the same resolve path (dedup, then cap): it is the cap's remediation, not a bypass — a looped caller cannot drain the ledger into a public repo. Only real deliveries count toward the cap (a dedup-hit copy consumed no API call).

## 15. Binary compatibility at bootstrap

`bootstrap` ships a binary to a box whose libc may predate the dev machine's toolchain. The shipped binary must run, not merely scp — a glibc-linked dev build on an old-glibc box previously ran only by weak-version luck, the loader warning buried in remote stderr that `exec` discards on success.

**Source order:** `--bin-path` > row `bin_path` > `current_exe()`. The row field exists because bootstrap doubles as the upgrade path — a musl build for an old-glibc box must survive re-bootstraps; when it is set, bootstrap prints both the local and the shipped version strings so drift is loud. An explicit path (flag or row) is the operator's choice: incompatibility fails with remediation rather than silently substituting a different binary.

**Check** — runs only when the chosen binary is glibc-linked. Glibc need is detected by an in-process byte-scan of the binary for `GLIBC_x.y` version refs (no `strings`/`readelf` dependency): no refs = libc-independent (musl static) and ships unconditionally.

1. `ldd --version` on the box over raw exec with `2>&1` (the transport surfaces stdout only). Parses to `glibc x.y` / `musl x.y.z` / undeterminable. This is deliberately not the wire `probe` — the binary the check would validate is not installed yet.
2. Box glibc ≥ the binary's maximum required GLIBC → compatible. Box glibc older, box musl, or box libc undeterminable → incompatible. Older is refused, not warned: missing weak-version refs run "by luck" until a touched code path crashes mid-task.
3. On incompatibility — along the auto source only — **rescue**: the local `--version` string `x.y.z (<sha> <date>)` names a commit. Download `shunt-v<x.y.z>-x86_64-unknown-linux-musl.tar.gz` + `SHA256SUMS-<target>` from the releases of `SHUNT_RELEASE_REPO` > `server.toml release_repo` > upstream default (`gh release download` first, `curl -L` fallback — public assets need no auth), verify the checksum, then run the extracted binary's `--version`: the embedded sha must equal the local sha — the artifact self-certifies it is the same code. A local build with no embedded sha accepts on `x.y.z` alone; `-dirty` warns that the shipped build lacks the uncommitted changes. Any rescue failure falls through to the hard error, whose remediation names the musl build recipe (`rustup target add x86_64-unknown-linux-musl && cargo build --release --target x86_64-unknown-linux-musl`) and `--bin-path`.
4. **Post-install smoke:** after the stage-then-mv install and before remote `init`, exec `<bin> --version 2>&1` — nonzero fails bootstrap with the output; the merged loader warnings print on stderr locally. Success echoes the remote's own version string (`remote shunt 0.2.0 (faae5d9 …)`).

The `libc` probe fact shares the ldd parser and lands in the row's `facts` for agent-side matching (§8). The release workflow packs both musl and gnu artifacts per tag; bootstrap only ever fetches the musl one — it runs on every libc the gnu one can and beyond.

## 16. Self-upgrade (`shunt upgrade`)

`shunt upgrade` fetches the newest non-prerelease GitHub release (gh → curl against the public releases API; repo from `$SHUNT_RELEASE_REPO` > `server.toml release_repo` > the built-in upstream) and installs it over `current_exe`:

- **Local** (default): `--check`/`--json` report `{current, latest, newer}` without touching anything. When the release is newer (semver triple compare; a release outranks its prereleases), the asset matching the _running binary's_ linkage is fetched — the §15 byte-scan decides: glibc-linked → `x86_64-unknown-linux-gnu`, static → musl. Integrity is `SHA256SUMS-<target>`; identity is the extracted binary's own `--version` reporting the tag's version (upgrade deliberately does NOT reuse rescue's same-sha check — a new version carries a new sha). Install is stage-then-rename (`<exe>.new` → `mv`), so replacing a running binary is ETXTBSY-safe and atomic. `--force` reinstalls the latest tag even when versions match (e.g. a dirty dev build reverting to the release). A non-writable install location fails with the package-manager remediation.
- **Remote** (`-c <client>`): upgrades the BOX's binary to the latest release. The box always gets the musl asset (static — runs on any libc, so §15 compat can't fail) delivered through `client bootstrap --bin-path` — owner claim, stage-then-mv, `--version 2>&1` smoke, init/start/probe all apply for free. A remote already reporting the target version is skipped (`--force` overrides); non-x86_64 boxes (checked via `uname -m`) are refused with the build-and-`--bin-path` recipe. Stage-then-mv replaces the binary file only — a live daemon keeps its old in-memory image (`start` on it is a no-op by design), so the upgrade ends by warning that the daemon is stale until `shunt stop` (graceful drain, never a kill) plus `shunt start`; the timing is the operator's call because queue-semantics changes don't apply until then.

`rescue_musl` (§15) and `upgrade` share one `fetch_asset(ver, target)` path — download, checksum gate, extract — with caller-specific identity verification on top.

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
    meta.json               # {created_at, members: [project...], clients: [client...], decision?}
    .seq + .seq.lock        # message counter under flock (§3.2 discipline)
    msgs/
      NNNN.json             # {author, ts, body_md, member: bool}
      .archive/             # overflow past the §17.4 cap
    readers/
      <label>.json          # {last_read_n} — high-water mark per identity (§17.5)
```

- A room is a LINEAR stream, not threads: one `msgs/` sequence, `NNNN` minted under `.seq.lock` with the §3.2 rules (flock, bounded wait, missing counter rebuilt from high-water, corrupt counter an error with remediation), message dir created temp-then-rename. The topic/reply two-level structure was rejected for rigidity — a coordination stream does not know its topics in advance, and a single ordered log is what a group chat is.
- `meta.json.members` names projects, not client rows: row names are private dev-side bookkeeping — two registries can name the same box `riboworld-7032` and `pytorch-7032` — while the project name is the shared vocabulary a counterpart actually needs to read. `clients` is annotation context (which boxes this room coordinates), never a gate.
- Message bodies are markdown (`body_md`) hard-capped at 16 KiB — oversize rejected before anything is written; a coordination message has no business carrying logs.

### 17.2 Command surface

```text
shunt room create <name> --members p1,p2 [--clients c1,c2]
shunt room ls [--as L] [--all]            # rooms + unread counts + suggested groups (§17.4)
shunt room say <room> (--body T | --body-file F | -) [--as L]
shunt room read <room> [--as L] [--all]   # unread messages (default) or full stream; marks read
shunt room decide <room> --note T [--as L]  # record the current agreed state in meta.decision
shunt room members <room>                 # roster
shunt room rm <room>
```

All server-local — no `-c` forwarding exists because there is nothing to forward to. Body source precedence `--body` > `--body-file` > `-` (stdin), the §14.1 rule. `<room>` resolves by exact name or unique prefix. `read` without `--all` prints only messages past the reader's mark (§17.5); `say`, `read`, and `decide` all mark. `decide` writes `meta.decision = {ts, author, note}` — latest wins, replacing the topic-close mechanism: what coordination needs is "the current agreement", not a closed thread; the stream retains how it was reached.

### 17.3 Identity: one `--as` label for author and reader

`--as <label>` (sanitized like a §3.3 slug) is both the `author` on messages and the key of the caller's `readers/` cursor — you read under the same name you speak under. Agents SHOULD use their project name (`riboworld`, `gerna`). Resolution: explicit `--as` > `$USER`. A `say`/`decide` whose `--as` is not in `meta.members` still posts — open-trust model — but the message is recorded `member: false` and stderr names the roster, so an agent that typed the wrong room learns where it went instead of leaking into a foreign channel silently.

### 17.4 Membership, discovery, lifecycle

- `room create --members` is explicit; `room ls` additionally prints _suggested_ rooms: groups of projects whose bindings land on the same box (client rows sharing an `ssh.host:port`, plus `local` rows grouped together) with ≥2 distinct projects and no room yet. Suggestions are computed live from bindings — zero stored state, always accurate — and materialize only via `create`.
- Membership tracks reality loosely on purpose: `project bind`/`unbind` does NOT rewrite room rosters (a room may span boxes, may predate the binding, may outlive it); `room ls` flags members whose projects no longer share any suggested group as `stale` so drift is loud, not hidden.
- A room's stream is capped at 500 live messages; past that, oldest `NNNN.json` move to `msgs/.archive/` — the §3.9 track-cap discipline, dot-prefixed and invisible to `read`/`ls`.
- Rooms themselves never auto-expire — the registry is cheap disk and the coordination record is the asset; cleanup is `room rm`. `readers/` is capped at 64 label files, oldest-mtime evicted — a label gone quiet loses only its read positions, never data.

### 17.5 Read state: per-label high-water marks

`readers/<label>.json` holds `{last_read_n}` — a single high-water counter, made trivial by the linear stream: `unread_msgs = count(NNNN > last_read_n)`, finer than the per-topic binary flag of the earlier board design and computable in one pass. Only real engagement marks: `say` and `decide` acknowledge the current live maximum after the op (a `say` marks its own message read — you obviously saw what you posted). `read`, including `--all`, acknowledges only through the highest-numbered successfully loaded message it returns; a concurrent arrival outside that loaded set remains unread. Archived messages are excluded. Unreadable/corrupt messages are skipped with a warning: a skipped tail does not advance the cursor, but a skipped hole below a returned message is crossed by the single high-water mark. An empty result does not advance the cursor. `room ls` and `status` report without marking — polling is awareness, not reading.

`shunt status --json [--as L]` gains a top-level `rooms: [{name, members, unread_msgs, decision}]` for the `--as` label (default `$USER`) — the aggregate view an agent polls anyway now carries its coordination inbox. Reader updates and reader-cap eviction serialize under the room's `.seq.lock`; files are written temp-then-rename and cursors only advance, so an older concurrent caller cannot regress a shared label's read position. Message loading stays outside that lock. A label shared by two callers shares one read set (intended: one identity, one read position); eviction still resets a quiet label as described in §17.4.
