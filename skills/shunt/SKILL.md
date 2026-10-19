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
- The client name is user-supplied, never agent-invented — ask the user what to call the box before `client add`. The name lands in the row filename, every `-c <name>` invocation, and the task/daemon logs; a made-up name puts the registry on a handle nobody recognizes. Same for project names. If a name turns out wrong, fix it with `shunt client rename <old> <new>` — it moves the row, keypair, mux socket, failure streak, spool refs, and project bindings together. Never hand-mv registry files; the sidecar paths keep pointing at the old name.

## Setup

All commands below run on the dev machine unless marked client.

```bash
# 1. Register a box. Vendor template seeds defaults; a dedicated
#    ed25519 keypair is generated at ~/.config/shunt/keys/rw-a and
#    ssh.key is filled automatically — ssh needs no manual keygen.
shunt client add rw-a --template autodl
#    then edit ~/.config/shunt/clients/rw-a.toml:
#      ssh.host, ssh.port (+ ssh.password for unattended key deploy)
#      expire, env_setup, shell — and optionally billing.hourly/currency

#    `shunt client templates` lists what --template accepts.
# 2. Bootstrap: deploys the pubkey via sshpass when ssh.password is
#    set (prints the .pub to install once otherwise), probes over raw
#    SSH, scp's the binary, remote init + start. Host keys are
#    TOFU-accepted into ~/.config/shunt/known_hosts — no known_hosts
#    maintenance. All execs ride one muxed connection per client
#    (~/.config/shunt/cm/<name>.sock) — vendors whose sshd sits behind
#    a startup-limited proxy flap on back-to-back handshakes
#    ("Exceeded MaxStartups"); mux + in-call retries absorb that.
shunt client bootstrap rw-a

# 3. Register a project and bind it to the box. --repo records the
#    dev-side checkout (agents know where the code lives); --desc a
#    one-liner. [defaults].env in the row merges under every binding.
shunt project add riboworld --repo ~/Code/RiboWorld --desc "RNA-seq pipeline"
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

### tunnel (outbound proxy)

```bash
shunt tunnel -c rw-a up              # box-side SOCKS5 on 127.0.0.1:1080,
                                    # exiting via the dev machine's network
shunt tunnel -c rw-a up --port 2080  # custom port
shunt tunnel -c rw-a status
shunt tunnel -c rw-a down
```

For boxes behind a hostile/poisoned network (mainland China vendors): `tunnel up` opens `ssh -N -R` remote dynamic forwarding, then writes `ALL_PROXY=socks5h://127.0.0.1:<port>` into the box's `<qroot>/env` — every task after that picks the proxy up transparently (git clone, cargo fetch, curl). `tunnel down` removes both. Tasks keep running through it as long as the tunnel ssh stays alive; check `status` if tasks start failing on network.

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
shunt shutdown -c rw-a status    # armed state + the command that would fire
shunt shutdown -c rw-a off
# the row's `shutdown_cmd` is pushed at every arm; `shutdown on --cmd`
# overrides it once. Set `shutdown_cmd = "true"` (or a marker script)
# on test rows so a copied registry can never hit a real poweroff.
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

| track      | contents                                                               |
| ---------- | ---------------------------------------------------------------------- |
| `pending/` | `NNNN_<slug>/` entry dirs, claimed in `LC_ALL=C` sort                  |
| `running/` | `NNNN_<slug>/` with `task.sh` + `log` + `pid` (leader pid = task pgid) |
| `done/`    | `NNNN_<slug>/` with `task.sh` + `log` + `exit` (contains `0`)          |
| `failed/`  | `NNNN_<slug>/` with `task.sh` + `log` + `exit` (nonzero rc, or killed) |

Queue-level runner env lives in the `env` dotenv file under `qroot` (`WORKDIR`, `PATH_PREPEND`, `TASK_THREADS`, plus passthrough keys); per-task `# shunt-env:` directives override it. Values are literal `KEY=value` — no quote stripping or `$` expansion, so write `K=a b`, never `K="a b"` (the quotes would become part of the value).

