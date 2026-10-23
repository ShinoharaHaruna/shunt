use super::seq::ON_SEQ_LOCK_WAIT;
use super::*;
use std::time::{Duration, Instant};

fn tmp_root(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("shunt-qtest-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    init(&root).unwrap();
    root
}

/// Losing the flat-file claim race must not strand an empty
/// `running/<name>/` ghost: it would count in running_list and
/// keep busy_peers busy forever, blocking idle-shutdown.
#[test]
fn claim_lost_race_leaves_no_ghost_dir() {
    let root = tmp_root("ghost");
    std::fs::write(root.join("pending/0100_x.sh"), "echo hi\n").unwrap();
    // cancel wins the race before the rename lands
    std::fs::remove_file(root.join("pending/0100_x.sh")).unwrap();
    assert!(!claim(&root, "0100_x").unwrap());
    assert!(running_list(&root).unwrap().is_empty());
    assert!(!root.join("running/0100_x").exists());
    std::fs::remove_dir_all(&root).ok();
}

/// A legacy flat claim normalizes inside pending/ and lands in
/// running/ as a complete dir entry — the only state visible to
/// running_list. At no point may running/<name> appear empty.
#[test]
fn claim_flat_normalizes_then_atomic_rename() {
    let root = tmp_root("flatclaim");
    std::fs::write(root.join("pending/0100_x.sh"), "echo hi\n").unwrap();
    assert!(claim(&root, "0100_x").unwrap());
    assert!(root.join("running/0100_x/task.sh").is_file());
    assert!(pending_list(&root).unwrap().is_empty());
    assert_eq!(running_list(&root).unwrap(), vec!["0100_x".to_string()]);
    // No staging litter may survive in pending/.
    let leftovers: Vec<_> = std::fs::read_dir(root.join("pending")).unwrap().flatten().collect();
    assert!(leftovers.is_empty());
    std::fs::remove_dir_all(&root).ok();
}

/// A flat entry whose dir-namespace is already held by a stray
/// file is unclaimable (Err -> quarantine), never Ok(false).
#[test]
fn claim_flat_name_collision_is_err() {
    let root = tmp_root("flatcollide");
    std::fs::write(root.join("pending/0100_x.sh"), "echo hi\n").unwrap();
    std::fs::write(root.join("pending/0100_x"), "stray\n").unwrap();
    assert!(claim(&root, "0100_x").is_err());
    assert!(root.join("pending/0100_x.sh").exists());
    assert!(running_list(&root).unwrap().is_empty());
    std::fs::remove_dir_all(&root).ok();
}

/// The wraparound floor spans all tracks + archives (§3.2): a
/// number held only in done/ or .archive/ must not be reminted —
/// the collision would fail a later claim/finalize rename.
#[test]
fn next_seq_wraparound_clears_all_tracks() {
    let root = tmp_root("hiwater");
    std::fs::create_dir_all(root.join("done/9500_old")).unwrap();
    std::fs::create_dir_all(root.join("failed/.archive/9700_ancient")).unwrap();
    std::fs::write(root.join(".seq"), format!("{SEQ_MAX}\n")).unwrap();
    let n = next_seq(&root).unwrap();
    assert_eq!(n, 9701);
    // true exhaustion: 9999 occupied in a terminal track
    std::fs::create_dir_all(root.join("done/9999_last")).unwrap();
    assert!(next_seq(&root).is_err());
    std::fs::remove_dir_all(&root).ok();
}

/// A re-push after a lost ack carries the same `# shunt:id=`:
/// the second enqueue must return the existing entry's name and
/// mint nothing (§3.3). Bodies without the directive are
/// distinct submissions — each mints its own number.
#[test]
fn enqueue_dedups_on_shunt_id() {
    let root = tmp_root("dedup");
    let body = "#!/usr/bin/env bash\n# shunt:id=abc-123\nexec true\n";
    let first = enqueue(&root, "t-abc123", body).unwrap();
    let EnqueueOutcome::Fresh(name) = first else {
        panic!("first push must mint");
    };
    match enqueue(&root, "t-abc123", body).unwrap() {
        EnqueueOutcome::Deduped(n) => assert_eq!(n, name),
        EnqueueOutcome::Fresh(n) => panic!("re-push minted {n}"),
    }
    assert_eq!(pending_list(&root).unwrap(), vec![name]);
    // No id -> no dedup, even for byte-identical bodies.
    let plain = "#!/usr/bin/env bash\nexec true\n";
    assert!(matches!(enqueue(&root, "x", plain).unwrap(), EnqueueOutcome::Fresh(_)));
    assert!(matches!(enqueue(&root, "x", plain).unwrap(), EnqueueOutcome::Fresh(_)));
    assert_eq!(pending_list(&root).unwrap().len(), 3);
    std::fs::remove_dir_all(&root).ok();
}

/// The dedup scan follows the state machine's forward direction:
/// by retry time the original delivery may already sit in
/// running/ or a terminal track — the re-push must still hit it.
#[test]
fn enqueue_dedup_finds_claimed_and_finished() {
    let root = tmp_root("deduptracks");
    for track in ["running", "done", "failed"] {
        let entry = root.join(track).join("0200_t");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("task.sh"), "#!/usr/bin/env bash\n# shunt:id=zzz-9\nexec true\n").unwrap();
        match enqueue(&root, "t-zzz9", "#!/usr/bin/env bash\n# shunt:id=zzz-9\nexec true\n").unwrap() {
            EnqueueOutcome::Deduped(n) => assert_eq!(n, "0200_t"),
            EnqueueOutcome::Fresh(n) => panic!("entry in {track}/ missed by dedup; minted {n}"),
        }
        std::fs::remove_dir_all(&entry).unwrap();
    }
    std::fs::remove_dir_all(&root).ok();
}

