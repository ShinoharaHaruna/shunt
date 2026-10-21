# shunt

English | [简体中文](README.cn.md)

![shunt — a railway switching yard routing task cartridges into a GPU compute module](docs/assets/readme/hero.png)

<p align="center">
  <strong>Queue experiments. Power down when done.</strong><br>
  A task queue for rented GPU boxes, operated by you or your AI agent over SSH.
</p>

<p align="center">
  <a href="Cargo.toml"><img alt="Rust 2021" src="https://img.shields.io/badge/Rust-2021-313c40?style=flat-square"></a>
  <img
    alt="Platform: Linux"
    src="https://img.shields.io/badge/platform-Linux-313c40?style=flat-square"
  >
  <a href="docs/design.md#5-wire-protocol"><img alt="Wire: SSH + JSON" src="https://img.shields.io/badge/wire-SSH_%2B_JSON-24776b?style=flat-square"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-bb6238?style=flat-square"></a>
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#how-it-works">Architecture</a> ·
  <a href="#built-for-agents">Agent guide</a> ·
  <a href="docs/design.md">Design spec</a> ·
  <a href="#contributing">Contributing</a>
</p>

GPU experiments can run overnight. Hourly billing keeps running after they finish. shunt handles the gap: stage work from your dev machine, keep a rented box's worker pool busy, and let the box power itself off when its queues drain and idle shutdown is armed.

One Rust binary runs on both sides. Tasks, logs, and queue state live on disk; the remote interface is a CLI over SSH. A human powers the box on in the vendor console. shunt takes it from there.

## Why shunt

| When you need to…                   | shunt gives you…                                                                                                                                      |
| ----------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Submit before the box is online** | A persistent dev-side spool. The dispatcher delivers when SSH becomes reachable and starts a dead queue daemon before pushing.                        |
| **Keep several experiments moving** | A concurrent worker pool, FIFO claim order, a manual priority band, and live resizing. Lowering the pool size lets running work finish.               |
| **Recover after interruption**      | At-least-once execution: orphaned running tasks return to the front of the queue on daemon restart. Artifact guards can skip completed work on rerun. |
| **Stop a task and its children**    | A separate process group for each task, with identity checks before signaling. Queue operations target recorded task processes.                       |
| **Avoid paying for an empty queue** | Opt-in idle shutdown with a grace countdown. New work cancels the countdown; busy sibling queues on the same box block poweroff.                      |
| **Give an agent useful context**    | Structured queue status, a task ledger, cgroup-aware machine facts, and configured billing metadata for the agent to reason about.                    |

## Quick start

### 1. Install from source

Use Linux with a stable Rust toolchain, a system linker, and Bash. Remote operation also needs OpenSSH tools (`ssh`, `scp`, and `ssh-keygen`) on the dev machine.

```bash
git clone https://github.com/ShinoharaHaruna/shunt.git
cd shunt
cargo install --path . --locked
shunt --help
```

### 2. Try the queue locally

The built-in `localhost` client uses the same CLI protocol through local process execution. You can try the full spool → dispatcher → queue path without SSH or a GPU:

```bash
# The task creates a completion marker in the local queue directory.
shunt submit -c localhost --slug hello --guard hello.done -- touch hello.done
shunt dispatch --once
shunt status -c localhost --json
shunt logs -c localhost hello

# When finished experimenting, drain the local daemon and exit.
shunt stop --root ~/.config/shunt/queue
```

Execution is asynchronous: repeat `status` or `logs` after a moment to see the result. The default local queue is `~/.config/shunt/queue/`; this demo leaves `hello.done` there, so repeating it skips the completed task. `stop` drains the daemon's running tasks; idle shutdown is a separate, opt-in operation.

### 3. Connect a rented GPU box

Power the box on in the vendor console, then run these commands on your dev machine. This example uses AutoDL's persistent mount; choose the `generic` template and configure paths for other vendors.

```bash
shunt client add gpu-a --template autodl

# Fill in ssh.host and ssh.port from your vendor console.
# shunt generates a dedicated keypair and fills ssh.key for you.
${EDITOR:-vi} ~/.config/shunt/clients/gpu-a.toml

shunt client bootstrap gpu-a

shunt project add experiments
shunt project bind experiments --client gpu-a \
    --workdir /root/autodl-tmp/experiments --env TASK_THREADS=4
```

Install the generated public key on the box before bootstrapping, or configure `ssh.password` in the client row and install `sshpass` for password-assisted key deployment. Bootstrap copies the current binary to the box, initializes the queue, starts the daemon, and probes machine facts, so the binary must match the box's CPU architecture and runtime.

