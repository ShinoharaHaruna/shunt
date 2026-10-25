# Playbooks — cost-aware scheduling and failure semantics

## Cost-aware playbook (metered machines)

1. When the user has opted into idle poweroff, arm it right after enqueueing the last task of a batch: `shunt shutdown -c rw-a on`. The countdown only fires when both `pending/` and `running/` are empty, re-checks every second, and cancels if new work arrives — but never arm unprompted; powering off is the user's call. Never arm on a `shared` box — the arm is refused server-side anyway. To drain only a queue daemon, use `shunt queue stop <queue> -c <client>`.
2. Submitting while the box is off costs nothing. The spool holds the task until a human powers the box on and the dispatcher delivers it. Batch overnight submissions; let one power-on drain them.
3. Read `billing.hourly`/`billing.currency` from `status --all --json` before queueing. On hourly billing, unused paid time is already spent: if the instance has 40 min of paid time left and the next task needs 3 h, hold the long task, run short ones, or warn the user. That decision is yours; shunt only exposes the rate.
4. Split multi-hour experiments into checkpointable tasks (`--guard runs/epoch10.pt` style) so an at-least-once rerun after a mid-flight death costs minutes, not the whole run. One giant unguarded 12 h task is the expensive failure mode.
5. When a metered machine is running but `pending/` is empty and you have nothing to add, arm shutdown only if the user opted into idle poweroff — otherwise report relevant idle spend and let them choose. On owned or shared machines, follow the configured daemon lifecycle and resource agreements.

## Failure semantics

- Tasks with an explicit `--retry N` budget can reschedule on rc 75 or recognized OOM signatures after `--retry-delay` seconds (default 60). Failures outside those criteria, timeouts, and exhausted retries land in `failed/` and require inspection before resubmission. Entry directories contain `exit` and `log` sidecars; logs end with `=== exit <rc> <iso>` (legacy flat entries use `.exit`/`.log`).
- A task that fails rc 97 never ran: the binding's `workdir` was unreachable.
- Orphan requeue: if the daemon dies with tasks in `running/`, the next start moves each to `pending/0000_requeued_<name>` (front of queue) and the interrupted log to `failed/<name>.interrupted.<ts>.log`. If the task's process group is still alive, the daemon logs a WARNING and requeues anyway. Your guard is what makes that safe: the rerun must no-op or cheaply resume while the zombie finishes.
- `shunt kill` and daemon TERM forwarding target recorded task process groups. The resulting exit follows the same reschedule or terminal-failure rules above; a matching OOM log can still qualify a retry-declared task for rescheduling after a kill.
