//! Phase 4 Trigger Engine background thread. Polls
//! `ctcl_store::trigger::Store::due_triggers` on a fixed interval and
//! dispatches each due trigger's action through an `ActionDispatcher` -
//! pluggable so tests never actually open a URI or spawn an OS process
//! (`FakeDispatcher` just records the call), while the real desktop app uses
//! `RealDispatcher`.
//!
//! A trigger is marked fired ONLY after a successful dispatch - if opening a
//! callback URI fails, the trigger stays active and is retried next tick,
//! rather than being silently marked "fired" without the action happening.
//!
//! `ActionKind::AgentWake` is a special case (whitepaper
//! CTCL_Agent_Wake_MCP_Temporal_Runtime §7, Phase 4.5A): it never reaches
//! `ActionDispatcher` at all. `evaluate_once` intercepts it before dispatch
//! and calls `Store::create_wake_event_from_trigger` directly, so a due
//! agent_wake trigger produces a persisted WakeEvent instead of OS-level I/O.
//! Same "mark fired only on success" retry semantics apply either way.

use ctcl_store::{ActionKind, Store, Trigger, TriggerAction};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub trait ActionDispatcher: Send + Sync {
    fn dispatch(&self, action: &TriggerAction) -> Result<(), String>;
}

/// notification: logged (no OS toast integration yet - honestly not claimed).
/// callback: hands the URI to the OS's own default handler for that scheme
/// (`start` on Windows, `open` on macOS, `xdg-open` on Linux). CTCL does not
/// register or resolve schemes itself - whatever app owns that scheme decides
/// what happens next, matching §7.1's "private scheme only" scope today.
pub struct RealDispatcher;

impl ActionDispatcher for RealDispatcher {
    fn dispatch(&self, action: &TriggerAction) -> Result<(), String> {
        match action.kind {
            ActionKind::Notification => {
                eprintln!("[ctcl trigger] notification: {}", action.target);
                Ok(())
            }
            ActionKind::Callback => open_uri(&action.target),
            // Intercepted earlier in evaluate_once() and never dispatched
            // here - see this module's top doc comment. If this arm is ever
            // reached it's a bug in that interception, so fail loudly rather
            // than silently no-op'ing or opening a URI for an agent id.
            ActionKind::AgentWake => Err("agent_wake actions are handled by the trigger engine directly, not ActionDispatcher".to_string()),
        }
    }
}

#[cfg(target_os = "windows")]
fn open_uri(uri: &str) -> Result<(), String> {
    std::process::Command::new("cmd").args(["/C", "start", "", uri]).spawn().map(|_| ()).map_err(|e| e.to_string())
}
#[cfg(target_os = "macos")]
fn open_uri(uri: &str) -> Result<(), String> {
    std::process::Command::new("open").arg(uri).spawn().map(|_| ()).map_err(|e| e.to_string())
}
#[cfg(all(unix, not(target_os = "macos")))]
fn open_uri(uri: &str) -> Result<(), String> {
    std::process::Command::new("xdg-open").arg(uri).spawn().map(|_| ()).map_err(|e| e.to_string())
}

