//! Tauri commands and host wiring for whole-app sync. See `docs/sync.md`
//! ("Native commands"). Every command requires the caller's enabled `sync`
//! permission, which also refuses under app lock and in safe mode.

use std::collections::BTreeMap;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_opener::OpenerExt;
use zeroize::Zeroizing;

use super::{
    config::{SyncOptions, SyncSetupRequest, SyncVaultRequest},
    engine::VaultLock,
    plugin_data::PluginDataSource,
    service::{SecretStore, SyncEnvironment, SyncService},
    status::{GoogleConnectionView, SyncAppliedEvent, SyncStatusView},
};
use crate::{
    db::AppState,
    dialogs::{self, SelectionKind, SelectionOptions},
    error::{AppError, AppResult},
    plugins::PluginManager,
};

pub(crate) const SYNC_STATUS_EVENT: &str = "denote://sync-status";
pub(crate) const SYNC_APPLIED_EVENT: &str = "denote://sync-applied";

impl SecretStore for PluginManager {
    fn get(&self, plugin_id: &str, name: &str) -> AppResult<Option<Zeroizing<String>>> {
        Ok(self.sync_secret_get(plugin_id, name)?.map(Zeroizing::new))
    }

    fn set(&self, plugin_id: &str, name: &str, value: &str) -> AppResult<()> {
        self.sync_secret_set(plugin_id, name, value)
    }

    fn delete(&self, plugin_id: &str, name: &str) -> AppResult<()> {
        self.sync_secret_delete(plugin_id, name)
    }
}

impl VaultLock for AppState {
    /// The same in-process lock every vault mutation command holds.
    fn with_vault_lock(&self, operation: &mut dyn FnMut() -> AppResult<()>) -> AppResult<()> {
        let _access = self.write_vault_access()?;
        operation()
    }
}

pub(crate) struct TauriSyncEnvironment {
    app: AppHandle,
}

impl TauriSyncEnvironment {
    pub(crate) fn new(app: AppHandle) -> Self {
        Self { app }
    }

    fn plugins(&self) -> AppResult<&PluginManager> {
        self.app
            .try_state::<PluginManager>()
            .map(|state| state.inner())
            .ok_or_else(|| AppError::State("Plugins are unavailable".into()))
    }
}

impl SyncEnvironment for TauriSyncEnvironment {
    fn authorize(&self, plugin_id: &str) -> AppResult<()> {
        self.app
            .try_state::<AppState>()
            .ok_or(AppError::AppLocked)?
            .app_lock
            .ensure_unlocked()?;
        self.plugins()?.enabled_permission(plugin_id, "sync")?;
        Ok(())
    }

    fn plugin_data(&self) -> Option<&dyn PluginDataSource> {
        self.plugins()
            .ok()
            .map(|plugins| plugins as &dyn PluginDataSource)
    }

    fn secrets(&self) -> AppResult<&dyn SecretStore> {
        Ok(self.plugins()?)
    }

    fn vault_lock(&self) -> AppResult<&dyn VaultLock> {
        self.app
            .try_state::<AppState>()
            .map(|state| state.inner() as &dyn VaultLock)
            .ok_or(AppError::AppLocked)
    }

    fn backend_plugins(&self) -> Option<PluginManager> {
        self.plugins().ok().cloned()
    }

    fn open_url(&self, url: &str) -> AppResult<()> {
        self.app
            .opener()
            .open_url(url, None::<String>)
            .map_err(|error| AppError::State(format!("Unable to open the sign-in page: {error}")))
    }

    fn emit_status(&self, status: &SyncStatusView) {
        if let Err(error) = self.app.emit(SYNC_STATUS_EVENT, status) {
            eprintln!("Unable to announce the sync status: {error}");
        }
    }

    fn emit_applied(&self, applied: &SyncAppliedEvent) {
        if let Err(error) = self.app.emit(SYNC_APPLIED_EVENT, applied) {
            eprintln!("Unable to announce synced changes: {error}");
        }
    }

    fn app_version(&self) -> String {
        self.app.package_info().version.to_string()
    }
}

fn service(app: &AppHandle) -> AppResult<SyncService> {
    app.try_state::<SyncService>()
        .map(|state| state.inner().clone())
        .ok_or_else(|| AppError::State("Sync is unavailable in this session".into()))
}

async fn with_service<T, F>(app: AppHandle, operation: F) -> AppResult<T>
where
    T: Send + 'static,
    F: FnOnce(&SyncService) -> AppResult<T> + Send + 'static,
{
    let service = service(&app)?;
    tauri::async_runtime::spawn_blocking(move || operation(&service))
        .await
        .map_err(|error| AppError::State(format!("Background task failed: {error}")))?
}

