//! One synchronization cycle. See `docs/sync.md` ("Synchronization").

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    apply::{Change, DirectoryCache, Expected, Outcome, apply_file},
    backend::{Backend, PutMode, PutOutcome},
    config::{MAX_PREFERENCE_BYTES, PREFERENCE_KEYS, PersistedConfig},
    crypto::{
        FORMAT_VERSION, Keys, MAX_FILE_BYTES, MAX_SNAPSHOT_BYTES, parse_header, random_hex_id,
        validate_object_id, validate_writer_id,
    },
    merge::merge_state,
    metadata::{self, DbVault},
    model::{
        Entry, EntryKind, EntryValue, Snapshot, VersionVector, file_key, present, same_presence,
        valid_vault_id,
    },
    plugin_data::{PluginDataSource, portable_setting},
    scan::{self, Availability, cancelled, read_file},
    store::{
        self, ConflictKind, ConflictRow, Fingerprint, Identity, IndexRow, Journal, Observed, Store,
        VaultRow, VaultState, WriterRow,
    },
};
use crate::{
    db,
    error::{AppError, AppResult},
};

/// Generations tried per writer before giving up on it for this cycle.
const MAX_FALLBACK_GENERATIONS: usize = 8;
const MAX_LISTED_OBJECTS: usize = 5_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Phase {
    Refresh,
    Scan,
    Merge,
    Download,
    Apply,
    Upload,
    Publish,
}

/// The application's native vault mutation lock.
pub(crate) trait VaultLock: Send + Sync {
    fn with_vault_lock(&self, operation: &mut dyn FnMut() -> AppResult<()>) -> AppResult<()>;
}

pub(crate) struct Engine<'a> {
    pub store: &'a Store,
    pub backend: &'a dyn Backend,
    pub keys: &'a Keys,
    pub app_db: &'a Path,
    pub plugins: Option<&'a dyn PluginDataSource>,
    pub vault_lock: &'a dyn VaultLock,
    pub staging: &'a Path,
    pub cancel: &'a AtomicBool,
    pub progress: &'a (dyn Fn(Phase, usize, usize) + Sync),
    pub app_version: &'a str,
}

/// What a cycle did. Filled in as the cycle runs, so a failed cycle still
/// reports the changes it applied before failing.
#[derive(Debug, Default)]
pub(crate) struct CycleReport {
    pub uploaded: usize,
    pub downloaded: usize,
    pub applied: usize,
    pub conflicts: usize,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
    pub vault_roots: BTreeSet<String>,
    pub preferences: BTreeMap<String, Option<String>>,
    pub plugin_ids: BTreeSet<String>,
    pub rotated: bool,
    pub published: Option<u64>,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub(crate) fn head_key(writer: &str, generation: u64) -> String {
    format!("heads/{writer}/{generation:020}")
}

pub(crate) fn object_key(id: &str) -> String {
    format!("objects/{}/{id}", &id[..2])
}

pub(crate) fn retired_key(writer: &str) -> String {
    format!("retired/{writer}")
}

fn parse_head_key(key: &str) -> Option<(String, u64)> {
    let rest = key.strip_prefix("heads/")?;
    let (writer, generation) = rest.split_once('/')?;
    validate_writer_id(writer).ok()?;
    if generation.len() != 20 || !generation.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((writer.to_string(), generation.parse().ok()?))
}

fn parse_object_key(key: &str) -> Option<[u8; 32]> {
    let rest = key.strip_prefix("objects/")?;
    let (prefix, id) = rest.split_once('/')?;
    validate_object_id(id).ok()?;
    if prefix != &id[..2] {
        return None;
    }
    hex::decode(id).ok()?.try_into().ok()
}

fn object_bytes(id: &str) -> Option<[u8; 32]> {
    hex::decode(id).ok()?.try_into().ok()
}

fn bump(vv: &VersionVector, writer: &str, counter: &mut u64) -> VersionVector {
    *counter += 1;
    let mut next = vv.clone();
    next.insert(writer.to_string(), *counter);
    next
}

/// Creates a fresh writer identity and configuration in an emptied store.
pub(crate) fn initialize_store(store: &Store, config: &PersistedConfig) -> AppResult<Identity> {
    store.reset()?;
    let identity = Identity {
        writer_id: random_hex_id()?,
        counter: 0,
        generation: 0,
    };
    store.transaction(|transaction| {
        store::set_meta(transaction, store::META_CONFIG, config)?;
        store::set_meta(transaction, store::META_IDENTITY, &identity)
    })?;
    Ok(identity)
}

struct Remote {
    writer: String,
    generation: u64,
    snapshot: Snapshot,
}

/// Uploads each new object once, before any snapshot references it.
struct Uploader<'a> {
    backend: &'a dyn Backend,
    keys: &'a Keys,
    objects: HashSet<[u8; 32]>,
    uploaded: usize,
}

impl Uploader<'_> {
    fn has(&self, id: &str) -> bool {
        object_bytes(id).is_some_and(|bytes| self.objects.contains(&bytes))
    }

    fn ensure(&mut self, id: &str, plaintext: &[u8]) -> AppResult<()> {
        let bytes = object_bytes(id)
            .ok_or_else(|| AppError::InvalidData("Invalid sync object identifier".into()))?;
        if self.objects.contains(&bytes) {
            return Ok(());
        }
        let envelope = self.keys.seal_object(id, plaintext)?;
        self.backend
            .put(&object_key(id), &envelope, PutMode::CreateOnly)?;
        self.objects.insert(bytes);
        self.uploaded += 1;
        Ok(())
    }
}

/// Local observations turned into new versions.
struct Observer<'a> {
    index: &'a mut BTreeMap<String, IndexRow>,
    /// New local versions, merged as the local side.
    changes: BTreeMap<String, Entry>,
    /// Index entries those versions replace, merged as one more input so a
    /// remote value that was never applied here is not silently overwritten.
    previous: BTreeMap<String, Entry>,
    writer: String,
    counter: u64,
}

