//! End-to-end cycles between simulated devices with real vault folders,
//! application databases, and folder or in-memory storage.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use serde_json::json;
use tempfile::TempDir;

use super::*;
use crate::sync::{
    backends::{folder::FolderBackend, memory::MemoryBackend},
    config::{BackendConfig, PersistedConfig, SyncOptions},
    crypto::{self, Keys},
    plugin_data::fake::FakePlugins,
    scan::is_excluded,
};

struct NoLock;

impl VaultLock for NoLock {
    fn with_vault_lock(&self, operation: &mut dyn FnMut() -> AppResult<()>) -> AppResult<()> {
        operation()
    }
}

/// Creates the repository header in `backend` and returns its keys.
fn repository(backend: &dyn Backend) -> Keys {
    let (header, master) = crypto::create_header("synthetic sync passphrase").unwrap();
    backend
        .put(
            "repository.json",
            &crypto::serialize_header(&header).unwrap(),
            PutMode::CreateOnly,
        )
        .unwrap();
    Keys::derive(&master, &header.repository_id).unwrap()
}

struct Device {
    name: String,
    directory: TempDir,
    store: Store,
    app_db: PathBuf,
    plugins: FakePlugins,
}

impl Device {
    fn new(name: &str, keys: &Keys) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("sync")).unwrap();
        let app_db = directory.path().join("denote.sqlite3");
        db::initialize(&app_db).unwrap();
        initialize_store(
            &store,
            &PersistedConfig {
                plugin_id: "denote.sync".into(),
                repository_id: keys.repository_id().into(),
                backend: BackendConfig::Folder {
                    path: "/synthetic".into(),
                },
                device_name: name.into(),
                remember_passphrase: false,
                options: SyncOptions::default(),
            },
        )
        .unwrap();
        Self {
            name: name.into(),
            directory,
            store,
            app_db,
            plugins: FakePlugins::default(),
        }
    }

    /// Creates a local vault folder and registers it like opening it would.
    fn add_vault(&self, folder: &str) -> (PathBuf, String) {
        let root = self.directory.path().join(folder);
        fs::create_dir_all(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let connection = db::open(&self.app_db).unwrap();
        let id = db::ensure_vault(&connection, &root.to_string_lossy(), folder).unwrap();
        (root, db::plugin_scope_id(&connection, id).unwrap())
    }

    fn map(&self, sync_id: &str, scope_id: &str) {
        let connection = self.store.connection().unwrap();
        store::upsert_vault(
            &connection,
            &VaultRow {
                sync_id: sync_id.into(),
                scope_id: Some(scope_id.into()),
                name: String::new(),
                state: VaultState::Synced,
                paused: false,
                message: None,
            },
        )
        .unwrap();
    }

    fn try_sync(
        &self,
        backend: &dyn Backend,
        keys: &Keys,
        cancel: bool,
    ) -> (AppResult<()>, CycleReport) {
        let cancel = AtomicBool::new(cancel);
        let staging = self.directory.path().join("sync/staging");
        let engine = Engine {
            store: &self.store,
            backend,
            keys,
            app_db: &self.app_db,
            plugins: Some(&self.plugins),
            vault_lock: &NoLock,
            staging: &staging,
            cancel: &cancel,
            progress: &|_, _, _| {},
            app_version: "test",
        };
        let mut report = CycleReport::default();
        let result = engine.run(&mut report);
        (result, report)
    }

    fn sync(&self, backend: &dyn Backend, keys: &Keys) -> CycleReport {
        let (result, report) = self.try_sync(backend, keys, false);
        if let Err(error) = result {
            panic!("{} failed to sync: {error}; {report:?}", self.name);
        }
        report
    }

    fn identity(&self) -> Identity {
        store::get_meta(&self.store.connection().unwrap(), store::META_IDENTITY)
            .unwrap()
            .unwrap()
    }

    fn conflicts(&self) -> Vec<ConflictRow> {
        store::list_conflicts(&self.store.connection().unwrap()).unwrap()
    }
}

