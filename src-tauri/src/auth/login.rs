//! The in-app sign-in window.
//!
//! Opens printables.com/login in a normal webview so the user can sign in with
//! Prusa Account (including Google/Apple SSO and 2FA). When the site sets its
//! `auth.access_token` cookie we adopt it, close the window, and report the
//! account name back to the UI.
//!
//! There is no redirect interception and no token parsing from URLs: the site
//! keeps the session in cookies, so we read the cookies.

use super::{adopt_tokens, AuthState};
use crate::error::{AppError, AppResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

pub const LOGIN_WINDOW_LABEL: &str = "printables-login";
const LOGIN_URL: &str = "https://www.printables.com/login";
const POLL_INTERVAL_MS: u64 = 700;
const MAX_WAIT_MS: u64 = 10 * 60 * 1000;

/// Reads the session cookies through the browser API as well as through
/// `document.cookie`, so non-HttpOnly values still work if the API differs.
const READ_COOKIES_JS: &str = r#"
(function () {
  try {
    var out = {};
    String(document.cookie || '').split(';').forEach(function (part) {
      var i = part.indexOf('=');
      if (i > 0) { out[part.slice(0, i).trim()] = part.slice(i + 1).trim(); }
    });
    if (out['auth.access_token']) {
      document.title = 'POAUTH:' + JSON.stringify({
        a: out['auth.access_token'],
        r: out['auth.refresh_token'] || null
      });
    }
  } catch (e) {}
})();
"#;

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
        .inner_size(520.0, 800.0)
        .min_inner_size(420.0, 620.0)
        .build()
        .map_err(|e| AppError::internal(format!("Cannot open sign-in window: {e}")))?;

    let app_handle = app.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_thread = cancelled.clone();

    std::thread::spawn(move || {
        let auth = app_handle.state::<AuthState>();
        let deadline = std::time::Instant::now() + Duration::from_millis(MAX_WAIT_MS);
        let mut announced = false;

        loop {
            if cancelled_thread.load(Ordering::Relaxed) {
                break;
            }
            if app_handle.get_webview_window(LOGIN_WINDOW_LABEL).is_none() {
                break;
            }
            if std::time::Instant::now() > deadline {
                let _ = app_handle.emit(
                    "auth://error",
                    serde_json::json!({ "message": "Sign-in window timed out. Try again." }),
                );
                break;
            }

            std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
            let _ = window.eval(READ_COOKIES_JS);

            let (access, refresh) = read_session_cookies(&window);

            if let Some(access_token) = access {
                let _ = app_handle.emit(
                    "auth://progress",
                    serde_json::json!({ "message": "Confirmed. Reading your account…" }),
                );
                match adopt_tokens(access_token, refresh) {
                    Ok(session) => {
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

            if !announced {
                announced = true;
                let _ = app_handle.emit(
                    "auth://progress",
                    serde_json::json!({ "message": "Waiting for you to finish signing in…" }),
                );
            }
        }

        if let Ok(mut running) = auth.login_running.lock() {
            *running = false;
        }
        let _ = window.close();
        let _ = app_handle.emit("auth://closed", serde_json::json!({}));
    });

    Ok(())
}

/// Collects `auth.access_token` / `auth.refresh_token` from the webview cookie
/// jar, falling back to the value the page reported through its title.
fn read_session_cookies(window: &tauri::WebviewWindow) -> (Option<String>, Option<String>) {
    let mut access = None;
    let mut refresh = None;

    if let Ok(cookies) = window.cookies() {
        for cookie in cookies {
            match cookie.name() {
                "auth.access_token" if !cookie.value().is_empty() => {
                    access = Some(cookie.value().to_string());
                }
                "auth.refresh_token" if !cookie.value().is_empty() => {
                    refresh = Some(cookie.value().to_string());
                }
                _ => {}
            }
        }
    }

    if access.is_none() {
        if let Ok(title) = window.title() {
            if let Some(payload) = title.strip_prefix("POAUTH:") {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
                    access = value
                        .get("a")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    refresh = value.get("r").and_then(|v| v.as_str()).map(String::from);
                }
            }
        }
    }

    (access, refresh)
}

/// Escape hatch for when the cookie jar is unreadable: the user can paste the
/// value of `auth.access_token` from the browser's devtools.
pub fn adopt_pasted_token(
    access_token: String,
    refresh_token: Option<String>,
) -> AppResult<super::StoredSession> {
    let token = access_token.trim().to_string();
    if token.is_empty() {
        return Err(AppError::invalid("That token is empty."));
    }
    adopt_tokens(
        token,
        refresh_token
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
    )
}
