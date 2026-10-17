---
name: shunt
description: Operate shunt, the task-scheduling layer between an agent and hourly-billed cloud GPU boxes. Register clients, bind projects, submit guarded tasks, read queue state and daemon events, arm idle poweroff, and reason about billing. Use whenever the user wants to run experiments on a rented GPU box, manage the queue, or shut a box down when work drains.
---

# shunt

shunt is a FIFO + priority-band + at-least-once experiment queue for hourly-billed GPU boxes, driven over SSH. One Rust binary runs on both sides: the dev machine is the marshalling yard (registry, spool, dispatcher), each GPU box is a siding with its own four-track filesystem queue (`pending/ running/ done/ failed/` under `qroot`).

Mental model:

- The wire is `ssh <box> 'shunt <cmd> --json --root <qroot>'`; the registry's `qroot` is authoritative. No ports, no server-side daemon besides the dispatcher, no client process besides `shuntd`.
- `localhost` is a peer client using the local transport (execs the `shunt` binary directly — no ssh, no key). Its row `clients/localhost.toml` is written on first use (default queue `~/.config/shunt/queue/`); edit it like any client row — `local_bin`/`timeout_s` tune the loopback transport. `shunt client rm localhost` resets the row to factory defaults (the name is reserved); to actually opt out, write `disabled = true` into the row. Use it for dry runs, integration smoke, and dev-loop tasks on the dev machine itself. `shunt client add <name> --local` creates additional loopback queues.
- Execution is at-least-once. Tasks must guard on the artifact they produce so reruns are no-ops.
- The box powers itself off when the queue drains and `shutdown_on_idle` is armed. Powering on is a human click in the vendor console; shunt never powers a box on.
- Submitting while the box is off is normal: tasks wait in the server-side spool until the dispatcher can deliver.

## Setup

All commands below run on the dev machine unless marked client.

```bash
# 1. Register a box. Vendor template seeds defaults; zero network traffic.
shunt client add rw-a --template autodl
#    then edit ~/.config/shunt/clients/rw-a.toml:
#      ssh.host, ssh.port, ssh.key  (and optionally billing.hourly/currency)

# 2. Bootstrap: probe over raw SSH, scp the binary, remote init + start.
shunt client bootstrap rw-a

# 3. Register a project and bind it to the box.
shunt project add riboworld
shunt project bind riboworld --client rw-a \
    --workdir /root/autodl-tmp/RiboWorld --env TASK_THREADS=4
```

Registry files are human-editable TOML under `~/.config/shunt/` (`$SHUNT_HOME` overrides, used by tests). A project binds to many clients; each binding supplies `workdir` and an env overlay. Useful overlay keys: `PATH_PREPEND`, `TASK_THREADS` (`auto` = cgroup probe).

Re-collect facts any time with `shunt client probe <name>`.

## Everyday ops

### submit

```bash
shunt submit -p riboworld -c rw-a --guard runs/exp1.json -- \
    uv run python train.py --config exp1.yaml

shunt submit -c rw-a --file ./mytask.sh     # pre-rendered script
shunt submit -c rw-a --slug exp2 -- python eval.py
```

`submit` writes a spool entry (`spool/<id>.json` + `<id>.sh`) and returns. Delivery happens when the dispatcher next reaches the box. The binding's `workdir`/`env` travel inside the task body itself, as comment-head directives the runner parses at spawn:

```bash
# shunt:workdir=/abs/path     # runner cwd for this task
# shunt-env:KEY=value         # env overlay; beats the queue env file
```

A `--file` script can carry its own directives the same way.

The guard contract is REQUIRED for safe at-least-once execution. The generated task body begins:

```bash
if [ -e runs/exp1.json ]; then echo "guard hit: runs/exp1.json exists; skipping"; exit 0; fi
exec uv run python train.py --config exp1.yaml
```