/// Every synced file of a vault, by relative path.
fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    for item in walkdir::WalkDir::new(root).follow_links(false) {
        let item = item.unwrap();
        if !item.file_type().is_file() {
            continue;
        }
        let relative = item
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if !is_excluded(&relative) {
            files.insert(relative, fs::read(item.path()).unwrap());
        }
    }
    files
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn conflict_copies(files: &BTreeMap<String, Vec<u8>>) -> Vec<&String> {
    files
        .keys()
        .filter(|path| path.contains(".sync-conflict-"))
        .collect()
}

const VAULT: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn two_devices_sync_creates_edits_deletes_and_renames_through_a_folder() {
    let remote = tempfile::tempdir().unwrap();
    let backend = FolderBackend::open(remote.path()).unwrap();
    let keys = repository(&backend);
    let laptop = Device::new("Laptop", &keys);
    let desk = Device::new("Desk", &keys);
    let (laptop_root, laptop_scope) = laptop.add_vault("Notes");
    let (desk_root, desk_scope) = desk.add_vault("Notes copy");
    write(&laptop_root, "a.md", "alpha");
    write(&laptop_root, "sub/b.md", "beta");
    write(&laptop_root, ".git/config", "[core]");
    write(&laptop_root, "repo/.git", "gitdir: elsewhere");
    write(&laptop_root, ".denote/trash/1/old.md", "trashed");
    write(&laptop_root, ".denote/note-comments.json", "{}");
    laptop.map(VAULT, &laptop_scope);
    desk.map(VAULT, &desk_scope);

    let first = laptop.sync(&backend, &keys);
    assert_eq!(first.published, Some(1));
    assert!(first.uploaded >= 3);
    let received = desk.sync(&backend, &keys);
    assert_eq!(received.applied, 3);
    assert_eq!(received.vault_roots.len(), 1);
    assert_eq!(tree(&desk_root), tree(&laptop_root));
    assert!(!desk_root.join(".git").exists() && !desk_root.join("repo/.git").exists());
    assert!(!desk_root.join(".denote/trash").exists());

    write(&desk_root, "a.md", "alpha edited on desk");
    fs::remove_file(desk_root.join("sub/b.md")).unwrap();
    write(&desk_root, "c.md", "gamma");
    desk.sync(&backend, &keys);
    laptop.sync(&backend, &keys);
    assert_eq!(
        fs::read_to_string(laptop_root.join("a.md")).unwrap(),
        "alpha edited on desk"
    );
    assert!(!laptop_root.join("sub").exists());
    assert_eq!(tree(&desk_root), tree(&laptop_root));

    fs::rename(laptop_root.join("c.md"), laptop_root.join("d.md")).unwrap();
    laptop.sync(&backend, &keys);
    desk.sync(&backend, &keys);
    assert!(!desk_root.join("c.md").exists());
    assert_eq!(fs::read_to_string(desk_root.join("d.md")).unwrap(), "gamma");
    assert_eq!(tree(&desk_root), tree(&laptop_root));

    // Settled devices publish nothing new.
    for device in [&laptop, &desk, &laptop, &desk] {
        let report = device.sync(&backend, &keys);
        assert_eq!(report.published, None, "{} republished", device.name);
        assert_eq!(report.applied, 0);
    }
}

#[test]
fn concurrent_edits_leave_one_identical_conflict_copy_on_every_device() {
    let backend = MemoryBackend::default();
    let keys = repository(&backend);
    let devices: Vec<Device> = ["A", "B", "C"]
        .iter()
        .map(|name| Device::new(name, &keys))
        .collect();
    let roots: Vec<PathBuf> = devices
        .iter()
        .map(|device| {
            let (root, scope) = device.add_vault("Notes");
            device.map(VAULT, &scope);
            root
        })
        .collect();
    write(&roots[0], "n.md", "base");
    for device in &devices {
        device.sync(&backend, &keys);
    }
    write(&roots[0], "n.md", "edited on A");
    write(&roots[1], "n.md", "edited on B, a little later");
    for _ in 0..2 {
        for device in &devices {
            device.sync(&backend, &keys);
        }
    }
    let reference = tree(&roots[0]);
    for root in &roots[1..] {
        assert_eq!(tree(root), reference);
    }
    assert_eq!(reference.len(), 2, "{reference:?}");
    assert_eq!(conflict_copies(&reference).len(), 1);
    let contents: Vec<&[u8]> = reference.values().map(Vec::as_slice).collect();
    assert!(contents.contains(&&b"edited on A"[..]));
    assert!(contents.contains(&&b"edited on B, a little later"[..]));
    for device in &devices {
        assert_eq!(device.sync(&backend, &keys).published, None);
    }

    // Editing the conflict copy afterwards is an ordinary change.
    let copy = conflict_copies(&reference)[0].clone();
    write(&roots[2], &copy, "merged by hand");
    fs::remove_file(roots[2].join("n.md")).unwrap();
    for _ in 0..2 {
        for device in &devices {
            device.sync(&backend, &keys);
        }
    }
    for root in &roots {
        assert_eq!(
            tree(root),
            BTreeMap::from([(copy.clone(), b"merged by hand".to_vec())])
        );
    }
}

