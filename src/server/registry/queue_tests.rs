//! Isolated regressions for base rows, execution overlays, and named queues.

use super::tests::HOME_LOCK;
use super::*;
use std::ffi::OsString;
use std::path::PathBuf;

struct RegistryHome {
    path: PathBuf,
    previous: Option<OsString>,
}

impl RegistryHome {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("shunt-queues-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        let previous = std::env::var_os("SHUNT_HOME");
        std::env::set_var("SHUNT_HOME", &path);
        Self { path, previous }
    }

    fn row_text(&self) -> String {
        std::fs::read_to_string(self.path.join("clients/box.toml")).unwrap()
    }
}

impl Drop for RegistryHome {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous {
            std::env::set_var("SHUNT_HOME", previous);
        } else {
            std::env::remove_var("SHUNT_HOME");
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn legacy() -> ClientEntry {
    toml::from_str("name = 'box'\nqroot = '/srv/queue'\ndeploy_root = '/srv'\nenv_setup = 'export FOO=bar'\nshell = 'zsh'\nexit_on_idle = true\nexpire = 'metered'\n[ssh]\nhost = 'example.invalid'\nkey = '/placeholder/key'\n[facts]\ncores = 16\n").unwrap()
}

fn queue(qroot: &str) -> QueueEntry {
    QueueEntry { qroot: qroot.into(), ..QueueEntry::default() }
}

fn canonical() -> ClientEntry {
    let mut client = legacy();
    canonicalize_queues(&mut client).unwrap();
    client
}

#[test]
fn legacy_reads_are_virtual_and_do_not_normalize_disk() {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = RegistryHome::new("legacy");
    let original = legacy();
    save_client(&original).unwrap();
    let before = home.row_text();
    let base = load_client("box").unwrap();
    assert!(base.queues.is_empty());
    assert_eq!(base.default_queue, None);
    assert_eq!(queue_names(&base), [DEFAULT_QUEUE]);
    let selected = load_queue("box", None).unwrap();
    assert_eq!(selected.name, DEFAULT_QUEUE);
    assert_eq!(selected.client.qroot, original.qroot);
    assert_eq!(selected.client.env_setup, original.env_setup);
    assert_eq!(selected.client.shell, original.shell);
    assert!(selected.client.exit_on_idle);
    assert_eq!(selected.workers, None);
    assert_eq!(all_queues(&base).unwrap().len(), 1);
    assert!(resolve_queue(&base, Some("missing")).unwrap_err().to_string().contains("unknown queue"));
    assert_eq!(home.row_text(), before);
}

#[test]
fn canonicalization_preserves_queue_and_machine_identity_and_is_idempotent() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _home = RegistryHome::new("normalize");
    let mut client = legacy();
    let original = toml::to_string(&client).unwrap();
    canonicalize_queues(&mut client).unwrap();
    assert!(client.qroot.is_empty() && client.env_setup.is_empty() && client.shell.is_empty());
    assert!(!client.exit_on_idle);
    assert_eq!(client.default_queue.as_deref(), Some(DEFAULT_QUEUE));
    let default = &client.queues[DEFAULT_QUEUE];
    assert_eq!(default.qroot, "/srv/queue");
    assert_eq!(default.env_setup, "export FOO=bar");
    assert_eq!(default.shell, "zsh");
    assert!(default.exit_on_idle);
    assert_eq!(default.workers, None);
    let once = toml::to_string(&client).unwrap();
    canonicalize_queues(&mut client).unwrap();
    assert_eq!(toml::to_string(&client).unwrap(), once);
    save_client(&client).unwrap();
    let selected = load_queue("box", Some(DEFAULT_QUEUE)).unwrap();
    assert_eq!(toml::to_string(&ClientEntry { queues: Default::default(), default_queue: None, ..selected.client }).unwrap(), original);
    let base = load_client("box").unwrap();
    assert!(base.qroot.is_empty());
    assert_eq!(base.ssh.key, "/placeholder/key");
    assert_eq!(base.facts["cores"], 16);
}

#[test]
fn selection_precedence_ambiguity_and_all_queues() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _home = RegistryHome::new("selection");
    let mut client = canonical();
    client.queues.insert("training".into(), QueueEntry { qroot: "/srv/training".into(), env_setup: "export TRAIN=1".into(), shell: "bash".into(), exit_on_idle: false, workers: Some(3) });
    assert_eq!(resolve_queue(&client, None).unwrap().name, DEFAULT_QUEUE);
    let selected = resolve_queue(&client, Some("training")).unwrap();
    assert_eq!(selected.name, "training");
    assert_eq!(selected.workers, Some(3));
    assert_eq!(selected.client.qroot, "/srv/training");
    assert_eq!(selected.client.env_setup, "export TRAIN=1");
    assert_eq!(selected.client.shell, "bash");
    assert!(!selected.client.exit_on_idle);
    assert_eq!(selected.client.queues.len(), 2);
    client.default_queue = Some("training".into());
    assert_eq!(resolve_queue(&client, None).unwrap().name, "training");
    assert_eq!(resolve_queue(&client, Some(DEFAULT_QUEUE)).unwrap().client.qroot, "/srv/queue");
    client.default_queue = None;
    let error = resolve_queue(&client, None).unwrap_err().to_string();
    assert!(error.contains("ambiguous") && error.contains("--queue"), "{error}");
    let all = all_queues(&client).unwrap();
    assert_eq!(all.iter().map(|resolved| resolved.name.as_str()).collect::<Vec<_>>(), [DEFAULT_QUEUE, "training"]);
    assert!(client.qroot.is_empty());
    client.queues.remove(DEFAULT_QUEUE);
    assert_eq!(resolve_queue(&client, None).unwrap().name, "training");
}