The binding's `workdir` must already contain your code, data, and runtime environment. Bootstrap deploys shunt; project binding records where your experiments run.

### 4. Submit, dispatch, and drain

Assuming your `train.py` writes `runs/exp1/metrics.json` on successful completion:

```bash
shunt submit -p experiments -c gpu-a --slug exp1 \
    --guard runs/exp1/metrics.json --timeout 14400 -- \
    uv run python train.py --out runs/exp1

shunt dispatch --once               # deliver the current spool
shunt status -c gpu-a --json         # inspect pending and running work
shunt logs -c gpu-a exp1             # inspect the task log

# Opt in after submitting the batch; the box checks for idle locally.
shunt shutdown -c gpu-a on
```

Keep `shunt dispatch` running in a separate dev-machine terminal for continuous delivery, including tasks submitted while the box is off. A successful delivery leaves the task in the box's own queue, where its daemon runs it independently of the dispatcher.

`--timeout 14400` sets a four-hour wall-clock limit for this task. `--guard` checks a path relative to the binding's `workdir` and skips execution if it already exists. `uv` and `train.py` are example workload dependencies; use the command your project needs.

## How it works

![Architecture: the dev machine holds the registry, spool, and dispatcher; SSH carries commands to the GPU box and returns JSON; the box runs its filesystem queue, worker pool, and opt-in idle shutdown](docs/assets/readme/architecture.svg)

The name comes from railway shunting. Your dev machine is the marshalling yard; each GPU box is a destination siding. Projects and clients have a many-to-many relationship: each binding supplies the working directory and environment for that project on that machine.

| On your dev machine                                                              | On each GPU box                                                                       |
| -------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| **Registry** — editable TOML files for clients, projects, bindings, and billing. | **Queue** — task directories under `pending/`, `running/`, `done/`, and `failed/`.    |
| **Spool** — submissions stay staged until the box acknowledges delivery.         | **Daemon** — claims tasks, runs workers, records logs, and recovers orphaned entries. |
| **Dispatcher** — pushes work over SSH and restarts a dead-but-reachable daemon.  | **Idle shutdown** — checks local and registered sibling queues before poweroff.       |

The wire is `ssh <box> 'shunt <cmd> --json'`. No HTTP service or reverse connection to the dev machine is required. The same binary also supports a local transport for `localhost`.

### A queue you can inspect

![Task lifecycle: pending tasks are claimed into running, then finalized into done or failed; daemon restart requeues orphaned running entries; armed idle shutdown counts down only after the queue drains](docs/assets/readme/queue-lifecycle.svg)

Each task is a directory named `NNNN_<slug>`. It contains `task.sh` and accumulates a `log`, a live `pid` record, and a terminal `exit` code. Queue transitions use filesystem renames. The worker count is re-read on every poll, so resizing needs no restart.

