//! Queue-entry CLI operations: enqueue numbering, cancel, prio,
//! seq wrap, slug rejection, status/mktask wire shapes.

mod common;

use common::{bin, marker_task, sleep_task, wait_until, Q};
use std::process::{Command, Stdio};

#[test]
fn dispatch_order_priority_band_first() {
    let q = Q::new("order");
    std::fs::write(q.root.join("workers"), "1\n").unwrap();
    let order = q.root.join("order.txt");
    let body = |tag: &str| format!("#!/usr/bin/env bash\necho {tag} >> {}\n", order.display());
    // normal band entries
    q.enqueue_script("n1", &body("n1"));
    q.enqueue_script("n2", &body("n2"));
    // priority-band entry installed directly (0000-0099 runs first)
    std::fs::write(q.root.join("pending/0005_p0.sh"), body("p0")).unwrap();
    let mut d = q.daemon();
    wait_until(|| std::fs::read_to_string(&order).map(|s| s.lines().count() == 3).unwrap_or(false), 15, "all three tasks to run");
    let lines: Vec<String> = std::fs::read_to_string(&order).unwrap().lines().map(|s| s.to_string()).collect();
    assert_eq!(lines, ["p0", "n1", "n2"]);
    q.stop();
    let _ = d.wait();
}

#[test]
fn enqueue_numbering_and_mode() {
    let q = Q::new("seq");
    for s in ["a", "b", "c"] {
        q.enqueue_script(s, &marker_task(&q.root.join(s)));
    }
    let names = q.names("pending");
    assert_eq!(names.len(), 3);
    assert!(names[0].starts_with("0100_"), "got {names:?}");
    assert!(names[2].starts_with("0102_"), "got {names:?}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(q.root.join("pending").join(&names[0]).join("task.sh")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    // stdin wire form with --json ack
    let o = Command::new(bin())
        .args(["enqueue", "-", "wireslug", "--json", "--root"])
        .arg(&q.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.as_mut().unwrap().write_all(b"#!/usr/bin/env bash\nexit 0\n")?;
            c.wait_with_output()
        })
        .unwrap();
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("ack is JSON");
    assert_eq!(v["protocol"], 1);
    assert_eq!(v["ok"], true);
    assert!(v["name"].as_str().unwrap().contains("wireslug"));
}

#[test]
fn cancel_zero_one_many() {
    let q = Q::new("cancel");
    q.enqueue_script("alpha", &sleep_task());
    q.enqueue_script("beta", &sleep_task());
    q.run_err(&["cancel", "nomatch"]);
    // "a" matches both alpha and beta
    q.run_err(&["cancel", "a_"]);
    let out = q.run_ok(&["cancel", "alpha"]);
    assert!(out.contains("cancelled:"), "got {out}");
    assert_eq!(q.names("pending").len(), 1);
}

#[test]
fn prio_renumber_and_refuse_existing() {
    let q = Q::new("prio");
    q.enqueue_script("one", &sleep_task());
    q.enqueue_script("two", &sleep_task());
    let out = q.run_ok(&["prio", "one", "5"]);
    assert!(out.contains("0005_one"), "got {out}");
    // refuse to renumber onto an existing name
    std::fs::write(q.root.join("pending/0005_two.sh"), sleep_task()).unwrap();
    let err = q.run_err(&["prio", "0101", "5"]);
    assert!(err.contains("exists"), "got {err}");
    assert_eq!(q.tasks("pending").len(), 3, "task must not be clobbered");
}

#[test]
fn prio_rename_keeps_delivery_dedup_valid() {
    let q = Q::new("prio-dedup");
    let body = "#!/usr/bin/env bash\n# shunt:id=prio-delivery\nexec true\n";
    let original = q.enqueue_script("delivery", body);
    let renamed = format!("0005_{}", original.split_once('_').unwrap().1);
    q.run_ok(&["prio", &original, "5"]);
    assert!(!q.root.join(".dedup").exists(), "name-changing prio kept a stale index");

    let source = q.root.join("task-delivery.sh.src");
    q.run_ok(&["enqueue", source.to_str().unwrap(), "delivery"]);
    assert_eq!(q.tasks("pending"), vec![renamed.clone()]);
    assert_eq!(std::fs::read_to_string(q.root.join(".dedup")).unwrap(), format!("prio-delivery\t{renamed}\n"));
}