#[test]
fn three_devices_converge_after_interleaved_changes_without_ping_pong() {
    let backend = MemoryBackend::default();
    let keys = repository(&backend);
    let devices: Vec<Device> = ["A", "B", "C"]
        .iter()
        .map(|name| Device::new(name, &keys))
        .collect();
    let roots: Vec<PathBuf> = devices
        .iter()
        .map(|device| {
            let (root, scope) = device.add_vault("Notes");
            device.map(VAULT, &scope);
            root
        })
        .collect();
    let mut seed = 17u64;
    let mut next = |limit: u64| {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) % limit
    };
    for round in 0..6 {
        for (index, root) in roots.iter().enumerate() {
            for _ in 0..3 {
                let file = format!("f{}.md", next(5));
                match next(4) {
                    0 => {
                        let _ = fs::remove_file(root.join(&file));
                    }
                    _ => write(root, &file, &format!("round {round} device {index}")),
                }
            }
        }
        let order = next(3) as usize;
        for offset in 0..3 {
            devices[(order + offset) % 3].sync(&backend, &keys);
        }
    }
    for _ in 0..2 {
        for device in &devices {
            device.sync(&backend, &keys);
        }
    }
    let reference = tree(&roots[0]);
    for root in &roots[1..] {
        assert_eq!(tree(root), reference);
    }
    for device in &devices {
        let report = device.sync(&backend, &keys);
        assert_eq!(report.published, None, "{} republished", device.name);
        assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    }
}

#[test]
fn missing_roots_never_delete_and_rollbacks_are_refused() {
    let backend = MemoryBackend::default();
    let keys = repository(&backend);
    let laptop = Device::new("Laptop", &keys);
    let desk = Device::new("Desk", &keys);
    let (laptop_root, laptop_scope) = laptop.add_vault("Notes");
    let (desk_root, desk_scope) = desk.add_vault("Notes");
    write(&laptop_root, "keep.md", "important");
    laptop.map(VAULT, &laptop_scope);
    desk.map(VAULT, &desk_scope);
    laptop.sync(&backend, &keys);
    desk.sync(&backend, &keys);

    // An unplugged drive is skipped, never published as deletions.
    let moved = laptop_root.with_file_name("Notes unplugged");
    fs::rename(&laptop_root, &moved).unwrap();
    let report = laptop.sync(&backend, &keys);
    assert_eq!(report.published, None);
    desk.sync(&backend, &keys);
    assert_eq!(
        fs::read_to_string(desk_root.join("keep.md")).unwrap(),
        "important"
    );
    fs::rename(&moved, &laptop_root).unwrap();

    write(&laptop_root, "keep.md", "important, revised");
    laptop.sync(&backend, &keys);
    desk.sync(&backend, &keys);
    let laptop_writer = laptop.identity().writer_id;
    backend.remove_raw(&head_key(&laptop_writer, 2));
    let report = desk.sync(&backend, &keys);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("older state")),
        "{:?}",
        report.warnings
    );
    assert_eq!(
        fs::read_to_string(desk_root.join("keep.md")).unwrap(),
        "important, revised"
    );
}

