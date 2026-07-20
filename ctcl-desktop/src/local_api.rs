//! The Phase 2 Local Gateway (whitepaper §2.3/§7.2): a loopback-only HTTP API
//! other apps/agents can call into, instead of going through Tauri's IPC
//! (which is only reachable from this app's own webview). Disabled by
//! default, bearer-token authenticated, capability-scope enforced per
//! endpoint, every request audit-logged - the whitepaper's own §13 security
//! model, not just a toggle that does nothing.

use ctcl_core::{from_ns, now_view, to_ns};
use ctcl_store::Store;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

pub struct LocalApiHandle {
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for LocalApiHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Bind and start serving. Returns Err if the port is already in use etc. -
/// nothing is bound at all unless this is explicitly called (§7.2 "default off").
pub fn start(store: Arc<Mutex<Store>>, port: u16) -> Result<LocalApiHandle, String> {
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| e.to_string())?;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();

    let thread = std::thread::spawn(move || loop {
        if stop_flag_thread.load(Ordering::SeqCst) {
            break;
        }
        match server.recv_timeout(Duration::from_millis(200)) {
            Ok(Some(request)) => handle_request(&store, request),
            Ok(None) => continue,
            Err(_) => break,
        }
    });

    Ok(LocalApiHandle { stop_flag, thread: Some(thread) })
}

/// Endpoint -> required capability scope, per the whitepaper's §12.1 list.
fn required_scope(method: &Method, path: &str) -> Option<&'static str> {
    match (method, path) {
        (Method::Get, "/v1/now") => Some("instant.read"),
        (Method::Post, "/v1/convert") => Some("convert.execute"),
        (Method::Get, "/v1/systems") => Some("systems.read"),
        (Method::Post, "/v1/systems") => Some("systems.write"),
        (Method::Get, p) if p.starts_with("/v1/systems/") => Some("systems.read"),
        (Method::Get, "/v1/groups") => Some("groups.read"),
        (Method::Post, "/v1/groups") => Some("groups.write"),
        (Method::Post, p) if p.starts_with("/v1/groups/") && p.ends_with("/expand") => Some("groups.read"),
        (Method::Get, "/v1/audit") => Some("history.read"),
        (Method::Get, "/v1/device-events") => Some("device_clock.read"),
        (Method::Get, "/v1/triggers") => Some("triggers.read"),
        (Method::Get, p) if p.starts_with("/v1/triggers/") => Some("triggers.read"),
        (Method::Post, "/v1/triggers") => Some("triggers.write"),
        (Method::Post, p) if p.starts_with("/v1/triggers/") && p.ends_with("/cancel") => Some("triggers.cancel"),
        (Method::Get, "/v1/wake-events") => Some("wake_events.read"),
        (Method::Post, p) if p.starts_with("/v1/wake-events/") && p.ends_with("/ack") => Some("wake_events.ack"),
        (Method::Post, p) if p.starts_with("/v1/wake-events/") && p.ends_with("/complete") => Some("wake_events.complete"),
        (Method::Post, p) if p.starts_with("/v1/wake-events/") && p.ends_with("/decision") => Some("decision_receipts.write"),
        // covers both GET /v1/wake-events/{id} and GET /v1/wake-events/{id}/decision - both are reads.
        (Method::Get, p) if p.starts_with("/v1/wake-events/") => Some("wake_events.read"),
        (Method::Get, "/v1/agents") => Some("agents.read"),
        (Method::Post, "/v1/agents") => Some("agents.write"),
        // enable/disable are registry management (agents.write), same as
        // create - actually DISPATCHING to an enabled endpoint is gated
        // separately by agent_wake.dispatch, checked by wake_delivery.rs
        // itself before every delivery attempt, not here.
        (Method::Post, p) if p.starts_with("/v1/agents/") && (p.ends_with("/enable") || p.ends_with("/disable")) => Some("agents.write"),
        (Method::Get, p) if p.starts_with("/v1/agents/") => Some("agents.read"),
        _ => None,
    }
}

