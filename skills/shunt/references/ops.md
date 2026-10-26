# Ops — running and controlling work

Day-2 command detail: submit, spool, queue control, tunnel, observe, task ledger, serve, upgrade, feedback. All commands run on the dev machine unless marked client.

## submit

```bash
shunt submit -p riboworld -c rw-a --guard runs/exp1.json -- \
    uv run python train.py --config exp1.yaml

shunt submit -c rw-a --file ./mytask.sh     # pre-rendered script
shunt submit -c rw-a --slug exp2 -- python eval.py
shunt submit -c rw-a --env CUDA_VISIBLE_DEVICES=1 --timeout 7200 \
    --gpu-mem-mb 20000 -- python eval.py    # per-task env, budget, sizing
```

`submit` writes a spool entry (`spool/<id>.json` + `<id>.sh`) and returns. Delivery happens when the dispatcher next reaches the box. The binding's `workdir`/`env` travel inside the task body itself, as comment-head directives the runner parses at spawn:

```bash
# shunt:workdir=/abs/path     # runner cwd for this task
# shunt-env:KEY=value         # env overlay; beats the queue env file
```

Other `submit` flags: `--timeout <s>` is a wall-clock budget (TERM at the deadline, KILL after a 10 s grace — the task lands `failed/`); `--retry N [--retry-delay <s>]` reschedules on rc 75 / OOM (see `playbook.md`); `--gpu-mem-mb`/`--ram-mb`/`--cpu-cores` are DECLARED sizing estimates recorded in the task head and shown beside measured usage — advisory, never gated on; `--env KEY=value` (repeatable) is a per-task overlay that beats the binding's env per key; `--hold[=REASON]` and `--hold-on-box[=REASON]` park the work instead of letting it run (see the holds section below).

A `--file` script can carry its own directives the same way.

The guard contract is REQUIRED for safe at-least-once execution. The generated task body begins:

```bash
if [ -e runs/exp1.json ]; then echo "guard hit: runs/exp1.json exists; skipping"; exit 0; fi
exec uv run python train.py --config exp1.yaml
```

Guard on the artifact the task produces, relative to the binding's `workdir` (tasks run with `cwd = workdir`, no `cd` needed). If the artifact is only written on success, a rerun after a mid-flight kill recomputes. If the artifact lands early, a rerun exits 0 fast. Never submit an unguarded task that is not naturally idempotent (pure appends, side-effect-free reruns are the exceptions).

## tunnel (outbound proxy)

```bash
shunt tunnel -c rw-a up              # box-side SOCKS5 on 127.0.0.1:1080,
                                    # exiting via the dev machine's network
shunt tunnel -c rw-a up --port 2080  # custom port
shunt tunnel -c rw-a status
shunt tunnel -c rw-a down
```

For boxes behind a hostile/poisoned network (mainland China vendors): `tunnel up` opens `ssh -N -R` remote dynamic forwarding, then writes `ALL_PROXY=socks5h://127.0.0.1:<port>` into the box's `<qroot>/env` — every task after that picks the proxy up transparently (git clone, cargo fetch, curl). `tunnel down` removes both. Tasks keep running through it as long as the tunnel ssh stays alive; check `status` if tasks start failing on network.

## status and logs

```bash
shunt status --all            # every registered box, human table
shunt status --all --json     # same, machine-readable + billing fields
shunt status -c rw-a --json   # one box
shunt logs -c rw-a            # daemon log
shunt logs -c rw-a exp1       # task log (pattern match)
shunt cat -c rw-a exp1        # the task's task.sh verbatim — what was actually submitted
shunt dlog --root <qroot>     # daemon log only (client convenience)
shunt logs --root <qroot> -f exp1  # follow on the client itself; remote follow is unsupported
```

`status` answers where a task is; `task` answers how it ended. For field-by-field interpretation see `state.md`. If `logs` shows nothing, check `shunt spool ls`: the task may still be staged because the box is off or SSH failed.

## queue control

