//! Pin `global = true` on `-c` for the subcommand-grouped Args
//! structs: spec §10 documents the flag after the action
//! (`tunnel up -c x`, `shutdown on -c x`, `workers set 5 -c x`).
//! Global flags must keep parsing in the pre-action position too.

use super::*;

fn parse(argv: &[&str]) -> Cli {
    Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} failed: {e}"))
}

#[test]
fn named_queue_server_forms() {
    for argv in [vec!["shunt", "submit", "-c", "box", "--queue", "fast", "--", "true"], vec!["shunt", "status", "-c", "box", "--queue", "fast", "--json"], vec!["shunt", "logs", "-c", "box", "--queue", "fast"], vec!["shunt", "cancel", "train", "-c", "box", "--queue", "fast"], vec!["shunt", "prio", "train", "1", "-c", "box", "--queue", "fast"], vec!["shunt", "kill", "train", "-c", "box", "--queue", "fast"], vec!["shunt", "workers", "set", "2", "-c", "box", "--queue", "fast", "--json"], vec!["shunt", "shutdown", "on", "-c", "box", "--queue", "fast", "--json"], vec!["shunt", "upgrade", "-c", "box", "--queue", "fast", "--check"]] {
        parse(&argv);
    }
    let cli = parse(&["shunt", "status", "--queue", "fast"]);
    let Command::Status(s) = cli.command else { panic!() };
    assert_eq!(s.queue.as_deref(), Some("fast"));
}

#[test]
fn named_queue_rejects_local_roots_and_aggregate() {
    for argv in [vec!["shunt", "status", "--all", "--queue", "fast"], vec!["shunt", "status", "--as", "p", "--queue", "fast"], vec!["shunt", "status", "--root", "/tmp/q", "--queue", "fast"], vec!["shunt", "logs", "--root", "/tmp/q", "--queue", "fast"], vec!["shunt", "cancel", "train", "--root", "/tmp/q", "--queue", "fast"], vec!["shunt", "prio", "train", "1", "--root", "/tmp/q", "--queue", "fast"], vec!["shunt", "kill", "train", "--root", "/tmp/q", "--queue", "fast"], vec!["shunt", "workers", "set", "2", "--root", "/tmp/q", "--queue", "fast"], vec!["shunt", "shutdown", "on", "--root", "/tmp/q", "--queue", "fast"]] {
        assert!(Cli::try_parse_from(&argv).is_err(), "accepted {argv:?}");
    }
}

#[test]
fn queue_actions_accept_global_flags_in_both_positions() {
    for action in [vec!["add", "fast", "--qroot", "/tmp/fast", "--workers", "2"], vec!["ls"], vec!["show", "fast"], vec!["default", "fast"], vec!["set", "fast", "--exit-on-idle", "--workers", "3"], vec!["set", "fast", "--no-workers", "--env-setup", ""], vec!["rm", "fast", "--purge"], vec!["bootstrap", "fast", "--bin-path", "/tmp/shunt"], vec!["env", "fast"], vec!["start", "fast"], vec!["stop", "fast"]] {
        for before in [true, false] {
            let mut argv = vec!["shunt", "queue"];
            if before {
                argv.extend(["-c", "box", "--json"]);
            }
            argv.extend(&action);
            if !before {
                argv.extend(["-c", "box", "--json"]);
            }
            let cli = parse(&argv);
            let Command::Queue(q) = cli.command else { panic!() };
            assert_eq!(q.client.as_deref(), Some("box"));
            assert!(q.json);
        }
    }
}

#[test]
fn project_pair_and_initial_queue_forms() {
    for argv in [vec!["shunt", "project", "bind", "p", "box", "--queue", "fast", "--workdir", "/work"], vec!["shunt", "project", "bind", "p", "--client", "box", "--queue", "fast", "--workdir", "/work"], vec!["shunt", "project", "unbind", "p", "box", "--queue", "fast"], vec!["shunt", "project", "unbind", "p", "--client", "box", "--queue", "fast"], vec!["shunt", "client", "add", "box", "--queue", "fast"]] {
        parse(&argv);
    }
    assert!(Cli::try_parse_from(["shunt", "project", "bind", "p", "--workdir", "/work"]).is_err());
    assert!(Cli::try_parse_from(["shunt", "project", "unbind", "p", "box", "--client", "other"]).is_err());
}

#[test]
fn tunnel_client_after_subcommand() {
    let cli = parse(&["shunt", "tunnel", "up", "-c", "box1"]);
    let Command::Tunnel(t) = cli.command else { panic!() };
    assert_eq!(t.client.as_deref(), Some("box1"));
    assert!(matches!(t.action, TunnelAction::Up { port: None }));
}

