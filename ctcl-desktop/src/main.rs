// CTCL Temporal Port - Phase 1 desktop shell + Phase 2 Local Gateway. Same
// ctcl-core/ctcl-store the CLI uses. The webview talks to this process via
// Tauri's IPC; OTHER apps/agents talk to it via the local_api module's
// loopback HTTP server (disabled by default, per the whitepaper's §7.2).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod local_api;

use ctcl_core::{from_ns, now_view, to_ns};
use ctcl_store::{AuditEntry, Settings, Store, ALL_SCOPES};
use serde::Serialize;
use std::sync::{Arc, Mutex};

struct AppState {
    store: Arc<Mutex<Store>>,
    local_api: Mutex<Option<local_api::LocalApiHandle>>,
}

#[derive(Serialize)]
struct ConvertResult {
    canonical_unix_ns: String,
    output: String,
}

#[tauri::command]
fn now() -> Result<serde_json::Value, String> {
    now_view().map(|v| serde_json::to_value(v).unwrap()).map_err(|e| e.to_string())
}

#[tauri::command]
fn convert(value: String, from: String, to: String, tz: Option<String>) -> Result<ConvertResult, String> {
    let ns = to_ns(&value, &from).map_err(|e| e.to_string())?;
    let output = from_ns(ns, &to, tz.as_deref()).map_err(|e| e.to_string())?;
    Ok(ConvertResult { canonical_unix_ns: ns.to_string(), output })
}

#[tauri::command]
fn list_systems(state: tauri::State<AppState>) -> Result<Vec<String>, String> {
    state.store.lock().unwrap().list_systems().map_err(|e| e.to_string())
}

#[tauri::command]
fn list_groups(state: tauri::State<AppState>) -> Result<Vec<String>, String> {
    state.store.lock().unwrap().list_groups().map_err(|e| e.to_string())
}

#[tauri::command]
fn expand_group(state: tauri::State<AppState>, id: String) -> Result<serde_json::Value, String> {
    let ns = ctcl_core::now_ns();
    state.store.lock().unwrap().expand_group(&id, ns).map_err(|e| e.to_string())
}

// ---- Settings / Local Gateway (Phase 2) ------------------------------------

#[derive(Serialize)]
struct SettingsView {
    #[serde(flatten)]
    settings: Settings,
    all_scopes: &'static [&'static str],
    feature_status: Vec<ctcl_store::settings::FeatureStatus>,
    local_api_running: bool,
}

fn settings_view(state: &tauri::State<AppState>, settings: Settings) -> SettingsView {
    let running = state.local_api.lock().unwrap().is_some();
    SettingsView { settings, all_scopes: ALL_SCOPES, feature_status: Settings::status(), local_api_running: running }
}

#[tauri::command]
fn get_settings(state: tauri::State<AppState>) -> Result<SettingsView, String> {
    let settings = state.store.lock().unwrap().get_settings().map_err(|e| e.to_string())?;
    Ok(settings_view(&state, settings))
}

/// Apply a full settings update. If the local API's enabled flag or port
/// changed, the running server is stopped/restarted to match - no stale
/// server left listening on an old port, no silently-ignored toggle.
#[tauri::command]
fn update_settings(state: tauri::State<AppState>, settings: Settings) -> Result<SettingsView, String> {
    state.store.lock().unwrap().save_settings(&settings).map_err(|e| e.to_string())?;
    sync_local_api(&state, &settings);
    Ok(settings_view(&state, settings))
}

#[tauri::command]
fn regenerate_api_token(state: tauri::State<AppState>) -> Result<SettingsView, String> {
    let settings = state.store.lock().unwrap().regenerate_api_token().map_err(|e| e.to_string())?;
    sync_local_api(&state, &settings);
    Ok(settings_view(&state, settings))
}

#[tauri::command]
fn list_audit_log(state: tauri::State<AppState>) -> Result<Vec<AuditEntry>, String> {
    state.store.lock().unwrap().list_audit_log(50).map_err(|e| e.to_string())
}

/// Ensure the running local API (if any) matches current settings - start it
/// if now enabled, stop it if now disabled, restart it if the port changed.
fn sync_local_api(state: &tauri::State<AppState>, settings: &Settings) {
    let mut slot = state.local_api.lock().unwrap();
    *slot = None; // dropping the old handle (if any) stops its thread and closes the socket
    if settings.local_api_enabled {
        match local_api::start(state.store.clone(), settings.local_api_port) {
            Ok(handle) => *slot = Some(handle),
            Err(e) => eprintln!("failed to start local API on port {}: {e}", settings.local_api_port),
        }
    }
}

fn main() {
    let db_path = "ctcl-desktop-data.sqlite3";
    let store = Store::open(db_path).unwrap_or_else(|e| {
        eprintln!("failed to open local store at {db_path}: {e}");
        std::process::exit(1);
    });
    let store = Arc::new(Mutex::new(store));

    // Start the local API immediately if a previous session left it enabled -
    // "enabled" is a persisted preference, not a per-run default.
    let initial_settings = store.lock().unwrap().get_settings().ok();
    let local_api = initial_settings
        .filter(|s| s.local_api_enabled)
        .and_then(|s| local_api::start(store.clone(), s.local_api_port).ok());

    tauri::Builder::default()
        .manage(AppState { store, local_api: Mutex::new(local_api) })
        .invoke_handler(tauri::generate_handler![
            now,
            convert,
            list_systems,
            list_groups,
            expand_group,
            get_settings,
            update_settings,
            regenerate_api_token,
            list_audit_log,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the CTCL Temporal Port app");
}
