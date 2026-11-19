//! The in-app login window.
//!
//! Opens a normal webview at printables.com/login so the user signs in exactly
//! as they would in a browser (including Google/Apple SSO and 2FA). Once the
//! page is back on printables.com the Auth0 SPA has cached its access token in
//! `localStorage`; we read it and close the window.
//!
//! The token never travels through a URL, a log line, or an error message.

use super::{adopt_token, AuthState};
use crate::error::{AppError, AppResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const LOGIN_WINDOW_LABEL: &str = "printables-login";
const LOGIN_URL: &str = "https://www.printables.com/login";
const TITLE_PREFIX: &str = "POAUTH:";
const POLL_INTERVAL_MS: u64 = 500;
const MAX_WAIT_MS: u64 = 10 * 60 * 1000;

/// Runs inside the login webview. Walks `localStorage` for the Auth0 cache and
/// publishes the token by overwriting the document title, which the Rust side
/// can read without injecting a callback bridge into a remote origin.
const EXTRACT_JS: &str = r#"
(function () {
  try {
    function find(o, depth) {
      if (!o || typeof o !== 'object' || depth > 4) return null;
      if (typeof o.access_token === 'string' && o.access_token.length > 20) {
        return { t: o.access_token, e: o.expires_in || 0 };
      }
      for (var k in o) {
        if (Object.prototype.hasOwnProperty.call(o, k)) {
          var r = find(o[k], depth + 1);
          if (r) return r;
        }
      }
      return null;
    }
    var hit = null;
    for (var i = 0; i < localStorage.length && !hit; i++) {
      var key = localStorage.key(i) || '';
      if (key.toLowerCase().indexOf('auth') === -1) continue;
      try { hit = find(JSON.parse(localStorage.getItem(key)), 0); } catch (e) {}
    }
    if (hit) { document.title = 'POAUTH:' + btoa(JSON.stringify(hit)); }
  } catch (e) {}
})();
"#;

/// Opens the login window and waits for a token in the background.
pub fn start_login(app: &AppHandle) -> AppResult<()> {
    let auth = app.state::<AuthState>();
    {
        let mut running = auth
            .login_running
            .lock()
            .map_err(|_| AppError::internal("Login state lock poisoned"))?;
        if *running {
            return Err(AppError::invalid("A sign-in window is already open."));
        }
        *running = true;
    }

    if let Some(existing) = app.get_webview_window(LOGIN_WINDOW_LABEL) {
        let _ = existing.close();
    }

    let url = LOGIN_URL
        .parse::<tauri::Url>()
        .map_err(|e| AppError::internal(format!("Invalid login URL: {e}")))?;

    let window = WebviewWindowBuilder::new(app, LOGIN_WINDOW_LABEL, WebviewUrl::External(url))
        .title("Sign in to Printables")
        .inner_size(520.0, 780.0)
        .min_inner_size(420.0, 600.0)
        .build()
        .map_err(|e| AppError::internal(format!("Cannot open sign-in window: {e}")))?;

    let app_handle = app.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_thread = cancelled.clone();

    std::thread::spawn(move || {
        let auth = app_handle.state::<AuthState>();
        let deadline = std::time::Instant::now() + Duration::from_millis(MAX_WAIT_MS);

        loop {
            if cancelled_thread.load(Ordering::Relaxed) {
                break;
            }
            if app_handle.get_webview_window(LOGIN_WINDOW_LABEL).is_none() {
                break;
            }
            if std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));

            let _ = window.eval(EXTRACT_JS);

            let Ok(title) = window.title() else { continue };
            let Some(encoded) = title.strip_prefix(TITLE_PREFIX) else {
                continue;
            };

            match decode_payload(encoded) {
                Ok((token, expires_in)) => {
                    match adopt_token(token, expires_in) {
                        Ok(session) => {
                            // Keep the in-memory state in step with disk so the
                            // very next command already sees the session.
                            let _ = auth.set(session.clone());
                            let _ = window.close();
                            let _ = app_handle.emit(
                                "auth://changed",
                                serde_json::json!({
                                    "signed_in": true,
                                    "username": session.username,
                                }),
                            );
                        }
                        Err(err) => {
                            let _ = app_handle.emit(
                                "auth://error",
                                serde_json::json!({ "message": err.message }),
                            );
                        }
                    }
                    break;
                }
                Err(err) => {
                    let _ = app_handle.emit(
                        "auth://error",
                        serde_json::json!({ "message": err.message }),
                    );
                    break;
                }
            }
        }

        if let Ok(mut running) = auth.login_running.lock() {
            *running = false;
        }
        let _ = window.close();
    });

    Ok(())
}

fn decode_payload(encoded: &str) -> AppResult<(String, Option<i64>)> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .map_err(|e| AppError::internal(format!("Malformed sign-in payload: {e}")))?;
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|e| AppError::internal(format!("Malformed sign-in payload: {e}")))?;

    let token = value
        .get("t")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::internal("Sign-in payload had no token"))?
        .to_string();
    let expires_in = value
        .get("e")
        .and_then(|v| v.as_i64())
        .filter(|secs| *secs > 0);

    Ok((token, expires_in))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_captured_payload() {
        use base64::Engine;
        let json = r#"{"t":"abc.def.ghi","e":86400}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(json);
        let (token, exp) = decode_payload(&encoded).unwrap();
        assert_eq!(token, "abc.def.ghi");
        assert_eq!(exp, Some(86400));
    }

    #[test]
    fn rejects_garbage_payloads() {
        assert!(decode_payload("not-base64!!").is_err());
    }
}
