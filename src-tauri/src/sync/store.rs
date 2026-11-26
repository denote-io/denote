//! Device-local sync state in `sync/sync.sqlite3`. See `docs/sync.md` ("Synchronization").

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};

use super::model::Entry;
use crate::error::{AppError, AppResult};

pub(crate) const STORE_FILE: &str = "sync.sqlite3";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub size: u64,
    pub mtime_ns: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IndexRow {
    pub entry: Entry,
    /// Disk state the entry was scanned or applied from (vault files only).
    pub fingerprint: Option<Fingerprint>,
    pub unconfirmed: bool,
    /// Merged but not yet applied locally (for example, plugin not installed).
    pub pending_apply: bool,
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
            other => return Err(AppError::InvalidData(format!("Unknown sync vault state {other}"))),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VaultRow {
    pub sync_id: String,
    pub scope_id: Option<String>,
    pub path: Option<String>,
    pub name: String,
    pub state: VaultState,
    pub paused: bool,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConflictRow {
    pub id: String,
    pub kind: String,
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
        let store = Self { path: directory.join(STORE_FILE) };
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
              path TEXT,
              name TEXT NOT NULL DEFAULT '',
              state TEXT NOT NULL,
              paused INTEGER NOT NULL DEFAULT 0,
              message TEXT
            );
            CREATE TABLE IF NOT EXISTS entries (
              key TEXT PRIMARY KEY,
              entry TEXT NOT NULL,
              fp_size INTEGER,
              fp_mtime INTEGER,
              unconfirmed INTEGER NOT NULL DEFAULT 0,
              pending_apply INTEGER NOT NULL DEFAULT 0
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
            CREATE TABLE IF NOT EXISTS suggestions (
              plugin_id TEXT PRIMARY KEY,
              from_device TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS preferences (
              key TEXT PRIMARY KEY,
              value TEXT,
              updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS remote_objects (id TEXT PRIMARY KEY);
            CREATE TABLE IF NOT EXISTS journal (
              id INTEGER PRIMARY KEY CHECK (id = 1),
              phase TEXT NOT NULL,
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

    pub(crate) fn transaction<T>(&self, operation: impl FnOnce(&Transaction<'_>) -> AppResult<T>) -> AppResult<T> {
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
                 DELETE FROM remote_objects; DELETE FROM journal;",
            )?;
            Ok(())
        })
    }
}

pub(crate) fn get_meta<T: DeserializeOwned>(connection: &Connection, key: &str) -> AppResult<Option<T>> {
    connection
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get::<_, String>(0))
        .optional()?
        .map(|value| {
            serde_json::from_str(&value)
                .map_err(|error| AppError::InvalidData(format!("Damaged sync setting {key}: {error}")))
        })
        .transpose()
}

pub(crate) fn set_meta<T: Serialize + ?Sized>(connection: &Connection, key: &str, value: &T) -> AppResult<()> {
    let value = serde_json::to_string(value)
        .map_err(|error| AppError::InvalidData(format!("Unable to save sync setting {key}: {error}")))?;
    connection.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub(crate) fn delete_meta(connection: &Connection, key: &str) -> AppResult<()> {
    connection.execute("DELETE FROM meta WHERE key = ?1", params![key])?;
    Ok(())
}

fn to_json<T: Serialize>(value: &T) -> AppResult<String> {
    serde_json::to_string(value).map_err(|error| AppError::InvalidData(format!("Unable to encode sync state: {error}")))
}

fn from_json<T: DeserializeOwned>(value: &str) -> AppResult<T> {
    serde_json::from_str(value).map_err(|error| AppError::InvalidData(format!("Damaged sync index: {error}")))
}

pub(crate) fn load_index(connection: &Connection) -> AppResult<BTreeMap<String, IndexRow>> {
    let mut statement =
        connection.prepare("SELECT key, entry, fp_size, fp_mtime, unconfirmed, pending_apply FROM entries")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;
    let mut index = BTreeMap::new();
    for row in rows {
        let (key, entry, size, mtime, unconfirmed, pending) = row?;
        let fingerprint = match (size, mtime) {
            (Some(size), Some(mtime_ns)) => Some(Fingerprint { size: size.max(0) as u64, mtime_ns }),
            _ => None,
        };
        index.insert(
            key,
            IndexRow { entry: from_json(&entry)?, fingerprint, unconfirmed: unconfirmed != 0, pending_apply: pending != 0 },
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
    for key in before.keys().filter(|key| !after.contains_key(*key)) {
        connection.execute("DELETE FROM entries WHERE key = ?1", params![key])?;
        changed += 1;
    }
    for (key, row) in after {
        if before.get(key) == Some(row) {
            continue;
        }
        let size = row.fingerprint.map(|fingerprint| i64::try_from(fingerprint.size).unwrap_or(i64::MAX));
        connection.execute(
            "INSERT INTO entries(key, entry, fp_size, fp_mtime, unconfirmed, pending_apply)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(key) DO UPDATE SET entry = excluded.entry, fp_size = excluded.fp_size,
               fp_mtime = excluded.fp_mtime, unconfirmed = excluded.unconfirmed,
               pending_apply = excluded.pending_apply",
            params![
                key,
                to_json(&row.entry)?,
                size,
                row.fingerprint.map(|fingerprint| fingerprint.mtime_ns),
                i64::from(row.unconfirmed),
                i64::from(row.pending_apply)
            ],
        )?;
        changed += 1;
    }
    Ok(changed)
}

pub(crate) fn mark_all_unconfirmed(connection: &Connection) -> AppResult<()> {
    connection.execute("UPDATE entries SET unconfirmed = 1", [])?;
    Ok(())
}

pub(crate) fn list_writers(connection: &Connection) -> AppResult<Vec<WriterRow>> {
    let mut statement = connection.prepare(
        "SELECT writer_id, name, last_applied, highest_seen, last_seen_at, retired FROM writers ORDER BY writer_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(WriterRow {
            writer_id: row.get(0)?,
            name: row.get(1)?,
            last_applied: row.get::<_, i64>(2)?.max(0) as u64,
            highest_seen: row.get::<_, i64>(3)?.max(0) as u64,
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
         ON CONFLICT(writer_id) DO UPDATE SET name = excluded.name, last_applied = excluded.last_applied,
           highest_seen = excluded.highest_seen, last_seen_at = excluded.last_seen_at, retired = excluded.retired",
        params![
            writer.writer_id,
            writer.name,
            i64::try_from(writer.last_applied).unwrap_or(i64::MAX),
            i64::try_from(writer.highest_seen).unwrap_or(i64::MAX),
            writer.last_seen_at,
            i64::from(writer.retired)
        ],
    )?;
    Ok(())
}

pub(crate) fn list_vaults(connection: &Connection) -> AppResult<Vec<VaultRow>> {
    let mut statement =
        connection.prepare("SELECT sync_id, scope_id, path, name, state, paused, message FROM vaults ORDER BY name, sync_id")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, Option<String>>(6)?,
        ))
    })?;
    let mut vaults = Vec::new();
    for row in rows {
        let (sync_id, scope_id, path, name, state, paused, message) = row?;
        vaults.push(VaultRow { sync_id, scope_id, path, name, state: VaultState::parse(&state)?, paused: paused != 0, message });
    }
    Ok(vaults)
}

pub(crate) fn upsert_vault(connection: &Connection, vault: &VaultRow) -> AppResult<()> {
    connection.execute(
        "INSERT INTO vaults(sync_id, scope_id, path, name, state, paused, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(sync_id) DO UPDATE SET scope_id = excluded.scope_id, path = excluded.path,
           name = excluded.name, state = excluded.state, paused = excluded.paused, message = excluded.message",
        params![
            vault.sync_id,
            vault.scope_id,
            vault.path,
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
        "SELECT id, kind, key, vault_sync_id, plugin_id, local_entry, remote_entry, remote_device, detected_at
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
        let (id, kind, key, vault_sync_id, plugin_id, local, remote, remote_device, detected_at) = row?;
        conflicts.push(ConflictRow {
            id,
            kind,
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

pub(crate) fn insert_conflict(connection: &Connection, conflict: &ConflictRow) -> AppResult<()> {
    connection.execute(
        "INSERT OR IGNORE INTO conflicts(id, kind, key, vault_sync_id, plugin_id, local_entry, remote_entry, remote_device, detected_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            conflict.id,
            conflict.kind,
            conflict.key,
            conflict.vault_sync_id,
            conflict.plugin_id,
            conflict.local.as_ref().map(to_json).transpose()?,
            to_json(&conflict.remote)?,
            conflict.remote_device,
            conflict.detected_at
        ],
    )?;
    Ok(())
}

pub(crate) fn delete_conflicts_for_key(connection: &Connection, key: &str) -> AppResult<()> {
    connection.execute("DELETE FROM conflicts WHERE key = ?1", params![key])?;
    Ok(())
}

pub(crate) fn list_suggestions(connection: &Connection) -> AppResult<Vec<(String, String)>> {
    let mut statement = connection.prepare("SELECT plugin_id, from_device FROM suggestions ORDER BY plugin_id")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub(crate) fn set_suggestion(connection: &Connection, plugin_id: &str, from_device: Option<&str>) -> AppResult<()> {
    match from_device {
        Some(device) => connection.execute(
            "INSERT INTO suggestions(plugin_id, from_device) VALUES (?1, ?2)
             ON CONFLICT(plugin_id) DO UPDATE SET from_device = excluded.from_device",
            params![plugin_id, device],
        )?,
        None => connection.execute("DELETE FROM suggestions WHERE plugin_id = ?1", params![plugin_id])?,
    };
    Ok(())
}

/// Renderer-pushed portable preferences; `None` records a removal.
pub(crate) fn list_preferences(connection: &Connection) -> AppResult<BTreeMap<String, (Option<String>, i64)>> {
    let mut statement = connection.prepare("SELECT key, value, updated_at FROM preferences")?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?))))?;
    Ok(rows.collect::<Result<BTreeMap<_, _>, _>>()?)
}

pub(crate) fn set_preference(connection: &Connection, key: &str, value: Option<&str>, updated_at: i64) -> AppResult<bool> {
    let current: Option<Option<String>> = connection
        .query_row("SELECT value FROM preferences WHERE key = ?1", params![key], |row| row.get(0))
        .optional()?;
    if current.as_ref().map(|value| value.as_deref()) == Some(value) {
        return Ok(false);
    }
    connection.execute(
        "INSERT INTO preferences(key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, updated_at],
    )?;
    Ok(true)
}

pub(crate) fn has_remote_object(connection: &Connection, id: &str) -> AppResult<bool> {
    Ok(connection
        .query_row("SELECT 1 FROM remote_objects WHERE id = ?1", params![id], |_| Ok(()))
        .optional()?
        .is_some())
}

pub(crate) fn add_remote_object(connection: &Connection, id: &str) -> AppResult<()> {
    connection.execute("INSERT OR IGNORE INTO remote_objects(id) VALUES (?1)", params![id])?;
    Ok(())
}

pub(crate) fn begin_journal(connection: &Connection, phase: &str, started_at: &str) -> AppResult<Option<String>> {
    let previous: Option<String> = connection
        .query_row("SELECT phase FROM journal WHERE id = 1", [], |row| row.get(0))
        .optional()?;
    connection.execute(
        "INSERT INTO journal(id, phase, started_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET phase = excluded.phase, started_at = excluded.started_at",
        params![phase, started_at],
    )?;
    Ok(previous)
}

pub(crate) fn set_journal_phase(connection: &Connection, phase: &str) -> AppResult<()> {
    connection.execute("UPDATE journal SET phase = ?1 WHERE id = 1", params![phase])?;
    Ok(())
}

pub(crate) fn finish_journal(connection: &Connection) -> AppResult<()> {
    connection.execute("DELETE FROM journal WHERE id = 1", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::EntryValue;

    fn row(counter: u64) -> IndexRow {
        IndexRow {
            entry: Entry {
                value: EntryValue::Tombstone,
                modified_at: 1,
                origin: "a".repeat(32),
                vv: BTreeMap::from([("a".repeat(32), counter)]),
            },
            fingerprint: Some(Fingerprint { size: 3, mtime_ns: 9 }),
            unconfirmed: false,
            pending_apply: counter % 2 == 0,
        }
    }

    #[test]
    fn index_changes_round_trip_in_one_transaction() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let before = BTreeMap::new();
        let after = BTreeMap::from([("a".to_string(), row(1)), ("b".to_string(), row(2))]);
        store
            .transaction(|transaction| {
                write_index_changes(transaction, &before, &after)?;
                set_meta(transaction, "counter", &2u64)
            })
            .unwrap();
        let connection = store.connection().unwrap();
        assert_eq!(load_index(&connection).unwrap(), after);
        assert_eq!(get_meta::<u64>(&connection, "counter").unwrap(), Some(2));

        let next = BTreeMap::from([("b".to_string(), row(3))]);
        let failed: AppResult<()> = store.transaction(|transaction| {
            write_index_changes(transaction, &after, &next)?;
            Err(AppError::State("synthetic failure".into()))
        });
        assert!(failed.is_err());
        assert_eq!(load_index(&connection).unwrap(), after, "a failed cycle commits nothing");
        store.transaction(|transaction| write_index_changes(transaction, &after, &next).map(|_| ())).unwrap();
        assert_eq!(load_index(&connection).unwrap(), next);
        store.reset().unwrap();
        assert!(load_index(&connection).unwrap().is_empty());
    }

    #[test]
    fn preferences_record_only_real_changes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path()).unwrap();
        let connection = store.connection().unwrap();
        assert!(set_preference(&connection, "denote-theme", Some("dark"), 1).unwrap());
        assert!(!set_preference(&connection, "denote-theme", Some("dark"), 2).unwrap());
        assert!(set_preference(&connection, "denote-theme", None, 3).unwrap());
        assert_eq!(list_preferences(&connection).unwrap()["denote-theme"], (None, 3));
    }
}
