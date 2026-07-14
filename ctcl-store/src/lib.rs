//! ctcl-store: SQLite-backed persistence for the CTCL Temporal Port app.
//! The local, offline equivalent of the hosted Worker's CTCL_KV-backed instant
//! registry, custom-system registry, and Temporal Groups - same concepts, same
//! record shapes, different storage engine (a local SQLite file instead of
//! Cloudflare KV), so the desktop app doesn't need a network connection to
//! remember what it was told.

mod error;
pub mod audit;
pub mod device_observer;
pub mod group;
pub mod instant;
pub mod settings;
pub mod system;
pub mod trigger;

pub use audit::AuditEntry;
pub use device_observer::{DeviceEvent, EventKind};
pub use error::StoreError;
pub use group::GroupRecord;
pub use instant::InstantRecord;
pub use settings::{Settings, ALL_SCOPES};
pub use system::SystemRecord;
pub use trigger::{ActionKind, Operator, Trigger, TriggerAction, TriggerKind, TriggerStatus};

use rusqlite::Connection;

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) a SQLite database at `path` and ensure the
    /// schema exists. Use ":memory:" for an ephemeral, test-only store.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        let store = Store { conn };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS instants (
                id              TEXT PRIMARY KEY,
                unix_ns         TEXT NOT NULL,
                registered_at   TEXT NOT NULL,
                label           TEXT,
                from_wall_clock INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS systems (
                id                TEXT PRIMARY KEY,
                parent            TEXT NOT NULL DEFAULT 'ctcl:system:unix',
                epoch_parent_sec  REAL NOT NULL,
                rate_json         TEXT NOT NULL,
                offset_sec        REAL NOT NULL DEFAULT 0,
                created_at        TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS groups (
                id            TEXT PRIMARY KEY,
                members_json  TEXT NOT NULL,
                owner         TEXT,
                version       INTEGER NOT NULL DEFAULT 1,
                created_at    TEXT NOT NULL,
                updated_at    TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS settings (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS audit_log (
                id      INTEGER PRIMARY KEY AUTOINCREMENT,
                at      TEXT NOT NULL,
                method  TEXT NOT NULL,
                path    TEXT NOT NULL,
                scope   TEXT,
                allowed INTEGER NOT NULL,
                reason  TEXT
            );
            CREATE TABLE IF NOT EXISTS device_events (
                id           INTEGER PRIMARY KEY AUTOINCREMENT,
                at           TEXT NOT NULL,
                kind         TEXT NOT NULL,
                delta_ms     INTEGER NOT NULL,
                wall_gap_ms  INTEGER NOT NULL,
                mono_gap_ms  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS triggers (
                id            TEXT PRIMARY KEY,
                kind          TEXT NOT NULL,
                system_id     TEXT,
                operator      TEXT NOT NULL,
                target_value  REAL NOT NULL,
                action_json   TEXT NOT NULL,
                status        TEXT NOT NULL,
                created_at    TEXT NOT NULL,
                fired_at      TEXT
            );
            ",
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_in_memory_and_creates_schema() {
        let store = Store::open(":memory:").unwrap();
        // schema exists and is queryable - a no-op query proves the tables are there
        let count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM instants", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}
