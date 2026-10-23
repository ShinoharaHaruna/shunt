use super::compact::split_index;
use super::store::initialize_room;
use crate::server::fsutil::tmp_sibling;
use std::fs;

#[test]
fn split_index_weights_bytes_not_count() {
    // A fat first message pulls the cut early: count-80% of 4
    // would land on the last index, byte-80% lands on index 1.
    assert_eq!(split_index(&[50, 10, 10, 10], 0.8), 1);
    // An exact byte hit splits there.
    assert_eq!(split_index(&[10, 10, 10, 10, 10], 0.8), 3);
    // Equal distance to the target takes the earlier index.
    assert_eq!(split_index(&[10, 10], 0.75), 0);
    // A single message can only split at itself.
    assert_eq!(split_index(&[100], 0.5), 0);
}

#[test]
fn initialization_requires_exclusive_directory_ownership() {
    let dir = std::env::temp_dir().join(format!("shunt-room-exclusive-{}", std::process::id()));
    let _ = fs::remove_dir(&dir);
    fs::create_dir(&dir).unwrap();
    let mut initialized = false;
    let result = initialize_room(&dir, |_| {
        initialized = true;
        Ok(())
    });
    fs::remove_dir(&dir).unwrap();
    assert!(!initialized, "an existing directory must never be initialized");
    assert!(result.is_err());
}

#[test]
fn failed_initialization_cleans_only_its_own_directory() {
    let dir = std::env::temp_dir().join(format!("shunt-room-failure-{}", std::process::id()));
    let moved = dir.with_extension("original");
    let result = initialize_room(&dir, |dir| {
        fs::create_dir(dir.join("msgs"))?;
        fs::create_dir(dir.join("readers"))?;
        fs::write(tmp_sibling(&dir.join("meta.json")), b"partial")?;
        anyhow::bail!("injected initialization failure")
    });
    assert!(result.is_err());
    assert!(!dir.exists(), "failed initialization must permit retry");
    let result = initialize_room(&dir, |dir| {
        fs::rename(dir, &moved)?;
        fs::create_dir(dir)?;
        anyhow::bail!("injected replacement")
    });
    assert!(result.is_err());
    assert!(dir.is_dir(), "another owner's empty directory must survive");
    fs::remove_dir(dir).unwrap();
    fs::remove_dir(moved).unwrap();
}
