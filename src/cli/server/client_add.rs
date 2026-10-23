//! `client add` row seeding: vendor templates + flag overrides
//! (server-local bookkeeping only).

use anyhow::{Context as _, Result};

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
