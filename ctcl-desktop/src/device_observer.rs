//! Phase 3 Device Clock Observer background thread. Samples the system wall
//! clock and monotonic clock on a fixed interval, classifies the gap via
//! ctcl_store::device_observer::classify_gap, and persists only anomalies
//! (drift/sleep-wake/rollback) - same "only what's real" discipline as
//! local_api.rs's audit log. The current (possibly "everything is fine")
//! status is kept in memory (`last`) so a Tauri command can read it live
//! without waiting for the next anomaly.

use ctcl_store::{device_observer::EventKind, Store};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Serialize)]
pub struct LastSample {
    pub at: String,
    pub kind: EventKind,
    pub delta_ms: i64,
    pub wall_gap_ms: i64,
    pub mono_gap_ms: i64,
}

pub struct ObserverHandle {
    stop_flag: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub last: Arc<Mutex<Option<LastSample>>>,
}

impl Drop for ObserverHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Sleep in short steps so `stop_flag` is checked responsively even when
/// `interval` itself is long - mirrors local_api.rs's 200ms poll granularity.
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

pub fn start(store: Arc<Mutex<Store>>, interval_s: u64, drift_warn_s: f64) -> ObserverHandle {
    let interval_s = interval_s.max(1);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let last = Arc::new(Mutex::new(None));
    let stop_flag_thread = stop_flag.clone();
    let last_thread = last.clone();

    let thread = std::thread::spawn(move || {
        let interval = Duration::from_secs(interval_s);
        let mut prev: Option<(SystemTime, Instant)> = None;
        loop {
            if !interruptible_sleep(interval, &stop_flag_thread) {
                return;
            }
            let wall_now = SystemTime::now();
            let mono_now = Instant::now();

            if let Some((prev_wall, prev_mono)) = prev {
                let wall_gap_ms: i64 = match wall_now.duration_since(prev_wall) {
                    Ok(d) => d.as_millis() as i64,
                    Err(e) => -(e.duration().as_millis() as i64), // wall clock went backward
                };
                let mono_gap_ms = mono_now.duration_since(prev_mono).as_millis() as i64;
                let (kind, delta_ms) = ctcl_store::device_observer::classify_gap(wall_gap_ms, mono_gap_ms, interval_s, drift_warn_s);

                let at = chrono::Utc::now().to_rfc3339();
                *last_thread.lock().unwrap() = Some(LastSample { at, kind, delta_ms, wall_gap_ms, mono_gap_ms });

                if kind != EventKind::Normal {
                    if let Ok(s) = store.lock() {
                        let _ = s.log_device_event(kind, delta_ms, wall_gap_ms, mono_gap_ms);
                    }
                }
            } else {
                // First tick: nothing to compare against yet, just record that
                // the observer is alive.
                *last_thread.lock().unwrap() = Some(LastSample {
                    at: chrono::Utc::now().to_rfc3339(),
                    kind: EventKind::Normal,
                    delta_ms: 0,
                    wall_gap_ms: 0,
                    mono_gap_ms: 0,
                });
            }
            prev = Some((wall_now, mono_now));
        }
    });

    ObserverHandle { stop_flag, thread: Some(thread), last }
}

#[cfg(test)]
mod tests {
    //! Real background-thread tests across real wall-clock time (short
    //! intervals), not mocked - proves start()/stop() and the sampling loop
    //! actually run, the same discipline as local_api.rs's socket-level tests.
    use super::*;

    #[test]
    fn samples_and_reports_normal_status_over_real_time() {
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        let handle = start(store, 1, 5.0); // 1s interval, 5s drift threshold
        std::thread::sleep(Duration::from_millis(2600)); // covers >= 2 ticks
        let last = handle.last.lock().unwrap().clone();
        assert!(last.is_some(), "observer should have recorded at least one sample by now");
        assert_eq!(last.unwrap().kind, EventKind::Normal, "an idle machine with a 1s interval should read as normal");
    }

    #[test]
    fn stopping_the_handle_joins_the_thread_cleanly() {
        let store = Arc::new(Mutex::new(Store::open(":memory:").unwrap()));
        let handle = start(store, 1, 5.0);
        std::thread::sleep(Duration::from_millis(150));
        drop(handle); // Drop must not hang - proves the stop_flag/join path works
    }

    #[test]
    fn a_synthetic_long_gap_is_persisted_as_sleep_wake() {
        // Exercises the real persistence path (not just classify_gap in
        // isolation): feed the pure classifier a whitepaper-scale gap and
        // confirm log_device_event + list_device_events round-trip it.
        let store = Store::open(":memory:").unwrap();
        let (kind, delta) = ctcl_store::device_observer::classify_gap(7_200_000, 7_200_000, 20, 5.0);
        store.log_device_event(kind, delta, 7_200_000, 7_200_000).unwrap();
        let events = store.list_device_events(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "sleep_wake");
    }
}
