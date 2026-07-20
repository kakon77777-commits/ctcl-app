// CTCL Temporal Port - Phase 1 desktop shell + Phase 2 Local Gateway. Same
// ctcl-core/ctcl-store the CLI uses. The webview talks to this process via
// Tauri's IPC; OTHER apps/agents talk to it via the local_api module's
// loopback HTTP server (disabled by default, per the whitepaper's §7.2).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod device_observer;
mod local_api;
mod trigger_engine;
mod wake_delivery;

use ctcl_core::{from_ns, now_view, to_ns};
use ctcl_store::{
    ActionKind, AgentEndpoint, AuditEntry, DeviceEvent, Operator, Settings, Store, Trigger, TriggerAction, TriggerKind, WakeEvent,
    WakeEventStatus, ALL_SCOPES,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

struct AppState {
    store: Arc<Mutex<Store>>,
    local_api: Mutex<Option<local_api::LocalApiHandle>>,
    device_observer: Mutex<Option<device_observer::ObserverHandle>>,
    trigger_engine: Mutex<Option<trigger_engine::TriggerEngineHandle>>,
    wake_delivery: Mutex<Option<wake_delivery::WakeDeliveryHandle>>,
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

/// Constant-rate only, matching ctcl-cli's own `system create` scope
/// (piecewise/paused/table systems remain CLI/API-only for now).
/// rename_all=snake_case so the JS side passes exactly epoch_parent_sec,
/// same explicit-match discipline as TriggerInput - no reliance on Tauri's
/// implicit camelCase<->snake_case argument conversion.
#[tauri::command(rename_all = "snake_case")]
fn create_system(state: tauri::State<AppState>, id: String, epoch_parent_sec: f64, rate: f64, offset: f64) -> Result<ctcl_store::SystemRecord, String> {
    state
        .store
        .lock()
        .unwrap()
        .create_system(&id, None, epoch_parent_sec, ctcl_core::Rate::Constant { value: rate }, offset)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_system(state: tauri::State<AppState>, id: String) -> Result<serde_json::Value, String> {
    let store = state.store.lock().unwrap();
    let record = store.get_system(&id).map_err(|e| e.to_string())?;
    let now_s = ctcl_core::now_ns() as f64 / 1e9;
    let (local, extra) = store.system_now(&id, now_s).map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "record": record, "current_local_seconds": local, "extra": extra }))
}

#[tauri::command]
fn list_groups(state: tauri::State<AppState>) -> Result<Vec<String>, String> {
    state.store.lock().unwrap().list_groups().map_err(|e| e.to_string())
}

#[tauri::command]
fn create_group(state: tauri::State<AppState>, id: String, members: Vec<String>, owner: Option<String>) -> Result<ctcl_store::GroupRecord, String> {
    state.store.lock().unwrap().create_group(&id, &members, owner.as_deref()).map_err(|e| e.to_string())
}

#[tauri::command]
fn get_group(state: tauri::State<AppState>, id: String) -> Result<ctcl_store::GroupRecord, String> {
    state.store.lock().unwrap().get_group(&id).map_err(|e| e.to_string())
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
    device_observer_running: bool,
    trigger_engine_running: bool,
    wake_delivery_running: bool,
}