impl Observer<'_> {
    fn observe(
        &mut self,
        key: &str,
        value: Option<EntryValue>,
        modified_at: i64,
        fingerprint: Option<Fingerprint>,
    ) {
        let observed = self.index.get(key).and_then(|row| row.observed.as_ref());
        if same_presence(observed.map(|observed| &observed.value), value.as_ref()) {
            if let Some(row) = self.index.get_mut(key)
                && fingerprint.is_some()
            {
                row.fingerprint = fingerprint;
            }
            return;
        }
        let base = observed
            .map(|observed| observed.vv.clone())
            .unwrap_or_default();
        let vv = bump(&base, &self.writer, &mut self.counter);
        let value = value.unwrap_or(EntryValue::Tombstone);
        let entry = Entry {
            value: value.clone(),
            modified_at,
            origin: self.writer.clone(),
            vv: vv.clone(),
        };
        match self.index.get_mut(key) {
            Some(row) => {
                self.previous.insert(key.to_string(), row.entry.clone());
                row.entry = entry.clone();
                row.observed = Some(Observed { value, vv });
                row.fingerprint = fingerprint;
            }
            None => {
                self.index.insert(
                    key.to_string(),
                    IndexRow {
                        entry: entry.clone(),
                        observed: Some(Observed { value, vv }),
                        fingerprint,
                        unconfirmed: false,
                    },
                );
            }
        }
        self.changes.insert(key.to_string(), entry);
    }

    /// Keys under `prefix` that were present and are now gone.
    fn removed(&self, prefix: &str, still: &dyn Fn(&str) -> bool) -> Vec<String> {
        self.index
            .range(prefix.to_string()..)
            .take_while(|(key, _)| key.starts_with(prefix))
            .filter(|(key, row)| {
                row.observed
                    .as_ref()
                    .is_some_and(|observed| !observed.value.is_tombstone())
                    && !still(key)
            })
            .map(|(key, _)| key.clone())
            .collect()
    }
}

/// A synced vault that can be scanned and applied this cycle.
struct ReadyVault {
    db: DbVault,
    root: PathBuf,
}

enum Task {
    File,
    Metadata,
    History,
    Preference(String),
    PluginSetting { plugin: String, key: String },
    PluginState { plugin: String, key: String },
    PluginEnabled(String),
}

fn inline_value(value: &EntryValue) -> Option<&Value> {
    match value {
        EntryValue::Inline { json } => Some(json),
        _ => None,
    }
}

fn conflict_id(key: &str, remote: &Entry) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hasher.update([0]);
    hasher.update(remote.value.order_key().as_bytes());
    hasher.update([0]);
    hasher.update(remote.origin.as_bytes());
    hex::encode(hasher.finalize())[..32].to_string()
}

fn summarize(value: Option<&EntryValue>) -> String {
    match value {
        None | Some(EntryValue::Tombstone) => "Not set".into(),
        Some(EntryValue::Object { id, size, .. }) => {
            format!("Version {} ({size} bytes)", &id[..8])
        }
        Some(EntryValue::Inline { json }) => {
            let shown = json.get("value").unwrap_or(json).to_string();
            if shown.chars().count() > 120 {
                format!("{}…", shown.chars().take(120).collect::<String>())
            } else {
                shown
            }
        }
    }
}

