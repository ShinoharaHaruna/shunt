//! `client` subcommand dispatch: registry mutations (add, rename,
//! update, remove), endpoint ops (exec, hostkey, probe, env,
//! bootstrap), and listings.

use crate::cli::args::{ClientAction, ClientCmd};
use crate::cli::print_cols;
use crate::cli::server::{client_add, vendor_template_dirs, ClientAddOpts};
use crate::server::registry;
use anyhow::Result;

pub fn client(c: &ClientCmd) -> Result<()> {
    match &c.action {
        ClientAction::Add(a) => client_add(&a.name, a.template.as_deref(), ClientAddOpts { queue: a.queue.clone(), host: a.host.clone(), port: a.port, user: a.user.clone(), key: a.key.clone(), password: a.password.clone(), qroot: a.qroot.clone(), deploy_root: a.deploy_root.clone(), local: a.local, expire: a.expire.clone(), env_setup: a.env_setup.clone(), shell: a.shell.clone(), endpoint_rotates: a.endpoint_rotates, bin_path: a.bin_path.clone(), pre_connect: a.pre_connect.clone(), shared: a.shared, exit_on_idle: a.exit_on_idle }),
        ClientAction::Rename { old, new } => crate::server::client_ops::rename(old, new),
        // Fold the --endpoint-rotates / --no-endpoint-rotates pair
        // into tri-state: Some(true) sets, Some(false) clears,
        // None leaves the row field untouched.
        ClientAction::Update { name, host, port, user, password, endpoint_rotates, no_endpoint_rotates, pre_connect } => crate::server::client_ops::update(
            name,
            crate::server::client_ops::EndpointUpdate {
                host: host.clone(),
                port: *port,
                user: user.clone(),
                password: password.clone(),
                pre_connect: pre_connect.clone(),
                endpoint_rotates: if *endpoint_rotates {
                    Some(true)
                } else if *no_endpoint_rotates {
                    Some(false)
                } else {
                    None
                },
            },
        ),
        ClientAction::Exec { name, cmd } => {
            let (c, t) = crate::server::transport::client_transport(name)?;
            let out = t.exec(&c, cmd, None)?;
            print!("{out}");
            Ok(())
        }
        ClientAction::Hostkey { name } => {
            let c = registry::load_client(name)?;
            crate::server::client_ops::rekey(&c)?;
            println!("host key dropped: {}", name);
            Ok(())
        }
        ClientAction::Templates => {
            let mut rows = Vec::new();
            for d in vendor_template_dirs() {
                if let Ok(rd) = std::fs::read_dir(&d) {
                    for e in rd.flatten() {
                        if let Some(n) = e.file_name().to_str().and_then(|s| s.strip_suffix(".toml")) {
                            rows.push(vec![n.to_string(), d.display().to_string()]);
                        }
                    }
                }
            }
            print_cols(&rows);
            Ok(())
        }
        ClientAction::Ls => {
            for n in registry::list_clients()? {
                println!("{n}");
            }
            Ok(())
        }
        ClientAction::Show { name } => {
            let e = registry::load_client(name)?;
            println!("{}", toml::to_string_pretty(&e)?);
            Ok(())
        }
        ClientAction::Remove { name, purge } => {
            if name == "localhost" {
                // rm on the reserved loopback = factory reset:
                // materialize the row, then delete it — the
                // defaults regenerate on next use (this is also
                // how a disabled localhost row comes back).
                if let Ok(client) = registry::load_client(name) {
                    let queues = registry::queue_names(&client);
                    anyhow::ensure!(queues.len() == 1 && queues[0] == registry::DEFAULT_QUEUE, "localhost has named queues ({}); refusing factory reset because it would erase their configuration without stopping daemons. Run `shunt queue stop NAME -c localhost` for each queue, then back up clients/localhost.toml and reset the row manually after checking its qroots", queues.join(", "));
                }
                return registry::remove("clients", name);
            }
            crate::server::client_ops::remove(name, *purge)
        }
        ClientAction::Probe { name } => {
            let (_, t) = crate::server::transport::client_transport(name)?;
            crate::server::client_ops::probe(t.as_ref(), name)
        }
        ClientAction::Env { name } => {
            let (_, t) = crate::server::transport::client_transport(name)?;
            crate::server::client_ops::push_env(t.as_ref(), name)?;
            println!("env pushed: {}", name);
            Ok(())
        }
        ClientAction::Bootstrap { name, bin_path } => {
            let (_, t) = crate::server::transport::client_transport(name)?;
            crate::server::client_ops::bootstrap(t.as_ref(), name, bin_path.as_deref())
        }
    }
}
