//! Wake Delivery Worker (Phase 4.5D, whitepaper §8): actively PUSHES pending
//! WakeEvents to a registered, enabled Agent Endpoint, instead of leaving the
//! agent to poll. Additive, not a replacement - any agent_id with no
//! registered endpoint keeps working exactly as Phase 4.5B's poll-only path.
//!
//! Same "mark only on success" discipline as trigger_engine.rs:
//! `Store::mark_wake_event_delivering` claims an event right before dispatch;
//! `record_delivery_success`/`record_delivery_failure` report the outcome.
//! The Store itself owns the retry-vs-dead-letter decision (§8.2's
//! exponential backoff, §8.1 step 6's attempt cap) - this module only
//! reports pass/fail per attempt.
//!
//! Three independent gates before anything is actually dispatched: (1)
//! `wake_delivery_enabled` (this thread doesn't even start otherwise), (2)
//! the `agent_wake.dispatch` capability scope (checked every tick, not just
//! once at startup, so revoking it mid-run takes effect on the next pass),
//! (3) the per-endpoint `enabled` flag (`due_for_delivery` already filters
//! on this in ctcl-store).

use ctcl_store::{AgentEndpoint, Store, WakeEvent, WakeEventStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub trait WakeDispatcher: Send + Sync {
    fn deliver(&self, endpoint: &AgentEndpoint, event: &WakeEvent) -> Result<(), String>;
}

/// local_process: spawns the registered executable with a FIXED, structured
/// argument template (`--wake-event <event_id>`) - never a shell, never
/// string-interpolated command text (whitepaper §9.1's explicit security
/// requirement: "不允許任意命令字串" / "參數使用結構化模板"). A successful
/// spawn counts as delivered; CTCL does not wait for or interpret the
/// child's exit code - what the agent does next is its own concern, same
/// boundary as everywhere else in this whitepaper's implementation.
///
/// loopback_http: a blocking POST of the WakeEvent JSON to the registered
/// `http://127.0.0.1:.../...` endpoint, `Authorization: Bearer <auth_ref>`
/// if one was registered, a 5s timeout. Only `202 Accepted` counts as
/// delivered (§9.2) - any other status, or a connection failure, is a
/// delivery failure and goes through the normal retry/backoff path.
pub struct RealWakeDispatcher;

impl WakeDispatcher for RealWakeDispatcher {
    fn deliver(&self, endpoint: &AgentEndpoint, event: &WakeEvent) -> Result<(), String> {
        match endpoint.transport.as_str() {
            "local_process" => deliver_local_process(endpoint, event),
            "loopback_http" => deliver_loopback_http(endpoint, event),
            other => Err(format!(
                "LOCAL_PROCESS_NOT_ALLOWLISTED: unsupported transport '{other}' for agent endpoint {}",
                endpoint.agent_id
            )),
        }
    }
}

fn deliver_local_process(endpoint: &AgentEndpoint, event: &WakeEvent) -> Result<(), String> {
    std::process::Command::new(&endpoint.endpoint)
        .arg("--wake-event")
        .arg(&event.event_id)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("failed to spawn {}: {e}", endpoint.endpoint))
}