Guard on the artifact the task produces, relative to the binding's `workdir` (tasks run with `cwd = workdir`, no `cd` needed). If the artifact is only written on success, a rerun after a mid-flight kill recomputes. If the artifact lands early, a rerun exits 0 fast. Never submit an unguarded task that is not naturally idempotent (pure appends, side-effect-free reruns are the exceptions).

### status and logs

```bash
shunt status --all            # every registered box, human table
shunt status --all --json     # same, machine-readable + billing fields
shunt status -c rw-a --json   # one box
shunt logs -c rw-a            # daemon log
shunt logs -c rw-a exp1       # task log (pattern match)
shunt logs -c rw-a -f exp1    # follow (streamed, not JSON)
```

### queue control

```bash
shunt cancel -c rw-a exp3        # remove a pending task (unique substring)
shunt prio -c rw-a exp4 10       # renumber into priority band 0000-0099
shunt kill -c rw-a exp1          # TERM the running task's pgid, then KILL
shunt workers -c rw-a            # resolved pool size + source
shunt workers -c rw-a 4          # resize; takes effect next poll
shunt shutdown -c rw-a on 120    # arm idle poweroff, 120 s grace
shunt shutdown -c rw-a status
shunt shutdown -c rw-a off
shunt spool ls                   # staged-but-undelivered submissions
shunt dispatch --once            # one spool sweep (tests, manual push)
```

`cancel`, `prio`, `kill`, `workers`, `shutdown`, `logs`, `status` all take `-c <client>` on the server side and forward over SSH. Without `-c` they operate on the local queue (client side).

Notes:

- `prio` refuses if the target `NNNN_slug` name already exists. Numbers 0000-0099 are the manual band and always claim ahead of normal sequence numbers (first normal task is 0100).
- Raising `workers` dispatches immediately; lowering only stops new dispatches. Running tasks drain naturally. To free a slot now, `kill`.
- `cancel` matches pending (or running) by unique substring; 0 or >1 match is an error.

## Reading state

The client queue is a directory of tracks under `qroot`:

| track      | contents                                              |
| ---------- | ----------------------------------------------------- |
| `pending/` | `NNNN_<slug>.sh`, claimed in `LC_ALL=C` filename sort |
| `running/` | task + `.log` + `.pid` (leader pid = task pgid)       |
| `done/`    | task + `.log` + `.exit` (contains `0`)                |
| `failed/`  | task + `.log` + `.exit` (nonzero rc, or killed)       |

Queue-level runner env lives in the `env` dotenv file under `qroot` (`WORKDIR`, `PATH_PREPEND`, `TASK_THREADS`, plus passthrough keys); per-task `# shunt-env:` directives override it.

`status --json` returns `{protocol:1, daemon:{alive,pid}, workers:{value,source}, running[], pending[], done_recent[], failed_recent[], stop, shutdown:{armed,grace_s}}`. On the server side, `status --all --json` merges per-client rows and adds `facts` and `billing` from the registry.

`facts` is probe-collected advisory data: `gpu`, `cuda`, `driver`, `cores`, `mem_gb`, `disk_free_gb`, `python`, and `can_root` (the box is root or has passwordless sudo — i.e. whether it can install packages or power off). Cores and memory are cgroup-aware; `nproc` on AutoDL reports the host, not your quota.

`daemon.log` is JSONL, one `{ts, event, ...}` per line:

| event                         | meaning                                                                                                                 |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `daemon start`                | daemon up; orphan requeue ran first                                                                                     |
| `dispatch <task>`             | claimed pending → running, pgid recorded                                                                                |
| `finish <task> rc=N -> track` | finalized to `done/` (rc 0) or `failed/`                                                                                |
| orphan-requeue WARNING        | a `running/` entry had a live pgid at startup; the task was requeued anyway and its guard must absorb the duplicate run |
| `IDLE-SHUTDOWN ARMED`         | countdown started; cancels on new pending, flag rm, or STOP                                                             |
| `daemon stop` / signal events | TERM/INT/HUP received; tracked pgids get TERM→KILL                                                                      |

