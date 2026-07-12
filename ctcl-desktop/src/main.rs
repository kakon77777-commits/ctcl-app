// CTCL Temporal Port - Phase 1 desktop shell. Same ctcl-core/ctcl-store the CLI
// uses, wired through Tauri's IPC instead of a local HTTP server - a real
// double-click window instead of `ctcl serve` + a browser tab.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use ctcl_core::{from_ns, now_view, to_ns};
use ctcl_store::Store;
use serde::Serialize;
use std::sync::Mutex;

struct AppState {
    store: Mutex<Store>,
}

#[derive(Serialize)]
struct ConvertResult {
    canonical_unix_ns: String,
    output: String,
}

#[tauri::command]
fn now() -> Result<serde_json::Value, String> {
    now_view()
        .map(|v| serde_json::to_value(v).unwrap())
        .map_err(|e| e.to_string())
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

fn main() {
    let db_path = "ctcl-desktop-data.sqlite3";
    let store = Store::open(db_path).unwrap_or_else(|e| {
        eprintln!("failed to open local store at {db_path}: {e}");
        std::process::exit(1);
    });

    tauri::Builder::default()
        .manage(AppState { store: Mutex::new(store) })
        .invoke_handler(tauri::generate_handler![now, convert, list_systems, list_groups, expand_group])
        .run(tauri::generate_context!())
        .expect("error while running the CTCL Temporal Port app");
}
