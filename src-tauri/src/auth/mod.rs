//! Printables account session handling.
//!
//! printables.com authenticates with the Prusa Account OAuth service and keeps
//! the result in two cookies on `www.printables.com`:
//!
//!   * `auth.access_token`  — sent to the API as `Authorization: Bearer …`
//!   * `auth.refresh_token` — exchanged at `POST /auth/refresh`
//!
//! Both were recovered from the site's own client bundle and verified against
//! the live API (a bearer-less request to `me` returns 401; the API also
//! accepts `credentials: include`). We therefore do not parse OAuth redirects:
//! we let the user sign in in a real window and read the resulting cookies.
//!
//! Tokens live in the macOS Keychain, with a `0600` file fallback.

pub mod login;

use crate::config;
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::Mutex;

const KEYCHAIN_SERVICE: &str = "dev.printablesmanager.app";
/// Item written by the build that was still called "Printables Offline".
const KEYCHAIN_SERVICE_PREVIOUS: &str = "dev.printablesoffline.app";
const KEYCHAIN_ACCOUNT: &str = "printables-session";
const KEYCHAIN_PROBE_ACCOUNT: &str = "keychain-selftest";
const REFRESH_URL: &str = "https://www.printables.com/auth/refresh";

/// Remembers why the last Keychain write fell back to a file, so the UI can
/// say so instead of quietly storing a token on disk.
static KEYCHAIN_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn note_keychain_error(message: Option<String>) {
    if let Ok(mut slot) = KEYCHAIN_ERROR.lock() {
        *slot = message;
    }
}

pub fn keychain_error() -> Option<String> {
    KEYCHAIN_ERROR.lock().ok().and_then(|slot| slot.clone())
}

/// Result of a real read/write/delete round-trip against the login Keychain.
#[derive(Debug, Clone, Serialize)]
pub struct KeychainReport {
    pub available: bool,
    pub writable: bool,
    pub readable: bool,
    pub service: String,
    pub error: Option<String>,
}

impl KeychainReport {
    pub fn healthy(&self) -> bool {
        self.available && self.writable && self.readable
    }
}

/// Writes, reads back and deletes a throwaway item. This is the only honest
/// way to know whether the Keychain works for *this* build's signature.
pub fn keychain_selftest() -> KeychainReport {
    let mut report = KeychainReport {
        available: false,
        writable: false,
        readable: false,
        service: KEYCHAIN_SERVICE.to_string(),
        error: None,
    };

    let entry = match keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_PROBE_ACCOUNT) {
        Ok(entry) => entry,
        Err(err) => {
            report.error = Some(format!("Keychain unavailable: {err}"));
            note_keychain_error(report.error.clone());
            return report;
        }
    };
    report.available = true;

    let probe = format!("ok-{}", uuid::Uuid::new_v4());
    if let Err(err) = entry.set_password(&probe) {
        report.error = Some(format!("Keychain write failed: {err}"));
        note_keychain_error(report.error.clone());
        return report;
    }
    report.writable = true;

    match entry.get_password() {
        Ok(value) if value == probe => report.readable = true,
        Ok(_) => report.error = Some("Keychain returned a different value".to_string()),
        Err(err) => report.error = Some(format!("Keychain read failed: {err}")),
    }

    let _ = entry.delete_credential();
    note_keychain_error(report.error.clone());
    report
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSession {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds, taken from the JWT `exp` when available.
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub stored_at: String,
    #[serde(default)]
    pub backend: String,
}

impl StoredSession {
    /// Expiry is taken from the stored field, and falls back to the JWT's own
    /// `exp`. Without that fallback a session written without `expires_at` looks
    /// valid forever while every API call 401s - which showed up as "signed in"
    /// in the UI and "download failed" on every request.
    pub fn expiry(&self) -> Option<i64> {
        self.expires_at.or_else(|| token_exp(&self.access_token))
    }

    pub fn is_expired(&self) -> bool {
        match self.expiry() {
            Some(exp) => chrono::Utc::now().timestamp() >= exp - 60,
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
    /// "keychain" | "file" | None when signed out.
    pub storage: Option<String>,
    /// Set when the token had to be written outside the Keychain.
    pub storage_warning: Option<String>,
    pub can_refresh: bool,
}

pub struct AuthState {
    session: Mutex<Option<StoredSession>>,
    /// Set once the Keychain has been consulted. Reading the Keychain can show
    /// a system permission dialog, so it must never happen while the main
    /// window is being created — otherwise the app looks like it never starts.
    loaded: std::sync::atomic::AtomicBool,
    pub login_running: Mutex<bool>,
}

impl AuthState {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
            loaded: std::sync::atomic::AtomicBool::new(false),
            login_running: Mutex::new(false),
        }
    }