#[test]
fn tunnel_client_before_subcommand_still_works() {
    let cli = parse(&["shunt", "tunnel", "-c", "box1", "status"]);
    let Command::Tunnel(t) = cli.command else { panic!() };
    assert_eq!(t.client.as_deref(), Some("box1"));
    assert!(matches!(t.action, TunnelAction::Status));
}

#[test]
fn shutdown_client_after_subcommand() {
    let cli = parse(&["shunt", "shutdown", "on", "-c", "box1", "--json"]);
    let Command::Shutdown(s) = cli.command else { panic!() };
    assert_eq!(s.client.as_deref(), Some("box1"));
    assert!(s.json);
    assert!(matches!(s.action, ShutdownAction::On { grace_s: None, .. }));
}

#[test]
fn shutdown_client_before_subcommand_still_works() {
    let cli = parse(&["shunt", "shutdown", "-c", "box1", "status"]);
    let Command::Shutdown(s) = cli.command else { panic!() };
    assert_eq!(s.client.as_deref(), Some("box1"));
    assert!(matches!(s.action, ShutdownAction::Status));
}

#[test]
fn workers_client_after_subcommand() {
    let cli = parse(&["shunt", "workers", "set", "5", "-c", "box1"]);
    let Command::Workers(w) = cli.command else { panic!() };
    assert_eq!(w.client.as_deref(), Some("box1"));
    assert!(matches!(w.action, Some(WorkerAction::Set { n: 5 })));
}

/// Spec §5's wire table spells the queue-root flag `--qroot`;
/// the visible alias keeps a spec-literal caller working.
#[test]
fn root_accepts_qroot_alias() {
    for flag in ["--root", "--qroot"] {
        let cli = parse(&["shunt", "init", flag, "/tmp/q"]);
        let Command::Init(a) = cli.command else { panic!() };
        assert_eq!(a.root.as_deref(), Some("/tmp/q"));
    }
}

/// Spec §5's wire table spells the enqueue slug as `--slug`;
/// the option and the positional merge into one value.
#[test]
fn enqueue_accepts_slug_flag() {
    let cli = parse(&["shunt", "enqueue", "-", "--slug", "train", "--json"]);
    let Command::Enqueue(a) = cli.command else { panic!() };
    assert_eq!(a.slug_opt.as_deref(), Some("train"));
    assert!(a.slug.is_none());
}

#[test]
fn enqueue_slug_flag_conflicts_with_positional() {
    assert!(Cli::try_parse_from(["shunt", "enqueue", "-", "pos", "--slug", "flag"]).is_err());
    assert!(Cli::try_parse_from(["shunt", "enqueue", "-", "pos"]).is_ok());
}

/// `--all` is the server aggregate; a local `--root` would be
/// silently dropped, so clap rejects the mix like `--client`.
#[test]
fn status_root_conflicts_with_all() {
    assert!(Cli::try_parse_from(["shunt", "status", "--all", "--root", "/tmp/q"]).is_err());
    assert!(Cli::try_parse_from(["shunt", "status", "--root", "/tmp/q"]).is_ok());
}

/// `room` command surface (§17.2): `say r -` reads stdin,
/// `--as` is the identity flag, `rm` is the only cleanup.
#[test]
fn room_forms() {
    let cli = parse(&["shunt", "room", "create", "gpu", "--members", "p1,p2", "--clients", "b1"]);
    let Command::Room(r) = cli.command else { panic!() };
    let RoomAction::Create { name, members, clients, .. } = r.action else { panic!() };
    assert_eq!(name, "gpu");
    assert_eq!(members, ["p1", "p2"]);
    assert_eq!(clients, ["b1"]);

    let cli = parse(&["shunt", "room", "say", "gpu", "-", "--as", "p1"]);
    let Command::Room(r) = cli.command else { panic!() };
    let RoomAction::Say { source, as_label, .. } = r.action else { panic!() };
    assert_eq!(source.as_deref(), Some("-"));
    assert_eq!(as_label.as_deref(), Some("p1"));

    let cli = parse(&["shunt", "room", "decide", "gpu", "--note", "cap 12G", "--json"]);
    let Command::Room(r) = cli.command else { panic!() };
    assert!(matches!(r.action, RoomAction::Decide { .. }));

    let cli = parse(&["shunt", "room", "rm", "gpu"]);
    let Command::Room(r) = cli.command else { panic!() };
    assert!(matches!(r.action, RoomAction::Rm { .. }));

    let cli = parse(&["shunt", "room", "archive", "gpu", "--json"]);
    let Command::Room(r) = cli.command else { panic!() };
    assert!(matches!(r.action, RoomAction::Archive { .. }));

    let cli = parse(&["shunt", "room", "unarchive", "gpu"]);
    let Command::Room(r) = cli.command else { panic!() };
    assert!(matches!(r.action, RoomAction::Unarchive { .. }));

    assert!(Cli::try_parse_from(["shunt", "room", "create", "x"]).is_err());
}

