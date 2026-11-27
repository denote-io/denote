//! Snapshot and entry model. See `docs/sync.md` ("Snapshot model").

use std::{
    collections::BTreeMap,
    io::{Read, Write},
};

use flate2::{Compression, read::DeflateDecoder, write::DeflateEncoder};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use super::crypto::{FORMAT_VERSION, MAX_SNAPSHOT_BYTES, validate_object_id, validate_writer_id};
use crate::error::{AppError, AppResult};

const MAX_KEY_BYTES: usize = 4096;
const MAX_ENTRIES: usize = 2_000_000;
pub(crate) const MAX_INLINE_BYTES: usize = 256 * 1024;

pub(crate) type VersionVector = BTreeMap<String, u64>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum EntryValue {
    Object { id: String, size: u64, mode: u32 },
    Inline { json: Json },
    Tombstone,
}

impl EntryValue {
    pub(crate) fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone)
    }

    pub(crate) fn object_id(&self) -> Option<&str> {
        match self {
            Self::Object { id, .. } => Some(id),
            _ => None,
        }
    }

    /// Total order used for deterministic tie-breaking after `modified_at`.
    pub(crate) fn order_key(&self) -> String {
        match self {
            Self::Object { id, size, mode } => format!("2:{id}:{size}:{mode}"),
            Self::Inline { json } => format!("1:{json}"),
            Self::Tombstone => "0".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub value: EntryValue,
    /// Milliseconds since the Unix epoch.
    pub modified_at: i64,
    pub origin: String,
    pub vv: VersionVector,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind<'a> {
    VaultFile {
        vault: &'a str,
        path: &'a str,
    },
    VaultMeta {
        vault: &'a str,
    },
    Note {
        vault: &'a str,
        path: &'a str,
    },
    Tag {
        vault: &'a str,
        tag: &'a str,
    },
    Project {
        vault: &'a str,
        root: &'a str,
    },
    History {
        vault: &'a str,
        id: &'a str,
    },
    Preference {
        key: &'a str,
    },
    PluginSetting {
        plugin: &'a str,
        key: &'a str,
    },
    PluginState {
        plugin: &'a str,
        key: &'a str,
    },
    PluginEnabled {
        plugin: &'a str,
    },
    /// A kind from a newer format. Carried through unchanged, never applied.
    Unknown,
}

impl<'a> EntryKind<'a> {
    pub(crate) fn parse(key: &'a str) -> Self {
        let parts: Vec<&str> = key.splitn(4, '/').collect();
        match parts.as_slice() {
            ["v", vault, "f", path] if valid_id(vault) && valid_relative_path(path) => {
                Self::VaultFile { vault, path }
            }
            ["v", vault, "meta"] if valid_id(vault) => Self::VaultMeta { vault },
            ["v", vault, "note", path] if valid_id(vault) && valid_relative_path(path) => {
                Self::Note { vault, path }
            }
            ["v", vault, "tag", tag] if valid_id(vault) && valid_segment(tag) => {
                Self::Tag { vault, tag }
            }
            ["v", vault, "project", root]
                if valid_id(vault) && (root.is_empty() || valid_relative_path(root)) =>
            {
                Self::Project { vault, root }
            }
            ["v", vault, "history", id] if valid_id(vault) && valid_history_id(id) => {
                Self::History { vault, id }
            }
            ["app", "preferences", key] if valid_segment(key) => Self::Preference { key },
            ["plugin", plugin, "settings", key] if valid_segment(plugin) && valid_segment(key) => {
                Self::PluginSetting { plugin, key }
            }
            ["plugin", plugin, "state", key] if valid_segment(plugin) && valid_segment(key) => {
                Self::PluginState { plugin, key }
            }
            ["plugin", plugin, "enabled"] if valid_segment(plugin) => {
                Self::PluginEnabled { plugin }
            }
            _ => Self::Unknown,
        }
    }

    pub(crate) fn vault(&self) -> Option<&'a str> {
        match self {
            Self::VaultFile { vault, .. }
            | Self::VaultMeta { vault }
            | Self::Note { vault, .. }
            | Self::Tag { vault, .. }
            | Self::Project { vault, .. }
            | Self::History { vault, .. } => Some(vault),
            _ => None,
        }
    }

    pub(crate) fn plugin(&self) -> Option<&'a str> {
        match self {
            Self::PluginSetting { plugin, .. }
            | Self::PluginState { plugin, .. }
            | Self::PluginEnabled { plugin } => Some(plugin),
            _ => None,
        }
    }
}

pub(crate) const ENCRYPTION_MANIFEST_PATH: &str = ".denote/encryption.json";