**Execution is at least once.** A daemon restart or a lost delivery acknowledgement can cause a rerun. A guard is an existence check before execution, so publish the guarded artifact only when it represents completed work, and make reruns safe. It does not provide exactly-once execution or a lock against overlapping attempts. See the [guard contract](docs/design.md#313-task-file-format-and-the-guard-contract) and the [task template](task_template.sh).

## Everyday controls

Run these commands on the dev machine. Client-specific examples use the registered client `gpu-a`.

| Intent                                  | Command                          |
| --------------------------------------- | -------------------------------- |
| See every registered box                | `shunt status --all --json`      |
| See submissions awaiting delivery       | `shunt spool ls`                 |
| Inspect a submitted task by its ID      | `shunt task show <id> --json`    |
| Read the server's task event stream     | `shunt task events`              |
| Resize the worker pool                  | `shunt workers 4 -c gpu-a`       |
| Move a pending task to the front        | `shunt prio exp1 0 -c gpu-a`     |
| Cancel a pending task                   | `shunt cancel exp1 -c gpu-a`     |
| Stop a running task's process group     | `shunt kill exp1 -c gpu-a`       |
| Arm idle shutdown                       | `shunt shutdown -c gpu-a on`     |
| Disarm idle shutdown                    | `shunt shutdown -c gpu-a off`    |
| Check the resolved poweroff command     | `shunt shutdown -c gpu-a status` |
| Refresh machine facts                   | `shunt client probe gpu-a`       |
| Open the optional box-side SOCKS5 proxy | `shunt tunnel up -c gpu-a`       |

Task-control patterns such as `exp1` must identify one task; use the full queue name when a pattern is ambiguous. The task ledger maps a submission ID to that queue name. `task show` queries the box lazily to resolve an outcome; an entry outside the recent-status window can appear as `unknown`.

Use `shunt <command> --help` for flags and the [CLI reference](docs/design.md#10-cli-surface) for the full surface. Client commands also work directly on a box with `--root <qroot>`.

## Built for agents

The CLI is also the agent interface. Queue status and task-ledger queries expose structured JSON; task and daemon logs provide the detail behind those states. Machine probes read cgroup CPU and memory limits, and status exposes configured billing rates and expiry metadata. An agent can use that context to choose a target and plan a batch.

The included [shunt skill](skills/shunt/SKILL.md) teaches setup, guarded submissions, queue interpretation, failure handling, and the cost-aware operating loop. Point your agent at it before managing a rented box.

Install the skill by linking `skills/shunt/` into wherever your agent loads skills:

```bash
ln -s "$PWD/skills/shunt" ~/.agents/skills/shunt   # shared agentskills directory
# or: cp -r skills/shunt ~/.claude/skills/shunt   # Claude Code user skills
# or: cp -r skills/shunt .claude/skills/          # project-local
```

```bash
shunt task ls --json
shunt status --all --json
shunt task show <id> --json
```

shunt exposes the facts and executes explicit commands. Choosing a machine, deciding whether a run fits the budget, and powering a box on remain with the human or the surrounding agent workflow.

When an agent hits a shunt defect or a rough edge, `shunt feedback "title" --body "…"` files it as a GitHub issue ([design.md §14](docs/design.md)): registry secrets are auto-redacted, reports persist before delivery (at-least-once), and dedup plus an hourly cap keep a looped caller from spamming the tracker.

## Operating boundaries

- **Independent tasks.** There is no dependency graph, automatic machine selection, or multi-machine load balancing; each submission targets a client.
- **Explicit failure handling.** Failed, killed, and timed-out tasks land in `failed/`. There is no automatic failure-retry policy; inspect the log and resubmit when appropriate. Orphan recovery after a daemon restart is separate.
- **Opt-in poweroff.** Idle shutdown uses a configurable command and a grace period, 60 seconds by default. Verify that the command actually stops your vendor's instance; shunt does not control the vendor's billing rules. An enqueue racing the final poweroff check can still lose to shutdown.
- **Explicit deployment.** Project binding records paths and environment; it does not synchronize source code, provision dependencies, or create the remote workdir.

The Rust implementation covers the registry, spool, dispatcher, SSH and local transports, tunnel, and client queue daemon. It is actively developed; [docs/design.md](docs/design.md) defines the behavioral contract and its edge cases.

## Contributing

Start with the [design spec](docs/design.md) and [repository guide](AGENTS.md). For a queue-behavior change, update the contract and cover the relevant transition or failure case.

```bash
just setup       # enable the versioned Git hooks
just fmt         # format Rust, Markdown, TOML, and JSON
just check       # format check + clippy (-D warnings) + tests
```

Install `just` and `dprint` for these recipes; Rust formatting and Clippy come from the configured toolchain. Hooks check formatting before commit and run Clippy before push. They never rewrite files; the full test suite runs through `just check` or `just test`.

### Releases

Push a SemVer tag such as `v0.1.0` (or `0.1.0`) on the commit to release. The version, excluding the optional `v` prefix, must match `package.version` in `Cargo.toml`; update `Cargo.lock` with the version change too. The tagged commit must include [the release workflow](.github/workflows/release.yml).

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

The workflow checks formatting, runs Clippy and tests, then publishes a GitHub Release with generated notes, a Linux x86_64 static musl binary archive, and `SHA256SUMS`. Tags with a prerelease suffix, such as `v0.2.0-rc.1`, create prereleases. It uses the built-in `GITHUB_TOKEN`; no additional secret is required. Published releases are not overwritten on reruns.

| Start here                           | For…                                                       |
| ------------------------------------ | ---------------------------------------------------------- |
| [Design spec](docs/design.md)        | Queue semantics, wire protocol, billing, and test strategy |
| [Agent skill](skills/shunt/SKILL.md) | Operating playbook for an AI agent                         |
| [Vendor templates](etc/vendors/)     | AutoDL and generic client defaults                         |
| [Task template](task_template.sh)    | Writing a guarded task script                              |
| [Integration tests](tests/)          | Queue, process, shutdown, and dispatch behavior            |

## License

[MIT](LICENSE).
