//! Coordination rooms (design.md §17): `room compact` — the
//! byte-weighted plan, the commit that archives originals and mints
//! an in-stream digest, the meta ratchet, and the range/digest read
//! views that span live + archive.

mod common;

use common::room::*;

/// `room compact` plan picks the byte-weighted split — a fat first
/// message pulls the cut earlier than a count-based 80% would —
/// and is a pure audit read: no archive, no mark, no mutation.
#[test]
fn compact_plan_splits_by_bytes_and_mutates_nothing() {
    let home = tmpdir("cplan");
    let env = [("SHUNT_ROOM_COMPACT_MIN", "4")];
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    // 50+10+10+10 = 80 bytes; ratio .8 targets 64 → cum 50/60/70/80,
    // |60-64| smallest → split after message 2. Count-based would
    // land on message 4.
    for b in ["x".repeat(50), "x".repeat(10), "x".repeat(10), "x".repeat(10)] {
        ok(&home, &["room", "say", "r", "--body", &b, "--as", "p1"]);
    }
    let o = run(&home, &["room", "compact", "r", "--as", "p2", "--json"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["through"], 2, "{v}");
    assert_eq!(v["msg_count"], 2);
    assert_eq!(v["total_bytes"], 60);
    let msgs_v = v["messages"].as_array().unwrap();
    assert_eq!(msgs_v.len(), 2);
    assert_eq!(msgs_v[0]["n"], 1);
    assert_eq!(msgs_v[1]["n"], 2);
    assert!(msgs_v[0]["summary_through"].is_null(), "packet msgs carry the field for agent detection: {v}");

    // A smaller ratio cuts earlier.
    let o = run(&home, &["room", "compact", "r", "--as", "p2", "--ratio", "0.4", "--json"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["through"], 1, "{v}");
    assert_eq!(v["msg_count"], 1);

    // Pure audit read: no .archive, no cursor for p2, no new msgs.
    assert!(!home.join("rooms/r/msgs/.archive").exists(), "plan must not archive");
    assert!(!home.join("rooms/r/readers/p2.json").exists(), "plan must not mark");
    assert_eq!(msgs(&home, "r").len(), 4);
    std::fs::remove_dir_all(&home).ok();
}

/// Below compact_min the plan refuses and names the manual escape;
/// a forced --through still lands under the floor.
#[test]
fn compact_plan_refuses_below_floor() {
    let home = tmpdir("cfloor");
    let env = [("SHUNT_ROOM_COMPACT_MIN", "5")];
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    for i in 1..=3 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    let o = run(&home, &["room", "compact", "r"], &env);
    assert!(!o.status.success(), "plan under the floor must refuse: {}", String::from_utf8_lossy(&o.stdout));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("non-summary live messages") && e.contains("--through"), "{e}");
    // The forced commit is the documented escape.
    let o = run(&home, &["room", "compact", "r", "--through", "2", "--body", "d", "--as", "p1"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    std::fs::remove_dir_all(&home).ok();
}

/// Commit archives the covered originals, mints the digest
/// in-stream, lands the meta ratchet, and marks the caller — while
/// staying one ordinary message to `read --all`.
#[test]
fn compact_commit_archives_mints_ratchet_marks() {
    let home = tmpdir("ccommit");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    // Rewind p2's cursor so the covered-range warning names it.
    ok(&home, &["room", "read", "r", "--as", "p2"]);
    std::fs::write(home.join("rooms/r/readers/p2.json"), "{\"last_read_n\":1}").unwrap();

    let o = run(&home, &["room", "compact", "r", "--through", "3", "--body", "digest of 1-3", "--as", "p1", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("covered range: p2"), "the warning names the behind reader: {e}");
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["through"], 3, "{v}");
    assert_eq!(v["summary_n"], 6);
    assert_eq!(v["member"], true);

    assert_eq!(msgs(&home, "r"), vec!["0004.json", "0005.json", "0006.json"]);
    assert_eq!(archived(&home, "r"), vec!["0001.json", "0002.json", "0003.json"]);
    let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/msgs/0006.json")).unwrap()).unwrap();
    assert_eq!(s["summary_through"], 3);
    assert_eq!(s["body_md"], "digest of 1-3");
    assert_eq!(s["member"], true);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["compacted_through"], 3);
    // Commit marks the caller through the live max, like say.
    assert_eq!(last_read(&home, "r", "p1"), 6);

    // The digest is an ordinary stream entry — `read --all` shows it
    // in place, tagged in human output.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--all"]);
    assert!(out.contains("(summary→0003)") && out.contains("digest of 1-3"), "{out}");
    std::fs::remove_dir_all(&home).ok();
}

/// A --through at or below the ratchet is the one coverage refusal;
/// a retry against a digest that committed but never landed meta
/// returns its n — no duplicate mint, first body retained.
#[test]
fn compact_rejects_regression_and_recommits_idempotently() {
    let home = tmpdir("cidem");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "first digest", "--as", "p1"]);
    for t in ["2", "3"] {
        let e = err(&home, &["room", "compact", "r", "--through", t, "--body", "x", "--as", "p1"]);
        assert!(e.contains("already covered"), "{e}");
    }
    // Simulate the post-write failure the idempotency check exists
    // for: the digest is live but the ratchet never landed.
    let meta_path = home.join("rooms/r/meta.json");
    let mut meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    meta.as_object_mut().unwrap().remove("compacted_through");
    std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
    let o = run(&home, &["room", "compact", "r", "--through", "3", "--body", "rewritten digest", "--as", "p1", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()["summary_n"], 6);
    assert_eq!(msgs(&home, "r"), vec!["0004.json", "0005.json", "0006.json"], "no duplicate mint");
    assert_eq!(archived(&home, "r"), vec!["0001.json", "0002.json", "0003.json"], "no double move");
    let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/msgs/0006.json")).unwrap()).unwrap();
    assert_eq!(s["body_md"], "first digest", "the committed digest survives the retry");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["compacted_through"], 3, "the retry lands the ratchet");
    std::fs::remove_dir_all(&home).ok();
}