#[test]
fn dangling_defaults_and_unknown_or_invalid_names_fail_loudly() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _home = RegistryHome::new("names");
    let mut client = canonical();
    let error = resolve_queue(&client, Some("unknown")).unwrap_err().to_string();
    assert!(error.contains("unknown queue") && error.contains("shunt queue ls"), "{error}");
    for name in ["", "..", "-flag", "a/b", "a b"] {
        assert!(resolve_queue(&client, Some(name)).is_err());
        client.queues.insert(name.into(), queue("/srv/other"));
        assert!(all_queues(&client).is_err(), "{name}");
        client.queues.remove(name);
    }
    client.default_queue = Some("unknown".into());
    let error = resolve_queue(&client, Some(DEFAULT_QUEUE)).unwrap_err().to_string();
    assert!(error.contains("default_queue") && error.contains("not registered"), "{error}");
    save_client(&client).unwrap();
    assert!(load_client("box").is_err());
    let mut old = legacy();
    old.default_queue = Some("other".into());
    assert!(resolve_queue(&old, None).is_err());
}

#[test]
fn canonical_paths_reject_empty_relative_whitespace_parent_and_lexical_overlap() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _home = RegistryHome::new("paths");
    for root in ["", "relative", " /srv/other", "/srv/other ", "/srv/../other", "/srv/queue", "/srv/queue/child", "/srv", "/srv//queue/./", "/"] {
        let mut client = canonical();
        client.queues.insert("other".into(), queue(root));
        assert!(all_queues(&client).is_err(), "invalid or overlapping root {root:?}");
        save_client(&client).unwrap();
        assert!(load_client("box").is_err(), "all queues must validate on load: {root:?}");
    }
    let mut client = canonical();
    client.queues.insert("other".into(), queue("/srv/queue-other"));
    assert!(all_queues(&client).is_ok(), "prefixes without a path-component boundary do not overlap");
    client.queues.get_mut(DEFAULT_QUEUE).unwrap().qroot = "/srv/./queue//".into();
    client.queues.get_mut("other").unwrap().qroot = "/srv/queue/child".into();
    assert!(all_queues(&client).is_err());
}

#[test]
fn mixed_rows_and_resolved_views_cannot_be_saved() {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = RegistryHome::new("mixed");
    let client = canonical();
    save_client(&client).unwrap();
    let before = home.row_text();
    let selected = resolve_queue(&client, None).unwrap();
    assert!(save_client(&selected.client).unwrap_err().to_string().contains("resolved execution view"));
    assert_eq!(home.row_text(), before);
    for field in ["qroot", "env_setup", "shell", "exit_on_idle"] {
        let mut mixed = client.clone();
        match field {
            "qroot" => mixed.qroot = "/legacy".into(),
            "env_setup" => mixed.env_setup = "true".into(),
            "shell" => mixed.shell = "sh".into(),
            _ => mixed.exit_on_idle = true,
        }
        assert!(save_client(&mixed).is_err(), "{field}");
        assert!(resolve_queue(&mixed, None).is_err(), "{field}");
        assert!(canonicalize_queues(&mut mixed).is_err(), "{field}");
        std::fs::write(home.path.join("clients/box.toml"), toml::to_string(&mixed).unwrap()).unwrap();
        assert!(load_client("box").is_err(), "hand-edited mixed row: {field}");
    }
}

