//! The sync service between two simulated devices sharing a folder.

use std::{collections::BTreeMap, fs, sync::atomic::AtomicBool};

use tempfile::TempDir;

use super::*;
use crate::sync::{config::SyncBackendRequest, plugin_data::fake::FakePlugins};

const PLUGIN: &str = "denote.sync";
const PASSPHRASE: &str = "synthetic sync passphrase";

#[derive(Default)]
struct FakeSecrets(Mutex<BTreeMap<(String, String), String>>);

impl SecretStore for FakeSecrets {
    fn get(&self, plugin_id: &str, name: &str) -> AppResult<Option<Zeroizing<String>>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(&(plugin_id.into(), name.into()))
            .cloned()
            .map(Zeroizing::new))
    }

    fn set(&self, plugin_id: &str, name: &str, value: &str) -> AppResult<()> {
        self.0
            .lock()
            .unwrap()
            .insert((plugin_id.into(), name.into()), value.into());
        Ok(())
    }

    fn delete(&self, plugin_id: &str, name: &str) -> AppResult<()> {
        self.0
            .lock()
            .unwrap()
            .remove(&(plugin_id.into(), name.into()));
        Ok(())
    }
}

/// Fails every request once its operation is cancelled, like the Git transport.
struct CancelAware {
    inner: Box<dyn Backend>,
    cancel: Arc<AtomicBool>,
}

impl CancelAware {
    fn check(&self) -> AppResult<()> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(AppError::State("Synthetic storage was cancelled".into()));
        }
        Ok(())
    }
}

impl Backend for CancelAware {
    fn get(&self, key: &str) -> AppResult<Option<Vec<u8>>> {
        self.check()?;
        self.inner.get(key)
    }

    fn put(&self, key: &str, bytes: &[u8], mode: PutMode) -> AppResult<PutOutcome> {
        self.check()?;
        self.inner.put(key, bytes, mode)
    }

    fn list(&self, prefix: &str) -> AppResult<Vec<String>> {
        self.check()?;
        self.inner.list(prefix)
    }

    fn delete(&self, key: &str) -> AppResult<()> {
        self.check()?;
        self.inner.delete(key)
    }

    fn refresh(&self) -> AppResult<()> {
        self.check()?;
        self.inner.refresh()
    }

    fn flush(&self) -> AppResult<()> {
        self.check()?;
        self.inner.flush()
    }
}

struct NoLock;

impl VaultLock for NoLock {
    fn with_vault_lock(&self, operation: &mut dyn FnMut() -> AppResult<()>) -> AppResult<()> {
        operation()
    }
}

#[derive(Default)]
struct FakeEnvironment {
    denied: AtomicBool,
    secrets: FakeSecrets,
    plugins: FakePlugins,
    statuses: Mutex<Vec<SyncState>>,
    applied: Mutex<Vec<SyncAppliedEvent>>,
}

impl SyncEnvironment for FakeEnvironment {
    fn authorize(&self, plugin_id: &str) -> AppResult<()> {
        if self.denied.load(Ordering::SeqCst) || !plugin_id.starts_with("denote.") {
            return Err(AppError::Plugin("sync permission is not enabled".into()));
        }
        Ok(())
    }

    fn plugin_data(&self) -> Option<&dyn PluginDataSource> {
        Some(&self.plugins)
    }

    fn secrets(&self) -> AppResult<&dyn SecretStore> {
        Ok(&self.secrets)
    }

    fn vault_lock(&self) -> AppResult<&dyn VaultLock> {
        Ok(&NoLock)
    }

    fn backend_plugins(&self) -> Option<crate::plugins::PluginManager> {
        None
    }

    fn open_url(&self, _url: &str) -> AppResult<()> {
        Ok(())
    }

    fn emit_status(&self, status: &SyncStatusView) {
        self.statuses.lock().unwrap().push(status.state);
    }

    fn emit_applied(&self, applied: &SyncAppliedEvent) {
        self.applied.lock().unwrap().push(applied.clone());
    }

    fn app_version(&self) -> String {
        "test".into()
    }

    fn open_backend(
        &self,
        config: &BackendConfig,
        secrets: &BackendSecrets,
        context: &BackendContext,
    ) -> AppResult<Box<dyn Backend>> {
        Ok(Box::new(CancelAware {
            inner: backends::open(config, secrets, context)?,
            cancel: context.cancel.clone(),
        }))
    }
}

struct Device {
    data: TempDir,
    environment: Arc<FakeEnvironment>,
    service: SyncService,
    vault_root: PathBuf,
    scope_id: String,
}

