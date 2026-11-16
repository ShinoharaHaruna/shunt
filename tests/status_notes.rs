mod common;

use shunt::proto::wire::PROTOCOL_VERSION;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn home(tag: &str) -> PathBuf {
    let dir = common::tmp_base().join(format!("shunt-status-notes-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn shunt(home: &Path, args: &[&str]) -> Output {
    Command::new(common::bin()).args(args).env("SHUNT_HOME", home).env("SHUNT_STATE", home.join("boxstate")).env_remove("SHUNT_ROOT").output().expect("shunt run failed")
}

fn run_ok(home: &Path, args: &[&str]) -> String {
    let out = shunt(home, args);
    assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn write_row(home: &Path, name: &str, extra: &str) {
    let root = home.join(format!("{name}-queue"));
    run_ok(home, &["init", "--root", root.to_str().unwrap()]);
    let clients = home.join("clients");
    std::fs::create_dir_all(&clients).unwrap();
    std::fs::write(clients.join(format!("{name}.toml")), format!("name = \"{name}\"\nlocal = true\nlocal_bin = {:?}\nqroot = {:?}\ndeploy_root = {:?}\n{extra}", common::bin(), root.to_str().unwrap(), home.to_str().unwrap())).unwrap();
}

#[test]
fn status_client_single_queue_report_carries_notes() {
    let home = home("single");
    write_row(&home, "box", "\n[notes]\nweights = '/data/models'\nquirk = 'shared disk'\n");
    for args in [&["status", "-c", "box", "--json"][..], &["status", "-c", "box"][..]] {
        let value: serde_json::Value = serde_json::from_str(&run_ok(&home, args)).unwrap();
        assert_eq!(value["protocol"], PROTOCOL_VERSION);
        assert_eq!(value["daemon"]["alive"], false);
        assert_eq!(value["notes"]["weights"], "/data/models");
        assert_eq!(value["notes"]["quirk"], "shared disk");
    }
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn status_client_unreachable_json_carries_notes() {
    let home = home("dead");
    write_row(&home, "dead", "\n[notes]\nwhy = 'retirement candidate'\n");
    let path = home.join("clients/dead.toml");
    let mut row: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    row["local_bin"] = toml::Value::String(home.join("missing-shunt").display().to_string());
    std::fs::write(path, toml::to_string(&row).unwrap()).unwrap();

    let value: serde_json::Value = serde_json::from_str(&run_ok(&home, &["status", "-c", "dead", "--json"])).unwrap();
    assert_eq!(value["client"], "dead");
    assert_eq!(value["protocol"], PROTOCOL_VERSION);
    assert_eq!(value["reachable"], false);
    assert!(value["error"].is_string());
    assert_eq!(value["notes"]["why"], "retirement candidate");
    assert_eq!(value["queues"]["default"]["reachable"], false);

    let out = shunt(&home, &["status", "-c", "dead"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("bootstrap"));
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn status_all_json_mirrors_notes_on_every_loaded_entry() {
    let home = home("all");
    write_row(&home, "live", "\n[notes]\nweights = '/data/models'\n");
    // An invalid queue path exercises a real queue failure without SSH access.
    write_row(&home, "dead", "\n[notes]\nquirk = 'disk unavailable'\n");
    let path = home.join("clients/dead.toml");
    let mut row: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let unavailable = home.join("unavailable-queue");
    std::fs::write(&unavailable, "not a directory\n").unwrap();
    row["qroot"] = toml::Value::String(unavailable.display().to_string());
    std::fs::write(path, toml::to_string(&row).unwrap()).unwrap();
    write_row(&home, "localhost", "disabled = true\n\n[notes]\nwhy = 'reserved'\n");
    write_row(&home, "plain", "disabled = true\n");
    std::fs::write(home.join("clients/bad.toml"), "not [valid").unwrap();

    let value: serde_json::Value = serde_json::from_str(&run_ok(&home, &["status", "--all", "--json"])).unwrap();
    let clients = &value["clients"];
    assert_eq!(clients["live"]["reachable"], true);
    assert_eq!(clients["live"]["notes"]["weights"], "/data/models");
    assert_eq!(clients["live"]["report"]["daemon"]["alive"], false);
    assert_eq!(clients["dead"]["reachable"], false);
    assert_eq!(clients["dead"]["notes"]["quirk"], "disk unavailable");
    assert_eq!(clients["localhost"]["skip"]["reason"], "disabled");
    assert_eq!(clients["localhost"]["notes"]["why"], "reserved");
    assert!(clients["plain"].get("notes").is_none());
    assert!(clients["bad"]["error"].as_str().unwrap().contains("cannot load row"));
    assert!(clients["bad"].get("notes").is_none());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn notes_survive_cli_updates_and_real_probe_merges() {
    let home = home("lifecycle");
    let root = home.join("queue");
    run_ok(&home, &["init", "--root", root.to_str().unwrap()]);
    run_ok(&home, &["client", "add", "box", "--local", "--qroot", root.to_str().unwrap(), "--note", "weights=/old", "--note", "weights=/data/models", "--note", "keep=shared"]);
    let show = || -> toml::Value { toml::from_str(&run_ok(&home, &["client", "show", "box"])).unwrap() };
    assert_eq!(show()["notes"]["weights"].as_str(), Some("/data/models"));
    run_ok(&home, &["client", "probe", "box"]);
    assert_eq!(show()["notes"]["keep"].as_str(), Some("shared"));
    assert_eq!(show()["notes"]["weights"].as_str(), Some("/data/models"));
    run_ok(&home, &["client", "update", "box", "--note", "weights=/data/updated", "--note", "empty=", "--note-rm", "keep", "--note-rm", "absent"]);
    let row = show();
    assert_eq!(row["notes"]["weights"].as_str(), Some("/data/updated"));
    assert_eq!(row["notes"]["empty"].as_str(), Some(""));
    assert!(row["notes"].get("keep").is_none());
    run_ok(&home, &["client", "update", "box", "--note-rm", "absent"]);
    run_ok(&home, &["client", "update", "box", "--no-notes"]);
    assert!(show().get("notes").is_none());
    run_ok(&home, &["client", "update", "box", "--no-notes"]);
    assert!(show().get("notes").is_none());
    std::fs::remove_dir_all(home).unwrap();
}