/// `room compact` is one verb with two phases: no --through is
/// the plan, --through + a body source commits. `--ratio` is a
/// plan knob and conflicts with --through. `read`'s --all
/// conflicts with --digest/--from/--to, and --digest with the
/// range bounds; --from/--to may each appear alone.
#[test]
fn room_compact_and_read_mode_forms() {
    let cli = parse(&["shunt", "room", "compact", "gpu"]);
    let Command::Room(r) = cli.command else { panic!() };
    assert!(matches!(r.action, RoomAction::Compact { through: None, ratio: None, .. }));

    let cli = parse(&["shunt", "room", "compact", "gpu", "--through", "40", "--body", "d", "--as", "p1"]);
    let Command::Room(r) = cli.command else { panic!() };
    let RoomAction::Compact { through, body, as_label, .. } = r.action else { panic!() };
    assert_eq!(through, Some(40));
    assert_eq!(body.as_deref(), Some("d"));
    assert_eq!(as_label.as_deref(), Some("p1"));

    assert!(Cli::try_parse_from(["shunt", "room", "compact", "gpu", "--through", "40", "--ratio", "0.5", "--body", "d"]).is_err());
    assert!(Cli::try_parse_from(["shunt", "room", "compact", "gpu", "-", "--through", "40"]).is_ok());

    for argv in [vec!["shunt", "room", "read", "gpu", "--all", "--digest"], vec!["shunt", "room", "read", "gpu", "--all", "--from", "1"], vec!["shunt", "room", "read", "gpu", "--all", "--to", "9"], vec!["shunt", "room", "read", "gpu", "--digest", "--from", "1"], vec!["shunt", "room", "read", "gpu", "--digest", "--to", "9"]] {
        assert!(Cli::try_parse_from(&argv).is_err(), "accepted {argv:?}");
    }
    for argv in [vec!["shunt", "room", "read", "gpu", "--from", "10", "--to", "20"], vec!["shunt", "room", "read", "gpu", "--from", "10"], vec!["shunt", "room", "read", "gpu", "--to", "20"], vec!["shunt", "room", "read", "gpu", "--digest"]] {
        parse(&argv);
    }
}

/// `status --as` carries the rooms block (§17.5) and only makes
/// sense on the server aggregate — a -c wire op or a local
/// --root has no rooms to report.
#[test]
fn status_as_conflicts_with_client_and_root() {
    let cli = parse(&["shunt", "status", "--json", "--as", "p1"]);
    let Command::Status(s) = cli.command else { panic!() };
    assert_eq!(s.as_label.as_deref(), Some("p1"));
    assert!(Cli::try_parse_from(["shunt", "status", "--as", "p1", "-c", "x"]).is_err());
    assert!(Cli::try_parse_from(["shunt", "status", "--as", "p1", "--root", "/tmp/q"]).is_err());
}

#[test]
fn workers_get_client_after_subcommand() {
    let cli = parse(&["shunt", "workers", "get", "-c", "box1"]);
    let Command::Workers(w) = cli.command else { panic!() };
    assert_eq!(w.client.as_deref(), Some("box1"));
    assert!(matches!(w.action, Some(WorkerAction::Get)));
}

/// The send form and the ledger subcommands share one Args
/// struct; `--json` is global so `feedback ls --json` parses.
#[test]
fn feedback_forms() {
    let cli = parse(&["shunt", "feedback", "daemon ate my task", "--kind", "ux"]);
    let Command::Feedback(f) = cli.command else { panic!() };
    assert_eq!(f.title.as_deref(), Some("daemon ate my task"));
    assert_eq!(f.kind, "ux");
    assert!(f.action.is_none());

    let cli = parse(&["shunt", "feedback", "ls", "--json"]);
    let Command::Feedback(f) = cli.command else { panic!() };
    assert!(matches!(f.action, Some(FeedbackAction::Ls)));
    assert!(f.json);

    let cli = parse(&["shunt", "feedback", "retry", "1735736"]);
    let Command::Feedback(f) = cli.command else { panic!() };
    assert!(matches!(f.action, Some(FeedbackAction::Retry { id: Some(ref s) }) if s == "1735736"));

    let cli = parse(&["shunt", "feedback", "retry"]);
    let Command::Feedback(f) = cli.command else { panic!() };
    assert!(matches!(f.action, Some(FeedbackAction::Retry { id: None })));

    assert!(Cli::try_parse_from(["shunt", "feedback"]).is_err());
    assert!(Cli::try_parse_from(["shunt", "feedback", "t", "--body", "a", "--body-file", "f"]).is_err());
}
