//! `client probe` (design.md §4.1, §8). Facts come over the wire
//! (`shunt probe --json` on the box); `agent::probe` is the single
//! probe implementation. On an unbootstrapped box the call fails
//! with the binary-drift guidance from `wire::extract`.

use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{shunt_bin, Transport};
use anyhow::{Context, Result};
use std::collections::BTreeMap;

/// Run `shunt probe --json` on the box and turn the `ProbeReport`
/// into a facts map (protocol field and nulls dropped).
pub fn probe_facts(t: &dyn Transport, client: &ClientEntry) -> Result<BTreeMap<String, serde_json::Value>> {
    let root = if client.deploy_root.is_empty() { "/" } else { client.deploy_root.as_str() };
    let out = t.exec(client, &[shunt_bin(client), "probe".into(), "--deploy-root".into(), root.into(), "--json".into()], None)?;
    let report: crate::proto::wire::ProbeReport = crate::proto::wire::parse(&out).context("remote probe reply")?;
    let mut v = serde_json::to_value(&report)?;
    let obj = v.as_object_mut().expect("ProbeReport serializes to an object");
    obj.remove("protocol");
    obj.retain(|_, x| !x.is_null());
    Ok(obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// `shunt client probe <name>`: re-collect facts, merge into the row.
/// Roundtrip rewrites the TOML; comments are lost (documented in §4.1).
pub fn probe(t: &dyn Transport, name: &str) -> Result<()> {
    let mut entry = registry::load_client(name)?;
    let facts = probe_facts(t, &entry)?;
    entry.facts.extend(facts);
    let p = registry::save_client(&entry)?;
    println!("wrote {}", p.display());
    println!("{}", serde_json::to_string_pretty(&entry.facts)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{probe_facts, ClientEntry};
    use crate::server::transport::Transport;

    /// Transport stub: answers any exec with a canned probe report
    /// and asserts the call really went to `shunt probe --json`.
    struct FakeProbe(String);
    impl Transport for FakeProbe {
        fn exec(&self, _c: &ClientEntry, argv: &[String], _stdin: Option<&str>) -> anyhow::Result<String> {
            assert!(argv.iter().any(|a| a == "probe"), "argv: {argv:?}");
            assert!(argv.iter().any(|a| a == "--json"), "argv: {argv:?}");
            Ok(self.0.clone())
        }
        fn copy_to(&self, _c: &ClientEntry, _l: &std::path::Path, _r: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    fn entry() -> ClientEntry {
        ClientEntry { name: "b".into(), local: false, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: Default::default(), qroot: "/q".into(), deploy_root: "/data".into(), facts: Default::default(), billing: None }
    }

    #[test]
    fn probe_facts_parses_wire_report() {
        let t = FakeProbe(r#"{"protocol":1,"gpu":"RTX 4090","driver":"535.54.03","cuda":"12.2","cores":14,"mem_gb":56.0,"disk_free_gb":118.0,"python":"3.10.12","can_root":true,"shutdown_cmd":"systemctl poweroff"}"#.into());
        let f = probe_facts(&t, &entry()).unwrap();
        assert_eq!(f["gpu"], "RTX 4090");
        assert_eq!(f["driver"], "535.54.03");
        assert_eq!(f["cuda"], "12.2");
        assert_eq!(f["cores"], 14);
        assert_eq!(f["mem_gb"], 56.0);
        assert_eq!(f["disk_free_gb"], 118.0);
        assert_eq!(f["python"], "3.10.12");
        assert_eq!(f["can_root"], true);
        assert_eq!(f["shutdown_cmd"], "systemctl poweroff");
        assert!(!f.contains_key("protocol"));
    }

    #[test]
    fn probe_facts_drops_nulls_and_rejects_drift() {
        // CPU-only boot: nulls are dropped, not stored as JSON null.
        let t = FakeProbe(r#"{"protocol":1,"cores":8,"gpu":null,"cuda":null,"can_root":false}"#.into());
        let f = probe_facts(&t, &entry()).unwrap();
        assert_eq!(f["cores"], 8);
        assert!(!f.contains_key("gpu"));
        assert_eq!(f["can_root"], false);
        // A stale remote binary is named drift, not a parse error.
        let bad = FakeProbe(r#"{"protocol":2,"cores":8}"#.into());
        assert!(probe_facts(&bad, &entry()).is_err());
    }
}