fn deliver_loopback_http(endpoint: &AgentEndpoint, event: &WakeEvent) -> Result<(), String> {
    let body = serde_json::to_string(event).map_err(|e| e.to_string())?;
    let mut request = ureq::post(&endpoint.endpoint).timeout(Duration::from_secs(5));
    if let Some(token) = &endpoint.auth_ref {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    match request.send_string(&body) {
        Ok(response) if response.status() == 202 => Ok(()),
        Ok(response) => Err(format!("agent endpoint responded {} (expected 202 Accepted, §9.2)", response.status())),
        Err(ureq::Error::Status(code, _)) => Err(format!("agent endpoint responded {code} (expected 202 Accepted, §9.2)")),
        Err(e) => Err(format!("delivery request failed: {e}")),
    }
}

pub struct WakeDeliveryHandle {
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WakeDeliveryHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn interruptible_sleep(interval: Duration, stop_flag: &AtomicBool) -> bool {
    let step = Duration::from_millis(200);
    let mut waited = Duration::ZERO;
    while waited < interval {
        if stop_flag.load(Ordering::SeqCst) {
            return false;
        }
        std::thread::sleep(step.min(interval - waited));
        waited += step;
    }
    true
}

/// §9.1/§23's "併發限制" (concurrency limit): the max WakeEvents one
/// evaluation tick will process. This is a single poll loop, not a thread
/// pool, so "at most N in-flight per tick" is the honest shape that
/// bounding takes here - a real cap on how many process spawns / outbound
/// HTTP calls happen in a short window, without literal multi-threaded
/// dispatch.
const MAX_PER_TICK: i64 = 5;

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// One evaluation pass. Pulled out of the loop body so tests can call it
/// directly without waiting on a real background thread - same pattern as
/// trigger_engine.rs's evaluate_once.
fn evaluate_once(store: &Mutex<Store>, dispatcher: &dyn WakeDispatcher, now_iso: &str) -> Vec<WakeEvent> {
    let dispatch_allowed = match store.lock() {
        Ok(s) => s.get_settings().map(|st| st.is_granted("agent_wake.dispatch")).unwrap_or(false),
        Err(_) => false,
    };
    if !dispatch_allowed {
        return Vec::new();
    }

    let due = match store.lock() {
        Ok(s) => s.due_for_delivery(now_iso, MAX_PER_TICK).unwrap_or_default(),
        Err(_) => return Vec::new(),
    };

    let mut delivered = Vec::new();
    for event in due {
        let claimed = match store.lock() {
            Ok(s) => s.mark_wake_event_delivering(&event.event_id),
            Err(_) => continue,
        };
        let Ok(claimed_event) = claimed else { continue };

        let endpoint = match store.lock() {
            Ok(s) => s.get_agent_endpoint(&claimed_event.agent_id),
            Err(_) => continue,
        };
        let Ok(endpoint) = endpoint else { continue };

        let result = dispatcher.deliver(&endpoint, &claimed_event);
        let outcome = match store.lock() {
            Ok(s) => match result {
                Ok(()) => s.record_delivery_success(&claimed_event.event_id),
                Err(e) => s.record_delivery_failure(&claimed_event.event_id, &e),
            },
            Err(_) => continue,
        };
        if let Ok(ev) = outcome {
            if ev.status == WakeEventStatus::Delivered {
                delivered.push(ev);
            }
        }
    }
    delivered
}

pub fn start(store: Arc<Mutex<Store>>, dispatcher: Arc<dyn WakeDispatcher>, interval_s: u64) -> WakeDeliveryHandle {
    let interval_s = interval_s.max(1);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();

    let thread = std::thread::spawn(move || {
        let interval = Duration::from_secs(interval_s);
        loop {
            if !interruptible_sleep(interval, &stop_flag_thread) {
                return;
            }
            evaluate_once(&store, dispatcher.as_ref(), &now_iso());
        }
    });

    WakeDeliveryHandle { stop_flag, thread: Some(thread) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctcl_store::{ActionKind, Operator, TriggerAction, TriggerKind};

    struct FakeWakeDispatcher {
        calls: Mutex<Vec<(String, String)>>, // (agent_id, event_id)
        should_succeed: bool,
    }
    impl FakeWakeDispatcher {
        fn new(should_succeed: bool) -> Self {
            FakeWakeDispatcher { calls: Mutex::new(Vec::new()), should_succeed }
        }
    }
    impl WakeDispatcher for FakeWakeDispatcher {
        fn deliver(&self, endpoint: &AgentEndpoint, event: &WakeEvent) -> Result<(), String> {
            self.calls.lock().unwrap().push((endpoint.agent_id.clone(), event.event_id.clone()));
            if self.should_succeed { Ok(()) } else { Err("simulated failure".to_string()) }
        }
    }

    fn store_with_dispatch_allowed_and_endpoint(agent_id: &str) -> Store {
        let store = Store::open(":memory:").unwrap();
        let mut settings = store.get_settings().unwrap();
        settings.scopes.insert("agent_wake.dispatch".to_string(), true);
        store.save_settings(&settings).unwrap();
        store.create_agent_endpoint(agent_id, "loopback_http", "http://127.0.0.1:4400/wake", None, &[]).unwrap();
        store.set_agent_endpoint_enabled(agent_id, true).unwrap();
        store
    }

    #[test]
    fn a_due_event_is_delivered_and_marked_delivered() {
        let store = Mutex::new(store_with_dispatch_allowed_and_endpoint("agent:primary"));
        store.lock().unwrap().create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();

        let dispatcher = FakeWakeDispatcher::new(true);
        let delivered = evaluate_once(&store, &dispatcher, &now_iso());

        assert_eq!(delivered.len(), 1);
        assert_eq!(dispatcher.calls.lock().unwrap().len(), 1, "dispatcher must actually be called");
        assert_eq!(delivered[0].status, WakeEventStatus::Delivered);
    }

    #[test]
    fn without_agent_wake_dispatch_scope_nothing_is_delivered() {
        let store = Mutex::new(Store::open(":memory:").unwrap()); // agent_wake.dispatch NOT granted
        {
            let s = store.lock().unwrap();
            s.create_agent_endpoint("agent:primary", "loopback_http", "http://127.0.0.1:4400/wake", None, &[]).unwrap();
            s.set_agent_endpoint_enabled("agent:primary", true).unwrap();
            s.create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();
        }

        let dispatcher = FakeWakeDispatcher::new(true);
        let delivered = evaluate_once(&store, &dispatcher, &now_iso());

        assert!(delivered.is_empty());
        assert!(dispatcher.calls.lock().unwrap().is_empty(), "must not dispatch to anyone without the global agent_wake.dispatch scope, even with an enabled endpoint");
    }

    #[test]
    fn an_agent_with_no_registered_endpoint_is_left_for_polling() {
        let store = Mutex::new(Store::open(":memory:").unwrap());
        {
            let s = store.lock().unwrap();
            let mut settings = s.get_settings().unwrap();
            settings.scopes.insert("agent_wake.dispatch".to_string(), true);
            s.save_settings(&settings).unwrap();
            s.create_wake_event("agent:no-endpoint", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();
        }

        let dispatcher = FakeWakeDispatcher::new(true);
        let delivered = evaluate_once(&store, &dispatcher, &now_iso());

        assert!(delivered.is_empty(), "active delivery is additive - an agent with no endpoint stays poll-only, not silently stuck");
        let ev = store.lock().unwrap().list_wake_events(Some("agent:no-endpoint"), None).unwrap();
        assert_eq!(ev[0].status, WakeEventStatus::Pending, "must remain pending, still pollable via the Local API");
    }

    #[test]
    fn a_failed_delivery_goes_to_retry_wait_not_lost() {
        let store = Mutex::new(store_with_dispatch_allowed_and_endpoint("agent:primary"));
        let ev = store.lock().unwrap().create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();

        let dispatcher = FakeWakeDispatcher::new(false);
        let delivered = evaluate_once(&store, &dispatcher, &now_iso());

        assert!(delivered.is_empty());
        let after = store.lock().unwrap().get_wake_event(&ev.event_id).unwrap();
        assert_eq!(after.status, WakeEventStatus::RetryWait);
        assert_eq!(after.attempt_count, 1);
    }

    #[test]
    fn agent_wake_trigger_firing_then_active_delivery_end_to_end() {
        // proves the full pipeline: Trigger -> WakeEvent (Phase 4.5A) -> active delivery (Phase 4.5D)
        let store = Mutex::new(store_with_dispatch_allowed_and_endpoint("agent:primary"));
        {
            let s = store.lock().unwrap();
            let action = TriggerAction { kind: ActionKind::AgentWake, target: "agent:primary".to_string() };
            let t = s.create_trigger("trigger:t", TriggerKind::CommonInstant, None, Operator::Ge, 100.0, action).unwrap();
            s.create_wake_event_from_trigger(&t, 150.0).unwrap();
        }

        let dispatcher = FakeWakeDispatcher::new(true);
        let delivered = evaluate_once(&store, &dispatcher, &now_iso());
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].trigger_id.as_deref(), Some("trigger:t"));
    }

    #[test]
    fn real_local_process_dispatcher_spawns_the_registered_executable() {
        let store = store_with_dispatch_allowed_and_endpoint("agent:primary");
        // re-register agent:primary as local_process, pointed at this very
        // test binary - a real executable guaranteed to exist. Passing it
        // unrecognized args (--wake-event <uuid>) just makes it exit fast
        // with no tests selected; this test only cares that spawning itself
        // succeeds, not what the child process does.
        let exe = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        store.create_agent_endpoint("agent:primary", "local_process", &exe, None, &[]).unwrap();
        store.set_agent_endpoint_enabled("agent:primary", true).unwrap();
        let endpoint = store.get_agent_endpoint("agent:primary").unwrap();
        let event = store.create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();

        let result = RealWakeDispatcher.deliver(&endpoint, &event);
        assert!(result.is_ok(), "spawning a real, registered executable must succeed: {result:?}");
    }

    #[test]
    fn real_local_process_dispatcher_fails_loudly_on_a_bad_path() {
        // build a valid-looking but nonexistent-at-dispatch-time AgentEndpoint
        // by hand (create_agent_endpoint itself already rejects a bad path at
        // registration time - this proves the dispatcher's own error path too).
        let endpoint = AgentEndpoint {
            agent_id: "agent:primary".to_string(),
            transport: "local_process".to_string(),
            endpoint: "C:/definitely/not/a/real/executable.exe".to_string(),
            auth_ref: None,
            enabled: true,
            allowed_event_kinds: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let event = Store::open(":memory:").unwrap().create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();
        let result = RealWakeDispatcher.deliver(&endpoint, &event);
        assert!(result.is_err());
    }

    #[test]
    fn real_loopback_http_dispatcher_succeeds_on_a_genuine_202() {
        let server = tiny_http::Server::http("127.0.0.1:4520").unwrap();
        let received_auth = Arc::new(Mutex::new(None));
        let received_auth_thread = received_auth.clone();
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            let auth = request.headers().iter().find(|h| h.field.equiv("Authorization")).map(|h| h.value.as_str().to_string());
            *received_auth_thread.lock().unwrap() = auth;
            let _ = request.respond(tiny_http::Response::from_string("ok").with_status_code(202));
        });

        let endpoint = AgentEndpoint {
            agent_id: "agent:primary".to_string(),
            transport: "loopback_http".to_string(),
            endpoint: "http://127.0.0.1:4520/wake".to_string(),
            auth_ref: Some("secret-token".to_string()),
            enabled: true,
            allowed_event_kinds: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let event = Store::open(":memory:").unwrap().create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();

        let result = RealWakeDispatcher.deliver(&endpoint, &event);
        handle.join().unwrap();

        assert!(result.is_ok(), "a genuine 202 response must count as delivered: {result:?}");
        assert_eq!(received_auth.lock().unwrap().as_deref(), Some("Bearer secret-token"), "the endpoint's auth_ref must be sent as a real Bearer header (§9.2)");
    }

    #[test]
    fn real_loopback_http_dispatcher_rejects_a_non_202_status() {
        let server = tiny_http::Server::http("127.0.0.1:4521").unwrap();
        let handle = std::thread::spawn(move || {
            let request = server.recv().unwrap();
            let _ = request.respond(tiny_http::Response::from_string("ok").with_status_code(200));
        });

        let endpoint = AgentEndpoint {
            agent_id: "agent:primary".to_string(),
            transport: "loopback_http".to_string(),
            endpoint: "http://127.0.0.1:4521/wake".to_string(),
            auth_ref: None,
            enabled: true,
            allowed_event_kinds: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let event = Store::open(":memory:").unwrap().create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();

        let result = RealWakeDispatcher.deliver(&endpoint, &event);
        handle.join().unwrap();
        assert!(result.is_err(), "§9.2: ONLY 202 Accepted counts as delivered, not any other 2xx");
    }

    #[test]
    fn real_loopback_http_dispatcher_fails_on_connection_refused() {
        let endpoint = AgentEndpoint {
            agent_id: "agent:primary".to_string(),
            transport: "loopback_http".to_string(),
            endpoint: "http://127.0.0.1:4522/wake".to_string(), // nothing listening
            auth_ref: None,
            enabled: true,
            allowed_event_kinds: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        let event = Store::open(":memory:").unwrap().create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();
        let result = RealWakeDispatcher.deliver(&endpoint, &event);
        assert!(result.is_err());
    }

    #[test]
    fn background_thread_actually_delivers_a_due_event_over_real_time() {
        let store = Arc::new(Mutex::new(store_with_dispatch_allowed_and_endpoint("agent:primary")));
        store.lock().unwrap().create_wake_event("agent:primary", None, "r", serde_json::json!({}), serde_json::json!({}), serde_json::json!({}), "k1").unwrap();

        let dispatcher = Arc::new(FakeWakeDispatcher::new(true));
        let handle = start(store.clone(), dispatcher.clone(), 1); // 1s interval
        std::thread::sleep(Duration::from_millis(1500));
        drop(handle);

        assert_eq!(dispatcher.calls.lock().unwrap().len(), 1);
        let ev = store.lock().unwrap().list_wake_events(Some("agent:primary"), None).unwrap();
        assert_eq!(ev[0].status, WakeEventStatus::Delivered);
    }
}
