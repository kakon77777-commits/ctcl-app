//! Trigger Engine (whitepaper §4.3/§9.4, Phase 4):
//!
//!   I*         = I_target      => action   (kind = common_instant)
//!   tau_custom = tau_target    => action   (kind = custom_time, needs system_id)
//!
//! Both reduce to one comparison: is a live numeric value (wall unix_s for
//! common_instant, or a stored system's current local seconds for
//! custom_time, via Store::system_now) >= or <= a target. "==" isn't offered
//! - periodic evaluation would almost always step over an exact instant and
//! the trigger would silently never fire, which is a footgun, not a feature.
//!
//! This module owns condition evaluation and persistence only. Actually
//! *doing* an action (opening a URI, showing a notification) is OS-level I/O
//! and lives in ctcl-desktop, same split as device_observer.rs.

use crate::{Store, StoreError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerKind {
    CommonInstant,
    CustomTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operator {
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "<=")]
    Le,
}

impl Operator {
    pub fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            ">=" => Ok(Operator::Ge),
            "<=" => Ok(Operator::Le),
            other => Err(StoreError::InvalidInput(format!(
                "unsupported operator '{other}' - only >= and <= are supported (== would silently never fire under periodic sampling)"
            ))),
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Operator::Ge => ">=",
            Operator::Le => "<=",
        }
    }
    pub fn satisfied(&self, current: f64, target: f64) -> bool {
        match self {
            Operator::Ge => current >= target,
            Operator::Le => current <= target,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Notification,
    Callback,
    /// Agent Wake (whitepaper §7 of CTCL_Agent_Wake_MCP_Temporal_Runtime):
    /// instead of an OS-level dispatch, produces a persisted WakeEvent for an
    /// external Agent Runtime to pick up. CTCL deliberately does NOT call MCP
    /// tools directly from here - see device_observer.rs's dispatcher for why
    /// that boundary matters (this is the same discipline, one level up).
    AgentWake,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerAction {
    pub kind: ActionKind,
    /// notification: the message to show. callback: the URI to open (the OS's
    /// default handler for that scheme decides what happens next - CTCL does
    /// not register or resolve schemes itself, matching §7.1's "private
    /// scheme only, no protocol handler" scope for this phase). agent_wake:
    /// the agent_id to wake (e.g. "agent:primary") - CTCL does not validate
    /// that any such agent is registered; it's an opaque routing label until
    /// Agent Endpoints (a later phase) exist.
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerStatus {
    Active,
    Fired,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    pub id: String,
    pub kind: TriggerKind,
    pub system_id: Option<String>,
    pub operator: Operator,
    pub target_value: f64,
    pub action: TriggerAction,
    pub status: TriggerStatus,
    pub created_at: String,
    pub fired_at: Option<String>,
}

fn status_from_str(s: &str) -> TriggerStatus {
    match s {
        "fired" => TriggerStatus::Fired,
        "cancelled" => TriggerStatus::Cancelled,
        _ => TriggerStatus::Active,
    }
}

impl Store {
    /// Re-registering an existing id rearms it (resets to active, clears
    /// fired_at) - same "re-post overwrites" convention as custom systems.
    pub fn create_trigger(
        &self,
        id: &str,
        kind: TriggerKind,
        system_id: Option<&str>,
        operator: Operator,
        target_value: f64,
        action: TriggerAction,
    ) -> Result<Trigger, StoreError> {
        if id.trim().is_empty() {
            return Err(StoreError::InvalidInput("trigger id must not be empty".into()));
        }
        if kind == TriggerKind::CustomTime && system_id.is_none() {
            return Err(StoreError::InvalidInput("custom_time trigger requires system_id".into()));
        }
        let kind_str = match kind {
            TriggerKind::CommonInstant => "common_instant",
            TriggerKind::CustomTime => "custom_time",
        };
        let action_json = serde_json::to_string(&action)?;
        let created_at = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO triggers (id, kind, system_id, operator, target_value, action_json, status, created_at, fired_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, NULL)
             ON CONFLICT(id) DO UPDATE SET kind=excluded.kind, system_id=excluded.system_id,
                operator=excluded.operator, target_value=excluded.target_value, action_json=excluded.action_json,
                status='active', created_at=excluded.created_at, fired_at=NULL",
            rusqlite::params![id, kind_str, system_id, operator.as_str(), target_value, action_json, created_at],
        )?;
        self.get_trigger(id)
    }

    pub fn get_trigger(&self, id: &str) -> Result<Trigger, StoreError> {
        self.conn
            .query_row(
                "SELECT id, kind, system_id, operator, target_value, action_json, status, created_at, fired_at FROM triggers WHERE id = ?1",
                [id],
                Self::row_to_parts,
            )
            .map_err(|_| StoreError::InvalidInput(format!("unknown trigger: {id}")))
            .and_then(Self::parts_to_trigger)
    }

    pub fn list_triggers(&self) -> Result<Vec<Trigger>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, system_id, operator, target_value, action_json, status, created_at, fired_at FROM triggers ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], Self::row_to_parts)?.collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(Self::parts_to_trigger).collect()
    }

    pub fn cancel_trigger(&self, id: &str) -> Result<Trigger, StoreError> {
        self.conn.execute(
            "UPDATE triggers SET status='cancelled' WHERE id=?1 AND status='active'",
            [id],
        )?;
        self.get_trigger(id)
    }

    /// Only "fired" if dispatch actually succeeded - the caller (ctcl-desktop)
    /// marks fired only after a real dispatch, so this stays internal-ish but
    /// public for that caller.
    pub fn mark_fired(&self, id: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE triggers SET status='fired', fired_at=?1 WHERE id=?2 AND status='active'",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), id],
        )?;
        Ok(())
    }

    /// Active triggers whose condition is satisfied RIGHT NOW, evaluated
    /// against `now_unix_s`. Does not mutate anything - purely a query, so a
    /// caller can dispatch actions first and only mark_fired on success.
    pub fn due_triggers(&self, now_unix_s: f64) -> Result<Vec<Trigger>, StoreError> {
        let all = self.list_triggers()?;
        let mut due = Vec::new();
        for t in all.into_iter().filter(|t| t.status == TriggerStatus::Active) {
            let current = match t.kind {
                TriggerKind::CommonInstant => now_unix_s,
                TriggerKind::CustomTime => {
                    let sid = match t.system_id.as_deref() {
                        Some(s) => s,
                        None => continue, // shouldn't happen (validated at creation), skip defensively
                    };
                    match self.system_now(sid, now_unix_s) {
                        Ok((local, _)) => local,
                        Err(_) => continue, // referenced system missing/broken - skip this round, don't fail the whole pass
                    }
                }
            };
            if t.operator.satisfied(current, t.target_value) {
                due.push(t);
            }
        }
        Ok(due)
    }

    fn row_to_parts(
        row: &rusqlite::Row,
    ) -> rusqlite::Result<(String, String, Option<String>, String, f64, String, String, String, Option<String>)> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
        ))
    }

    fn parts_to_trigger(
        parts: (String, String, Option<String>, String, f64, String, String, String, Option<String>),
    ) -> Result<Trigger, StoreError> {
        let (id, kind_str, system_id, op_str, target_value, action_json, status, created_at, fired_at) = parts;
        let kind = if kind_str == "custom_time" { TriggerKind::CustomTime } else { TriggerKind::CommonInstant };
        let operator = Operator::parse(&op_str)?;
        let action: TriggerAction = serde_json::from_str(&action_json)?;
        Ok(Trigger { id, kind, system_id, operator, target_value, action, status: status_from_str(&status), created_at, fired_at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify(msg: &str) -> TriggerAction {
        TriggerAction { kind: ActionKind::Notification, target: msg.to_string() }
    }

    #[test]
    fn common_instant_trigger_fires_when_wall_time_reaches_target() {
        let store = Store::open(":memory:").unwrap();
        store.create_trigger("trigger:deadline", TriggerKind::CommonInstant, None, Operator::Ge, 1_700_000_000.0, notify("deadline")).unwrap();

        assert!(store.due_triggers(1_699_999_999.0).unwrap().is_empty(), "not due yet");
        let due = store.due_triggers(1_700_000_000.0).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "trigger:deadline");
    }

    #[test]
    fn custom_time_trigger_fires_from_a_stored_system() {
        let store = Store::open(":memory:").unwrap();
        store.create_system("agent:a:active-time", None, 1_700_000_000.0, ctcl_core::Rate::Constant { value: 1.0 }, 0.0).unwrap();
        store
            .create_trigger("trigger:hour-worked", TriggerKind::CustomTime, Some("agent:a:active-time"), Operator::Ge, 3600.0, notify("an hour has passed"))
            .unwrap();

        assert!(store.due_triggers(1_700_003_000.0).unwrap().is_empty(), "only ~50min elapsed");
        let due = store.due_triggers(1_700_003_601.0).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "trigger:hour-worked");
    }

    #[test]
    fn custom_time_without_system_id_is_rejected() {
        let store = Store::open(":memory:").unwrap();
        let err = store.create_trigger("t", TriggerKind::CustomTime, None, Operator::Ge, 10.0, notify("x")).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)));
    }

    #[test]
    fn unsupported_operator_is_rejected_not_silently_accepted() {
        let err = Operator::parse("==").unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)));
    }

    #[test]
    fn a_fired_trigger_is_no_longer_due_and_stays_fired() {
        let store = Store::open(":memory:").unwrap();
        store.create_trigger("trigger:once", TriggerKind::CommonInstant, None, Operator::Ge, 100.0, notify("x")).unwrap();
        assert_eq!(store.due_triggers(200.0).unwrap().len(), 1);

        store.mark_fired("trigger:once").unwrap();
        assert!(store.due_triggers(300.0).unwrap().is_empty(), "a fired trigger must not fire again");
        assert_eq!(store.get_trigger("trigger:once").unwrap().status, TriggerStatus::Fired);
    }

    #[test]
    fn cancelling_an_active_trigger_removes_it_from_due_evaluation() {
        let store = Store::open(":memory:").unwrap();
        store.create_trigger("trigger:cancel-me", TriggerKind::CommonInstant, None, Operator::Ge, 0.0, notify("x")).unwrap();
        store.cancel_trigger("trigger:cancel-me").unwrap();
        assert_eq!(store.get_trigger("trigger:cancel-me").unwrap().status, TriggerStatus::Cancelled);
        assert!(store.due_triggers(1_000_000_000.0).unwrap().is_empty());
    }

    #[test]
    fn le_operator_fires_when_value_drops_to_or_below_target() {
        let store = Store::open(":memory:").unwrap();
        // a countdown system: rate -1 from a high starting value
        store.create_system("countdown", None, 0.0, ctcl_core::Rate::Constant { value: -1.0 }, 1000.0).unwrap();
        store.create_trigger("trigger:countdown-done", TriggerKind::CustomTime, Some("countdown"), Operator::Le, 0.0, notify("countdown finished")).unwrap();
        assert!(store.due_triggers(500.0).unwrap().is_empty()); // 1000 - 500 = 500, still positive
        assert_eq!(store.due_triggers(1000.0).unwrap().len(), 1); // 1000 - 1000 = 0, satisfies <= 0
    }

    #[test]
    fn recreating_a_trigger_rearms_it() {
        let store = Store::open(":memory:").unwrap();
        store.create_trigger("trigger:x", TriggerKind::CommonInstant, None, Operator::Ge, 100.0, notify("x")).unwrap();
        store.mark_fired("trigger:x").unwrap();
        assert_eq!(store.get_trigger("trigger:x").unwrap().status, TriggerStatus::Fired);

        store.create_trigger("trigger:x", TriggerKind::CommonInstant, None, Operator::Ge, 999.0, notify("y")).unwrap();
        let rec = store.get_trigger("trigger:x").unwrap();
        assert_eq!(rec.status, TriggerStatus::Active);
        assert!(rec.fired_at.is_none());
        assert_eq!(rec.target_value, 999.0);
    }

    #[test]
    fn list_triggers_returns_everything_regardless_of_status() {
        let store = Store::open(":memory:").unwrap();
        store.create_trigger("a", TriggerKind::CommonInstant, None, Operator::Ge, 1.0, notify("x")).unwrap();
        store.create_trigger("b", TriggerKind::CommonInstant, None, Operator::Ge, 2.0, notify("x")).unwrap();
        store.cancel_trigger("b").unwrap();
        assert_eq!(store.list_triggers().unwrap().len(), 2);
    }
}
