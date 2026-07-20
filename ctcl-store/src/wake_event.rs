//! WakeEvent Core (Phase 4.5A, whitepaper CTCL_Agent_Wake_MCP_Temporal_Runtime
//! §5-6): the persisted, immutable record produced when an `agent_wake`
//! trigger fires. This is deliberately a SEPARATE concept from a Trigger
//! firing (§5.3): `TriggerDue != WakeEventCreated != Delivered != Acknowledged
//! != Acted`. Phase 4.5A implements only the first and last of those five -
//! creation and manual acknowledgement - matching the whitepaper's own
//! staged scope exactly (delivery, decision receipts, and active dispatch to
//! an Agent Endpoint are later phases, not built here).
//!
//! CTCL's job stops at "reliably record that this fired, and let whoever asks
//! retrieve and acknowledge it." It does not decide what the agent does next,
//! and does not call any MCP tool on the agent's behalf - see trigger.rs's
//! `ActionKind::AgentWake` doc comment for why that boundary is deliberate.

use crate::trigger::{ActionKind, Trigger, TriggerKind};
use crate::{Store, StoreError};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeEventStatus {
    Pending,
    Delivering,
    Delivered,
    Acknowledged,
    DecidedNoAction,
    DecidedAction,
    Completed,
    RetryWait,
    DeadLetter,
}

