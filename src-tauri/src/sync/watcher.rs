//! Filesystem watcher that turns local edits into git commits.
//!
//! Debounced: an edit burst produces one commit. Commits are skipped while a
//! sync job is running, because the pull pipeline writes into the same folders
//! and commits its own result.

use crate::git;
use crate::jobs::{JobRegistry, JobState};
use crate::models::AppPrefs;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const DEBOUNCE: Duration = Duration::from_secs(5);
const SUPERVISOR_TICK: Duration = Duration::from_secs(2);

type Pending = Arc<Mutex<HashMap<PathBuf, Instant>>>;

pub fn spawn_supervisor(app: AppHandle, jobs: Arc<JobRegistry>) {
    std::thread::spawn(move || {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let mut active: Option<(PathBuf, RecommendedWatcher)> = None;

        loop {
            std::thread::sleep(SUPERVISOR_TICK);

            let library = current_library(&app);
            let needs_restart = match (&active, &library) {
                (None, None) => false,
                (Some((path, _)), Some(lib)) => path != lib,
                _ => true,
            };

            if needs_restart {
                active = None;
                if let Some(lib) = library.clone() {
                    match start_watcher(&app, &lib, pending.clone()) {
                        Ok(watcher) => active = Some((lib, watcher)),
                        Err(err) => {
                            eprintln!("[watcher] cannot watch {}: {}", lib.display(), err);
                        }
                    }
                }
            }

            process_pending(&app, &jobs, &pending);
        }
    });
}

fn current_library(app: &AppHandle) -> Option<PathBuf> {
    let prefs = app.state::<Mutex<AppPrefs>>();
    let prefs = prefs.lock().ok()?;
    let lib = prefs.library_folder.as_ref()?;
    Some(PathBuf::from(lib))
}

fn start_watcher(
    app: &AppHandle,
    library: &Path,
    pending: Pending,
) -> Result<RecommendedWatcher, String> {
    let root = library.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        if event.kind.is_access() {
            return;
        }
        for path in event.paths {
            let Some(model_dir) = model_dir_of(&root, &path) else {
                continue;
            };
            if let Ok(mut map) = pending.lock() {
                map.insert(model_dir, Instant::now());
            }
        }
    })
    .map_err(|e| e.to_string())?;

    watcher
        .watch(library, RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;

    let _ = app; // kept for symmetry with future per-model filtering
    Ok(watcher)
}

/// `<library>/<model>/…` -> `<library>/<model>`
fn model_dir_of(library: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(library).ok()?;
    let first = relative.components().next()?;
    let name = first.as_os_str().to_string_lossy();
    if name.starts_with('.') {
        return None;
    }
    let candidate = library.join(first.as_os_str());
    if candidate.join("metadata.json").exists() {
        Some(candidate)
    } else {
        None
    }
}

fn process_pending(app: &AppHandle, jobs: &Arc<JobRegistry>, pending: &Pending) {
    // Never interleave with a running sync: the pull commits its own result.
    if jobs
        .list()
        .iter()
        .any(|job| matches!(job.state, JobState::Queued | JobState::Running))
    {
        return;
    }

    let ready: Vec<PathBuf> = {
        let Ok(mut map) = pending.lock() else { return };
        let now = Instant::now();
        let ready: Vec<PathBuf> = map
            .iter()
            .filter(|(_, at)| now.duration_since(**at) >= DEBOUNCE)
            .map(|(path, _)| path.clone())
            .collect();
        for path in &ready {
            map.remove(path);
        }
        ready
    };

    if ready.is_empty() {
        return;
    }

    let snapshot = app
        .state::<Mutex<AppPrefs>>()
        .lock()
        .map(|prefs| prefs.clone())
        .ok();
    let (git_enabled, auto_commit) = snapshot
        .map(|prefs| (prefs.git_enabled, prefs.auto_commit_local_edits))
        .unwrap_or((false, false));

    for model_dir in ready {
        let mut committed = None;
        if git_enabled && auto_commit && git::is_repo(&model_dir) {
            if let Ok(result) = git::commit_all(&model_dir, "Local edits") {
                committed = result;
            }
        }
        let _ = app.emit(
            "library://model-changed",
            serde_json::json!({
                "model_dir": model_dir.display().to_string(),
                "committed": committed,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn model_dir_is_the_first_component_with_metadata() {
        let lib = std::env::temp_dir().join(format!("po-watch-{}", uuid::Uuid::new_v4()));
        let model = lib.join("42-demo");
        fs::create_dir_all(model.join("files")).unwrap();
        fs::write(model.join("metadata.json"), "{}").unwrap();

        assert_eq!(
            model_dir_of(&lib, &model.join("files/a.stl")),
            Some(model.clone())
        );
        assert_eq!(model_dir_of(&lib, &lib.join(".manager/x.json")), None);
        assert_eq!(model_dir_of(&lib, &lib.join("unknown/a.stl")), None);

        let _ = fs::remove_dir_all(&lib);
    }
}