pub struct TriggerEngineHandle {
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for TriggerEngineHandle {
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

fn now_unix_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// One evaluation pass: find due triggers, dispatch each, mark fired only on
/// success. Pulled out of the loop body so tests can call it directly without
/// waiting on a real background thread.
fn evaluate_once(store: &Mutex<Store>, dispatcher: &dyn ActionDispatcher, now_s: f64) -> Vec<Trigger> {
    let due = match store.lock() {
        Ok(s) => s.due_triggers(now_s).unwrap_or_default(),
        Err(_) => return Vec::new(),
    };
    let mut fired = Vec::new();
    for t in due {
        let dispatched_ok = if t.action.kind == ActionKind::AgentWake {
            match store.lock() {
                Ok(s) => s.create_wake_event_from_trigger(&t, now_s).is_ok(),
                Err(_) => false,
            }
        } else {
            dispatcher.dispatch(&t.action).is_ok()
        };
        if dispatched_ok {
            if let Ok(s) = store.lock() {
                let _ = s.mark_fired(&t.id);
            }
            fired.push(t);
        }
    }
    fired
}

pub fn start(store: Arc<Mutex<Store>>, dispatcher: Arc<dyn ActionDispatcher>, interval_s: u64) -> TriggerEngineHandle {
    let interval_s = interval_s.max(1);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();

    let thread = std::thread::spawn(move || {
        let interval = Duration::from_secs(interval_s);
        loop {
            if !interruptible_sleep(interval, &stop_flag_thread) {
                return;
            }
            evaluate_once(&store, dispatcher.as_ref(), now_unix_s());
        }
    });

    TriggerEngineHandle { stop_flag, thread: Some(thread) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctcl_store::{Operator, TriggerKind};

    struct FakeDispatcher {
        calls: Mutex<Vec<TriggerAction>>,
    }
    impl FakeDispatcher {
        fn new() -> Self {
            FakeDispatcher { calls: Mutex::new(Vec::new()) }
        }
    }
    impl ActionDispatcher for FakeDispatcher {
        fn dispatch(&self, action: &TriggerAction) -> Result<(), String> {
            self.calls.lock().unwrap().push(action.clone());
            Ok(())
        }
    }

    struct AlwaysFailsDispatcher;
    impl ActionDispatcher for AlwaysFailsDispatcher {
        fn dispatch(&self, _action: &TriggerAction) -> Result<(), String> {
            Err("simulated failure".into())
        }
    }

    fn notify(msg: &str) -> TriggerAction {
        TriggerAction { kind: ActionKind::Notification, target: msg.to_string() }
    }

    #[test]
    fn a_due_trigger_is_dispatched_and_marked_fired() {
        let store = Mutex::new(Store::open(":memory:").unwrap());
        store.lock().unwrap().create_trigger("t", TriggerKind::CommonInstant, None, Operator::Ge, 100.0, notify("hi")).unwrap();

        let dispatcher = FakeDispatcher::new();
        let fired = evaluate_once(&store, &dispatcher, 150.0);

        assert_eq!(fired.len(), 1);
        assert_eq!(dispatcher.calls.lock().unwrap().len(), 1, "dispatcher must actually be called");
        assert_eq!(store.lock().unwrap().get_trigger("t").unwrap().status, ctcl_store::TriggerStatus::Fired);
    }

    #[test]
    fn a_trigger_not_yet_due_is_left_alone() {
        let store = Mutex::new(Store::open(":memory:").unwrap());
        store.lock().unwrap().create_trigger("t", TriggerKind::CommonInstant, None, Operator::Ge, 1000.0, notify("hi")).unwrap();

        let dispatcher = FakeDispatcher::new();
        let fired = evaluate_once(&store, &dispatcher, 500.0);

        assert!(fired.is_empty());
        assert!(dispatcher.calls.lock().unwrap().is_empty());
        assert_eq!(store.lock().unwrap().get_trigger("t").unwrap().status, ctcl_store::TriggerStatus::Active);
    }

    #[test]
    fn a_failed_dispatch_leaves_the_trigger_active_for_retry() {
        let store = Mutex::new(Store::open(":memory:").unwrap());
        store.lock().unwrap().create_trigger("t", TriggerKind::CommonInstant, None, Operator::Ge, 100.0, notify("hi")).unwrap();

        let fired = evaluate_once(&store, &AlwaysFailsDispatcher, 150.0);

        assert!(fired.is_empty(), "a failed dispatch must not be reported as fired");
        assert_eq!(
            store.lock().unwrap().get_trigger("t").unwrap().status,
            ctcl_store::TriggerStatus::Active,
            "must stay active so it retries next tick rather than being silently lost"
        );
    }

    #[test]
    fn a_due_agent_wake_trigger_creates_a_wake_event_instead_of_dispatching() {
        let store = Mutex::new(Store::open(":memory:").unwrap());
        let action = TriggerAction { kind: ActionKind::AgentWake, target: "agent:primary".to_string() };
        store.lock().unwrap().create_trigger("t", TriggerKind::CommonInstant, None, Operator::Ge, 100.0, action).unwrap();

        let dispatcher = FakeDispatcher::new();
        let fired = evaluate_once(&store, &dispatcher, 150.0);

        assert_eq!(fired.len(), 1);
        assert!(dispatcher.calls.lock().unwrap().is_empty(), "agent_wake must never reach ActionDispatcher");
        assert_eq!(store.lock().unwrap().get_trigger("t").unwrap().status, ctcl_store::TriggerStatus::Fired);

        let events = store.lock().unwrap().list_wake_events(Some("agent:primary"), None).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].trigger_id.as_deref(), Some("t"));
        assert_eq!(events[0].status, ctcl_store::WakeEventStatus::Pending);
    }

    #[test]
    fn real_dispatcher_rejects_agent_wake_if_it_is_ever_reached_directly() {
        let action = TriggerAction { kind: ActionKind::AgentWake, target: "agent:primary".to_string() };
        assert!(RealDispatcher.dispatch(&action).is_err(), "AgentWake reaching the dispatcher directly is an invariant violation, not a silent no-op");
    }

    #[test]
    fn background_thread_actually_fires_a_due_trigger_over_real_time() {
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        // target in the past (now_unix_s() at creation time minus 1s) so the very first tick fires it
        let target = now_unix_s() - 1.0;
        store.lock().unwrap().create_trigger("t", TriggerKind::CommonInstant, None, Operator::Ge, target, notify("hi")).unwrap();

        let dispatcher = Arc::new(FakeDispatcher::new());
        let handle = start(store.clone(), dispatcher.clone(), 1); // 1s interval
        std::thread::sleep(Duration::from_millis(1500)); // let it tick at least once
        drop(handle);

        assert_eq!(dispatcher.calls.lock().unwrap().len(), 1);
        assert_eq!(store.lock().unwrap().get_trigger("t").unwrap().status, ctcl_store::TriggerStatus::Fired);
    }
}
