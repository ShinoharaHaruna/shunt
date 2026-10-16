# shunt

A task queue for rented GPU boxes, built so an AI agent can babysit the
box instead of you. Submit experiments, let the queue drain, and the
instance powers itself off before the next billing hour lands.

The problem it solves: vendors like AutoDL bill by wall-clock uptime,
experiments run for hours or days, and nobody wants to sit there
watching a progress bar at 2 a.m. Today the loop is manual — power on,
run, remember to power off. shunt turns the loop into: human pays and
powers the box on in the vendor console → the queue daemon claims work
→ the queue drains → the box shuts itself down. An agent on your dev
machine can drive the whole cycle.

## What it does

- **Filesystem state machine** on the box: `pending/ → running/ → done/`
  or `failed/`, with `.log`, `.pid`, `.exit` sidecars. Moving a task
  between tracks is an atomic `mv`.
- **At-least-once execution.** If the daemon dies mid-task, the next
  start requeues the orphan at the front of the queue. Tasks guard on
  the artifact they produce (`--guard runs/x.json`), so reruns are cheap
  no-ops.
- **Per-task process-group isolation.** Every task runs under its own
  pgid; `shunt kill` signals the group. Nothing outside the queue's own
  pidfiles is ever touched.
- **Live-resizable worker pool.** `shunt workers 4` takes effect on the
  next poll, no restart. Shrinking drains naturally, never kills.
- **Billing-aware idle shutdown.** `shunt shutdown on` arms a countdown
  that powers the box off once the queue empties. The countdown re-checks
  every second and cancels if new work arrives.
- **Submit while the box is off.** Tasks land in a server-side spool and
  the dispatcher delivers them when the box comes up — and restarts a
  dead-but-reachable daemon first.
- **SSH is the only wire.** Server runs `ssh box 'shunt <cmd> --json'`.
  No open ports on the box, no API server, and the box never needs to
  reach back to the dev machine.

## 30-second tour

```bash
# on the dev machine: register a box (vendor template fills the rest)
shunt client add rw-a --template autodl
#   edit ~/.config/shunt/clients/rw-a.toml: ssh.host, ssh.port, ssh.key
shunt client bootstrap rw-a        # probe facts, ship binary, start daemon

shunt project add riboworld
shunt project bind riboworld --client rw-a \
    --workdir /root/autodl-tmp/RiboWorld --env TASK_THREADS=4

# submit work; lands in the spool, survives the box being off
shunt submit -p riboworld -c rw-a --guard runs/exp1.json -- \
    uv run python train.py --config exp1.yaml

shunt status --all                 # aggregate every registered box
```

The agent-facing surface is identical — every command has `--json`, and
`skills/shunt/SKILL.md` teaches an agent (Claude Code, pi, etc.) the
full operational semantics: register, probe, submit, interpret queue
state, and when to arm idle shutdown.

## How it fits together

```text
dev machine (always on)                 GPU box (on-demand)
┌──────────────────────────────┐        ┌───────────────────────────┐
│ shunt                        │        │ shunt (same binary)       │
│  registry: clients+projects  │  SSH   │  pending/running/done/    │
│  spool: staged submissions   │ ─────► │  failed/, workers, STOP   │
│  dispatcher: spool → client  │ ◄───── │  daemon: pool + idle off  │
└──────────────────────────────┘  JSON  └───────────────────────────┘
```

One Rust binary runs both sides. The dev machine holds the registry
(TOML files you can edit and git-track), the spool, and the dispatcher
loop. Each box holds its own queue — the server is a marshalling yard,
each box a destination siding.

Projects and machines are many-to-many: a project binds to one or more
clients, and each binding carries the `workdir` and env overlay for that
pair. A client row records machine facts (`gpu`, `cuda`, `cores`, `mem`,
`disk`) gathered by `probe` — including cgroup-aware core/memory counts,
because `nproc` on these containers reports the host, not your quota —
plus billing rate metadata so an agent can reason about cost.

## Command summary

| command                                                | side   | what it does                       |
| ------------------------------------------------------ | ------ | ---------------------------------- |
| `shunt client add\|bootstrap\|probe\|ls\|rm`           | server | register and set up boxes          |
| `shunt project add\|bind\|ls`                          | server | register projects, bind to clients |
| `shunt submit -p proj -c client [--guard p] -- cmd...` | server | stage a task in the spool          |
| `shunt status [--all] [--json]`                        | server | aggregate queue states             |
| `shunt spool ls`                                       | server | inspect staged submissions         |
| `shunt dispatch`                                       | server | run the spool → client loop        |
| `shunt start\|stop\|daemon`                            | client | queue daemon lifecycle             |
| `shunt enqueue\|new\|cancel\|prio\|kill`               | client | task operations                    |
| `shunt workers [N]`                                    | client | pool size get/set                  |
| `shunt shutdown on\|off\|status`                       | client | idle poweroff                      |
| `shunt status\|logs\|probe`                            | client | queue state, logs, machine facts   |

## What shunt is not

No DAG dependencies (use Airflow). No retry backoff — failures land in
`failed/` and the agent decides. No user-facing HTTP API — the client
CLI over SSH is the API. No multi-machine load balancing — submission
names the client; matching requirements to machine facts is the agent's
job. No remote power-on — on hourly-billed vendors, powering on _is_
paying, and that click stays with a human.

## Status

Active development: the Rust implementation is being built against
`docs/design.md`, which is the behavioral spec. The proven bash
prototype it ports from (orphan requeue, pgid cleanup, flock'd
counters, signal forwarding, and friends) is archived locally under
`reference/` as the oracle — gitignored, since it contains
deployment-specific details.

## License

MIT
