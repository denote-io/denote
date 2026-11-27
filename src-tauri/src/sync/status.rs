//! Renderer-facing sync payloads. See `docs/sync.md` ("Command payloads").

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    config::SyncOptions,
    engine::{CycleReport, Phase},
};

const MAX_REPORTED_ITEMS: usize = 200;
const MAX_REPORTED_WARNINGS: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SyncState {
    NotConfigured,
    NeedsPassphrase,
    Idle,
    Running,
    Paused,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct BackendView {
    pub kind: &'static str,
    pub label: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ProgressView {
    pub phase: Phase,
    pub done: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastRun {
    pub finished_at: String,
    pub ok: bool,
    pub message: String,
    pub uploaded: usize,
    pub downloaded: usize,
    pub applied: usize,
    pub conflicts: usize,
    pub skipped: Vec<String>,
}

/// The stored last run with the warnings the status view lists separately.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LastRunRecord {
    pub run: LastRun,
    pub warnings: Vec<String>,
}

impl LastRunRecord {
    pub(crate) fn from_report(report: &CycleReport, error: Option<String>) -> Self {
        let message = match (&error, report.published) {
            (Some(error), _) => error.clone(),
            (None, _) if report.applied == 0 && report.uploaded == 0 => "Up to date".into(),
            (None, _) => format!(
                "Applied {} change(s) and uploaded {} item(s)",
                report.applied, report.uploaded
            ),
        };
        Self {
            run: LastRun {
                finished_at: crate::db::now(),
                ok: error.is_none(),
                message,
                uploaded: report.uploaded,
                downloaded: report.downloaded,
                applied: report.applied,
                conflicts: report.conflicts,
                skipped: report
                    .skipped
                    .iter()
                    .take(MAX_REPORTED_ITEMS)
                    .cloned()
                    .collect(),
            },
            warnings: report
                .warnings
                .iter()
                .take(MAX_REPORTED_WARNINGS)
                .cloned()
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceView {
    pub writer_id: String,
    pub name: String,
    /// Another device removed this one; it syncs again only after a rejoin.
    pub removed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceEntryView {
    pub writer_id: String,
    pub name: String,
    pub last_generation: u64,
    pub last_seen_at: Option<String>,
    pub retired: bool,
    pub is_self: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VaultViewState {
    Synced,
    NotEnrolled,
    PendingMapping,
    Skipped,
    PausedConflict,
    PausedEncryption,
    /// A scan would delete most of the vault; waiting for the files or a confirmation.
    PausedDeletions,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VaultView {
    pub sync_id: Option<String>,
    pub name: String,
    pub scope_id: Option<String>,
    pub path: Option<String>,
    pub state: VaultViewState,
    pub message: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConflictView {
    pub id: String,
    pub kind: &'static str,
    pub vault_sync_id: Option<String>,
    pub plugin_id: Option<String>,
    pub key: Option<String>,
    pub local_summary: String,
    pub remote_summary: String,
    pub remote_device: String,
    pub detected_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SuggestionView {
    pub plugin_id: String,
    pub from_device: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncStatusView {
    pub state: SyncState,
    pub backend: Option<BackendView>,
    pub remember_passphrase: bool,
    pub options: SyncOptions,
    pub progress: Option<ProgressView>,
    pub last_run: Option<LastRun>,
    pub device: Option<DeviceView>,
    pub devices: Vec<DeviceEntryView>,
    pub vaults: Vec<VaultView>,
    pub conflicts: Vec<ConflictView>,
    pub plugin_suggestions: Vec<SuggestionView>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncAppliedEvent {
    pub vault_roots: Vec<String>,
    pub preferences: BTreeMap<String, Option<String>>,
    pub plugin_ids: Vec<String>,
}

impl SyncAppliedEvent {
    pub(crate) fn from_report(report: &CycleReport) -> Option<Self> {
        let event = Self {
            vault_roots: report.vault_roots.iter().cloned().collect(),
            preferences: report.preferences.clone(),
            plugin_ids: report.plugin_ids.iter().cloned().collect(),
        };
        (!event.vault_roots.is_empty()
            || !event.preferences.is_empty()
            || !event.plugin_ids.is_empty())
        .then_some(event)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GoogleConnectionView {
    pub connection_token: String,
    pub account_label: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_serialize_with_the_documented_names() {
        let view = SyncStatusView {
            state: SyncState::NeedsPassphrase,
            backend: Some(BackendView {
                kind: "folder",
                label: "/synthetic".into(),
            }),
            remember_passphrase: false,
            options: SyncOptions::default(),
            progress: Some(ProgressView {
                phase: Phase::Download,
                done: 1,
                total: 2,
            }),
            last_run: Some(LastRun::default()),
            device: None,
            devices: vec![],
            vaults: vec![VaultView {
                sync_id: None,
                name: "Notes".into(),
                scope_id: Some("scope".into()),
                path: None,
                state: VaultViewState::NotEnrolled,
                message: None,
            }],
            conflicts: vec![],
            plugin_suggestions: vec![],
            warnings: vec![],
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["state"], "needs-passphrase");
        assert_eq!(json["options"]["intervalMinutes"], 15);
        assert_eq!(json["progress"]["phase"], "download");
        assert_eq!(json["vaults"][0]["state"], "not-enrolled");
        assert!(json["lastRun"].get("warnings").is_none());
        assert!(json["lastRun"].get("finishedAt").is_some());
        assert!(json.get("pluginSuggestions").is_some());
        let applied = SyncAppliedEvent {
            preferences: BTreeMap::from([("denote-theme".into(), None)]),
            ..SyncAppliedEvent::default()
        };
        let json = serde_json::to_value(&applied).unwrap();
        assert!(json["preferences"]["denote-theme"].is_null());
        assert!(json["vaultRoots"].is_array() && json["pluginIds"].is_array());
    }
}
