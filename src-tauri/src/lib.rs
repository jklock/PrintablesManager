//! Printables Offline 2.0 — macOS-only, account-aware, git-backed library.

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
            // prefs
            commands::prefs::get_prefs,
            commands::prefs::set_prefs,
            commands::prefs::open_config_folder,
            // shell / integrations
            commands::dialogs::dialog_open,
            commands::slicer::open_in_slicer,
            commands::slicer::slice_file,
            commands::maintenance::rebuild_thumbs,
            commands::export::export_files,
            commands::shell::open_folder,
            commands::shell::open_external,
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

            if let Some(lib_path) = &prefs.library_folder {
                if let Ok(path) = std::path::PathBuf::from(lib_path).canonicalize() {
                    let _ = app.asset_protocol_scope().allow_directory(&path, true);
                }
            }

            // Bring pre-2.0 model folders under version control, off the UI thread.
            if prefs.git_enabled {
                if let Some(lib_path) = prefs.library_folder.clone() {
                    let app_handle = app.handle().clone();
                    std::thread::spawn(move || {
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
