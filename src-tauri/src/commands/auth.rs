//! Account commands: sign in, sign out, status, token escape hatch, and the
//! (static, truthful) statement of what the push path supports.

use crate::auth::{login, AuthState, AuthStatus, StoredSession};
use crate::error::{AppError, AppResult};
use crate::printables::api::PrintablesClient;
use crate::sync::push;
use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, State};

#[tauri::command]
pub fn auth_status(auth: State<'_, AuthState>) -> AuthStatus {
    auth.status()
}

/// Opens the Printables sign-in window. Completion arrives as `auth://changed`.
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
    let me = client.viewer()?;

    let mut status = auth.status();
    status.signed_in = true;
    status.username = me.public_username.or(me.handle);
    Ok(status)
}

#[tauri::command]
pub fn auth_refresh(auth: State<'_, AuthState>) -> AppResult<AuthStatus> {
    auth.refresh()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenArgs {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// Escape hatch: adopt a token copied out of the browser.
#[tauri::command]
pub fn auth_set_token(args: TokenArgs, auth: State<'_, AuthState>) -> AppResult<AuthStatus> {
    let session: StoredSession = login::adopt_pasted_token(args.access_token, args.refresh_token)?;
    auth.set(session)?;
    Ok(auth.status())
}

/// What publishing supports. Real values, not a schema probe: printables.com
/// has GraphQL introspection disabled, so the operations are pinned in code.
#[tauri::command]
pub fn auth_capabilities() -> Value {
    push::capabilities()
}

/// Round-trips a throwaway item through the login Keychain so the user can see
/// whether this build's signature may use it.
#[tauri::command]
pub fn auth_keychain_test() -> crate::auth::KeychainReport {
    crate::auth::keychain_selftest()
}

/// Retries storing the current session in the Keychain after a file fallback.
#[tauri::command]
pub fn auth_storage_retry(auth: State<'_, AuthState>) -> AppResult<AuthStatus> {
    auth.promote_to_keychain()
}
