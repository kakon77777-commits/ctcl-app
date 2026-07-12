//! Device Clock Observer persistence (whitepaper §4.2 / Phase 3): pure gap
//! classification plus a SQLite log of anomalies (drift, sleep/wake, rollback).
//!
//! Classification is a self-consistency check, not a network time comparison:
//! the sampling thread (in ctcl-desktop) wakes on a fixed interval and captures
//! (wall clock, monotonic clock) pairs. In the *normal* case both gaps roughly
//! equal the requested interval. Two things can go wrong between samples:
//!
//! 1. The wall clock moved far more than the monotonic clock did relative to
//!    what was requested - either the process was suspended for a long stretch
//!    (sleep/wake, deep background throttling, long-term offline), caught by
//!    comparing wall_gap against the requested interval directly, or
//! 2. Within a normal-length gap, the wall clock still disagrees with the
//!    monotonic clock's elapsed time by more than a threshold - either a
//!    forward jump (drift, e.g. NTP correction or manual clock set forward) or
//!    a backward jump (rollback, e.g. manual clock set backward).
//!
//! Deciding (1) before (2) sidesteps a real cross-platform inconsistency:
//! whether a monotonic clock's elapsed reading includes suspended time varies
//! by OS. Using wall_gap-vs-requested-interval for the long-gap case avoids
//! depending on that platform-specific behavior at all.

use crate::{Store, StoreError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    Normal,
    Drift,
    SleepWake,
    Rollback,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::Normal => "normal",
            EventKind::Drift => "drift",
            EventKind::SleepWake => "sleep_wake",
            EventKind::Rollback => "rollback",
        }
    }
}

/// A wall-clock gap far exceeding the requested sampling interval means the
/// device (or at least this process) was not continuously running.
fn sleep_wake_threshold_ms(interval_s: u64) -> i64 {
    let interval_ms = (interval_s as i64).saturating_mul(1000);
    interval_ms.saturating_mul(3).max(interval_ms.saturating_add(30_000))
}

/// Below this, a negative delta is just clock-smoothing jitter (e.g. gradual
/// NTP correction), not a "someone set the clock backward" rollback.
const ROLLBACK_SLACK_MS: i64 = 500;

