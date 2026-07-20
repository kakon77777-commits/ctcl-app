//! Local MCP Server (Phase 4.5C, whitepaper §12): exposes CTCL's local
//! capabilities as MCP tools over `stdio`, per §12.1's "Local MCP" deployment
//! form and §9.1's local-process trust model - whoever can spawn this binary
//! already has local execution rights, so unlike the Local API's loopback
//! HTTP surface, tool calls here are NOT bearer-token gated. Capability
//! scopes still apply (§11/§12.3 "讀寫分離" - write-capable tools live only
//! here, never on a future Remote MCP), and every call is audit-logged to
//! the SAME `audit_log` table the Local API already writes to - one audit
//! trail regardless of which interface was used.
//!
//! Tool set is the whitepaper's own §12.2 list, MINUS three tools this
//! project cannot honestly implement yet: `ctcl.inspect_boundary`,
//! `ctcl.resolve_temporal_context`, and `ctcl.plan_shared_instant` are
//! CTCL Web (the Cloudflare Worker, a separate JS codebase) features with no
//! equivalent in `ctcl-core`/`ctcl-store` today. Faking them or silently
//! dropping them would violate this project's honesty discipline; they're
//! declared missing instead - see `get_info()`'s `instructions` below, which
//! surfaces the gap honestly instead of leaving a connecting Agent Runtime to
//! guess why `ctcl.inspect_boundary` doesn't exist.
//!
//! No `ctcl.*` tool for writing a decision receipt exists because the
//! whitepaper's own §12.2 list doesn't name one - an Agent Runtime posts a
//! decision receipt over the Local API's `POST /v1/wake-events/{id}/decision`
//! (Phase 4.5B) instead.