/// Prior digests are exempt from the compact move — only originals
/// ≤N archive; a second compact ratchets forward.
#[test]
fn compact_move_exempts_prior_summaries() {
    let home = tmpdir("cexempt");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "d1", "--as", "p1"]);
    for i in 7..=9 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    // through=9 covers the live head: warning (b) fires but the
    // commit still lands.
    let o = run(&home, &["room", "compact", "r", "--through", "9", "--body", "d2", "--as", "p1", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("newest live message"));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()["summary_n"], 10);
    // Covered originals 4,5,7,8,9 archived; the old digest stays.
    assert_eq!(msgs(&home, "r"), vec!["0006.json", "0010.json"]);
    assert_eq!(archived(&home, "r"), vec!["0001.json", "0002.json", "0003.json", "0004.json", "0005.json", "0007.json", "0008.json", "0009.json"]);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["compacted_through"], 9);
    std::fs::remove_dir_all(&home).ok();
}

/// A non-member commit warns like say (open trust, member:false);
/// through ≥ the live head warns; behind readers are named. None of
/// it gates.
#[test]
fn compact_commit_warns_without_gating() {
    let home = tmpdir("cwarn");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=3 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "read", "r", "--as", "p2"]);
    std::fs::write(home.join("rooms/r/readers/p2.json"), "{\"last_read_n\":1}").unwrap();
    let o = run(&home, &["room", "compact", "r", "--through", "3", "--body", "d", "--as", "intruder", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("not a member") && e.contains("roster: p1, p2"), "{e}");
    assert!(e.contains("newest live message"), "{e}");
    assert!(e.contains("covered range: p2 —"), "only the behind reader is named: {e}");
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["member"], false);
    assert_eq!(msgs(&home, "r"), vec!["0004.json"]);
    std::fs::remove_dir_all(&home).ok();
}

/// `--from/--to` reads span live + archive (inclusive, either bound
/// open) and never mark — a pure audit read.
#[test]
fn read_range_spans_archive_and_never_marks() {
    let home = tmpdir("crange");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "digest", "--as", "p1"]);

    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--from", "1", "--to", "4", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ns: Vec<u64> = v["messages"].as_array().unwrap().iter().map(|m| m["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![1, 2, 3, 4], "archived 1-3 + live 4: {out}");
    assert!(!home.join("rooms/r/readers/p2.json").exists(), "range read must not mark");

    // Either bound may be omitted (open range).
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--from", "5", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ns: Vec<u64> = v["messages"].as_array().unwrap().iter().map(|m| m["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![5, 6], "{out}");
    assert_eq!(v["messages"][1]["summary_through"], 3);
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--to", "2"]);
    assert!(out.contains("m1") && out.contains("m2") && !out.contains("m3"), "{out}");

    // Human output tags digest headers; still no marking.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--from", "6"]);
    assert!(out.contains("(summary→0003)"), "{out}");
    assert!(!home.join("rooms/r/readers/p2.json").exists());
    std::fs::remove_dir_all(&home).ok();
}

