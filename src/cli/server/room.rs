//! `shunt room` dispatch shell: body-source resolution and the
//! human render of room views (design.md §17).

use super::super::print_cols;
use anyhow::{Context as _, Result};

/// `shunt room` (design.md §17): all server-local, so dispatch is a
/// thin shell over crate::server::room — body-source resolution is
/// the only CLI concern here (the §14.1 precedence rule).
pub(crate) fn cmd_room(c: &crate::cli::args::RoomCmd) -> Result<()> {
    use crate::cli::args::RoomAction as A;
    use crate::server::room;
    match &c.action {
        A::Create { name, members, clients, json } => {
            room::create(name, members, clients)?;
            if *json {
                println!("{}", serde_json::json!({"ok": true, "name": name, "members": members, "clients": clients}));
            } else {
                println!("room '{name}' created — members: {}", members.join(", "));
            }
            Ok(())
        }
        A::Ls { as_label, all, json } => {
            let ls = room::list(as_label.as_deref(), *all)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "as": ls.label, "rooms": ls.rooms, "suggested": ls.suggested, "archived": ls.archived}))?);
            } else {
                print_room_ls(&ls, *all);
            }
            Ok(())
        }
        A::Say { room, body, body_file, source, as_label, json } => {
            let label = room_required_label(as_label.as_deref())?;
            let body = room_body("say", body.as_deref(), body_file.as_deref(), source.as_deref())?;
            let (n, member) = room::say(room, &label, &body)?;
            if *json {
                println!("{}", serde_json::json!({"ok": true, "room": room, "n": n, "member": member}));
            } else {
                println!("posted {n:04} to '{room}'{}", if member { "" } else { " (non-member)" });
            }
            Ok(())
        }
        A::Read { room, as_label, all, digest, from, to, json } => {
            let label = room_required_label(as_label.as_deref())?;
            let mode = if *all {
                room::ReadMode::All
            } else if *digest {
                room::ReadMode::Digest
            } else if from.is_some() || to.is_some() {
                room::ReadMode::Range { from: *from, to: *to }
            } else {
                room::ReadMode::Unread
            };
            let msgs = room::read(room, &label, mode)?;
            if *json {
                let msgs: Vec<_> = msgs.iter().map(|(n, m)| room_msg_json(*n, m)).collect();
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "room": room, "messages": msgs}))?);
            } else {
                print_room_msgs(&msgs);
                if msgs.is_empty() {
                    println!("(no new messages)");
                }
            }
            Ok(())
        }
        A::Compact { room, ratio, through, body, body_file, source, as_label, json } => {
            let label = room_required_label(as_label.as_deref())?;
            match through {
                // Plan phase: the digest's coverage is exactly what
                // the plan picked, so a body source here is a caller
                // that skipped a step — refuse with the remediation.
                None => {
                    anyhow::ensure!(body.is_none() && body_file.is_none() && source.is_none(), "a digest body needs --through N — run `room compact {room}` first to get the split point");
                    let plan = room::compact_plan(room, &label, ratio.unwrap_or(0.8))?;
                    if *json {
                        let msgs: Vec<_> = plan.messages.iter().map(|(n, m)| room_msg_json(*n, m)).collect();
                        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "room": room, "through": plan.through, "msg_count": plan.msg_count, "total_bytes": plan.total_bytes, "messages": msgs}))?);
                    } else {
                        println!("compaction plan for '{room}': {} messages ≤{:04} ({} bytes) — commit with `room compact {room} --through {} (--body|--body-file|-)`", plan.msg_count, plan.through, plan.total_bytes, plan.through);
                        print_room_msgs(&plan.messages);
                    }
                    Ok(())
                }
                Some(through) => {
                    // Commit requires an explicit body source — the
                    // digest covers exactly what the agent wrote, so
                    // a bare piped stdin never implies one.
                    anyhow::ensure!(body.is_some() || body_file.is_some() || source.is_some(), "compaction commit needs a digest body: --body TEXT, --body-file F, or `-`/stdin");
                    let body = room_body("compact", body.as_deref(), body_file.as_deref(), source.as_deref())?;
                    let (n_s, member) = room::compact_commit(room, &label, *through, &body)?;
                    if *json {
                        println!("{}", serde_json::json!({"ok": true, "room": room, "through": through, "summary_n": n_s, "member": member}));
                    } else {
                        println!("compacted ≤{through:04} into summary {n_s:04} in '{room}'{}", if member { "" } else { " (non-member)" });
                    }
                    Ok(())
                }
            }
        }
        A::Decide { room, note, as_label, json } => {
            let label = room_required_label(as_label.as_deref())?;
            let d = room::decide(room, &label, note)?;
            if *json {
                println!("{}", serde_json::json!({"ok": true, "room": room, "decision": d}));
            } else {
                println!("decision recorded in '{room}': {} ({})", d.note, d.author);
            }
            Ok(())
        }
        A::Members { room, json } => {
            let v = room::roster(room, None)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "room": v}))?);
            } else {
                println!("room:    {}", v.name);
                println!("created: {}", v.created_at);
                print_members(&v);
                println!("clients: {}", if v.clients.is_empty() { "-".into() } else { v.clients.join(", ") });
                match &v.decision {
                    Some(d) => println!("decision: {} ({} {})", d.note, d.author, d.ts),
                    None => println!("decision: none yet"),
                }
            }
            Ok(())
        }
        A::Archive { room, json } => {
            let name = room::archive(room)?;
            if *json {
                println!("{}", serde_json::json!({"ok": true, "archived": name}));
            } else {
                println!("room '{name}' archived");
            }
            Ok(())
        }
        A::Unarchive { name, json } => {
            room::unarchive(name)?;
            if *json {
                println!("{}", serde_json::json!({"ok": true, "unarchived": name}));
            } else {
                println!("room '{name}' unarchived");
            }
            Ok(())
        }
        A::Rm { room, json } => {
            let (name, archived) = room::remove(room)?;
            if *json {
                println!("{}", serde_json::json!({"ok": true, "removed": name, "archived": archived}));
            } else if archived {
                println!("archived room '{name}' removed");
            } else {
                println!("room '{name}' removed");
            }
            Ok(())
        }
    }
}