use ctcl_core::CtclError;
use ctcl_store::{ActionKind, Operator, Store, StoreError, TriggerAction, TriggerKind, WakeEventStatus};
use rmcp::{ServerHandler, handler::server::wrapper::Parameters, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::{Arc, Mutex};

fn fmt_store_err(e: StoreError) -> String {
    format!("{}: {}", e.code(), e)
}
fn fmt_core_err(e: CtclError) -> String {
    format!("{}: {}", e.code(), e)
}
fn fmt_json_err(e: serde_json::Error) -> String {
    format!("SERIALIZATION_ERROR: {e}")
}

pub struct CtclMcpServer {
    store: Arc<Mutex<Store>>,
}

impl CtclMcpServer {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        Self { store }
    }

    /// Scope + audit gate every tool shares. `tool_name` doubles as the
    /// audit log's `path` column (mirroring how local_api.rs logs an HTTP
    /// path) so `GET /v1/audit` shows MCP and Local API calls side by side.
    fn require_scope(&self, tool_name: &str, scope: &str) -> Result<(), String> {
        let store = self.store.lock().unwrap();
        let settings = store.get_settings().map_err(fmt_store_err)?;
        if !settings.is_granted(scope) {
            if settings.audit_log_enabled {
                let _ = store.log_audit("MCP", tool_name, Some(scope), false, Some("scope not granted"));
            }
            return Err(format!("SCOPE_NOT_GRANTED: this MCP caller lacks the '{scope}' scope"));
        }
        if settings.audit_log_enabled {
            let _ = store.log_audit("MCP", tool_name, Some(scope), true, None);
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ConvertInput {
    /// The value to convert, in the `from` encoding.
    value: String,
    /// Source encoding: unix_s | unix_ms | unix_us | unix_ns | rfc3339.
    from: String,
    /// Target encoding: unix_s | unix_ms | unix_us | unix_ns | rfc3339.
    to: String,
    /// IANA timezone for rfc3339 output (e.g. "Asia/Taipei"). Ignored otherwise.
    tz: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RegisterInstantInput {
    /// Canonical unix nanoseconds as a decimal string (a JSON number would
    /// lose precision at this magnitude). Omit to register "now".
    unix_ns: Option<String>,
    label: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct IdInput {
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SystemNowInput {
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateSystemInput {
    id: String,
    /// The system's epoch, in the parent (unix) system's seconds.
    epoch_parent_sec: f64,
    /// Constant rate only (matches ctcl-cli's own `system create` scope -
    /// piecewise/paused/table systems remain CLI/API-only).
    rate: f64,
    #[serde(default)]
    offset: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CreateTriggerInput {
    id: String,
    /// "common_instant" | "custom_time".
    kind: String,
    /// Required when kind = "custom_time".
    system_id: Option<String>,
    /// ">=" | "<=".
    operator: String,
    target_value: f64,
    /// "notification" | "callback" | "agent_wake".
    action_kind: String,
    /// notification: message. callback: URI. agent_wake: agent_id.
    action_target: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListWakeEventsInput {
    agent_id: Option<String>,
    /// pending | delivering | delivered | acknowledged | decided_no_action |
    /// decided_action | completed | retry_wait | dead_letter.
    status: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EventIdInput {
    event_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SchedulePulseInput {
    /// Which agent this pulse wakes (an opaque routing label, not validated
    /// against any registry - same as agent_wake's action_target elsewhere).
    agent_id: String,
    /// Seconds from now the wake should fire.
    after_seconds: f64,
    /// Trigger id. Auto-generated if omitted.
    id: Option<String>,
}

#[tool_router]
impl CtclMcpServer {
    #[tool(name = "ctcl.now", description = "Get the current verified reference instant across all supported encodings and timescales.")]
    async fn now(&self) -> Result<String, String> {
        self.require_scope("ctcl.now", "instant.read")?;
        let v = ctcl_core::now_view().map_err(fmt_core_err)?;
        serde_json::to_string(&v).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.convert", description = "Convert a time value between encodings (unix_s/ms/us/ns, rfc3339) and timezones.")]
    async fn convert(&self, Parameters(input): Parameters<ConvertInput>) -> Result<String, String> {
        self.require_scope("ctcl.convert", "convert.execute")?;
        let ns = ctcl_core::to_ns(&input.value, &input.from).map_err(fmt_core_err)?;
        let out = ctcl_core::from_ns(ns, &input.to, input.tz.as_deref()).map_err(fmt_core_err)?;
        serde_json::to_string(&serde_json::json!({ "canonical_unix_ns": ns.to_string(), "output": out })).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.register_instant", description = "Register a reference instant for later retrieval by id. Omit unix_ns to register the current wall-clock instant.")]
    async fn register_instant(&self, Parameters(input): Parameters<RegisterInstantInput>) -> Result<String, String> {
        self.require_scope("ctcl.register_instant", "instant.create")?;
        let (ns, from_wall_clock) = match input.unix_ns {
            Some(s) => (s.parse::<i128>().map_err(|e| format!("INVALID_TIME_VALUE: {e}"))?, false),
            None => (ctcl_core::now_ns(), true),
        };
        let store = self.store.lock().unwrap();
        let rec = store.register_instant(ns, input.label.as_deref(), from_wall_clock).map_err(fmt_store_err)?;
        serde_json::to_string(&rec).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.get_instant", description = "Retrieve a previously-registered instant by id.")]
    async fn get_instant(&self, Parameters(input): Parameters<IdInput>) -> Result<String, String> {
        self.require_scope("ctcl.get_instant", "instant.read")?;
        let store = self.store.lock().unwrap();
        let rec = store.get_instant(&input.id).map_err(fmt_store_err)?;
        serde_json::to_string(&rec).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.list_systems", description = "List all custom temporal system ids.")]
    async fn list_systems(&self) -> Result<String, String> {
        self.require_scope("ctcl.list_systems", "systems.read")?;
        let store = self.store.lock().unwrap();
        let ids = store.list_systems().map_err(fmt_store_err)?;
        serde_json::to_string(&ids).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.system_now", description = "Evaluate a custom temporal system's current local time.")]
    async fn system_now(&self, Parameters(input): Parameters<SystemNowInput>) -> Result<String, String> {
        self.require_scope("ctcl.system_now", "systems.read")?;
        let store = self.store.lock().unwrap();
        let now_s = ctcl_core::now_ns() as f64 / 1e9;
        let (local, extra) = store.system_now(&input.id, now_s).map_err(fmt_store_err)?;
        serde_json::to_string(&serde_json::json!({ "current_local_seconds": local, "extra": extra })).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.create_system", description = "Create or rearm a constant-rate custom temporal system.")]
    async fn create_system(&self, Parameters(input): Parameters<CreateSystemInput>) -> Result<String, String> {
        self.require_scope("ctcl.create_system", "systems.write")?;
        let store = self.store.lock().unwrap();
        let rec = store
            .create_system(&input.id, None, input.epoch_parent_sec, ctcl_core::Rate::Constant { value: input.rate }, input.offset)
            .map_err(fmt_store_err)?;
        serde_json::to_string(&rec).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.expand_group", description = "Project the current instant across every member of a Temporal Group.")]
    async fn expand_group(&self, Parameters(input): Parameters<IdInput>) -> Result<String, String> {
        self.require_scope("ctcl.expand_group", "groups.read")?;
        let store = self.store.lock().unwrap();
        let ns = ctcl_core::now_ns();
        let result = store.expand_group(&input.id, ns).map_err(fmt_store_err)?;
        serde_json::to_string(&result).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.create_trigger", description = "Create or rearm a Trigger (fires once when a common instant or custom-time condition is satisfied).")]
    async fn create_trigger(&self, Parameters(input): Parameters<CreateTriggerInput>) -> Result<String, String> {
        self.require_scope("ctcl.create_trigger", "triggers.write")?;
        let kind = if input.kind == "custom_time" { TriggerKind::CustomTime } else { TriggerKind::CommonInstant };
        let operator = Operator::parse(&input.operator).map_err(fmt_store_err)?;
        let action_kind = match input.action_kind.as_str() {
            "callback" => ActionKind::Callback,
            "agent_wake" => ActionKind::AgentWake,
            _ => ActionKind::Notification,
        };
        let action = TriggerAction { kind: action_kind, target: input.action_target };
        let store = self.store.lock().unwrap();
        let t = store
            .create_trigger(&input.id, kind, input.system_id.as_deref(), operator, input.target_value, action)
            .map_err(fmt_store_err)?;
        serde_json::to_string(&t).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.list_triggers", description = "List every Trigger regardless of status.")]
    async fn list_triggers(&self) -> Result<String, String> {
        self.require_scope("ctcl.list_triggers", "triggers.read")?;
        let store = self.store.lock().unwrap();
        let triggers = store.list_triggers().map_err(fmt_store_err)?;
        serde_json::to_string(&triggers).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.cancel_trigger", description = "Cancel an active Trigger.")]
    async fn cancel_trigger(&self, Parameters(input): Parameters<IdInput>) -> Result<String, String> {
        self.require_scope("ctcl.cancel_trigger", "triggers.cancel")?;
        let store = self.store.lock().unwrap();
        let t = store.cancel_trigger(&input.id).map_err(fmt_store_err)?;
        serde_json::to_string(&t).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.list_wake_events", description = "List WakeEvents, optionally filtered by agent_id and/or status.")]
    async fn list_wake_events(&self, Parameters(input): Parameters<ListWakeEventsInput>) -> Result<String, String> {
        self.require_scope("ctcl.list_wake_events", "wake_events.read")?;
        let status: Option<WakeEventStatus> = input.status.and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok());
        let store = self.store.lock().unwrap();
        let events = store.list_wake_events(input.agent_id.as_deref(), status).map_err(fmt_store_err)?;
        serde_json::to_string(&events).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.ack_wake_event", description = "Acknowledge a pending WakeEvent (pending -> acknowledged).")]
    async fn ack_wake_event(&self, Parameters(input): Parameters<EventIdInput>) -> Result<String, String> {
        self.require_scope("ctcl.ack_wake_event", "wake_events.ack")?;
        let store = self.store.lock().unwrap();
        let ev = store.ack_wake_event(&input.event_id).map_err(fmt_store_err)?;
        serde_json::to_string(&ev).map_err(fmt_json_err)
    }

    #[tool(name = "ctcl.complete_wake_event", description = "Mark an acknowledged WakeEvent completed (acknowledged -> completed).")]
    async fn complete_wake_event(&self, Parameters(input): Parameters<EventIdInput>) -> Result<String, String> {
        self.require_scope("ctcl.complete_wake_event", "wake_events.complete")?;
        let store = self.store.lock().unwrap();
        let ev = store.complete_wake_event(&input.event_id).map_err(fmt_store_err)?;
        serde_json::to_string(&ev).map_err(fmt_json_err)
    }

    #[tool(
        name = "ctcl.schedule_pulse",
        description = "Convenience wrapper over ctcl.create_trigger: schedule this agent's own next wake `after_seconds` from now, without computing an absolute timestamp (whitepaper §25 schedule_next_wake_if_needed)."
    )]
    async fn schedule_pulse(&self, Parameters(input): Parameters<SchedulePulseInput>) -> Result<String, String> {
        self.require_scope("ctcl.schedule_pulse", "triggers.write")?;
        if input.after_seconds <= 0.0 {
            return Err("INVALID_TIME_VALUE: after_seconds must be positive".to_string());
        }
        let id = input.id.unwrap_or_else(|| format!("trigger:pulse:{}", uuid::Uuid::new_v4()));
        let target = ctcl_core::now_ns() as f64 / 1e9 + input.after_seconds;
        let action = TriggerAction { kind: ActionKind::AgentWake, target: input.agent_id };
        let store = self.store.lock().unwrap();
        let t = store
            .create_trigger(&id, TriggerKind::CommonInstant, None, Operator::Ge, target, action)
            .map_err(fmt_store_err)?;
        serde_json::to_string(&t).map_err(fmt_json_err)
    }
}

#[tool_handler(
    instructions = "CTCL (Common Temporal Coordinate Layer) local tools: reference-instant math, custom temporal systems, Temporal Groups, and the Trigger/WakeEvent pipeline (whitepaper CTCL_Agent_Wake_MCP_Temporal_Runtime). CTCL produces WakeEvents and lets you poll/ack/complete them - it never calls other tools or takes action on your behalf (Wake != Act). File a decision receipt over the Local API's POST /v1/wake-events/{event_id}/decision, not through a tool here. Not implemented: ctcl.inspect_boundary, ctcl.resolve_temporal_context, ctcl.plan_shared_instant - these are CTCL Web (commoninstant.org, a separate Cloudflare Worker) features with no local Rust equivalent yet."
)]
impl ServerHandler for CtclMcpServer {}

#[cfg(test)]
mod tests {
    //! Direct method-level tests - call the tool functions as plain async
    //! methods, bypassing the JSON-RPC/stdio transport entirely. The real
    //! protocol wiring is covered separately by tests/mcp_protocol.rs, which
    //! spawns the actual compiled binary and talks MCP to it for real.
    use super::*;

    fn server_with_scopes(granted: &[&str]) -> CtclMcpServer {
        let store = Store::open(":memory:").unwrap();
        let mut settings = store.get_settings().unwrap();
        for scope in granted {
            settings.scopes.insert(scope.to_string(), true);
        }
        store.save_settings(&settings).unwrap();
        CtclMcpServer::new(Arc::new(Mutex::new(store)))
    }

    fn server_with_every_scope_granted() -> CtclMcpServer {
        server_with_scopes(ctcl_store::ALL_SCOPES)
    }

    #[tokio::test]
    async fn now_returns_a_real_instant_view() {
        let server = server_with_every_scope_granted();
        let out = server.now().await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("unix_ns").is_some(), "expected an InstantView with unix_ns, got: {out}");
        assert!(v.get("encodings").is_some());
        assert!(v.get("timescales").is_some());
    }

    // triggers.write is off by default (§12.2) - a real test of refusal,
    // unlike instant.read which server_with_scopes(&[]) can't revoke since
    // it's already granted by Settings::default().
    #[tokio::test]
    async fn write_tool_is_refused_without_the_scope() {
        let server = server_with_scopes(&[]);
        let err = server
            .create_trigger(Parameters(CreateTriggerInput {
                id: "t".to_string(),
                kind: "common_instant".to_string(),
                system_id: None,
                operator: ">=".to_string(),
                target_value: 1.0,
                action_kind: "notification".to_string(),
                action_target: "x".to_string(),
            }))
            .await
            .unwrap_err();
        assert!(err.starts_with("SCOPE_NOT_GRANTED"), "got: {err}");
    }

    #[tokio::test]
    async fn scope_refusal_is_audit_logged() {
        let server = server_with_scopes(&[]);
        server.list_triggers().await.unwrap_err(); // triggers.read is also off by default
        let entries = server.store.lock().unwrap().list_audit_log(10).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].allowed);
        assert_eq!(entries[0].path, "ctcl.list_triggers");
    }

    #[tokio::test]
    async fn convert_round_trips_a_value() {
        let server = server_with_every_scope_granted();
        let out = server
            .convert(Parameters(ConvertInput {
                value: "1783420000.123456789".to_string(),
                from: "unix_s".to_string(),
                to: "rfc3339".to_string(),
                tz: Some("Asia/Taipei".to_string()),
            }))
            .await
            .unwrap();
        assert!(out.contains("2026-07-07T18:26:40.123456789+08:00"), "got: {out}");
    }

    #[tokio::test]
    async fn register_then_get_instant_round_trips() {
        let server = server_with_every_scope_granted();
        let registered = server
            .register_instant(Parameters(RegisterInstantInput { unix_ns: Some("1700000000000000000".to_string()), label: Some("test".to_string()) }))
            .await
            .unwrap();
        let rec: serde_json::Value = serde_json::from_str(&registered).unwrap();
        let id = rec["id"].as_str().unwrap().to_string();

        let fetched = server.get_instant(Parameters(IdInput { id })).await.unwrap();
        assert!(fetched.contains("1700000000000000000"));
        assert!(fetched.contains("test"));
    }

    #[tokio::test]
    async fn create_trigger_then_list_and_cancel() {
        let server = server_with_every_scope_granted();
        server
            .create_trigger(Parameters(CreateTriggerInput {
                id: "trigger:mcp-test".to_string(),
                kind: "common_instant".to_string(),
                system_id: None,
                operator: ">=".to_string(),
                target_value: 9_999_999_999.0,
                action_kind: "notification".to_string(),
                action_target: "hi".to_string(),
            }))
            .await
            .unwrap();

        let listed = server.list_triggers().await.unwrap();
        assert!(listed.contains("trigger:mcp-test"));

        let cancelled = server.cancel_trigger(Parameters(IdInput { id: "trigger:mcp-test".to_string() })).await.unwrap();
        assert!(cancelled.contains("\"cancelled\""));
    }

    #[tokio::test]
    async fn schedule_pulse_creates_an_agent_wake_trigger_due_soon() {
        let server = server_with_every_scope_granted();
        let out = server
            .schedule_pulse(Parameters(SchedulePulseInput { agent_id: "agent:primary".to_string(), after_seconds: 1.0, id: None }))
            .await
            .unwrap();
        let t: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(t["action"]["kind"], "agent_wake");
        assert_eq!(t["action"]["target"], "agent:primary");
        assert!(t["id"].as_str().unwrap().starts_with("trigger:pulse:"));
    }

    #[tokio::test]
    async fn schedule_pulse_rejects_non_positive_after_seconds() {
        let server = server_with_every_scope_granted();
        let err = server
            .schedule_pulse(Parameters(SchedulePulseInput { agent_id: "agent:primary".to_string(), after_seconds: 0.0, id: None }))
            .await
            .unwrap_err();
        assert!(err.starts_with("INVALID_TIME_VALUE"), "got: {err}");
    }

    #[tokio::test]
    async fn wake_event_ack_and_complete_flow() {
        let server = server_with_every_scope_granted();
        let event_id = {
            let store = server.store.lock().unwrap();
            store.create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap().event_id
        };

        let too_early = server.complete_wake_event(Parameters(EventIdInput { event_id: event_id.clone() })).await;
        assert!(too_early.is_err(), "must not complete before ack");

        let acked = server.ack_wake_event(Parameters(EventIdInput { event_id: event_id.clone() })).await.unwrap();
        assert!(acked.contains("\"acknowledged\""));

        let completed = server.complete_wake_event(Parameters(EventIdInput { event_id })).await.unwrap();
        assert!(completed.contains("\"completed\""));
    }

    #[tokio::test]
    async fn list_wake_events_filters_by_agent_and_status() {
        let server = server_with_every_scope_granted();
        {
            let store = server.store.lock().unwrap();
            store.create_wake_event("agent:a", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();
            store.create_wake_event("agent:b", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k2").unwrap();
        }
        let filtered = server
            .list_wake_events(Parameters(ListWakeEventsInput { agent_id: Some("agent:a".to_string()), status: None }))
            .await
            .unwrap();
        assert!(filtered.contains("agent:a"));
        assert!(!filtered.contains("agent:b"));
    }

    #[tokio::test]
    async fn create_system_then_system_now() {
        let server = server_with_every_scope_granted();
        server
            .create_system(Parameters(CreateSystemInput { id: "agent:a:active-time".to_string(), epoch_parent_sec: 1_700_000_000.0, rate: 1.0, offset: 0.0 }))
            .await
            .unwrap();
        let out = server.system_now(Parameters(SystemNowInput { id: "agent:a:active-time".to_string() })).await.unwrap();
        assert!(out.contains("current_local_seconds"));
    }
}