/// A tombstone and an absent value both mean "not present".
pub(crate) fn present(value: Option<&EntryValue>) -> Option<&EntryValue> {
    value.filter(|value| !value.is_tombstone())
}

pub(crate) fn same_presence(left: Option<&EntryValue>, right: Option<&EntryValue>) -> bool {
    present(left) == present(right)
}

/// Tags may contain `/`, so their key segment escapes `%` and `/`.
pub(crate) fn encode_segment(value: &str) -> String {
    value.replace('%', "%25").replace('/', "%2F")
}

pub(crate) fn decode_segment(value: &str) -> Option<String> {
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('%') {
        decoded.push_str(&rest[..index]);
        match rest.get(index..index + 3) {
            Some("%25") => decoded.push('%'),
            Some("%2F") => decoded.push('/'),
            _ => return None,
        }
        rest = &rest[index + 3..];
    }
    decoded.push_str(rest);
    Some(decoded)
}

pub(crate) fn valid_vault_id(value: &str) -> bool {
    valid_id(value)
}

pub(crate) fn file_key(vault: &str, path: &str) -> String {
    format!("v/{vault}/f/{path}")
}

pub(crate) fn is_manifest_key(key: &str) -> bool {
    matches!(EntryKind::parse(key), EntryKind::VaultFile { path, .. } if path == ENCRYPTION_MANIFEST_PATH)
}

fn valid_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_history_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains('/')
        && value != "."
        && value != ".."
        && !value.chars().any(char::is_control)
}

/// A forward-slash relative path without empty, `.`, `..`, backslash, or control parts.
pub(crate) fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_KEY_BYTES
        && !value.contains('\\')
        && !value.contains(':')
        && value.split('/').all(valid_segment)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    pub format: u32,
    pub repository: String,
    pub writer: String,
    pub generation: u64,
    pub created_at: String,
    pub app_version: String,
    #[serde(default)]
    pub device_name: String,
    /// Display names of the vaults this writer syncs, by vault sync ID.
    #[serde(default)]
    pub vaults: BTreeMap<String, String>,
    pub entries: BTreeMap<String, Entry>,
}

impl Snapshot {
    /// Deflate-compressed JSON, ready for sealing.
    pub(crate) fn encode(&self) -> AppResult<Vec<u8>> {
        let json = serde_json::to_vec(self).map_err(|error| {
            AppError::InvalidData(format!("Unable to encode sync snapshot: {error}"))
        })?;
        if json.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(AppError::InvalidData(
                "The sync snapshot exceeds the 64 MiB limit. Stop syncing a large vault or remove files."
                    .into(),
            ));
        }
        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&json)?;
        Ok(encoder.finish()?)
    }

    pub(crate) fn decode(
        bytes: &[u8],
        repository: &str,
        writer: &str,
        generation: u64,
    ) -> AppResult<Self> {
        let mut json = Vec::new();
        DeflateDecoder::new(bytes)
            .take(MAX_SNAPSHOT_BYTES + 1)
            .read_to_end(&mut json)
            .map_err(|_| AppError::InvalidData("A sync snapshot is damaged".into()))?;
        if json.len() as u64 > MAX_SNAPSHOT_BYTES {
            return Err(AppError::InvalidData(
                "A sync snapshot exceeds the 64 MiB limit".into(),
            ));
        }
        let snapshot: Snapshot = serde_json::from_slice(&json).map_err(|error| {
            AppError::InvalidData(format!("A sync snapshot is invalid: {error}"))
        })?;
        if snapshot.format != FORMAT_VERSION {
            return Err(AppError::InvalidData(format!(
                "Sync snapshot format {} is not supported",
                snapshot.format
            )));
        }
        if snapshot.repository != repository
            || snapshot.writer != writer
            || snapshot.generation != generation
        {
            return Err(AppError::InvalidData(
                "A sync snapshot does not match its location".into(),
            ));
        }
        if snapshot.entries.len() > MAX_ENTRIES
            || snapshot.device_name.len() > 256
            || snapshot.vaults.len() > 10_000
            || snapshot
                .vaults
                .iter()
                .any(|(id, name)| !valid_id(id) || name.len() > 256)
        {
            return Err(AppError::InvalidData("A sync snapshot is too large".into()));
        }
        for (key, entry) in &snapshot.entries {
            validate_entry(key, entry)?;
        }
        Ok(snapshot)
    }
}

