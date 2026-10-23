//! Server-side dispatch helpers: remote forwarding, client
//! resolution, registry mutations (client add, vendor templates),
//! and the task-ledger `task show` read path.

use anyhow::{Context as _, Result};

use super::args::Cli;
use super::print_cols;

/// outcome live from the box when none is recorded yet (lazy
/// write-back, design.md §4.5). A unique id prefix is accepted.
pub(crate) fn task_show(id: &str, json: bool) -> Result<()> {
    use crate::server::registry;
    use crate::server::tasks::{self, Observed};
    let mut rec = match tasks::load(id) {
        Ok(r) => r,
        Err(e) => {
            // Prefix fallback: ids are long; a unique prefix resolves.
            let matches: Vec<_> = tasks::list()?.into_iter().filter(|r| r.id.starts_with(id)).collect();
            match matches.len() {
                1 => matches.into_iter().next().unwrap(),
                0 => return Err(e).context("no ledger record; the task may predate the ledger or the id is wrong"),
                _ => anyhow::bail!("id prefix '{id}' matches several tasks"),
            }
        }
    };
    let mut live: Option<String> = None;
    if rec.outcome.is_none() {
        match registry::load_client(&rec.client) {
            Ok(client) => {
                let t = crate::server::transport::transport_for(&client);
                live = Some(match tasks::observe(t.as_ref(), &client, &mut rec) {
                    Ok(obs) => match obs {
                        Observed::Pending => "pending".into(),
                        Observed::Running => "running".into(),
                        Observed::Done(rc) => format!("done rc={rc}"),
                        Observed::Failed(rc) => format!("failed rc={rc}"),
                        Observed::Unknown => "unknown".into(),
                        Observed::Unreachable(e) => format!("unreachable: {}", e.lines().next().unwrap_or(&e)),
                    },
                    Err(e) => format!("unreachable: {e:#}"),
                });
            }
            Err(e) => live = Some(format!("client unloadable: {e:#}")),
        }
    }
    if json {
        let mut v = serde_json::to_value(&rec)?;
        if let Some(l) = &live {
            v["live"] = serde_json::json!(l);
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("id:        {}", rec.id);
        println!("client:    {}", rec.client);
        println!("project:   {}", if rec.project.is_empty() { "-" } else { &rec.project });
        println!("slug:      {}", rec.slug);
        println!("queue:     {}  (on {}/{})", rec.queue_name, rec.client, rec.queue);
        println!("submitted: {}", rec.submitted_at);
        println!("pushed:    {}", rec.pushed_at);
        match (&rec.outcome, &live) {
            (Some(o), _) => println!("outcome:   {} rc={} (observed {})", o.state, o.rc, o.observed_at),
            (None, Some(l)) => println!("outcome:   {l} (live)"),
            (None, None) => println!("outcome:   not yet observed"),
        }
    }
    Ok(())
}

/// Forward one wire op to a client and print remote stdout.
/// The remote argv is `<qroot>/bin/shunt <tail...> --root <qroot>`;
/// transport is `ssh` or the `localhost` loopback (design.md §5).
pub(crate) fn remote(name: &str, tail: &[&str], stdin: Option<&str>, expect_json: bool) -> Result<()> {
    use crate::server::registry;
    use crate::server::transport::{transport_for, wire_argv};
    let base = registry::load_client(name)?;
    let queue = registry::queue_names(&base).into_iter().next().context("client has no registered queues")?;
    let client = registry::resolve_queue(&base, Some(&queue))?.client;
    let argv = wire_argv(&client, tail);
    let t = transport_for(&client);
    let out = t.exec(&client, &argv, stdin)?;
    print!("{}", remote_render(&out, expect_json)?);
    Ok(())
}

/// Queue controls must resolve an execution target, never the machine row.
pub(crate) fn remote_queue(name: &str, queue: Option<&str>, tail: &[&str], stdin: Option<&str>, expect_json: bool) -> Result<()> {
    use crate::server::transport::{transport_for, wire_argv};
    let resolved = crate::server::registry::load_queue(name, queue)?;
    let client = &resolved.client;
    let t = transport_for(client);
    let out = t.exec(client, &wire_argv(client, tail), stdin)?;
    print!("{}", remote_render(&out, expect_json)?);
    Ok(())
}

/// What `remote` prints for one chunk of remote stdout. Wire ops
/// answer a `{"protocol":1,…}` object that can sit inside login-shell
/// noise (motd/.bashrc banners — design.md §5): extract renders the
/// object pretty and drops the noise. A JSON object that fails
/// extraction — wrong version or no `protocol` at all — is binary
/// drift: surface §5's steering error rather than printing the
/// stale line raw. Pure-noise output still prints verbatim.
///
/// `expect_json` is the op's §5 response kind: wire ops answering a
/// `{protocol:1,…}` object pass `true`; text ops (`logs`) pass `false`
/// and their stdout prints verbatim — a log line that happens to be a
/// protocol-shaped JSON object is log content, never a wire answer.
fn remote_render(out: &str, expect_json: bool) -> Result<String> {
    if !expect_json {
        return Ok(verbatim(out));
    }
    match crate::proto::wire::extract(out) {
        Ok(v) => Ok(format!("{}\n", serde_json::to_string_pretty(&v)?)),
        Err(e) => {
            let drift = out.lines().any(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok().is_some_and(|v| v.is_object()));
            if drift {
                return Err(e);
            }
            Ok(verbatim(out))
        }
    }
}

/// Pass remote stdout through unchanged, adding one trailing newline
/// when the stream lacked it (keeps the user's next prompt clean).
fn verbatim(out: &str) -> String {
    if out.ends_with('\n') {
        out.to_string()
    } else {
        format!("{out}\n")
    }
}

/// No queue context: no --root flag and no SHUNT_ROOT env — the
/// call has nothing local to act on.
pub(crate) fn rootless(root: &Option<String>) -> bool {
    root.is_none() && std::env::var_os("SHUNT_ROOT").is_none()
}

/// A typed client reference (`-c`, `version -c`, `upgrade -c`,
/// `project bind <client>`): resolve through the registry name
/// middleware (design.md §4.1). A unique case-fold hit substitutes
/// the canonical spelling with a stderr note — picking a box to
/// talk to deserves a spoken word — and a miss carries did-you-mean
/// suggestions. (Configured names like `default_client` are not
/// typed input and do not come through here.)
pub(crate) fn canonical_client(typed: &str) -> Result<String> {
    use crate::server::registry::names::{self, Resolution};
    match names::resolve_client(typed)? {
        Resolution::Unknown { input, suggestions } => {
            // list_clients skips disabled rows, so a disabled name
            // lands here — name it disabled rather than unknown.
            if crate::server::registry::load_client_row(&input).is_ok_and(|r| r.disabled) {
                anyhow::bail!("client {input} is disabled");
            }
            anyhow::bail!("unknown client {input:?}{}", names::did_you_mean(&suggestions));
        }
        r => names::into_resolved(r, "client"),
    }
}

/// Client for a server-only command (submit, tunnel): `-c` wins,
/// else `server.toml` `default_client`; neither is an error
/// (design.md §4.1).
pub(crate) fn required_client(flag: &Option<String>) -> Result<String> {
    if let Some(c) = flag {
        return canonical_client(c);
    }
    crate::server::registry::load_server_config()?.default_client.ok_or_else(|| anyhow::anyhow!("no -c/--client given and no default_client in server.toml"))
}

/// Client for a dual-mode command (`-c` or the local queue): `-c`
/// wins; a known local root keeps the call local; only when there
/// is nothing local to act on does `default_client` fill in — so a
/// server shell needn't repeat `-c`, and a box's own queue ops are
/// never hijacked by a stray server.toml (design.md §4.1).
pub(crate) fn resolve_client(flag: &Option<String>, root: &Option<String>) -> Result<Option<String>> {
    if let Some(c) = flag {
        return Ok(Some(canonical_client(c)?));
    }
    if !rootless(root) {
        return Ok(None);
    }
    Ok(crate::server::registry::load_server_config()?.default_client)
}

/// Bare invocations that cannot act print their own -h instead of
/// dying on 'queue root unknown' — a lost user learns the flags.
pub(crate) fn print_subhelp(sub: &str) -> Result<()> {
    use clap::CommandFactory;
    Cli::command().find_subcommand_mut(sub).expect(sub).print_help()?;
    Ok(())
}

/// `shunt room` (design.md §17): all server-local, so dispatch is a
/// thin shell over crate::server::room — body-source resolution is
/// the only CLI concern here (the §14.1 precedence rule).
pub(crate) fn cmd_room(c: &super::args::RoomCmd) -> Result<()> {
    use super::args::RoomAction as A;
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
            let (label, views, suggested) = room::list(as_label.as_deref(), *all)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "as": label, "rooms": views, "suggested": suggested}))?);
            } else {
                print_room_ls(&label, &views, &suggested);
            }
            Ok(())
        }
        A::Say { room, body, body_file, source, as_label, json } => {
            let label = room_required_label(as_label.as_deref())?;
            let body = room_body(body.as_deref(), body_file.as_deref(), source.as_deref())?;
            let (n, member) = room::say(room, &label, &body)?;
            if *json {
                println!("{}", serde_json::json!({"ok": true, "room": room, "n": n, "member": member}));
            } else {
                println!("posted {n:04} to '{room}'{}", if member { "" } else { " (non-member)" });
            }
            Ok(())
        }
        A::Read { room, as_label, all, json } => {
            let label = room_required_label(as_label.as_deref())?;
            let msgs = room::read(room, &label, *all)?;
            if *json {
                let msgs: Vec<_> = msgs.iter().map(|(n, m)| serde_json::json!({"n": n, "author": m.author, "ts": m.ts, "body_md": m.body_md, "member": m.member})).collect();
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({"ok": true, "room": room, "messages": msgs}))?);
            } else {
                for (n, m) in &msgs {
                    println!("[{n:04}] {} {}{}", m.ts, m.author, if m.member { "" } else { " (non-member)" });
                    println!("{}\n", m.body_md);
                }
                if msgs.is_empty() {
                    println!("(no new messages)");
                }
            }
            Ok(())
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
        A::Rm { room, json } => {
            let name = room::remove(room)?;
            if *json {
                println!("{}", serde_json::json!({"ok": true, "removed": name}));
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

/// say body: `--body` > `--body-file` > `-`/stdin, the §14.1
/// precedence rule. A bare interactive call errors naming its
/// options rather than hanging on stdin.
fn room_body(body: Option<&str>, body_file: Option<&str>, source: Option<&str>) -> Result<String> {
    use std::io::{IsTerminal, Read};
    if let Some(b) = body {
        return Ok(b.to_string());
    }
    if let Some(f) = body_file {
        return std::fs::read_to_string(f).with_context(|| format!("cannot read body file {f}"));
    }
    if let Some(s) = source {
        anyhow::ensure!(s == "-", "say takes `-` for stdin, or --body/--body-file — got {s:?}");
    }
    if std::io::stdin().is_terminal() {
        anyhow::bail!("say needs a body: --body TEXT, --body-file F, or `-`/stdin");
    }
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).context("cannot read body from stdin")?;
    anyhow::ensure!(!s.trim().is_empty(), "say body is empty");
    Ok(s)
}

/// Member list for human output, stale members marked (§17.4).
fn print_members(v: &crate::server::room::RoomView) {
    let members = v.members.iter().map(|m| if v.stale.contains(m) { format!("{m}(stale)") } else { m.clone() }).collect::<Vec<_>>().join(", ");
    println!("members: {}", if members.is_empty() { "-".into() } else { members });
}

fn print_room_ls(label: &str, views: &[crate::server::room::RoomView], suggested: &[crate::server::room::Suggested]) {
    if views.is_empty() {
        println!("no rooms for '{label}' (--all lists every room)");
    } else {
        let rows: Vec<Vec<String>> = views
            .iter()
            .map(|v| {
                let members = v.members.iter().map(|m| if v.stale.contains(m) { format!("{m}(stale)") } else { m.clone() }).collect::<Vec<_>>().join(",");
                let unread = v.unread_msgs.map(|n| n.to_string()).unwrap_or_else(|| "-".into());
                let dec = v.decision.as_ref().map(|d| format!("{} ({}, {})", d.note, d.author, d.ts)).unwrap_or_else(|| "-".into());
                vec![v.name.clone(), unread, members, dec]
            })
            .collect();
        let mut all_rows = vec![vec!["room".into(), "unread".into(), "members".into(), "decision".into()]];
        all_rows.extend(rows);
        print_cols(&all_rows);
    }
    if !suggested.is_empty() {
        println!("\nsuggested (projects sharing a box, no room yet):");
        for s in suggested {
            println!("  {}: {}  ->  shunt room create <name> --members {} --clients {}", s.endpoint, s.projects.join(","), s.projects.join(","), s.clients.join(","));
        }
    }
}

/// `shunt client add`: seeds the row from a vendor template when given,
/// then applies explicit flags. Real (server-local bookkeeping only).
/// Optional field overrides for `client add`.
pub(crate) struct ClientAddOpts {
    pub(crate) queue: Option<String>,
    pub(crate) host: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) user: Option<String>,
    pub(crate) key: Option<String>,
    pub(crate) qroot: Option<String>,
    pub(crate) deploy_root: Option<String>,
    pub(crate) local: bool,
    pub(crate) expire: Option<String>,
    pub(crate) env_setup: Option<String>,
    pub(crate) shell: Option<String>,
    pub(crate) endpoint_rotates: bool,
    pub(crate) password: Option<String>,
    pub(crate) bin_path: Option<String>,
    pub(crate) shared: bool,
    pub(crate) exit_on_idle: bool,
}