/// `read --digest` is the compacted catch-up: the newest summary
/// plus the raw live tail past its coverage, marked like --all.
/// With no compaction yet it notes so and returns the full stream.
#[test]
fn read_digest_returns_summary_and_tail() {
    let home = tmpdir("cdigest");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    // No compaction yet: a stderr note, the full live stream.
    let o = run(&home, &["room", "read", "r", "--as", "p2", "--digest"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("no compaction yet"));
    assert!(String::from_utf8_lossy(&o.stdout).contains("m1"));
    assert_eq!(last_read(&home, "r", "p2"), 5);

    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "digest of 1-3", "--as", "p1"]);
    ok(&home, &["room", "say", "r", "--body", "tail", "--as", "p1"]);
    // Live is 4,5,6(sum→3),7 — the digest plus every n > 3.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--digest", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ns: Vec<u64> = v["messages"].as_array().unwrap().iter().map(|m| m["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![4, 5, 6, 7], "{out}");
    assert_eq!(v["messages"][2]["summary_through"], 3);
    assert_eq!(v["messages"][2]["body_md"], "digest of 1-3");
    assert_eq!(last_read(&home, "r", "p2"), 7, "digest marks through the returned max");
    std::fs::remove_dir_all(&home).ok();
}

/// An unread read surfaces the digest like any other message — the
/// originals are gone from live, the summary arrives as catch-up.
#[test]
fn unread_read_surfaces_the_summary() {
    let home = tmpdir("cunread");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "read", "r", "--as", "p2"]);
    assert_eq!(last_read(&home, "r", "p2"), 5);
    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "d", "--as", "p1"]);
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let msgs_v = v["messages"].as_array().unwrap();
    assert_eq!(msgs_v.len(), 1, "{out}");
    assert_eq!(msgs_v[0]["n"], 6);
    assert_eq!(msgs_v[0]["summary_through"], 3);
    assert_eq!(last_read(&home, "r", "p2"), 6);
    std::fs::remove_dir_all(&home).ok();
}

/// Summaries are exempt only from the compact MOVE — the live cap
/// still drains them like any message, and `read --digest` then
/// falls back to the archive for the newest one.
#[test]
fn cap_drain_takes_summaries_and_digest_falls_back() {
    let home = tmpdir("ccap");
    let env = [("SHUNT_ROOM_KEEP", "3")];
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=3 {
        let o = run(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"], &env);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    ok(&home, &["room", "compact", "r", "--through", "2", "--body", "d", "--as", "p1"]); // live = 3, 4(sum→2)
    for i in 5..=7 {
        let o = run(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"], &env);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    // Cap=3 drained 3 then the summary 4 into .archive.
    assert_eq!(msgs(&home, "r"), vec!["0005.json", "0006.json", "0007.json"]);
    assert!(archived(&home, "r").contains(&"0004.json".to_string()), "the cap drains summaries too: {:?}", archived(&home, "r"));
    // Digest view: no live summary → the archived 0004 heads it,
    // then the raw tail n > 2.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--digest", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ns: Vec<u64> = v["messages"].as_array().unwrap().iter().map(|m| m["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![4, 5, 6, 7], "{out}");
    assert_eq!(v["messages"][0]["summary_through"], 2);
    std::fs::remove_dir_all(&home).ok();
}

/// The phase-dispatch validation: --through with no body source is
/// the commit error; a body source without --through names the
/// plan step; --ratio out of range refuses.
#[test]
fn compact_arg_validation() {
    let home = tmpdir("cargs");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    ok(&home, &["room", "say", "r", "--body", "m1", "--as", "p1"]);
    let e = err(&home, &["room", "compact", "r", "--through", "1"]);
    assert!(e.contains("digest body"), "{e}");
    let e = err(&home, &["room", "compact", "r", "--body", "d"]);
    assert!(e.contains("--through") && e.contains("split point"), "{e}");
    let e = err(&home, &["room", "compact", "r", "--ratio", "1.5"]);
    assert!(e.contains("between 0 and 1"), "{e}");
    let e = err(&home, &["room", "compact", "r", "--through", "0", "--body", "d"]);
    assert!(e.contains("--through 0"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}
