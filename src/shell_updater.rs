use std::sync::Mutex;
use tauri::{AppHandle, State};
use tauri_plugin_updater::UpdaterExt;

#[derive(Default)]
pub struct ShellUpdateState(pub Mutex<Option<String>>);

/// Checks the remote manifest and caches the result — cheap to poll from the frontend
/// afterward via get_shell_update_status without hitting the network each time.
#[tauri::command]
pub async fn check_for_shell_update(
    app: AppHandle,
    state: State<'_, ShellUpdateState>,
) -> Result<Option<String>, String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    let found = updater.check().await.map_err(|e| e.to_string())?;
    let version = found.as_ref().map(|u| u.version.clone());
    *state.0.lock().map_err(|e| e.to_string())? = version.clone();
    Ok(version)
}

#[tauri::command]
pub fn get_shell_update_status(state: State<'_, ShellUpdateState>) -> Result<Option<String>, String> {
    Ok(state.0.lock().map_err(|e| e.to_string())?.clone())
}

/// Downloads, verifies the signature, installs, and restarts. Call this only when the
/// till is safe to interrupt — e.g. the cart is empty — not mid-order.
#[tauri::command]
pub async fn install_shell_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(|e| e.to_string())?;
    if let Some(update) = updater.check().await.map_err(|e| e.to_string())? {
        update
            .download_and_install(|_chunk, _total| {}, || {})
            .await
            .map_err(|e| e.to_string())?;
        app.restart();
    }
    Ok(())
}