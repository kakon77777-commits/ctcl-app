//! Persistent instant registry - the local equivalent of the Worker's
//! POST /v1/instants + GET /v1/instant/{id}. Register once, retrieve later
//! (even after restarting the app), same exact unix_ns every time.

use crate::{Store, StoreError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstantRecord {
    pub id: String,
    pub unix_ns: String,
    pub registered_at: String,
    pub label: Option<String>,
    pub from_wall_clock: bool,
}

fn instant_id() -> String {
    format!("ctcl:instant:{}", uuid::Uuid::new_v4())
}

fn uuid_of(id: &str) -> String {
    id.trim_start_matches("ctcl:instant:")
        .trim_start_matches("instant:")
        .to_string()
}

impl Store {
    /// Register a reference instant, mirroring the Worker's registerInstant().
    /// `unix_ns` should already be the canonical value the caller wants to
    /// remember - this function does not compute "now" itself.
    pub fn register_instant(
        &self,
        unix_ns: i128,
        label: Option<&str>,
        from_wall_clock: bool,
    ) -> Result<InstantRecord, StoreError> {
        let id = instant_id();
        let registered_at = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO instants (id, unix_ns, registered_at, label, from_wall_clock) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                uuid_of(&id),
                unix_ns.to_string(),
                registered_at,
                label,
                from_wall_clock as i64
            ],
        )?;
        Ok(InstantRecord {
            id,
            unix_ns: unix_ns.to_string(),
            registered_at,
            label: label.map(String::from),
            from_wall_clock,
        })
    }

    /// Retrieve a previously-registered instant by id (with or without the
    /// "ctcl:instant:" prefix - both resolve to the same record).
    pub fn get_instant(&self, id: &str) -> Result<InstantRecord, StoreError> {
        let key = uuid_of(id);
        self.conn
            .query_row(
                "SELECT id, unix_ns, registered_at, label, from_wall_clock FROM instants WHERE id = ?1",
                [&key],
                |row| {
                    Ok(InstantRecord {
                        id: format!("ctcl:instant:{}", row.get::<_, String>(0)?),
                        unix_ns: row.get(1)?,
                        registered_at: row.get(2)?,
                        label: row.get(3)?,
                        from_wall_clock: row.get::<_, i64>(4)? != 0,
                    })
                },
            )
            .map_err(|_| StoreError::UnknownInstant(id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_retrieve_same_instant() {
        let store = Store::open(":memory:").unwrap();
        let rec = store.register_instant(1_700_000_000_500_000_000, Some("handoff"), true).unwrap();
        assert!(rec.id.starts_with("ctcl:instant:"));

        let fetched = store.get_instant(&rec.id).unwrap();
        assert_eq!(fetched.unix_ns, "1700000000500000000");
        assert_eq!(fetched.label.as_deref(), Some("handoff"));

        // also retrievable by the bare uuid, no prefix
        let bare = rec.id.trim_start_matches("ctcl:instant:");
        let fetched2 = store.get_instant(bare).unwrap();
        assert_eq!(fetched2.unix_ns, rec.unix_ns);
    }

    #[test]
    fn unknown_instant_errors_with_correct_code() {
        let store = Store::open(":memory:").unwrap();
        let err = store.get_instant("ctcl:instant:does-not-exist").unwrap_err();
        assert_eq!(err.code(), "UNKNOWN_INSTANT");
    }
}
