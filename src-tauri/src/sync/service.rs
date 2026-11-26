//! Sync lifecycle: setup, unlock, scheduling, cancellation, and status.
//! See `docs/sync.md` ("Scheduling and lifecycle" and "Native commands").

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use zeroize::Zeroizing;

use super::{
    backend::{Backend, PutMode, PutOutcome},
    backends,
    config::{
        BackendConfig, BackendContext, BackendSecrets, GoogleConnection, MAX_PREFERENCE_BYTES,
        PREFERENCE_KEYS, PersistedConfig, SetupMode, SyncBackendRequest, SyncOptions,
        SyncSetupRequest, SyncVaultRequest, bounded_field, bounded_secret, validate_device_name,
    },
    crypto::{
        Keys, MasterKey, create_header, parse_header, random_hex_id, rewrap, serialize_header,
        unwrap_master, validate_writer_id,
    },
    engine::{self, CycleReport, Engine, Phase, VaultLock},
    metadata::{self, DbVault},
    model::valid_vault_id,
    plugin_data::PluginDataSource,
    scan::{self, Availability},
    status::{
        BackendView, ConflictView, DeviceEntryView, DeviceView, GoogleConnectionView,
        LastRunRecord, ProgressView, SuggestionView, SyncAppliedEvent, SyncState, SyncStatusView,
        VaultView, VaultViewState,
    },
    store::{self, Identity, Store, VaultRow, VaultState},
};
use crate::{
    db,
    error::{AppError, AppResult},
};

const SECRET_MASTER: &str = "master-key";
const SECRET_BACKEND: &str = "backend";
const STARTUP_DELAY: Duration = Duration::from_secs(30);
const SOON: Duration = Duration::from_secs(2);
const MANUAL_ONLY_RECHECK: Duration = Duration::from_secs(3600);
const GOOGLE_CONNECTION_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_PENDING_GOOGLE_CONNECTIONS: usize = 4;
const MAX_PASSPHRASE_BYTES: usize = 1024;
const PROGRESS_EVENT_INTERVAL: Duration = Duration::from_millis(250);

/// Host services the sync service needs from the running application.
pub(crate) trait SyncEnvironment: Send + Sync {
    /// The plugin's enabled `sync` permission, an unlocked app, and no safe mode.
    fn authorize(&self, plugin_id: &str) -> AppResult<()>;
    fn plugin_data(&self) -> Option<&dyn PluginDataSource>;
    fn secrets(&self) -> AppResult<&dyn SecretStore>;
    fn vault_lock(&self) -> AppResult<&dyn VaultLock>;
    fn backend_plugins(&self) -> Option<crate::plugins::PluginManager>;
    fn open_url(&self, url: &str) -> AppResult<()>;
    fn emit_status(&self, status: &SyncStatusView);
    fn emit_applied(&self, applied: &SyncAppliedEvent);
    fn app_version(&self) -> String;
    /// Opens the storage transport. Tests substitute their own.
    fn open_backend(
        &self,
        config: &BackendConfig,
        secrets: &BackendSecrets,
        context: &BackendContext,
    ) -> AppResult<Box<dyn Backend>> {
        backends::open(config, secrets, context)
    }
}

/// The operating-system credential store, under the reserved `host-sync.` namespace.
pub(crate) trait SecretStore: Send + Sync {
    fn get(&self, plugin_id: &str, name: &str) -> AppResult<Option<Zeroizing<String>>>;
    fn set(&self, plugin_id: &str, name: &str, value: &str) -> AppResult<()>;
    fn delete(&self, plugin_id: &str, name: &str) -> AppResult<()>;
}

/// One in-flight operation that may touch storage or vaults.
struct Registration {
    plugin_id: String,
    service: u64,
    cancel: Arc<AtomicBool>,
}

static ACTIVE: Mutex<Vec<Registration>> = Mutex::new(Vec::new());
static NEXT_SERVICE_ID: AtomicU64 = AtomicU64::new(1);

/// Cancels in-flight sync operations at their next file boundary or storage
/// request (all plugins for `None`). Nothing outlives the operations it hits.
pub(crate) fn cancel_running(plugin_id: Option<&str>) {
    let active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
    for registration in active.iter() {
        if plugin_id.is_none_or(|plugin_id| plugin_id == registration.plugin_id) {
            registration.cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// A cancellation token for one cycle, setup, unlock, retirement, or
/// passphrase change. Each operation gets a fresh token, registered until it
/// ends, so a cancellation never outlives the operations it was aimed at.
struct Operation(Arc<AtomicBool>);

impl Operation {
    fn begin(service: u64, plugin_id: &str) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        ACTIVE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Registration {
                plugin_id: plugin_id.to_string(),
                service,
                cancel: cancel.clone(),
            });
        Self(cancel)
    }

    fn token(&self) -> &Arc<AtomicBool> {
        &self.0
    }

    fn check(&self) -> AppResult<()> {
        if self.0.load(Ordering::SeqCst) {
            Err(scan::cancelled())
        } else {
            Ok(())
        }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        ACTIVE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|registration| !Arc::ptr_eq(&registration.cancel, &self.0));
    }
}

#[derive(Default)]
struct Runtime {
    master: Option<MasterKey>,
    running: bool,
    progress: Option<ProgressView>,
    last_progress_event: Option<Instant>,
    google: HashMap<String, (Instant, GoogleConnection)>,
    run_soon: bool,
    reschedule: bool,
    shutdown: bool,
}

struct Inner {
    sync_dir: PathBuf,
    app_db: PathBuf,
    store: Store,
    environment: Arc<dyn SyncEnvironment>,
    runtime: Mutex<Runtime>,
    wake: Condvar,
    cycle: Mutex<()>,
    /// Identifies this service's operations in the shared registry.
    id: u64,
}

#[derive(Clone)]
pub(crate) struct SyncService {
    inner: Arc<Inner>,
}

