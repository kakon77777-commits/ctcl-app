//! Decision Receipt (Phase 4.5B, whitepaper §6.3/§10.4): the record an
//! external Agent Runtime posts back after processing a WakeEvent - what it
//! decided (`no_action` vs `action`), a human-readable summary, which tools
//! it called, and what it wants to happen next (§25's
//! `schedule_next_wake_if_needed`, realized here as the agent itself calling
//! the new Trigger write API in local_api.rs - CTCL does not read
//! `next_wake` and act on it automatically).
//!
//! CTCL stores this purely as a receipt; it never inspects `decision` to
//! trigger behavior of its own - that would reintroduce exactly the
//! "CTCL calls tools on the agent's behalf" coupling wake_event.rs's own doc
//! comment explicitly rejects for WakeEvents. A receipt is filed, not acted on.

use crate::{Store, StoreError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionReceipt {
    pub receipt_id: String,
    pub event_id: String,
    pub agent_id: String,
    pub run_id: String,
    /// "no_action" | "action" - validated at creation, not an open string.
    pub decision: String,
    pub summary: Option<String>,
    pub tool_calls: Option<serde_json::Value>,
    pub next_wake: Option<serde_json::Value>,
    pub cost: Option<serde_json::Value>,
    pub created_at: String,
}

impl Store {
    /// Files a receipt against `event_id`. Only requires the WakeEvent to
    /// exist - deliberately does NOT require it to be in any particular
    /// status first (e.g. `acknowledged`), since a receipt is an honest
    /// append-only record of what the agent did, not a gate on the event's
    /// own state machine (that's what `ack_wake_event`/`complete_wake_event`
    /// already enforce).
    #[allow(clippy::too_many_arguments)]
    pub fn create_decision_receipt(
        &self,
        event_id: &str,
        agent_id: &str,
        run_id: &str,
        decision: &str,
        summary: Option<&str>,
        tool_calls: Option<serde_json::Value>,
        next_wake: Option<serde_json::Value>,
        cost: Option<serde_json::Value>,
    ) -> Result<DecisionReceipt, StoreError> {
        if decision != "no_action" && decision != "action" {
            return Err(StoreError::InvalidInput(format!(
                "decision must be 'no_action' or 'action', got '{decision}'"
            )));
        }
        self.get_wake_event(event_id)?; // fails honestly if the event doesn't exist, rather than filing an orphan receipt

        let receipt_id = format!("receipt:{}", uuid::Uuid::new_v4());
        let created_at = chrono::Utc::now().to_rfc3339();
        let tool_calls_json = tool_calls.as_ref().map(serde_json::to_string).transpose()?;
        let next_wake_json = next_wake.as_ref().map(serde_json::to_string).transpose()?;
        let cost_json = cost.as_ref().map(serde_json::to_string).transpose()?;
        self.conn.execute(
            "INSERT INTO decision_receipts (receipt_id, event_id, agent_id, run_id, decision, summary, tool_calls_json, next_wake_json, cost_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![receipt_id, event_id, agent_id, run_id, decision, summary, tool_calls_json, next_wake_json, cost_json, created_at],
        )?;
        self.get_decision_receipt(&receipt_id)
    }

    fn get_decision_receipt(&self, receipt_id: &str) -> Result<DecisionReceipt, StoreError> {
        self.conn
            .query_row(
                "SELECT receipt_id, event_id, agent_id, run_id, decision, summary, tool_calls_json, next_wake_json, cost_json, created_at
                 FROM decision_receipts WHERE receipt_id = ?1",
                [receipt_id],
                Self::row_to_receipt,
            )
            .map_err(|_| StoreError::InvalidInput(format!("unknown decision receipt: {receipt_id}")))?
    }

    /// The whitepaper's own §6.3 schema has no UNIQUE constraint on
    /// `event_id` (multiple receipts per event are allowed - e.g. an interim
    /// then a final one), so this returns the most recent, matching §10.4's
    /// singular `GET /v1/wake-events/{event_id}/decision`.
    pub fn get_latest_decision_receipt(&self, event_id: &str) -> Result<Option<DecisionReceipt>, StoreError> {
        let result = self.conn.query_row(
            "SELECT receipt_id, event_id, agent_id, run_id, decision, summary, tool_calls_json, next_wake_json, cost_json, created_at
             FROM decision_receipts WHERE event_id = ?1 ORDER BY created_at DESC LIMIT 1",
            [event_id],
            Self::row_to_receipt,
        );
        match result {
            Ok(r) => Ok(Some(r?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn row_to_receipt(row: &rusqlite::Row) -> rusqlite::Result<Result<DecisionReceipt, StoreError>> {
        let receipt_id: String = row.get(0)?;
        let event_id: String = row.get(1)?;
        let agent_id: String = row.get(2)?;
        let run_id: String = row.get(3)?;
        let decision: String = row.get(4)?;
        let summary: Option<String> = row.get(5)?;
        let tool_calls_json: Option<String> = row.get(6)?;
        let next_wake_json: Option<String> = row.get(7)?;
        let cost_json: Option<String> = row.get(8)?;
        let created_at: String = row.get(9)?;
        Ok((|| {
            Ok(DecisionReceipt {
                receipt_id,
                event_id,
                agent_id,
                run_id,
                decision,
                summary,
                tool_calls: tool_calls_json.as_deref().map(serde_json::from_str).transpose()?,
                next_wake: next_wake_json.as_deref().map(serde_json::from_str).transpose()?,
                cost: cost_json.as_deref().map(serde_json::from_str).transpose()?,
                created_at,
            })
        })())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn seed_event(store: &Store) -> String {
        store.create_wake_event("agent:primary", None, "r", json!({}), json!({}), json!({}), "k1").unwrap().event_id
    }

    #[test]
    fn create_then_get_latest_round_trips() {
        let store = Store::open(":memory:").unwrap();
        let event_id = seed_event(&store);
        let receipt = store
            .create_decision_receipt(
                &event_id,
                "agent:primary",
                "run:01J",
                "no_action",
                Some("no repository changes since last review"),
                Some(json!([])),
                Some(json!({ "kind": "relative", "after_seconds": 3600 })),
                Some(json!({ "model_tokens": 1832, "tool_calls": 0 })),
            )
            .unwrap();
        assert_eq!(receipt.decision, "no_action");
        assert_eq!(receipt.event_id, event_id);

        let latest = store.get_latest_decision_receipt(&event_id).unwrap().unwrap();
        assert_eq!(latest.receipt_id, receipt.receipt_id);
        assert_eq!(latest.next_wake.unwrap()["after_seconds"], 3600);
    }

    #[test]
    fn invalid_decision_value_is_rejected() {
        let store = Store::open(":memory:").unwrap();
        let event_id = seed_event(&store);
        let err = store
            .create_decision_receipt(&event_id, "agent:primary", "run:1", "maybe", None, None, None, None)
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "an open-ended decision string must be rejected, not silently accepted");
    }

    #[test]
    fn unknown_event_id_is_rejected() {
        let store = Store::open(":memory:").unwrap();
        let err = store
            .create_decision_receipt("wake:does-not-exist", "agent:primary", "run:1", "no_action", None, None, None, None)
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "must not file a receipt against a nonexistent event");
    }

    #[test]
    fn no_receipt_yet_returns_none_not_an_error() {
        let store = Store::open(":memory:").unwrap();
        let event_id = seed_event(&store);
        assert!(store.get_latest_decision_receipt(&event_id).unwrap().is_none());
    }

    #[test]
    fn multiple_receipts_per_event_returns_the_most_recent() {
        let store = Store::open(":memory:").unwrap();
        let event_id = seed_event(&store);
        store.create_decision_receipt(&event_id, "agent:primary", "run:1", "no_action", Some("first"), None, None, None).unwrap();
        let second = store.create_decision_receipt(&event_id, "agent:primary", "run:2", "action", Some("second"), None, None, None).unwrap();

        let latest = store.get_latest_decision_receipt(&event_id).unwrap().unwrap();
        assert_eq!(latest.receipt_id, second.receipt_id);
        assert_eq!(latest.summary.as_deref(), Some("second"));
    }
}