pub(crate) fn client_add(name: &str, template: Option<&str>, opts: ClientAddOpts) -> Result<()> {
    use crate::server::registry::{valid_name, ClientEntry, SshEndpoint};

    valid_name(name)?;
    // `add` is create-only: silently overwriting would clobber
    // hand-edited fields and regenerate the dedicated keypair.
    let row = crate::server::registry::shunt_home()?.join("clients").join(format!("{name}.toml"));
    anyhow::ensure!(!row.exists(), "client '{name}' already exists (use `client update` or `client rm` first)");

    let qroot_explicit = opts.qroot.is_some();
    let mut entry = match template {
        Some(t) => load_vendor_template(t)?,
        None => ClientEntry { name: name.into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint { host: String::new(), port: 22, user: "root".into(), key: String::new(), password: String::new() }, qroot: String::new(), queues: Default::default(), default_queue: None, deploy_root: String::new(), facts: Default::default(), billing: None },
    };
    entry.name = name.into();
    // Adding a row for the built-in name materializes the loopback
    // defaults instead of a broken ssh-shaped row (registry §4.1).
    if name == "localhost" {
        entry.local = true;
        if entry.qroot.is_empty() {
            entry.qroot = crate::server::registry::shunt_home()?.join("queue").display().to_string();
        }
        if entry.deploy_root.is_empty() {
            entry.deploy_root = std::env::var("HOME").unwrap_or_default();
        }
    }
    if let Some(v) = opts.host {
        entry.ssh.host = v;
    }
    if let Some(v) = opts.port {
        entry.ssh.port = v;
    }
    if let Some(v) = opts.user {
        entry.ssh.user = v;
    }
    if let Some(v) = opts.key {
        entry.ssh.key = v;
    }
    if let Some(v) = opts.qroot {
        entry.qroot = v;
    }
    if let Some(v) = opts.deploy_root {
        entry.deploy_root = v;
    }
    if opts.local {
        entry.local = true;
    }
    if let Some(v) = opts.expire {
        entry.expire = v;
    }
    if let Some(v) = opts.env_setup {
        entry.env_setup = v;
    }
    if let Some(v) = opts.shell {
        entry.shell = v;
    }
    if let Some(v) = opts.password {
        entry.ssh.password = v;
    }
    if let Some(v) = opts.bin_path {
        entry.bin_path = Some(v);
    }
    if opts.shared {
        entry.shared = true;
    }
    if opts.exit_on_idle {
        entry.exit_on_idle = true;
    }
    if opts.endpoint_rotates {
        entry.endpoint_rotates = true;
    }
    // qroot defaults to <deploy_root>/queue — an empty qroot would
    // make remote ops land in the box's CWD (silent wrongness).
    if entry.qroot.is_empty() && !entry.deploy_root.is_empty() {
        entry.qroot = format!("{}/queue", entry.deploy_root.trim_end_matches('/'));
    }
    let initial_queue = opts.queue.as_deref().unwrap_or(crate::server::registry::DEFAULT_QUEUE);
    valid_name(initial_queue)?;
    let legacy = entry.queues.is_empty();
    crate::server::registry::canonicalize_queues(&mut entry)?;
    if legacy && initial_queue != crate::server::registry::DEFAULT_QUEUE {
        let initial = entry.queues.remove(crate::server::registry::DEFAULT_QUEUE).expect("canonical initial queue");
        entry.queues.insert(initial_queue.into(), initial);
        entry.default_queue = Some(initial_queue.into());
    }
    let execution = crate::server::registry::resolve_queue(&entry, Some(initial_queue))?.client;
    // Dedicated keypair per client (design.md §4.1): generated at
    // registration so the row is usable immediately.
    if !entry.local {
        crate::server::client_ops::ensure_key(&mut entry)?;
    }
    let p = crate::server::registry::save_client(&entry)?;
    println!("wrote {}", p.display());
    if !entry.local && (entry.ssh.host.is_empty() || entry.ssh.key.is_empty() || entry.ssh.port == 0) {
        eprintln!("note: ssh fields still incomplete; edit the row before bootstrap");
    }
    // §4.1 shared: the template default may collide with another
    // tenant's queue — placement on a shared box is a user choice,
    // not a default to accept silently.
    if entry.shared && !qroot_explicit {
        eprintln!("note: shared row defaults qroot to '{}' — confirm with the user it does not collide with other tenants' queues (--qroot)", execution.qroot);
    }
    // Same endpoint+user+qroot = the same remote queue: a second
    // install on it merges silently (§4.1 owner claim). Warn at
    // registration, not at bootstrap.
    for p in crate::server::client_ops::same_queue_peers(name, &execution) {
        eprintln!("note: client '{p}' already targets {}@{}:{} + qroot '{}' — two installs on one queue merge silently; pick a different --qroot", entry.ssh.user, entry.ssh.host, entry.ssh.port, execution.qroot);
    }
    Ok(())
}

