//! Server-side dispatch helpers: remote forwarding, client
//! resolution, registry mutations (client add, vendor templates),
//! and the task-ledger `task show` read path.

use anyhow::{Context as _, Result};

use super::args::Cli;

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
                if let Ok(obs) = tasks::observe(t.as_ref(), &client, &mut rec) {
                    live = Some(match obs {
                        Observed::Pending => "pending".into(),
                        Observed::Running => "running".into(),
                        Observed::Done(rc) => format!("done rc={rc}"),
                        Observed::Failed(rc) => format!("failed rc={rc}"),
                        Observed::Unknown => "unknown".into(),
                        Observed::Unreachable(e) => format!("unreachable: {}", e.lines().next().unwrap_or(&e)),
                    });
                }
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
        println!("queue:     {}  (on {})", rec.queue_name, rec.client);
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
    let client = registry::load_client(name)?;
    let argv = wire_argv(&client, tail);
    let t = transport_for(&client);
    let out = t.exec(&client, &argv, stdin)?;
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

/// Client for a server-only command (submit, tunnel): `-c` wins,
/// else `server.toml` `default_client`; neither is an error
/// (design.md §4.1).
pub(crate) fn required_client(flag: &Option<String>) -> Result<String> {
    if let Some(c) = flag {
        return Ok(c.clone());
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
        return Ok(Some(c.clone()));
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

/// `shunt client add`: seeds the row from a vendor template when given,
/// then applies explicit flags. Real (server-local bookkeeping only).
/// Optional field overrides for `client add`.
pub(crate) struct ClientAddOpts {
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
}

pub(crate) fn client_add(name: &str, template: Option<&str>, opts: ClientAddOpts) -> Result<()> {
    use crate::server::registry::{valid_name, ClientEntry, SshEndpoint};

    valid_name(name)?;
    // `add` is create-only: silently overwriting would clobber
    // hand-edited fields and regenerate the dedicated keypair.
    let row = crate::server::registry::shunt_home()?.join("clients").join(format!("{name}.toml"));
    anyhow::ensure!(!row.exists(), "client '{name}' already exists (use `client update` or `client rm` first)");

    let mut entry = match template {
        Some(t) => load_vendor_template(t)?,
        None => ClientEntry { name: name.into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint { host: String::new(), port: 22, user: "root".into(), key: String::new(), password: String::new() }, qroot: String::new(), deploy_root: String::new(), facts: Default::default(), billing: None },
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
    if opts.endpoint_rotates {
        entry.endpoint_rotates = true;
    }
    // qroot defaults to <deploy_root>/queue — an empty qroot would
    // make remote ops land in the box's CWD (silent wrongness).
    if entry.qroot.is_empty() && !entry.deploy_root.is_empty() {
        entry.qroot = format!("{}/queue", entry.deploy_root.trim_end_matches('/'));
    }
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
    // An empty qroot survives add only when deploy_root was empty
    // too; load_client now rejects such rows, so warn for any
    // transport (local AND ssh-shaped rows with no deploy_root).
    if entry.qroot.is_empty() {
        eprintln!("note: row has no qroot — it will not load as a client; set qroot (or deploy_root) in the row");
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
        // Explicit -c always wins.
        assert_eq!(resolve_client(&Some("x".into()), &Some("/tmp/q".into())).unwrap().as_deref(), Some("x"));
        std::fs::remove_dir_all(&home).ok();
    }
}
