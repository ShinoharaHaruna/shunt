# Setup — registering boxes, projects, and named queues

Read this before `client add`, `client rm`, `client update`, `queue add|set`, `project bind`, or `bootstrap`.

## Consent before `client add` — ask the user, never assume

shunt touches a machine the user may share with other people. Before registering a box, ask the user these questions — the agent's job is to record the answers, not to choose them:

1. **Is the box shared with other people or workloads?** If yes, register with `--shared`: server-side `shutdown on` then refuses to arm idle poweroff — powering off would kill other users' jobs — and `bootstrap` never auto-deploys the ssh key. On a shared box also prefer `--exit-on-idle` (below) and never suggest `shutdown on` at all.
2. **SSH identity — let the user pick:**
   - dedicated shunt keypair + auto-deploy: needs `ssh.password` once; appends one line to `~/.ssh/authorized_keys` on the box (the only write shunt makes outside `<qroot>`). Dedicated boxes only — a `shared` row skips this automatically; `client add --password` without `--key` still generates this key for bootstrap to deploy;
   - reuse an already-authorized key: `--key ~/.ssh/id_ed25519`, no password, zero `authorized_keys` writes;
   - manual: leave `ssh.password` empty; bootstrap prints the generated `.pub` for the user to install via the vendor console;
   - password-only transport auth: `client add --key "" --password <password>` (or `client update --key ""` on a row with a password); requires `sshpass` on the dev machine, uses no private key, never touches `authorized_keys`, and skips the `.shunt-owner` claim, so same-qroot collision protection is weaker.
3. **qroot — REQUIRED: ask for the exact path, never silently take a default.** The whole footprint — queue, binary, daemon state — lives inside `<qroot>`. Vendor templates seed `<deploy_root>/queue`, which is fine on a dedicated box but can collide with another tenant's queue on a shared one; get the user to name an explicit `--qroot` they own (e.g. under their work dir). Outside qroot, shunt writes only the optional authorized_keys line and a small self-cleaned daemon-registry entry under `$XDG_STATE_HOME`/`/tmp`. shunt also guards collisions itself: bootstrap claims the qroot via `<qroot>/.shunt-owner` (the client's ssh pubkey) and refuses a queue already claimed by a different install.
4. **Daemon residency.** By default `shunt daemon` stays resident after the queue drains. `--exit-on-idle` makes it exit cleanly on drain (the next submit revives it automatically) — the polite choice on shared or heavily-monitored boxes.
5. **Reachability precondition.** Can the dev machine ssh the box directly, or must something local be established first — a VPN, an overlay interface, a port-forward, a gate to open? Record it as `pre_connect = "<cmd>"` (`--pre-connect` at `client add`/`update`): a `sh -c` snippet that fires once per call after the first connect failure — and before the first attempt while the box is already marked unreachable — then the retries ride whatever it set up. The snippet must be idempotent and non-interactive (stdin is null — no prompts). A precondition needing a resident process hands off to a supervisor (`nmcli`, `systemd`, `autossh`, `openconnect -b`): arranging and supervising whatever keeps it alive is the user's own deployment, never shunt's — a foreground keepalive is killed at the hook's deadline and accomplishes nothing. Where the precondition cannot be automated (manual OTP), write a check that fails fast with a remediation message instead; its stderr rides the unreachable error.

**Two shunt users, one box.** If user A and user B each run their own shunt against box X: under the _same_ unix account (the shared-root-container norm) give each its own qroot — the per-user daemon registry (§3.11) already coordinates idle-shutdown between their daemons, and `.shunt-owner` stops them pointing at one queue by accident. Under _different_ unix accounts their registries can't see each other at all — mark the row `shared` and keep idle poweroff off the table.

## Register → bootstrap → bind

```bash
# 1. Register a box. Vendor template seeds defaults; a dedicated
#    ed25519 keypair is generated at ~/.config/shunt/keys/rw-a and
#    ssh.key is filled automatically — ssh needs no manual keygen.
shunt client add rw-a --template autodl
#    then edit ~/.config/shunt/clients/rw-a.toml:
#      ssh.host, ssh.port (+ ssh.password for unattended key deploy)
#      expire, env_setup, shell — and optionally billing.hourly/currency,
#      pre_connect (reachability precondition, design.md §4.1)

#    `shunt client templates` lists what --template accepts.
# 2. Bootstrap: normally deploys the generated pubkey via sshpass
#    when ssh.password is set (prints the .pub to install once
#    otherwise). With explicit --key "" and a password, transport uses
#    sshpass without generating/deploying a key. Then probes, scp's the
#    binary, and runs remote init + start. Host keys are
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
#   bindings target a (client, queue) pair; --queue NAME selects
#   a named queue (omitted = the client's configured default).
```

### named queues on one box

```bash
# Two independent queues on one rented instance — separate qroots,
# daemons, seq spaces and worker pools over ONE ssh identity.
shunt queue add train -c rw-a --qroot /root/autodl-tmp/queue-train
shunt queue add eval -c rw-a --qroot /root/autodl-tmp/queue-eval
shunt queue bootstrap train -c rw-a      # installs binary + init + start
shunt queue bootstrap eval -c rw-a
shunt queue ls -c rw-a                    # names, qroots, default marker
shunt queue default eval -c rw-a          # unqualified ops land here
shunt queue set eval -c rw-a --exit-on-idle  # patch ONE field in place
shunt queue start|stop|restart eval -c rw-a  # per-queue daemon lifecycle
shunt submit -c rw-a --queue train -- ./task.sh
shunt status -c rw-a --queue eval --json
```

