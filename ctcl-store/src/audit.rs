//! Audit log for the local API (whitepaper §13 "audit log"). Every request the
//! local API receives is recorded here, whether allowed or refused - this is
//! what lets a user actually check what other apps/agents have been asking
//! for, not just trust that the permission model is working.

use crate::{Store, StoreError};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub at: String,
    pub method: String,
    pub path: String,
    pub scope: Option<String>,
    pub allowed: bool,
    pub reason: Option<String>,
}

impl Store {
    pub fn log_audit(&self, method: &str, path: &str, scope: Option<&str>, allowed: bool, reason: Option<&str>) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO audit_log (at, method, path, scope, allowed, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![chrono::Utc::now().to_rfc3339(), method, path, scope, allowed as i64, reason],
        )?;
        Ok(())
    }

    pub fn list_audit_log(&self, limit: u32) -> Result<Vec<AuditEntry>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT id, at, method, path, scope, allowed, reason FROM audit_log ORDER BY id DESC LIMIT ?1")?;
        let rows = stmt
            .query_map([limit], |row| {
                Ok(AuditEntry {
                    id: row.get(0)?,
                    at: row.get(1)?,
                    method: row.get(2)?,
                    path: row.get(3)?,
                    scope: row.get(4)?,
                    allowed: row.get::<_, i64>(5)? != 0,
                    reason: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_and_lists_in_reverse_chronological_order() {
        let store = Store::open(":memory:").unwrap();
        store.log_audit("GET", "/v1/now", Some("instant.read"), true, None).unwrap();
        store.log_audit("POST", "/v1/systems", Some("systems.write"), false, Some("scope not granted")).unwrap();

        let entries = store.list_audit_log(10).unwrap();
        assert_eq!(entries.len(), 2);
        // most recent first
        assert_eq!(entries[0].path, "/v1/systems");
        assert!(!entries[0].allowed);
        assert_eq!(entries[0].reason.as_deref(), Some("scope not granted"));
        assert_eq!(entries[1].path, "/v1/now");
        assert!(entries[1].allowed);
    }

    #[test]
    fn list_respects_limit() {
        let store = Store::open(":memory:").unwrap();
        for i in 0..5 {
            store.log_audit("GET", &format!("/v1/now/{i}"), None, true, None).unwrap();
        }
        let entries = store.list_audit_log(2).unwrap();
        assert_eq!(entries.len(), 2);
    }
}
