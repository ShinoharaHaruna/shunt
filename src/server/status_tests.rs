use super::{skipped_status, ProbeSkip};
use crate::server::registry::tests::HOME_LOCK;

#[test]
fn skipped_status_keeps_notes_and_omits_empty_map() {
    let _g = HOME_LOCK.lock().unwrap();
    let home = crate::testutil::tmp_base().join(format!("shunt-status-notes-skip-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let mut client = crate::server::client_ops::test_entry();
    client.notes.insert("weights".into(), "/data/models".into());
    let skip = ProbeSkip { reason: "disabled", fails: None, retry_in_s: None };
    let status = skipped_status(&client, skip.clone());
    assert!(!status.reachable && status.queues.is_empty());
    assert_eq!(status.notes, client.notes);
    assert_eq!(serde_json::to_value(&status).unwrap()["notes"]["weights"], "/data/models");

    client.notes.clear();
    let value = serde_json::to_value(skipped_status(&client, skip)).unwrap();
    assert!(value.get("notes").is_none(), "{value}");
    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(home).ok();
}
