use super::kill::kill_task_with;

use super::env::read_env_file;

/// pgid 0/1 in a `pid` file are kill(2) specials, never task
/// groups: `kill(0, sig)` would signal the daemon's own process
/// group and `kill(-1, sig)` would broadcast to every signalable
/// process. `read_pid_file` must reject them, the §3.9a gate must
/// refuse them even when probed directly (pgid_alive(0) succeeds
/// on our own group, and init's pgrp is 1 in containers), and
/// `kill_task` must never reach a signal for either. Asserting on
/// the gate predicate only — no kill(2) is ever issued here.
#[test]
fn kill_special_pgids_are_never_signaled() {
    for pgid in [0u32, 1] {
        assert!(!super::pgid_is_task_group(pgid, None), "pgid {pgid} passed the signal gate");
        match super::signal_pgid(pgid, "TERM") {
            super::SignalOutcome::Failed(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
            _ => panic!("signal_pgid({pgid}) did not refuse"),
        }

        let root = std::env::temp_dir().join(format!("shunt-badpgid-{pgid}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let entry = root.join("running").join("t");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("task.sh"), "exit 0\n").unwrap();
        std::fs::write(entry.join("pid"), format!("{pgid}\n")).unwrap();
        assert_eq!(super::read_pid_file(&root, "running", "t"), None);
        // No pid parses -> "no pid file" error; a pgid that did
        // parse would have to come back Gone, never Signaled.
        match super::kill_task(&root, "t") {
            Err(_) | Ok(super::KillOutcome::Gone) => {}
            Ok(super::KillOutcome::Signaled { .. }) => panic!("kill_task signaled pgid {pgid}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }
}

/// §3.9a ESRCH races (design.md "a kill result is honest"): a
/// group that dies between the §3.9a gate and the TERM send was
/// never signaled — `kill_task` must report `Gone`, not a phantom
/// kill. A group that dies between the escalation re-check and
/// the KILL send did take the TERM but no KILL — `escalated`
/// must not claim a signal that never went out. Both races are
/// unreachable deterministically through real kill(2), so the
/// sender and the drain sleep are injected; the pid file points
/// at a real live process group so the real gates run.
#[test]
fn kill_esrch_races_report_honestly() {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    let pgid = child.id();
    let root = std::env::temp_dir().join(format!("shunt-esrch-{}-{}", std::process::id(), pgid));
    let entry = root.join("running").join("t");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("task.sh"), "exit 0\n").unwrap();
    std::fs::write(entry.join("pid"), format!("{pgid}\n")).unwrap();

    // TERM hits ESRCH: nothing was sent -> Gone.
    match kill_task_with(&root, "t", |_, _| super::SignalOutcome::Gone, |_| {}) {
        Ok(super::KillOutcome::Gone) => {}
        other => panic!("TERM-ESRCH must report Gone, got {other:?}"),
    }

    // TERM delivered, group stays live through the drain, KILL
    // hits ESRCH: TERM landed but nothing escalated.
    let sig = |_: u32, s: &str| match s {
        "TERM" => super::SignalOutcome::Delivered,
        "KILL" => super::SignalOutcome::Gone,
        _ => unreachable!(),
    };
    match kill_task_with(&root, "t", sig, |_| {}) {
        Ok(super::KillOutcome::Signaled { pgid: p, escalated: false }) => assert_eq!(p, pgid),
        other => panic!("KILL-ESRCH must report Signaled{{escalated:false}}, got {other:?}"),
    }

    let _ = child.kill();
    let _ = child.wait();
    std::fs::remove_dir_all(&root).ok();
}

/// A hostile guard path must reach the guard line only as a
/// single-quoted printf argv: inside a double-quoted message the
/// quotes are literal characters and `$()`, backticks, or `"`
/// would still expand when the guard-hit branch runs.
#[test]
fn guard_echo_uses_quoted_form() {
    for guard in ["runs/$(touch X).json", "runs/evil\".json", "runs/`id`.json"] {
        let meta = crate::proto::task::TaskMeta { id: None, slug: "t".into(), guard: Some(guard.into()), workdir: None, env: Default::default(), cmd: vec!["true".into()], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None };
        let body = super::render_task(&meta);
        let q = crate::server::transport::shell_quote(guard);
        let line = format!("if [ -e {q} ]; then printf 'guard hit: %s exists; skipping\\n' {q}; exit 0; fi");
        assert!(body.contains(&line), "body missing quoted guard line for {guard:?}: {body}");
        // The quoted guard must never sit inside a double-quoted
        // string, where its `$()`/backticks would re-expand.
        assert!(!body.contains(&format!("\"guard hit: {q}")), "quoted guard inside a double-quoted echo for {guard:?}");
    }
}

/// Bash smoke check: run the rendered body with a live hostile
/// guard and assert the `$( )` payload never executes — the
/// message prints the guard literally.
#[test]
fn guard_hit_branch_cannot_expand() {
    let root = std::env::temp_dir().join(format!("shunt-guardhit-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let pwned = root.join("PWNED");
    // Relative guard name (bash runs with cwd=root) so the
    // `$( )` payload carries no slashes of its own.
    let guard = root.join("x$(touch PWNED).json");
    std::fs::write(&guard, "").unwrap();
    let meta = crate::proto::task::TaskMeta { id: None, slug: "t".into(), guard: Some("x$(touch PWNED).json".into()), workdir: None, env: Default::default(), cmd: vec!["true".into()], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None };
    let body = super::render_task(&meta);
    let out = std::process::Command::new("bash").arg("-c").arg(&body).current_dir(&root).output().unwrap();
    assert!(out.status.success(), "bash exited {:?}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    assert!(!pwned.exists(), "guard payload executed: {body}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("guard hit: x$(touch PWNED).json exists; skipping"), "unexpected stdout: {stdout}");
    std::fs::remove_dir_all(&root).ok();
}

/// The env file is literal KEY=value (design.md §3.12): spaces
/// survive verbatim — including leading/trailing ones in the
/// value — but quotes are never stripped: `K="a b"` keeps the
/// quote characters as part of the value.
#[test]
fn env_values_are_literal_no_quote_semantics() {
    let root = std::env::temp_dir().join(format!("shunt-envtest-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("env"), "# comment\n\nFOO=a b c\nQUOTED=\"a b\"\nSQUOTED='x'\nSPACED =  padded  \nNOEQ\n").unwrap();
    let m = read_env_file(&root);
    assert_eq!(m["FOO"], "a b c");
    assert_eq!(m["QUOTED"], "\"a b\"");
    assert_eq!(m["SQUOTED"], "'x'");
    // Value-side whitespace is data: only the key is trimmed.
    assert_eq!(m["SPACED"], "  padded  ");
    assert!(!m.contains_key("NOEQ"));
    std::fs::remove_dir_all(&root).ok();
}

/// `=foo` yields an empty key, and `Command::env` panics on it —
/// the parse layer must drop it so the daemon never crashes
/// mid-dispatch (same guard for `# shunt-env:=x` directives).
#[test]
fn empty_env_keys_are_dropped() {
    let root = std::env::temp_dir().join(format!("shunt-emptykey-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("env"), "=orphan-value\nGOOD=1\n  =also-empty\n").unwrap();
    let m = read_env_file(&root);
    assert_eq!(m.len(), 1);
    assert_eq!(m["GOOD"], "1");
    std::fs::remove_dir_all(&root).ok();

    let task = std::env::temp_dir().join(format!("shunt-emptydir-{}", std::process::id()));
    std::fs::write(&task, "#!/usr/bin/env bash\n# shunt-env:=bad\n# shunt-env:OK=yes\n# shunt-env:PAD= x \necho hi\n").unwrap();
    let td = super::task_directives(&task);
    let envs = td.envs;
    // Literal values here too: PAD's spaces ride through verbatim.
    assert_eq!(envs, vec![("OK".to_string(), "yes".to_string()), ("PAD".to_string(), " x ".to_string())]);
    std::fs::remove_file(&task).ok();
}

/// NUL is valid UTF-8, so `read_to_string` passes it through —
/// but `Command::env`/`arg`/`new` panic on it. Every parsed
/// surface must drop NUL-bearing entries the same way empty keys
/// are dropped, or a hostile file crash-loops the daemon
/// (panic in dispatch → orphan requeue → panic again).
#[test]
fn nul_entries_are_dropped_not_panics() {
    let root = std::env::temp_dir().join(format!("shunt-nul-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("env"), "BAD\u{0}KEY=v\nK2=v\u{0}x\nGOOD=1\n").unwrap();
    let m = read_env_file(&root);
    assert_eq!(m.len(), 1);
    assert_eq!(m["GOOD"], "1");

    let task = root.join("t.sh");
    std::fs::write(&task, "#!/usr/bin/env bash\n# shunt:workdir=/x\u{0}y\n# shunt-env:A\u{0}B=x\n# shunt-env:C=d\u{0}e\n# shunt-env:OK=yes\necho hi\n").unwrap();
    let td = super::task_directives(&task);
    assert_eq!(td.workdir, None, "NUL workdir directive must be dropped");
    assert_eq!(td.envs, vec![("OK".to_string(), "yes".to_string())]);
    std::fs::remove_dir_all(&root).ok();
}

/// An indented `#` is still a comment to the shell, so it must
/// neither end the §3.13 head scan nor hide a directive — the
/// same trim_start rule read_env_file already applies. A plain
/// indented comment between directives keeps the scan going, and
/// an indented `# shunt-env:` is honored outright.
#[test]
fn indented_comments_do_not_end_directive_head() {
    let task = std::env::temp_dir().join(format!("shunt-indent-{}", std::process::id()));
    std::fs::write(&task, "#!/usr/bin/env bash\n# shunt-env:EARLY=1\n  # an indented note\n\t# shunt-env:LATE=2\n   # shunt:timeout_s=60\necho hi\n# shunt-env:NEVER=seen\n").unwrap();
    let td = super::task_directives(&task);
    assert_eq!(td.envs, vec![("EARLY".to_string(), "1".to_string()), ("LATE".to_string(), "2".to_string())]);
    assert_eq!(td.timeout_s, Some(60));
    std::fs::remove_file(&task).ok();
}

/// `TASK_THREADS` is literal env data (§3.12): padded `auto `
/// still resolves to the cgroup probe, but a value with no
/// digits caps at 1 *and* says so in the task log instead of
/// silently degrading the task.
#[test]
fn task_threads_garbage_caps_loudly_and_padded_auto_works() {
    for (value, expect_warn) in [("x", true), ("auto ", false)] {
        let root = std::env::temp_dir().join(format!("shunt-threads-{}-{}", std::process::id(), value.replace(' ', "_")));
        std::fs::create_dir_all(root.join("running")).unwrap();
        std::fs::write(root.join("running").join("t.sh"), "#!/usr/bin/env bash\necho \"OMP=$OMP_NUM_THREADS\"\n").unwrap();
        std::fs::write(root.join("env"), format!("TASK_THREADS={value}\n")).unwrap();
        let (mut child, _) = super::spawn_task(&root, "t").unwrap();
        assert_eq!(child.wait().unwrap().code(), Some(0));
        let log = std::fs::read_to_string(root.join("running").join("t.sh.log")).unwrap();
        let omp = log.lines().find_map(|l| l.strip_prefix("OMP=")).and_then(|v| v.parse::<u32>().ok());
        if expect_warn {
            assert!(log.contains("=== warn: TASK_THREADS=\"x\" has no digits; capping threads to 1"), "log: {log}");
            assert_eq!(omp, Some(1), "log: {log}");
        } else {
            assert!(!log.contains("has no digits"), "log: {log}");
            assert!(omp.unwrap_or(0) >= 1, "log: {log}");
        }
        std::fs::remove_dir_all(&root).ok();
    }
}

/// A NUL-bearing `<qroot>/shell` must fall back to bash: spawn
/// must return a live child, never panic inside dispatch.
#[test]
fn nul_shell_file_falls_back_to_bash() {
    let root = std::env::temp_dir().join(format!("shunt-nulshell-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(root.join("running")).unwrap();
    std::fs::write(root.join("running").join("t.sh"), "#!/usr/bin/env bash\nexit 0\n").unwrap();
    std::fs::write(root.join("shell"), "bash\u{0}evil").unwrap();
    let (mut child, _) = super::spawn_task(&root, "t").expect("spawn_task must not fail on NUL shell file");
    assert_eq!(child.wait().unwrap().code(), Some(0));
    std::fs::remove_dir_all(&root).ok();
}

/// Quarantine with a missing source is a no-op: when the entry
/// already left the track (e.g. finalize's rename succeeded and
/// only the exit-file write failed) no bogus `failed/<name>/`
/// holding only `shunt_error` may appear beside the real entry
/// in done/.
#[test]
fn quarantine_missing_source_is_noop() {
    let root = std::env::temp_dir().join(format!("shunt-qnoop-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(root.join("running")).unwrap();
    std::fs::create_dir_all(root.join("done")).unwrap();
    // Real entry already finalized into done/.
    std::fs::create_dir_all(root.join("done").join("0001_t")).unwrap();
    super::quarantine(&root, "running", "0001_t", "exit write failed").unwrap();
    assert!(!root.join("failed").join("0001_t").exists(), "ghost failed/ entry was created");
    assert!(root.join("done").join("0001_t").exists());
    std::fs::remove_dir_all(&root).ok();
}

/// `exit`/`timeout` are written inside the running entry BEFORE
/// the rename (design.md §3.9 deviates: write-then-mv): on
/// success the sidecars simply travel with the dir to done/.
#[test]
fn finalize_writes_exit_inside_dir_entry() {
    let root = std::env::temp_dir().join(format!("shunt-fin-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let entry = root.join("running").join("0001_t");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("task.sh"), "exit 0\n").unwrap();
    std::fs::write(entry.join("log"), "=== task 0001_t\n").unwrap();
    std::fs::write(entry.join("pid"), "12345\n").unwrap();
    super::finalize(&root, "0001_t", 0, None).unwrap();
    let done = root.join("done").join("0001_t");
    assert_eq!(std::fs::read_to_string(done.join("exit")).unwrap(), "0\n");
    assert!(!done.join("pid").exists());
    assert!(!entry.exists());
    std::fs::remove_dir_all(&root).ok();
}

/// A failed `exit` write must leave the entry still in running/
/// so the caller's quarantine lands it in failed/ with a
/// `shunt_error` — never an exit-less terminal entry that
/// `recent_exits` filters into `unknown`.
#[test]
fn finalize_exit_write_failure_leaves_entry_in_running() {
    let root = std::env::temp_dir().join(format!("shunt-finfail-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let entry = root.join("running").join("0001_t");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("task.sh"), "exit 0\n").unwrap();
    // `exit` as a directory makes fs::write fail.
    std::fs::create_dir(entry.join("exit")).unwrap();
    assert!(super::finalize(&root, "0001_t", 0, None).is_err());
    assert!(entry.exists(), "entry left running/ despite failed exit write");
    assert!(!root.join("done").join("0001_t").exists());
    // The caller's fallback still sees a real entry to quarantine.
    super::quarantine(&root, "running", "0001_t", "finalize failed (rc 0)").unwrap();
    assert!(root.join("failed").join("0001_t").join("shunt_error").exists());
    std::fs::remove_dir_all(&root).ok();
}

/// A failed task.sh rename on a legacy flat entry must not strand
/// the just-created `dest` holding only `exit`: `list_entries`
/// counts any dir and `recent_exits` reads its `exit`, so the
/// task would report done (rc 0) while the caller's quarantine
/// also lands the flat remainder in failed/ — a phantom
/// double-outcome.
#[test]
fn finalize_flat_tasksh_rename_failure_removes_dest() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("shunt-finflat-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let running = root.join("running");
    std::fs::create_dir_all(&running).unwrap();
    std::fs::write(running.join("0001_t.sh"), "exit 0\n").unwrap();
    std::fs::write(running.join("0001_t.sh.log"), "=== task 0001_t\n").unwrap();
    std::fs::write(running.join("0001_t.sh.pid"), "12345\n").unwrap();
    // Rename needs write perm on both parent dirs; read-only
    // running/ fails the rename AFTER dest/exit were written.
    std::fs::set_permissions(&running, std::fs::Permissions::from_mode(0o555)).unwrap();
    assert!(super::finalize(&root, "0001_t", 0, None).is_err());
    std::fs::set_permissions(&running, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!root.join("done").join("0001_t").exists(), "partial done/ entry survived failed rename");
    // The flat source is untouched so quarantine can still move it.
    assert!(running.join("0001_t.sh").exists());
    super::quarantine(&root, "running", "0001_t", "finalize failed (rc 0)").unwrap();
    assert!(root.join("failed").join("0001_t").join("task.sh").exists());
    std::fs::remove_dir_all(&root).ok();
}

/// Reschedule fixture: a running/ dir entry with a retry
/// directive task.sh plus log + pid sidecars.
fn running_entry(root: &std::path::Path, base: &str, head: &str, log: &str) {
    let entry = root.join("running").join(base);
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("task.sh"), head).unwrap();
    std::fs::write(entry.join("log"), log).unwrap();
    std::fs::write(entry.join("pid"), "1\n").unwrap();
}

/// rc 75 on a retry-declared task requeues into pending/ under
/// the SAME name — the ledger's queue_name keeps resolving —
/// with attempts/retry_after sidecars and the failed log swept
/// out (spawn truncates `log` on the rerun).
#[test]
fn reschedule_rc75_requeues_same_name() {
    let root = std::env::temp_dir().join(format!("shunt-rs75-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(root.join("pending")).unwrap();
    std::fs::create_dir_all(root.join("failed")).unwrap();
    let head = "#!/usr/bin/env bash\n# shunt:retry=2\n# shunt:retry_delay_s=7\nexec false\n";
    running_entry(&root, "0100_t", head, "=== task 0100_t\nbusy\n");
    let td = super::task_directives(&root.join("running/0100_t/task.sh"));
    match super::maybe_reschedule(&root, "0100_t", 75, &td).unwrap() {
        super::Resched::Queued { attempt, via, .. } => {
            assert_eq!(attempt, 1);
            assert_eq!(via, "rc75");
        }
        _ => panic!("rc 75 + retry=2 must requeue"),
    }
    let entry = root.join("pending/0100_t");
    assert!(entry.is_dir(), "entry did not land back in pending/");
    assert_eq!(std::fs::read_to_string(entry.join("attempts")).unwrap(), "1\n");
    let ra: u64 = std::fs::read_to_string(entry.join("retry_after")).unwrap().trim().parse().unwrap();
    assert!(ra >= crate::proto::time::unix_now() + 6, "retry_after not ~7s out: {ra}");
    assert!(!entry.join("pid").exists());
    assert!(root.join("failed/0100_t.attempt1.log").exists(), "failed log not swept");
    assert!(!entry.join("log").exists());
    std::fs::remove_dir_all(&root).ok();
}

/// The OOM heuristic is bounded by declaration: a retry task
/// dying rc 1 with an OOM traceback requeues via the heuristic,
/// while a task WITHOUT `# shunt:retry=` saying the same words
/// still finalizes — the estimate never turns into a silent
/// retry of an arbitrary failure.
#[test]
fn reschedule_oom_heuristic_only_for_retry_tasks() {
    let root = std::env::temp_dir().join(format!("shunt-rsom-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(root.join("pending")).unwrap();
    let oom_log = "=== task 0100_t\ntorch.OutOfMemoryError: CUDA out of memory. Tried to allocate 1.50 GiB\n";
    running_entry(&root, "0100_t", "#!/usr/bin/env bash\n# shunt:retry=1\nexec false\n", oom_log);
    let td = super::task_directives(&root.join("running/0100_t/task.sh"));
    match super::maybe_reschedule(&root, "0100_t", 1, &td).unwrap() {
        super::Resched::Queued { via, .. } => assert_eq!(via, "oom_heuristic"),
        _ => panic!("OOM traceback + retry=1 must requeue"),
    }
    // Same log, no retry declared -> No.
    running_entry(&root, "0200_u", "#!/usr/bin/env bash\nexec false\n", oom_log);
    let td = super::task_directives(&root.join("running/0200_u/task.sh"));
    assert!(matches!(super::maybe_reschedule(&root, "0200_u", 1, &td).unwrap(), super::Resched::No));
    // rc 1, retry declared, but log is clean -> No.
    running_entry(&root, "0300_v", "#!/usr/bin/env bash\n# shunt:retry=1\nexec false\n", "=== task\nsyntax error near token\n");
    let td = super::task_directives(&root.join("running/0300_v/task.sh"));
    assert!(matches!(super::maybe_reschedule(&root, "0300_v", 1, &td).unwrap(), super::Resched::No));
    std::fs::remove_dir_all(&root).ok();
}

/// Lock acquisition and sidecar failures must leave the entry
/// finalizable, and an acquired lock must not escape the error path.
#[test]
fn reschedule_errors_release_lock_and_allow_finalize() {
    use fs2::FileExt;

    for fault in ["lock", "sidecar"] {
        let root = std::env::temp_dir().join(format!("shunt-rsfail-{fault}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        crate::client::queue::init(&root).unwrap();
        let head = "#!/usr/bin/env bash\n# shunt:id=reschedule-error\n# shunt:retry=1\nexec false\n";
        running_entry(&root, "0100_t", head, "busy\n");
        let entry = root.join("running/0100_t");
        if fault == "lock" {
            std::fs::create_dir(root.join(".seq.lock")).unwrap();
        } else {
            std::fs::create_dir(entry.join("attempts")).unwrap();
        }
        let td = super::task_directives(&entry.join("task.sh"));
        assert!(super::maybe_reschedule(&root, "0100_t", 75, &td).is_err());
        assert!(entry.join("task.sh").exists());
        assert!(entry.join("pid").exists());
        assert_eq!(std::fs::read_to_string(entry.join("log")).unwrap(), "busy\n");
        assert!(!entry.join("retry_after").exists());
        assert!(crate::client::queue::pending_list(&root).unwrap().is_empty());
        if fault == "lock" {
            std::fs::remove_dir(root.join(".seq.lock")).unwrap();
            assert!(!entry.join("attempts").exists());
        }
        let lock = std::fs::OpenOptions::new().create(true).append(true).open(root.join(".seq.lock")).unwrap();
        lock.try_lock_exclusive().expect("reschedule leaked .seq.lock on error");
        drop(lock);
        // The daemon takes this fallback after maybe_reschedule
        // returns Err; it must preserve the failure and delivery id.
        super::finalize(&root, "0100_t", 75, None).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("failed/0100_t/exit")).unwrap(), "75\n");
        assert!(!entry.exists());
        let replay = crate::client::queue::enqueue(&root, "copy", head).unwrap();
        assert!(replay.deduped());
        assert_eq!(replay.name(), "0100_t");
        std::fs::remove_dir_all(&root).unwrap();
    }
}

/// `attempts` reaching the declared budget finalizes instead of
/// requeueing — retry=N means N extra runs, not N failures.
#[test]
fn reschedule_budget_exhaustion_finalizes() {
    let root = std::env::temp_dir().join(format!("shunt-rsex-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    std::fs::create_dir_all(root.join("pending")).unwrap();
    running_entry(&root, "0100_t", "#!/usr/bin/env bash\n# shunt:retry=1\nexec false\n", "=== task\n");
    std::fs::write(root.join("running/0100_t/attempts"), "1\n").unwrap();
    let td = super::task_directives(&root.join("running/0100_t/task.sh"));
    assert!(matches!(super::maybe_reschedule(&root, "0100_t", 75, &td).unwrap(), super::Resched::No));
    assert!(root.join("running/0100_t").exists(), "exhausted entry must stay for finalize");
    std::fs::remove_dir_all(&root).ok();
}
