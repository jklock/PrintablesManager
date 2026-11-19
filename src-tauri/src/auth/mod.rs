//! Printables account session handling.
//!
//! Printables authenticates through an Auth0 single-page application, so the
//! API expects an `Authorization: Bearer <access_token>` header. We obtain that
//! token by opening a real login window (so Google/Apple SSO and 2FA work
//! exactly as they do on the website) and reading the token the page caches.
//!
//! The token is stored in the macOS Keychain, with a `0600` file fallback for
//! ad-hoc-signed builds where Keychain access is unreliable.

pub mod login;

use crate::config;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

const KEYCHAIN_SERVICE: &str = "dev.printablesoffline.app";
const KEYCHAIN_ACCOUNT: &str = "printables-session";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub access_token: String,
    /// Unix seconds. `None` when the server did not report a lifetime.
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub stored_at: String,
    #[serde(default)]
    pub backend: String,
}

impl StoredSession {
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => chrono::Utc::now().timestamp() >= exp - 30,
            None => false,
        }
    }
}

/// Public status shape consumed by the UI. Never contains the token itself.
#[derive(Debug, Clone, Serialize, Default)]
pub struct AuthStatus {
    pub signed_in: bool,
    pub username: Option<String>,
    pub expires_at: Option<i64>,
    pub expired: bool,
    pub storage: Option<String>,
}

pub struct AuthState {
    session: Mutex<Option<StoredSession>>,
    pub login_running: Mutex<bool>,
}

impl AuthState {
    pub fn new() -> Self {
        let session = load_session().ok().flatten();
        Self {
            session: Mutex::new(session),
            login_running: Mutex::new(false),
        }
    }

    pub fn status(&self) -> AuthStatus {
        let guard = match self.session.lock() {
            Ok(g) => g,
            Err(_) => return AuthStatus::default(),
        };
        match guard.as_ref() {
            Some(s) => AuthStatus {
                signed_in: !s.is_expired(),
                username: s.username.clone(),
                expires_at: s.expires_at,
                expired: s.is_expired(),
                storage: Some(s.backend.clone()),
            },
            None => AuthStatus::default(),
        }
    }

    pub fn token(&self) -> Option<String> {
        let guard = self.session.lock().ok()?;
        guard
            .as_ref()
            .filter(|s| !s.is_expired())
            .map(|s| s.access_token.clone())
    }

    pub fn set(&self, session: StoredSession) -> AppResult<()> {
        save_session(&session)?;
        let mut guard = self
            .session
            .lock()
            .map_err(|_| AppError::internal("Session lock poisoned"))?;
        *guard = Some(session);
        Ok(())
    }

    pub fn clear(&self) -> AppResult<()> {
        delete_session();
        let mut guard = self
            .session
            .lock()
            .map_err(|_| AppError::internal("Session lock poisoned"))?;
        *guard = None;
        Ok(())
    }
}

impl Default for AuthState {
    fn default() -> Self {
        Self::new()
    }
}

fn keychain_entry() -> AppResult<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| AppError::internal(format!("Keychain unavailable: {e}")))
}

pub fn load_session() -> AppResult<Option<StoredSession>> {
    if let Ok(entry) = keychain_entry() {
        if let Ok(secret) = entry.get_password() {
            if let Ok(session) = serde_json::from_str::<StoredSession>(&secret) {
                return Ok(Some(session));
            }
        }
    }

    let path = config::token_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let mut session: StoredSession = serde_json::from_str(&raw)?;
    session.backend = "file".to_string();
    Ok(Some(session))
}

fn save_session(session: &StoredSession) -> AppResult<()> {
    let mut stored = session.clone();

    if let Ok(entry) = keychain_entry() {
        let payload = serde_json::to_string(&stored)?;
        if entry.set_password(&payload).is_ok() {
            stored.backend = "keychain".to_string();
            // Keep a file copy only when the Keychain refused the write.
            let _ = fs::remove_file(config::token_path()?);
            return Ok(());
        }
    }

    stored.backend = "file".to_string();
    write_private_file(
        &config::token_path()?,
        &serde_json::to_string_pretty(&stored)?,
    )?;
    Ok(())
}

fn write_private_file(path: &Path, contents: &str) -> AppResult<()> {
    fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(path, perms)?;
    }
    Ok(())
}

fn delete_session() {
    if let Ok(entry) = keychain_entry() {
        let _ = entry.delete_credential();
    }
    if let Ok(path) = config::token_path() {
        let _ = fs::remove_file(path);
    }
}

/// Persist a freshly captured token and fill in the account name.
pub fn adopt_token(access_token: String, expires_in: Option<i64>) -> AppResult<StoredSession> {
    let expires_at = expires_in.map(|secs| chrono::Utc::now().timestamp() + secs);
    let mut session = StoredSession {
        access_token,
        expires_at,
        username: None,
        stored_at: chrono::Utc::now().to_rfc3339(),
        backend: String::new(),
    };

    // Identify the account. Failure here must not lose a valid token.
    if let Ok(client) =
        crate::printables::api::PrintablesClient::with_token(Some(session.access_token.clone()))
    {
        if let Ok(viewer) = client.viewer() {
            session.username = viewer.public_username;
        }
    }

    save_session(&session)?;
    let backend = load_session()?.map(|s| s.backend).unwrap_or_default();
    session.backend = backend;
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_is_evaluated_against_wall_clock() {
        let past = StoredSession {
            access_token: "t".into(),
            expires_at: Some(chrono::Utc::now().timestamp() - 10),
            username: None,
            stored_at: String::new(),
            backend: "file".into(),
        };
        assert!(past.is_expired());

        let future = StoredSession {
            expires_at: Some(chrono::Utc::now().timestamp() + 3600),
            ..past.clone()
        };
        assert!(!future.is_expired());

        let unknown = StoredSession {
            expires_at: None,
            ..past
        };
        assert!(
            !unknown.is_expired(),
            "unknown lifetime must not be treated as expired"
        );
    }
}
