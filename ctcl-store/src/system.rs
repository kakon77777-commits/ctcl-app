//! Persistent custom temporal systems - the local equivalent of the Worker's
//! POST/GET /v1/systems. Re-posting the same id overwrites the definition
//! (unlike groups, systems aren't versioned - matches the Worker's behavior).

use crate::{Store, StoreError};
use ctcl_core::{Rate, TemporalSystem};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemRecord {
    pub id: String,
    pub parent: String,
    pub epoch_parent_sec: f64,
    pub rate: Rate,
    pub offset: f64,
    pub created_at: String,
}

impl Store {
    pub fn create_system(
        &self,
        id: &str,
        parent: Option<&str>,
        epoch_parent_sec: f64,
        rate: Rate,
        offset: f64,
    ) -> Result<SystemRecord, StoreError> {
        if id.trim().is_empty() {
            return Err(StoreError::InvalidInput("system id must not be empty".into()));
        }
        let parent = parent.unwrap_or("ctcl:system:unix").to_string();
        let rate_json = serde_json::to_string(&rate)?;
        let created_at = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO systems (id, parent, epoch_parent_sec, rate_json, offset_sec, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET parent=excluded.parent, epoch_parent_sec=excluded.epoch_parent_sec,
                rate_json=excluded.rate_json, offset_sec=excluded.offset_sec",
            rusqlite::params![id, parent, epoch_parent_sec, rate_json, offset, created_at],
        )?;
        Ok(SystemRecord { id: id.to_string(), parent, epoch_parent_sec, rate, offset, created_at })
    }

    pub fn get_system(&self, id: &str) -> Result<SystemRecord, StoreError> {
        self.conn
            .query_row(
                "SELECT id, parent, epoch_parent_sec, rate_json, offset_sec, created_at FROM systems WHERE id = ?1",
                [id],
                |row| {
                    let rate_json: String = row.get(3)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                        rate_json,
                        row.get::<_, f64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .map_err(|_| StoreError::UnknownSystem(id.to_string()))
            .and_then(|(id, parent, epoch_parent_sec, rate_json, offset, created_at)| {
                let rate: Rate = serde_json::from_str(&rate_json)?;
                Ok(SystemRecord { id, parent, epoch_parent_sec, rate, offset, created_at })
            })
    }

    pub fn list_systems(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare("SELECT id FROM systems ORDER BY id")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Current local time in a stored system, given the parent-timeline instant
    /// (unix seconds) - mirrors the Worker's GET /v1/systems/{id}/now.
    pub fn system_now(&self, id: &str, parent_sec: f64) -> Result<(f64, ctcl_core::LocalTimeExtra), StoreError> {
        let rec = self.get_system(id)?;
        let sys = TemporalSystem { id: rec.id, epoch_parent_sec: rec.epoch_parent_sec, rate: rec.rate, offset: rec.offset };
        Ok(sys.local_seconds(parent_sec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctcl_core::Rate;

    #[test]
    fn create_get_and_evaluate_a_system() {
        let store = Store::open(":memory:").unwrap();
        store
            .create_system("user:game_world", None, 1_700_000_000.0, Rate::Constant { value: 20.0 }, 0.0)
            .unwrap();

        let rec = store.get_system("user:game_world").unwrap();
        assert_eq!(rec.parent, "ctcl:system:unix");

        let (local, _) = store.system_now("user:game_world", 1_700_000_100.0).unwrap();
        assert_eq!(local, 2000.0);
    }

    #[test]
    fn recreate_overwrites_definition() {
        let store = Store::open(":memory:").unwrap();
        store.create_system("s", None, 0.0, Rate::Constant { value: 1.0 }, 0.0).unwrap();
        store.create_system("s", None, 0.0, Rate::Constant { value: 99.0 }, 0.0).unwrap();
        let rec = store.get_system("s").unwrap();
        match rec.rate {
            Rate::Constant { value } => assert_eq!(value, 99.0),
            _ => panic!("expected constant rate"),
        }
    }

    #[test]
    fn list_systems_persists_across_creates() {
        let store = Store::open(":memory:").unwrap();
        store.create_system("a", None, 0.0, Rate::Constant { value: 1.0 }, 0.0).unwrap();
        store.create_system("b", None, 0.0, Rate::Constant { value: 1.0 }, 0.0).unwrap();
        let ids = store.list_systems().unwrap();
        assert_eq!(ids, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn unknown_system_errors_with_correct_code() {
        let store = Store::open(":memory:").unwrap();
        let err = store.get_system("nope").unwrap_err();
        assert_eq!(err.code(), "UNKNOWN_SYSTEM");
    }
}
