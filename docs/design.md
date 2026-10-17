# shunt design spec

shunt is the task-scheduling interface layer between an AI agent and hourly-billed cloud GPU instances (AutoDL-style: no systemd, containers that may vanish, billing tied to wall-clock uptime). It exists so that an agent — or a human — can submit an experiment, let a rented box run it, and have the box power itself off when the work is done, without anyone watching the queue.

The name is the rail-yard metaphor: the server is the marshalling yard, each client box is a destination siding, and `pending/`, `running/`, `done/`, `failed/` are the four tracks. Moving a task between tracks is a filesystem `mv`.

This document is the behavioral spec for the Rust rewrite. The proven bash implementation is archived under `reference/bash-queue/` and is the oracle for every semantic in §3. When behavior is ambiguous, the bash source wins.

## 1. Positioning and non-goals

shunt is a FIFO + priority-band + at-least-once experiment queue for rented GPU boxes, operated headlessly by agents over SSH. It is not a CI system, not Slurm, not a task spooler clone.

FIFO is the _claim_ order, not the execution shape: the client runs a concurrent worker pool (`workers`, live-tunable), so several tasks run in parallel and the daemon's job is to keep the box saturated, not serialized.

Non-goals, by deliberate exclusion:

- No DAG dependencies between tasks. Use Airflow.
- No retry backoff or retry budget. Failed tasks land in `failed/`; the agent decides what to do next.
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

| Entry                | Meaning                                                                        |
| -------------------- | ------------------------------------------------------------------------------ |
| `pending/`           | `NNNN_<slug>/` entry dirs holding `task.sh`; claimed in name sort order        |
| `running/`           | in-flight `NNNN_<slug>/` with `task.sh`, `log`, `pid`                          |
| `done/`              | `NNNN_<slug>/` with `task.sh` + `log` + `exit` for rc 0                        |
| `failed/`            | `NNNN_<slug>/` with `task.sh` + `log` + `exit` for nonzero rc or killed tasks  |
| `workers`            | pool size file, re-read every poll cycle                                       |
| `env`                | runner env dotenv: `WORKDIR`, `PATH_PREPEND`, `TASK_THREADS`, passthrough keys |
| `STOP`               | sentinel file: drain in-flight tasks, then daemon exits                        |
| `shutdown_on_idle`   | flag file: power off once queue fully drains                                   |
| `shutdown_grace_s`   | countdown seconds (default 60)                                                 |
| `shutdown_cmd`       | command exec'd at countdown end                                                |
| `daemon.pid`         | daemon pid, written at startup                                                 |
| `daemon.log`         | structured JSONL event stream (see §3.9)                                       |
| `.seq` + `.seq.lock` | enqueue counter under flock                                                    |

Every queue entry is a directory named `NNNN_<slug>` containing `task.sh` plus the sidecars it accumulates (`log`, `pid`, `exit`). Readers also understand the legacy flat layout (`NNNN_<slug>.sh` with `.sh.log`/`.sh.pid`/`.sh.exit` sidecars) so pre-existing queues need no migration; writers always create dirs, and `finalize` normalizes any legacy entry it touches. A suffixless flat file is NOT an entry — it is ignored entirely.

The daemon only manages tasks it dequeued itself, tracked via `running/*/pid`. Other jobs on the box are never touched. This boundary is a hard safety rule: every signal the daemon or `shunt kill` sends goes to a pgid recorded in a `pid` file, never to a name match.

### 3.2 Sequence numbers and the priority band

- `pending/` entry names are `NNNN_<slug>`, `NNNN` zero-padded to 4.
- `.seq` starts at 99, so the first enqueued task is `0100`. Values `0000`–`0099` are the manual priority band: `shunt prio` renumbers into it, and requeued orphans land at `0000_requeued_*`.
- `.seq` is read/incremented/written under an flock on `.seq.lock` (30 s wait, matching `flock -w 30`). Concurrent enqueues must never produce duplicate numbers.
- Claim order is `LC_ALL=C` filename sort, so `0000_*` always runs ahead of `0100_*`.

### 3.3 Enqueue

`shunt enqueue` (or the stdin form used over the wire):

