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
        (Method::Get, "/v1/wake-events") => Some("wake_events.read"),
        (Method::Post, p) if p.starts_with("/v1/wake-events/") && p.ends_with("/ack") => Some("wake_events.ack"),
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
    fn stopping_the_handle_closes_the_socket() {
        let (_store, handle) = test_store_with_token(4506, "secret-token");
        drop(handle);
        std::thread::sleep(Duration::from_millis(300));
        // a fresh bind on the same port must now succeed - nothing left listening
        let rebound = Server::http("127.0.0.1:4506");
        assert!(rebound.is_ok(), "port should be free after the handle is dropped");
    }
}