/// Same-key pushes under `.seq.lock` serialize: exactly one
/// mints, every other thread dedup-hits the same entry name.
#[test]
fn enqueue_concurrent_same_key_single_entry() {
    let root = tmp_root("deduprace");
    let body = "#!/usr/bin/env bash\n# shunt:id=race-1\nexec true\n";
    let mut handles = Vec::new();
    for _ in 0..8 {
        let r = root.clone();
        handles.push(std::thread::spawn(move || enqueue(&r, "t-race1", body).unwrap()));
    }
    let mut names = std::collections::BTreeSet::new();
    let mut minted = 0;
    for h in handles {
        let out = h.join().unwrap();
        minted += usize::from(!out.deduped());
        names.insert(out.name().to_string());
    }
    assert_eq!(minted, 1, "more than one push minted a number");
    assert_eq!(names.len(), 1, "threads disagreed on the entry: {names:?}");
    assert_eq!(pending_list(&root).unwrap().len(), 1);
    std::fs::remove_dir_all(&root).ok();
}

/// The actual reschedule path must not move behind an enqueue's
/// pending snapshot. FIFO EOF and observed flock contention order
/// the race; neither the mutation nor its timing is simulated.
#[test]
fn enqueue_dedup_cannot_miss_concurrent_reschedule() {
    use crate::client::exec;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::mpsc;

    enum Progress {
        Waiting,
        Finished,
    }

    let root = tmp_root("dedup-reschedule");
    let body = "#!/usr/bin/env bash\n# shunt:id=repeat-delivery\n# shunt:retry=1\nexec true\n";
    let original = root.join("running/0100_original");
    std::fs::create_dir(&original).unwrap();
    std::fs::write(original.join("task.sh"), body).unwrap();
    std::fs::write(original.join("log"), "busy\n").unwrap();
    let td = exec::task_directives(&original.join("task.sh"));

    let blocker = root.join("pending/0001_block");
    std::fs::create_dir(&blocker).unwrap();
    let fifo = blocker.join("task.sh");
    let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
    let (enqueue_tx, enqueue_rx) = mpsc::channel();
    let r = root.clone();
    let enqueue_thread = std::thread::spawn(move || enqueue_tx.send(enqueue(&r, "copy", body)).unwrap());

    // Opening the writer proves enqueue reached this FIFO after
    // snapshotting pending/ and while holding .seq.lock. Keep it
    // open so the scan cannot advance until reschedule is observed.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut writer = loop {
        match std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&fifo) {
            Ok(writer) => break writer,
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(1)),
            Err(e) => panic!("enqueue did not reach FIFO: {e}"),
        }
    };
    writer.write_all(b"#!/usr/bin/env bash\nexec true\n").unwrap();

    let (progress_tx, progress_rx) = mpsc::channel();
    let (reschedule_tx, reschedule_rx) = mpsc::channel();
    let r = root.clone();
    let reschedule_thread = std::thread::spawn(move || {
        let waiting_tx = progress_tx.clone();
        ON_SEQ_LOCK_WAIT.with(|observer| *observer.borrow_mut() = Some(Box::new(move || waiting_tx.send(Progress::Waiting).unwrap())));
        let result = exec::maybe_reschedule(&r, "0100_original", 75, &td);
        progress_tx.send(Progress::Finished).unwrap();
        reschedule_tx.send(result).unwrap();
    });
    let progress = progress_rx.recv_timeout(Duration::from_secs(5));
    let stayed_running = original.is_dir();
    drop(writer);
    let replay = enqueue_rx.recv_timeout(Duration::from_secs(5)).expect("enqueue deadlocked").unwrap();
    let rescheduled = reschedule_rx.recv_timeout(Duration::from_secs(5)).expect("reschedule deadlocked").unwrap();
    enqueue_thread.join().unwrap();
    reschedule_thread.join().unwrap();
    std::fs::remove_dir_all(&blocker).unwrap();
    let pending = pending_list(&root).unwrap();
    let counter = std::fs::read_to_string(root.join(".seq")).unwrap();
    std::fs::remove_dir_all(&root).unwrap();

    assert!(replay.deduped(), "enqueue missed rescheduling original: deduped=false name={} pending={pending:?}", replay.name());
    assert_eq!(replay.name(), "0100_original");
    assert!(matches!(progress, Ok(Progress::Waiting)), "reschedule did not wait for enqueue's lock");
    assert!(stayed_running, "reschedule moved while enqueue held .seq.lock");
    assert!(matches!(rescheduled, exec::Resched::Queued { attempt: 1, .. }));
    assert_eq!(pending, vec!["0100_original"]);
    assert_eq!(counter, "99\n", "a deduped replay must not mint a number");
}

/// A corrupt `.seq` fails loudly (§3.2) — garbage, negative,
/// and >9999 are all operator damage, never silently reseeded.
/// A missing `.seq` is fine: the high-water floor rebuilds it.
#[test]
fn next_seq_corrupt_counter_bails() {
    let root = tmp_root("corruptseq");
    for bad in ["garbage", "-5", "10000", "99999999", ""] {
        std::fs::write(root.join(".seq"), bad).unwrap();
        let e = next_seq(&root).unwrap_err().to_string();
        assert!(e.contains("corrupt"), "{bad:?} should bail as corrupt, got: {e}");
    }
    std::fs::remove_file(root.join(".seq")).unwrap();
    assert_eq!(next_seq(&root).unwrap(), SEQ_INIT + 1);
    std::fs::remove_dir_all(&root).ok();
}
