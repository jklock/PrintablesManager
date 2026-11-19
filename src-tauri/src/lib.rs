//! Printables Manager 2.0 — macOS-only, account-aware, git-backed library.

mod commands;
mod config;
mod error;
mod git;
mod jobs;
mod library;
mod models;
mod printables;
mod sync;

// Diagnostics (`examples/keychain_check.rs`) reach into the real session code.
pub mod auth;
pub mod backup;
pub mod mcp;

/// `Printables Manager --selftest`: prints what the app can actually do on this
/// machine. Used for support and for verifying a freshly installed bundle.
pub fn selftest() {
    println!(
        "Printables Manager {} — selftest",
        env!("CARGO_PKG_VERSION")
    );

    let report = auth::keychain_selftest();
    println!(
        "keychain     : available={} writable={} readable={}{}",
        report.available,
        report.writable,
        report.readable,
        match &report.error {
            Some(err) => format!("  ({err})"),
            None => String::new(),
        }
    );

    match config::prefs_path() {
        Ok(path) => println!("preferences  : {}", path.display()),
        Err(err) => println!("preferences  : unavailable ({})", err.message),
    }

    let prefs = config::load_prefs().unwrap_or_default();
    println!(
        "library      : {}",
        prefs
            .library_folder
            .clone()
            .unwrap_or_else(|| "(not set)".to_string())
    );
    println!("git enabled  : {}", prefs.git_enabled);

    // Reading the session can raise a Keychain permission dialog, so it is opt-in:
    //   printables-manager --selftest --with-session
    if std::env::args().any(|arg| arg == "--with-session") {
        let state = auth::AuthState::new();
        let status = state.status();
        println!(
            "session      : signed_in={} user={} storage={} refreshable={}",
            status.signed_in,
            status.username.clone().unwrap_or_else(|| "-".to_string()),
            status.storage.clone().unwrap_or_else(|| "-".to_string()),
            status.can_refresh
        );
    } else {
        println!("session      : not read (pass --with-session to include it)");
    }

    let slicer = commands::slicer::inspect(&prefs);
    println!(
        "slicer       : {} -> {} (exists={})",
        slicer.label,
        slicer
            .app_bundle
            .clone()
            .or(slicer.executable.clone())
            .unwrap_or_else(|| "(not configured)".to_string()),
        slicer.exists
    );

    if report.healthy() {
        println!("\nOK: this build may store the session in the login Keychain.");
    } else {
        println!("\nWARNING: the Keychain is not usable; sessions fall back to a 0600 file.");
    }
}

use std::sync::{Arc, Mutex};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let job_registry = Arc::new(jobs::JobRegistry::new());
    let jobs_for_setup = job_registry.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(Mutex::new(models::AppPrefs::default()))
        .manage(auth::AuthState::new())
        .manage(job_registry)
        .invoke_handler(tauri::generate_handler![
            // library
            commands::clone::clone_model,
            commands::delete::delete_model,
            commands::library::list_models,
            commands::library::library_stats,
            commands::library::rescan_library,
            commands::collections::list_collections,
            commands::collections::create_collection,
            commands::collections::rename_collection,
            commands::collections::delete_collection,
            commands::collections::add_model_to_collection,
            commands::collections::remove_model_from_collection,
            commands::collections::import_site_collections,
            // prefs
            commands::prefs::get_prefs,
            commands::prefs::set_prefs,
            commands::prefs::open_config_folder,
            commands::prefs::ui_ready,
            backup::backup_to_icloud,
            // shell / integrations
            commands::dialogs::dialog_open,
            commands::slicer::open_in_slicer,
            commands::slicer::slice_file,
            commands::slicer::slicer_info,
            commands::slicer::slicer_test,
            commands::maintenance::rebuild_thumbs,
            commands::export::export_files,
            commands::shell::open_folder,
            commands::shell::open_external,
            // editing
            commands::edit::get_model_edit,
            commands::edit::update_model_details,
            commands::edit::import_model_files,
            commands::edit::delete_model_file,
            commands::edit::set_model_cover,
            commands::edit::create_local_model,
            // account
            commands::auth::auth_status,
            commands::auth::auth_login,
            commands::auth::auth_logout,
            commands::auth::auth_verify,
            commands::auth::auth_refresh,
            commands::auth::auth_set_token,
            commands::auth::auth_capabilities,
            commands::auth::auth_keychain_test,
            commands::auth::auth_storage_retry,
            commands::auth::auth_refresh,
            commands::auth::auth_set_token,
            commands::auth::auth_capabilities,
            // sync
            commands::sync::account_models,
            commands::sync::sync_all,
            commands::sync::sync_model,
            commands::sync::sync_cancel,
            commands::sync::jobs_list,
            commands::sync::jobs_clear,
            // git
            commands::git::git_status,
            commands::git::git_log,
            commands::git::git_show,
            commands::git::git_commit,
            commands::git::git_restore,
            commands::git::git_compact,
            commands::git::git_init,
            commands::git::git_adopt_library,
            // push
            commands::push::push_preview,
            commands::push::push_apply,
            commands::push::push_log,
            commands::publish::publish_model,
            commands::publish::delete_remote_model,
        ])
        .setup(move |app| {
            // 1.x -> 2.0 preferences migration, then hydrate managed state.
            let _ = config::migrate_legacy_prefs();
            let prefs = config::load_prefs().unwrap_or_default();

            {
                let managed = app.state::<Mutex<models::AppPrefs>>();
                match managed.lock() {
                    Ok(mut guard) => *guard = prefs.clone(),
                    Err(_) => eprintln!("[setup] preferences lock poisoned"),
                };
            }

            // Never touch the library path during setup. `allow_directory`
            // resolves the path internally, and when the library sits on an
            // external volume macOS blocks that first touch behind a "access
            // files on a removable volume" prompt - which froze setup() and left
            // the window blank, with the dialog hidden behind it. Widen the
            // asset scope on a worker thread so the UI always paints.
            // The asset scope is widened by `ui_ready`, which the frontend calls
            // after its first frame. Doing it here would touch an external
            // library volume before anything is painted, and the resulting
            // consent sheet is app-modal - the window would stay blank.

            // Bring pre-2.0 model folders under version control, off the UI thread.
            if prefs.git_enabled {
                if let Some(lib_path) = prefs.library_folder.clone() {
                    let app_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(3000));
                        match printables::clone::adopt_existing_models(std::path::Path::new(
                            &lib_path,
                        )) {
                            Ok(0) => {}
                            Ok(n) => {
                                use tauri::Emitter;
                                let _ = app_handle
                                    .emit("library://adopted", serde_json::json!({ "models": n }));
                            }
                            Err(err) => eprintln!("[setup] adoption failed: {}", err.message),
                        }
                    });
                }
            }

            // Watch the library so local edits become commits.
            sync::watcher::spawn_supervisor(app.handle().clone(), jobs_for_setup);

            // Stay signed in: renew an expired session from the Keychain-held
            // refresh token, off the UI thread.
            {
                let app_handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(2000));
                    let auth = app_handle.state::<auth::AuthState>();
                    let status = auth.restore();
                    if status.signed_in {
                        use tauri::Emitter;
                        let _ = app_handle.emit(
                            "auth://changed",
                            serde_json::json!({
                                "signed_in": true,
                                "username": status.username,
                            }),
                        );
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
