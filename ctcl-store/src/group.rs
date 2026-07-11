//! Persistent Temporal Groups - the local equivalent of the Worker's
//! POST/GET /v1/temporal-groups + POST /v1/temporal-groups/{id}/expand.
//! "One Instant, Many Systems": project one instant across every member of a
//! named, versioned group in a single call. Re-posting the same id bumps its
//! version, exactly like the hosted API.

use crate::{Store, StoreError};
use ctcl_core::{gps_approx_ns, rfc3339, tai_approx_ns};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRecord {
    pub id: String,
    pub members: Vec<String>,
    pub owner: Option<String>,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
}

impl Store {
    pub fn create_group(&self, id: &str, members: &[String], owner: Option<&str>) -> Result<GroupRecord, StoreError> {
        if id.trim().is_empty() {
            return Err(StoreError::InvalidInput("group id must not be empty".into()));
        }
        if members.is_empty() {
            return Err(StoreError::InvalidInput("group.members must be a non-empty array".into()));
        }
        let existing = self.get_group(id).ok();
        let version = existing.as_ref().map(|g| g.version + 1).unwrap_or(1);
        let created_at = existing.as_ref().map(|g| g.created_at.clone()).unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        let updated_at = chrono::Utc::now().to_rfc3339();
        let owner = owner.map(String::from).or_else(|| existing.and_then(|g| g.owner));
        let members_json = serde_json::to_string(members)?;

        self.conn.execute(
            "INSERT INTO groups (id, members_json, owner, version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET members_json=excluded.members_json, owner=excluded.owner,
                version=excluded.version, updated_at=excluded.updated_at",
            rusqlite::params![id, members_json, owner, version, created_at, updated_at],
        )?;
        Ok(GroupRecord { id: id.to_string(), members: members.to_vec(), owner, version, created_at, updated_at })
    }

    pub fn get_group(&self, id: &str) -> Result<GroupRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT id, members_json, owner, version, created_at, updated_at FROM groups WHERE id = ?1",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .map_err(|_| StoreError::UnknownGroup(id.to_string()))
            .and_then(|(id, members_json, owner, version, created_at, updated_at)| {
                let members: Vec<String> = serde_json::from_str(&members_json)?;
                Ok(GroupRecord { id, members, owner, version, created_at, updated_at })
            })
    }

    pub fn list_groups(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT id FROM groups ORDER BY id")?;
        let ids = stmt.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Project one instant (canonical unix_ns) across every member of a group.
    /// Mirrors the Worker's resolveMember()/expandGroup(): a member is a
    /// builtin timescale ("utc"|"posix"|"tai"|"gps"), a civil timezone
    /// ("tz:<IANA>"), or a stored custom system id. One bad member produces a
    /// per-member error, never fails the whole request.
    pub fn expand_group(&self, id: &str, unix_ns: i128) -> Result<serde_json::Value, StoreError> {
        let group = self.get_group(id)?;
        let members: Vec<serde_json::Value> = group
            .members
            .iter()
            .map(|m| self.resolve_member(m, unix_ns))
            .collect();
        Ok(json!({
            "group_id": group.id,
            "group_version": group.version,
            "instant": { "unix_ns": unix_ns.to_string(), "rfc3339": rfc3339(unix_ns, None).unwrap_or_default() },
            "members": members,
        }))
    }

    fn resolve_member(&self, member: &str, unix_ns: i128) -> serde_json::Value {
        match member {
            "utc" => json!({ "member": "utc", "kind": "builtin", "value": rfc3339(unix_ns, None).unwrap_or_default() }),
            "posix" => json!({ "member": "posix", "kind": "builtin", "value": (unix_ns / ctcl_core::encoding::NS_PER_S).to_string() }),
            "tai" => json!({ "member": "tai", "kind": "builtin", "value": (tai_approx_ns(unix_ns) / ctcl_core::encoding::NS_PER_S).to_string() }),
            "gps" => json!({ "member": "gps", "kind": "builtin", "value": (gps_approx_ns(unix_ns) / ctcl_core::encoding::NS_PER_S).to_string() }),
            m if m.starts_with("tz:") => {
                let tz = &m[3..];
                match rfc3339(unix_ns, Some(tz)) {
                    Ok(value) => json!({ "member": m, "kind": "timezone", "timezone": tz, "value": value }),
                    Err(_) => json!({ "member": m, "kind": "timezone", "error": "INVALID_TIMEZONE", "message": format!("unrecognized IANA timezone: {tz}") }),
                }
            }
            m => match self.system_now(m, unix_ns as f64 / ctcl_core::encoding::NS_PER_S as f64) {
                Ok((local, _)) => json!({ "member": m, "kind": "system", "value": local.to_string() }),
                Err(_) => json!({ "member": m, "kind": "system", "error": "UNKNOWN_SYSTEM", "message": format!("no such system: {m}") }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctcl_core::Rate;

    #[test]
    fn create_bumps_version_on_repost() {
        let store = Store::open(":memory:").unwrap();
        let g1 = store.create_group("group:demo", &["utc".to_string()], None).unwrap();
        assert_eq!(g1.version, 1);
        let g2 = store.create_group("group:demo", &["utc".to_string(), "tai".to_string()], None).unwrap();
        assert_eq!(g2.version, 2);
        assert_eq!(g2.members.len(), 2);
    }

    #[test]
    fn expand_resolves_builtin_tz_and_system_members_independently() {
        let store = Store::open(":memory:").unwrap();
        store.create_system("user:game_world", None, 1_700_000_000.0, Rate::Constant { value: 20.0 }, 0.0).unwrap();
        store
            .create_group(
                "group:demo",
                &["utc".to_string(), "tz:Asia/Taipei".to_string(), "user:game_world".to_string(), "user:nonexistent".to_string()],
                None,
            )
            .unwrap();

        let ns: i128 = 1_700_000_100 * ctcl_core::encoding::NS_PER_S;
        let result = store.expand_group("group:demo", ns).unwrap();
        let members = result["members"].as_array().unwrap();
        assert_eq!(members.len(), 4);
        assert_eq!(members[0]["kind"], "builtin");
        assert_eq!(members[1]["kind"], "timezone");
        assert_eq!(members[2]["kind"], "system");
        assert_eq!(members[2]["value"], "2000"); // 100s elapsed * 20x
        assert_eq!(members[3]["error"], "UNKNOWN_SYSTEM"); // graceful, not a hard failure
    }

    #[test]
    fn empty_members_rejected() {
        let store = Store::open(":memory:").unwrap();
        let err = store.create_group("group:empty", &[], None).unwrap_err();
        assert_eq!(err.code(), "INVALID_TIME_VALUE");
    }
}