#[tauri::command]
pub async fn sync_status(app: AppHandle, plugin_id: String) -> AppResult<SyncStatusView> {
    with_service(app, move |service| service.status(&plugin_id)).await
}

#[tauri::command]
pub async fn sync_configure(
    app: AppHandle,
    plugin_id: String,
    request: SyncSetupRequest,
) -> AppResult<SyncStatusView> {
    with_service(app, move |service| service.configure(&plugin_id, request)).await
}

#[tauri::command]
pub async fn sync_connect_google(
    app: AppHandle,
    plugin_id: String,
    client_id: Option<String>,
) -> AppResult<GoogleConnectionView> {
    with_service(app, move |service| {
        service.connect_google(&plugin_id, client_id)
    })
    .await
}

#[tauri::command]
pub async fn sync_unlock(
    app: AppHandle,
    plugin_id: String,
    passphrase: String,
    remember: bool,
) -> AppResult<SyncStatusView> {
    let passphrase = Zeroizing::new(passphrase);
    with_service(app, move |service| {
        service.unlock(&plugin_id, &passphrase, remember)
    })
    .await
}

/// Resolves when the cycle ends; progress arrives through `denote://sync-status`.
#[tauri::command]
pub async fn sync_run(app: AppHandle, plugin_id: String) -> AppResult<SyncStatusView> {
    with_service(app, move |service| service.run_now(&plugin_id)).await
}

#[tauri::command]
pub async fn sync_cancel(app: AppHandle, plugin_id: String) -> AppResult<()> {
    with_service(app, move |service| service.cancel(&plugin_id)).await
}

#[tauri::command]
pub async fn sync_set_options(
    app: AppHandle,
    plugin_id: String,
    options: SyncOptions,
) -> AppResult<SyncStatusView> {
    with_service(app, move |service| service.set_options(&plugin_id, options)).await
}

#[tauri::command]
pub async fn sync_map_vault(
    app: AppHandle,
    plugin_id: String,
    request: SyncVaultRequest,
) -> AppResult<SyncStatusView> {
    with_service(app, move |service| service.map_vault(&plugin_id, request)).await
}

#[tauri::command]
pub async fn sync_resolve(
    app: AppHandle,
    plugin_id: String,
    conflict_id: String,
    choice: String,
) -> AppResult<SyncStatusView> {
    with_service(app, move |service| {
        service.resolve(&plugin_id, &conflict_id, &choice)
    })
    .await
}

#[tauri::command]
pub async fn sync_put_preferences(
    app: AppHandle,
    plugin_id: String,
    preferences: BTreeMap<String, Option<String>>,
) -> AppResult<()> {
    with_service(app, move |service| {
        service.put_preferences(&plugin_id, preferences)
    })
    .await
}

#[tauri::command]
pub async fn sync_retire_device(
    app: AppHandle,
    plugin_id: String,
    writer_id: String,
) -> AppResult<SyncStatusView> {
    with_service(app, move |service| {
        service.retire_device(&plugin_id, &writer_id)
    })
    .await
}

#[tauri::command]
pub async fn sync_change_passphrase(
    app: AppHandle,
    plugin_id: String,
    current: String,
    next: String,
) -> AppResult<SyncStatusView> {
    let current = Zeroizing::new(current);
    let next = Zeroizing::new(next);
    with_service(app, move |service| {
        service.change_passphrase(&plugin_id, &current, &next)
    })
    .await
}

#[tauri::command]
pub async fn sync_disconnect(app: AppHandle, plugin_id: String) -> AppResult<SyncStatusView> {
    with_service(app, move |service| service.disconnect(&plugin_id)).await
}

/// Opens the host folder chooser for a folder backend or a new vault folder.
#[tauri::command]
pub async fn sync_choose_folder(
    app: AppHandle,
    plugin_id: String,
    purpose: String,
) -> AppResult<Option<String>> {
    let title = match purpose.as_str() {
        "backend" => "Choose a folder for sync storage",
        "vault" => "Choose an empty folder for the synced vault",
        _ => return Err(AppError::InvalidData("Unknown folder purpose".into())),
    };
    let checked = app.clone();
    with_service(checked, move |service| service.authorize_plugin(&plugin_id)).await?;
    let Some(path) =
        dialogs::select(&app, SelectionKind::Folder, SelectionOptions::new(title)).await?
    else {
        return Ok(None);
    };
    path.to_str()
        .map(|path| Some(path.to_string()))
        .ok_or_else(|| AppError::InvalidPath("The chosen folder name is not valid Unicode".into()))
}