Interpretation cheats: `daemon.alive=false` with nonempty `pending/` means nothing will run until the dispatcher auto-heals or someone runs `shunt start` on the box. `stop=true` means draining. A task absent from all lists but present in `spool ls` has not been delivered yet.

## Cost-aware playbook

1. Arm idle-shutdown right after enqueueing the last task of a batch: `shunt shutdown -c rw-a on`. The countdown only fires when both `pending/` and `running/` are empty, re-checks every second, and cancels if new work arrives, so arming early is safe.
2. Submitting while the box is off costs nothing. The spool holds the task until a human powers the box on and the dispatcher delivers it. Batch overnight submissions; let one power-on drain them.
3. Read `billing.hourly`/`billing.currency` from `status --all --json` before queueing. On hourly billing, unused paid time is already spent: if the instance has 40 min of paid time left and the next task needs 3 h, hold the long task, run short ones, or warn the user. That decision is yours; shunt only exposes the rate.
4. Split multi-hour experiments into checkpointable tasks (`--guard runs/epoch10.pt` style) so an at-least-once rerun after a mid-flight death costs minutes, not the whole run. One giant unguarded 12 h task is the expensive failure mode.
5. When a box is running but `pending/` is empty and you have nothing to add, arm shutdown. Paid idle time is the thing shunt exists to kill.

## Failure semantics

- `failed/` means rc ≠ 0 or killed. The `.exit` file has the rc; the `.log` ends with `=== exit <rc> <iso>`. shunt has no retry backoff: read the log, fix, resubmit.
- A task that fails rc 97 never ran: the binding's `workdir` was unreachable.
- Orphan requeue: if the daemon dies with tasks in `running/`, the next start moves each to `pending/0000_requeued_<name>` (front of queue) and the interrupted log to `failed/<name>.interrupted.<ts>.log`. If the task's process group is still alive, the daemon logs a WARNING and requeues anyway. Your guard is what makes that safe: the rerun must no-op or cheaply resume while the zombie finishes.
- A killed task (via `shunt kill` or daemon TERM forwarding) lands in `failed/` with its recorded rc.

## Safety rules

- Never signal anything by name or pgrep. Only the pgids recorded in `running/*.pid` belong to the queue; `shunt kill` is the only way to signal a task. Other jobs on the box are not yours.
- Never let a test reach real `/usr/bin/shutdown`. Tests redirect the command via the `shutdown_cmd` file or `$SHUTDOWN_CMD` env, and never arm `shutdown on` against a box you are not willing to power off.
- Every daemon liveness check is pidfile + `/proc/<pid>/cmdline` match.
- Use placeholders (`root@<host>:<port>`) in docs, examples, and task scripts. Never write real hostnames, ports, or key paths into tracked files.

## Worked example

```bash
# once: register + bootstrap
shunt client add rw-a --template autodl
$EDITOR ~/.config/shunt/clients/rw-a.toml   # ssh.host/port/key
shunt client bootstrap rw-a                  # probes facts, ships binary

shunt project add riboworld
shunt project bind riboworld --client rw-a \
    --workdir /root/autodl-tmp/RiboWorld --env TASK_THREADS=4

# submit a training run guarded on its metrics file
shunt submit -p riboworld -c rw-a \
    --guard runs/exp1/metrics.json -- \
    uv run python train.py --out runs/exp1

shunt dispatch --once          # or let the dispatcher loop push it
shunt status -c rw-a --json    # pending → running
shunt logs -c rw-a exp1        # watch the task log

# drain and walk away
shunt shutdown -c rw-a on      # fires ~60 s after running+pending empty
# box powers off; billing stops at the end of the paid window
```

If `shunt logs -c rw-a exp1` shows nothing, check `shunt spool ls`: the task may still be staged because the box is off or SSH failed.