1. Slug is sanitized to `[A-Za-z0-9_.-]` (anything else → `-`); an empty result is an error.
2. Take the `.seq` lock, increment, write back, release.
3. Install the task file into `pending/` with mode 0755 via write-temp-then-`mv` inside `pending/` (atomic on the same filesystem). A remote `scp` + `mv` is _not_ atomic across filesystems; never install directly to the final name from an off-queue temp.

### 3.4 Daemon liveness

- Liveness is pidfile + `/proc/<pid>/cmdline` content match: read `daemon.pid`, `kill -0`, then check cmdline mentions the daemon identity. **Never use pgrep/pkill -f** — a remote shell whose command line happens to mention the pattern self-matches and gives false positives. The cmdline match string is a single named constant in the codebase (the bash version duplicated it in three scripts; that was a defect).
- Stale pidfile → treated as dead, removed, daemon may start.

### 3.5 Daemon loop

Poll cycle is 2 s (`POLL` env overridable for tests). Each cycle, in order: `reap` → if `STOP` present, drain; else `dispatch`, then check idle-shutdown.

On startup:

1. Create the four track dirs.
2. Write `daemon.pid`.
3. Log `daemon start`.
4. Run `requeue_orphans` (§3.6).

On TERM/INT/HUP: log, remove `daemon.pid`, send TERM to every tracked task pgid, wait 2 s, send KILL to any survivors, exit 143.

### 3.6 Orphan requeue (at-least-once core)

Any `running/` entry present at daemon start means a previous daemon died mid-flight. For each:

1. If its `pid` file exists and `kill -0 -- -<pgid>` succeeds, the task process group is _still alive_ — log a WARNING and requeue anyway. The task's own guard makes the rerun a no-op. (This happens when the daemon dies but a detached task survives.)
2. Strip any stacked `0000_requeued_` prefixes from the basename in a loop — a task requeued twice must not accumulate prefixes.
3. `mv` the task to `pending/0000_requeued_<clean>` (front of queue).
4. Its `log` moves to `failed/<clean>.interrupted.<unixts>.log`.
5. Remove the `pid` file.

Then sweep legacy strays: any remaining `running/*.pid` files are removed; any remaining `running/*.log` moves to `failed/<name>.interrupted.<unixts>.log`. Dir entries carry their sidecars inside, so they cannot stray.

### 3.7 Dispatch

- Read desired workers (see §3.8). While `len(running) < workers`:
  1. Pick the first `pending/` entry by `LC_ALL=C` name sort.
  2. Claim by `mv pending/<f> running/<f>`. If the `mv` fails (a concurrent `shunt cancel` won the race), skip to next iteration — the `mv` result is the recheck, not a prior existence test.
  3. `chmod +x`, write log header (`=== task`, `=== start <iso>`).
  4. Spawn via `process_group(0)` (setsid) so the task's pgid equals its leader pid; stdout/stderr append to `running/<name>/log`.
  5. Write the leader pid to `running/<name>/pid`, record pgid → entry name.
- Each task runs in its own process group precisely so `shunt kill` and the daemon's signal forwarding can address the whole task tree by pgid without touching the daemon or sibling tasks.

### 3.8 Worker pool resolution

Re-read every cycle. Order: `workers` file > `QUEUE_WORKERS` env (set at daemon start) > built-in default 3.

- Digits are extracted (`tr -cd '0-9'` semantics); non-numeric or empty falls back to the last valid value, else default.
- Clamped to ≥ 1.
- Raising takes effect on the next dispatch. Lowering only stops new dispatches — running tasks finish naturally ("drain"), so shrinking never kills work. To free a slot now, use `shunt kill`.
- Status output must report the _resolved_ value and its source; never hardcode the default (the bash `qstatus.sh` printed `3 (default)` as a literal — a defect).

### 3.9 Reap and finalize

- Reap: for each tracked pid, if `kill -0` fails the child is gone; collect its exit status and finalize.
- Finalize entry `<name>` with rc:
  - append `=== exit <rc> <iso>` to the log;
  - remove the entry's `pid` file;
  - `mv` the whole entry dir to `done/` if rc 0, else `failed/` (legacy flat entries are normalized into the dir layout here);
  - write `<rc>` to `<name>/exit`;
  - log `finish <base> rc=<rc> -> done|failed`.

### 3.10 STOP sentinel