#[test]
fn queue_mutations_preserve_legacy_default_and_guard_removal() {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = RegistryHome::new("mutations");
    save_client(&legacy()).unwrap();
    add_queue("box", "training", QueueEntry { workers: Some(4), ..queue("/srv/training") }).unwrap();
    let base = load_client("box").unwrap();
    assert!(base.qroot.is_empty());
    assert_eq!(base.default_queue.as_deref(), Some(DEFAULT_QUEUE));
    assert_eq!(base.queues[DEFAULT_QUEUE].qroot, "/srv/queue");
    assert_eq!(base.queues[DEFAULT_QUEUE].shell, "zsh");
    assert!(base.queues[DEFAULT_QUEUE].exit_on_idle);
    assert_eq!(base.ssh.key, legacy().ssh.key);
    assert_eq!(base.facts, legacy().facts);
    let before = home.row_text();
    assert!(add_queue("box", "training", queue("/different")).is_err());
    assert!(add_queue("box", "nested", queue("/srv/queue/nested")).is_err());
    assert!(add_queue("box", "bad/name", queue("/bad")).is_err());
    assert!(set_default_queue("box", "unknown").is_err());
    assert!(remove_queue_config("box", DEFAULT_QUEUE).unwrap_err().to_string().contains("configured default"));
    assert!(remove_queue_config("box", "missing").is_err());
    assert_eq!(home.row_text(), before, "failed mutations must not persist normalization or partial changes");
    set_default_queue("box", "training").unwrap();
    assert_eq!(load_queue("box", None).unwrap().name, "training");
    assert_eq!(load_queue("box", Some(DEFAULT_QUEUE)).unwrap().client.qroot, "/srv/queue");
    remove_queue_config("box", DEFAULT_QUEUE).unwrap();
    assert_eq!(queue_names(&load_client("box").unwrap()), ["training"]);
    assert!(remove_queue_config("box", "training").unwrap_err().to_string().contains("last queue"));
    assert!(load_queue("box", Some(DEFAULT_QUEUE)).is_err(), "a removed literal default must not redirect to training");
}

#[test]
fn queue_set_patches_only_passed_fields_and_writes_only_on_change() {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = RegistryHome::new("queue-set");
    save_client(&legacy()).unwrap();
    let patch = |exit_on_idle, workers| QueuePatch { exit_on_idle, workers, ..QueuePatch::default() };

    // Single-field patch: exit_on_idle flips, every sibling survives.
    let out = set_queue_fields("box", "default", patch(Some(false), None)).unwrap();
    assert_eq!(out.queue, "default");
    assert_eq!(out.changed, ["exit_on_idle: -> false"]);
    assert!(out.path.is_some());
    let stored = load_client("box").unwrap();
    let q = &stored.queues["default"];
    assert_eq!(q.qroot, "/srv/queue");
    assert_eq!(q.env_setup, "export FOO=bar");
    assert_eq!(q.shell, "zsh");
    assert!(!q.exit_on_idle);

    // A no-diff call is honest: no save, file byte-identical.
    let before = home.row_text();
    let out = set_queue_fields("box", "default", patch(Some(false), None)).unwrap();
    assert!(out.changed.is_empty() && out.path.is_none());
    assert_eq!(home.row_text(), before);

    // workers: set then --no-workers clears back to runtime-managed.
    let out = set_queue_fields("box", "default", patch(None, Some(Some(3)))).unwrap();
    assert_eq!(out.changed, ["workers: -> 3"]);
    let out = set_queue_fields("box", "default", patch(None, Some(None))).unwrap();
    assert_eq!(out.changed, ["workers: cleared"]);
    assert_eq!(load_client("box").unwrap().queues["default"].workers, None);

    // Empty strings clear env_setup/shell; canonical resolution
    // (case-fold) reaches the same queue.
    let out = set_queue_fields("box", "DEFAULT", QueuePatch { env_setup: Some(String::new()), shell: Some(String::new()), ..QueuePatch::default() }).unwrap();
    assert_eq!(out.changed, ["env_setup: cleared", "shell: cleared"]);
    let q = &load_client("box").unwrap().queues["default"];
    assert!(q.env_setup.is_empty() && q.shell.is_empty());

    // Rejections leave the file byte-identical: empty patch, unknown
    // name, and a qroot the validator rejects (relative / overlap).
    let before = home.row_text();
    assert!(set_queue_fields("box", "default", QueuePatch::default()).unwrap_err().to_string().contains("nothing to update"));
    assert!(set_queue_fields("box", "missing", patch(Some(true), None)).is_err());
    assert!(set_queue_fields("box", "default", QueuePatch { qroot: Some("relative".into()), ..QueuePatch::default() }).is_err());
    assert_eq!(home.row_text(), before);
}