impl Engine<'_> {
    fn check_cancel(&self) -> AppResult<()> {
        if self.cancel.load(Ordering::SeqCst) {
            Err(cancelled())
        } else {
            Ok(())
        }
    }

    fn load_snapshot(&self, writer: &str, generation: u64) -> AppResult<Snapshot> {
        let envelope = self
            .backend
            .get(&head_key(writer, generation))?
            .ok_or_else(|| AppError::NotFound("A sync snapshot is missing".into()))?;
        if envelope.len() as u64 > MAX_SNAPSHOT_BYTES + 64 {
            return Err(AppError::InvalidData("A sync snapshot is too large".into()));
        }
        let plaintext = self.keys.open_head(writer, generation, &envelope)?;
        Snapshot::decode(&plaintext, self.keys.repository_id(), writer, generation)
    }

    fn complete(snapshot: &Snapshot, objects: &HashSet<[u8; 32]>) -> bool {
        snapshot.entries.values().all(|entry| {
            entry
                .value
                .object_id()
                .is_none_or(|id| object_bytes(id).is_some_and(|bytes| objects.contains(&bytes)))
        })
    }

    fn download(&self, id: &str) -> AppResult<PathBuf> {
        let path = self.staging.join(id);
        if path.is_file() {
            return Ok(path);
        }
        let envelope = self
            .backend
            .get(&object_key(id))?
            .ok_or_else(|| AppError::NotFound("A synced file is missing from storage".into()))?;
        let plaintext = self.keys.open_object(id, &envelope)?;
        let temporary = self.staging.join(format!("{id}.part"));
        fs::write(&temporary, &plaintext)?;
        fs::rename(&temporary, &path)?;
        Ok(path)
    }

    /// Runs one cycle: refresh, scan, merge, download, apply, publish, commit.
    pub(crate) fn run(&self, report: &mut CycleReport) -> AppResult<()> {
        match fs::remove_dir_all(self.staging) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        fs::create_dir_all(self.staging)?;
        let result = self.run_inner(report);
        if let Err(error) = fs::remove_dir_all(self.staging)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            report
                .warnings
                .push("Temporary sync files could not be removed.".into());
        }
        result
    }

    fn run_inner(&self, report: &mut CycleReport) -> AppResult<()> {
        let connection = self.store.connection()?;
        let config: PersistedConfig = store::get_meta(&connection, store::META_CONFIG)?
            .ok_or_else(|| AppError::State("Sync is not configured.".into()))?;
        if config.repository_id != self.keys.repository_id() {
            return Err(AppError::State(
                "The unlocked sync key belongs to another repository.".into(),
            ));
        }
        let options = config.options.clone();
        let mut identity: Identity = store::get_meta(&connection, store::META_IDENTITY)?
            .ok_or_else(|| AppError::State("Sync has no device identity.".into()))?;
        let journal = store::get_journal(&connection)?;
        let mut writers: BTreeMap<String, WriterRow> = store::list_writers(&connection)?
            .into_iter()
            .map(|writer| (writer.writer_id.clone(), writer))
            .collect();
        let mut vault_rows: BTreeMap<String, VaultRow> = store::list_vaults(&connection)?
            .into_iter()
            .map(|row| (row.sync_id.clone(), row))
            .collect();
        let before = store::load_index(&connection)?;
        let mut index = before.clone();
        let mut watermarks: BTreeMap<String, i64> =
            store::get_meta(&connection, store::META_HISTORY_WATERMARKS)?.unwrap_or_default();
        let publish_pending: bool =
            store::get_meta(&connection, store::META_PUBLISH_PENDING)?.unwrap_or(false);
        drop(connection);

        // 1. Refresh.
        (self.progress)(Phase::Refresh, 0, 1);
        self.backend.refresh()?;
        let header = self.backend.get("repository.json")?.ok_or_else(|| {
            AppError::NotFound("The sync repository is missing from storage.".into())
        })?;
        if parse_header(&header)?.repository_id != self.keys.repository_id() {
            return Err(AppError::State(
                "The sync storage now holds a different repository. Disconnect and set up sync again."
                    .into(),
            ));
        }
        let mut heads: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
        for key in self.backend.list("heads/")? {
            match parse_head_key(&key) {
                Some((writer, generation)) => {
                    heads.entry(writer).or_default().insert(generation);
                }
                None => report
                    .warnings
                    .push("Ignored an unrecognized entry in sync storage.".into()),
            }
        }
        let mut retired = BTreeSet::new();
        for key in self.backend.list("retired/")? {
            let Some(writer) = key
                .strip_prefix("retired/")
                .filter(|writer| validate_writer_id(writer).is_ok())
            else {
                continue;
            };
            if writers.get(writer).is_some_and(|row| row.retired) {
                retired.insert(writer.to_string());
                continue;
            }
            let verified = self
                .backend
                .get(&key)?
                .map(|envelope| self.keys.open_retirement(writer, &envelope).is_ok())
                .unwrap_or(false);
            if verified {
                retired.insert(writer.to_string());
            } else {
                report
                    .warnings
                    .push("Ignored a device removal record that failed verification.".into());
            }
        }
        let listed = self.backend.list("objects/")?;
        if listed.len() > MAX_LISTED_OBJECTS {
            return Err(AppError::InvalidData(
                "Sync storage holds more objects than this version supports".into(),
            ));
        }
        let objects: HashSet<[u8; 32]> = listed
            .iter()
            .filter_map(|key| parse_object_key(key))
            .collect();
        self.check_cancel()?;

        // Writer identity: a newer generation of our own, or a removal record,
        // means this application data was restored, copied, or retired.
        let mut remotes: Vec<Remote> = Vec::new();
        let own_max = heads
            .get(&identity.writer_id)
            .and_then(|generations| generations.last().copied());
        let mut rotate = retired.contains(&identity.writer_id);
        if let Some(own_max) = own_max.filter(|own_max| *own_max > identity.generation) {
            let interrupted = journal.as_ref().is_some_and(|journal| {
                journal.writer_id == identity.writer_id
                    && journal.generation == own_max
                    && own_max == identity.generation + 1
            });
            match interrupted
                .then(|| self.load_snapshot(&identity.writer_id, own_max))
                .transpose()
            {
                Ok(Some(snapshot)) => {
                    identity.generation = own_max;
                    identity.counter = identity
                        .counter
                        .max(journal.as_ref().map(|journal| journal.counter).unwrap_or(0));
                    remotes.push(Remote {
                        writer: identity.writer_id.clone(),
                        generation: own_max,
                        snapshot,
                    });
                }
                _ => rotate = true,
            }
        }
        if rotate {
            identity = Identity {
                writer_id: random_hex_id()?,
                counter: 0,
                generation: 0,
            };
            remotes.clear();
            for row in index.values_mut() {
                row.unconfirmed = true;
            }
            report.rotated = true;
            report.warnings.push(
                "This device's sync data was restored, copied, or removed elsewhere. It re-enrolled under a new identity; differing files are kept as conflict copies."
                    .into(),
            );
        }

        for (writer, generations) in &heads {
            if *writer == identity.writer_id || retired.contains(writer) {
                continue;
            }
            let last = writers.get(writer).map(|row| row.last_applied).unwrap_or(0);
            let Some(highest) = generations.last().copied() else {
                continue;
            };
            let name = writers
                .get(writer)
                .map(|row| row.name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "another device".into());
            if highest < last {
                report.warnings.push(format!(
                    "Sync data from {name} went back to an older state and was refused."
                ));
                continue;
            }
            let mut loaded = None;
            for generation in generations
                .iter()
                .rev()
                .copied()
                .filter(|generation| *generation >= last)
                .take(MAX_FALLBACK_GENERATIONS)
            {
                self.check_cancel()?;
                if let Ok(snapshot) = self.load_snapshot(writer, generation)
                    && Self::complete(&snapshot, &objects)
                {
                    loaded = Some((generation, snapshot));
                    break;
                }
            }
            match loaded {
                Some((generation, snapshot)) => {
                    if generation != highest {
                        report.warnings.push(format!(
                            "The latest sync data from {name} is incomplete; an earlier complete state was used."
                        ));
                    }
                    remotes.push(Remote {
                        writer: writer.clone(),
                        generation,
                        snapshot,
                    });
                }
                None => report.warnings.push(format!(
                    "Sync data from {name} could not be read or verified and was skipped."
                )),
            }
        }

        // Writer bookkeeping and newly offered vaults.
        let mut names: HashMap<String, String> = writers
            .values()
            .map(|row| (row.writer_id.clone(), row.name.clone()))
            .collect();
        names.insert(identity.writer_id.clone(), config.device_name.clone());
        for (writer, generations) in &heads {
            if *writer == identity.writer_id {
                continue;
            }
            let row = writers.entry(writer.clone()).or_insert_with(|| WriterRow {
                writer_id: writer.clone(),
                name: String::new(),
                last_applied: 0,
                highest_seen: 0,
                last_seen_at: None,
                retired: false,
            });
            row.highest_seen = generations.last().copied().unwrap_or(0);
            row.retired = retired.contains(writer);
        }
        for writer in &retired {
            if let Some(row) = writers.get_mut(writer) {
                row.retired = true;
            }
        }
        for remote in &remotes {
            if remote.writer == identity.writer_id {
                continue;
            }
            if let Some(row) = writers.get_mut(&remote.writer) {
                row.last_applied = remote.generation;
                row.name = remote.snapshot.device_name.clone();
                row.last_seen_at = Some(remote.snapshot.created_at.clone());
            }
            names.insert(remote.writer.clone(), remote.snapshot.device_name.clone());
            let mut offered: BTreeMap<String, String> = remote.snapshot.vaults.clone();
            for key in remote.snapshot.entries.keys() {
                if let Some(vault) = key
                    .strip_prefix("v/")
                    .and_then(|rest| rest.split('/').next())
                    .filter(|vault| valid_vault_id(vault))
                {
                    offered
                        .entry(vault.to_string())
                        .or_insert_with(|| "Vault".into());
                }
            }
            for (sync_id, name) in offered {
                vault_rows
                    .entry(sync_id.clone())
                    .or_insert_with(|| VaultRow {
                        sync_id,
                        scope_id: None,
                        name,
                        state: VaultState::PendingMapping,
                        paused: false,
                        message: None,
                    });
            }
        }

        // Resolve synced vaults on this device.
        let app = db::open(self.app_db)?;
        let mut ready: BTreeMap<String, ReadyVault> = BTreeMap::new();
        for row in vault_rows.values_mut() {
            if row.state != VaultState::Synced {
                continue;
            }
            let resolved = match &row.scope_id {
                Some(scope) => metadata::vault_by_scope(&app, scope)?,
                None => None,
            };
            let Some(db_vault) = resolved else {
                row.message = Some("This vault is no longer known on this device.".into());
                continue;
            };
            let root = PathBuf::from(&db_vault.path);
            match scan::availability(&root) {
                Availability::Ready => {
                    row.message = None;
                    row.name = db_vault.name.clone();
                    ready.insert(row.sync_id.clone(), ReadyVault { db: db_vault, root });
                }
                Availability::Unavailable(message) => row.message = Some(message),
                Availability::Transition => {
                    row.message =
                        Some("Encryption is being changed; sync resumes when it finishes.".into())
                }
            }
        }

        // 2. Scan.
        let mut uploader = Uploader {
            backend: self.backend,
            keys: self.keys,
            objects,
            uploaded: 0,
        };
        let mut disk_paths: HashMap<String, PathBuf> = HashMap::new();
        let mut observer = Observer {
            index: &mut index,
            changes: BTreeMap::new(),
            previous: BTreeMap::new(),
            writer: identity.writer_id.clone(),
            counter: identity.counter,
        };
        let total_vaults = ready.len();
        let mut unscannable = Vec::new();
        for (position, (sync_id, vault)) in ready.iter().enumerate() {
            (self.progress)(Phase::Scan, position, total_vaults);
            let walk = match scan::walk_vault(&vault.root, self.cancel) {
                Ok(walk) => walk,
                Err(error) if self.cancel.load(Ordering::SeqCst) => return Err(error),
                Err(error) => {
                    report
                        .warnings
                        .push(format!("{} was not synced: {error}", vault.db.name));
                    unscannable.push(sync_id.clone());
                    continue;
                }
            };
            report.skipped.extend(
                walk.skipped
                    .iter()
                    .map(|skipped| format!("{}: {skipped}", vault.db.name)),
            );
            for (path, file) in &walk.files {
                let key = file_key(sync_id, path);
                disk_paths.insert(key.clone(), file.disk_path.clone());
                let unchanged = observer.index.get(&key).is_some_and(|row| {
                    row.fingerprint == Some(file.fingerprint)
                        && row
                            .observed
                            .as_ref()
                            .is_some_and(|observed| observed.value.object_id().is_some())
                });
                if unchanged {
                    continue;
                }
                self.check_cancel()?;
                let bytes = match read_file(&vault.root, &file.disk_path, MAX_FILE_BYTES) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        report
                            .skipped
                            .push(format!("{}: {path}: {error}", vault.db.name));
                        continue;
                    }
                };
                let id = self.keys.object_id(&bytes)?;
                uploader.ensure(&id, &bytes)?;
                observer.observe(
                    &key,
                    Some(EntryValue::Object {
                        id,
                        size: bytes.len() as u64,
                        mode: file.mode,
                    }),
                    file.modified_ms,
                    Some(file.fingerprint),
                );
            }
            if walk.complete {
                let prefix = format!("v/{sync_id}/f/");
                let still = |key: &str| {
                    key.strip_prefix(&prefix).is_some_and(|path| {
                        walk.files.contains_key(path) || walk.held.contains(path)
                    })
                };
                for key in observer.removed(&prefix, &still) {
                    observer.observe(&key, None, now_ms(), None);
                }
            }

            let observed = metadata::observe(&app, &vault.db, sync_id)?;
            for (key, value) in &observed {
                observer.observe(key, Some(value.clone()), now_ms(), None);
            }
            let prefix = format!("v/{sync_id}/");
            let still = |key: &str| {
                observed.contains_key(key)
                    || !matches!(
                        EntryKind::parse(key),
                        EntryKind::VaultMeta { .. }
                            | EntryKind::Note { .. }
                            | EntryKind::Tag { .. }
                            | EntryKind::Project { .. }
                    )
            };
            for key in observer.removed(&prefix, &still) {
                observer.observe(&key, None, now_ms(), None);
            }

            if options.sync_history {
                let watermark = watermarks.get(sync_id).copied().unwrap_or(0);
                let rows = metadata::history_since(&app, vault.db.id, watermark)?;
                for (row_id, record) in rows {
                    self.check_cancel()?;
                    let bytes = metadata::encode_history(&record)?;
                    if bytes.len() as u64 > MAX_FILE_BYTES {
                        watermarks.insert(sync_id.clone(), row_id);
                        continue;
                    }
                    let id = self.keys.object_id(&bytes)?;
                    uploader.ensure(&id, &bytes)?;
                    let created = record
                        .created_at
                        .parse::<jiff::Timestamp>()
                        .map(|timestamp| timestamp.as_millisecond())
                        .unwrap_or_else(|_| now_ms());
                    observer.observe(
                        &metadata::history_key(sync_id, &id),
                        Some(EntryValue::Object {
                            id,
                            size: bytes.len() as u64,
                            mode: 0,
                        }),
                        created,
                        None,
                    );
                    watermarks.insert(sync_id.clone(), row_id);
                }
            }
        }
        for sync_id in unscannable {
            ready.remove(&sync_id);
        }

        if options.sync_preferences {
            let connection = self.store.connection()?;
            let preferences = store::list_preferences(&connection)?;
            for key in PREFERENCE_KEYS {
                let (value, updated_at) = preferences
                    .get(key)
                    .map(|(value, updated_at)| (value.clone(), *updated_at))
                    .unwrap_or((None, 0));
                observer.observe(
                    &format!("app/preferences/{key}"),
                    value.map(|value| EntryValue::Inline {
                        json: json!({ "value": value }),
                    }),
                    if updated_at > 0 { updated_at } else { now_ms() },
                    None,
                );
            }
        }

        let mut enabled_plugins: BTreeSet<String> = BTreeSet::new();
        let mut plugin_versions: BTreeMap<String, u32> = BTreeMap::new();
        let plugins_active = options.sync_plugin_data && self.plugins.is_some();
        if let (true, Some(plugins)) = (plugins_active, self.plugins) {
            let (views, warnings) = plugins.enabled_plugins()?;
            report.warnings.extend(warnings);
            for view in views {
                if !segment_ok(&view.plugin_id) {
                    continue;
                }
                let plugin = view.plugin_id.clone();
                enabled_plugins.insert(plugin.clone());
                plugin_versions.insert(plugin.clone(), view.settings_version);
                let mut present_keys = BTreeSet::new();
                for (key, value) in &view.settings {
                    if !portable_setting(key) || !segment_ok(key) {
                        continue;
                    }
                    let entry_key = format!("plugin/{plugin}/settings/{key}");
                    present_keys.insert(entry_key.clone());
                    observer.observe(
                        &entry_key,
                        Some(EntryValue::Inline {
                            json: json!({ "version": view.settings_version, "value": value }),
                        }),
                        now_ms(),
                        None,
                    );
                }
                for (key, value) in &view.state {
                    if !segment_ok(key) {
                        continue;
                    }
                    let entry_key = format!("plugin/{plugin}/state/{key}");
                    present_keys.insert(entry_key.clone());
                    observer.observe(
                        &entry_key,
                        Some(EntryValue::Inline {
                            json: json!({ "value": value }),
                        }),
                        now_ms(),
                        None,
                    );
                }
                for prefix in [
                    format!("plugin/{plugin}/settings/"),
                    format!("plugin/{plugin}/state/"),
                ] {
                    let still = |key: &str| present_keys.contains(key);
                    for key in observer.removed(&prefix, &still) {
                        observer.observe(&key, None, now_ms(), None);
                    }
                }
                observer.observe(
                    &format!("plugin/{plugin}/enabled"),
                    Some(EntryValue::Inline {
                        json: json!({ "enabled": true }),
                    }),
                    now_ms(),
                    None,
                );
            }
            let disabled: Vec<String> = observer
                .removed("plugin/", &|key: &str| {
                    !matches!(EntryKind::parse(key), EntryKind::PluginEnabled { plugin } if !enabled_plugins.contains(plugin))
                });
            for key in disabled {
                observer.observe(&key, None, now_ms(), None);
            }
        }
        identity.counter = observer.counter;
        let changes = std::mem::take(&mut observer.changes);
        let previous = std::mem::take(&mut observer.previous);
        drop(observer);
        self.check_cancel()?;

        // 3. Merge.
        (self.progress)(Phase::Merge, 0, 1);
        let include = |key: &str| -> bool {
            match EntryKind::parse(key) {
                EntryKind::History { vault, .. } => {
                    options.sync_history && ready.contains_key(vault)
                }
                EntryKind::Preference { key } => {
                    options.sync_preferences && PREFERENCE_KEYS.contains(&key)
                }
                EntryKind::PluginSetting { .. }
                | EntryKind::PluginState { .. }
                | EntryKind::PluginEnabled { .. } => plugins_active,
                EntryKind::Unknown => match key
                    .strip_prefix("v/")
                    .and_then(|rest| rest.split('/').next())
                {
                    Some(vault) => ready.contains_key(vault),
                    None => true,
                },
                kind => kind.vault().is_some_and(|vault| ready.contains_key(vault)),
            }
        };
        let local: BTreeMap<String, Entry> = index
            .iter()
            .map(|(key, row)| (key.clone(), row.entry.clone()))
            .collect();
        let mut inputs: Vec<&BTreeMap<String, Entry>> = remotes
            .iter()
            .map(|remote| &remote.snapshot.entries)
            .collect();
        inputs.push(&previous);
        let merged = merge_state(
            &local,
            &|key| index.get(key).is_some_and(|row| row.unconfirmed),
            &inputs,
            &include,
        );
        for (key, entry) in &merged.entries {
            match index.get_mut(key) {
                Some(row) => row.entry = entry.clone(),
                None => {
                    index.insert(
                        key.clone(),
                        IndexRow {
                            entry: entry.clone(),
                            observed: None,
                            fingerprint: None,
                            unconfirmed: false,
                        },
                    );
                }
            }
        }
        for row in vault_rows.values_mut() {
            row.paused = merged.paused_vaults.contains(&row.sync_id);
            if row.paused {
                row.message = Some(
                    "The vault's encryption was changed on two devices. Choose which to keep."
                        .into(),
                );
            }
        }
        let new_copies = merged
            .copies
            .iter()
            .filter(|key| !before.contains_key(*key))
            .count();
        let detected_at = db::now();
        let conflicts: Vec<ConflictRow> = merged
            .pending
            .iter()
            .map(|(key, remote)| {
                let kind = EntryKind::parse(key);
                ConflictRow {
                    id: conflict_id(key, remote),
                    kind: match kind {
                        EntryKind::PluginSetting { .. } => ConflictKind::PluginSetting,
                        EntryKind::PluginState { .. } => ConflictKind::PluginState,
                        _ => ConflictKind::EncryptionManifest,
                    },
                    key: key.clone(),
                    vault_sync_id: kind.vault().map(str::to_string),
                    plugin_id: kind.plugin().map(str::to_string),
                    local: index.get(key).map(|row| row.entry.clone()),
                    remote: remote.clone(),
                    remote_device: names
                        .get(&remote.origin)
                        .cloned()
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| "another device".into()),
                    detected_at: detected_at.clone(),
                }
            })
            .collect();
        report.conflicts = new_copies + conflicts.len();

        // Decide what to apply.
        let mut tasks: Vec<(String, Task)> = Vec::new();
        for (key, row) in index.iter_mut() {
            if !include(key) {
                continue;
            }
            let observed_value = row.observed.as_ref().map(|observed| &observed.value);
            if same_presence(observed_value, Some(&row.entry.value)) {
                // Already in place: adopt the merged version so the next local
                // change descends from it.
                row.observed = Some(Observed {
                    value: row.entry.value.clone(),
                    vv: row.entry.vv.clone(),
                });
                continue;
            }
            let kind = EntryKind::parse(key);
            if kind
                .vault()
                .is_some_and(|vault| merged.paused_vaults.contains(vault))
            {
                continue;
            }
            let task = match kind {
                EntryKind::VaultFile { .. } => Task::File,
                EntryKind::VaultMeta { .. }
                | EntryKind::Note { .. }
                | EntryKind::Tag { .. }
                | EntryKind::Project { .. } => Task::Metadata,
                EntryKind::History { .. } => {
                    if row.entry.value.is_tombstone() {
                        continue;
                    }
                    Task::History
                }
                EntryKind::Preference { key } => Task::Preference(key.to_string()),
                EntryKind::PluginSetting { plugin, key } => Task::PluginSetting {
                    plugin: plugin.to_string(),
                    key: key.to_string(),
                },
                EntryKind::PluginState { plugin, key } => Task::PluginState {
                    plugin: plugin.to_string(),
                    key: key.to_string(),
                },
                EntryKind::PluginEnabled { plugin } => Task::PluginEnabled(plugin.to_string()),
                EntryKind::Unknown => continue,
            };
            tasks.push((key.clone(), task));
        }

        // 4. Prepare: download and verify every object needed locally.
        let needed: BTreeSet<String> = tasks
            .iter()
            .filter(|(_, task)| matches!(task, Task::File | Task::History))
            .filter_map(|(key, _)| index.get(key)?.entry.value.object_id().map(str::to_string))
            .collect();
        let mut staged: HashMap<String, PathBuf> = HashMap::new();
        for (position, id) in needed.iter().enumerate() {
            (self.progress)(Phase::Download, position, needed.len());
            self.check_cancel()?;
            match self.download(id) {
                Ok(path) => {
                    staged.insert(id.clone(), path);
                    report.downloaded += 1;
                }
                Err(error) => report.warnings.push(format!(
                    "A synced file could not be downloaded and will be retried: {error}"
                )),
            }
        }

        // 5. Apply.
        let total = tasks.len();
        let mut done = 0usize;
        let mut by_vault: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (key, task) in &tasks {
            if let (Task::File, Some(vault)) = (task, EntryKind::parse(key).vault()) {
                by_vault
                    .entry(vault.to_string())
                    .or_default()
                    .push(key.clone());
            }
        }
        for (sync_id, keys) in &by_vault {
            let Some(vault) = ready.get(sync_id) else {
                continue;
            };
            let mut applied_here = 0usize;
            let mut run = || -> AppResult<()> {
                let _vault_lock = crate::vault::acquire_vault_control_lock(&vault.root)?;
                let mut cache = DirectoryCache::default();
                for key in keys {
                    self.check_cancel()?;
                    done += 1;
                    (self.progress)(Phase::Apply, done, total);
                    let Some(row) = index.get_mut(key) else {
                        continue;
                    };
                    let path = key
                        .strip_prefix(&format!("v/{sync_id}/f/"))
                        .unwrap_or_default()
                        .to_string();
                    let expected = match row
                        .observed
                        .as_ref()
                        .and_then(|observed| present(Some(&observed.value)))
                    {
                        Some(EntryValue::Object { id, .. }) => match row.fingerprint {
                            Some(fingerprint) => Expected::File {
                                disk_path: disk_paths
                                    .get(key)
                                    .cloned()
                                    .unwrap_or_else(|| PathBuf::from(&path)),
                                fingerprint,
                                id: id.clone(),
                            },
                            None => continue,
                        },
                        Some(_) => continue,
                        None => Expected::Absent,
                    };
                    let staged_path;
                    let change = match &row.entry.value {
                        EntryValue::Object { id, mode, .. } => {
                            let Some(path) = staged.get(id) else {
                                continue;
                            };
                            staged_path = path.clone();
                            Change::Write {
                                staged: &staged_path,
                                mode: *mode,
                                modified_ms: row.entry.modified_at,
                            }
                        }
                        EntryValue::Tombstone => Change::Remove,
                        EntryValue::Inline { .. } => {
                            report
                                .skipped
                                .push(format!("{}: {path}: invalid synced value", vault.db.name));
                            continue;
                        }
                    };
                    match apply_file(&vault.root, &path, &expected, change, self.keys, &mut cache) {
                        Ok(Outcome::Applied(fingerprint)) => {
                            row.observed = Some(Observed {
                                value: row.entry.value.clone(),
                                vv: row.entry.vv.clone(),
                            });
                            row.fingerprint = fingerprint;
                            applied_here += 1;
                        }
                        Ok(Outcome::Skipped(reason)) => {
                            report.skipped.push(format!("{}: {reason}", vault.db.name))
                        }
                        Err(error) => report
                            .skipped
                            .push(format!("{}: {path}: {error}", vault.db.name)),
                    }
                }
                Ok(())
            };
            let result = self.vault_lock.with_vault_lock(&mut run);
            if applied_here > 0 {
                report.applied += applied_here;
                report.vault_roots.insert(vault.db.path.clone());
            }
            result?;
        }

        let mut app = app;
        let store_connection = self.store.connection()?;
        for (key, task) in &tasks {
            if matches!(task, Task::File) {
                continue;
            }
            self.check_cancel()?;
            done += 1;
            (self.progress)(Phase::Apply, done, total);
            let Some(row) = index.get_mut(key) else {
                continue;
            };
            let kind = EntryKind::parse(key);
            let value = present(Some(&row.entry.value));
            let applied: AppResult<bool> = match task {
                Task::File => Ok(false),
                Task::Metadata => match kind.vault().and_then(|vault| ready.get(vault)) {
                    Some(vault) => metadata::apply(&mut app, vault.db.id, &kind, value).map(|()| {
                        report.vault_roots.insert(vault.db.path.clone());
                        true
                    }),
                    None => Ok(false),
                },
                Task::History => {
                    match (
                        kind.vault().and_then(|vault| ready.get(vault)),
                        row.entry.value.object_id().and_then(|id| staged.get(id)),
                    ) {
                        (Some(vault), Some(path)) => fs::read(path)
                            .map_err(AppError::from)
                            .and_then(|bytes| metadata::decode_history(&bytes))
                            .and_then(|record| {
                                metadata::apply_history(&mut app, vault.db.id, &record)
                            })
                            .map(|()| true),
                        _ => Ok(false),
                    }
                }
                Task::Preference(preference) => {
                    let text = match value.and_then(inline_value) {
                        Some(json) => json
                            .get("value")
                            .and_then(Value::as_str)
                            .filter(|text| text.len() <= MAX_PREFERENCE_BYTES)
                            .map(str::to_string)
                            .ok_or_else(|| {
                                AppError::InvalidData("A synced preference is invalid".into())
                            })
                            .map(Some),
                        None => Ok(None),
                    };
                    text.and_then(|text| {
                        store::set_preference(
                            &store_connection,
                            preference,
                            text.as_deref(),
                            now_ms(),
                        )?;
                        report.preferences.insert(preference.clone(), text);
                        Ok(true)
                    })
                }
                Task::PluginSetting { plugin, key } => match self.plugins {
                    Some(plugins) => {
                        let parsed = match value.and_then(inline_value) {
                            Some(json) => json
                                .get("version")
                                .and_then(Value::as_u64)
                                .and_then(|version| u32::try_from(version).ok())
                                .zip(json.get("value"))
                                .map(|(version, value)| (version, Some(value)))
                                .ok_or_else(|| {
                                    AppError::InvalidData(
                                        "A synced plugin setting is invalid".into(),
                                    )
                                }),
                            None => Ok((plugin_versions.get(plugin).copied().unwrap_or(0), None)),
                        };
                        parsed.and_then(|(version, value)| {
                            let applied = plugins.apply_setting(plugin, version, key, value)?;
                            if applied {
                                report.plugin_ids.insert(plugin.clone());
                            }
                            Ok(applied)
                        })
                    }
                    None => Ok(false),
                },
                Task::PluginState { plugin, key } => match self.plugins {
                    Some(plugins) => {
                        let parsed = match value.and_then(inline_value) {
                            Some(json) => json.get("value").map(Some).ok_or_else(|| {
                                AppError::InvalidData("A synced plugin value is invalid".into())
                            }),
                            None => Ok(None),
                        };
                        parsed.and_then(|value| {
                            let applied = plugins.apply_state(plugin, key, value)?;
                            if applied {
                                report.plugin_ids.insert(plugin.clone());
                            }
                            Ok(applied)
                        })
                    }
                    None => Ok(false),
                },
                Task::PluginEnabled(plugin) => {
                    // Suggestions only: never installs or enables anything.
                    let suggested = value.is_some() && !enabled_plugins.contains(plugin);
                    let device = names
                        .get(&row.entry.origin)
                        .cloned()
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| "another device".into());
                    store::set_suggestion(
                        &store_connection,
                        plugin,
                        suggested.then_some(device.as_str()),
                    )
                    .map(|()| false)
                }
            };
            match applied {
                Ok(true) => {
                    row.observed = Some(Observed {
                        value: row.entry.value.clone(),
                        vv: row.entry.vv.clone(),
                    });
                    report.applied += 1;
                }
                Ok(false) => {}
                Err(error) => report.skipped.push(format!("{key}: {error}")),
            }
        }
        for plugin in &enabled_plugins {
            store::set_suggestion(&store_connection, plugin, None)?;
        }
        self.check_cancel()?;

        // 6. Publish.
        let changed = identity.generation == 0
            || report.rotated
            || publish_pending
            || !changes.is_empty()
            || index.iter().any(|(key, row)| {
                before
                    .get(key)
                    .is_none_or(|previous| previous.entry != row.entry)
            });
        if changed {
            let mut entries = BTreeMap::new();
            let referenced: Vec<(String, String)> = index
                .iter()
                .filter_map(|(key, row)| {
                    row.entry
                        .value
                        .object_id()
                        .filter(|id| !uploader.has(id))
                        .map(|id| (key.clone(), id.to_string()))
                })
                .collect();
            let mut unavailable = BTreeSet::new();
            for (position, (key, id)) in referenced.iter().enumerate() {
                (self.progress)(Phase::Upload, position, referenced.len());
                self.check_cancel()?;
                if !self.repair(key, id, &index, &ready, &disk_paths, &mut uploader)? {
                    unavailable.insert(key.clone());
                }
            }
            if !unavailable.is_empty() {
                report.warnings.push(format!(
                    "{} synced item(s) are not in storage yet and were left out of this device's published state.",
                    unavailable.len()
                ));
            }
            for (key, row) in &index {
                if !unavailable.contains(key) {
                    entries.insert(key.clone(), row.entry.clone());
                }
            }
            let generation = identity.generation + 1;
            let vaults = vault_rows
                .values()
                .filter(|row| row.state == VaultState::Synced)
                .filter_map(|row| {
                    ready
                        .get(&row.sync_id)
                        .map(|vault| (row.sync_id.clone(), vault.db.name.clone()))
                })
                .collect();
            let snapshot = Snapshot {
                format: FORMAT_VERSION,
                repository: self.keys.repository_id().to_string(),
                writer: identity.writer_id.clone(),
                generation,
                created_at: db::now(),
                app_version: self.app_version.to_string(),
                device_name: config.device_name.clone(),
                vaults,
                entries,
            };
            let envelope =
                self.keys
                    .seal_head(&identity.writer_id, generation, &snapshot.encode()?)?;
            (self.progress)(Phase::Publish, 0, 1);
            self.store.transaction(|transaction| {
                store::set_journal(
                    transaction,
                    &Journal {
                        writer_id: identity.writer_id.clone(),
                        generation,
                        counter: identity.counter,
                        started_at: db::now(),
                    },
                )
            })?;
            let outcome = self.backend.put(
                &head_key(&identity.writer_id, generation),
                &envelope,
                PutMode::CreateOnly,
            )?;
            if outcome == PutOutcome::AlreadyExists {
                self.store
                    .transaction(|transaction| store::clear_journal(transaction))?;
                return Err(AppError::State(
                    "Another device is publishing under this device's identity. Sync will re-enroll this device on the next run."
                        .into(),
                ));
            }
            self.backend.flush()?;
            identity.generation = generation;
            report.published = Some(generation);
            for row in index.values_mut() {
                row.unconfirmed = false;
            }
        }
        report.uploaded = uploader.uploaded;

        // 7. Commit counter, index, and journal together.
        self.store.transaction(|transaction| {
            store::write_index_changes(transaction, &before, &index)?;
            store::set_meta(transaction, store::META_IDENTITY, &identity)?;
            store::set_meta(transaction, store::META_HISTORY_WATERMARKS, &watermarks)?;
            if report.published.is_some() {
                store::set_meta(transaction, store::META_PUBLISH_PENDING, &false)?;
            }
            for writer in writers.values() {
                store::upsert_writer(transaction, writer)?;
            }
            for row in vault_rows.values() {
                store::upsert_vault(transaction, row)?;
            }
            store::replace_conflicts(transaction, &conflicts)?;
            store::clear_journal(transaction)
        })?;
        Ok(())
    }

    /// Re-uploads an object this device can still produce from its own disk.
    fn repair(
        &self,
        key: &str,
        id: &str,
        index: &BTreeMap<String, IndexRow>,
        ready: &BTreeMap<String, ReadyVault>,
        disk_paths: &HashMap<String, PathBuf>,
        uploader: &mut Uploader<'_>,
    ) -> AppResult<bool> {
        let Some(row) = index.get(key) else {
            return Ok(false);
        };
        let observed_here = row
            .observed
            .as_ref()
            .and_then(|observed| observed.value.object_id())
            == Some(id);
        let (EntryKind::VaultFile { vault, path }, true) = (EntryKind::parse(key), observed_here)
        else {
            return Ok(false);
        };
        let Some(vault) = ready.get(vault) else {
            return Ok(false);
        };
        let disk_path = disk_paths
            .get(key)
            .cloned()
            .unwrap_or_else(|| PathBuf::from(path));
        let Ok(bytes) = read_file(&vault.root, &disk_path, MAX_FILE_BYTES) else {
            return Ok(false);
        };
        if self.keys.object_id(&bytes)? != id {
            return Ok(false);
        }
        uploader.ensure(id, &bytes)?;
        Ok(true)
    }
}

