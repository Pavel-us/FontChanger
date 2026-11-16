#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod fonts;

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::{AppHandle, Emitter, Manager};

/// Changes fonts and sizes on a worker thread; per-file results are streamed as events.
#[tauri::command]
async fn run_change(app: AppHandle, req: engine::Request) -> Result<engine::Stats, String> {
    let handle = tauri::async_runtime::spawn_blocking(move || {
        engine::run(&req, |ev| {
            let _ = app.emit("file-result", ev);
        })
    });
    let result = handle.await.map_err(|e| e.to_string())?;
    result
}

/// After a real run: lists what the rules would still change (text the engine cannot reach).
#[tauri::command]
async fn verify_run(req: engine::Request) -> Result<Vec<engine::Residual>, String> {
    let handle = tauri::async_runtime::spawn_blocking(move || engine::verify(&req));
    let result = handle.await.map_err(|e| e.to_string())?;
    result
}

/// Fonts the selected files use that this machine does not have.
#[tauri::command]
async fn missing_fonts(req: engine::Request) -> Result<Vec<engine::MissingFont>, String> {
    let handle = tauri::async_runtime::spawn_blocking(move || engine::missing_fonts(&req, || false));
    handle.await.map_err(|e| e.to_string())?
}

/// Reads one document and returns the fonts and sizes it uses ("from a sample").
#[tauri::command]
async fn sample_file(path: String) -> Result<engine::Sample, String> {
    let handle = tauri::async_runtime::spawn_blocking(move || engine::sample(&path));
    handle.await.map_err(|e| e.to_string())?
}

/// Writes the results table as CSV; returns the path that was written.
#[tauri::command]
fn save_report(path: String, rows: Vec<engine::ReportRow>) -> Result<String, String> {
    engine::write_report(&path, &rows)
}

/// Reads a .fctpl template (or an old tab-separated rules file).
#[tauri::command]
fn load_template(path: String) -> Result<engine::Template, String> {
    engine::load_template(&path)
}

/// Writes a .fctpl template; returns the path that was written.
#[tauri::command]
fn save_template(path: String, template: engine::Template) -> Result<String, String> {
    engine::save_template(&path, template)
}

/// Incremented by every scan and by `cancel_scan`; a running scan stops when its own number is no longer current.
static SCAN_GEN: AtomicU64 = AtomicU64::new(0);

/// Lists the fonts and sizes found in the selected files. A newer scan cancels an older one ("cancelled" error).
#[tauri::command]
async fn scan_fonts(req: engine::Request) -> Result<engine::ScanResult, String> {
    let id = SCAN_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let handle = tauri::async_runtime::spawn_blocking(move || {
        engine::scan(&req, || SCAN_GEN.load(Ordering::SeqCst) != id)
    });
    let result = handle.await.map_err(|e| e.to_string())?;
    result
}

#[tauri::command]
fn cancel_scan() {
    SCAN_GEN.fetch_add(1, Ordering::SeqCst);
}

#[derive(Serialize)]
struct PathInfo {
    dir: String,
    file: Option<String>,
}

/// For drag-and-drop: folder -> (folder, -), file -> (its folder, file name).
#[tauri::command]
fn inspect_path(path: String) -> PathInfo {
    let p = Path::new(&path);
    if p.is_dir() {
        return PathInfo { dir: path, file: None };
    }
    PathInfo {
        dir: p
            .parent()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default(),
        file: p.file_name().map(|f| f.to_string_lossy().to_string()),
    }
}

/// <config dir>/dev.fontchanger.app/locales — where users put their own translations.
fn locales_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|d| d.join("locales"))
        .map_err(|e| e.to_string())
}

/// Contents of user translations (*.json; files starting with "_" are ignored).
#[tauri::command]
fn user_locales(app: AppHandle) -> Vec<String> {
    let Ok(dir) = locales_dir(&app) else { return vec![] };
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let p = entry.path();
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with('_') || p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&p) {
            out.push(text);
        }
    }
    out
}

/// In-app updating works on Windows, macOS and AppImage only:
/// .deb / .rpm installations are managed by the system package manager.
#[tauri::command]
fn can_self_update() -> bool {
    if cfg!(target_os = "linux") {
        std::env::var_os("APPIMAGE").is_some()
    } else {
        true
    }
}

const LOCALE_TEMPLATE: &str = include_str!("../../src/locales/en.json");

/// Creates the translations folder and `_template.json`; returns the template path.
#[tauri::command]
fn prepare_locales_dir(app: AppHandle) -> Result<String, String> {
    let dir = locales_dir(&app)?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let template = dir.join("_template.json");
    if !template.exists() {
        std::fs::write(&template, LOCALE_TEMPLATE).map_err(|e| e.to_string())?;
    }
    Ok(template.to_string_lossy().to_string())
}

fn main() {
    // no arguments = the normal window; with arguments it is a headless run for scripts
    // (only known arguments start it: whatever else a launcher passes must not keep the window from opening)
    let args: Vec<String> = std::env::args().skip(1).collect();
    if engine::wants_cli(&args) {
        match engine::run_cli(&args) {
            Ok(Some(code)) => std::process::exit(code),
            Ok(None) => {}
            Err(e) => {
                eprintln!("{}", engine::cli_message(&e));
                std::process::exit(2);
            }
        }
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            run_change,
            verify_run,
            sample_file,
            save_report,
            missing_fonts,
            load_template,
            save_template,
            scan_fonts,
            cancel_scan,
            inspect_path,
            user_locales,
            prepare_locales_dir,
            can_self_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running the Tauri application");
}