#[test]
fn missing_objects_fall_back_to_the_previous_complete_generation() {
    let backend = MemoryBackend::default();
    let keys = repository(&backend);
    let laptop = Device::new("Laptop", &keys);
    let desk = Device::new("Desk", &keys);
    let (laptop_root, laptop_scope) = laptop.add_vault("Notes");
    let (desk_root, desk_scope) = desk.add_vault("Notes");
    laptop.map(VAULT, &laptop_scope);
    desk.map(VAULT, &desk_scope);
    write(&laptop_root, "a.md", "first");
    laptop.sync(&backend, &keys);
    write(&laptop_root, "a.md", "second");
    write(&laptop_root, "b.md", "new");
    laptop.sync(&backend, &keys);
    let second = keys.object_id(b"second").unwrap();
    backend.remove_raw(&object_key(&second));

    let report = desk.sync(&backend, &keys);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("earlier complete state")),
        "{:?}",
        report.warnings
    );
    assert_eq!(
        tree(&desk_root),
        BTreeMap::from([("a.md".into(), b"first".to_vec())])
    );

    // The writer republishes the object it can still read from disk.
    write(&laptop_root, "c.md", "trigger");
    laptop.sync(&backend, &keys);
    desk.sync(&backend, &keys);
    assert_eq!(tree(&desk_root), tree(&laptop_root));
}

#[test]
fn restored_application_data_rotates_the_writer_without_losing_edits() {
    let backend = MemoryBackend::default();
    let keys = repository(&backend);
    let laptop = Device::new("Laptop", &keys);
    let desk = Device::new("Desk", &keys);
    let (laptop_root, laptop_scope) = laptop.add_vault("Notes");
    let (desk_root, desk_scope) = desk.add_vault("Notes");
    laptop.map(VAULT, &laptop_scope);
    desk.map(VAULT, &desk_scope);
    write(&laptop_root, "a.md", "original");
    laptop.sync(&backend, &keys);
    desk.sync(&backend, &keys);

    let sync_dir = laptop.directory.path().join("sync");
    let backup = laptop.directory.path().join("sync-backup");
    fs::create_dir(&backup).unwrap();
    for item in fs::read_dir(&sync_dir).unwrap() {
        let item = item.unwrap();
        if item.file_type().unwrap().is_file() {
            fs::copy(item.path(), backup.join(item.file_name())).unwrap();
        }
    }
    let old_writer = laptop.identity().writer_id;
    write(&laptop_root, "a.md", "edited after the backup");
    laptop.sync(&backend, &keys);
    desk.sync(&backend, &keys);

    for item in fs::read_dir(&sync_dir).unwrap() {
        let item = item.unwrap();
        if item.file_type().unwrap().is_file() {
            fs::remove_file(item.path()).unwrap();
        }
    }
    for item in fs::read_dir(&backup).unwrap() {
        let item = item.unwrap();
        fs::copy(item.path(), sync_dir.join(item.file_name())).unwrap();
    }
    write(&laptop_root, "a.md", "edited after the restore");
    let report = laptop.sync(&backend, &keys);
    assert!(report.rotated);
    assert_ne!(laptop.identity().writer_id, old_writer);
    desk.sync(&backend, &keys);
    laptop.sync(&backend, &keys);
    desk.sync(&backend, &keys);
    let files = tree(&desk_root);
    assert_eq!(files, tree(&laptop_root));
    let contents: Vec<&[u8]> = files.values().map(Vec::as_slice).collect();
    assert!(
        contents.contains(&&b"edited after the restore"[..]),
        "{files:?}"
    );
}