pub(crate) fn validate_entry(key: &str, entry: &Entry) -> AppResult<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES || key.chars().any(char::is_control) {
        return Err(AppError::InvalidData(
            "A sync entry has an invalid key".into(),
        ));
    }
    validate_writer_id(&entry.origin)?;
    if entry.vv.is_empty() || entry.vv.len() > 10_000 {
        return Err(AppError::InvalidData(
            "A sync entry has an invalid version vector".into(),
        ));
    }
    for (writer, counter) in &entry.vv {
        validate_writer_id(writer)?;
        if *counter == 0 {
            return Err(AppError::InvalidData(
                "A sync entry has an invalid version vector".into(),
            ));
        }
    }
    match &entry.value {
        EntryValue::Object { id, size, .. } => {
            validate_object_id(id)?;
            if *size > super::crypto::MAX_FILE_BYTES {
                return Err(AppError::InvalidData(
                    "A sync entry exceeds the file size limit".into(),
                ));
            }
        }
        EntryValue::Inline { json } => {
            if json.to_string().len() > MAX_INLINE_BYTES {
                return Err(AppError::InvalidData(
                    "A sync entry value is too large".into(),
                ));
            }
        }
        EntryValue::Tombstone => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: &str = "0123456789abcdef0123456789abcdef";
    const WRITER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn tag_segments_round_trip() {
        for tag in ["plain", "area/sub", "100%", "%2F/%25"] {
            let encoded = encode_segment(tag);
            assert!(!encoded.contains('/'));
            assert_eq!(decode_segment(&encoded).as_deref(), Some(tag));
        }
        assert_eq!(decode_segment("bad%2"), None);
        assert_eq!(decode_segment("bad%41"), None);
        assert!(same_presence(None, Some(&EntryValue::Tombstone)));
    }

    #[test]
    fn keys_parse_into_known_kinds_and_preserve_unknown_ones() {
        assert_eq!(
            EntryKind::parse(&format!("v/{VAULT}/f/notes/a.md")),
            EntryKind::VaultFile {
                vault: VAULT,
                path: "notes/a.md"
            }
        );
        assert_eq!(
            EntryKind::parse(&format!("v/{VAULT}/meta")),
            EntryKind::VaultMeta { vault: VAULT }
        );
        assert_eq!(
            EntryKind::parse(&format!("v/{VAULT}/project/")),
            EntryKind::Project {
                vault: VAULT,
                root: ""
            }
        );
        assert_eq!(
            EntryKind::parse("plugin/denote.git/settings/remoteName"),
            EntryKind::PluginSetting {
                plugin: "denote.git",
                key: "remoteName"
            }
        );
        assert_eq!(
            EntryKind::parse("plugin/denote.x/enabled"),
            EntryKind::PluginEnabled { plugin: "denote.x" }
        );
        for key in [
            "future/kind",
            &format!("v/{VAULT}/f/../escape"),
            &format!("v/{VAULT}/f/a//b"),
            &format!("v/{VAULT}/f/a\\b"),
            "v/short/meta",
        ] {
            assert_eq!(EntryKind::parse(key), EntryKind::Unknown, "{key}");
        }
        assert!(is_manifest_key(&file_key(VAULT, ENCRYPTION_MANIFEST_PATH)));
    }

    #[test]
    fn snapshots_round_trip_and_bind_their_location() {
        let mut entries = BTreeMap::new();
        entries.insert(
            "future/kind".to_string(),
            Entry {
                value: EntryValue::Inline {
                    json: serde_json::json!({"x": 1}),
                },
                modified_at: 1,
                origin: WRITER.into(),
                vv: BTreeMap::from([(WRITER.to_string(), 1)]),
            },
        );
        let snapshot = Snapshot {
            format: 1,
            repository: VAULT.into(),
            writer: WRITER.into(),
            generation: 2,
            created_at: "2026-01-01T00:00:00+00:00".into(),
            app_version: "test".into(),
            device_name: "Synthetic".into(),
            vaults: BTreeMap::from([(VAULT.to_string(), "Notes".to_string())]),
            entries,
        };
        let encoded = snapshot.encode().unwrap();
        assert_eq!(
            Snapshot::decode(&encoded, VAULT, WRITER, 2).unwrap(),
            snapshot
        );
        assert!(Snapshot::decode(&encoded, VAULT, WRITER, 3).is_err());
        assert!(Snapshot::decode(&encoded, WRITER, WRITER, 2).is_err());
        assert!(Snapshot::decode(b"garbage", VAULT, WRITER, 2).is_err());

        let mut invalid = snapshot.clone();
        for entry in invalid.entries.values_mut() {
            entry.vv.insert(WRITER.into(), 0);
        }
        assert!(Snapshot::decode(&invalid.encode().unwrap(), VAULT, WRITER, 2).is_err());
    }
}