/// Mark-capable room ops need an identity; --as > `admin` (the
/// operator), canonicalized against registered projects (§17.3).
fn room_required_label(as_flag: Option<&str>) -> Result<String> {
    crate::server::room::required_label(as_flag)
}

/// Message body: `--body` > `--body-file` > `-`/stdin, the §14.1
/// precedence rule. A bare interactive call errors naming its
/// options rather than hanging on stdin. `op` names the verb in
/// errors (say, compact).
fn room_body(op: &str, body: Option<&str>, body_file: Option<&str>, source: Option<&str>) -> Result<String> {
    use std::io::{IsTerminal, Read};
    if let Some(b) = body {
        return Ok(b.to_string());
    }
    if let Some(f) = body_file {
        return std::fs::read_to_string(f).with_context(|| format!("cannot read body file {f}"));
    }
    if let Some(s) = source {
        anyhow::ensure!(s == "-", "{op} takes `-` for stdin, or --body/--body-file — got {s:?}");
    }
    if std::io::stdin().is_terminal() {
        anyhow::bail!("{op} needs a body: --body TEXT, --body-file F, or `-`/stdin");
    }
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).context("cannot read body from stdin")?;
    anyhow::ensure!(!s.trim().is_empty(), "{op} body is empty");
    Ok(s)
}

/// One message's wire shape — shared by `read --json` and the
/// `compact` plan packet. `summary_through` stays explicit (null
/// when absent) so agents detect digests from JSON.
fn room_msg_json(n: u32, m: &crate::server::room::Msg) -> serde_json::Value {
    serde_json::json!({"n": n, "author": m.author, "ts": m.ts, "body_md": m.body_md, "member": m.member, "summary_through": m.summary_through})
}

/// The human `[NNNN] ts author` rendering shared by `read` and the
/// `compact` plan packet; a digest header carries `(summary→NNNN)`
/// the same way a non-member post carries `(non-member)`.
fn print_room_msgs(msgs: &[(u32, crate::server::room::Msg)]) {
    for (n, m) in msgs {
        println!("[{n:04}] {} {}{}{}", m.ts, m.author, if m.member { "" } else { " (non-member)" }, m.summary_through.map(|t| format!(" (summary→{t:04})")).unwrap_or_default());
        println!("{}\n", m.body_md);
    }
}

/// Member list for human output, stale members marked (§17.4).
fn print_members(v: &crate::server::room::RoomView) {
    let members = v.members.iter().map(|m| if v.stale.contains(m) { format!("{m}(stale)") } else { m.clone() }).collect::<Vec<_>>().join(", ");
    println!("members: {}", if members.is_empty() { "-".into() } else { members });
}

fn print_room_ls(ls: &crate::server::room::Listings, all: bool) {
    let (label, views, suggested, archived) = (&ls.label, &ls.rooms, &ls.suggested, &ls.archived);
    if views.is_empty() {
        // Under --all the label never scoped the listing — an empty
        // table means zero rooms exist, not that it has none.
        if all {
            println!("no rooms");
        } else {
            println!("no rooms for '{label}' (--all lists every room)");
        }
    } else {
        let rows: Vec<Vec<String>> = views
            .iter()
            .map(|v| {
                let members = v.members.iter().map(|m| if v.stale.contains(m) { format!("{m}(stale)") } else { m.clone() }).collect::<Vec<_>>().join(",");
                let unread = v.unread_msgs.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
                let dec = v.decision.as_ref().map(|d| format!("{} ({}, {})", d.note, d.author, d.ts)).unwrap_or_else(|| "-".into());
                vec![v.name.clone(), v.msgs.to_string(), unread, members, dec]
            })
            .collect();
        // `msgs` is the live total; `unread` is past the resolved
        // label's mark. The header names that reader — under --all
        // the label may own no room, and a bare 'unread' column
        // would read as a total (the issue-#10 bug).
        let mut all_rows = vec![vec!["room".into(), "msgs".into(), format!("unread({label})"), "members".into(), "decision".into()]];
        all_rows.extend(rows);
        print_cols(&all_rows);
    }
    if !suggested.is_empty() {
        println!("\nsuggested (projects sharing a box, no room yet):");
        for s in suggested {
            println!("  {}: {}  ->  shunt room create <name> --members {} --clients {}", s.endpoint, s.projects.join(","), s.projects.join(","), s.clients.join(","));
        }
    }
    // Cold storage is names only — the archived line carries no
    // msgs/unread columns because an archived room has no cursor
    // work to report.
    if !archived.is_empty() {
        println!("archived: {}", archived.join(", "));
    }
}
