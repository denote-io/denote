//! Device-local sync state in `sync/sync.sqlite3`. See `docs/sync.md` ("Synchronization").

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::model::{Entry, EntryValue, VersionVector};
use crate::error::{AppError, AppResult};

pub(crate) const STORE_FILE: &str = "sync.sqlite3";

pub(crate) const META_CONFIG: &str = "config";
pub(crate) const META_IDENTITY: &str = "identity";
pub(crate) const META_LAST_RUN: &str = "last-run";
pub(crate) const META_HISTORY_WATERMARKS: &str = "history-watermarks";
/// Set when the index changed outside a cycle and must be published.
pub(crate) const META_PUBLISH_PENDING: &str = "publish-pending";
/// Sync IDs of vaults whose folder held Denote's `.denote` folder when scanned.
pub(crate) const META_VAULT_MARKERS: &str = "vault-markers";
/// Deletions held back per vault sync ID; see [`DeletionHold`].
pub(crate) const META_DELETION_HOLDS: &str = "deletion-holds";

/// A scan that would delete most of a vault publishes nothing for it until the
/// files come back or the user confirms the deletions.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeletionHold {
    /// Synced files missing from the folder.
    pub missing: usize,
    /// Synced files the folder held before.
    pub observed: usize,
    /// The user chose to apply up to `missing` deletions on the next scan.
    pub confirmed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub size: u64,
    pub mtime_ns: i64,
}