#[test]
fn status_json_shape() {
    let q = Q::new("status");
    q.enqueue_script("s1", &sleep_task());
    let o = Command::new(bin()).args(["status", "--json", "--root"]).arg(&q.root).output().unwrap();
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["protocol"], 1);
    assert_eq!(v["daemon"]["alive"], false);
    assert_eq!(v["workers"]["value"], 3);
    assert_eq!(v["workers"]["source"], "file");
    assert_eq!(v["pending"].as_array().unwrap().len(), 1);
    assert_eq!(v["stop"], false);
    assert_eq!(v["shutdown"]["armed"], false);
}

#[test]
fn mktask_and_new() {
    let q = Q::new("mktask");
    let out = q.run_ok(&["mktask", "--slug", "gen", "--guard", "runs/x.json", "--", "echo", "a b"]);
    assert!(out.contains("set -uo pipefail"), "{out}");
    assert!(out.contains("[ -e runs/x.json ]"), "{out}");
    assert!(out.contains("exec echo 'a b'"), "{out}");
    assert!(!out.contains("\ncd "), "no cd line allowed: {out}");
    q.run_ok(&["new", "--slug", "gen", "--", "echo", "hi"]);
    assert_eq!(q.names("pending").len(), 1);
    let name = &q.names("pending")[0];
    let body = std::fs::read_to_string(q.root.join("pending").join(name).join("task.sh")).unwrap();
    assert!(body.contains("exec echo hi"));
}