fn settings_view(state: &tauri::State<AppState>, settings: Settings) -> SettingsView {
    let running = state.local_api.lock().unwrap().is_some();
    let observer_running = state.device_observer.lock().unwrap().is_some();
    let trigger_running = state.trigger_engine.lock().unwrap().is_some();
    let wake_delivery_running = state.wake_delivery.lock().unwrap().is_some();
    SettingsView {
        settings,
        all_scopes: ALL_SCOPES,
        feature_status: Settings::status(),
        local_api_running: running,
        device_observer_running: observer_running,
        trigger_engine_running: trigger_running,
        wake_delivery_running,
    }
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
    sync_device_observer(&state, &settings);
    sync_trigger_engine(&state, &settings);
    sync_wake_delivery(&state, &settings);
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

// ---- Device Clock Observer (Phase 3) ---------------------------------------

#[derive(Serialize)]
struct DeviceObserverStatus {
    running: bool,
    last: Option<device_observer::LastSample>,
}

#[tauri::command]
fn device_observer_status(state: tauri::State<AppState>) -> DeviceObserverStatus {
    let slot = state.device_observer.lock().unwrap();
    match slot.as_ref() {
        Some(handle) => DeviceObserverStatus { running: true, last: handle.last.lock().unwrap().clone() },
        None => DeviceObserverStatus { running: false, last: None },
    }
}

#[tauri::command]
fn list_device_events(state: tauri::State<AppState>) -> Result<Vec<DeviceEvent>, String> {
    state.store.lock().unwrap().list_device_events(50).map_err(|e| e.to_string())
}

// ---- Trigger Engine (Phase 4) -----------------------------------------------

#[derive(Deserialize)]
struct TriggerInput {
    id: String,
    kind: String,           // "common_instant" | "custom_time"
    system_id: Option<String>,
    operator: String,       // ">=" | "<="
    target_value: f64,
    action_kind: String,    // "notification" | "callback" | "agent_wake"
    action_target: String,  // notification: message. callback: URI. agent_wake: agent_id.
}

#[tauri::command]
fn create_trigger(state: tauri::State<AppState>, input: TriggerInput) -> Result<Trigger, String> {
    let kind = if input.kind == "custom_time" { TriggerKind::CustomTime } else { TriggerKind::CommonInstant };
    let operator = Operator::parse(&input.operator).map_err(|e| e.to_string())?;
    let action_kind = match input.action_kind.as_str() {
        "callback" => ActionKind::Callback,
        "agent_wake" => ActionKind::AgentWake,
        _ => ActionKind::Notification,
    };
    let action = TriggerAction { kind: action_kind, target: input.action_target };
    state
        .store
        .lock()
        .unwrap()
        .create_trigger(&input.id, kind, input.system_id.as_deref(), operator, input.target_value, action)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_triggers(state: tauri::State<AppState>) -> Result<Vec<Trigger>, String> {
    state.store.lock().unwrap().list_triggers().map_err(|e| e.to_string())
}

#[tauri::command]
fn cancel_trigger(state: tauri::State<AppState>, id: String) -> Result<Trigger, String> {
    state.store.lock().unwrap().cancel_trigger(&id).map_err(|e| e.to_string())
}

// ---- WakeEvent Core (Phase 4.5A) -------------------------------------------

// rename_all=snake_case so the JS side passes agent_id/event_id verbatim,
// same explicit-match discipline as create_system - these are multi-word
// top-level command params, which Tauri's default IPC naming would otherwise
// silently expect as agentId/eventId instead.
#[tauri::command(rename_all = "snake_case")]
fn list_wake_events(state: tauri::State<AppState>, agent_id: Option<String>, status: Option<String>) -> Result<Vec<WakeEvent>, String> {
    // reuses WakeEventStatus's own serde mapping instead of a second,
    // hand-duplicated string->variant table (same trick as local_api.rs).
    let status = status.and_then(|s| serde_json::from_str::<WakeEventStatus>(&format!("\"{s}\"")).ok());
    state.store.lock().unwrap().list_wake_events(agent_id.as_deref(), status).map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "snake_case")]
fn ack_wake_event(state: tauri::State<AppState>, event_id: String) -> Result<WakeEvent, String> {
    state.store.lock().unwrap().ack_wake_event(&event_id).map_err(|e| e.to_string())
}

// ---- Phase 4.5B: complete + decision receipts (read-only in this UI) ------

#[tauri::command(rename_all = "snake_case")]
fn complete_wake_event(state: tauri::State<AppState>, event_id: String) -> Result<WakeEvent, String> {
    state.store.lock().unwrap().complete_wake_event(&event_id).map_err(|e| e.to_string())
}

/// Authoring a decision receipt is an Agent Runtime's job (run_id, tool
/// calls, cost data aren't things a human types into a form) - this UI only
/// ever reads one back, over the Local API is where an agent writes them.
#[tauri::command(rename_all = "snake_case")]
fn get_decision_receipt(state: tauri::State<AppState>, event_id: String) -> Result<Option<ctcl_store::DecisionReceipt>, String> {
    state.store.lock().unwrap().get_latest_decision_receipt(&event_id).map_err(|e| e.to_string())
}

// ---- Phase 4.5D: Agent Endpoint registry ------------------------------------

#[derive(Deserialize)]
struct CreateAgentEndpointInput {
    agent_id: String,
    transport: String, // "local_process" | "loopback_http"
    endpoint: String,
    auth_ref: Option<String>,
}

#[tauri::command(rename_all = "snake_case")]
fn create_agent_endpoint(state: tauri::State<AppState>, input: CreateAgentEndpointInput) -> Result<AgentEndpoint, String> {
    state
        .store
        .lock()
        .unwrap()
        .create_agent_endpoint(&input.agent_id, &input.transport, &input.endpoint, input.auth_ref.as_deref(), &[])
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_agent_endpoints(state: tauri::State<AppState>) -> Result<Vec<AgentEndpoint>, String> {
    state.store.lock().unwrap().list_agent_endpoints().map_err(|e| e.to_string())
}

#[tauri::command(rename_all = "snake_case")]
fn set_agent_endpoint_enabled(state: tauri::State<AppState>, agent_id: String, enabled: bool) -> Result<AgentEndpoint, String> {
    state.store.lock().unwrap().set_agent_endpoint_enabled(&agent_id, enabled).map_err(|e| e.to_string())
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

/// Same start/stop discipline as sync_local_api - "off" means no thread
/// running at all, not a thread that quietly does nothing.
fn sync_device_observer(state: &tauri::State<AppState>, settings: &Settings) {
    let mut slot = state.device_observer.lock().unwrap();
    *slot = None; // dropping the old handle (if any) stops the sampling thread
    if settings.device_clock_observer_enabled {
        *slot = Some(device_observer::start(
            state.store.clone(),
            settings.device_clock_sample_interval_s,
            settings.device_clock_drift_threshold_s,
        ));
    }
}

/// Same start/stop discipline as the other two background threads.
fn sync_trigger_engine(state: &tauri::State<AppState>, settings: &Settings) {
    let mut slot = state.trigger_engine.lock().unwrap();
    *slot = None; // dropping the old handle (if any) stops the evaluation thread
    if settings.triggers_enabled {
        *slot = Some(trigger_engine::start(
            state.store.clone(),
            Arc::new(trigger_engine::RealDispatcher),
            settings.trigger_check_interval_s,
        ));
    }
}

/// Same start/stop discipline as the other three background threads. The
/// `agent_wake.dispatch` scope is checked by the thread itself every tick,
/// not here - this toggle only controls whether the thread runs at all.
fn sync_wake_delivery(state: &tauri::State<AppState>, settings: &Settings) {
    let mut slot = state.wake_delivery.lock().unwrap();
    *slot = None; // dropping the old handle (if any) stops the delivery thread
    if settings.wake_delivery_enabled {
        *slot = Some(wake_delivery::start(
            state.store.clone(),
            Arc::new(wake_delivery::RealWakeDispatcher),
            settings.wake_delivery_check_interval_s,
        ));
    }
}

fn main() {
    let db_path = "ctcl-desktop-data.sqlite3";
    let store = Store::open(db_path).unwrap_or_else(|e| {
        eprintln!("failed to open local store at {db_path}: {e}");
        std::process::exit(1);
    });
    let store = Arc::new(Mutex::new(store));

    // Start the local API / device observer immediately if a previous session
    // left them enabled - "enabled" is a persisted preference, not a
    // per-run default.
    let initial_settings = store.lock().unwrap().get_settings().ok();
    let local_api = initial_settings
        .as_ref()
        .filter(|s| s.local_api_enabled)
        .and_then(|s| local_api::start(store.clone(), s.local_api_port).ok());
    let observer = initial_settings
        .as_ref()
        .filter(|s| s.device_clock_observer_enabled)
        .map(|s| device_observer::start(store.clone(), s.device_clock_sample_interval_s, s.device_clock_drift_threshold_s));
    let triggers = initial_settings
        .as_ref()
        .filter(|s| s.triggers_enabled)
        .map(|s| trigger_engine::start(store.clone(), Arc::new(trigger_engine::RealDispatcher), s.trigger_check_interval_s));
    let wake_delivery = initial_settings
        .as_ref()
        .filter(|s| s.wake_delivery_enabled)
        .map(|s| wake_delivery::start(store.clone(), Arc::new(wake_delivery::RealWakeDispatcher), s.wake_delivery_check_interval_s));

    tauri::Builder::default()
        .manage(AppState {
            store,
            local_api: Mutex::new(local_api),
            device_observer: Mutex::new(observer),
            trigger_engine: Mutex::new(triggers),
            wake_delivery: Mutex::new(wake_delivery),
        })
        .invoke_handler(tauri::generate_handler![
            now,
            convert,
            list_systems,
            create_system,
            get_system,
            list_groups,
            create_group,
            get_group,
            expand_group,
            get_settings,
            update_settings,
            regenerate_api_token,
            list_audit_log,
            device_observer_status,
            list_device_events,
            create_trigger,
            list_triggers,
            cancel_trigger,
            list_wake_events,
            ack_wake_event,
            complete_wake_event,
            get_decision_receipt,
            create_agent_endpoint,
            list_agent_endpoints,
            set_agent_endpoint_enabled,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the CTCL Temporal Port app");
}
