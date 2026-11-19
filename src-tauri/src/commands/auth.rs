//! Account commands: sign in, sign out, status, and write-API discovery.

use crate::auth::{login, AuthState, AuthStatus};
use crate::config;
use crate::error::{AppError, AppResult};
use crate::models::AppPrefs;
use crate::printables::api::PrintablesClient;
use crate::sync::capabilities::{self, Capabilities};
use std::sync::Mutex;
use tauri::{AppHandle, State};

#[tauri::command]
pub fn auth_status(auth: State<'_, AuthState>) -> AuthStatus {
    auth.status()
}

/// Opens the Printables sign-in window. Completion arrives as an
/// `auth://changed` event.
#[tauri::command]
pub fn auth_login(app: AppHandle) -> AppResult<()> {
    login::start_login(&app)
}

#[tauri::command]
pub fn auth_logout(auth: State<'_, AuthState>) -> AppResult<AuthStatus> {
    auth.clear()?;
    Ok(auth.status())
}

/// Verifies the stored session against the API and returns the account name.
#[tauri::command]
pub fn auth_verify(auth: State<'_, AuthState>) -> AppResult<AuthStatus> {
    let token = auth
        .token()
        .ok_or_else(|| AppError::not_authenticated("Not signed in to Printables."))?;
    let client = PrintablesClient::with_token(Some(token))?;
    let viewer = client.viewer()?;

    let mut status = auth.status();
    status.signed_in = true;
    status.username = viewer.public_username;
    Ok(status)
}

/// Introspects the GraphQL schema for write mutations and stores the result in
/// `<library>/.manager/capabilities.json`.
#[tauri::command]
pub fn auth_probe_write_api(
    prefs: State<'_, Mutex<AppPrefs>>,
    auth: State<'_, AuthState>,
) -> AppResult<Capabilities> {
    let snapshot = {
        let prefs = prefs
            .lock()
            .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
        prefs.clone()
    };
    let library = config::library_dir(&snapshot)?;
    let token = auth
        .token()
        .ok_or_else(|| AppError::not_authenticated("Sign in before probing the write API."))?;
    let client = PrintablesClient::with_token(Some(token))?;
    capabilities::probe(&client, &library)
}

/// Returns the cached capability report, if any.
#[tauri::command]
pub fn auth_cached_capabilities(
    prefs: State<'_, Mutex<AppPrefs>>,
) -> AppResult<Option<Capabilities>> {
    let snapshot = {
        let prefs = prefs
            .lock()
            .map_err(|_| AppError::internal("Preferences lock poisoned"))?;
        prefs.clone()
    };
    let library = config::library_dir(&snapshot)?;
    Ok(capabilities::load(&library))
}
