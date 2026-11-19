use crate::models::AppPrefs;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tauri::State;

/// Human name for a slicer key, kept next to the launch code so the UI and the
/// backend can never disagree about what is configured.
pub fn slicer_label(key: &str) -> &'static str {
    match key {
        "prusa" => "PrusaSlicer",
        "orca" => "OrcaSlicer",
        "bambu" => "Bambu Studio",
        "cura" => "UltiMaker Cura",
        "qidi" => "QIDISlicer",
        _ => "Slicer",
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlicerInfo {
    pub key: String,
    pub label: String,
    pub executable: Option<String>,
    pub app_bundle: Option<String>,
    /// Whether the configured path actually exists on disk.
    pub exists: bool,
}

/// The app name is taken from the configured bundle when there is one, because
/// a stale preference can pair `slicer_key: "orca"` with a QIDISlicer path and
/// the UI must show what will actually launch.
fn app_label(bundle: Option<&str>, key: &str) -> String {
    bundle
        .and_then(|path| {
            Path::new(path)
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| slicer_label(key).to_string())
}

pub fn inspect(prefs: &AppPrefs) -> SlicerInfo {
    let exe = prefs.slicer_executable.clone();
    let bundle = exe.as_deref().and_then(resolve_app_bundle);
    let exists = match (&bundle, &exe) {
        (Some(app), _) => Path::new(app).exists(),
        (None, Some(path)) => Path::new(path).exists(),
        _ => false,
    };

    SlicerInfo {
        key: prefs.slicer_key.clone(),
        label: app_label(bundle.as_deref(), &prefs.slicer_key),
        executable: exe,
        app_bundle: bundle,
        exists,
    }
}

/// What the app will launch, and whether that thing is installed.
#[tauri::command]
pub fn slicer_info(prefs: State<'_, Mutex<AppPrefs>>) -> Result<SlicerInfo, String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    Ok(inspect(&prefs))
}

/// Launches the slicer with no files, to prove the configured path works.
#[tauri::command]
pub fn slicer_test(prefs: State<'_, Mutex<AppPrefs>>) -> Result<SlicerInfo, String> {
    let snapshot = prefs.lock().map_err(|e| e.to_string())?.clone();
    let info = inspect(&snapshot);

    let exe = info
        .executable
        .clone()
        .ok_or("No slicer configured. Choose one in Preferences.")?;
    if !info.exists {
        return Err(format!("{} is not installed at {exe}", info.label));
    }

    match &info.app_bundle {
        Some(app) => {
            Command::new("open")
                .arg("-a")
                .arg(app)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("Failed to launch {}: {e}", info.label))?;
        }
        None => {
            Command::new(&exe)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|e| format!("Failed to launch {}: {e}", info.label))?;
        }
    }

    Ok(info)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenInSlicerArgs {
    // Accepted for API symmetry with the UI; launching only needs `files`.
    #[allow(dead_code)]
    pub model_id: String,
    #[allow(dead_code)]
    pub slicer: String,
    pub files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SliceFileArgs {
    pub file: String,
}

/// On macOS, resolve an `*.app` bundle whether the user pointed the preference
/// at the bundle itself or at the binary inside it.
fn resolve_app_bundle(exe: &str) -> Option<String> {
    let path = Path::new(exe);
    if path.extension().map(|e| e == "app").unwrap_or(false) {
        return Some(exe.to_string());
    }

    let mut current = path;
    while let Some(parent) = current.parent() {
        if parent.extension().map(|e| e == "app").unwrap_or(false) {
            return Some(parent.display().to_string());
        }
        current = parent;
    }
    None
}

/// Launch the configured slicer with the given model files as arguments.
#[tauri::command]
pub fn open_in_slicer(
    args: OpenInSlicerArgs,
    prefs: State<'_, Mutex<AppPrefs>>,
) -> Result<(), String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    let exe = prefs
        .slicer_executable
        .as_ref()
        .ok_or("Slicer executable not configured. Set it in Preferences.")?;

    // Fail with a precise message instead of a bare spawn error.
    let info = inspect(&prefs);
    if !info.exists {
        return Err(format!(
            "{} is not installed at {exe}. Choose the app in Preferences.",
            info.label
        ));
    }

    // Prefer `open -a <app>` so an .app bundle launches the normal way.
    if let Some(app_path) = resolve_app_bundle(exe) {
        let mut cmd = Command::new("open");
        cmd.arg("-a").arg(&app_path);
        for f in &args.files {
            cmd.arg(f);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        cmd.spawn()
            .map_err(|e| format!("Failed to launch slicer: {e}"))?;
        return Ok(());
    }

    let mut cmd = Command::new(exe);
    cmd.args(&args.files)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()
        .map_err(|e| format!("Failed to launch slicer: {e}"))?;
    Ok(())
}

/// Launch the slicer with a single file (quick-slice from card).
#[tauri::command]
pub fn slice_file(args: SliceFileArgs, prefs: State<'_, Mutex<AppPrefs>>) -> Result<(), String> {
    let prefs = prefs.lock().map_err(|e| e.to_string())?;
    let exe = prefs
        .slicer_executable
        .as_ref()
        .ok_or("Slicer executable not configured.")?;

    let info = inspect(&prefs);
    if !info.exists {
        return Err(format!(
            "{} is not installed at {exe}. Choose the app in Preferences.",
            info.label
        ));
    }

    // Prefer `open -a <app>` so an .app bundle launches the normal way.
    if let Some(app_path) = resolve_app_bundle(exe) {
        let mut cmd = Command::new("open");
        cmd.arg("-a").arg(&app_path).arg(&args.file);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        cmd.spawn()
            .map_err(|e| format!("Failed to launch slicer: {e}"))?;
        return Ok(());
    }

    let mut cmd = Command::new(exe);
    cmd.arg(&args.file)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn()
        .map_err(|e| format!("Failed to launch slicer: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_bundle_is_found_from_an_inner_binary_path() {
        assert_eq!(
            resolve_app_bundle("/Applications/QIDISlicer.app/Contents/MacOS/QIDISlicer").as_deref(),
            Some("/Applications/QIDISlicer.app")
        );
        assert_eq!(resolve_app_bundle("/usr/local/bin/orca-slicer"), None);
    }

    #[test]
    fn an_app_path_is_its_own_bundle() {
        // Preferences store whatever the folder picker returned: usually the
        // bundle itself. Missing this used to make launching try to execute a
        // directory.
        assert_eq!(
            resolve_app_bundle("/Applications/QIDISlicer.app").as_deref(),
            Some("/Applications/QIDISlicer.app")
        );
    }

    #[test]
    fn label_prefers_the_installed_app_over_a_stale_key() {
        assert_eq!(
            app_label(Some("/Applications/QIDISlicer.app"), "orca"),
            "QIDISlicer"
        );
        assert_eq!(app_label(None, "orca"), "OrcaSlicer");
        assert_eq!(app_label(None, "qidi"), "QIDISlicer");
    }
}