/// Vendor-template search dirs, first hit wins: `$SHUNT_VENDOR_DIR`,
/// the registry's own `vendors/` (user-supplied templates live with
/// the data they seed), then the repo's `etc/vendors/` — a
/// compile-time path that only works for builds run from the source
/// tree.
pub(crate) fn vendor_template_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(d) = std::env::var("SHUNT_VENDOR_DIR") {
        dirs.push(std::path::PathBuf::from(d));
    }
    if let Ok(h) = crate::server::registry::shunt_home() {
        dirs.push(h.join("vendors"));
    }
    dirs.push(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("etc/vendors"));
    dirs
}

/// Load `<name>.toml` from the vendor-template dirs as a client-row
/// seed. On a miss the error lists the templates that do exist.
pub(crate) fn load_vendor_template(name: &str) -> Result<crate::server::registry::ClientEntry> {
    let dirs = vendor_template_dirs();
    for dir in &dirs {
        let path = dir.join(format!("{name}.toml"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            return toml::from_str(&text).with_context(|| format!("bad vendor template {}", path.display()));
        }
    }
    let mut have: Vec<String> = Vec::new();
    for dir in &dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str().and_then(|s| s.strip_suffix(".toml")) {
                    have.push(n.to_string());
                }
            }
        }
    }
    have.sort();
    have.dedup();
    let note = if have.is_empty() { format!("no templates found in any of: {}", dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")) } else { format!("available templates: {}", have.join(", ")) };
    anyhow::bail!("no vendor template named {name}; {note}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::registry::tests::HOME_LOCK;

    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shunt-cli-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn required_client_falls_back_to_default() {
        let _g = HOME_LOCK.lock().unwrap();
        let home = tmp_home("reqcli");
        std::env::set_var("SHUNT_HOME", &home);
        std::fs::write(home.join("server.toml"), "default_client = \"gpu-1\"\n").unwrap();
        // The -c value resolves through the registry name middleware,
        // so the row must exist for the flag path to return it.
        std::fs::create_dir_all(home.join("clients")).unwrap();
        std::fs::write(home.join("clients/other.toml"), "name = \"other\"\n").unwrap();
        assert_eq!(required_client(&None).unwrap(), "gpu-1");
        assert_eq!(required_client(&Some("other".into())).unwrap(), "other");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn required_client_errors_without_flag_or_default() {
        let _g = HOME_LOCK.lock().unwrap();
        let home = tmp_home("nocli");
        std::env::set_var("SHUNT_HOME", &home);
        let err = required_client(&None).unwrap_err().to_string();
        assert!(err.contains("default_client"), "{err}");
        std::fs::remove_dir_all(&home).ok();
    }

    /// Wire stdout buried in login-shell noise renders as the
    /// pretty object; non-wire output (logs, ad-hoc text) stays
    /// verbatim; a wrong-protocol object surfaces §5's drift error.
    #[test]
    fn remote_render_extracts_wire_from_noise() {
        let noisy = "Welcome to AutoDL\n{\"protocol\":1,\"ok\":true,\"name\":\"0100_x\"}\n";
        let r = remote_render(noisy, true).unwrap();
        assert!(r.contains("\"protocol\": 1") && r.contains("\"name\": \"0100_x\""), "{r}");
        assert!(!r.contains("Welcome"), "{r}");

        assert_eq!(remote_render("task log line\nmore\n", true).unwrap(), "task log line\nmore\n");
        assert_eq!(remote_render("no trailing nl", true).unwrap(), "no trailing nl\n");

        let err = remote_render("banner\n{\"protocol\":2,\"ok\":true}\n", true).unwrap_err().to_string();
        assert!(err.contains("protocol mismatch"), "{err}");

        // A pre-v1 remote answering a bare JSON object (no `protocol`)
        // is drift too — the §5 steering error must surface, not a
        // verbatim print that exits 0.
        let err = remote_render("{\"ok\":true,\"name\":\"0100_x\"}\n", true).unwrap_err().to_string();
        assert!(err.contains("client bootstrap"), "{err}");

        // Garbage around a protocol-bearing stale answer still errors.
        let err = remote_render("\x1b[0mgarbage\n{\"protocol\":2}\ntrailing", true).unwrap_err().to_string();
        assert!(err.contains("protocol mismatch"), "{err}");
    }

    /// Text ops (`logs`) never touch extract: a task log containing a
    /// protocol-shaped line prints verbatim — it is neither collapsed
    /// to that object nor read as binary drift (design.md §5).
    #[test]
    fn remote_render_text_op_prints_verbatim() {
        let log = "epoch 1 loss 0.5\n{\"protocol\":1,\"ok\":true}\nepoch 2 loss 0.4\n";
        assert_eq!(remote_render(log, false).unwrap(), log);

        let drift_looking = "epoch 1\n{\"protocol\":2,\"ok\":true}\n";
        assert_eq!(remote_render(drift_looking, false).unwrap(), drift_looking);

        assert_eq!(remote_render("log without newline", false).unwrap(), "log without newline\n");
    }

    #[test]
    fn resolve_client_respects_local_scope() {
        let _g = HOME_LOCK.lock().unwrap();
        let home = tmp_home("rescli");
        std::env::set_var("SHUNT_HOME", &home);
        std::env::remove_var("SHUNT_ROOT");
        std::fs::write(home.join("server.toml"), "default_client = \"gpu-1\"\n").unwrap();
        // No flag + no local queue -> default_client.
        assert_eq!(resolve_client(&None, &None).unwrap().as_deref(), Some("gpu-1"));
        // A local root in scope keeps the call local.
        assert_eq!(resolve_client(&None, &Some("/tmp/q".into())).unwrap(), None);
        // Explicit -c always wins — resolved through the name
        // middleware, so the row must be registered.
        std::fs::create_dir_all(home.join("clients")).unwrap();
        std::fs::write(home.join("clients/x.toml"), "name = \"x\"\n").unwrap();
        assert_eq!(resolve_client(&Some("x".into()), &Some("/tmp/q".into())).unwrap().as_deref(), Some("x"));
        std::fs::remove_dir_all(&home).ok();
    }
}
