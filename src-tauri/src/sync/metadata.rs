//! Vault preferences, note placement, tag colors, project roots, and revision
//! history in the application database. See `docs/sync.md` ("Scope matrix").

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    model::{EntryValue, decode_segment, encode_segment, valid_relative_path},
    scan::nfc,
};
use crate::{
    db,
    error::{AppError, AppResult},
    models::MarkdownViewMode,
};

/// History rows read per cycle and vault; the remainder follows next cycle.
pub(crate) const HISTORY_BATCH: i64 = 2_000;

/// A local vault resolved from its plugin storage scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DbVault {
    pub id: i64,
    pub path: String,
    pub name: String,
    pub scope_id: String,
}

pub(crate) fn vault_by_scope(
    connection: &Connection,
    scope_id: &str,
) -> AppResult<Option<DbVault>> {
    Ok(connection
        .query_row(
            "SELECT id, path, name, plugin_scope_id FROM vaults WHERE plugin_scope_id = ?1",
            params![scope_id],
            |row| {
                Ok(DbVault {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    name: row.get(2)?,
                    scope_id: row.get(3)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn all_vaults(connection: &Connection) -> AppResult<Vec<DbVault>> {
    let mut statement = connection.prepare(
        "SELECT id, path, name, plugin_scope_id FROM vaults ORDER BY name COLLATE NOCASE, path",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(DbVault {
            id: row.get(0)?,
            path: row.get(1)?,
            name: row.get(2)?,
            scope_id: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VaultPreferences {
    markdown_view_mode: Option<String>,
    restore_tabs: bool,
    welcome_page_path: Option<String>,
}

impl Default for VaultPreferences {
    fn default() -> Self {
        Self {
            markdown_view_mode: None,
            restore_tabs: true,
            welcome_page_path: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NotePlacement {
    bookmarked: bool,
    position: Option<i64>,
    pinned: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TagColor {
    color: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectRoot {
    explicit: bool,
}

/// One revision row; its keyed object ID is its history entry ID.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HistoryRecord {
    pub path: String,
    pub content: String,
    pub encoding: String,
    pub line_ending: String,
    pub is_encrypted: bool,
    pub content_hash: String,
    pub reason: String,
    pub created_at: String,
}

fn inline<T: Serialize>(value: &T) -> AppResult<EntryValue> {
    Ok(EntryValue::Inline {
        json: serde_json::to_value(value).map_err(|error| {
            AppError::InvalidData(format!("Unable to encode synced metadata: {error}"))
        })?,
    })
}

fn parse<T: for<'de> Deserialize<'de>>(value: &EntryValue) -> AppResult<T> {
    let EntryValue::Inline { json } = value else {
        return Err(AppError::InvalidData(
            "Synced metadata has an unexpected type".into(),
        ));
    };
    serde_json::from_value(json.clone())
        .map_err(|error| AppError::InvalidData(format!("Synced metadata is invalid: {error}")))
}

pub(crate) fn meta_key(vault: &str) -> String {
    format!("v/{vault}/meta")
}

pub(crate) fn note_key(vault: &str, path: &str) -> String {
    format!("v/{vault}/note/{path}")
}

pub(crate) fn tag_key(vault: &str, tag: &str) -> String {
    format!("v/{vault}/tag/{}", encode_segment(tag))
}

pub(crate) fn project_key(vault: &str, root: &str) -> String {
    format!("v/{vault}/project/{root}")
}

pub(crate) fn history_key(vault: &str, id: &str) -> String {
    format!("v/{vault}/history/{id}")
}

/// Vault preferences, note placement, tag colors, and explicit project roots.
/// Default values are absent, so a fresh device never outvotes a configured one.
pub(crate) fn observe(
    connection: &Connection,
    vault: &DbVault,
    sync_id: &str,
) -> AppResult<BTreeMap<String, EntryValue>> {
    let mut observed = BTreeMap::new();
    let preferences = VaultPreferences {
        markdown_view_mode: db::get_vault_markdown_view_mode(connection, vault.id)?
            .map(|mode| mode.as_str().to_string()),
        restore_tabs: db::get_restore_tabs(connection, vault.id)?,
        welcome_page_path: db::get_welcome_page_path(connection, vault.id)?,
    };
    if preferences != VaultPreferences::default() {
        observed.insert(meta_key(sync_id), inline(&preferences)?);
    }

    let mut placements: BTreeMap<String, NotePlacement> = BTreeMap::new();
    let mut statement = connection
        .prepare("SELECT path FROM note_stats WHERE vault_id = ?1 AND is_bookmarked = 1")?;
    for path in statement.query_map(params![vault.id], |row| row.get::<_, String>(0))? {
        placements.insert(
            nfc(&path?),
            NotePlacement {
                bookmarked: true,
                position: None,
                pinned: false,
            },
        );
    }
    for (path, placement) in db::entry_placement_map(connection, vault.id)? {
        let entry = placements.entry(nfc(&path)).or_insert(NotePlacement {
            bookmarked: false,
            position: None,
            pinned: false,
        });
        entry.position = Some(placement.position);
        entry.pinned = placement.pinned;
    }
    for (path, placement) in placements {
        if valid_relative_path(&path) {
            observed.insert(note_key(sync_id, &path), inline(&placement)?);
        }
    }

    for tag in db::list_tag_colors(connection, vault.id)? {
        if !tag.tag.is_empty() && tag.tag.len() <= 512 {
            observed.insert(
                tag_key(sync_id, &tag.tag),
                inline(&TagColor { color: tag.color })?,
            );
        }
    }

    for root in db::list_project_roots(connection, vault.id)? {
        if root.is_explicit && (root.root_path.is_empty() || valid_relative_path(&root.root_path)) {
            observed.insert(
                project_key(sync_id, &root.root_path),
                inline(&ProjectRoot { explicit: true })?,
            );
        }
    }
    Ok(observed)
}

/// Revision rows added since `watermark`, oldest first, with their row IDs.
pub(crate) fn history_since(
    connection: &Connection,
    vault_id: i64,
    watermark: i64,
) -> AppResult<Vec<(i64, HistoryRecord)>> {
    let mut statement = connection.prepare(
        "SELECT id, path, content, encoding, line_ending, is_encrypted, content_hash, reason,
                created_at
         FROM history WHERE vault_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3",
    )?;
    let rows = statement.query_map(params![vault_id, watermark, HISTORY_BATCH], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            HistoryRecord {
                path: row.get(1)?,
                content: row.get(2)?,
                encoding: row.get(3)?,
                line_ending: row.get(4)?,
                is_encrypted: row.get::<_, i64>(5)? != 0,
                content_hash: row.get(6)?,
                reason: row.get(7)?,
                created_at: row.get(8)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub(crate) fn encode_history(record: &HistoryRecord) -> AppResult<Vec<u8>> {
    serde_json::to_vec(record)
        .map_err(|error| AppError::InvalidData(format!("Unable to encode a revision: {error}")))
}

pub(crate) fn decode_history(bytes: &[u8]) -> AppResult<HistoryRecord> {
    let record: HistoryRecord = serde_json::from_slice(bytes)
        .map_err(|error| AppError::InvalidData(format!("A synced revision is invalid: {error}")))?;
    let valid = valid_relative_path(&record.path)
        && matches!(record.encoding.as_str(), "utf8" | "base64")
        && matches!(record.line_ending.as_str(), "lf" | "crlf" | "cr")
        && record.content_hash.len() <= 128
        && record.reason.len() <= 256
        && record.created_at.len() <= 64;
    if !valid {
        return Err(AppError::InvalidData("A synced revision is invalid".into()));
    }
    Ok(record)
}

/// Inserts a revision unless it is already present or older than every
/// revision the per-file limit keeps.
pub(crate) fn apply_history(
    connection: &mut Connection,
    vault_id: i64,
    record: &HistoryRecord,
) -> AppResult<()> {
    let transaction = connection.transaction()?;
    let exists = transaction
        .query_row(
            "SELECT 1 FROM history WHERE vault_id = ?1 AND path = ?2 AND content_hash = ?3
               AND created_at = ?4 AND reason = ?5",
            params![
                vault_id,
                record.path,
                record.content_hash,
                record.created_at,
                record.reason
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let newer: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM history WHERE vault_id = ?1 AND path = ?2 AND created_at > ?3",
        params![vault_id, record.path, record.created_at],
        |row| row.get(0),
    )?;
    if exists || newer >= db::HISTORY_LIMIT {
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO history(vault_id, path, content, encoding, line_ending, is_encrypted,
           content_hash, reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            vault_id,
            record.path,
            record.content,
            record.encoding,
            record.line_ending,
            i64::from(record.is_encrypted),
            record.content_hash,
            record.reason,
            record.created_at
        ],
    )?;
    transaction.execute(
        "DELETE FROM history WHERE vault_id = ?1 AND path = ?2 AND id NOT IN (
           SELECT id FROM history WHERE vault_id = ?1 AND path = ?2
           ORDER BY created_at DESC, id DESC LIMIT ?3)",
        params![vault_id, record.path, db::HISTORY_LIMIT],
    )?;
    transaction.commit()?;
    Ok(())
}

fn valid_color(color: &str) -> bool {
    color.len() == 7
        && color.starts_with('#')
        && color[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Applies one vault metadata entry; `None` restores the default.
pub(crate) fn apply(
    connection: &mut Connection,
    vault_id: i64,
    kind: &super::model::EntryKind<'_>,
    value: Option<&EntryValue>,
) -> AppResult<()> {
    use super::model::EntryKind;
    match kind {
        EntryKind::VaultMeta { .. } => {
            let preferences: VaultPreferences = value.map(parse).transpose()?.unwrap_or_default();
            if preferences
                .welcome_page_path
                .as_deref()
                .is_some_and(|path| !valid_relative_path(path))
            {
                return Err(AppError::InvalidData(
                    "A synced welcome page is invalid".into(),
                ));
            }
            match preferences
                .markdown_view_mode
                .as_deref()
                .map(|mode| {
                    MarkdownViewMode::from_str(mode).ok_or_else(|| {
                        AppError::InvalidData("A synced Markdown view mode is invalid".into())
                    })
                })
                .transpose()?
            {
                Some(mode) => db::set_vault_markdown_view_mode(connection, vault_id, mode)?,
                None => {
                    connection.execute(
                        "UPDATE vaults SET markdown_view_mode = NULL, updated_at = ?1 WHERE id = ?2",
                        params![db::now(), vault_id],
                    )?;
                }
            }
            db::set_restore_tabs(connection, vault_id, preferences.restore_tabs)?;
            db::set_welcome_page_path(
                connection,
                vault_id,
                preferences.welcome_page_path.as_deref(),
            )?;
        }
        EntryKind::Note { path, .. } => {
            let placement: Option<NotePlacement> = value.map(parse).transpose()?;
            let bookmarked = placement
                .as_ref()
                .is_some_and(|placement| placement.bookmarked);
            if bookmarked {
                db::set_bookmark(connection, vault_id, path, true)?;
            } else {
                connection.execute(
                    "UPDATE note_stats SET is_bookmarked = 0, updated_at = ?1
                     WHERE vault_id = ?2 AND path = ?3 AND is_bookmarked = 1",
                    params![db::now(), vault_id, path],
                )?;
            }
            match placement.filter(|placement| placement.position.is_some() || placement.pinned) {
                Some(placement) => {
                    connection.execute(
                        "INSERT INTO entry_order(vault_id, path, position, is_pinned, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(vault_id, path) DO UPDATE SET position = excluded.position,
                           is_pinned = excluded.is_pinned, updated_at = excluded.updated_at",
                        params![
                            vault_id,
                            path,
                            placement.position.unwrap_or(i64::MAX),
                            i64::from(placement.pinned),
                            db::now()
                        ],
                    )?;
                }
                None => {
                    connection.execute(
                        "DELETE FROM entry_order WHERE vault_id = ?1 AND path = ?2",
                        params![vault_id, path],
                    )?;
                }
            }
        }
        EntryKind::Tag { tag, .. } => {
            let tag = decode_segment(tag)
                .filter(|tag| !tag.is_empty())
                .ok_or_else(|| AppError::InvalidData("A synced tag is invalid".into()))?;
            match value.map(parse::<TagColor>).transpose()? {
                Some(TagColor { color }) if valid_color(&color) => {
                    db::set_tag_color(connection, vault_id, &tag, &color.to_ascii_lowercase())?;
                }
                Some(_) => {
                    return Err(AppError::InvalidData(
                        "A synced tag color is invalid".into(),
                    ));
                }
                None => {
                    connection.execute(
                        "DELETE FROM tag_colors WHERE vault_id = ?1 AND tag = ?2",
                        params![vault_id, tag],
                    )?;
                }
            }
        }
        EntryKind::Project { root, .. } => match value.map(parse::<ProjectRoot>).transpose()? {
            Some(ProjectRoot { explicit: true }) => {
                db::ensure_project_root(connection, vault_id, root, true)?;
            }
            _ => {
                let id: Option<String> = connection
                    .query_row(
                        "SELECT id FROM project_roots WHERE vault_id = ?1 AND root_path = ?2",
                        params![vault_id, root],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(id) = id {
                    db::clear_explicit_project_root(connection, vault_id, &id)?;
                }
            }
        },
        _ => {
            return Err(AppError::State(
                "Only vault metadata entries are applied to the database".into(),
            ));
        }
    }
    Ok(())
}

/// Convenience for tests and diagnostics: the JSON of an inline value.
#[cfg(test)]
pub(crate) fn inline_json(value: &EntryValue) -> Option<&Value> {
    match value {
        EntryValue::Inline { json } => Some(json),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::model::EntryKind;

    const SYNC: &str = "0123456789abcdef0123456789abcdef";

    fn database() -> (tempfile::TempDir, Connection, DbVault) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("denote.sqlite3");
        db::initialize(&path).unwrap();
        let connection = db::open(&path).unwrap();
        let id = db::ensure_vault(&connection, "/synthetic/vault", "vault").unwrap();
        let scope = db::plugin_scope_id(&connection, id).unwrap();
        let vault = vault_by_scope(&connection, &scope).unwrap().unwrap();
        (directory, connection, vault)
    }

    #[test]
    fn metadata_round_trips_between_databases() {
        let (_source_dir, mut source, vault) = database();
        assert!(
            observe(&source, &vault, SYNC).unwrap().is_empty(),
            "defaults are absent"
        );
        db::set_vault_markdown_view_mode(&mut source, vault.id, MarkdownViewMode::Source).unwrap();
        db::set_bookmark(&source, vault.id, "notes/a.md", true).unwrap();
        db::set_entry_pinned(&source, vault.id, "notes/b.md", true).unwrap();
        db::set_tag_color(&source, vault.id, "area/sub", "#aabbcc").unwrap();
        db::ensure_project_root(&source, vault.id, "code", true).unwrap();
        db::ensure_project_root(&source, vault.id, "implicit", false).unwrap();
        let observed = observe(&source, &vault, SYNC).unwrap();
        assert_eq!(
            observed.keys().cloned().collect::<Vec<_>>(),
            vec![
                format!("v/{SYNC}/meta"),
                format!("v/{SYNC}/note/notes/a.md"),
                format!("v/{SYNC}/note/notes/b.md"),
                format!("v/{SYNC}/project/code"),
                format!("v/{SYNC}/tag/area%2Fsub"),
            ]
        );

        let (_target_dir, mut target, target_vault) = database();
        for (key, value) in &observed {
            apply(
                &mut target,
                target_vault.id,
                &EntryKind::parse(key),
                Some(value),
            )
            .unwrap();
        }
        assert_eq!(observe(&target, &target_vault, SYNC).unwrap(), observed);

        for key in observed.keys() {
            apply(&mut target, target_vault.id, &EntryKind::parse(key), None).unwrap();
        }
        assert!(observe(&target, &target_vault, SYNC).unwrap().is_empty());

        let bad = EntryValue::Inline {
            json: serde_json::json!({"color": "red"}),
        };
        assert!(
            apply(
                &mut target,
                target_vault.id,
                &EntryKind::parse(&tag_key(SYNC, "x")),
                Some(&bad)
            )
            .is_err()
        );
        assert!(inline_json(&bad).is_some());
    }

    #[test]
    fn history_is_appended_without_duplicates_and_respects_the_limit() {
        let (_directory, mut connection, vault) = database();
        let record = |index: usize| HistoryRecord {
            path: "notes/a.md".into(),
            content: format!("revision {index}"),
            encoding: "utf8".into(),
            line_ending: "lf".into(),
            is_encrypted: false,
            content_hash: format!("hash-{index}"),
            reason: "save".into(),
            created_at: format!("2026-01-01T00:00:{index:02}+00:00"),
        };
        for index in 10..22 {
            apply_history(&mut connection, vault.id, &record(index)).unwrap();
        }
        apply_history(&mut connection, vault.id, &record(21)).unwrap();
        apply_history(&mut connection, vault.id, &record(1)).unwrap();
        let rows = history_since(&connection, vault.id, 0).unwrap();
        assert_eq!(rows.len() as i64, db::HISTORY_LIMIT);
        assert!(rows.iter().all(|(_, row)| row.content_hash != "hash-1"));
        let bytes = encode_history(&rows[0].1).unwrap();
        assert_eq!(decode_history(&bytes).unwrap(), rows[0].1);
        let mut invalid = rows[0].1.clone();
        invalid.path = "../escape".into();
        assert!(decode_history(&encode_history(&invalid).unwrap()).is_err());
    }
}