fn segment_ok(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains('/')
        && value != "."
        && value != ".."
        && !value.chars().any(char::is_control)
}

/// Resolves a pending conflict with a new local version that dominates both
/// sides; the next cycle applies and publishes it.
pub(crate) fn resolve_conflict(
    store: &Store,
    conflict_id: &str,
    keep_remote: bool,
) -> AppResult<()> {
    store.transaction(|transaction| {
        let conflict = store::list_conflicts(transaction)?
            .into_iter()
            .find(|conflict| conflict.id == conflict_id)
            .ok_or_else(|| AppError::NotFound("That sync conflict was already resolved.".into()))?;
        let mut identity: Identity = store::get_meta(transaction, store::META_IDENTITY)?
            .ok_or_else(|| AppError::State("Sync has no device identity.".into()))?;
        let before = store::load_index(transaction)?;
        let mut index = before.clone();
        let base = index
            .get(&conflict.key)
            .map(|row| row.entry.clone())
            .or_else(|| conflict.local.clone());
        let mut vv = crate::sync::merge::pointwise_max(
            &base
                .as_ref()
                .map(|entry| entry.vv.clone())
                .unwrap_or_default(),
            &conflict.remote.vv,
        );
        vv = bump(&vv, &identity.writer_id, &mut identity.counter);
        let value = if keep_remote {
            conflict.remote.value.clone()
        } else {
            base.map(|entry| entry.value)
                .unwrap_or(EntryValue::Tombstone)
        };
        let entry = Entry {
            value,
            modified_at: now_ms(),
            origin: identity.writer_id.clone(),
            vv,
        };
        match index.get_mut(&conflict.key) {
            Some(row) => row.entry = entry,
            None => {
                index.insert(
                    conflict.key.clone(),
                    IndexRow {
                        entry,
                        observed: None,
                        fingerprint: None,
                        unconfirmed: false,
                    },
                );
            }
        }
        store::write_index_changes(transaction, &before, &index)?;
        store::set_meta(transaction, store::META_IDENTITY, &identity)?;
        store::set_meta(transaction, store::META_PUBLISH_PENDING, &true)?;
        store::delete_conflicts_for_key(transaction, &conflict.key)
    })
}

/// Human summaries for the status view.
pub(crate) fn conflict_summaries(conflict: &ConflictRow) -> (String, String) {
    (
        summarize(conflict.local.as_ref().map(|entry| &entry.value)),
        summarize(Some(&conflict.remote.value)),
    )
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