#[test]
fn enqueue_concurrent_no_duplicate_seq() {
    let q = Q::new("conc");
    let body = marker_task(&q.root.join("m"));
    let f = q.root.join("task-conc.sh.src");
    std::fs::write(&f, &body).unwrap();
    let mut handles = Vec::new();
    for i in 0..8 {
        let root = q.root.clone();
        let f = f.clone();
        handles.push(std::thread::spawn(move || Command::new(bin()).args(["enqueue", f.to_str().unwrap(), &format!("c{i}"), "--root"]).arg(&root).output().unwrap()));
    }
    for h in handles {
        let o = h.join().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    let names = q.tasks("pending");
    assert_eq!(names.len(), 8);
    let mut seqs: Vec<String> = names.iter().map(|n| n.split('_').next().unwrap().to_string()).collect();
    seqs.sort();
    seqs.dedup();
    assert_eq!(seqs.len(), 8, "duplicate seq numbers: {names:?}");
}

#[test]
fn seq_wrap_stays_four_digits_and_fifo() {
    // design.md §3.2: the 4-digit namespace tops out at 9999. After
    // the counter passes it, allocation continues just past the
    // highest pending number so claim order stays FIFO — a 5-digit
    // `10000_x` would sort ahead of `9999_*` and leapfrog the queue.
    let q = Q::new("seqwrap");
    std::fs::write(q.root.join("pending/0500_held.sh"), sleep_task()).unwrap();
    std::fs::write(q.root.join(".seq"), "9999\n").unwrap();
    let name = q.enqueue_script("n1", &sleep_task());
    assert_eq!(name, "0501_n1", "wrapped seq must continue past the pending max, got {name}");
    // pending reaching 9999 leaves no name that sorts last: refuse.
    std::fs::write(q.root.join("pending/9999_top.sh"), sleep_task()).unwrap();
    let f = q.root.join("task-n2.sh.src");
    std::fs::write(&f, sleep_task()).unwrap();
    let err = q.run_err(&["enqueue", f.to_str().unwrap(), "n2"]);
    assert!(err.contains("sequence"), "got {err}");
    assert_eq!(q.tasks("pending").len(), 3, "failed enqueue must not install a task");
    assert!(q.names("pending").iter().all(|n| n.split('_').next().unwrap().len() == 4), "5-digit name leaked: {:?}", q.names("pending"));
}

#[test]
fn prio_rejects_outside_band() {
    // design.md §3.2: prio moves a task INTO 0000-0099; renumbering
    // into the normal sequence would collide with .seq allocation.
    let q = Q::new("prioband");
    q.enqueue_script("one", &sleep_task());
    let err = q.run_err(&["prio", "one", "123"]);
    assert!(err.contains("priority band"), "got {err}");
    let err = q.run_err(&["prio", "one", "100"]);
    assert!(err.contains("priority band"), "got {err}");
    // band edges still work
    let out = q.run_ok(&["prio", "one", "0"]);
    assert!(out.contains("0000_one"), "got {out}");
    let out = q.run_ok(&["prio", "one", "99"]);
    assert!(out.contains("0099_one"), "got {out}");
}

#[test]
fn done_track_archives_beyond_keep() {
    // Terminal tracks are capped: past SHUNT_TRACK_KEEP the oldest
    // entries move to done/.archive/ so status scans stay cheap
    // (design.md §3.9). Dot-prefixed archive is invisible to listings.
    let q = Q::new("archive");
    for i in 0..4 {
        q.enqueue_script(&format!("t{i}"), &marker_task(&q.root.join(format!("m{i}"))));
    }
    let mut c = Command::new(bin());
    let mut d = c.args(["daemon", "--root"]).arg(&q.root).env("POLL", "0.1").env("SHUNT_TRACK_KEEP", "2").env("SHUNT_STATE", q.root.join("boxstate")).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("spawn daemon");
    let live = |q: &Q| q.tasks("done").into_iter().filter(|n| !n.starts_with('.')).count();
    wait_until(|| (0..4).all(|i| q.root.join(format!("m{i}")).exists()), 15, "all four tasks ran");
    wait_until(|| q.names("done/.archive").len() == 2, 10, "oldest entries archived");
    assert_eq!(live(&q), 2, "done capped at keep: {:?}", q.tasks("done"));
    q.stop();
    let _ = d.wait();
}

#[test]
fn slug_leading_dash_rejected() {
    // A `-`-leading slug parses as a flag in the remote argv; entries
    // rejected there stay in spool retrying forever (design.md §3.3).
    let q = Q::new("dashslug");
    let f = q.root.join("task.sh.src");
    std::fs::write(&f, sleep_task()).unwrap();
    // Wire form (`--` before the positionals, as dispatch sends it):
    // the check must produce a clean nonzero exit, not a queued task.
    let o = Command::new(bin())
        .args(["enqueue", "--root"])
        .arg(&q.root)
        .args(["--json", "--", "-", "-rf"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.as_mut().unwrap().write_all(b"#!/usr/bin/env bash\nexit 0\n")?;
            c.wait_with_output()
        })
        .unwrap();
    assert!(!o.status.success(), "dash slug must not enqueue");
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("rejection is still wire JSON");
    assert_eq!(v["ok"], false);
    // mktask renders without a queue; --slug=-x reaches our check.
    let o = Command::new(bin()).args(["mktask", "--slug=-rf", "--root"]).arg(&q.root).args(["--", "echo", "hi"]).output().unwrap();
    assert!(!o.status.success(), "mktask must reject dash slug: {}", String::from_utf8_lossy(&o.stdout));
    // `new` shares it.
    let o = Command::new(bin()).args(["new", "--slug=-rf", "--root"]).arg(&q.root).args(["--", "echo", "hi"]).output().unwrap();
    assert!(!o.status.success(), "new must reject dash slug");
    assert!(q.names("pending").is_empty(), "rejected slugs must not enqueue: {:?}", q.names("pending"));
}
