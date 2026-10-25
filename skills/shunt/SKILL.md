---
name: shunt
description: Operate shunt to run experiments on local workstations, lab servers, and cloud CPU or GPU machines. Use for shunt client and project setup, named queues, guarded task submission, resource inspection, project coordination rooms, the ops dashboard, and optional idle shutdown or cost-aware operation.
---

# shunt

shunt is an experiment operations layer for humans and agents, with FIFO + priority-band + at-least-once queues on local or remote machines. One Rust binary runs on both sides: the dev machine is the marshalling yard (registry, spool, dispatcher, coordination rooms), and each experiment machine is a siding with independent named queues (`pending/ running/ done/ failed/` under each `qroot`). CPU and GPU workloads share the same interface. Billing metadata and idle shutdown are optional deployment features.

Mental model:

- The wire is `ssh <box> 'shunt <cmd> --json --root <qroot>'`; the registry's `qroot` is authoritative. No ports, no server-side daemon besides the dispatcher; the only client process is `shunt daemon` (the same binary).
- **Client = box, queue = named subresource.** One client row is one managed instance and one SSH identity (endpoint, key, mux, tunnel, billing, facts). Under it, named queues (`[queues.<name>]` in the row) each own an independent qroot, sequence space, worker pool, daemon, env_setup/shell, and binary. Old flat rows keep working as a literal queue named `default`.
- `localhost` is a peer client using the local transport (execs the `shunt` binary directly — no ssh, no key). Its row `clients/localhost.toml` is written on first use (default queue `~/.config/shunt/queue/`); edit it like any client row — `local_bin`/`timeout_s` tune the loopback transport. `shunt client rm localhost` resets the row to factory defaults (the name is reserved); to actually opt out, write `disabled = true` into the row. Use it for dry runs, integration smoke, and dev-loop tasks on the dev machine itself. `shunt client add <name> --local` creates additional loopback queues.
- **Operate through the CLI, not the files.** Read state via `status --json` / `logs` / `spool ls` / `task ls|show` / `client show` / `queue ls|show` / `room read`; change it via `client add|update|env|rename|rm`, `queue add|set|default|…`, `project add|bind|unbind|remove`, `submit`, `cancel`, `prio`, `clean`, `workers`, `shutdown`. Commands carry side effects a hand-edit misses (mux socket, known_hosts, conn-fail streak, spool refs, `.shunt-owner`, tunnel env). Hand-edit only row fields with no setter (`expire`, `env_setup`, `billing`, … — the seeded-row edit after `client add` is exactly this case), and never write machine-owned state at all: `<qroot>` queue tracks, `spool/`, `known_hosts`, `cm/*.sock`, the daemon registry, room ledgers.
- Execution is at-least-once. Tasks must guard on the artifact they produce so reruns are no-ops.
- On drain, a daemon stays resident by default, or exits if `exit_on_idle` is configured. The machine powers itself off only when idle shutdown has been explicitly armed; shared clients refuse server-side arming. shunt never powers a machine on — a human or upstream automation makes it available.
- Submitting while the box is off is normal: tasks wait in the server-side spool until the dispatcher can deliver.
- The client name is user-supplied, never agent-invented — ask the user what to call the box before `client add`. The name lands in the row filename, every `-c <name>` invocation, and the task/daemon logs; a made-up name puts the registry on a handle nobody recognizes. Same for project names. If a name turns out wrong, fix it with `shunt client rename <old> <new>` — it moves the row, keypair, mux socket, failure streak, spool refs, and project bindings together. Never hand-mv registry files; the sidecar paths keep pointing at the old name.

## Rooms first — check the inbox before acting

Before ANY state-changing action on a box other projects may share — `submit`, `workers`, `kill`, `cancel`, `prio`, `shutdown`, `queue`/`client`/`project` changes — poll the coordination inbox:

```bash
shunt status --all --json --as <your-project>   # top-level rooms[] carries unread counts
shunt room read <room> --as <your-project>      # drains unread and marks your cursor
```

An unread `say` or `decide` is exactly what changes what you were about to do: VRAM budgets, worker caps, GPU splits negotiated while you were away. The poll is a local read — nothing reaches the box — so there is no reason to skip it. No rooms, or all read: proceed. Unread traffic: read it first, then reconsider the planned action against it.