    /// Load the stored session the first time it is actually needed.
    fn ensure_loaded(&self) {
        use std::sync::atomic::Ordering;
        if self.loaded.load(Ordering::Acquire) {
            return;
        }
        // Whoever gets here first does the (possibly prompting) read; the
        // others wait until it finishes.
        if self
            .loaded
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        if let Ok(Some(session)) = load_session() {
            if let Ok(mut guard) = self.session.lock() {
                *guard = Some(session);
            }
        }
        // Release so concurrent callers see the fully populated slot.
        self.loaded.store(true, Ordering::Release);
    }

    pub fn status(&self) -> AuthStatus {
        self.ensure_loaded();
        let guard = match self.session.lock() {
            Ok(g) => g,
            Err(_) => return AuthStatus::default(),
        };
        match guard.as_ref() {
            Some(s) => AuthStatus {
                signed_in: !s.is_expired(),
                username: s.username.clone(),
                expires_at: s.expiry(),
                expired: s.is_expired(),
                storage: Some(s.backend.clone()),
                storage_warning: if s.backend == "keychain" {
                    None
                } else {
                    keychain_error()
                        .or_else(|| Some("The session is not in the Keychain.".to_string()))
                },
                can_refresh: s.refresh_token.is_some(),
            },
            None => AuthStatus::default(),
        }
    }

    /// A usable token, refreshing once when it has expired.
    pub fn token(&self) -> Option<String> {
        self.ensure_loaded();
        let expired = {
            let guard = self.session.lock().ok()?;
            guard.as_ref().map(|s| s.is_expired()).unwrap_or(false)
        };
        if expired {
            let _ = self.refresh();
        }
        let guard = self.session.lock().ok()?;
        guard
            .as_ref()
            .filter(|s| !s.is_expired())
            .map(|s| s.access_token.clone())
    }

    pub fn set(&self, session: StoredSession) -> AppResult<()> {
        save_session(&session)?;
        self.loaded
            .store(true, std::sync::atomic::Ordering::Release);
        let mut guard = self
            .session
            .lock()
            .map_err(|_| AppError::internal("Session lock poisoned"))?;
        *guard = Some(session);
        Ok(())
    }

    pub fn clear(&self) -> AppResult<()> {
        delete_session();
        self.loaded
            .store(true, std::sync::atomic::Ordering::Release);
        let mut guard = self
            .session
            .lock()
            .map_err(|_| AppError::internal("Session lock poisoned"))?;
        *guard = None;
        Ok(())
    }

    /// Called once at startup, from a background thread, so a Keychain prompt
    /// never delays the window. Renews an expired session from the refresh
    /// token kept in the Keychain, so a signed-in user stays signed in.
    pub fn restore(&self) -> AuthStatus {
        self.ensure_loaded();
        let current = self.status();
        if !current.signed_in && current.can_refresh {
            let _ = self.refresh();
        }
        self.status()
    }

    /// Re-saves the current session, preferring the Keychain. Used by the
    /// "Store in Keychain" retry after a fallback write.
    pub fn promote_to_keychain(&self) -> AppResult<AuthStatus> {
        let current = {
            let guard = self
                .session
                .lock()
                .map_err(|_| AppError::internal("Session lock poisoned"))?;
            guard
                .clone()
                .ok_or_else(|| AppError::not_authenticated("Not signed in to Printables."))?
        };

        save_session(&current)?;
        let backend = load_session()?
            .map(|session| session.backend)
            .unwrap_or_default();

        let mut updated = current;
        updated.backend = backend;
        let mut guard = self
            .session
            .lock()
            .map_err(|_| AppError::internal("Session lock poisoned"))?;
        *guard = Some(updated);
        drop(guard);
        Ok(self.status())
    }