#[test]
fn explicit_default_mutation_normalizes_but_failures_leave_legacy_untouched() {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = RegistryHome::new("default-mutation");
    save_client(&legacy()).unwrap();
    let before = home.row_text();
    assert!(add_queue("box", DEFAULT_QUEUE, queue("/other")).is_err());
    assert!(set_default_queue("box", "missing").is_err());
    assert!(remove_queue_config("box", DEFAULT_QUEUE).is_err());
    assert_eq!(home.row_text(), before);
    set_default_queue("box", DEFAULT_QUEUE).unwrap();
    let after = home.row_text();
    assert!(after.contains("[queues.default]"));
    let stored: toml::Value = toml::from_str(&after).unwrap();
    for legacy_field in ["qroot", "env_setup", "shell", "exit_on_idle"] {
        assert!(stored.get(legacy_field).is_none(), "canonical top-level {legacy_field} must be absent");
    }
    set_default_queue("box", DEFAULT_QUEUE).unwrap();
    assert_eq!(home.row_text(), after, "setting the same default is idempotent");
    let base = load_client("box").unwrap();
    assert!(base.qroot.is_empty());
    assert_eq!(base.queues[DEFAULT_QUEUE].env_setup, legacy().env_setup);
}

#[test]
fn malformed_legacy_rows_remain_savable_for_lifecycle_and_normalization_is_atomic() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _home = RegistryHome::new("malformed");
    let mut client = legacy();
    client.qroot.clear();
    save_client(&client).unwrap();
    assert!(load_client("box").unwrap_err().to_string().contains("qroot is empty"));
    assert_eq!(list_clients().unwrap(), ["box", "localhost"]);
    assert!(load_client_row("box").is_ok());
    client.disabled = true;
    save_client(&client).unwrap();
    assert_eq!(list_clients().unwrap(), ["localhost"]);
    assert!(load_client_row("box").unwrap().disabled);
    client = legacy();
    client.qroot = "relative-legacy-root".into();
    assert!(resolve_queue(&client, None).is_ok(), "legacy paths retain compatibility until mutation");
    let before = toml::to_string(&client).unwrap();
    assert!(canonicalize_queues(&mut client).is_err());
    assert_eq!(toml::to_string(&client).unwrap(), before);
}

#[test]
fn normalization_never_relocates_queue_or_identity_material() {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = RegistryHome::new("material");
    let root = home.path.join("old-queue");
    let mut client = legacy();
    client.qroot = root.display().to_string();
    client.ssh.key = home.path.join("keys/box").display().to_string();
    let artifacts = [(root.join(".seq"), "0017\n"), (root.join(".shunt-owner"), "existing-owner\n"), (root.join("bin/shunt"), "inert binary fixture\n"), (PathBuf::from(&client.ssh.key), "inert private key fixture\n")];
    for (path, contents) in &artifacts {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    save_client(&client).unwrap();
    add_queue("box", "second", queue(&home.path.join("second-queue").display().to_string())).unwrap();
    let normalized = load_client("box").unwrap();
    assert_eq!(normalized.queues[DEFAULT_QUEUE].qroot, client.qroot);
    assert_eq!(normalized.ssh.key, client.ssh.key);
    assert!(!home.path.join("second-queue").exists(), "a config mutation must not bootstrap the new root");
    for (path, contents) in artifacts {
        assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
    }
}

#[test]
fn missing_binding_queue_is_literal_default_and_new_bindings_serialize_explicitly() {
    let _lock = HOME_LOCK.lock().unwrap();
    let _home = RegistryHome::new("bindings");
    let project: ProjectEntry = toml::from_str("name = 'project'\n[[bind]]\nclient = 'box'\nworkdir = '/work'\n").unwrap();
    assert_eq!(project.bind[0].queue, DEFAULT_QUEUE);
    assert_eq!(default_queue_name(), DEFAULT_QUEUE);
    save_project(&project).unwrap();
    let back = load_project("project").unwrap();
    assert_eq!(back.bind[0].queue, DEFAULT_QUEUE);
    assert!(toml::to_string(&back).unwrap().contains("queue = \"default\""));
    let explicit: Binding = toml::from_str("client = 'box'\nqueue = 'training'\nworkdir = '/work'\n").unwrap();
    assert_eq!(explicit.queue, "training");
}