/// Minimal `key=value` query-string lookup - none of this API's endpoints
/// needed one before wake-events' `?agent_id=&status=` filters (§10.2).
fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key { Some(v) } else { None }
    })
}

fn handle_request(store: &Arc<Mutex<Store>>, mut request: Request) {
    let method = request.method().clone();
    let full_url = request.url().to_string();
    let (path, query) = full_url.split_once('?').map(|(p, q)| (p.to_string(), q.to_string())).unwrap_or((full_url.clone(), String::new()));
    let method_str = format!("{method:?}").to_uppercase();
    let scope = required_scope(&method, &path);

    let settings = match store.lock().unwrap().get_settings() {
        Ok(s) => s,
        Err(_) => return respond_error(request, 500, "STORE_ERROR", "failed to read settings"),
    };

    let expected = format!("Bearer {}", settings.local_api_token);
    let auth_ok = request.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str() == expected);
    if !auth_ok {
        if settings.audit_log_enabled {
            let _ = store.lock().unwrap().log_audit(&method_str, &full_url, scope, false, Some("missing or invalid bearer token"));
        }
        return respond_error(request, 401, "UNAUTHORIZED", "missing or invalid bearer token");
    }

    if let Some(scope) = scope {
        if !settings.is_granted(scope) {
            if settings.audit_log_enabled {
                let _ = store.lock().unwrap().log_audit(&method_str, &full_url, Some(scope), false, Some("scope not granted"));
            }
            return respond_error(request, 403, "SCOPE_NOT_GRANTED", &format!("this caller lacks the '{scope}' scope"));
        }
    }
    if settings.audit_log_enabled {
        let _ = store.lock().unwrap().log_audit(&method_str, &full_url, scope, true, None);
    }

    let mut body = String::new();
    let _ = request.as_reader().read_to_string(&mut body);
    let json_body: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);

    match (&method, path.as_str()) {
        (Method::Get, "/v1/now") => match now_view() {
            Ok(v) => respond_ok(request, json!(v)),
            Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
        },
        (Method::Post, "/v1/convert") => {
            let value = json_body.get("value").and_then(|v| v.as_str()).unwrap_or("");
            let from = json_body.get("from").and_then(|v| v.as_str()).unwrap_or("rfc3339");
            let to = json_body.get("to").and_then(|v| v.as_str()).unwrap_or("rfc3339");
            let tz = json_body.get("tz").and_then(|v| v.as_str());
            match to_ns(value, from).and_then(|ns| from_ns(ns, to, tz).map(|out| (ns, out))) {
                Ok((ns, out)) => respond_ok(request, json!({ "canonical_unix_ns": ns.to_string(), "output": out })),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Get, "/v1/systems") => match store.lock().unwrap().list_systems() {
            Ok(ids) => respond_ok(request, json!({ "systems": ids })),
            Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
        },
        (Method::Get, "/v1/groups") => match store.lock().unwrap().list_groups() {
            Ok(ids) => respond_ok(request, json!({ "groups": ids })),
            Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
        },
        (Method::Get, "/v1/audit") => match store.lock().unwrap().list_audit_log(50) {
            Ok(entries) => respond_ok(request, json!({ "entries": entries })),
            Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
        },
        (Method::Get, "/v1/device-events") => match store.lock().unwrap().list_device_events(50) {
            Ok(events) => respond_ok(request, json!({ "events": events })),
            Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
        },
        (Method::Get, "/v1/triggers") => match store.lock().unwrap().list_triggers() {
            Ok(triggers) => respond_ok(request, json!({ "triggers": triggers })),
            Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
        },
        // §10.1 Trigger write API - lets an external Agent Runtime create and
        // cancel its own triggers over the loopback API, not just the desktop
        // UI. Re-posting an existing id rearms it (create_trigger's existing
        // convention), so no separate /rearm endpoint is needed.
        (Method::Post, "/v1/triggers") => {
            let id = json_body.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let kind = if json_body.get("kind").and_then(|v| v.as_str()) == Some("custom_time") {
                ctcl_store::TriggerKind::CustomTime
            } else {
                ctcl_store::TriggerKind::CommonInstant
            };
            let system_id = json_body.get("system_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let operator_str = json_body.get("operator").and_then(|v| v.as_str()).unwrap_or(">=");
            let target_value = json_body.get("target_value").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let action_kind = match json_body.get("action_kind").and_then(|v| v.as_str()) {
                Some("callback") => ctcl_store::ActionKind::Callback,
                Some("agent_wake") => ctcl_store::ActionKind::AgentWake,
                _ => ctcl_store::ActionKind::Notification,
            };
            let action_target = json_body.get("action_target").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let operator = match ctcl_store::Operator::parse(operator_str) {
                Ok(o) => o,
                Err(e) => return respond_error(request, 400, e.code(), &e.to_string()),
            };
            let action = ctcl_store::TriggerAction { kind: action_kind, target: action_target };
            match store.lock().unwrap().create_trigger(&id, kind, system_id.as_deref(), operator, target_value, action) {
                Ok(t) => respond_ok(request, json!(t)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Post, p) if p.starts_with("/v1/triggers/") && p.ends_with("/cancel") => {
            let id = &p["/v1/triggers/".len()..p.len() - "/cancel".len()];
            match store.lock().unwrap().cancel_trigger(id) {
                Ok(t) => respond_ok(request, json!(t)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Get, p) if p.starts_with("/v1/triggers/") => {
            let id = &p["/v1/triggers/".len()..];
            match store.lock().unwrap().get_trigger(id) {
                Ok(t) => respond_ok(request, json!(t)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Get, "/v1/wake-events") => {
            let agent_id = query_param(&query, "agent_id");
            // reuses WakeEventStatus's own serde mapping instead of a second,
            // hand-duplicated string->variant table.
            let status = query_param(&query, "status").and_then(|s| serde_json::from_str::<ctcl_store::WakeEventStatus>(&format!("\"{s}\"")).ok());
            match store.lock().unwrap().list_wake_events(agent_id, status) {
                Ok(events) => respond_ok(request, json!({ "wake_events": events })),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Post, p) if p.starts_with("/v1/wake-events/") && p.ends_with("/ack") => {
            let id = &p["/v1/wake-events/".len()..p.len() - "/ack".len()];
            match store.lock().unwrap().ack_wake_event(id) {
                Ok(ev) => respond_ok(request, json!(ev)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Post, p) if p.starts_with("/v1/wake-events/") && p.ends_with("/complete") => {
            let id = &p["/v1/wake-events/".len()..p.len() - "/complete".len()];
            match store.lock().unwrap().complete_wake_event(id) {
                Ok(ev) => respond_ok(request, json!(ev)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        // §10.4 Decision Receipt API - the Agent Runtime's own report of what
        // it decided (no_action | action), never inspected or acted on by
        // CTCL itself - see decision_receipt.rs's own doc comment.
        (Method::Post, p) if p.starts_with("/v1/wake-events/") && p.ends_with("/decision") => {
            let id = &p["/v1/wake-events/".len()..p.len() - "/decision".len()];
            let agent_id = json_body.get("agent_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let run_id = json_body.get("run_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let decision = json_body.get("decision").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let summary = json_body.get("summary").and_then(|v| v.as_str()).map(|s| s.to_string());
            let tool_calls = json_body.get("tool_calls").cloned();
            let next_wake = json_body.get("next_wake").cloned();
            let cost = json_body.get("cost").cloned();
            match store.lock().unwrap().create_decision_receipt(id, &agent_id, &run_id, &decision, summary.as_deref(), tool_calls, next_wake, cost) {
                Ok(r) => respond_ok(request, json!(r)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Get, p) if p.starts_with("/v1/wake-events/") && p.ends_with("/decision") => {
            let id = &p["/v1/wake-events/".len()..p.len() - "/decision".len()];
            match store.lock().unwrap().get_latest_decision_receipt(id) {
                Ok(Some(r)) => respond_ok(request, json!(r)),
                Ok(None) => respond_error(request, 404, "NOT_FOUND", "no decision receipt filed yet for this wake event"),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Get, p) if p.starts_with("/v1/wake-events/") => {
            let id = &p["/v1/wake-events/".len()..];
            match store.lock().unwrap().get_wake_event(id) {
                Ok(ev) => respond_ok(request, json!(ev)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        // §10.3 Agent Endpoint API - the registry Phase 4.5D's Wake Delivery
        // Worker reads from. Registering one here never dispatches anything
        // by itself: it's created disabled (agents.write only toggles the
        // registry row), and even enabled, wake_delivery.rs still checks the
        // separate agent_wake.dispatch scope before ever calling out.
        (Method::Get, "/v1/agents") => match store.lock().unwrap().list_agent_endpoints() {
            Ok(agents) => respond_ok(request, json!({ "agents": agents })),
            Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
        },
        (Method::Post, "/v1/agents") => {
            let agent_id = json_body.get("agent_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let transport = json_body.get("transport").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let endpoint = json_body.get("endpoint").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let auth_ref = json_body.get("auth_ref").and_then(|v| v.as_str()).map(|s| s.to_string());
            match store.lock().unwrap().create_agent_endpoint(&agent_id, &transport, &endpoint, auth_ref.as_deref(), &[]) {
                Ok(ep) => respond_ok(request, json!(ep)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Post, p) if p.starts_with("/v1/agents/") && p.ends_with("/enable") => {
            let id = &p["/v1/agents/".len()..p.len() - "/enable".len()];
            match store.lock().unwrap().set_agent_endpoint_enabled(id, true) {
                Ok(ep) => respond_ok(request, json!(ep)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Post, p) if p.starts_with("/v1/agents/") && p.ends_with("/disable") => {
            let id = &p["/v1/agents/".len()..p.len() - "/disable".len()];
            match store.lock().unwrap().set_agent_endpoint_enabled(id, false) {
                Ok(ep) => respond_ok(request, json!(ep)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Get, p) if p.starts_with("/v1/agents/") => {
            let id = &p["/v1/agents/".len()..];
            match store.lock().unwrap().get_agent_endpoint(id) {
                Ok(ep) => respond_ok(request, json!(ep)),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        (Method::Post, p) if p.starts_with("/v1/groups/") && p.ends_with("/expand") => {
            let id = &p["/v1/groups/".len()..p.len() - "/expand".len()];
            let ns = ctcl_core::now_ns();
            match store.lock().unwrap().expand_group(id, ns) {
                Ok(result) => respond_ok(request, result),
                Err(e) => respond_error(request, 400, e.code(), &e.to_string()),
            }
        }
        _ => respond_error(request, 404, "NOT_FOUND", &format!("no route: {method_str} {full_url}")),
    }
}

fn respond_ok(request: Request, data: serde_json::Value) {
    let body = json!({ "ok": true, "data": data }).to_string();
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).unwrap();
    let _ = request.respond(Response::from_string(body).with_status_code(200).with_header(header));
}

fn respond_error(request: Request, status: u16, code: &str, message: &str) {
    let body = json!({ "ok": false, "error": { "code": code, "message": message } }).to_string();
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).unwrap();
    let _ = request.respond(Response::from_string(body).with_status_code(status).with_header(header));
}

#[cfg(test)]
mod tests {
    //! Real socket-level integration tests - a raw TcpStream HTTP client
    //! rather than a mocked handler, so these actually exercise start(),
    //! recv_timeout(), and the full auth/scope/audit pipeline end to end.
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;

    struct HttpResponse {
        status: u16,
        body: String,
    }

    fn raw_request(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> HttpResponse {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let auth_header = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth_header}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
        let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        HttpResponse { status, body: body.to_string() }
    }

    fn test_store_with_token(port: u16, token: &str) -> (Arc<Mutex<Store>>, LocalApiHandle) {
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.local_api_token = token.to_string();
        store.lock().unwrap().save_settings(&settings).unwrap();
        let handle = start(store.clone(), port).expect("server should bind");
        std::thread::sleep(Duration::from_millis(50)); // let the accept loop actually start
        (store, handle)
    }

    #[test]
    fn rejects_missing_or_wrong_token() {
        let (_store, _handle) = test_store_with_token(4501, "secret-token");
        let no_auth = raw_request(4501, "GET", "/v1/now", None, "");
        assert_eq!(no_auth.status, 401);
        let wrong_auth = raw_request(4501, "GET", "/v1/now", Some("wrong-token"), "");
        assert_eq!(wrong_auth.status, 401);
    }

    #[test]
    fn correct_token_and_default_granted_scope_succeeds() {
        let (_store, _handle) = test_store_with_token(4502, "secret-token");
        let r = raw_request(4502, "GET", "/v1/now", Some("secret-token"), "");
        assert_eq!(r.status, 200);
        assert!(r.body.contains("\"ok\":true") || r.body.contains("\"ok\": true"));
    }

    #[test]
    fn ungranted_scope_is_refused_even_with_valid_token() {
        let (_store, _handle) = test_store_with_token(4503, "secret-token");
        // systems.write is off by default (§12.2) - creating a system over the local API must fail
        let r = raw_request(4503, "POST", "/v1/systems", Some("secret-token"), "{}");
        assert_eq!(r.status, 403);
        assert!(r.body.contains("SCOPE_NOT_GRANTED"));
    }

    #[test]
    fn convert_endpoint_computes_correctly_over_http() {
        let (_store, _handle) = test_store_with_token(4504, "secret-token");
        let body = r#"{"value":"1783420000.123456789","from":"unix_s","to":"rfc3339","tz":"Asia/Taipei"}"#;
        let r = raw_request(4504, "POST", "/v1/convert", Some("secret-token"), body);
        assert_eq!(r.status, 200);
        assert!(r.body.contains("2026-07-07T18:26:40.123456789+08:00"));
    }

    #[test]
    fn every_call_is_audit_logged_allowed_and_refused_alike() {
        let (store, _handle) = test_store_with_token(4505, "secret-token");
        raw_request(4505, "GET", "/v1/now", Some("secret-token"), ""); // allowed
        raw_request(4505, "GET", "/v1/now", Some("wrong"), ""); // refused: bad token
        raw_request(4505, "POST", "/v1/systems", Some("secret-token"), "{}"); // refused: scope

        let entries = store.lock().unwrap().list_audit_log(10).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|e| e.allowed));
        assert!(entries.iter().filter(|e| !e.allowed).count() == 2);
    }

    #[test]
    fn device_events_route_is_scope_gated_then_succeeds_once_granted() {
        let (store, _handle) = test_store_with_token(4507, "secret-token");
        // device_clock.read is off by default (§12.2) - the route must refuse.
        let refused = raw_request(4507, "GET", "/v1/device-events", Some("secret-token"), "");
        assert_eq!(refused.status, 403);

        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("device_clock.read".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let allowed = raw_request(4507, "GET", "/v1/device-events", Some("secret-token"), "");
        assert_eq!(allowed.status, 200);
        assert!(allowed.body.contains("\"events\""));
    }

    #[test]
    fn triggers_route_is_scope_gated_then_succeeds_once_granted() {
        let (store, _handle) = test_store_with_token(4508, "secret-token");
        let refused = raw_request(4508, "GET", "/v1/triggers", Some("secret-token"), "");
        assert_eq!(refused.status, 403);

        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("triggers.read".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let allowed = raw_request(4508, "GET", "/v1/triggers", Some("secret-token"), "");
        assert_eq!(allowed.status, 200);
        assert!(allowed.body.contains("\"triggers\""));
    }

    #[test]
    fn wake_events_read_is_scope_gated_then_succeeds_once_granted() {
        let (store, _handle) = test_store_with_token(4509, "secret-token");
        store.lock().unwrap().create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();

        let refused = raw_request(4509, "GET", "/v1/wake-events", Some("secret-token"), "");
        assert_eq!(refused.status, 403, "wake_events.read is off by default, same discipline as triggers.read");

        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("wake_events.read".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let allowed = raw_request(4509, "GET", "/v1/wake-events", Some("secret-token"), "");
        assert_eq!(allowed.status, 200);
        assert!(allowed.body.contains("\"wake_events\""));
        assert!(allowed.body.contains("agent:primary"));
    }

    #[test]
    fn wake_events_read_filters_by_query_params() {
        let (store, _handle) = test_store_with_token(4510, "secret-token");
        store.lock().unwrap().create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        store.lock().unwrap().create_wake_event("agent:b", None, "r", json!({}), json!({}), json!({}), "k2").unwrap();
        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("wake_events.read".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let filtered = raw_request(4510, "GET", "/v1/wake-events?agent_id=agent:a", Some("secret-token"), "");
        assert_eq!(filtered.status, 200);
        assert!(filtered.body.contains("agent:a"));
        assert!(!filtered.body.contains("agent:b"));
    }

    #[test]
    fn wake_events_ack_is_scope_gated_and_transitions_status() {
        let (store, _handle) = test_store_with_token(4511, "secret-token");
        let ev = store.lock().unwrap().create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();

        let refused = raw_request(4511, "POST", &format!("/v1/wake-events/{}/ack", ev.event_id), Some("secret-token"), "");
        assert_eq!(refused.status, 403, "wake_events.ack is off by default");

        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("wake_events.ack".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let allowed = raw_request(4511, "POST", &format!("/v1/wake-events/{}/ack", ev.event_id), Some("secret-token"), "");
        assert_eq!(allowed.status, 200);
        assert!(allowed.body.contains("\"acknowledged\""));
        assert_eq!(store.lock().unwrap().get_wake_event(&ev.event_id).unwrap().status, ctcl_store::WakeEventStatus::Acknowledged);
    }

    #[test]
    fn trigger_write_api_creates_reads_and_cancels() {
        let (store, _handle) = test_store_with_token(4512, "secret-token");
        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("triggers.write".to_string(), true);
        settings.scopes.insert("triggers.read".to_string(), true);
        settings.scopes.insert("triggers.cancel".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let refused = raw_request(4512, "POST", "/v1/triggers", None, "{}");
        assert_eq!(refused.status, 401, "still needs a valid bearer token even with scopes granted");

        let body = r#"{"id":"trigger:api-created","kind":"common_instant","operator":">=","target_value":100,"action_kind":"agent_wake","action_target":"agent:primary"}"#;
        let created = raw_request(4512, "POST", "/v1/triggers", Some("secret-token"), body);
        assert_eq!(created.status, 200, "body: {}", created.body);
        assert!(created.body.contains("\"agent_wake\""));

        let read = raw_request(4512, "GET", "/v1/triggers/trigger:api-created", Some("secret-token"), "");
        assert_eq!(read.status, 200);
        assert!(read.body.contains("\"active\""));

        let cancelled = raw_request(4512, "POST", "/v1/triggers/trigger:api-created/cancel", Some("secret-token"), "");
        assert_eq!(cancelled.status, 200);
        assert_eq!(store.lock().unwrap().get_trigger("trigger:api-created").unwrap().status, ctcl_store::TriggerStatus::Cancelled);
    }

    #[test]
    fn trigger_write_and_cancel_are_separately_scope_gated() {
        let (_store, _handle) = test_store_with_token(4513, "secret-token");
        let create_refused = raw_request(4513, "POST", "/v1/triggers", Some("secret-token"), "{}");
        assert_eq!(create_refused.status, 403, "triggers.write is off by default");

        let cancel_refused = raw_request(4513, "POST", "/v1/triggers/some-id/cancel", Some("secret-token"), "");
        assert_eq!(cancel_refused.status, 403, "triggers.cancel is off by default, separately from triggers.write");
    }

    #[test]
    fn wake_event_single_item_read_route() {
        let (store, _handle) = test_store_with_token(4514, "secret-token");
        let ev = store.lock().unwrap().create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("wake_events.read".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let refused_status = raw_request(4514, "GET", &format!("/v1/wake-events/{}", ev.event_id), None, "");
        assert_eq!(refused_status.status, 401);

        let allowed = raw_request(4514, "GET", &format!("/v1/wake-events/{}", ev.event_id), Some("secret-token"), "");
        assert_eq!(allowed.status, 200);
        assert!(allowed.body.contains("agent:primary"));

        let unknown = raw_request(4514, "GET", "/v1/wake-events/wake:does-not-exist", Some("secret-token"), "");
        assert_eq!(unknown.status, 400);
    }

    #[test]
    fn wake_event_complete_requires_prior_ack_and_is_scope_gated() {
        let (store, _handle) = test_store_with_token(4515, "secret-token");
        let ev = store.lock().unwrap().create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();

        let refused = raw_request(4515, "POST", &format!("/v1/wake-events/{}/complete", ev.event_id), Some("secret-token"), "");
        assert_eq!(refused.status, 403, "wake_events.complete is off by default");

        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("wake_events.complete".to_string(), true);
        settings.scopes.insert("wake_events.ack".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let too_early = raw_request(4515, "POST", &format!("/v1/wake-events/{}/complete", ev.event_id), Some("secret-token"), "");
        assert_eq!(too_early.status, 400, "must not complete before acknowledging");

        raw_request(4515, "POST", &format!("/v1/wake-events/{}/ack", ev.event_id), Some("secret-token"), "");
        let completed = raw_request(4515, "POST", &format!("/v1/wake-events/{}/complete", ev.event_id), Some("secret-token"), "");
        assert_eq!(completed.status, 200);
        assert_eq!(store.lock().unwrap().get_wake_event(&ev.event_id).unwrap().status, ctcl_store::WakeEventStatus::Completed);
    }

    #[test]
    fn decision_receipt_write_and_read_round_trip_over_http() {
        let (store, _handle) = test_store_with_token(4516, "secret-token");
        let ev = store.lock().unwrap().create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();

        let refused = raw_request(4516, "GET", &format!("/v1/wake-events/{}/decision", ev.event_id), Some("secret-token"), "");
        assert_eq!(refused.status, 403, "reading a decision receipt still needs wake_events.read");

        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("wake_events.read".to_string(), true);
        settings.scopes.insert("decision_receipts.write".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let none_yet = raw_request(4516, "GET", &format!("/v1/wake-events/{}/decision", ev.event_id), Some("secret-token"), "");
        assert_eq!(none_yet.status, 404, "no receipt filed yet");

        let write_refused = raw_request(4516, "POST", &format!("/v1/wake-events/{}/decision", ev.event_id), None, "{}");
        assert_eq!(write_refused.status, 401);

        let body = r#"{"agent_id":"agent:primary","run_id":"run:01J","decision":"no_action","summary":"nothing to do","next_wake":{"kind":"relative","after_seconds":3600}}"#;
        let written = raw_request(4516, "POST", &format!("/v1/wake-events/{}/decision", ev.event_id), Some("secret-token"), body);
        assert_eq!(written.status, 200, "body: {}", written.body);
        assert!(written.body.contains("no_action"));

        let read = raw_request(4516, "GET", &format!("/v1/wake-events/{}/decision", ev.event_id), Some("secret-token"), "");
        assert_eq!(read.status, 200);
        assert!(read.body.contains("nothing to do"));
        assert!(read.body.contains("3600"));
    }

    #[test]
    fn decision_receipt_rejects_invalid_decision_value_over_http() {
        let (store, _handle) = test_store_with_token(4517, "secret-token");
        let ev = store.lock().unwrap().create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("decision_receipts.write".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let body = r#"{"agent_id":"agent:primary","run_id":"run:1","decision":"maybe"}"#;
        let r = raw_request(4517, "POST", &format!("/v1/wake-events/{}/decision", ev.event_id), Some("secret-token"), body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn agent_endpoint_create_list_get_over_http() {
        let (store, _handle) = test_store_with_token(4523, "secret-token");
        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("agents.write".to_string(), true);
        settings.scopes.insert("agents.read".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let body = r#"{"agent_id":"agent:primary","transport":"loopback_http","endpoint":"http://127.0.0.1:4400/wake","auth_ref":"secret"}"#;
        let created = raw_request(4523, "POST", "/v1/agents", Some("secret-token"), body);
        assert_eq!(created.status, 200, "body: {}", created.body);
        assert!(created.body.contains("\"enabled\":false") || created.body.contains("\"enabled\": false"), "must start disabled per §9.1");

        let listed = raw_request(4523, "GET", "/v1/agents", Some("secret-token"), "");
        assert_eq!(listed.status, 200);
        assert!(listed.body.contains("agent:primary"));

        let got = raw_request(4523, "GET", "/v1/agents/agent:primary", Some("secret-token"), "");
        assert_eq!(got.status, 200);
        assert!(got.body.contains("loopback_http"));
    }

    #[test]
    fn agent_endpoint_write_and_read_are_separately_scope_gated() {
        let (_store, _handle) = test_store_with_token(4524, "secret-token");
        let create_refused = raw_request(4524, "POST", "/v1/agents", Some("secret-token"), "{}");
        assert_eq!(create_refused.status, 403, "agents.write is off by default");

        let list_refused = raw_request(4524, "GET", "/v1/agents", Some("secret-token"), "");
        assert_eq!(list_refused.status, 403, "agents.read is off by default");
    }

    #[test]
    fn agent_endpoint_enable_then_disable_over_http() {
        let (store, _handle) = test_store_with_token(4525, "secret-token");
        store.lock().unwrap().create_agent_endpoint("agent:primary", "loopback_http", "http://127.0.0.1:4400/wake", None, &[]).unwrap();
        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("agents.write".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let enabled = raw_request(4525, "POST", "/v1/agents/agent:primary/enable", Some("secret-token"), "");
        assert_eq!(enabled.status, 200);
        assert!(store.lock().unwrap().get_agent_endpoint("agent:primary").unwrap().enabled);

        let disabled = raw_request(4525, "POST", "/v1/agents/agent:primary/disable", Some("secret-token"), "");
        assert_eq!(disabled.status, 200);
        assert!(!store.lock().unwrap().get_agent_endpoint("agent:primary").unwrap().enabled);
    }

    #[test]
    fn agent_endpoint_create_rejects_an_invalid_transport_over_http() {
        let (store, _handle) = test_store_with_token(4526, "secret-token");
        let mut settings = store.lock().unwrap().get_settings().unwrap();
        settings.scopes.insert("agents.write".to_string(), true);
        store.lock().unwrap().save_settings(&settings).unwrap();

        let body = r#"{"agent_id":"agent:primary","transport":"remote_webhook","endpoint":"https://example.com"}"#;
        let r = raw_request(4526, "POST", "/v1/agents", Some("secret-token"), body);
        assert_eq!(r.status, 400);
    }

    #[test]
    fn stopping_the_handle_closes_the_socket() {
        let (_store, handle) = test_store_with_token(4506, "secret-token");
        drop(handle);
        std::thread::sleep(Duration::from_millis(300));
        // a fresh bind on the same port must now succeed - nothing left listening
        let rebound = Server::http("127.0.0.1:4506");
        assert!(rebound.is_ok(), "port should be free after the handle is dropped");
    }
}