```bash
shunt cancel -c rw-a exp3        # remove a pending task (unique substring)
shunt prio -c rw-a exp4 10       # renumber into priority band 0000-0099
shunt kill -c rw-a exp1          # TERM the running task's pgid, then KILL
shunt clean -c rw-a --pending    # bulk-remove every pending entry
shunt clean -c rw-a --done --failed  # clear terminal listings; --archive empties .archive/, --all does all four
shunt clean -c rw-a              # refuses with per-track counts — never a silent no-op
shunt workers -c rw-a            # resolved pool size + source
shunt workers -c rw-a 4          # resize; takes effect next poll
shunt shutdown -c rw-a on 120    # arm idle poweroff, 120 s grace
shunt shutdown -c rw-a status    # armed state + the command that would fire
shunt shutdown -c rw-a off
# the row's `shutdown_cmd` is pushed at every arm; `shutdown on --cmd`
# overrides it once. Set `shutdown_cmd = "true"` (or a marker script)
# on test rows so a copied registry can never hit a real poweroff.
shunt spool ls                   # staged-but-undelivered submissions
shunt spool remove <id>          # drop a staged entry outright
shunt dispatch --once            # one spool sweep (tests, manual push)
shunt submit -c rw-a --hold="awaiting lease decision" -- python train.py
shunt spool hold <id> "reason"   # hold an already-staged entry
shunt spool release <id>         # let the next sweep deliver it
```

A held spool entry is skipped by EVERY dispatch sweep — `--once` and the loop alike: no probe, no push. Use it for work staged ahead of a decision (lease approval, quota, a prerequisite run): staging is free, but an unheld entry fires the moment the box answers. `spool ls` carries two hold columns — `held in spool[: reason]` and `held on box[: reason]` — so both gates stay visible before and after delivery. `--hold` and `--hold-on-box` take their reason only via `=` (`--hold=why`), never a bare word — that slot belongs to task files.

## Box admission holds — delivered but not claimable

```bash
shunt submit -c rw-a --hold-on-box="GPU lease pending" -- python train.py
shunt hold -c rw-a exp2 "waiting on the dataset copy"
shunt release -c rw-a exp2
```

`--hold-on-box` still delivers: the dispatcher pushes the task on its next sweep, and the box installs `pending/<entry>/held` atomically with the entry, so the daemon skips it without changing its position. `shunt hold <pattern> [reason]` parks an entry already sitting in `pending/`; `shunt release <pattern>` clears the gate so the next poll can claim it. The gate is one-shot — a claim consumes it, so a rescheduled retry or an orphan requeue never waits for a second release. `status --json` exposes it as `pending_detail[].held`/`hold_reason` (a local `status --root` table annotates the entry `held` or `held: <reason>`), and `cat`, `prio`, `cancel`, and `clean --pending` still reach a held entry. A task already in `running/` cannot be re-parked: cancel before it is claimed, or resubmit with `--hold-on-box`.

## task ledger — what a spool id became

```bash
shunt task ls                    # spool id -> client/queue entry + outcome (done rc=0, failed rc=N, ...)
shunt task show <id>             # one record; a still-open record queries the box live and writes back
shunt task events                # server-side events.jsonl stream
```

The ledger is the join between "staged in the spool" and "ran on the box": every submission records its spool id, the queue entry name it became, and the terminal result when known. Reach for `task ls`/`task show` instead of correlating `spool ls` with `status` by hand — `status` answers where it is, `task` answers how it ended.

`cancel`, `prio`, `kill`, `clean`, `workers`, `shutdown`, `logs`, `status` all take `-c <client>` on the server side and forward over SSH. Without `-c` they operate on the local queue (client side).

Notes:

- `prio` refuses if the target `NNNN_slug` name already exists. Numbers 0000-0099 are the manual band and always claim ahead of normal sequence numbers (first normal task is 0100).
- Raising `workers` dispatches immediately; lowering only stops new dispatches. Running tasks drain naturally. To free a slot now, `kill`.
- `cancel` matches pending by unique substring, `kill` matches running likewise; 0 or >1 match is an error. For bulk cleanup use `clean`: its flags select whole sets (`--pending`, `--done`, `--failed`, `--archive` for the `<track>/.archive/` overflow, `--all`), `running/` is never a target, and entries claimed mid-sweep are skipped, never half-deleted. A 0-match refusal names the pattern's hits in the other tracks — "already running" means `shunt kill`, and a task claimed between submit and `cancel` is a moved task, not a matching failure.