fn validate_passphrase(passphrase: &str) -> AppResult<()> {
    crate::crypto::validate_password(passphrase)?;
    if passphrase.len() > MAX_PASSPHRASE_BYTES {
        return Err(AppError::Crypto(format!(
            "The sync passphrase must be at most {MAX_PASSPHRASE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn not_configured() -> AppError {
    AppError::State("Sync is not set up on this device.".into())
}

fn needs_passphrase() -> AppError {
    AppError::State("Enter the sync passphrase to unlock sync on this device.".into())
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

impl SyncService {
    pub(crate) fn new(
        app_data_dir: &Path,
        app_db: PathBuf,
        environment: Arc<dyn SyncEnvironment>,
    ) -> AppResult<Self> {
        let sync_dir = app_data_dir.join("sync");
        let store = Store::open(&sync_dir)?;
        Ok(Self {
            inner: Arc::new(Inner {
                sync_dir,
                app_db,
                store,
                environment,
                runtime: Mutex::new(Runtime::default()),
                wake: Condvar::new(),
                cycle: Mutex::new(()),
                id: NEXT_SERVICE_ID.fetch_add(1, Ordering::Relaxed),
            }),
        })
    }

    pub(crate) fn start_scheduler(&self) -> AppResult<()> {
        let service = self.clone();
        thread::Builder::new()
            .name("denote-sync".into())
            .spawn(move || service.scheduler())
            .map(|_| ())
            .map_err(|error| AppError::State(format!("Unable to start sync: {error}")))
    }

    pub(crate) fn shutdown(&self) {
        self.cancel_operations();
        let mut runtime = self
            .inner
            .runtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        runtime.shutdown = true;
        runtime.master = None;
        drop(runtime);
        self.inner.wake.notify_all();
    }

    fn runtime(&self) -> AppResult<MutexGuard<'_, Runtime>> {
        self.inner
            .runtime
            .lock()
            .map_err(|_| AppError::State("Sync state lock is poisoned".into()))
    }

    fn cycle_guard(&self) -> AppResult<MutexGuard<'_, ()>> {
        self.inner
            .cycle
            .lock()
            .map_err(|_| AppError::State("Sync cycle lock is poisoned".into()))
    }

    fn environment(&self) -> &dyn SyncEnvironment {
        self.inner.environment.as_ref()
    }

    fn begin(&self, plugin_id: &str) -> Operation {
        Operation::begin(self.inner.id, plugin_id)
    }

    /// Cancels this service's in-flight operations. With none running it
    /// does nothing, so a later operation starts uncancelled.
    fn cancel_operations(&self) {
        let active = ACTIVE.lock().unwrap_or_else(PoisonError::into_inner);
        for registration in active.iter() {
            if registration.service == self.inner.id {
                registration.cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    fn config(&self) -> AppResult<Option<PersistedConfig>> {
        store::get_meta(&self.inner.store.connection()?, store::META_CONFIG)
    }

    /// Authorizes `plugin_id` and, once configured, requires it to be the plugin
    /// that set sync up.
    fn authorize(&self, plugin_id: &str) -> AppResult<Option<PersistedConfig>> {
        self.environment().authorize(plugin_id)?;
        let config = self.config()?;
        if let Some(config) = &config
            && config.plugin_id != plugin_id
        {
            return Err(AppError::Plugin(format!(
                "Sync on this device is managed by {}",
                config.plugin_id
            )));
        }
        Ok(config)
    }

    fn configured(&self, plugin_id: &str) -> AppResult<PersistedConfig> {
        self.authorize(plugin_id)?.ok_or_else(not_configured)
    }

    fn wake(&self, run_soon: bool, reschedule: bool) {
        if let Ok(mut runtime) = self.runtime() {
            runtime.run_soon |= run_soon;
            runtime.reschedule |= reschedule;
        }
        self.inner.wake.notify_all();
    }

    /// The unlocked keys, loading a remembered master key on first use.
    fn keys(&self, config: &PersistedConfig) -> AppResult<Option<Keys>> {
        let loaded = self.runtime()?.master.is_some();
        if !loaded && config.remember_passphrase {
            let remembered = self
                .environment()
                .secrets()?
                .get(&config.plugin_id, SECRET_MASTER)?;
            if let Some(encoded) = remembered {
                let master = MasterKey::from_base64(&encoded)?;
                let mut runtime = self.runtime()?;
                if runtime.master.is_none() {
                    runtime.master = Some(master);
                }
            }
        }
        self.runtime()?
            .master
            .as_ref()
            .map(|master| Keys::derive(master, &config.repository_id))
            .transpose()
    }

    fn backend_secrets(&self, config: &PersistedConfig) -> AppResult<BackendSecrets> {
        let Some(encoded) = self
            .environment()
            .secrets()?
            .get(&config.plugin_id, SECRET_BACKEND)?
        else {
            return Ok(BackendSecrets::None);
        };
        serde_json::from_str(&encoded).map_err(|_| {
            AppError::State(
                "The saved sync storage credentials are damaged. Disconnect and set up sync again."
                    .into(),
            )
        })
    }

    fn vault_roots(&self) -> AppResult<Vec<PathBuf>> {
        Ok(metadata::all_vaults(&db::open(&self.inner.app_db)?)?
            .into_iter()
            .map(|vault| canonical_or_self(Path::new(&vault.path)))
            .collect())
    }

    fn context(&self, plugin_id: &str, operation: &Operation) -> AppResult<BackendContext> {
        Ok(BackendContext {
            plugin_id: plugin_id.to_string(),
            sync_dir: self.inner.sync_dir.clone(),
            cancel: operation.token().clone(),
            plugins: self.environment().backend_plugins(),
            vault_roots: self.vault_roots()?,
        })
    }

    fn open_backend(
        &self,
        config: &PersistedConfig,
        operation: &Operation,
    ) -> AppResult<Box<dyn Backend>> {
        self.environment().open_backend(
            &config.backend,
            &self.backend_secrets(config)?,
            &self.context(&config.plugin_id, operation)?,
        )
    }

    fn emit_status(&self) {
        match self.build_status() {
            Ok(status) => self.environment().emit_status(&status),
            Err(error) => eprintln!("Unable to report sync status: {error}"),
        }
    }

    fn progress(&self, phase: Phase, done: usize, total: usize) {
        let emit = match self.runtime() {
            Ok(mut runtime) => {
                let changed_phase = runtime.progress.map(|progress| progress.phase) != Some(phase);
                runtime.progress = Some(ProgressView { phase, done, total });
                let due = runtime
                    .last_progress_event
                    .is_none_or(|last| last.elapsed() >= PROGRESS_EVENT_INTERVAL);
                if changed_phase || due {
                    runtime.last_progress_event = Some(Instant::now());
                }
                changed_phase || due
            }
            Err(_) => false,
        };
        if emit {
            self.emit_status();
        }
    }

    // ----- Status -----

    /// Checks only the caller's enabled `sync` permission (for host pickers).
    pub(crate) fn authorize_plugin(&self, plugin_id: &str) -> AppResult<()> {
        self.authorize(plugin_id).map(|_| ())
    }

    pub(crate) fn status(&self, plugin_id: &str) -> AppResult<SyncStatusView> {
        self.authorize(plugin_id)?;
        self.build_status()
    }

    fn build_status(&self) -> AppResult<SyncStatusView> {
        let connection = self.inner.store.connection()?;
        let Some(config) = store::get_meta::<PersistedConfig>(&connection, store::META_CONFIG)?
        else {
            return Ok(SyncStatusView {
                state: SyncState::NotConfigured,
                backend: None,
                remember_passphrase: false,
                options: SyncOptions::default(),
                progress: None,
                last_run: None,
                device: None,
                devices: Vec::new(),
                vaults: self.local_vault_views(&[])?,
                conflicts: Vec::new(),
                plugin_suggestions: Vec::new(),
                warnings: Vec::new(),
            });
        };
        let mut warnings = Vec::new();
        let unlocked = match self.keys(&config) {
            Ok(keys) => keys.is_some(),
            Err(error) => {
                warnings.push(format!(
                    "The remembered sync key could not be used: {error}"
                ));
                false
            }
        };
        let (running, progress) = {
            let runtime = self.runtime()?;
            (runtime.running, runtime.progress)
        };
        let last: Option<LastRunRecord> = store::get_meta(&connection, store::META_LAST_RUN)?;
        let identity: Option<Identity> = store::get_meta(&connection, store::META_IDENTITY)?;
        let state = if running {
            SyncState::Running
        } else if !unlocked {
            SyncState::NeedsPassphrase
        } else if config.options.paused {
            SyncState::Paused
        } else if last.as_ref().is_some_and(|last| !last.run.ok) {
            SyncState::Error
        } else {
            SyncState::Idle
        };
        if let Some(last) = &last {
            warnings.extend(last.warnings.iter().cloned());
        }
        let mut devices = Vec::new();
        if let Some(identity) = &identity {
            devices.push(DeviceEntryView {
                writer_id: identity.writer_id.clone(),
                name: config.device_name.clone(),
                last_generation: identity.generation,
                last_seen_at: last.as_ref().map(|last| last.run.finished_at.clone()),
                retired: false,
                is_self: true,
            });
        }
        for writer in store::list_writers(&connection)? {
            if identity
                .as_ref()
                .is_some_and(|identity| identity.writer_id == writer.writer_id)
            {
                continue;
            }
            devices.push(DeviceEntryView {
                name: if writer.name.is_empty() {
                    "Unnamed device".into()
                } else {
                    writer.name.clone()
                },
                writer_id: writer.writer_id,
                last_generation: writer.last_applied,
                last_seen_at: writer.last_seen_at,
                retired: writer.retired,
                is_self: false,
            });
        }
        let rows = store::list_vaults(&connection)?;
        let conflicts = store::list_conflicts(&connection)?
            .into_iter()
            .map(|conflict| {
                let (local_summary, remote_summary) = engine::conflict_summaries(&conflict);
                ConflictView {
                    id: conflict.id.clone(),
                    kind: conflict.kind.as_str(),
                    vault_sync_id: conflict.vault_sync_id.clone(),
                    plugin_id: conflict.plugin_id.clone(),
                    key: super::model::EntryKind::parse(&conflict.key)
                        .plugin()
                        .and_then(|_| conflict.key.rsplit('/').next().map(str::to_string)),
                    local_summary,
                    remote_summary,
                    remote_device: conflict.remote_device,
                    detected_at: conflict.detected_at,
                }
            })
            .collect();
        let plugin_suggestions = store::list_suggestions(&connection)?
            .into_iter()
            .map(|(plugin_id, from_device)| SuggestionView {
                plugin_id,
                from_device,
            })
            .collect();
        Ok(SyncStatusView {
            state,
            backend: Some(BackendView {
                kind: config.backend.kind(),
                label: config.backend.label(),
            }),
            remember_passphrase: config.remember_passphrase,
            options: config.options.clone(),
            progress: if running { progress } else { None },
            last_run: last.map(|last| last.run),
            device: identity.map(|identity| DeviceView {
                writer_id: identity.writer_id,
                name: config.device_name.clone(),
            }),
            devices,
            vaults: self.local_vault_views(&rows)?,
            conflicts,
            plugin_suggestions,
            warnings,
        })
    }

    fn local_vault_views(&self, rows: &[VaultRow]) -> AppResult<Vec<VaultView>> {
        let local = metadata::all_vaults(&db::open(&self.inner.app_db)?)?;
        let mut views = Vec::new();
        for vault in &local {
            let row = rows
                .iter()
                .find(|row| row.scope_id.as_deref() == Some(vault.scope_id.as_str()));
            let (state, message) = match row {
                Some(row) if row.state == VaultState::Synced => {
                    if row.paused {
                        (VaultViewState::PausedConflict, row.message.clone())
                    } else {
                        match scan::availability(Path::new(&vault.path)) {
                            Availability::Ready => (VaultViewState::Synced, row.message.clone()),
                            Availability::Unavailable(message) => {
                                (VaultViewState::Unavailable, Some(message))
                            }
                            Availability::Transition => (
                                VaultViewState::PausedEncryption,
                                Some(
                                    "Encryption is being changed; sync resumes when it finishes."
                                        .into(),
                                ),
                            ),
                        }
                    }
                }
                _ => (VaultViewState::NotEnrolled, None),
            };
            views.push(VaultView {
                sync_id: row.map(|row| row.sync_id.clone()),
                name: vault.name.clone(),
                scope_id: Some(vault.scope_id.clone()),
                path: Some(vault.path.clone()),
                state,
                message,
            });
        }
        for row in rows {
            let mapped_locally = row
                .scope_id
                .as_deref()
                .is_some_and(|scope| local.iter().any(|vault| vault.scope_id == scope));
            if mapped_locally {
                continue;
            }
            let state = match row.state {
                VaultState::PendingMapping => VaultViewState::PendingMapping,
                VaultState::Skipped => VaultViewState::Skipped,
                VaultState::Synced => VaultViewState::Unavailable,
                VaultState::Stopped => continue,
            };
            views.push(VaultView {
                sync_id: Some(row.sync_id.clone()),
                name: if row.name.is_empty() {
                    "Vault".into()
                } else {
                    row.name.clone()
                },
                scope_id: None,
                path: None,
                state,
                message: row.message.clone(),
            });
        }
        Ok(views)
    }

    // ----- Setup -----

    fn validate_folder_backend(&self, path: &str) -> AppResult<PathBuf> {
        let path = PathBuf::from(bounded_field(path, "sync folder", false)?);
        if !path.is_absolute() {
            return Err(AppError::InvalidPath(
                "Choose an absolute sync folder".into(),
            ));
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| AppError::NotFound("The sync folder does not exist".into()))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(AppError::InvalidPath(
                "The sync folder must be a real folder, not a file or link".into(),
            ));
        }
        let canonical = fs::canonicalize(&path)?;
        if self
            .vault_roots()?
            .iter()
            .any(|vault| overlaps(&canonical, vault))
        {
            return Err(AppError::InvalidPath(
                "The sync folder cannot be inside a vault or contain one".into(),
            ));
        }
        if overlaps(&canonical, &canonical_or_self(&self.inner.sync_dir)) {
            return Err(AppError::InvalidPath(
                "The sync folder cannot be inside Denote's own data".into(),
            ));
        }
        Ok(canonical)
    }

    fn take_google_connection(&self, token: &str) -> AppResult<GoogleConnection> {
        let mut runtime = self.runtime()?;
        runtime
            .google
            .retain(|_, (created, _)| created.elapsed() < GOOGLE_CONNECTION_TTL);
        runtime
            .google
            .remove(token)
            .map(|(_, connection)| connection)
            .ok_or_else(|| {
                AppError::State("The Google sign-in expired. Connect Google Drive again.".into())
            })
    }

    fn backend_from_request(
        &self,
        request: &SyncBackendRequest,
    ) -> AppResult<(BackendConfig, BackendSecrets)> {
        Ok(match request {
            SyncBackendRequest::Folder { path } => (
                BackendConfig::Folder {
                    path: self
                        .validate_folder_backend(path)?
                        .to_string_lossy()
                        .into_owned(),
                },
                BackendSecrets::None,
            ),
            SyncBackendRequest::Webdav {
                url,
                username,
                password,
            } => (
                BackendConfig::Webdav {
                    url: bounded_field(url, "WebDAV URL", false)?,
                },
                BackendSecrets::Webdav {
                    username: bounded_field(username, "WebDAV user name", true)?,
                    password: bounded_secret(password, "WebDAV password")?,
                },
            ),
            SyncBackendRequest::S3 {
                endpoint,
                region,
                bucket,
                prefix,
                path_style,
                access_key_id,
                secret_access_key,
            } => (
                BackendConfig::S3 {
                    endpoint: bounded_field(endpoint, "S3 endpoint", false)?,
                    region: bounded_field(region, "S3 region", false)?,
                    bucket: bounded_field(bucket, "S3 bucket", false)?,
                    prefix: bounded_field(prefix, "S3 prefix", true)?,
                    path_style: *path_style,
                },
                BackendSecrets::S3 {
                    access_key_id: bounded_field(access_key_id, "S3 access key ID", false)?,
                    secret_access_key: bounded_secret(secret_access_key, "S3 secret access key")?,
                },
            ),
            SyncBackendRequest::GoogleDrive {
                connection_token,
                folder_name,
            } => {
                let folder_name = bounded_field(folder_name, "Google Drive folder name", false)?;
                let connection = self.take_google_connection(connection_token)?;
                (
                    BackendConfig::GoogleDrive {
                        folder_name,
                        account_label: connection.account_label.clone(),
                        client_id: connection.client_id.clone(),
                    },
                    BackendSecrets::GoogleDrive {
                        refresh_token: connection.refresh_token.clone(),
                    },
                )
            }
            SyncBackendRequest::Git {
                remote_url,
                branch,
                auth_mode,
            } => (
                BackendConfig::Git {
                    remote_url: bounded_field(remote_url, "Git remote URL", false)?,
                    branch: bounded_field(branch, "Git branch", false)?,
                    auth_mode: *auth_mode,
                },
                BackendSecrets::None,
            ),
        })
    }

    /// Creates or joins a repository. The passphrase never leaves this device.
    pub(crate) fn configure(
        &self,
        plugin_id: &str,
        request: SyncSetupRequest,
    ) -> AppResult<SyncStatusView> {
        if self.authorize(plugin_id)?.is_some() {
            return Err(AppError::State(
                "Sync is already set up on this device. Disconnect it first.".into(),
            ));
        }
        let operation = self.begin(plugin_id);
        let _cycle = self.cycle_guard()?;
        let device_name = validate_device_name(&request.device_name)?;
        validate_passphrase(&request.passphrase)?;
        let (backend_config, secrets) = self.backend_from_request(&request.backend)?;
        let backend = self.environment().open_backend(
            &backend_config,
            &secrets,
            &self.context(plugin_id, &operation)?,
        )?;
        backend.refresh()?;
        let existing = backend.get("repository.json")?;
        let (header, master) = match (request.mode, existing) {
            (SetupMode::Create, Some(_)) => {
                return Err(AppError::State(
                    "This storage already holds a sync repository. Choose Join instead.".into(),
                ));
            }
            (SetupMode::Join, None) => {
                return Err(AppError::NotFound(
                    "No sync repository was found in this storage. Choose Create instead.".into(),
                ));
            }
            (SetupMode::Join, Some(bytes)) => {
                let header = parse_header(&bytes)?;
                let master = unwrap_master(&header, &request.passphrase)?;
                (header, master)
            }
            (SetupMode::Create, None) => {
                let (header, master) = create_header(&request.passphrase)?;
                let outcome = backend.put(
                    "repository.json",
                    &serialize_header(&header)?,
                    PutMode::CreateOnly,
                )?;
                backend.flush()?;
                // Re-read and adopt whatever won a concurrent creation.
                let stored = backend.get("repository.json")?.ok_or_else(|| {
                    AppError::State("The sync repository could not be created".into())
                })?;
                let stored = parse_header(&stored)?;
                if outcome == PutOutcome::Written && stored.repository_id == header.repository_id {
                    (header, master)
                } else {
                    let master = unwrap_master(&stored, &request.passphrase).map_err(|_| {
                        AppError::State(
                            "Another device created a repository here at the same time. Join it with its passphrase."
                                .into(),
                        )
                    })?;
                    (stored, master)
                }
            }
        };
        let config = PersistedConfig {
            plugin_id: plugin_id.to_string(),
            repository_id: header.repository_id.clone(),
            backend: backend_config,
            device_name,
            remember_passphrase: request.remember_passphrase,
            options: SyncOptions::default(),
        };
        let secret_store = self.environment().secrets()?;
        if secrets != BackendSecrets::None {
            let encoded = Zeroizing::new(serde_json::to_string(&secrets).map_err(|error| {
                AppError::State(format!("Unable to save sync credentials: {error}"))
            })?);
            secret_store.set(plugin_id, SECRET_BACKEND, &encoded)?;
        }
        if request.remember_passphrase {
            secret_store.set(plugin_id, SECRET_MASTER, &master.to_base64())?;
        }
        engine::initialize_store(&self.inner.store, &config)?;
        self.runtime()?.master = Some(master);
        drop(_cycle);
        self.wake(true, true);
        let status = self.build_status()?;
        self.environment().emit_status(&status);
        Ok(status)
    }

    /// Signs in to Google Drive. The refresh token stays in host memory under an
    /// opaque token until `configure` consumes it.
    pub(crate) fn connect_google(
        &self,
        plugin_id: &str,
        client_id: Option<String>,
    ) -> AppResult<GoogleConnectionView> {
        self.authorize(plugin_id)?;
        let client_id = client_id
            .map(|id| bounded_field(&id, "Google client ID", true))
            .transpose()?
            .filter(|id| !id.is_empty());
        let environment = self.inner.environment.clone();
        let connection = backends::connect_google(client_id, &|url| environment.open_url(url))?;
        let token = random_hex_id()?;
        let label = connection.account_label.clone();
        let mut runtime = self.runtime()?;
        runtime
            .google
            .retain(|_, (created, _)| created.elapsed() < GOOGLE_CONNECTION_TTL);
        while runtime.google.len() >= MAX_PENDING_GOOGLE_CONNECTIONS {
            let oldest = runtime
                .google
                .iter()
                .min_by_key(|(_, (created, _))| *created)
                .map(|(token, _)| token.clone());
            match oldest {
                Some(oldest) => runtime.google.remove(&oldest),
                None => break,
            };
        }
        runtime
            .google
            .insert(token.clone(), (Instant::now(), connection));
        Ok(GoogleConnectionView {
            connection_token: token,
            account_label: label,
        })
    }

    pub(crate) fn unlock(
        &self,
        plugin_id: &str,
        passphrase: &str,
        remember: bool,
    ) -> AppResult<SyncStatusView> {
        let mut config = self.configured(plugin_id)?;
        if passphrase.len() > MAX_PASSPHRASE_BYTES {
            return Err(AppError::Crypto("The sync passphrase is incorrect".into()));
        }
        let operation = self.begin(plugin_id);
        let backend = self.open_backend(&config, &operation)?;
        backend.refresh()?;
        let header = parse_header(&backend.get("repository.json")?.ok_or_else(|| {
            AppError::NotFound("The sync repository is missing from storage.".into())
        })?)?;
        if header.repository_id != config.repository_id {
            return Err(AppError::State(
                "The sync storage now holds a different repository. Disconnect and set up sync again."
                    .into(),
            ));
        }
        let master = unwrap_master(&header, passphrase)?;
        let secrets = self.environment().secrets()?;
        if remember {
            secrets.set(plugin_id, SECRET_MASTER, &master.to_base64())?;
        } else {
            secrets.delete(plugin_id, SECRET_MASTER)?;
        }
        if config.remember_passphrase != remember {
            config.remember_passphrase = remember;
            store::set_meta(&self.inner.store.connection()?, store::META_CONFIG, &config)?;
        }
        self.runtime()?.master = Some(master);
        self.wake(true, false);
        let status = self.build_status()?;
        self.environment().emit_status(&status);
        Ok(status)
    }

    // ----- Cycles -----

    /// Runs one cycle under `operation`, which the caller registered before
    /// authorizing, so a cancellation while this cycle waits for another one
    /// still applies to it.
    fn run_cycle(
        &self,
        config: &PersistedConfig,
        keys: &Keys,
        operation: &Operation,
        scheduled: bool,
    ) -> AppResult<()> {
        let _cycle = self.cycle_guard()?;
        // Configuration may have changed while waiting for the previous cycle,
        // and the app may have been locked, put in safe mode, or the plugin
        // disabled or stopped.
        let current = self
            .authorize(&config.plugin_id)?
            .ok_or_else(not_configured)?;
        if current.repository_id != config.repository_id {
            return Err(not_configured());
        }
        operation.check()?;
        if scheduled && (current.options.paused || current.options.interval_minutes == 0) {
            return Ok(());
        }
        {
            let mut runtime = self.runtime()?;
            runtime.running = true;
            runtime.progress = None;
        }
        self.emit_status();
        let mut report = CycleReport::default();
        let version = self.environment().app_version();
        let staging = self.inner.sync_dir.join("staging");
        let result = self.open_backend(&current, &operation).and_then(|backend| {
            let vault_lock = self.environment().vault_lock()?;
            let engine = Engine {
                store: &self.inner.store,
                backend: backend.as_ref(),
                keys,
                app_db: &self.inner.app_db,
                plugins: self.environment().plugin_data(),
                vault_lock,
                staging: &staging,
                cancel: operation.token(),
                progress: &|phase, done, total| self.progress(phase, done, total),
                app_version: &version,
            };
            engine.run(&mut report)
        });
        {
            let mut runtime = self.runtime()?;
            runtime.running = false;
            runtime.progress = None;
        }
        let record =
            LastRunRecord::from_report(&report, result.as_ref().err().map(ToString::to_string));
        if let Err(error) = self
            .inner
            .store
            .connection()
            .and_then(|connection| store::set_meta(&connection, store::META_LAST_RUN, &record))
        {
            eprintln!("Unable to record the sync result: {error}");
        }
        if let Some(applied) = SyncAppliedEvent::from_report(&report) {
            self.environment().emit_applied(&applied);
        }
        self.emit_status();
        result
    }

    /// Runs a cycle now and returns when it ends.
    pub(crate) fn run_now(&self, plugin_id: &str) -> AppResult<SyncStatusView> {
        let operation = self.begin(plugin_id);
        let config = self.configured(plugin_id)?;
        let keys = self.keys(&config)?.ok_or_else(needs_passphrase)?;
        self.run_cycle(&config, &keys, &operation, false)?;
        self.build_status()
    }

    pub(crate) fn cancel(&self, plugin_id: &str) -> AppResult<()> {
        self.authorize(plugin_id)?;
        self.cancel_operations();
        Ok(())
    }

    fn scheduler(self) {
        let mut next_due = Instant::now() + STARTUP_DELAY;
        loop {
            {
                let Ok(mut runtime) = self.runtime() else {
                    return;
                };
                loop {
                    if runtime.shutdown {
                        return;
                    }
                    if runtime.reschedule {
                        runtime.reschedule = false;
                        next_due = Instant::now() + self.interval();
                    }
                    if runtime.run_soon {
                        runtime.run_soon = false;
                        next_due = next_due.min(Instant::now() + SOON);
                    }
                    let now = Instant::now();
                    if now >= next_due {
                        break;
                    }
                    runtime = match self.inner.wake.wait_timeout(runtime, next_due - now) {
                        Ok((runtime, _)) => runtime,
                        Err(_) => return,
                    };
                }
            }
            // Failures are recorded in the last run shown by the panel.
            let _ = self.scheduled_cycle();
            next_due = Instant::now() + self.interval();
        }
    }

    fn interval(&self) -> Duration {
        match self.config() {
            Ok(Some(config)) if config.options.interval_minutes > 0 => {
                Duration::from_secs(u64::from(config.options.interval_minutes) * 60)
            }
            _ => MANUAL_ONLY_RECHECK,
        }
    }

    /// Runs only when configured, unpaused, unlocked, and authorized; the app
    /// lock, safe mode, and a disabled plugin all pause scheduling.
    fn scheduled_cycle(&self) -> AppResult<()> {
        let Some(config) = self.config()? else {
            return Ok(());
        };
        if config.options.paused || config.options.interval_minutes == 0 {
            return Ok(());
        }
        let operation = self.begin(&config.plugin_id);
        if self.environment().authorize(&config.plugin_id).is_err() {
            return Ok(());
        }
        let Some(keys) = self.keys(&config)? else {
            return Ok(());
        };
        self.run_cycle(&config, &keys, &operation, true)
    }

    // ----- Options, vaults, conflicts, preferences -----

    pub(crate) fn set_options(
        &self,
        plugin_id: &str,
        options: SyncOptions,
    ) -> AppResult<SyncStatusView> {
        let mut config = self.configured(plugin_id)?;
        options.validate()?;
        config.options = options;
        store::set_meta(&self.inner.store.connection()?, store::META_CONFIG, &config)?;
        self.wake(false, true);
        let status = self.build_status()?;
        self.environment().emit_status(&status);
        Ok(status)
    }

    fn ensure_free_vault_root(
        &self,
        connection: &rusqlite::Connection,
        root: &Path,
        except: Option<&str>,
    ) -> AppResult<()> {
        let app = db::open(&self.inner.app_db)?;
        for row in store::list_vaults(connection)? {
            if row.state != VaultState::Synced || Some(row.sync_id.as_str()) == except {
                continue;
            }
            let Some(vault) = row
                .scope_id
                .as_deref()
                .map(|scope| metadata::vault_by_scope(&app, scope))
                .transpose()?
                .flatten()
            else {
                continue;
            };
            if overlaps(root, &canonical_or_self(Path::new(&vault.path))) {
                return Err(AppError::InvalidPath(
                    "This folder overlaps another synced vault".into(),
                ));
            }
        }
        if let Some(PersistedConfig {
            backend: BackendConfig::Folder { path },
            ..
        }) = store::get_meta(connection, store::META_CONFIG)?
            && overlaps(root, &canonical_or_self(Path::new(&path)))
        {
            return Err(AppError::InvalidPath(
                "A synced vault cannot contain the sync folder or be inside it".into(),
            ));
        }
        Ok(())
    }

    fn local_vault(&self, scope_id: &str) -> AppResult<DbVault> {
        metadata::vault_by_scope(&db::open(&self.inner.app_db)?, scope_id)?
            .ok_or_else(|| AppError::NotFound("That vault is not known on this device".into()))
    }

    fn vault_row(connection: &rusqlite::Connection, sync_id: &str) -> AppResult<VaultRow> {
        if !valid_vault_id(sync_id) {
            return Err(AppError::InvalidData("Invalid vault sync ID".into()));
        }
        store::list_vaults(connection)?
            .into_iter()
            .find(|row| row.sync_id == sync_id)
            .ok_or_else(|| AppError::NotFound("That synced vault is unknown".into()))
    }

    fn prepare_new_vault_folder(path: &str) -> AppResult<PathBuf> {
        let path = PathBuf::from(bounded_field(path, "vault folder", false)?);
        if !path.is_absolute() {
            return Err(AppError::InvalidPath(
                "Choose an absolute vault folder".into(),
            ));
        }
        let parent = path
            .parent()
            .ok_or_else(|| AppError::InvalidPath("Choose a folder inside another folder".into()))?;
        let parent_metadata = fs::symlink_metadata(parent)
            .map_err(|_| AppError::NotFound("The parent folder does not exist".into()))?;
        if !parent_metadata.is_dir() || parent_metadata.file_type().is_symlink() {
            return Err(AppError::InvalidPath(
                "The parent folder is not a real folder".into(),
            ));
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(AppError::InvalidPath(
                        "Choose a folder, not a file or link".into(),
                    ));
                }
                if fs::read_dir(&path)?.next().is_some() {
                    return Err(AppError::InvalidPath(
                        "Choose an empty folder, or map the vault to an existing vault instead"
                            .into(),
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&path)?,
            Err(error) => return Err(error.into()),
        }
        Ok(fs::canonicalize(&path)?)
    }

    pub(crate) fn map_vault(
        &self,
        plugin_id: &str,
        request: SyncVaultRequest,
    ) -> AppResult<SyncStatusView> {
        self.configured(plugin_id)?;
        let _cycle = self.cycle_guard()?;
        let connection = self.inner.store.connection()?;
        match request {
            SyncVaultRequest::MapNew { sync_id, path } => {
                let row = Self::vault_row(&connection, &sync_id)?;
                if row.state == VaultState::Synced {
                    return Err(AppError::State("That vault is already synced here".into()));
                }
                let root = Self::prepare_new_vault_folder(&path)?;
                self.ensure_free_vault_root(&connection, &root, Some(&sync_id))?;
                let name = root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Vault")
                    .to_string();
                let app = db::open(&self.inner.app_db)?;
                let id = db::ensure_vault(&app, &root.to_string_lossy(), &name)?;
                let scope = db::plugin_scope_id(&app, id)?;
                store::upsert_vault(
                    &connection,
                    &VaultRow {
                        scope_id: Some(scope),
                        state: VaultState::Synced,
                        paused: false,
                        message: None,
                        ..row
                    },
                )?;
            }
            SyncVaultRequest::MapExisting { sync_id, scope_id } => {
                let row = Self::vault_row(&connection, &sync_id)?;
                let vault = self.local_vault(&scope_id)?;
                if store::list_vaults(&connection)?.iter().any(|other| {
                    other.sync_id != sync_id
                        && other.state == VaultState::Synced
                        && other.scope_id.as_deref() == Some(scope_id.as_str())
                }) {
                    return Err(AppError::State(
                        "That local vault already syncs as another vault".into(),
                    ));
                }
                let root = canonical_or_self(Path::new(&vault.path));
                self.ensure_free_vault_root(&connection, &root, Some(&sync_id))?;
                store::upsert_vault(
                    &connection,
                    &VaultRow {
                        scope_id: Some(scope_id),
                        state: VaultState::Synced,
                        paused: false,
                        message: None,
                        ..row
                    },
                )?;
            }
            SyncVaultRequest::Skip { sync_id } => {
                let row = Self::vault_row(&connection, &sync_id)?;
                if row.state == VaultState::Synced {
                    return Err(AppError::State(
                        "Stop syncing the vault instead of skipping it".into(),
                    ));
                }
                store::upsert_vault(
                    &connection,
                    &VaultRow {
                        state: VaultState::Skipped,
                        ..row
                    },
                )?;
            }
            SyncVaultRequest::Enroll { scope_id } => {
                let vault = self.local_vault(&scope_id)?;
                let existing = store::list_vaults(&connection)?
                    .into_iter()
                    .find(|row| row.scope_id.as_deref() == Some(scope_id.as_str()));
                if existing
                    .as_ref()
                    .is_some_and(|row| row.state == VaultState::Synced)
                {
                    return Err(AppError::State("That vault is already synced".into()));
                }
                let root = canonical_or_self(Path::new(&vault.path));
                let sync_id = match &existing {
                    Some(row) => row.sync_id.clone(),
                    None => random_hex_id()?,
                };
                self.ensure_free_vault_root(&connection, &root, Some(&sync_id))?;
                store::upsert_vault(
                    &connection,
                    &VaultRow {
                        sync_id,
                        scope_id: Some(scope_id),
                        name: vault.name,
                        state: VaultState::Synced,
                        paused: false,
                        message: None,
                    },
                )?;
            }
            SyncVaultRequest::Stop { sync_id } => {
                let row = Self::vault_row(&connection, &sync_id)?;
                self.inner.store.transaction(|transaction| {
                    store::delete_entries_with_prefix(transaction, &format!("v/{sync_id}/"))?;
                    let mut watermarks: BTreeMap<String, i64> =
                        store::get_meta(transaction, store::META_HISTORY_WATERMARKS)?
                            .unwrap_or_default();
                    watermarks.remove(&sync_id);
                    store::set_meta(transaction, store::META_HISTORY_WATERMARKS, &watermarks)?;
                    let remaining: Vec<_> = store::list_conflicts(transaction)?
                        .into_iter()
                        .filter(|conflict| {
                            conflict.vault_sync_id.as_deref() != Some(sync_id.as_str())
                        })
                        .collect();
                    store::replace_conflicts(transaction, &remaining)?;
                    store::upsert_vault(
                        transaction,
                        &VaultRow {
                            state: VaultState::Stopped,
                            paused: false,
                            message: None,
                            ..row
                        },
                    )
                })?;
            }
        }
        drop(connection);
        drop(_cycle);
        self.wake(true, false);
        let status = self.build_status()?;
        self.environment().emit_status(&status);
        Ok(status)
    }

    pub(crate) fn resolve(
        &self,
        plugin_id: &str,
        conflict_id: &str,
        choice: &str,
    ) -> AppResult<SyncStatusView> {
        self.configured(plugin_id)?;
        let keep_remote = match choice {
            "local" => false,
            "remote" => true,
            _ => return Err(AppError::InvalidData("Choose local or remote".into())),
        };
        if conflict_id.len() > 64 {
            return Err(AppError::InvalidData("Invalid conflict ID".into()));
        }
        {
            let _cycle = self.cycle_guard()?;
            engine::resolve_conflict(&self.inner.store, conflict_id, keep_remote)?;
        }
        self.wake(true, false);
        let status = self.build_status()?;
        self.environment().emit_status(&status);
        Ok(status)
    }

    /// Records the renderer's portable preferences; `None` means not set here.
    pub(crate) fn put_preferences(
        &self,
        plugin_id: &str,
        preferences: BTreeMap<String, Option<String>>,
    ) -> AppResult<()> {
        self.configured(plugin_id)?;
        for (key, value) in &preferences {
            if !PREFERENCE_KEYS.contains(&key.as_str()) {
                return Err(AppError::InvalidData(format!(
                    "The {key} preference is not synced"
                )));
            }
            if value
                .as_ref()
                .is_some_and(|value| value.len() > MAX_PREFERENCE_BYTES)
            {
                return Err(AppError::InvalidData(format!(
                    "The {key} preference is too large"
                )));
            }
        }
        // Each push is a full snapshot: an omitted key is not set on this device.
        // Only values that differ are recorded, so only they become new versions.
        let now = engine::now_ms();
        self.inner.store.transaction(|transaction| {
            for key in PREFERENCE_KEYS {
                let value = preferences.get(key).cloned().flatten();
                store::set_preference(transaction, key, value.as_deref(), now)?;
            }
            Ok(())
        })
    }

    pub(crate) fn retire_device(
        &self,
        plugin_id: &str,
        writer_id: &str,
    ) -> AppResult<SyncStatusView> {
        let config = self.configured(plugin_id)?;
        validate_writer_id(writer_id)?;
        let keys = self.keys(&config)?.ok_or_else(needs_passphrase)?;
        let connection = self.inner.store.connection()?;
        let identity: Identity =
            store::get_meta(&connection, store::META_IDENTITY)?.ok_or_else(not_configured)?;
        if identity.writer_id == writer_id {
            return Err(AppError::State(
                "To remove this device, disconnect sync on it instead".into(),
            ));
        }
        let mut writer = store::list_writers(&connection)?
            .into_iter()
            .find(|writer| writer.writer_id == writer_id)
            .ok_or_else(|| AppError::NotFound("That device is unknown".into()))?;
        let record = serde_json::to_vec(&serde_json::json!({
            "writer": writer_id,
            "retiredBy": identity.writer_id,
            "retiredAt": db::now(),
        }))
        .map_err(|error| AppError::State(format!("Unable to record the removal: {error}")))?;
        let operation = self.begin(plugin_id);
        let backend = self.open_backend(&config, &operation)?;
        backend.refresh()?;
        backend.put(
            &engine::retired_key(writer_id),
            &keys.seal_retirement(writer_id, &record)?,
            PutMode::Overwrite,
        )?;
        backend.flush()?;
        writer.retired = true;
        store::upsert_writer(&connection, &writer)?;
        let status = self.build_status()?;
        self.environment().emit_status(&status);
        Ok(status)
    }

    /// Rewraps the same master key; it does not revoke devices that knew the old passphrase.
    pub(crate) fn change_passphrase(
        &self,
        plugin_id: &str,
        current: &str,
        next: &str,
    ) -> AppResult<SyncStatusView> {
        let config = self.configured(plugin_id)?;
        validate_passphrase(next)?;
        let keys_available = self.keys(&config)?.is_some();
        if !keys_available {
            return Err(needs_passphrase());
        }
        let operation = self.begin(plugin_id);
        let _cycle = self.cycle_guard()?;
        let backend = self.open_backend(&config, &operation)?;
        backend.refresh()?;
        let header = parse_header(&backend.get("repository.json")?.ok_or_else(|| {
            AppError::NotFound("The sync repository is missing from storage.".into())
        })?)?;
        if header.repository_id != config.repository_id {
            return Err(AppError::State(
                "The sync storage now holds a different repository".into(),
            ));
        }
        let master = unwrap_master(&header, current)?;
        let next_header = rewrap(&header, &master, next)?;
        backend.put(
            "repository.json",
            &serialize_header(&next_header)?,
            PutMode::Overwrite,
        )?;
        backend.flush()?;
        drop(_cycle);
        let status = self.build_status()?;
        self.environment().emit_status(&status);
        Ok(status)
    }

    /// Removes the local configuration, index, and keychain entries. Remote
    /// data and local files are left untouched.
    pub(crate) fn disconnect(&self, plugin_id: &str) -> AppResult<SyncStatusView> {
        let config = self.configured(plugin_id)?;
        self.cancel_operations();
        let _cycle = self.cycle_guard()?;
        let secrets = self.environment().secrets()?;
        secrets.delete(&config.plugin_id, SECRET_BACKEND)?;
        secrets.delete(&config.plugin_id, SECRET_MASTER)?;
        self.inner.store.reset()?;
        {
            let mut runtime = self.runtime()?;
            runtime.master = None;
            runtime.google.clear();
        }
        for private in ["staging", "backends"] {
            match fs::remove_dir_all(self.inner.sync_dir.join(private)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        drop(_cycle);
        let status = self.build_status()?;
        self.environment().emit_status(&status);
        Ok(status)
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