impl WakeEventStatus {
    fn as_str(&self) -> &'static str {
        match self {
            WakeEventStatus::Pending => "pending",
            WakeEventStatus::Delivering => "delivering",
            WakeEventStatus::Delivered => "delivered",
            WakeEventStatus::Acknowledged => "acknowledged",
            WakeEventStatus::DecidedNoAction => "decided_no_action",
            WakeEventStatus::DecidedAction => "decided_action",
            WakeEventStatus::Completed => "completed",
            WakeEventStatus::RetryWait => "retry_wait",
            WakeEventStatus::DeadLetter => "dead_letter",
        }
    }
    fn from_str(s: &str) -> Self {
        match s {
            "delivering" => WakeEventStatus::Delivering,
            "delivered" => WakeEventStatus::Delivered,
            "acknowledged" => WakeEventStatus::Acknowledged,
            "decided_no_action" => WakeEventStatus::DecidedNoAction,
            "decided_action" => WakeEventStatus::DecidedAction,
            "completed" => WakeEventStatus::Completed,
            "retry_wait" => WakeEventStatus::RetryWait,
            "dead_letter" => WakeEventStatus::DeadLetter,
            _ => WakeEventStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeEvent {
    pub event_id: String,
    pub trigger_id: Option<String>,
    pub agent_id: String,
    pub reason: String,
    /// {"unix_s": .., "source": ..} - when the condition was satisfied and by
    /// what evaluated it. Free-form JSON per the whitepaper's own §5.2 schema.
    pub fired: serde_json::Value,
    /// {"operator": .., "target_value": .., "observed_value": ..}
    pub observed: serde_json::Value,
    /// Free-form, e.g. {"goal_refs": [...]}. §18.6: references only, never a
    /// full copy of memory/email/file content - that discipline is on the
    /// CALLER (whoever populates payload), not enforced here.
    pub payload: serde_json::Value,
    pub status: WakeEventStatus,
    pub attempt_count: i64,
    pub created_at: String,
    pub acknowledged_at: Option<String>,
    pub completed_at: Option<String>,
    /// Phase 4.5D: set when `record_delivery_success` transitions delivering -> delivered.
    pub delivered_at: Option<String>,
    /// Phase 4.5D: set by `record_delivery_failure` when retrying - the
    /// Wake Delivery Worker won't pick this event up again before this time.
    pub next_attempt_at: Option<String>,
    /// Phase 4.5D: the most recent delivery error, kept even after a
    /// successful retry or a dead-letter, for post-mortem visibility.
    pub last_error: Option<String>,
    pub idempotency_key: String,
}

/// §8.1 step 6: after this many failed delivery attempts, an event moves to
/// `dead_letter` instead of retrying again. Not user-configurable in this
/// phase - the whitepaper names the *mechanism* (exponential backoff, a cap)
/// without specifying an exact number.
const MAX_DELIVERY_ATTEMPTS: i64 = 5;
/// §8.2's d_0.
const BASE_DELAY_S: i64 = 5;
/// §8.2's d_max.
const MAX_DELAY_S: i64 = 300;

/// §8.2's epsilon - random jitter so multiple events retrying around the
/// same time don't all wake the delivery thread in lockstep. Not
/// cryptographic - just needs to differ call to call, so a `rand` crate
/// dependency isn't justified for it.
fn jitter_ms() -> i64 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos();
    (nanos % 1000) as i64
}

impl Store {
    /// Low-level create. Phase 4.5A's actual entry point is
    /// `create_wake_event_from_trigger` (called by ctcl-desktop's trigger
    /// engine) - this exists separately so a future MCP tool or manual API
    /// call can create a WakeEvent without first needing a Trigger to exist.
    pub fn create_wake_event(
        &self,
        agent_id: &str,
        trigger_id: Option<&str>,
        reason: &str,
        fired: serde_json::Value,
        observed: serde_json::Value,
        payload: serde_json::Value,
        idempotency_key: &str,
    ) -> Result<WakeEvent, StoreError> {
        if agent_id.trim().is_empty() {
            return Err(StoreError::InvalidInput("wake event agent_id must not be empty".into()));
        }
        let event_id = format!("wake:{}", uuid::Uuid::new_v4());
        let created_at = chrono::Utc::now().to_rfc3339();
        let fired_json = serde_json::to_string(&fired)?;
        let observed_json = serde_json::to_string(&observed)?;
        let payload_json = serde_json::to_string(&payload)?;
        // idempotency_key is UNIQUE - a duplicate create (e.g. a retried
        // evaluation pass after a partial failure) returns the EXISTING
        // event instead of erroring or silently creating a second one.
        let existing = self.get_wake_event_by_idempotency_key(idempotency_key)?;
        if let Some(ev) = existing {
            return Ok(ev);
        }
        self.conn.execute(
            "INSERT INTO wake_events (event_id, trigger_id, agent_id, reason, fired_json, observed_json, payload_json, status, attempt_count, created_at, acknowledged_at, completed_at, delivered_at, next_attempt_at, last_error, idempotency_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 0, ?8, NULL, NULL, NULL, NULL, NULL, ?9)",
            rusqlite::params![event_id, trigger_id, agent_id, reason, fired_json, observed_json, payload_json, created_at, idempotency_key],
        )?;
        self.get_wake_event(&event_id)
    }

    pub fn get_wake_event(&self, event_id: &str) -> Result<WakeEvent, StoreError> {
        self.conn
            .query_row(
                "SELECT event_id, trigger_id, agent_id, reason, fired_json, observed_json, payload_json, status, attempt_count, created_at, acknowledged_at, completed_at, delivered_at, next_attempt_at, last_error, idempotency_key
                 FROM wake_events WHERE event_id = ?1",
                [event_id],
                Self::row_to_wake_event,
            )
            .map_err(|_| StoreError::InvalidInput(format!("unknown wake event: {event_id}")))?
    }

    fn get_wake_event_by_idempotency_key(&self, key: &str) -> Result<Option<WakeEvent>, StoreError> {
        let result = self.conn.query_row(
            "SELECT event_id, trigger_id, agent_id, reason, fired_json, observed_json, payload_json, status, attempt_count, created_at, acknowledged_at, completed_at, delivered_at, next_attempt_at, last_error, idempotency_key
             FROM wake_events WHERE idempotency_key = ?1",
            [key],
            Self::row_to_wake_event,
        );
        match result {
            Ok(ev) => Ok(Some(ev?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// All WakeEvents, most recent first. `agent_id`/`status` filters are
    /// optional (§10.2's `GET /v1/wake-events?status=pending&agent_id=...`).
    pub fn list_wake_events(&self, agent_id: Option<&str>, status: Option<WakeEventStatus>) -> Result<Vec<WakeEvent>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT event_id, trigger_id, agent_id, reason, fired_json, observed_json, payload_json, status, attempt_count, created_at, acknowledged_at, completed_at, delivered_at, next_attempt_at, last_error, idempotency_key
             FROM wake_events ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], Self::row_to_wake_event)?.collect::<Result<Vec<_>, rusqlite::Error>>()?;
        let mut events = rows.into_iter().collect::<Result<Vec<_>, StoreError>>()?;
        if let Some(a) = agent_id {
            events.retain(|e| e.agent_id == a);
        }
        if let Some(s) = status {
            events.retain(|e| e.status == s);
        }
        Ok(events)
    }

    /// Manual ack: `pending` -> `acknowledged` (Phase 4.5A's Poll-only path)
    /// or, since Phase 4.5D, `delivered` -> `acknowledged` (the Active
    /// Delivery path - a WakeEvent pushed to a registered Agent Endpoint
    /// still needs an explicit ack once the agent has actually seen it, `202
    /// Accepted` from a loopback_http endpoint or a successful local_process
    /// spawn is NOT treated as acknowledgement, matching §9.2's explicit
    /// warning not to treat 202 as task completion). Anything else (already
    /// acknowledged, dead-lettered, etc.) is rejected rather than silently
    /// overwritten - an ack is a one-time signal that a real Agent Runtime
    /// actually picked this event up.
    pub fn ack_wake_event(&self, event_id: &str) -> Result<WakeEvent, StoreError> {
        let changed = self.conn.execute(
            "UPDATE wake_events SET status='acknowledged', acknowledged_at=?1 WHERE event_id=?2 AND status IN ('pending','delivered')",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), event_id],
        )?;
        if changed == 0 {
            let existing = self.get_wake_event(event_id)?;
            return Err(StoreError::InvalidInput(format!(
                "cannot acknowledge wake event {event_id}: status is '{}', not 'pending'/'delivered'",
                existing.status.as_str()
            )));
        }
        self.get_wake_event(event_id)
    }

    /// Phase 4.5B (§10.2 `POST /v1/wake-events/{event_id}/complete`):
    /// acknowledged -> completed only, same one-way-transition discipline as
    /// `ack_wake_event`. Requires a prior ack rather than completing straight
    /// from `pending` - the whitepaper's own state chain is
    /// pending->...->acknowledged->...->completed, and skipping ack would mean
    /// "completed" no longer implies "some Agent Runtime actually saw this."
    pub fn complete_wake_event(&self, event_id: &str) -> Result<WakeEvent, StoreError> {
        let changed = self.conn.execute(
            "UPDATE wake_events SET status='completed', completed_at=?1 WHERE event_id=?2 AND status='acknowledged'",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), event_id],
        )?;
        if changed == 0 {
            let existing = self.get_wake_event(event_id)?;
            return Err(StoreError::InvalidInput(format!(
                "cannot complete wake event {event_id}: status is '{}', not 'acknowledged'",
                existing.status.as_str()
            )));
        }
        self.get_wake_event(event_id)
    }

    /// Phase 4.5D (§8.1): events an active-delivery Wake Delivery Worker
    /// should attempt right now - `pending` (never tried) or `retry_wait`
    /// whose backoff has elapsed - for an agent_id with an ENABLED
    /// `agent_endpoints` row. An agent with no registered/enabled endpoint
    /// simply never appears here and stays reachable only by polling
    /// (Phase 4.5B) - active delivery is additive, not a replacement.
    /// `limit` bounds how many events one evaluation tick processes, which
    /// is this phase's concurrency limit (§9.1/§23's "併發限制"): a single
    /// poll loop, not a thread pool, so "at most N in-flight per tick" is
    /// the honest shape that control takes here.
    pub fn due_for_delivery(&self, now_iso: &str, limit: i64) -> Result<Vec<WakeEvent>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT we.event_id, we.trigger_id, we.agent_id, we.reason, we.fired_json, we.observed_json, we.payload_json, we.status, we.attempt_count, we.created_at, we.acknowledged_at, we.completed_at, we.delivered_at, we.next_attempt_at, we.last_error, we.idempotency_key
             FROM wake_events we
             JOIN agent_endpoints ae ON ae.agent_id = we.agent_id
             WHERE ae.enabled = 1
               AND (we.status = 'pending' OR (we.status = 'retry_wait' AND (we.next_attempt_at IS NULL OR we.next_attempt_at <= ?1)))
             ORDER BY we.created_at ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![now_iso, limit], Self::row_to_wake_event)?.collect::<Result<Vec<_>, rusqlite::Error>>()?;
        rows.into_iter().collect()
    }

    /// pending/retry_wait -> delivering, right before a dispatcher is
    /// actually invoked - claims the event so a second delivery-thread tick
    /// (or a future concurrent worker) doesn't double-dispatch it.
    pub fn mark_wake_event_delivering(&self, event_id: &str) -> Result<WakeEvent, StoreError> {
        let changed = self.conn.execute(
            "UPDATE wake_events SET status='delivering' WHERE event_id=?1 AND status IN ('pending','retry_wait')",
            [event_id],
        )?;
        if changed == 0 {
            let existing = self.get_wake_event(event_id)?;
            return Err(StoreError::InvalidInput(format!(
                "cannot start delivering wake event {event_id}: status is '{}', not 'pending'/'retry_wait'",
                existing.status.as_str()
            )));
        }
        self.get_wake_event(event_id)
    }

    /// delivering -> delivered. This is NOT the same as acknowledged (§9.2:
    /// a `202 Accepted` / a successful process spawn means "handed off,"
    /// not "the agent has acted" - `ack_wake_event` is the agent's own
    /// separate, later signal).
    pub fn record_delivery_success(&self, event_id: &str) -> Result<WakeEvent, StoreError> {
        let changed = self.conn.execute(
            "UPDATE wake_events SET status='delivered', delivered_at=?1 WHERE event_id=?2 AND status='delivering'",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), event_id],
        )?;
        if changed == 0 {
            let existing = self.get_wake_event(event_id)?;
            return Err(StoreError::InvalidInput(format!(
                "cannot mark wake event {event_id} delivered: status is '{}', not 'delivering'",
                existing.status.as_str()
            )));
        }
        self.get_wake_event(event_id)
    }

    /// delivering -> retry_wait (with §8.2's exponential-backoff-plus-jitter
    /// `next_attempt_at`) or, once `MAX_DELIVERY_ATTEMPTS` is reached,
    /// delivering -> dead_letter (§8.1 step 6). Owns both the backoff math
    /// and the give-up decision so `wake_delivery.rs` doesn't need to
    /// reimplement the retry policy - it just reports "this attempt failed."
    pub fn record_delivery_failure(&self, event_id: &str, error: &str) -> Result<WakeEvent, StoreError> {
        let current = self.get_wake_event(event_id)?;
        if current.status != WakeEventStatus::Delivering {
            return Err(StoreError::InvalidInput(format!(
                "cannot record a delivery failure for {event_id}: status is '{}', not 'delivering'",
                current.status.as_str()
            )));
        }
        let next_attempt_count = current.attempt_count + 1;
        if next_attempt_count >= MAX_DELIVERY_ATTEMPTS {
            self.conn.execute(
                "UPDATE wake_events SET status='dead_letter', attempt_count=?1, last_error=?2 WHERE event_id=?3",
                rusqlite::params![next_attempt_count, error, event_id],
            )?;
        } else {
            let delay_s = (BASE_DELAY_S * 2i64.pow(current.attempt_count as u32)).min(MAX_DELAY_S);
            let next_attempt_at = (chrono::Utc::now() + chrono::Duration::seconds(delay_s) + chrono::Duration::milliseconds(jitter_ms())).to_rfc3339();
            self.conn.execute(
                "UPDATE wake_events SET status='retry_wait', attempt_count=?1, next_attempt_at=?2, last_error=?3 WHERE event_id=?4",
                rusqlite::params![next_attempt_count, next_attempt_at, error, event_id],
            )?;
        }
        self.get_wake_event(event_id)
    }

    /// The trigger_engine.rs entry point for `ActionKind::AgentWake`: turns a
    /// due Trigger into a WakeEvent instead of doing an OS-level dispatch.
    /// `now_unix_s` is the same clock reading the caller used to decide the
    /// trigger was due (so `observed.observed_value` reflects what was
    /// actually evaluated, not a second, possibly-different clock read here).
    ///
    /// Idempotency key is `trigger-fire:{id}:{created_at}`: stable across
    /// retries of the SAME arming (e.g. dispatch succeeds but a crash happens
    /// before `mark_fired` runs, so the next tick re-evaluates the same due
    /// trigger) but distinct after a rearm, since `create_trigger` always
    /// stamps a fresh `created_at` on (re-)registration.
    pub fn create_wake_event_from_trigger(&self, t: &Trigger, now_unix_s: f64) -> Result<WakeEvent, StoreError> {
        if t.action.kind != ActionKind::AgentWake {
            return Err(StoreError::InvalidInput(format!(
                "trigger {} is not an agent_wake trigger (action.kind = {:?})",
                t.id, t.action.kind
            )));
        }
        let agent_id = &t.action.target;
        let observed_value = match t.kind {
            TriggerKind::CommonInstant => now_unix_s,
            TriggerKind::CustomTime => {
                let sid = t.system_id.as_deref().ok_or_else(|| {
                    StoreError::InvalidInput(format!("custom_time trigger {} is missing system_id", t.id))
                })?;
                self.system_now(sid, now_unix_s)?.0
            }
        };
        let idempotency_key = format!("trigger-fire:{}:{}", t.id, t.created_at);
        self.create_wake_event(
            agent_id,
            Some(&t.id),
            "trigger_condition_satisfied",
            json!({ "unix_s": now_unix_s, "source": "ctcl_trigger_engine" }),
            json!({ "operator": t.operator.as_str(), "target_value": t.target_value, "observed_value": observed_value }),
            json!({}),
            &idempotency_key,
        )
    }

    fn row_to_wake_event(row: &rusqlite::Row) -> rusqlite::Result<Result<WakeEvent, StoreError>> {
        let event_id: String = row.get(0)?;
        let trigger_id: Option<String> = row.get(1)?;
        let agent_id: String = row.get(2)?;
        let reason: String = row.get(3)?;
        let fired_json: String = row.get(4)?;
        let observed_json: String = row.get(5)?;
        let payload_json: String = row.get(6)?;
        let status: String = row.get(7)?;
        let attempt_count: i64 = row.get(8)?;
        let created_at: String = row.get(9)?;
        let acknowledged_at: Option<String> = row.get(10)?;
        let completed_at: Option<String> = row.get(11)?;
        let delivered_at: Option<String> = row.get(12)?;
        let next_attempt_at: Option<String> = row.get(13)?;
        let last_error: Option<String> = row.get(14)?;
        let idempotency_key: String = row.get(15)?;
        Ok((|| {
            Ok(WakeEvent {
                event_id,
                trigger_id,
                agent_id,
                reason,
                fired: serde_json::from_str(&fired_json)?,
                observed: serde_json::from_str(&observed_json)?,
                payload: serde_json::from_str(&payload_json)?,
                status: WakeEventStatus::from_str(&status),
                attempt_count,
                created_at,
                acknowledged_at,
                completed_at,
                delivered_at,
                next_attempt_at,
                last_error,
                idempotency_key,
            })
        })())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::{Operator, TriggerAction};

    #[test]
    fn create_then_get_round_trips() {
        let store = Store::open(":memory:").unwrap();
        let ev = store
            .create_wake_event(
                "agent:primary",
                Some("trigger:hourly-review"),
                "temporal_condition_satisfied",
                json!({ "unix_s": 1784422800.0, "source": "local_temporal_port" }),
                json!({ "operator": ">=", "target_value": 1784422800.0, "observed_value": 1784422800.5 }),
                json!({ "goal_refs": ["goal:ctcl-development"] }),
                "trigger:hourly-review",
            )
            .unwrap();
        assert_eq!(ev.status, WakeEventStatus::Pending);
        assert_eq!(ev.agent_id, "agent:primary");
        assert_eq!(ev.attempt_count, 0);

        let fetched = store.get_wake_event(&ev.event_id).unwrap();
        assert_eq!(fetched.event_id, ev.event_id);
        assert_eq!(fetched.payload["goal_refs"][0], "goal:ctcl-development");
    }

    #[test]
    fn duplicate_idempotency_key_returns_existing_not_a_second_row() {
        let store = Store::open(":memory:").unwrap();
        let a = store.create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "same-key").unwrap();
        let b = store.create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "same-key").unwrap();
        assert_eq!(a.event_id, b.event_id, "second create with the same idempotency_key must return the same event");
        assert_eq!(store.list_wake_events(None, None).unwrap().len(), 1);
    }

    #[test]
    fn ack_transitions_pending_to_acknowledged() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        let acked = store.ack_wake_event(&ev.event_id).unwrap();
        assert_eq!(acked.status, WakeEventStatus::Acknowledged);
        assert!(acked.acknowledged_at.is_some());
    }

    #[test]
    fn ack_is_not_repeatable() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        store.ack_wake_event(&ev.event_id).unwrap();
        let err = store.ack_wake_event(&ev.event_id).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "acking an already-acknowledged event must fail, not silently succeed again");
    }

    #[test]
    fn complete_requires_prior_ack() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        let err = store.complete_wake_event(&ev.event_id).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "completing straight from pending must be rejected - completed should imply an ack happened");
    }

    #[test]
    fn complete_transitions_acknowledged_to_completed() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        store.ack_wake_event(&ev.event_id).unwrap();
        let completed = store.complete_wake_event(&ev.event_id).unwrap();
        assert_eq!(completed.status, WakeEventStatus::Completed);
        assert!(completed.completed_at.is_some());
    }

    #[test]
    fn complete_is_not_repeatable() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        store.ack_wake_event(&ev.event_id).unwrap();
        store.complete_wake_event(&ev.event_id).unwrap();
        let err = store.complete_wake_event(&ev.event_id).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "completing an already-completed event must fail, not silently succeed again");
    }

    #[test]
    fn list_filters_by_agent_and_status() {
        let store = Store::open(":memory:").unwrap();
        store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        let b = store.create_wake_event("agent:b", None, "r", json!({}), json!({}), json!({}), "k2").unwrap();
        store.ack_wake_event(&b.event_id).unwrap();

        assert_eq!(store.list_wake_events(Some("agent:a"), None).unwrap().len(), 1);
        assert_eq!(store.list_wake_events(None, Some(WakeEventStatus::Pending)).unwrap().len(), 1);
        assert_eq!(store.list_wake_events(None, Some(WakeEventStatus::Acknowledged)).unwrap().len(), 1);
        assert_eq!(store.list_wake_events(None, None).unwrap().len(), 2);
    }

    #[test]
    fn unknown_event_id_errors() {
        let store = Store::open(":memory:").unwrap();
        let err = store.get_wake_event("wake:does-not-exist").unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)));
    }

    #[test]
    fn from_trigger_creates_a_wake_event_for_common_instant() {
        let store = Store::open(":memory:").unwrap();
        let action = TriggerAction { kind: ActionKind::AgentWake, target: "agent:primary".into() };
        let t = store
            .create_trigger("trigger:wake-me", crate::TriggerKind::CommonInstant, None, Operator::Ge, 1_700_000_000.0, action)
            .unwrap();

        let ev = store.create_wake_event_from_trigger(&t, 1_700_000_001.0).unwrap();
        assert_eq!(ev.agent_id, "agent:primary");
        assert_eq!(ev.trigger_id.as_deref(), Some("trigger:wake-me"));
        assert_eq!(ev.status, WakeEventStatus::Pending);
        assert_eq!(ev.observed["observed_value"], 1_700_000_001.0);
        assert_eq!(ev.observed["target_value"], 1_700_000_000.0);
    }

    #[test]
    fn from_trigger_is_idempotent_across_retries_of_the_same_arming() {
        let store = Store::open(":memory:").unwrap();
        let action = TriggerAction { kind: ActionKind::AgentWake, target: "agent:primary".into() };
        let t = store
            .create_trigger("trigger:wake-me", crate::TriggerKind::CommonInstant, None, Operator::Ge, 1_700_000_000.0, action)
            .unwrap();

        let a = store.create_wake_event_from_trigger(&t, 1_700_000_001.0).unwrap();
        let b = store.create_wake_event_from_trigger(&t, 1_700_000_002.0).unwrap();
        assert_eq!(a.event_id, b.event_id, "re-evaluating the same still-active trigger must not create a second WakeEvent");
        assert_eq!(store.list_wake_events(None, None).unwrap().len(), 1);
    }

    #[test]
    fn from_trigger_uses_the_live_local_value_for_custom_time() {
        let store = Store::open(":memory:").unwrap();
        store.create_system("agent:a:active-time", None, 1_700_000_000.0, ctcl_core::Rate::Constant { value: 1.0 }, 0.0).unwrap();
        let action = TriggerAction { kind: ActionKind::AgentWake, target: "agent:primary".into() };
        let t = store
            .create_trigger("trigger:hour-worked", crate::TriggerKind::CustomTime, Some("agent:a:active-time"), Operator::Ge, 3600.0, action)
            .unwrap();

        let ev = store.create_wake_event_from_trigger(&t, 1_700_003_601.0).unwrap();
        assert_eq!(ev.observed["observed_value"], 3601.0);
    }

    #[test]
    fn from_trigger_rejects_non_agent_wake_triggers() {
        let store = Store::open(":memory:").unwrap();
        let action = TriggerAction { kind: ActionKind::Notification, target: "hello".into() };
        let t = store
            .create_trigger("trigger:notify", crate::TriggerKind::CommonInstant, None, Operator::Ge, 100.0, action)
            .unwrap();
        let err = store.create_wake_event_from_trigger(&t, 200.0).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)));
    }

    // ---- Phase 4.5D: Active Delivery ----

    fn register_enabled_endpoint(store: &Store, agent_id: &str) {
        store.create_agent_endpoint(agent_id, "loopback_http", "http://127.0.0.1:4400/wake", None, &[]).unwrap();
        store.set_agent_endpoint_enabled(agent_id, true).unwrap();
    }

    #[test]
    fn due_for_delivery_only_returns_events_for_enabled_endpoints() {
        let store = Store::open(":memory:").unwrap();
        store.create_wake_event("agent:no-endpoint", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        register_enabled_endpoint(&store, "agent:has-endpoint");
        store.create_wake_event("agent:has-endpoint", None, "r", json!({}), json!({}), json!({}), "k2").unwrap();

        let now = chrono::Utc::now().to_rfc3339();
        let due = store.due_for_delivery(&now, 10).unwrap();
        assert_eq!(due.len(), 1, "an agent with no registered/enabled endpoint must stay poll-only, not get picked up for active delivery");
        assert_eq!(due[0].agent_id, "agent:has-endpoint");
    }

    #[test]
    fn due_for_delivery_respects_the_batch_limit() {
        let store = Store::open(":memory:").unwrap();
        register_enabled_endpoint(&store, "agent:a");
        store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k2").unwrap();
        store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k3").unwrap();

        let now = chrono::Utc::now().to_rfc3339();
        assert_eq!(store.due_for_delivery(&now, 2).unwrap().len(), 2, "the per-tick concurrency limit must cap how many come back");
        assert_eq!(store.due_for_delivery(&now, 10).unwrap().len(), 3);
    }

    #[test]
    fn due_for_delivery_excludes_a_disabled_endpoint() {
        let store = Store::open(":memory:").unwrap();
        register_enabled_endpoint(&store, "agent:a");
        store.set_agent_endpoint_enabled("agent:a", false).unwrap();
        store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();

        let now = chrono::Utc::now().to_rfc3339();
        assert!(store.due_for_delivery(&now, 10).unwrap().is_empty());
    }

    #[test]
    fn mark_delivering_requires_pending_or_retry_wait() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        store.mark_wake_event_delivering(&ev.event_id).unwrap();
        let err = store.mark_wake_event_delivering(&ev.event_id).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "already-delivering must not be claimed a second time");
    }

    #[test]
    fn delivering_then_success_reaches_delivered_and_can_still_be_acked() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();

        let delivering = store.mark_wake_event_delivering(&ev.event_id).unwrap();
        assert_eq!(delivering.status, WakeEventStatus::Delivering);

        let delivered = store.record_delivery_success(&ev.event_id).unwrap();
        assert_eq!(delivered.status, WakeEventStatus::Delivered);
        assert!(delivered.delivered_at.is_some());

        // delivered != acknowledged (§9.2: a 202/successful spawn is not task completion)
        let acked = store.ack_wake_event(&ev.event_id).unwrap();
        assert_eq!(acked.status, WakeEventStatus::Acknowledged);
    }

    #[test]
    fn record_delivery_failure_requires_delivering_status() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        let err = store.record_delivery_failure(&ev.event_id, "boom").unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "can't fail a delivery that was never claimed");
    }

    #[test]
    fn record_delivery_failure_retries_with_backoff_before_dead_lettering() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();
        store.mark_wake_event_delivering(&ev.event_id).unwrap();

        let retried = store.record_delivery_failure(&ev.event_id, "connection refused").unwrap();
        assert_eq!(retried.status, WakeEventStatus::RetryWait);
        assert_eq!(retried.attempt_count, 1);
        assert!(retried.next_attempt_at.is_some(), "a retry must schedule a next_attempt_at (§8.2 backoff)");
        assert_eq!(retried.last_error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn record_delivery_failure_dead_letters_after_max_attempts() {
        let store = Store::open(":memory:").unwrap();
        let ev = store.create_wake_event("agent:a", None, "r", json!({}), json!({}), json!({}), "k1").unwrap();

        for _ in 0..MAX_DELIVERY_ATTEMPTS {
            store.mark_wake_event_delivering(&ev.event_id).unwrap();
            store.record_delivery_failure(&ev.event_id, "still down").unwrap();
        }
        let final_state = store.get_wake_event(&ev.event_id).unwrap();
        assert_eq!(final_state.status, WakeEventStatus::DeadLetter, "must give up after §8.1 step 6's attempt cap, not retry forever");
        assert_eq!(final_state.attempt_count, MAX_DELIVERY_ATTEMPTS);
        assert_eq!(final_state.last_error.as_deref(), Some("still down"));
    }
}