- `--queue` exists on `submit`, `status`, `logs`, `cancel`, `prio`, `kill`, `clean`, `workers`, `shutdown`, `upgrade`, and `project bind`/`unbind`. Without it: configured `default_queue` > the sole queue > ambiguity error — never a guess.
- A staged task pins its `(queue, qroot)`: changing `queue default` afterwards does NOT retarget undelivered work. qroot drift is a loud error, not a silent reroute.
- `client bootstrap`/`client env` cover every registered queue; the `queue` variants scope to one. `upgrade -c` on a multi-queue client requires `--queue` (one binary per qroot).
- `queue add` refuses an existing name; `queue set <NAME>` is the single-field update path — only passed flags change (`--env-setup ""`/`--shell ""` clear, `--no-exit-on-idle`/`--no-workers` clear), the row revalidates before writing, and a no-diff call reports `unchanged`. It is registry-only like `add`: env/shell/residency/workers reach the box on `queue env`, and a changed qroot retargets wire ops while a live daemon and staged work stay pinned to the old root — relocate means stop the old daemon, then `queue bootstrap`.
- `queue rm` refuses while bound, staged, or running work references it, never revokes the shared client key, and `--purge` deletes only that qroot.
- One tunnel per box: `tunnel up` writes `ALL_PROXY` into every queue's env; newly added queues reconcile on up/env automatically.
- `shutdown -c rw-a on` still powers the whole box off — arming refuses while ANY sibling queue is busy or unreachable, including a stopped queue with pending work. This is a preflight, not a lock: an enqueue racing the final poweroff check can still lose (§3.11).

Registry files are human-editable TOML under `~/.config/shunt/` (`$SHUNT_HOME` overrides, used by tests). A project binds to many clients; each binding supplies `workdir` and an env overlay. Useful overlay keys: `PATH_PREPEND`, `TASK_THREADS` (`auto` = cgroup probe).

Re-collect facts any time with `shunt client probe <name>`.

## Client-row rules

1. Ask the billing model at registration and write it into the row's `expire` field: owned or lab boxes (and `localhost`) get `expire = "unlimited"`; pay-as-you-go gets `expire = "metered"` (no deadline — it burns until the balance does, so keep an eye on spend anyway); subscription boxes get the deadline (`expire = "2026-02-01T00:00:00+08:00"`, keep the offset so `status` can compute `expires_in_s`). The vendor console shows the expiry — ask the user for it; don't guess. When a subscription's `expires_in_s` is close to the next task's runtime, warn before queueing rather than after. `status --all --json` surfaces both `expire` and `expires_in_s`.
2. Ask what the box needs to become runnable — conda env, PATH exports, sourced rc files, a non-bash shell. The first time the user (or you, watching a manual run) discovers the working preamble, record it in the row: `env_setup` is a shell snippet and `shell` is what runs it (default bash; conda/zsh setups need the right shell recorded or the source fails silently). Then `shunt client env <name>` pushes it to `<qroot>/env_setup` + `<qroot>/shell` and every task after that runs with it — no per-task boilerplate, and `--file` tasks get it too. Never put `exit`/`exec` in the snippet: it runs inside the task's wrapper shell, so `exit` kills the launch. `bootstrap` re-pushes automatically.
3. Endpoint drift: `status --all --json` reports `connect_failures` and `endpoint_rotates` per client. 3+ consecutive failures means stop retrying and escalate: for a rotating vendor ask the user for the console's current IP/port and run `shunt client update <name> --host <h> --port <p>` (rewrites the row AND clears mux/known_hosts/fail-streak state that hand-editing leaves pinned to the dead endpoint), then retry once; for stable-endpoint vendors (AutoDL) the box is more likely off or asleep — same escalation, different fix. Don't nag on the first failure — the counter only escalates after the streak, and any successful ssh clears it.
4. `project bind`'s workdir is where task data lands — user-supplied, never inferred from a template's deploy_root (that is where the QUEUE lives, unrelated to where experiments belong). Ask before binding. Same for the template: only use one matching the actual vendor (`client templates` lists them; `generic` for an unknown vendor — autodl's `/root/autodl-tmp` defaults pollute a foreign box). For ad-hoc box probing use `shunt client exec <name> -- <cmd>` — managed ssh (mux, retries, our key/known_hosts); never hand-roll an `ssh -i ...` invocation.
5. Host keys are shunt's business, never the user's: shunt TOFU-accepts into `~/.config/shunt/known_hosts`. **Never** echo, `ssh-keyscan >>`, or otherwise touch `~/.ssh/known_hosts` — a rebuilt container changes its key, and a stale entry there means the user's own ssh hits REMOTE HOST IDENTIFICATION HAS CHANGED with no obvious culprit. When shunt hits a changed key (`Offending key`/`IDENTIFICATION HAS CHANGED` in an ssh error), run `shunt client hostkey <name>` — it drops the stale entry from OUR store and kills the mux master, then reconnect (fresh TOFU).
6. Deregistering a box is `shunt client rm <name>` — remote daemon stop + authorized_keys revocation (best-effort), then every local artifact (row, keys, mux socket, state, spool, project bindings). Add `--purge` to also `rm -rf <qroot>` on the box. Task data in bind workdirs is never deleted — the command lists it for the user to decide. Never hand-run the dozen cleanup steps.

## Dedicated cloud example

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

# Optional: only after the user opts into poweroff on a dedicated machine.
shunt shutdown -c rw-a on      # fires ~60 s after running+pending empty (dedicated boxes only — refused on `shared` rows or busy sibling queues)
# Whether poweroff stops billing depends on the provider.
```