If `STOP` exists: no new dispatches. When the running set empties, the daemon removes `STOP` and `daemon.pid`, logs, and exits 0. A task in flight when STOP appears finishes normally and lands in `done/` or `failed/` first. Immediate stop = `shunt stop` + `shunt kill`: the killed task lands in `failed/`, the daemon exits at the next boundary.

### 3.11 Idle shutdown

When `shutdown_on_idle` exists AND `running/` is empty AND `pending/` is empty:

1. Arm: log `IDLE-SHUTDOWN ARMED` with the grace length.
2. Count down `shutdown_grace_s` (file > default 60) one second at a time. Each second, re-check: flag removed → cancel; a new `pending/` entry → cancel; `STOP` sentinel → cancel. Log every 10 s and for the final 5 s.
3. After the countdown, re-check once more: flag still present, pending still empty, running set empty. Only then exec the shutdown command.
4. Resolve the command at the last moment: `shutdown_cmd` file > `$SHUTDOWN_CMD` env > `/usr/bin/shutdown`. Last-moment resolution is what lets a test redirect the command to a marker — a test must never reach the real `/usr/bin/shutdown`. The `shutdown_cmd` file is written by `shunt shutdown on --cmd`, and the server-side arm (`shutdown -c <name> on`) pushes the client row's `shutdown_cmd` field every time, so the registry stays the source of truth — copy a row to a test env and its placeholder command comes along. `shutdown status` reports the resolved command before anything fires.

Residual race, documented in the bash source and retained: an enqueue in flight during the final instant (or an scp mid-copy) can still lose to poweroff. The last re-check narrows but cannot close it.

### 3.12 Task environment (exec wrapper)

Every task runs under a fixed prologue (the bash `qexec.sh`). The queue-level `env` file under `$QROOT` supplies defaults; per-task `# shunt:` directives in the task's comment head override them for that task only:

- `cwd` = task's `# shunt:workdir=` directive, else `env`'s `WORKDIR`, else `$QROOT` itself. If the workdir is unreachable the task fails rc 97 without running.
- `PATH` prepended with `env`'s `PATH_PREPEND`.
- All proxy variables unset: `ALL_PROXY all_proxy http_proxy https_proxy HTTP_PROXY HTTPS_PROXY`. A task that needs a tunnel re-exports it inside itself.
- `OMP_NUM_THREADS` / `MKL_NUM_THREADS` set to `env`'s `TASK_THREADS` (`auto` → cgroup probe, see §8). Rationale: AutoDL containers are cgroup-capped while `nproc`/`free` report host values — on RW-A, 14 real cores vs 112 reported. An uncapped torch process claims all of them and N workers thrash.
- Remaining `env` keys pass through as environment; per-task `# shunt-env:K=V` directives apply last and win.
- `<qroot>/env_setup` is a client-row `env_setup` snippet pushed by `client env`/`bootstrap`: sourced in the wrapper right before exec, so conda-activate/PATH-export land in the task env, and `--file` tasks get it too (the wrapper, not the render, owns it). The wrapper's shell is `<qroot>/shell` or bash — record `shell = "zsh"` when the snippet isn't POSIX (conda activate under bare `sh` fails silently). A nonzero snippet return doesn't block; `exit`/`exec` inside it kills the launch — same footgun as bashrc.
- Then `exec <shell> <taskfile>`.

A task may override any of these inside itself. `set -uo pipefail` is part of the generated task template, not enforced by the runner.

### 3.13 Task file format and the guard contract

Generated task (single generator on the client; `shunt new`/`shunt mktask` share it):

```bash
#!/usr/bin/env bash
# task: <slug> (generated by shunt <ts>)
set -uo pipefail
if [ -e <guard> ]; then echo "guard hit: <guard> exists; skipping"; exit 0; fi
exec <cmd...>            # args shell-quoted
```

A submitted task carries its binding inside the body, as comment-head directives the runner parses at spawn (the wire carries only the body, so metadata cannot travel beside it):

```bash
# shunt:workdir=/abs/path        # runner cwd for this task only
# shunt-env:KEY=value            # env overlay; beats the queue env file
```