impl Device {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let app_db = data.path().join("denote.sqlite3");
        db::initialize(&app_db).unwrap();
        let vault_root = data.path().join("Notes");
        fs::create_dir(&vault_root).unwrap();
        let vault_root = fs::canonicalize(vault_root).unwrap();
        let connection = db::open(&app_db).unwrap();
        let id = db::ensure_vault(&connection, &vault_root.to_string_lossy(), "Notes").unwrap();
        let scope_id = db::plugin_scope_id(&connection, id).unwrap();
        let environment = Arc::new(FakeEnvironment::default());
        let service = SyncService::new(data.path(), app_db, environment.clone()).unwrap();
        Self {
            data,
            environment,
            service,
            vault_root,
            scope_id,
        }
    }

    fn setup(
        &self,
        mode: SetupMode,
        storage: &Path,
        passphrase: &str,
        remember: bool,
    ) -> AppResult<SyncStatusView> {
        self.service.configure(
            PLUGIN,
            SyncSetupRequest {
                mode,
                device_name: "Device".into(),
                passphrase: passphrase.into(),
                remember_passphrase: remember,
                backend: SyncBackendRequest::Folder {
                    path: storage.to_string_lossy().into_owned(),
                },
            },
        )
    }

    /// Runs a cycle, retrying if a parallel test's global cancellation hit it.
    fn run(&self) -> SyncStatusView {
        for _ in 0..3 {
            match self.service.run_now(PLUGIN) {
                Ok(status) => return status,
                Err(error) if error.to_string().contains("cancelled") => continue,
                Err(error) => panic!("sync failed: {error}"),
            }
        }
        panic!("sync was cancelled repeatedly")
    }
}

#[test]
fn devices_set_up_join_map_sync_retire_and_disconnect() {
    let storage = tempfile::tempdir().unwrap();
    let laptop = Device::new();
    let desk = Device::new();

    let status = laptop.service.status(PLUGIN).unwrap();
    assert_eq!(status.state, SyncState::NotConfigured);
    assert_eq!(status.vaults.len(), 1);
    assert_eq!(status.vaults[0].state, VaultViewState::NotEnrolled);
    assert!(laptop.service.status("other.plugin").is_err());
    assert!(
        laptop
            .setup(SetupMode::Create, &laptop.vault_root, PASSPHRASE, true)
            .is_err(),
        "a vault cannot hold sync storage"
    );
    assert!(
        laptop
            .setup(SetupMode::Join, storage.path(), PASSPHRASE, true)
            .is_err()
    );

    let status = laptop
        .setup(SetupMode::Create, storage.path(), PASSPHRASE, true)
        .unwrap();
    assert_eq!(status.state, SyncState::Idle);
    assert!(status.remember_passphrase);
    assert!(
        laptop
            .environment
            .secrets
            .get(PLUGIN, SECRET_MASTER)
            .unwrap()
            .is_some()
    );
    assert!(
        laptop
            .setup(SetupMode::Create, storage.path(), PASSPHRASE, true)
            .is_err()
    );
    laptop
        .service
        .map_vault(
            PLUGIN,
            SyncVaultRequest::Enroll {
                scope_id: laptop.scope_id.clone(),
            },
        )
        .unwrap();
    fs::write(laptop.vault_root.join("a.md"), "alpha").unwrap();
    let status = laptop.run();
    assert!(
        status.last_run.as_ref().unwrap().ok,
        "{:?}",
        status.last_run
    );
    assert!(
        laptop
            .environment
            .statuses
            .lock()
            .unwrap()
            .contains(&SyncState::Running)
    );
    let sync_id = status.vaults[0].sync_id.clone().unwrap();
    assert_eq!(status.vaults[0].state, VaultViewState::Synced);

    assert!(
        desk.setup(
            SetupMode::Join,
            storage.path(),
            "wrong synthetic passphrase",
            false
        )
        .is_err()
    );
    desk.setup(SetupMode::Join, storage.path(), PASSPHRASE, false)
        .unwrap();
    let status = desk.run();
    let offered = status
        .vaults
        .iter()
        .find(|vault| vault.state == VaultViewState::PendingMapping)
        .unwrap();
    assert_eq!(offered.sync_id.as_deref(), Some(sync_id.as_str()));
    assert_eq!(offered.name, "Notes");
    let target = desk.data.path().join("Synced notes");
    desk.service
        .map_vault(
            PLUGIN,
            SyncVaultRequest::MapNew {
                sync_id: sync_id.clone(),
                path: target.to_string_lossy().into_owned(),
            },
        )
        .unwrap();
    desk.run();
    let target = fs::canonicalize(target).unwrap();
    assert_eq!(fs::read_to_string(target.join("a.md")).unwrap(), "alpha");
    let applied = desk.environment.applied.lock().unwrap().clone();
    assert_eq!(
        applied.last().unwrap().vault_roots,
        vec![target.to_string_lossy().into_owned()]
    );

    // Preferences are full snapshots; only changed keys become new versions.
    assert!(
        desk.service
            .put_preferences(
                PLUGIN,
                BTreeMap::from([("denote-sidebar-width".into(), Some("9".into()))])
            )
            .is_err()
    );
    desk.service
        .put_preferences(
            PLUGIN,
            BTreeMap::from([("denote-theme".into(), Some("dark".into()))]),
        )
        .unwrap();
    desk.run();
    laptop.run();
    let applied = laptop.environment.applied.lock().unwrap().clone();
    assert_eq!(
        applied.last().unwrap().preferences,
        BTreeMap::from([("denote-theme".into(), Some("dark".into()))])
    );

    // Removing the desk makes it re-enroll under a new identity on its next run.
    let desk_writer = desk
        .service
        .status(PLUGIN)
        .unwrap()
        .device
        .unwrap()
        .writer_id;
    assert!(
        laptop
            .service
            .retire_device(PLUGIN, &"0".repeat(32))
            .is_err(),
        "unknown device"
    );
    let status = laptop.service.retire_device(PLUGIN, &desk_writer).unwrap();
    assert!(
        status
            .devices
            .iter()
            .any(|device| device.writer_id == desk_writer && device.retired)
    );
    let status = desk.run();
    assert_ne!(status.device.unwrap().writer_id, desk_writer);
    assert!(
        status
            .warnings
            .iter()
            .any(|warning| warning.contains("re-enrolled"))
    );

    assert!(
        laptop
            .service
            .change_passphrase(
                PLUGIN,
                "wrong synthetic passphrase",
                "next synthetic passphrase"
            )
            .is_err()
    );
    laptop
        .service
        .change_passphrase(PLUGIN, PASSPHRASE, "next synthetic passphrase")
        .unwrap();
    assert!(desk.service.unlock(PLUGIN, PASSPHRASE, false).is_err());
    desk.service
        .unlock(PLUGIN, "next synthetic passphrase", true)
        .unwrap();

    let status = laptop.service.disconnect(PLUGIN).unwrap();
    assert_eq!(status.state, SyncState::NotConfigured);
    assert!(laptop.environment.secrets.0.lock().unwrap().is_empty());
    assert!(
        storage
            .path()
            .join("denote-sync/v1/repository.json")
            .is_file()
    );
    assert_eq!(
        fs::read_to_string(laptop.vault_root.join("a.md")).unwrap(),
        "alpha"
    );
}

