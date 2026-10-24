use super::logs::{render_dlog_line, tail_bytes};
use super::{agent_cancel, agent_kill, agent_prio, logs_wire_tail, workers_wire_tail};
use crate::cli::args::{LogsArgs, PatternArgs, PrioArgs, WorkerAction, WorkersArgs};

/// Both the one-shot `dlog` render and `logs -f`'s appended-
/// line render must go through the same JSONL→text transform
/// so a followed daemon log reads `[ts] event k=v`, not raw JSON.
#[test]
fn render_dlog_line_formats_jsonl() {
    let out = render_dlog_line(r#"{"ts":"12:00","event":"claim","task":"0001_a","pid":7}"#);
    // serde_json::Map is BTreeMap: k=v pairs sort by key.
    assert_eq!(out, "[12:00] claim pid=7 task=\"0001_a\"");
    assert_eq!(render_dlog_line("not json"), "not json");
}

#[test]
fn tail_bytes_keeps_last_n_lines() {
    assert_eq!(tail_bytes(b"a\nb\nc\n", 2), b"b\nc\n");
    assert_eq!(tail_bytes(b"a\nb\nc\n", 5), b"a\nb\nc\n");
    assert_eq!(tail_bytes(b"a\nb\nc", 1), b"c");
    assert_eq!(tail_bytes(b"", 3), b"");
    assert_eq!(tail_bytes(b"one\n", 1), b"one\n");
    // L10 regression: -n 0 must print nothing, not the whole file.
    assert_eq!(tail_bytes(b"a\nb\nc\n", 0), b"");
    assert_eq!(tail_bytes(b"", 0), b"");
}

#[test]
fn logs_wire_tail_forwards_tail_and_pattern() {
    let a = LogsArgs { pattern: Some("train".into()), client: None, queue: None, root: None, follow: false, tail: Some(50) };
    assert_eq!(logs_wire_tail(&a), vec!["logs", "-n", "50", "--", "train"]);
    // Bare `logs` must not send an empty positional — that would
    // match every task log instead of the daemon log.
    let bare = LogsArgs { pattern: None, ..a };
    assert_eq!(logs_wire_tail(&bare), vec!["logs", "-n", "50"]);
}

/// `set 5` spelled via the subcommand must cross the wire as
/// `workers set 5`, not `workers get` (which would report the
/// old value as a silent no-op).
#[test]
fn workers_wire_tail_honors_set_subcommand() {
    let wa = |n, action| WorkersArgs { n, action, client: None, queue: None, root: None, json: false };
    assert_eq!(workers_wire_tail(&wa(None, Some(WorkerAction::Set { n: 5 }))), vec!["workers", "set", "5"]);
    // Positional `N` still sets; `get` and bare both fetch.
    assert_eq!(workers_wire_tail(&wa(Some(3), None)), vec!["workers", "set", "3"]);
    assert_eq!(workers_wire_tail(&wa(None, Some(WorkerAction::Get))), vec!["workers", "get"]);
    assert_eq!(workers_wire_tail(&wa(None, None)), vec!["workers", "get"]);
}

fn pat_args(root: &std::path::Path, pattern: &str) -> PatternArgs {
    PatternArgs { pattern: pattern.into(), client: None, queue: None, root: Some(root.display().to_string()), json: false }
}

fn tmp_root(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("shunt-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()))
}

/// A pending miss must name the pattern's hits in the other tracks
/// and point at the command that reaches them — a task claimed
/// between submit and `cancel` reads as "no pending task matches"
/// for every spelling of its name otherwise (issue #15).
#[test]
fn cancel_miss_names_other_track_hits() {
    let root = tmp_root("cancel-miss");
    for t in ["pending", "running", "done", "failed"] {
        std::fs::create_dir_all(root.join(t)).unwrap();
    }
    std::fs::create_dir_all(root.join("running").join("0107_c10_x")).unwrap();
    std::fs::create_dir_all(root.join("failed").join("0108_c10_y")).unwrap();
    let err = agent_cancel(&pat_args(&root, "c10")).unwrap_err().to_string();
    assert!(err.contains("no pending task matches 'c10'"), "{err}");
    assert!(err.contains("running: 0107_c10_x"), "{err}");
    assert!(err.contains("`shunt kill`"), "{err}");
    assert!(err.contains("failed: 0108_c10_y"), "{err}");
    std::fs::remove_dir_all(&root).ok();

    // A hit nowhere stays a plain miss — no empty hint appended.
    let err = agent_cancel(&pat_args(&root, "zzz")).unwrap_err().to_string();
    assert_eq!(err, "no pending task matches 'zzz'", "{err}");
    std::fs::remove_dir_all(&root).ok();
}

/// A real pending hit still cancels — the hint path must not
/// disturb the one-match fast path.
#[test]
fn cancel_matches_pending_substring() {
    let root = tmp_root("cancel-hit");
    let pending = root.join("pending");
    std::fs::create_dir_all(pending.join("0107_c10_x")).unwrap();
    std::fs::write(pending.join("0107_c10_x").join("task.sh"), "echo hi\n").unwrap();
    agent_cancel(&pat_args(&root, "0107")).unwrap();
    assert!(!pending.join("0107_c10_x").exists());
    std::fs::remove_dir_all(&root).ok();
}

/// `kill`'s own list is pid-gated: a miss must steer to `cancel`
/// on a pending hit and name a claimed-but-pid-less entry plainly.
#[test]
fn kill_miss_names_pending_and_pidless_hits() {
    let root = tmp_root("kill-miss");
    for t in ["pending", "running"] {
        std::fs::create_dir_all(root.join(t)).unwrap();
    }
    std::fs::create_dir_all(root.join("pending").join("0107_c10_p")).unwrap();
    std::fs::create_dir_all(root.join("running").join("0108_c10_r")).unwrap();
    let err = agent_kill(&pat_args(&root, "c10")).unwrap_err().to_string();
    assert!(err.contains("no running task matches 'c10'"), "{err}");
    assert!(err.contains("pending: 0107_c10_p"), "{err}");
    assert!(err.contains("`shunt cancel`"), "{err}");
    assert!(err.contains("claimed, no pid yet: 0108_c10_r"), "{err}");
    std::fs::remove_dir_all(&root).ok();
}

/// prio must refuse a flat entry renaming onto a same-named
/// pending DIR (and a dir onto a flat `.sh`): either coexistence
/// would list as two pending tasks (design.md §3.14).
#[test]
fn prio_refuses_cross_shape_target() {
    let root = std::env::temp_dir().join(format!("shunt-prio-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    let pending = root.join("pending");
    std::fs::create_dir_all(&pending).unwrap();
    // Flat source 0001_a.sh + dir target 0002_a/.
    std::fs::write(pending.join("0001_a.sh"), "echo hi\n").unwrap();
    std::fs::create_dir_all(pending.join("0002_a")).unwrap();
    let args = PrioArgs { pattern: "0001_a".into(), n: 2, client: None, queue: None, root: Some(root.display().to_string()), json: false };
    let err = agent_prio(&args).unwrap_err().to_string();
    assert!(err.contains("target name exists"), "flat->dir not refused: {err}");
    assert!(pending.join("0001_a.sh").exists() && pending.join("0002_a").exists());
    std::fs::remove_dir_all(&root).ok();

    // Dir source 0001_b/ + flat target 0002_b.sh.
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::create_dir_all(pending.join("0001_b")).unwrap();
    std::fs::write(pending.join("0002_b.sh"), "echo hi\n").unwrap();
    let args = PrioArgs { pattern: "0001_b".into(), n: 2, client: None, queue: None, root: Some(root.display().to_string()), json: false };
    let err = agent_prio(&args).unwrap_err().to_string();
    assert!(err.contains("target name exists"), "dir->flat not refused: {err}");
    std::fs::remove_dir_all(&root).ok();
}