Both are optional and repeatable (`env`); scanning stops at the first non-comment line, so hand-written tasks can set them too. No `cd` line — the runner already places cwd at `workdir` (the bash version's generated `cd` was redundant). The guard convention is the contract that makes at-least-once safe: tasks must be idempotent or guard on the artifact they produce.

### 3.14 Known defects in the bash source — fix in the port

| Defect                                                                                             | Required behavior                                                      |
| -------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------- |
| `qprio.sh` uses `mv -f`: renumbering onto an existing `NNNN_slug` silently destroys a pending task | `shunt prio` must fail if the target name exists                       |
| `rwq_enqueue.sh` uses predictable remote tmp `/tmp/rwq_enqueue_$$.sh`                              | remote temp files come from `mktemp`                                   |
| `daemon_alive` duplicated in three scripts with the grep string inline                             | liveness check + identity string are single-sourced                    |
| `qstatus.sh` prints `3 (default)` as a literal                                                     | report the resolved workers value and source                           |
| task-wrap logic duplicated client-side (`rwq_enqueue`) and remote (`qnew`)                         | one generator: `shunt mktask` on the client; submit paths forward args |

## 4. Server: registry, spool, dispatcher

### 4.1 Registry layout

`~/.config/shunt/` (XDG; `$SHUNT_HOME` overrides for tests):

```text
server.toml            # global defaults (poll interval, default client)
clients/<name>.toml
projects/<name>.toml
keys/<name>[.pub]      # dedicated ed25519 keypair per ssh client
known_hosts            # shunt-scoped TOFU store (never ~/.ssh)
state/<name>.fail      # consecutive ssh connect failures (endpoint-drift watchdog)
spool/                 # server-side staging queue
```

Registry files are human-editable TOML and safe to keep in git (minus key paths). "Registration" is server-local bookkeeping: `shunt client add` writes the row with zero network traffic. A client never needs to reach the server.

**SSH is fully managed.** `client add`/`bootstrap` generate a dedicated ed25519 keypair at `keys/<name>` and fill `ssh.key` — registration alone makes the row usable. Deploying the public key is the one step that needs existing trust: when `ssh.password` is set, `bootstrap` runs `sshpass ssh-copy-id` unattended; without it, bootstrap prints the `.pub` to install once via the vendor console or a manual `ssh-copy-id`. Host-key checking is TOFU (`StrictHostKeyChecking=accept-new`) against `keys/`-sibling `known_hosts`, so first contact never prompts and the user's own `~/.ssh` stays untouched.

**`localhost` is a pre-registered peer client, not a special case.** The dev machine is a real execution target (light tasks, tests, CPU-only work): it binds in projects, takes submissions, runs the daemon, and answers every wire op exactly like a remote box — the only difference is the transport. `local = true` in a client row selects the loopback transport (exec the local `shunt` binary instead of ssh); `localhost` is simply that transport's conventional reserved name, and `client add <name> --local` can mint more local queues. The first access materializes `clients/localhost.toml` with the defaults (`qroot=$SHUNT_HOME/queue`, `deploy_root=$HOME`), so a fresh install still runs the whole submit → dispatch → execute → finalize path with zero setup — and afterwards the row sits on disk, editable like any other: `local_bin`, `timeout_s` and friends override the loopback transport. The write is best-effort: an unwritable HOME still yields working defaults. `client rm localhost` resets to factory — the row regenerates on next use; a user who truly wants it gone writes `disabled = true` into the row (that file is never auto-rewritten, so the opt-out sticks).

`clients/<name>.toml`:

| field                                                         | meaning                                                                                                                                                      |
| ------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `name`                                                        | registry key                                                                                                                                                 |
| `local`                                                       | loopback transport instead of ssh (localhost and any `client add --local` row)                                                                               |
| `local_bin`                                                   | local transport: binary to exec (default: the running `shunt`)                                                                                               |
| `timeout_s`                                                   | local transport: exec timeout; on expiry the child is killed                                                                                                 |
| `ssh_timeout_s`                                               | ssh transport: ConnectTimeout override (default 15)                                                                                                          |
| `shutdown_cmd`                                                | poweroff command pushed to `<qroot>/shutdown_cmd` at `shutdown on` arm time — point at `true` or a marker script in test envs                                |
| `disabled`                                                    | opt-out: row stays on disk but is not a usable client (skipped by listings, rejected by load)                                                                |
| `ssh.host`, `ssh.port`, `ssh.user`, `ssh.key`, `ssh.password` | exec endpoint; `password` feeds the one-time `sshpass ssh-copy-id` key deploy                                                                                |
| `qroot`                                                       | queue dir on the box                                                                                                                                         |
| `deploy_root`                                                 | vendor work root (AutoDL: `/root/autodl-tmp`)                                                                                                                |
| `facts`                                                       | table filled by `probe`/`bootstrap`: `gpu`, `cuda`, `driver`, `cores`, `mem_gb`, `disk_free_gb`, `python`, `can_root`, `shutdown_cmd` (poweroff candidate)   |
| `expire`                                                      | billing lifetime: `unlimited` (owned box, localhost), `metered` (pay-as-you-go until balance), or an ISO-8601 deadline; empty = unknown                      |
| `env_setup`, `shell`                                          | task preamble: snippet sourced before every task + the shell it runs under (default bash); pushed to `<qroot>/env_setup`/`shell` by `client env`/`bootstrap` |
| `endpoint_rotates`                                            | vendor reassigns IP/port across restarts (AutoDL does not); steers the escalation hint after `CONN_FAIL_ESCALATE` consecutive ssh failures                   |
| `billing.hourly`, `billing.currency`                          | cost metadata surfaced to the agent                                                                                                                          |

`projects/<name>.toml`:

```toml
name = "riboworld"
[[bind]]
client = "rw-a"
workdir = "/root/autodl-tmp/RiboWorld"
env = { PATH_PREPEND = "/root/.local/bin", TASK_THREADS = "4" }
[[bind]]
client = "gerna-b"
workdir = "/data/RiboWorld"
```

### 4.2 Vendor templates

A vendor template is a plain client-row TOML used as a seed for `client add` — no special syntax, FILL markers are just comments. Search order, first hit wins: `$SHUNT_VENDOR_DIR`, then `$SHUNT_HOME/vendors/` (user-supplied templates live with the registry they seed), then the repo's `etc/vendors/` (a compile-time path that only resolves for builds from the source tree). `shunt client add --template autodl` seeds `ssh.user=root`, `deploy_root=/root/autodl-tmp`, `qroot=/root/autodl-tmp/queue`, leaving only `ssh.host`, `ssh.port` (and optionally `ssh.password`, `billing`) to fill — `ssh.key` is generated automatically. Templates are defaults for the row file, nothing more — after `add`, the client row stands alone. A bad name errors out listing every template the search dirs actually contain.

### 4.4 Box-side SOCKS5 tunnel

Boxes with poor international connectivity (mainland-China GPU vendors) get their work done through a reverse dynamic forward: `ssh -N -R <port>` opens `127.0.0.1:<port>` as a SOCKS5 listener **on the box**, tunneled back to exit via the dev machine's network. `shunt tunnel up -c <name> [--port 1080]` spawns that ssh detached (pidfile + log under `tunnels/`), then upserts `ALL_PROXY=socks5h://127.0.0.1:<port>` into the box's `<qroot>/env` — tasks pick the proxy up with zero task-level edits, because the runner strips ambient proxy vars and then injects the env file. `tunnel down` kills the ssh and removes the env line; `tunnel status` reports liveness and the port. The `h` in `socks5h` makes DNS resolve remotely too, which is the point — a poisoned resolver stays unused. ssh-side prerequisites: OpenSSH ≥7.6 client and `AllowTcpForwarding` on the box's sshd (on by default).

### 4.5 Spool and dispatcher

`submit` lands a task in `spool/` as `spool/<task-id>.json` (metadata: project, client, slug, guard, argv, submitted_at) + `<task-id>.sh` (rendered task body). Persisted before returning, so submitting while the box is off is normal operation.

Two-phase dispatch, at-least-once end to end:

1. Dispatcher (server-side loop, poll interval from `server.toml`) scans `spool/` oldest-first.
2. For each entry bound to client C, over the row's transport: `shunt enqueue - <slug>-<shortid> --json --root <qroot>` with the body on stdin.
3. Only on a clean ack (exit 0 + JSON `{ok: true, name: ...}`) does the server remove the spool entry. Any failure leaves it for the next poll.
4. Dupes from a retry after a lost ack are absorbed client-side by the task's guard; the server task id is embedded in the slug (`<slug>-<shortid>`) so a re-pushed task is recognizable in logs.

Auto-heal: if the box answers but the daemon liveness check fails, the dispatcher runs `shunt start --root <qroot>` once before pushing (this is the "power on → claims work" link; box-side autostart remains optional sugar). If the transport fails entirely (box off), the entry stays in spool.

## 5. Wire protocol

The remote interface is `shunt <subcmd> --json` on the client. Every response is a single JSON object with `"protocol": 1`. Errors exit nonzero with `{"protocol":1,"ok":false,"error":"..."}`.

Transport is chosen per client row: `ssh` (default) execs over SSH; `local` (`local = true` in a row, including the pre-registered `localhost`) spawns the same argv via the local `shunt` binary — the wire surface is identical and the loopback needs no network, no key. Every wire call appends `--root <qroot>` from the client row, so the registry is the single source of truth for where the queue lives; ops that read state also pass `--json`.

| op                                       | request                        | response sketch                                                                                                                      |
| ---------------------------------------- | ------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------ |
| `enqueue`                                | task body on stdin; `--slug`   | `{ok, name}`                                                                                                                         |
| `new`                                    | `--slug`, `--guard`, `--` argv | `{ok, name}`                                                                                                                         |
| `mktask`                                 | same as `new`                  | task body on stdout                                                                                                                  |
| `status`                                 | —                              | `{daemon:{alive,pid}, workers:{value,source}, running[], pending[], done_recent[], failed_recent[], stop, shutdown:{armed,grace_s}}` |
| `cancel`                                 | pattern                        | `{ok, name}` or error on 0/multi match                                                                                               |
| `prio`                                   | pattern, n                     | `{ok, from, to}`; error if target exists                                                                                             |
| `kill`                                   | pattern                        | `{ok, pgid, escalated}`                                                                                                              |
| `workers get` / `workers set N`          | —                              | `{workers, source}`                                                                                                                  |
| `shutdown on [grace]` / `off` / `status` | —                              | `{armed, grace_s, running, pending}`                                                                                                 |
| `probe`                                  | —                              | `{gpu, driver, cuda, cores, mem_gb, disk_free_gb, python}`                                                                           |
| `logs`                                   | `--task pat` or daemon         | text stream (may be non-JSON for `-f`)                                                                                               |
| `start` / `stop` / `daemon`              | —                              | `{ok, pid}` / `{ok}` / runs the loop (foreground; `start` daemonizes)                                                                |
| `init`                                   | `--qroot`                      | `{ok}` creates dirs                                                                                                                  |

All argv crossing SSH is shell-quoted with `%q`-equivalent escaping on the server side; quoting is centralized in the transport layer, never hand-rolled per call site.

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
2. `shunt client bootstrap rw-a` — probe over raw SSH (no binary yet: the probe is server-side shell), scp the binary, remote `shunt init`
   - `shunt start`, facts merged into the row.
3. `shunt project add riboworld` + `shunt project bind riboworld --client rw-a --workdir /root/autodl-tmp/RiboWorld`.
4. `shunt submit -p riboworld -c rw-a --guard runs/x.json -- uv run python ...` → lands in `spool/`.
5. Dispatcher pushes when the box is reachable; box runs it; on drain, idle-shutdown powers off. Overnight submissions sit in spool until the human powers the box on; the dispatcher's auto-heal restarts `shuntd` and delivers.

## 8. Probe spec

`shunt probe` (and the bootstrap pre-binary shell equivalent) collects, emitting JSON:

- `gpu`: `nvidia-smi --query-gpu=name,driver_version --format=csv,noheader`; absent → `null` (CPU-only boot is a valid state — AutoDL no-card boots drop the GPU).
- `cuda`: `nvidia-smi` header parse.
- `cores`: cgroup-aware — `cpu.max` quota/period (v2) or `cpu.cfs_quota_us`/`cpu.cfs_period_us` (v1); `nproc` only as fallback and marked suspect.
- `mem_gb`: `memory.max` / `memory.limit_in_bytes`; `free` only as fallback, same caveat. Never size off host-reported values.
- `disk_free_gb`: `df` on `deploy_root`.
- `python`: `python3 --version`.
- `shutdown_cmd`: best-guess poweroff command — `systemctl poweroff` when `/run/systemd/system` exists and `systemctl` is found, else `shutdown`, else `poweroff`, else absent. Advisory: a container may carry the binary yet lack CAP_SYS_BOOT; the row's `shutdown_cmd` field and `--cmd` flag still override it at arm time.

Facts are advisory registry data for agent-side matching, not enforced constraints.

## 9. Billing and cost surfacing

`billing.hourly`/`billing.currency` in a client row flow into `shunt status --all --json` output so the agent can reason about cost. `expire` carries the billing lifetime — `unlimited` for owned compute (localhost, lab GPUs), `metered` for pay-as-you-go (no deadline; burns until the balance does), or an ISO-8601 timestamp for subscription boxes — and `status` adds `expires_in_s` when the timestamp carries an offset. shunt never makes the decision — "instance has 40 min of paid time left but the next task needs 3 h → hold and notify" is agent logic taught in the skill file. shunt's job is accurate exposure of queue state, uptime context, and rates.

## 10. CLI surface

| command                                                | side   | purpose                     |
| ------------------------------------------------------ | ------ | --------------------------- |
| `shunt submit -p proj -c client [--guard p] -- cmd...` | server | spool a task                |
| `shunt submit -c client file.sh`                       | server | spool a script file         |
| `shunt status [--all] [--json]`                        | server | aggregate client states     |
| `shunt spool ls\|rm`                                   | server | inspect staging             |
| `shunt client add\|ls\|show\|bootstrap\|probe\|rm`     | server | registry                    |
| `shunt project add\|bind\|ls\|show`                    | server | registry                    |
| `shunt dispatch [--once]`                              | server | run the dispatcher loop     |
| `shunt daemon`                                         | client | run queue loop (foreground) |
| `shunt init`                                           | client | create queue dirs           |
| `shunt start` / `shunt stop`                           | client | daemon lifecycle            |
| `shunt enqueue [file\|-] [slug]`                       | client | install into pending        |
| `shunt new` / `shunt mktask`                           | client | wrap argv as task (guard)   |
| `shunt status`                                         | client | queue summary               |
| `shunt cancel` / `prio` / `kill`                       | client | pending/running ops         |
| `shunt workers [N]`                                    | client | pool size get/set           |
| `shunt shutdown on\|off\|status`                       | client | idle poweroff               |
| `shunt logs [pat]` / `shunt dlog`                      | client | task/daemon logs            |
| `shunt probe`                                          | client | machine facts JSON          |

## 11. Testing strategy

Integration tests spawn a real daemon against a tmp `SHUNT_ROOT`; the poll interval is shortened via env, and `shutdown_cmd` is redirected to a script that touches a marker file. Must cover:

- dispatch order (filename sort; `0000_` band ahead of `0100_`)
- enqueue concurrency vs dispatch (`.seq` under lock; no dup numbers)
- `cancel` 0/1/many match; `prio` renumber incl. target-exists refusal
- `kill`: TERM → 10 s → KILL, rc recorded, lands in `failed/`
- orphan requeue: dead daemon leaves `running/` entries → restart → front-of-queue, prefix strip, interrupted logs swept
- live-pgid orphan: WARNING + requeue anyway
- STOP drain: running task finalizes, then daemon exits
- idle-shutdown: arm → fire (marker), cancel via flag rm / new pending / STOP, grace default, final re-check
- workers resize: raise dispatches immediately; lower drains only; invalid value falls back to last-known
- env file overlay, workdir, proxy unset, thread caps
- dispatcher: spool → push → ack → delete; unreachable client retains; auto-heal restarts a dead daemon

## 12. Skill deliverable

`skills/shunt/SKILL.md` ships in the repo (no `shunt skill` subcommand). Contents: register/bootstrap a client, add/bind a project, submit and guard conventions, query and interpret queue state and JSONL events, cost-aware practices (arm idle-shutdown, watch billing metadata), and splitting long experiments into checkpointable tasks so at-least-once reruns are cheap.

## 13. Repo layout

```text
shunt/
  Cargo.toml
  src/
    main.rs          # clap entry; subcommands dispatch
    server/          # registry, spool, dispatcher, ssh transport, status
    agent/           # daemon loop, FS queue ops, exec, reap, shutdown, probe
    proto/           # task format, wire JSON types (shared)
  etc/vendors/autodl.toml
  docs/design.md
  skills/shunt/SKILL.md
  tests/             # tmp-SHUNT_ROOT integration suite
  reference/         # bash oracle (read-only archive)
  task_template.sh
  README.md  LICENSE(MIT)  install.sh
```
