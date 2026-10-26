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

/// The wraparound floor spans only live tracks (§3.2): archived
/// legacy NNNN names are inert, while a live terminal entry still
/// prevents a collision at claim/finalize time.
#[test]
fn next_seq_wraparound_ignores_archives() {
    let root = tmp_root("hiwater");
    std::fs::create_dir_all(root.join("done/9500_old")).unwrap();
    std::fs::create_dir_all(root.join("failed/.archive/9999_ancient")).unwrap();
    std::fs::write(root.join(".seq"), format!("{SEQ_MAX}\n")).unwrap();
    assert_eq!(next_seq(&root).unwrap(), 9501);
    // True exhaustion still applies when 9999 remains on a live track.
    std::fs::create_dir_all(root.join("done/9999_last")).unwrap();
    assert!(next_seq(&root).is_err());
    std::fs::remove_dir_all(&root).ok();
}

/// Archiving releases the original NNNN prefix and leaves the data
/// visible only under an underscore-prefixed cold-storage name.
#[test]
fn archive_renames_out_of_sequence_namespace() {
    let root = tmp_root("archivename");
    for name in ["0100_old", "0101_new"] {
        std::fs::create_dir_all(root.join("done").join(name)).unwrap();
    }
    assert_eq!(archive_oldest(&root, "done", 1), 1);
    let archived: Vec<_> = std::fs::read_dir(root.join("done/.archive")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(archived.len(), 1);
    assert!(archived[0].starts_with('_'), "archive name retained a numeric prefix: {}", archived[0]);
    assert!(archived[0].ends_with("_0100_old"));
    assert_eq!(list_entries(&root, "done").unwrap(), vec!["0101_new".to_string()]);
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
    assert_eq!(pending_list(&root).unwrap(), vec![name.clone()]);
    assert_eq!(std::fs::read_to_string(root.join(".dedup")).unwrap(), format!("abc-123\t{name}\n"));
    // Removal makes the latest index row stale; a re-push is fresh,
    // then that newer row becomes the dedup target.
    remove_pending(&root, &name).unwrap();
    let replacement = match enqueue(&root, "t-abc123", body).unwrap() {
        EnqueueOutcome::Fresh(n) => n,
        EnqueueOutcome::Deduped(n) => panic!("removed entry {n} remained a dedup target"),
    };
    assert_ne!(replacement, name);
    match enqueue(&root, "t-abc123", body).unwrap() {
        EnqueueOutcome::Deduped(n) => assert_eq!(n, replacement),
        EnqueueOutcome::Fresh(n) => panic!("re-push minted {n} after re-mint"),
    }
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

/// Pre-index queues bootstrap from live task heads, including the
/// legacy flat-file layout, before subsequent lookups use `.dedup`.
#[test]
fn enqueue_bootstraps_legacy_flat_entry() {
    let root = tmp_root("dedupbootstrap");
    let name = "0090_legacy";
    std::fs::write(root.join(format!("done/{name}.sh")), "#!/usr/bin/env bash\n# shunt:id=legacy-9\nexec true\n").unwrap();
    let body = "#!/usr/bin/env bash\n# shunt:id=legacy-9\nexec true\n";
    match enqueue(&root, "legacy9", body).unwrap() {
        EnqueueOutcome::Deduped(n) => assert_eq!(n, name),
        EnqueueOutcome::Fresh(n) => panic!("legacy flat entry missed bootstrap: {n}"),
    }
    assert_eq!(std::fs::read_to_string(root.join(".dedup")).unwrap(), format!("legacy-9\t{name}\n"));
    assert_eq!(std::fs::read_to_string(root.join(".seq")).unwrap(), "99\n");
    std::fs::remove_dir_all(&root).ok();
}

/// A stale last index row is a miss rather than a phantom dedup hit.
#[test]
fn enqueue_stale_dedup_row_is_miss() {
    let root = tmp_root("dedupstale");
    std::fs::write(root.join(".dedup"), "gone-1\t0100_removed\n").unwrap();
    let out = enqueue(&root, "gone1", "#!/usr/bin/env bash\n# shunt:id=gone-1\nexec true\n").unwrap();
    assert!(matches!(out, EnqueueOutcome::Fresh(_)));
    assert_eq!(pending_list(&root).unwrap().len(), 1);
    std::fs::remove_dir_all(&root).ok();
}

/// Compaction runs at the 64 KiB growth boundary, dropping stale rows
/// while retaining every row that still names a live entry.
#[test]
fn enqueue_compacts_dedup_index_at_threshold() {
    use super::enqueue::{dedup_compact_bytes, DEDUP_COMPACT_BYTES_OVERRIDE};

    struct ResetThreshold;
    impl Drop for ResetThreshold {
        fn drop(&mut self) {
            DEDUP_COMPACT_BYTES_OVERRIDE.with(|threshold| threshold.set(None));
        }
    }

    DEDUP_COMPACT_BYTES_OVERRIDE.with(|threshold| threshold.set(Some(512)));
    let _reset = ResetThreshold;
    let threshold = dedup_compact_bytes() as usize;
    let root = tmp_root("dedupcompact");
    std::fs::create_dir_all(root.join("pending/0100_kept")).unwrap();
    std::fs::write(root.join("pending/0100_kept/task.sh"), "#!/usr/bin/env bash\n# shunt:id=kept-1\nexec true\n").unwrap();
    let append = "fresh-1\t0101_fresh\n";
    let stale = "stale-1\t9999_gone\n";
    let mut seeded = String::from("kept-1\t0100_kept\n");
    while seeded.len() + stale.len() + append.len() <= threshold {
        seeded.push_str(stale);
    }
    let gap = threshold - append.len() - seeded.len();
    if gap > 0 {
        seeded.push_str(&"x".repeat(gap - 1));
        seeded.push('\n');
    }
    assert_eq!(seeded.len() + append.len(), threshold);
    std::fs::write(root.join(".dedup"), seeded).unwrap();

    match enqueue(&root, "fresh", "#!/usr/bin/env bash\n# shunt:id=fresh-1\nexec true\n").unwrap() {
        EnqueueOutcome::Fresh(n) => assert_eq!(n, "0101_fresh"),
        EnqueueOutcome::Deduped(n) => panic!("unexpected hit: {n}"),
    }
    let compacted = std::fs::read_to_string(root.join(".dedup")).unwrap();
    assert_eq!(compacted, "kept-1\t0100_kept\nfresh-1\t0101_fresh\n");
    assert!(compacted.len() < threshold);
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

/// §3.9b: a clean sweep removes only the selected sets — pending
/// and archive here — while running/, the live terminal listings,
/// and every sidecar story stay untouched.
#[test]
fn clean_selected_sets_only() {
    let root = tmp_root("cleansets");
    for n in ["0100_a", "0101_b"] {
        std::fs::create_dir_all(root.join(format!("pending/{n}"))).unwrap();
        std::fs::write(root.join(format!("pending/{n}/task.sh")), "echo hi\n").unwrap();
    }
    std::fs::write(root.join("pending/0102_c.sh"), "echo c\n").unwrap();
    std::fs::write(root.join("pending/0102_c.sh.retry_after"), "9999999999\n").unwrap();
    std::fs::create_dir_all(root.join("running/0103_r")).unwrap();
    std::fs::create_dir_all(root.join("done/0090_d")).unwrap();
    std::fs::create_dir_all(root.join("failed/0091_f")).unwrap();
    std::fs::create_dir_all(root.join("done/.archive/0001_anc")).unwrap();
    std::fs::create_dir_all(root.join("done/.archive/0002_anc")).unwrap();
    std::fs::create_dir_all(root.join("failed/.archive/0003_anc")).unwrap();
    std::fs::write(root.join(".dedup"), "kept\t0100_a\n").unwrap();
    init(&root).unwrap();

    let r = clean(&root, &CleanSets { pending: true, archive: true, ..Default::default() });
    assert_eq!((r.pending, r.archive, r.done, r.failed, r.skipped), (3, 3, 0, 0, 0));
    assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
    assert!(pending_list(&root).unwrap().is_empty());
    // Flat entry's sidecars went with it — no `.sh.*` residue.
    let leftovers: Vec<_> = std::fs::read_dir(root.join("pending")).unwrap().flatten().collect();
    assert!(leftovers.is_empty());
    assert_eq!(running_list(&root).unwrap(), vec!["0103_r".to_string()]);
    assert_eq!(list_entries(&root, "done").unwrap(), vec!["0090_d".to_string()]);
    assert_eq!(list_entries(&root, "failed").unwrap(), vec!["0091_f".to_string()]);
    assert!(list_entries(&root, "done/.archive").unwrap().is_empty());
    assert!(list_entries(&root, "failed/.archive").unwrap().is_empty());
    assert_eq!(std::fs::read_to_string(root.join(".dedup")).unwrap(), "kept\t0100_a\n");
    // The run's own trash dir must not survive.
    let stray: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".clean-")).collect();
    assert!(stray.is_empty());
    std::fs::remove_dir_all(&root).ok();
}

/// `--all` clears every non-running set; running/ is never a
/// clean target — it answers to `kill` (§3.9a), not janitorial.
#[test]
fn clean_all_leaves_running_untouched() {
    let root = tmp_root("cleanall");
    std::fs::create_dir_all(root.join("pending/0100_a")).unwrap();
    std::fs::create_dir_all(root.join("running/0101_r")).unwrap();
    std::fs::write(root.join("done/0102_d.sh"), "echo d\n").unwrap();
    std::fs::write(root.join("done/0102_d.sh.log"), "log\n").unwrap();
    std::fs::write(root.join("done/0102_d.sh.exit"), "0\n").unwrap();
    std::fs::create_dir_all(root.join("failed/.archive/0103_f")).unwrap();

    let r = clean(&root, &CleanSets { pending: true, done: true, failed: true, archive: true });
    assert_eq!((r.pending, r.done, r.failed, r.archive), (1, 1, 0, 1));
    assert_eq!(running_list(&root).unwrap(), vec!["0101_r".to_string()]);
    assert!(pending_list(&root).unwrap().is_empty());
    assert!(list_entries(&root, "done").unwrap().is_empty());
    let leftovers: Vec<_> = std::fs::read_dir(root.join("done")).unwrap().flatten().filter(|e| e.file_name() != ".archive").collect();
    assert!(leftovers.is_empty());
    std::fs::remove_dir_all(&root).ok();
}

/// Stale `.clean-*` trash (dead owner pid, unparsable name, or
/// residue of this same pid's earlier run) is swept before the
/// selected sets — it holds only doomed entries.
#[test]
fn clean_sweeps_stale_trash_dirs() {
    let root = tmp_root("cleantrash");
    std::fs::create_dir_all(root.join(".clean-notapid/done/0001_x")).unwrap();
    std::fs::create_dir_all(root.join(format!(".clean-{}/pending/0002_y", std::process::id()))).unwrap();
    std::fs::create_dir_all(root.join("pending/0100_a")).unwrap();
    let r = clean(&root, &CleanSets { pending: true, ..Default::default() });
    assert_eq!(r.pending, 1);
    assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
    let stray: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(".clean-")).collect();
    assert!(stray.is_empty(), "stale trash survived: {stray:?}");
    std::fs::remove_dir_all(&root).ok();
}

/// An entry claimed by the daemon mid-listing vanishes before our
/// rename lands: ENOENT maps to `skipped`, never an error and
/// never a half-deleted running/ entry.
#[test]
fn clean_lost_claim_race_counts_skipped() {
    let root = tmp_root("cleanrace");
    std::fs::create_dir_all(root.join("pending/0100_a")).unwrap();
    // The daemon's claim wins before clean() lists: equivalent to
    // losing the rename race — the name is simply gone.
    assert!(claim(&root, "0100_a").unwrap());
    let r = clean(&root, &CleanSets { pending: true, ..Default::default() });
    assert_eq!((r.pending, r.skipped, r.errors.len()), (0, 0, 0));
    assert_eq!(running_list(&root).unwrap(), vec!["0100_a".to_string()]);
    std::fs::remove_dir_all(&root).ok();
}