/// Classify what happened between two samples. Returns (kind, delta_ms) where
/// delta_ms = wall_gap_ms - mono_gap_ms: how much the wall clock moved beyond
/// what monotonic progression accounts for (positive = wall ran ahead).
pub fn classify_gap(wall_gap_ms: i64, mono_gap_ms: i64, interval_s: u64, drift_warn_s: f64) -> (EventKind, i64) {
    let delta_ms = wall_gap_ms - mono_gap_ms;
    if wall_gap_ms > sleep_wake_threshold_ms(interval_s) {
        return (EventKind::SleepWake, delta_ms);
    }
    if delta_ms < -ROLLBACK_SLACK_MS {
        return (EventKind::Rollback, delta_ms);
    }
    let drift_warn_ms = (drift_warn_s.max(0.0) * 1000.0).round() as i64;
    if delta_ms.abs() > drift_warn_ms {
        return (EventKind::Drift, delta_ms);
    }
    (EventKind::Normal, delta_ms)
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceEvent {
    pub id: i64,
    pub at: String,
    pub kind: String,
    pub delta_ms: i64,
    pub wall_gap_ms: i64,
    pub mono_gap_ms: i64,
}

impl Store {
    /// Only anomalies are persisted (kind != Normal) - a routine 20-second
    /// heartbeat would otherwise fill the database with rows nobody needs to
    /// see. The observer's *live* current status (including "everything is
    /// fine") is a separate, in-memory concern in ctcl-desktop.
    pub fn log_device_event(&self, kind: EventKind, delta_ms: i64, wall_gap_ms: i64, mono_gap_ms: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO device_events (at, kind, delta_ms, wall_gap_ms, mono_gap_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), kind.as_str(), delta_ms, wall_gap_ms, mono_gap_ms],
        )?;
        Ok(())
    }

    pub fn list_device_events(&self, limit: u32) -> Result<Vec<DeviceEvent>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, at, kind, delta_ms, wall_gap_ms, mono_gap_ms FROM device_events ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit], |row| {
                Ok(DeviceEvent {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    kind: row.get(2)?,
                    delta_ms: row.get(3)?,
                    wall_gap_ms: row.get(4)?,
                    mono_gap_ms: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- classify_gap: pure logic, synthetic gaps, no real sleep needed ----

    #[test]
    fn normal_gap_matching_the_requested_interval() {
        let (kind, delta) = classify_gap(20_000, 20_000, 20, 5.0);
        assert_eq!(kind, EventKind::Normal);
        assert_eq!(delta, 0);
    }

    #[test]
    fn small_jitter_within_drift_threshold_is_still_normal() {
        let (kind, _) = classify_gap(20_300, 20_000, 20, 5.0); // 0.3s off, threshold 5s
        assert_eq!(kind, EventKind::Normal);
    }

    #[test]
    fn wall_clock_running_ahead_beyond_threshold_is_drift() {
        let (kind, delta) = classify_gap(26_000, 20_000, 20, 5.0); // 6s ahead, threshold 5s
        assert_eq!(kind, EventKind::Drift);
        assert_eq!(delta, 6_000);
    }

    #[test]
    fn wall_clock_set_backward_is_rollback() {
        let (kind, delta) = classify_gap(15_000, 20_000, 20, 5.0); // wall is 5s BEHIND expected
        assert_eq!(kind, EventKind::Rollback);
        assert_eq!(delta, -5_000);
    }

    #[test]
    fn tiny_negative_delta_is_jitter_not_rollback() {
        let (kind, _) = classify_gap(19_800, 20_000, 20, 5.0); // 0.2s behind, under the 500ms slack
        assert_eq!(kind, EventKind::Normal);
    }

    #[test]
    fn a_huge_wall_gap_is_sleep_wake_regardless_of_monotonic_reading() {
        // Simulates a 2-hour suspend where the monotonic clock (per this
        // platform's semantics) also advanced ~2h - still must be sleep_wake,
        // not "drift", because 2h vastly exceeds the requested 20s interval.
        let (kind, _) = classify_gap(7_200_000, 7_200_000, 20, 5.0);
        assert_eq!(kind, EventKind::SleepWake);
    }

    #[test]
    fn a_huge_wall_gap_with_small_monotonic_gap_is_still_sleep_wake() {
        // The Linux CLOCK_MONOTONIC case: monotonic excludes suspended time,
        // so mono_gap looks almost normal while wall_gap does not.
        let (kind, _) = classify_gap(7_200_000, 20_000, 20, 5.0);
        assert_eq!(kind, EventKind::SleepWake);
    }

    #[test]
    fn sleep_wake_check_uses_wall_gap_not_the_delta() {
        // A long gap where BOTH clocks agree closely (delta near zero) must
        // still be sleep_wake, not normal - length alone is the signal here.
        let (kind, delta) = classify_gap(600_000, 599_800, 20, 5.0);
        assert_eq!(kind, EventKind::SleepWake);
        assert_eq!(delta, 200);
    }

    // ---- persistence ----

    #[test]
    fn only_anomalies_are_persisted_normal_is_not() {
        let store = Store::open(":memory:").unwrap();
        store.log_device_event(EventKind::Drift, 6_000, 26_000, 20_000).unwrap();
        let events = store.list_device_events(10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "drift");
        assert_eq!(events[0].delta_ms, 6_000);
    }

    #[test]
    fn events_list_most_recent_first_and_respect_limit() {
        let store = Store::open(":memory:").unwrap();
        store.log_device_event(EventKind::Drift, 6_000, 26_000, 20_000).unwrap();
        store.log_device_event(EventKind::Rollback, -5_000, 15_000, 20_000).unwrap();
        store.log_device_event(EventKind::SleepWake, 0, 7_200_000, 7_200_000).unwrap();

        let all = store.list_device_events(10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].kind, "sleep_wake"); // most recent first

        let limited = store.list_device_events(1).unwrap();
        assert_eq!(limited.len(), 1);
    }
}