## Consent before `client add` — ask, never assume

Before registering a box, ask the user — record their answers, don't choose them:

1. **Shared?** `yes` → `--shared` (blocks idle poweroff and key auto-deploy); never suggest `shutdown on`.
2. **SSH identity?** dedicated keypair + auto-deploy (needs `ssh.password`), reuse an authorized key (`--key`, the shared-box choice), or manual `.pub` install.
3. **qroot?** REQUIRED — ask for the exact path; the whole footprint lives inside it. Templates seed `<deploy_root>/queue`, which can collide on shared boxes.
4. **Residency?** daemon stays resident by default; `--exit-on-idle` exits on drain — polite on shared boxes.
5. **Reachability?** VPN/overlay/port-forward first → record `pre_connect` (idempotent, non-interactive; resident parts are the user's supervisor problem).

Full rationale, the two-users-one-box topology rules, and the register → bootstrap → bind walkthrough: `references/setup.md`.

## Command map — load the reference for the domain you're in

Everything below runs on the dev machine. Before acting, read the matching file — it carries the flags, output shapes, and follow-up obligations:

| domain                                                                                                                                                           | file                     |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------ |
| Register/deregister clients, projects, named queues (`client add                                                                                                 | update                   | rm | bootstrap | env | hostkey | rename | exec`, `queue add | set | default | rm | bootstrap | env | start | stop | restart`, `project add | bind | unbind | remove`) | `references/setup.md` |
| Run and control work (`submit`, `spool`, `dispatch`, `cancel`, `prio`, `kill`, `clean`, `workers`, `shutdown`, `tunnel`, `task`, `serve`, `upgrade`, `feedback`) | `references/ops.md`      |
| Interpret `status`/`logs`/queue tracks/`daemon.log`/`facts`                                                                                                      | `references/state.md`    |
| `room` verbs (say/read/decide/members/compact/archive/rm)                                                                                                        | `references/rooms.md`    |
| Metered-machine scheduling + retry/timeout/orphan semantics                                                                                                      | `references/playbook.md` |

## Scheduling objective — the utilization/efficiency Pareto front

The default goal when scheduling on shunt is the Pareto frontier between resource occupancy and task throughput: use the capacity allocated to your workloads without oversubscribing into the regime where tasks stop progressing efficiently. Respect shared-machine agreements and other workloads. Idle capacity reduces throughput; oversubscription wastes time on OOM kills and contention. On metered machines both also increase cost.

- Keep `pending/` fed. Stage the next batch in the spool before `running/` drains — staging is free even while the box is off; an empty queue on a running box is the most preventable utilization failure.
- Size the pool to the bottleneck, not the default. `workers -c <box> N` × per-task footprint (GPU VRAM, `TASK_THREADS` cores, RAM — read `facts` in `status --all --json`) should cover the box's capacity without exceeding it. Wall-time rising across similar tasks, or OOM `failed/` entries, means the frontier moved inward: back `workers` off.
- Prefer many small checkpointable tasks over monoliths: fine-grained work bin-packs the box's gaps and holds occupancy high even as tasks finish unevenly.
- At true drain, keep the machine's intended lifecycle: a resident daemon, daemon-only exit, or explicitly authorized poweroff. Use `shutdown on` only when the user has actively opted into idle poweroff (armed it themselves or asked for it). On metered machines, surface idle cost when relevant.

## Safety rules

- Prefer `shunt` commands over reading or writing shunt's files directly (see the mental-model rule); the only sanctioned hand-edit is a client row field that has no setter.
- Never signal anything by name or pgrep. Only the pgids recorded in `running/*.pid` belong to the queue; `shunt kill` is the only way to signal a task. Other jobs on the box are not yours.
- Never let a test reach real `/usr/bin/shutdown`. Tests redirect the command via the `shutdown_cmd` file or `$SHUTDOWN_CMD` env, and never arm `shutdown on` against a box you are not willing to power off.
- Every daemon liveness check is pidfile + `/proc/<pid>/cmdline` match.
- Use placeholders (`root@<host>:<port>`) in docs, examples, and task scripts. Never write real hostnames, ports, or key paths into tracked files.