#[test]
fn metadata_preferences_and_plugin_data_follow_their_merge_rules() {
    let backend = MemoryBackend::default();
    let keys = repository(&backend);
    let laptop = Device::new("Laptop", &keys);
    let desk = Device::new("Desk", &keys);
    let (laptop_root, laptop_scope) = laptop.add_vault("Notes");
    let (desk_root, desk_scope) = desk.add_vault("Notes");
    laptop.map(VAULT, &laptop_scope);
    desk.map(VAULT, &desk_scope);
    write(&laptop_root, "a.md", "alpha");
    {
        let connection = db::open(&laptop.app_db).unwrap();
        let id = metadata::vault_by_scope(&connection, &laptop_scope)
            .unwrap()
            .unwrap()
            .id;
        db::set_bookmark(&connection, id, "a.md", true).unwrap();
        db::set_tag_color(&connection, id, "area/sub", "#123456").unwrap();
        let store = laptop.store.connection().unwrap();
        store::set_preference(&store, "denote-theme", Some("dark"), 1).unwrap();
        store::set_preference(&store, "denote-sidebar-width", Some("300"), 1).unwrap();
    }
    for device in [&laptop, &desk] {
        device.plugins.enable("denote.sample", 1);
    }
    laptop.plugins.enable("denote.extra", 0);
    laptop
        .plugins
        .set_setting("denote.sample", "fontSize", json!(18));
    laptop
        .plugins
        .set_setting("denote.sample", "toolPath", json!("/local/only"));
    laptop.plugins.set_state("denote.sample", "count", json!(1));
    laptop.sync(&backend, &keys);
    let report = desk.sync(&backend, &keys);

    assert_eq!(tree(&desk_root), tree(&laptop_root));
    assert_eq!(
        report.preferences,
        BTreeMap::from([("denote-theme".into(), Some("dark".into()))])
    );
    assert!(report.plugin_ids.contains("denote.sample"));
    let sample = desk.plugins.view("denote.sample").unwrap();
    assert_eq!(
        sample.settings,
        BTreeMap::from([("fontSize".into(), json!(18))])
    );
    assert_eq!(sample.state, BTreeMap::from([("count".into(), json!(1))]));
    assert!(
        desk.plugins.view("denote.extra").is_none(),
        "never enabled remotely"
    );
    let suggestions = store::list_suggestions(&desk.store.connection().unwrap()).unwrap();
    assert_eq!(suggestions, vec![("denote.extra".into(), "Laptop".into())]);
    {
        let connection = db::open(&desk.app_db).unwrap();
        let id = metadata::vault_by_scope(&connection, &desk_scope)
            .unwrap()
            .unwrap()
            .id;
        assert!(db::get_stats(&connection, id, "a.md").unwrap().bookmarked);
        assert_eq!(
            db::list_tag_colors(&connection, id).unwrap()[0].tag,
            "area/sub"
        );
    }

    // Concurrent plugin state keeps each device's value until someone chooses.
    laptop.plugins.set_state("denote.sample", "count", json!(2));
    desk.plugins.set_state("denote.sample", "count", json!(3));
    for _ in 0..2 {
        laptop.sync(&backend, &keys);
        desk.sync(&backend, &keys);
    }
    assert_eq!(
        laptop.plugins.view("denote.sample").unwrap().state["count"],
        json!(2)
    );
    assert_eq!(
        desk.plugins.view("denote.sample").unwrap().state["count"],
        json!(3)
    );
    let conflict = laptop.conflicts().remove(0);
    assert_eq!(conflict.kind, ConflictKind::PluginState);
    assert_eq!(conflict.remote_device, "Desk");
    assert_eq!(desk.conflicts().len(), 1);
    assert_eq!(
        laptop.sync(&backend, &keys).published,
        None,
        "a pending conflict is stable"
    );

    resolve_conflict(&laptop.store, &conflict.id, true).unwrap();
    for _ in 0..2 {
        laptop.sync(&backend, &keys);
        desk.sync(&backend, &keys);
    }
    for device in [&laptop, &desk] {
        assert_eq!(
            device.plugins.view("denote.sample").unwrap().state["count"],
            json!(3)
        );
        assert!(
            device.conflicts().is_empty(),
            "{} {:?}",
            device.name,
            device.conflicts()
        );
    }

    // Enabling the suggested plugin clears the suggestion.
    desk.plugins.enable("denote.extra", 0);
    desk.sync(&backend, &keys);
    assert!(
        store::list_suggestions(&desk.store.connection().unwrap())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn concurrent_encryption_manifest_changes_pause_the_vault_until_resolved() {
    let backend = MemoryBackend::default();
    let keys = repository(&backend);
    let laptop = Device::new("Laptop", &keys);
    let desk = Device::new("Desk", &keys);
    let (laptop_root, laptop_scope) = laptop.add_vault("Notes");
    let (desk_root, desk_scope) = desk.add_vault("Notes");
    laptop.map(VAULT, &laptop_scope);
    desk.map(VAULT, &desk_scope);
    let manifest = |salt: &str| {
        json!({
            "version": 1, "phase": "encrypted",
            "kdf": {"memory_kib": 19456, "iterations": 2, "parallelism": 1},
            "password": {"salt": salt, "nonce": "n", "ciphertext": "c"},
            "recovery": []
        })
        .to_string()
    };
    write(&laptop_root, ".denote/encryption.json", &manifest("one"));
    laptop.sync(&backend, &keys);
    desk.sync(&backend, &keys);
    write(&laptop_root, ".denote/encryption.json", &manifest("laptop"));
    write(&desk_root, ".denote/encryption.json", &manifest("desk"));
    write(&desk_root, "later.md", "held back");
    desk.sync(&backend, &keys);
    laptop.sync(&backend, &keys);
    assert!(
        !laptop_root.join("later.md").exists(),
        "nothing applies while paused"
    );
    let paused = store::list_vaults(&laptop.store.connection().unwrap()).unwrap();
    assert!(paused[0].paused);
    let conflict = laptop.conflicts().remove(0);
    assert_eq!(conflict.kind, ConflictKind::EncryptionManifest);

    resolve_conflict(&laptop.store, &conflict.id, false).unwrap();
    for _ in 0..2 {
        laptop.sync(&backend, &keys);
        desk.sync(&backend, &keys);
    }
    assert!(!store::list_vaults(&laptop.store.connection().unwrap()).unwrap()[0].paused);
    assert_eq!(tree(&desk_root), tree(&laptop_root));
    assert!(
        fs::read_to_string(desk_root.join(".denote/encryption.json"))
            .unwrap()
            .contains("laptop")
    );
}

#[test]
fn cancelled_cycles_commit_nothing_and_unknown_entries_are_carried() {
    let backend = MemoryBackend::default();
    let keys = repository(&backend);
    let laptop = Device::new("Laptop", &keys);
    let (root, scope) = laptop.add_vault("Notes");
    laptop.map(VAULT, &scope);
    write(&root, "a.md", "alpha");
    let (result, _) = laptop.try_sync(&backend, &keys, true);
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    assert!(
        store::load_index(&laptop.store.connection().unwrap())
            .unwrap()
            .is_empty()
    );
    assert_eq!(laptop.identity().generation, 0);

    // A newer client's entry kind survives an older client's republish.
    let writer = "f".repeat(32);
    let future = Entry {
        value: EntryValue::Inline {
            json: json!({"from": "the future"}),
        },
        modified_at: 1,
        origin: writer.clone(),
        vv: BTreeMap::from([(writer.clone(), 1)]),
    };
    let snapshot = Snapshot {
        format: FORMAT_VERSION,
        repository: keys.repository_id().into(),
        writer: writer.clone(),
        generation: 1,
        created_at: db::now(),
        app_version: "future".into(),
        device_name: "Future".into(),
        vaults: BTreeMap::new(),
        entries: BTreeMap::from([("calendar/2027/item".to_string(), future.clone())]),
    };
    backend
        .put(
            &head_key(&writer, 1),
            &keys
                .seal_head(&writer, 1, &snapshot.encode().unwrap())
                .unwrap(),
            PutMode::CreateOnly,
        )
        .unwrap();
    let report = laptop.sync(&backend, &keys);
    let generation = report.published.unwrap();
    let own = laptop.identity().writer_id;
    let published = Snapshot::decode(
        &keys
            .open_head(
                &own,
                generation,
                &backend.raw(&head_key(&own, generation)).unwrap(),
            )
            .unwrap(),
        keys.repository_id(),
        &own,
        generation,
    )
    .unwrap();
    assert_eq!(published.entries["calendar/2027/item"], future);
    assert_eq!(published.vaults[VAULT], "Notes");
    assert!(published.entries.contains_key(&file_key(VAULT, "a.md")));
}