    /// Exchanges the refresh token for a new access token.
    pub fn refresh(&self) -> AppResult<AuthStatus> {
        let refresh_token = {
            let guard = self
                .session
                .lock()
                .map_err(|_| AppError::internal("Session lock poisoned"))?;
            guard
                .as_ref()
                .and_then(|s| s.refresh_token.clone())
                .ok_or_else(|| {
                    AppError::not_authenticated("Session expired. Sign in to Printables again.")
                })?
        };

        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| AppError::network(e.to_string()))?;

        let resp = client
            .post(REFRESH_URL)
            .header(
                reqwest::header::COOKIE,
                format!("auth.refresh_token={refresh_token}"),
            )
            .header("accept", "application/json")
            .send()
            .map_err(|e| AppError::network(format!("Token refresh failed: {e}")))?;

        if !resp.status().is_success() {
            let _ = self.clear();
            return Err(AppError::not_authenticated(
                "Session expired. Sign in to Printables again.",
            ));
        }

        // The endpoint may return JSON or refreshed Set-Cookie headers.
        let headers = resp.headers().clone();
        let body: serde_json::Value = resp.json().unwrap_or(serde_json::Value::Null);

        let access = body
            .pointer("/access_token")
            .or_else(|| body.pointer("/accessToken"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| cookie_from_headers(&headers, "auth.access_token"));

        let Some(access_token) = access else {
            let _ = self.clear();
            return Err(AppError::not_authenticated(
                "Session expired. Sign in to Printables again.",
            ));
        };

        let mut session = {
            let guard = self
                .session
                .lock()
                .map_err(|_| AppError::internal("Session lock poisoned"))?;
            guard.clone().unwrap_or(StoredSession {
                access_token: String::new(),
                refresh_token: None,
                expires_at: None,
                username: None,
                user_id: None,
                stored_at: String::new(),
                backend: String::new(),
            })
        };
        session.access_token = access_token;
        session.expires_at = token_exp(&session.access_token);
        if let Some(new_refresh) = cookie_from_headers(&headers, "auth.refresh_token") {
            session.refresh_token = Some(new_refresh);
        }
        self.set(session)?;
        Ok(self.status())
    }
}

impl Default for AuthState {
    fn default() -> Self {
        Self::new()
    }
}