`status --json` returns `{protocol:1, daemon:{alive,pid}, workers:{value,source}, running[], pending[], done_recent[], failed_recent[], stop, shutdown:{armed,grace_s}}`. On the server side, `status --all --json` merges per-client rows and adds `facts` and `billing` from the registry.

`facts` is probe-collected advisory data: `gpu`, `cuda`, `driver`, `cores`, `mem_gb`, `disk_free_gb`, `python`, and `can_root` (the box is root or has passwordless sudo — i.e. whether it can install packages or power off). Cores and memory are cgroup-aware; `nproc` on AutoDL reports the host, not your quota.

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

Interpretation cheats: `daemon.alive=false` with nonempty `pending/` means nothing will run until the dispatcher auto-heals or someone runs `shunt start` on the box. `stop=true` means draining. A task absent from all lists but present in `spool ls` has not been delivered yet.

3. Ask the billing model at registration and write it into the row's `expire` field: owned or lab boxes (and `localhost`) get `expire = "unlimited"`; pay-as-you-go gets `expire = "metered"` (no deadline — it burns until the balance does, so keep an eye on spend anyway); subscription boxes get the deadline (`expire = "2026-02-01T00:00:00+08:00"`, keep the offset so `status` can compute `expires_in_s`). The vendor console shows the expiry — ask the user for it; don't guess. When a subscription's `expires_in_s` is close to the next task's runtime, warn before queueing rather than after. `status --all --json` surfaces both `expire` and `expires_in_s`.
4. Ask what the box needs to become runnable — conda env, PATH exports, sourced rc files, a non-bash shell. The first time the user (or you, watching a manual run) discovers the working preamble, record it in the row: `env_setup` is a shell snippet and `shell` is what runs it (default bash; conda/zsh setups need the right shell recorded or the source fails silently). Then `shunt client env <name>` pushes it to `<qroot>/env_setup` + `<qroot>/shell` and every task after that runs with it — no per-task boilerplate, and `--file` tasks get it too. Never put `exit`/`exec` in the snippet: it runs inside the task's wrapper shell, so `exit` kills the launch. `bootstrap` re-pushes automatically.
5. Endpoint drift: `status --all --json` reports `connect_failures` and `endpoint_rotates` per client. 3+ consecutive failures means stop retrying and escalate: for a rotating vendor ask the user for the console's current IP/port and run `shunt client update <name> --host <h> --port <p>` (rewrites the row AND clears mux/known_hosts/fail-streak state that hand-editing leaves pinned to the dead endpoint), then retry once; for stable-endpoint vendors (AutoDL) the box is more likely off or asleep — same escalation, different fix. Don't nag on the first failure — the counter only escalates after the streak, and any successful ssh clears it.
6. `project bind`'s workdir is where task data lands — user-supplied, never inferred from a template's deploy_root (that is where the QUEUE lives, unrelated to where experiments belong). Ask before binding. Same for the template: only use one matching the actual vendor (`client templates` lists them; `generic` for an unknown vendor — autodl's `/root/autodl-tmp` defaults pollute a foreign box). For ad-hoc box probing use `shunt client exec <name> -- <cmd>` — managed ssh (mux, retries, our key/known_hosts); never hand-roll an `ssh -i ...` invocation.
7. Host keys are shunt's business, never the user's: shunt TOFU-accepts into `~/.config/shunt/known_hosts`. **Never** echo, `ssh-keyscan >>`, or otherwise touch `~/.ssh/known_hosts` — a rebuilt container changes its key, and a stale entry there means the user's own ssh hits REMOTE HOST IDENTIFICATION HAS CHANGED with no obvious culprit. When shunt hits a changed key (`Offending key`/`IDENTIFICATION HAS CHANGED` in an ssh error), run `shunt client hostkey <name>` — it drops the stale entry from OUR store and kills the mux master, then reconnect (fresh TOFU).
8. Deregistering a box is `shunt client rm <name>` — remote daemon stop + authorized_keys revocation (best-effort), then every local artifact (row, keys, mux socket, state, spool, project bindings). Add `--purge` to also `rm -rf <qroot>` on the box. Task data in bind workdirs is never deleted — the command lists it for the user to decide. Never hand-run the dozen cleanup steps.

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