## serve — the ops dashboard

```bash
shunt serve up                   # detached dashboard on 127.0.0.1:8747
shunt serve status               # liveness + endpoint + log path
shunt serve restart              # hard down+up onto the installed binary
shunt serve down
shunt serve run --port 8799      # foreground (dev loop: just web + vite proxy)
```

A read-only embedded web UI + websocket snapshots of clients, spool, tasks, and rooms (design.md §18). Loopback-only by default; `--bind` off loopback requires `--token` (`?token=`, `x-shunt-token`, or `Authorization: Bearer`). To watch a remote dev machine, `ssh -L PORT:127.0.0.1:PORT <dev>` — never expose it bare. `--interval-s` sets the wire-probe cadence (one ssh exec per queue per tick; local state refreshes every 2 s). Probing is patient by design: an expired row is never probed automatically, a failing box is retried on a doubling 30 s→10 min clock once its streak hits 3, and at 9 consecutive failures the sweep stops entirely — the row then shows why, plus a "probe now" button that forces one round trip (`POST /api/reprobe`); editing the row (renewing `expire`, fixing qroot) clears that streak, so a resumed box heals itself on the next sweep. The page's own reconnect is bounded too: five attempts with a doubling wait, then `disconnected` and a retry button instead of retrying forever.

## upgrade — keeping shunt itself current

`shunt upgrade` self-upgrades the dev binary from the latest GitHub release (`--check`/`--json` read-only, `--force` to reinstall); `shunt upgrade -c rw-a` upgrades the box's binary to the latest musl build through the normal bootstrap path — that is also how wire-protocol drift gets fixed after a dev-side upgrade. `shunt version [-c rw-a]` reports either side's build.

Upgrade handoffs: every live queue daemon gets a SIGUSR2 (§3.5a) — the image re-execs its own argv in place, keeping the pid and adopting in-flight tasks; a pre-handler image is refused via `daemon.ver`, never killed. The `serve` dashboard is the exception: it holds only the merged snapshot, never in-flight work, so upgrade restarts it the plain way (`down` then `up`) — a brief port gap and one dropped websocket per open page, no capability gate, so any running image moves onto the new binary. A stopped daemon is started (and immediately exits again on an `exit_on_idle` box with an empty queue); a stopped dashboard is left alone. A dispatcher carries no pidfile, so upgrade discovers live ones by a /proc cmdline scan and names their pids — send USR2 or restart them yourself (a pre-handler dispatcher would die; spool + dedup make a restart lossless anyway). `upgrade -c` on a `local = true` row is an explicit no-op — local daemons run the dev machine's own image.

## Reporting problems with shunt

Hit a shunt defect, a confusing error, or a UX wart? File it upstream — that is how the tool improves:

```bash
shunt feedback "one-line title" --kind bug --body "what you ran, what happened, what you expected"
# kinds: bug | ux | feature | docs; --client rw-a attaches the row's probe facts
```

- The report lands as a GitHub issue (`[feedback/<kind>] …`) on the upstream repo (or `server.toml` `feedback_repo` / `$SHUNT_FEEDBACK_REPO`). Deduped within 24 h; capped at 5/h — if delivery fails the report is persisted and `shunt feedback ls` / `shunt feedback retry` manage it.
- Redaction covers every client row's `ssh.host/port/user/password/key` and `ssh.options` entries, the `$HOME` prefix, and registered names — client names become `<BOX_NAME>`, projects `<PROJECT_NAME>`, queues `<QUEUE_NAME>`, case-insensitively, with generic words (`localhost`, `default`, `admin`, `root`, …) exempt. That is still no guarantee: never paste tokens, passwords, or private data into the body — the issues are public, and the scrubber cannot know an arbitrary secret.
- Include the command you ran and the JSON/log lines that matter; do not attach credentials or hostnames the scrubber cannot know about.