/// Reads a token out of a `Set-Cookie` header without pulling in a cookie jar.
fn cookie_from_headers(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(reqwest::header::SET_COOKIE).iter() {
        let Ok(text) = value.to_str() else { continue };
        let first = text.split(';').next().unwrap_or("");
        if let Some((key, val)) = first.split_once('=') {
            if key.trim() == name && !val.is_empty() {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

/// Reads the `exp` claim from a JWT. Returns `None` for opaque tokens.
pub fn token_exp(token: &str) -> Option<i64> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    value.get("exp").and_then(|v| v.as_i64())
}

fn keychain_entry() -> AppResult<keyring::Entry> {
    keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| AppError::internal(format!("Keychain unavailable: {e}")))
}

pub fn load_session() -> AppResult<Option<StoredSession>> {
    let keychain_enabled = crate::config::load_prefs()
        .map(|p| p.store_in_keychain)
        .unwrap_or(false);

    // Default path: the 0600 file. Reading the Keychain can raise a permission
    // dialog whenever the binary's signature changed, which is every rebuild.
    if !keychain_enabled {
        let path = config::token_path()?;
        if !path.exists() {
            // One last chance to rescue a session that only ever lived in the
            // Keychain: read it once, copy it to the protected file, and never
            // touch the Keychain again. This is the only prompt the user should
            // ever see after this change.
            for service in [KEYCHAIN_SERVICE, KEYCHAIN_SERVICE_PREVIOUS] {
                if let Ok(entry) = keyring::Entry::new(service, KEYCHAIN_ACCOUNT) {
                    if let Ok(secret) = entry.get_password() {
                        if let Ok(mut session) = serde_json::from_str::<StoredSession>(&secret) {
                            session.backend = "file".to_string();
                            let _ = write_private_file(
                                &path,
                                &serde_json::to_string_pretty(&session).unwrap_or_default(),
                            );
                            let _ = entry.delete_credential();
                            return Ok(Some(session));
                        }
                    }
                }
            }
            return Ok(None);
        }
        let raw = fs::read_to_string(&path)?;
        let mut session: StoredSession = serde_json::from_str(&raw)?;
        session.backend = "file".to_string();
        return Ok(Some(session));
    }

    if let Ok(entry) = keychain_entry() {
        if let Ok(secret) = entry.get_password() {
            if let Ok(session) = serde_json::from_str::<StoredSession>(&secret) {
                return Ok(Some(session));
            }
        }
    }

    // A session saved before the rename lives under the previous service name;
    // adopt it and move it to the new item so the user stays signed in.
    if let Some(session) = load_previous_keychain_session() {
        let _ = save_session(&session);
        return Ok(Some(session));
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
    let payload = serde_json::to_string(&stored)?;

    let keychain_enabled = crate::config::load_prefs()
        .map(|p| p.store_in_keychain)
        .unwrap_or(false);

    if !keychain_enabled {
        stored.backend = "file".to_string();
        note_keychain_error(None);
        write_private_file(
            &config::token_path()?,
            &serde_json::to_string_pretty(&stored)?,
        )?;
        return Ok(());
    }

    match keychain_entry() {
        Ok(entry) => match entry.set_password(&payload) {
            Ok(()) => {
                stored.backend = "keychain".to_string();
                note_keychain_error(None);
                // A Keychain copy always wins: drop any older file copy.
                let _ = fs::remove_file(config::token_path()?);
                return Ok(());
            }
            Err(err) => {
                note_keychain_error(Some(format!("Keychain write failed: {err}")));
            }
        },
        Err(err) => {
            note_keychain_error(Some(err.message.clone()));
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

fn load_previous_keychain_session() -> Option<StoredSession> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE_PREVIOUS, KEYCHAIN_ACCOUNT).ok()?;
    let secret = entry.get_password().ok()?;
    let mut session: StoredSession = serde_json::from_str(&secret).ok()?;
    session.backend = "keychain".to_string();
    Some(session)
}

/// Remove the stored session (used by the MCP server, which has no state).
pub fn delete_stored_session() -> AppResult<()> {
    delete_session();
    Ok(())
}

fn delete_session() {
    if let Ok(entry) = keychain_entry() {
        let _ = entry.delete_credential();
    }
    if let Ok(entry) = keyring::Entry::new(KEYCHAIN_SERVICE_PREVIOUS, KEYCHAIN_ACCOUNT) {
        let _ = entry.delete_credential();
    }
    if let Ok(path) = config::token_path() {
        let _ = fs::remove_file(path);
    }
}

/// Persist freshly captured cookies and fill in the account identity.
pub fn adopt_tokens(
    access_token: String,
    refresh_token: Option<String>,
) -> AppResult<StoredSession> {
    let mut session = StoredSession {
        expires_at: token_exp(&access_token),
        access_token,
        refresh_token,
        username: None,
        user_id: None,
        stored_at: chrono::Utc::now().to_rfc3339(),
        backend: String::new(),
    };

    // Identify the account. Failure here must not lose a valid token.
    if let Ok(client) =
        crate::printables::api::PrintablesClient::with_token(Some(session.access_token.clone()))
    {
        if let Ok(me) = client.viewer() {
            session.username = me.public_username.or(me.handle);
            session.user_id = me.user_id;
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
            refresh_token: None,
            expires_at: Some(chrono::Utc::now().timestamp() - 10),
            username: None,
            user_id: None,
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
        assert!(!unknown.is_expired());
    }

    #[test]
    fn jwt_exp_is_read_from_the_payload() {
        use base64::Engine;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"exp":1893456000,"sub":"abc"}"#);
        let token = format!("header.{payload}.sig");
        assert_eq!(token_exp(&token), Some(1893456000));
        assert_eq!(token_exp("opaque-token"), None);
    }

    #[test]
    fn set_cookie_values_are_parsed() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.append(
            reqwest::header::SET_COOKIE,
            "auth.access_token=abc.def.ghi; Path=/; HttpOnly"
                .parse()
                .unwrap(),
        );
        headers.append(
            reqwest::header::SET_COOKIE,
            "auth.refresh_token=ref123; Path=/".parse().unwrap(),
        );
        assert_eq!(
            cookie_from_headers(&headers, "auth.access_token").as_deref(),
            Some("abc.def.ghi")
        );
        assert_eq!(
            cookie_from_headers(&headers, "auth.refresh_token").as_deref(),
            Some("ref123")
        );
        assert!(cookie_from_headers(&headers, "missing").is_none());
    }
}
