# Reading state — tracks, env, status JSON, daemon.log

Field-level reference for interpreting `status`, `logs`, and the qroot layout.

## Queue tracks

The client queue is a directory of tracks under `qroot`:

| track      | contents                                                               |
| ---------- | ---------------------------------------------------------------------- |
| `pending/` | `NNNN_<slug>/` entry dirs, claimed in `LC_ALL=C` sort                  |
| `running/` | `NNNN_<slug>/` with `task.sh` + `log` + `pid` (leader pid = task pgid) |
| `done/`    | `NNNN_<slug>/` with `task.sh` + `log` + `exit` (contains `0`)          |
| `failed/`  | `NNNN_<slug>/` with `task.sh` + `log` + `exit` (nonzero rc, or killed) |

When a client has named queues, `status -c <client>` (no `--queue`) prints a per-queue block instead of a single wire report; `status --all --json` carries a `queues` map beside the single-queue `report` field. Queue-level runner env lives in the `env` dotenv file under `qroot` (`WORKDIR`, `PATH_PREPEND`, `TASK_THREADS`, plus passthrough keys); per-task `# shunt-env:` directives override it. Values are literal `KEY=value` — no quote stripping or `$` expansion, so write `K=a b`, never `K="a b"` (the quotes would become part of the value).

## status JSON

`status --json` returns `{protocol:1, daemon:{alive,pid}, workers:{value,source}, running[], pending[], done_recent[], failed_recent[], stop, shutdown:{armed,grace_s}}`. On the server side, `status --all --json` merges per-client rows and adds `facts` and `billing` from the registry.

`facts` is probe-collected advisory data: `gpu`, `cuda`, `driver`, `cores`, `mem_gb`, `disk_free_gb`, `python`, and `can_root` (the box is root or has passwordless sudo — i.e. whether it can install packages or power off). Cores and memory are cgroup-aware; `nproc` on AutoDL reports the host, not your quota.

## daemon.log events

`daemon.log` is JSONL, one `{ts, event, ...}` per line:

| event                                                           | meaning                                                                                                             |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `daemon_start`                                                  | daemon up; orphan requeue ran first                                                                                 |
| `start`                                                         | task claimed pending → running, pgid + slots recorded                                                               |
| `finish`                                                        | task finalized (`{task, rc, dest}` → `done`/`failed`); a `timeout` field marks budget death                         |
| `task_timeout`/`..._kill`                                       | a task exceeded its `# shunt:timeout_s=` budget: TERM at the deadline, KILL after a 10 s grace                      |
| `orphan_live_pgid`                                              | WARNING: a `running/` entry had a live pgid at startup; requeued anyway and its guard must absorb the duplicate run |
| `idle_shutdown_armed`                                           | countdown started; `idle_shutdown_cancelled`/`_blocked`/`_exec`/`_exec_failed`/`_aborted` trace how it ended        |
| `signal` / `stop_drained`                                       | TERM/INT/HUP received (tracked pgids get TERM→KILL) / STOP drain finished, daemon exiting                           |
| `claim_failed`/`finalize_failed`/`requeue_failed`/`ghost_entry` | entry move/quarantine failures — anything persistent here means a permissions or disk problem on the box            |

## Interpretation cheats

`daemon.alive=false` with nonempty `pending/` means nothing will run until the dispatcher auto-heals or someone runs `shunt start` on the box. `stop=true` means draining. A task absent from all lists but present in `spool ls` has not been delivered yet.