/// The local state a key was last observed in or applied to, and the version
/// that state corresponds to. A new local change descends from this version.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Observed {
    pub value: EntryValue,
    pub vv: VersionVector,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexRow {
    /// The merged entry this device publishes.
    pub entry: Entry,
    pub observed: Option<Observed>,
    /// Disk state of `observed` (vault files only).
    pub fingerprint: Option<Fingerprint>,
    /// Restored, cloned, or re-enrolled state compared as concurrent with every remote.
    pub unconfirmed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Identity {
    pub writer_id: String,
    /// Highest version-vector counter this writer has assigned.
    pub counter: u64,
    /// Highest generation this writer has committed.
    pub generation: u64,
}

/// A publish in progress: written before the head, cleared with the commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Journal {
    pub writer_id: String,
    pub generation: u64,
    pub counter: u64,
    pub started_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WriterRow {
    pub writer_id: String,
    pub name: String,
    pub last_applied: u64,
    pub highest_seen: u64,
    pub last_seen_at: Option<String>,
    pub retired: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VaultState {
    Synced,
    PendingMapping,
    Skipped,
    Stopped,
}

impl VaultState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Synced => "synced",
            Self::PendingMapping => "pending-mapping",
            Self::Skipped => "skipped",
            Self::Stopped => "stopped",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        Ok(match value {
            "synced" => Self::Synced,
            "pending-mapping" => Self::PendingMapping,
            "skipped" => Self::Skipped,
            "stopped" => Self::Stopped,
            other => {
                return Err(AppError::InvalidData(format!(
                    "Unknown sync vault state {other}"
                )));
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VaultRow {
    pub sync_id: String,
    /// Local `vaults.plugin_scope_id` once mapped.
    pub scope_id: Option<String>,
    pub name: String,
    pub state: VaultState,
    /// Paused for an encryption-manifest choice.
    pub paused: bool,
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConflictKind {
    EncryptionManifest,
    PluginSetting,
    PluginState,
}

impl ConflictKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::EncryptionManifest => "encryption-manifest",
            Self::PluginSetting => "plugin-setting",
            Self::PluginState => "plugin-state",
        }
    }

    fn parse(value: &str) -> AppResult<Self> {
        Ok(match value {
            "encryption-manifest" => Self::EncryptionManifest,
            "plugin-setting" => Self::PluginSetting,
            "plugin-state" => Self::PluginState,
            other => {
                return Err(AppError::InvalidData(format!(
                    "Unknown sync conflict kind {other}"
                )));
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConflictRow {
    pub id: String,
    pub kind: ConflictKind,
    pub key: String,
    pub vault_sync_id: Option<String>,
    pub plugin_id: Option<String>,
    pub local: Option<Entry>,
    pub remote: Entry,
    pub remote_device: String,
    pub detected_at: String,
}

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
}

impl Store {
    pub(crate) fn open(directory: &Path) -> AppResult<Self> {
        std::fs::create_dir_all(directory)?;
        let store = Self {
            path: directory.join(STORE_FILE),
        };
        let connection = store.connection()?;
        connection.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS writers (
              writer_id TEXT PRIMARY KEY,
              name TEXT NOT NULL DEFAULT '',
              last_applied INTEGER NOT NULL DEFAULT 0,
              highest_seen INTEGER NOT NULL DEFAULT 0,
              last_seen_at TEXT,
              retired INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS vaults (
              sync_id TEXT PRIMARY KEY,
              scope_id TEXT UNIQUE,
              name TEXT NOT NULL DEFAULT '',
              state TEXT NOT NULL,
              paused INTEGER NOT NULL DEFAULT 0,
              message TEXT
            );
            CREATE TABLE IF NOT EXISTS entries (
              key TEXT PRIMARY KEY,
              entry TEXT NOT NULL,
              observed TEXT,
              fp_size INTEGER,
              fp_mtime INTEGER,
              unconfirmed INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS conflicts (
              id TEXT PRIMARY KEY,
              kind TEXT NOT NULL,
              key TEXT NOT NULL,
              vault_sync_id TEXT,
              plugin_id TEXT,
              local_entry TEXT,
              remote_entry TEXT NOT NULL,
              remote_device TEXT NOT NULL,
              detected_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS conflicts_by_key ON conflicts(key);
            CREATE TABLE IF NOT EXISTS suggestions (
              plugin_id TEXT PRIMARY KEY,
              from_device TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS preferences (
              key TEXT PRIMARY KEY,
              value TEXT,
              updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS journal (
              id INTEGER PRIMARY KEY CHECK (id = 1),
              writer_id TEXT NOT NULL,
              generation INTEGER NOT NULL,
              counter INTEGER NOT NULL,
              started_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(store)
    }

    pub(crate) fn connection(&self) -> AppResult<Connection> {
        let connection = Connection::open(&self.path)?;
        connection.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA secure_delete = ON;")?;
        Ok(connection)
    }

    pub(crate) fn transaction<T>(
        &self,
        operation: impl FnOnce(&Transaction<'_>) -> AppResult<T>,
    ) -> AppResult<T> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Removes every configuration, index, and journal row (Disconnect).
    pub(crate) fn reset(&self) -> AppResult<()> {
        self.transaction(|transaction| {
            transaction.execute_batch(
                "DELETE FROM meta; DELETE FROM writers; DELETE FROM vaults; DELETE FROM entries;
                 DELETE FROM conflicts; DELETE FROM suggestions; DELETE FROM preferences;
                 DELETE FROM journal;",
            )?;
            Ok(())
        })
    }
}

pub(crate) fn get_meta<T: DeserializeOwned>(
    connection: &Connection,
    key: &str,
) -> AppResult<Option<T>> {
    connection
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| {
                AppError::InvalidData(format!("Damaged sync setting {key}: {error}"))
            })
        })
        .transpose()
}

pub(crate) fn set_meta<T: Serialize + ?Sized>(
    connection: &Connection,
    key: &str,
    value: &T,
) -> AppResult<()> {
    let value = serde_json::to_string(value).map_err(|error| {
        AppError::InvalidData(format!("Unable to save sync setting {key}: {error}"))
    })?;
    connection.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn to_json<T: Serialize>(value: &T) -> AppResult<String> {
    serde_json::to_string(value)
        .map_err(|error| AppError::InvalidData(format!("Unable to encode sync state: {error}")))
}

fn from_json<T: DeserializeOwned>(value: &str) -> AppResult<T> {
    serde_json::from_str(value)
        .map_err(|error| AppError::InvalidData(format!("Damaged sync index: {error}")))
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(crate) fn load_index(connection: &Connection) -> AppResult<BTreeMap<String, IndexRow>> {
    let mut statement = connection
        .prepare("SELECT key, entry, observed, fp_size, fp_mtime, unconfirmed FROM entries")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;
    let mut index = BTreeMap::new();
    for row in rows {
        let (key, entry, observed, size, mtime, unconfirmed) = row?;
        let fingerprint = match (size, mtime) {
            (Some(size), Some(mtime_ns)) => Some(Fingerprint {
                size: to_u64(size),
                mtime_ns,
            }),
            _ => None,
        };
        index.insert(
            key,
            IndexRow {
                entry: from_json(&entry)?,
                observed: observed.as_deref().map(from_json).transpose()?,
                fingerprint,
                unconfirmed: unconfirmed != 0,
            },
        );
    }
    Ok(index)
}

/// Writes only rows that differ between `before` and `after`.
pub(crate) fn write_index_changes(
    connection: &Connection,
    before: &BTreeMap<String, IndexRow>,
    after: &BTreeMap<String, IndexRow>,
) -> AppResult<usize> {
    let mut changed = 0;
    let mut delete = connection.prepare_cached("DELETE FROM entries WHERE key = ?1")?;
    for key in before.keys().filter(|key| !after.contains_key(*key)) {
        delete.execute(params![key])?;
        changed += 1;
    }
    let mut upsert = connection.prepare_cached(
        "INSERT INTO entries(key, entry, observed, fp_size, fp_mtime, unconfirmed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(key) DO UPDATE SET entry = excluded.entry, observed = excluded.observed,
           fp_size = excluded.fp_size, fp_mtime = excluded.fp_mtime,
           unconfirmed = excluded.unconfirmed",
    )?;
    for (key, row) in after {
        if before.get(key) == Some(row) {
            continue;
        }
        upsert.execute(params![
            key,
            to_json(&row.entry)?,
            row.observed.as_ref().map(to_json).transpose()?,
            row.fingerprint.map(|fingerprint| to_i64(fingerprint.size)),
            row.fingerprint.map(|fingerprint| fingerprint.mtime_ns),
            i64::from(row.unconfirmed)
        ])?;
        changed += 1;
    }
    Ok(changed)
}

pub(crate) fn delete_entries_with_prefix(connection: &Connection, prefix: &str) -> AppResult<()> {
    connection.execute(
        "DELETE FROM entries WHERE substr(key, 1, length(?1)) = ?1",
        params![prefix],
    )?;
    Ok(())
}

pub(crate) fn list_writers(connection: &Connection) -> AppResult<Vec<WriterRow>> {
    let mut statement = connection.prepare(
        "SELECT writer_id, name, last_applied, highest_seen, last_seen_at, retired
         FROM writers ORDER BY writer_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(WriterRow {
            writer_id: row.get(0)?,
            name: row.get(1)?,
            last_applied: to_u64(row.get(2)?),
            highest_seen: to_u64(row.get(3)?),
            last_seen_at: row.get(4)?,
            retired: row.get::<_, i64>(5)? != 0,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub(crate) fn upsert_writer(connection: &Connection, writer: &WriterRow) -> AppResult<()> {
    connection.execute(
        "INSERT INTO writers(writer_id, name, last_applied, highest_seen, last_seen_at, retired)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(writer_id) DO UPDATE SET name = excluded.name,
           last_applied = excluded.last_applied, highest_seen = excluded.highest_seen,
           last_seen_at = excluded.last_seen_at, retired = excluded.retired",
        params![
            writer.writer_id,
            writer.name,
            to_i64(writer.last_applied),
            to_i64(writer.highest_seen),
            writer.last_seen_at,
            i64::from(writer.retired)
        ],
    )?;
    Ok(())
}

pub(crate) fn list_vaults(connection: &Connection) -> AppResult<Vec<VaultRow>> {
    let mut statement = connection.prepare(
        "SELECT sync_id, scope_id, name, state, paused, message FROM vaults
         ORDER BY name COLLATE NOCASE, sync_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut vaults = Vec::new();
    for row in rows {
        let (sync_id, scope_id, name, state, paused, message) = row?;
        vaults.push(VaultRow {
            sync_id,
            scope_id,
            name,
            state: VaultState::parse(&state)?,
            paused: paused != 0,
            message,
        });
    }
    Ok(vaults)
}

pub(crate) fn upsert_vault(connection: &Connection, vault: &VaultRow) -> AppResult<()> {
    if let Some(scope_id) = &vault.scope_id {
        connection.execute(
            "UPDATE vaults SET scope_id = NULL WHERE scope_id = ?1 AND sync_id <> ?2",
            params![scope_id, vault.sync_id],
        )?;
    }
    connection.execute(
        "INSERT INTO vaults(sync_id, scope_id, name, state, paused, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(sync_id) DO UPDATE SET scope_id = excluded.scope_id, name = excluded.name,
           state = excluded.state, paused = excluded.paused, message = excluded.message",
        params![
            vault.sync_id,
            vault.scope_id,
            vault.name,
            vault.state.as_str(),
            i64::from(vault.paused),
            vault.message
        ],
    )?;
    Ok(())
}

pub(crate) fn list_conflicts(connection: &Connection) -> AppResult<Vec<ConflictRow>> {
    let mut statement = connection.prepare(
        "SELECT id, kind, key, vault_sync_id, plugin_id, local_entry, remote_entry,
                remote_device, detected_at
         FROM conflicts ORDER BY detected_at, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
        ))
    })?;
    let mut conflicts = Vec::new();
    for row in rows {
        let (id, kind, key, vault_sync_id, plugin_id, local, remote, remote_device, detected_at) =
            row?;
        conflicts.push(ConflictRow {
            id,
            kind: ConflictKind::parse(&kind)?,
            key,
            vault_sync_id,
            plugin_id,
            local: local.as_deref().map(from_json).transpose()?,
            remote: from_json(&remote)?,
            remote_device,
            detected_at,
        });
    }
    Ok(conflicts)
}

/// Replaces every pending conflict with `conflicts`, keeping the detection
/// time of conflicts that are still present.
pub(crate) fn replace_conflicts(
    connection: &Connection,
    conflicts: &[ConflictRow],
) -> AppResult<()> {
    let existing: BTreeMap<String, String> = list_conflicts(connection)?
        .into_iter()
        .map(|conflict| (conflict.id, conflict.detected_at))
        .collect();
    connection.execute("DELETE FROM conflicts", [])?;
    for conflict in conflicts {
        connection.execute(
            "INSERT OR REPLACE INTO conflicts(id, kind, key, vault_sync_id, plugin_id,
               local_entry, remote_entry, remote_device, detected_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                conflict.id,
                conflict.kind.as_str(),
                conflict.key,
                conflict.vault_sync_id,
                conflict.plugin_id,
                conflict.local.as_ref().map(to_json).transpose()?,
                to_json(&conflict.remote)?,
                conflict.remote_device,
                existing.get(&conflict.id).unwrap_or(&conflict.detected_at)
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn delete_conflicts_for_key(connection: &Connection, key: &str) -> AppResult<()> {
    connection.execute("DELETE FROM conflicts WHERE key = ?1", params![key])?;
    Ok(())
}

pub(crate) fn list_suggestions(connection: &Connection) -> AppResult<Vec<(String, String)>> {
    let mut statement =
        connection.prepare("SELECT plugin_id, from_device FROM suggestions ORDER BY plugin_id")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub(crate) fn set_suggestion(
    connection: &Connection,
    plugin_id: &str,
    from_device: Option<&str>,
) -> AppResult<()> {
    match from_device {
        Some(device) => connection.execute(
            "INSERT INTO suggestions(plugin_id, from_device) VALUES (?1, ?2)
             ON CONFLICT(plugin_id) DO UPDATE SET from_device = excluded.from_device",
            params![plugin_id, device],
        )?,
        None => connection.execute(
            "DELETE FROM suggestions WHERE plugin_id = ?1",
            params![plugin_id],
        )?,
    };
    Ok(())
}

/// Renderer-pushed portable preferences; `None` means "not set on this device".
pub(crate) fn list_preferences(
    connection: &Connection,
) -> AppResult<BTreeMap<String, (Option<String>, i64)>> {
    let mut statement = connection.prepare("SELECT key, value, updated_at FROM preferences")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
    })?;
    Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
}

/// Records a preference value; returns whether it changed.
pub(crate) fn set_preference(
    connection: &Connection,
    key: &str,
    value: Option<&str>,
    updated_at: i64,
) -> AppResult<bool> {
    let current: Option<Option<String>> = connection
        .query_row(
            "SELECT value FROM preferences WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    if current.as_ref().map(|value| value.as_deref()) == Some(value)
        || (current.is_none() && value.is_none())
    {
        return Ok(false);
    }
    connection.execute(
        "INSERT INTO preferences(key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, updated_at],
    )?;
    Ok(true)
}

pub(crate) fn get_journal(connection: &Connection) -> AppResult<Option<Journal>> {
    Ok(connection
        .query_row(
            "SELECT writer_id, generation, counter, started_at FROM journal WHERE id = 1",
            [],
            |row| {
                Ok(Journal {
                    writer_id: row.get(0)?,
                    generation: to_u64(row.get(1)?),
                    counter: to_u64(row.get(2)?),
                    started_at: row.get(3)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn set_journal(connection: &Connection, journal: &Journal) -> AppResult<()> {
    connection.execute(
        "INSERT INTO journal(id, writer_id, generation, counter, started_at)
         VALUES (1, ?1, ?2, ?3, ?4)
         ON CONFLICT(id) DO UPDATE SET writer_id = excluded.writer_id,
           generation = excluded.generation, counter = excluded.counter,
           started_at = excluded.started_at",
        params![
            journal.writer_id,
            to_i64(journal.generation),
            to_i64(journal.counter),
            journal.started_at
        ],
    )?;
    Ok(())
}

pub(crate) fn clear_journal(connection: &Connection) -> AppResult<()> {
    connection.execute("DELETE FROM journal WHERE id = 1", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::EntryValue;

    fn row(counter: u64) -> IndexRow {
        let vv = BTreeMap::from([("a".repeat(32), counter)]);
        IndexRow {
            entry: Entry {
                value: EntryValue::Tombstone,
                modified_at: 1,
                origin: "a".repeat(32),
                vv: vv.clone(),
            },
            observed: counter.is_multiple_of(2).then_some(Observed {
                value: EntryValue::Tombstone,
                vv,
            }),
            fingerprint: Some(Fingerprint {
                size: 3,
                mtime_ns: 9,
            }),
            unconfirmed: counter == 3,
        }
    }

    #[test]
    fn index_identity_and_journal_commit_together_or_not_at_all() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let before = BTreeMap::new();
        let after = BTreeMap::from([("a".to_string(), row(1)), ("b".to_string(), row(2))]);
        let identity = Identity {
            writer_id: "a".repeat(32),
            counter: 2,
            generation: 1,
        };
        store
            .transaction(|transaction| {
                write_index_changes(transaction, &before, &after)?;
                set_meta(transaction, META_IDENTITY, &identity)?;
                clear_journal(transaction)
            })
            .unwrap();
        let connection = store.connection().unwrap();
        assert_eq!(load_index(&connection).unwrap(), after);
        assert_eq!(
            get_meta::<Identity>(&connection, META_IDENTITY).unwrap(),
            Some(identity.clone())
        );

        let next = BTreeMap::from([("b".to_string(), row(3))]);
        let failed: AppResult<()> = store.transaction(|transaction| {
            write_index_changes(transaction, &after, &next)?;
            set_meta(
                transaction,
                META_IDENTITY,
                &Identity {
                    counter: 9,
                    ..identity.clone()
                },
            )?;
            Err(AppError::State("synthetic failure".into()))
        });
        assert!(failed.is_err());
        assert_eq!(
            load_index(&connection).unwrap(),
            after,
            "a failed cycle commits nothing"
        );
        assert_eq!(
            get_meta::<Identity>(&connection, META_IDENTITY)
                .unwrap()
                .unwrap()
                .counter,
            2
        );
        store
            .transaction(|transaction| write_index_changes(transaction, &after, &next).map(|_| ()))
            .unwrap();
        assert_eq!(load_index(&connection).unwrap(), next);

        let journal = Journal {
            writer_id: "a".repeat(32),
            generation: 4,
            counter: 7,
            started_at: "2026-01-01T00:00:00+00:00".into(),
        };
        set_journal(&connection, &journal).unwrap();
        assert_eq!(get_journal(&connection).unwrap(), Some(journal));
        delete_entries_with_prefix(&connection, "b").unwrap();
        assert!(load_index(&connection).unwrap().is_empty());
        store.reset().unwrap();
        assert_eq!(get_journal(&connection).unwrap(), None);
    }

    #[test]
    fn preferences_record_only_real_changes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let connection = store.connection().unwrap();
        assert!(!set_preference(&connection, "denote-theme", None, 1).unwrap());
        assert!(set_preference(&connection, "denote-theme", Some("dark"), 1).unwrap());
        assert!(!set_preference(&connection, "denote-theme", Some("dark"), 2).unwrap());
        assert!(set_preference(&connection, "denote-theme", None, 3).unwrap());
        assert_eq!(
            list_preferences(&connection).unwrap()["denote-theme"],
            (None, 3)
        );
    }

    #[test]
    fn conflicts_keep_their_detection_time_while_pending() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let connection = store.connection().unwrap();
        let conflict = ConflictRow {
            id: "c1".into(),
            kind: ConflictKind::PluginState,
            key: "plugin/denote.sample/state/count".into(),
            vault_sync_id: None,
            plugin_id: Some("denote.sample".into()),
            local: None,
            remote: row(1).entry,
            remote_device: "Desk".into(),
            detected_at: "first".into(),
        };
        replace_conflicts(&connection, std::slice::from_ref(&conflict)).unwrap();
        replace_conflicts(
            &connection,
            &[ConflictRow {
                detected_at: "second".into(),
                ..conflict.clone()
            }],
        )
        .unwrap();
        assert_eq!(list_conflicts(&connection).unwrap()[0].detected_at, "first");
        replace_conflicts(&connection, &[]).unwrap();
        assert!(list_conflicts(&connection).unwrap().is_empty());

        let vault = VaultRow {
            sync_id: "0".repeat(32),
            scope_id: Some("scope".into()),
            name: "Notes".into(),
            state: VaultState::Synced,
            paused: false,
            message: None,
        };
        upsert_vault(&connection, &vault).unwrap();
        upsert_vault(
            &connection,
            &VaultRow {
                sync_id: "1".repeat(32),
                ..vault.clone()
            },
        )
        .unwrap();
        let vaults = list_vaults(&connection).unwrap();
        assert_eq!(vaults.len(), 2);
        assert_eq!(
            vaults.iter().filter(|row| row.scope_id.is_some()).count(),
            1,
            "a local vault maps to one sync ID"
        );
    }
}