#[test]
fn commands_refuse_without_permission_and_validate_options() {
    let storage = tempfile::tempdir().unwrap();
    let device = Device::new();
    device
        .setup(SetupMode::Create, storage.path(), PASSPHRASE, false)
        .unwrap();
    assert!(
        device
            .service
            .set_options(
                PLUGIN,
                SyncOptions {
                    interval_minutes: 5000,
                    ..SyncOptions::default()
                },
            )
            .is_err()
    );
    let status = device
        .service
        .set_options(
            PLUGIN,
            SyncOptions {
                paused: true,
                ..SyncOptions::default()
            },
        )
        .unwrap();
    assert_eq!(status.state, SyncState::Paused);
    assert!(
        device.service.status("denote.other").is_err(),
        "another plugin cannot take over"
    );
    device.environment.denied.store(true, Ordering::SeqCst);
    for result in [
        device.service.status(PLUGIN).map(|_| ()),
        device.service.run_now(PLUGIN).map(|_| ()),
        device.service.cancel(PLUGIN),
        device.service.disconnect(PLUGIN).map(|_| ()),
    ] {
        assert!(result.is_err());
    }
    device.environment.denied.store(false, Ordering::SeqCst);

    // Cancelling reaches registered operations of the named plugin only.
    let operation = Operation::begin(device.service.inner.id, "denote.sync.test-only");
    let other = Operation::begin(device.service.inner.id, "denote.sync.other-test-only");
    cancel_running(Some("denote.sync.test-only"));
    assert!(operation.token().load(Ordering::SeqCst));
    assert!(!other.token().load(Ordering::SeqCst));
    assert!(device.service.resolve(PLUGIN, "missing", "maybe").is_err());
    assert!(device.service.resolve(PLUGIN, "missing", "local").is_err());
}

#[test]
fn a_cancellation_never_outlives_the_operations_it_hit() {
    let storage = tempfile::tempdir().unwrap();
    let laptop = Device::new();
    let desk = Device::new();
    laptop
        .setup(SetupMode::Create, storage.path(), PASSPHRASE, false)
        .unwrap();
    desk.setup(SetupMode::Join, storage.path(), PASSPHRASE, false)
        .unwrap();
    laptop.run();
    desk.run();
    laptop.run();
    let desk_writer = desk
        .service
        .status(PLUGIN)
        .unwrap()
        .device
        .unwrap()
        .writer_id;

    // Cancel pressed while idle leaves later storage operations alone.
    laptop.service.cancel(PLUGIN).unwrap();
    laptop
        .service
        .change_passphrase(PLUGIN, PASSPHRASE, "next synthetic passphrase")
        .unwrap();
    laptop.service.cancel(PLUGIN).unwrap();
    laptop.service.retire_device(PLUGIN, &desk_writer).unwrap();
    laptop.service.cancel(PLUGIN).unwrap();
    laptop.run();

    // Disconnect cancels, then setting up again talks to storage normally.
    laptop.service.disconnect(PLUGIN).unwrap();
    laptop
        .setup(
            SetupMode::Join,
            storage.path(),
            "next synthetic passphrase",
            false,
        )
        .unwrap();
    laptop.service.cancel(PLUGIN).unwrap();
    laptop
        .service
        .unlock(PLUGIN, "next synthetic passphrase", false)
        .unwrap();
    laptop.run();
}
