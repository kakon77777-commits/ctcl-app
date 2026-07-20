//! ctcl-store: SQLite-backed persistence for the CTCL Temporal Port app.
//! The local, offline equivalent of the hosted Worker's CTCL_KV-backed instant
//! registry, custom-system registry, and Temporal Groups - same concepts, same
//! record shapes, different storage engine (a local SQLite file instead of
//! Cloudflare KV), so the desktop app doesn't need a network connection to
//! remember what it was told.

mod error;
pub mod agent_endpoint;
pub mod audit;
pub mod decision_receipt;
pub mod device_observer;
pub mod group;
pub mod instant;
pub mod settings;
pub mod system;
pub mod trigger;
pub mod wake_event;

pub use agent_endpoint::AgentEndpoint;
pub use audit::AuditEntry;
pub use decision_receipt::DecisionReceipt;
pub use device_observer::{DeviceEvent, EventKind};
pub use error::StoreError;
pub use group::GroupRecord;
pub use instant::InstantRecord;
pub use settings::{Settings, ALL_SCOPES};
pub use system::SystemRecord;
pub use trigger::{ActionKind, Operator, Trigger, TriggerAction, TriggerKind, TriggerStatus};
pub use wake_event::{WakeEvent, WakeEventStatus};

use rusqlite::Connection;

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) a SQLite database at `path` and ensure the
    /// schema exists. Use ":memory:" for an ephemeral, test-only store.
    ///
    /// Sets a 5s busy_timeout: as of Phase 4.5C, `ctcl-mcp` can open the same
    /// on-disk file `ctcl-desktop` already has open (two separate OS
    /// processes, same db). rusqlite's default is 0 - a write that collides
    /// with another connection's transaction fails immediately with
    /// SQLITE_BUSY instead of waiting briefly, which is a routine occurrence
    /// with two live writers and shouldn't surface as a hard error.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
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
            CREATE TABLE IF NOT EXISTS wake_events (
                event_id          TEXT PRIMARY KEY,
                trigger_id        TEXT,
                agent_id          TEXT NOT NULL,
                reason            TEXT NOT NULL,
                fired_json        TEXT NOT NULL,
                observed_json     TEXT NOT NULL,
                payload_json      TEXT NOT NULL,
                status            TEXT NOT NULL,
                attempt_count     INTEGER NOT NULL DEFAULT 0,
                created_at        TEXT NOT NULL,
                acknowledged_at   TEXT,
                completed_at      TEXT,
                delivered_at      TEXT,
                next_attempt_at   TEXT,
                last_error        TEXT,
                idempotency_key   TEXT NOT NULL UNIQUE
            );
            CREATE TABLE IF NOT EXISTS decision_receipts (
                receipt_id      TEXT PRIMARY KEY,
                event_id        TEXT NOT NULL,
                agent_id        TEXT NOT NULL,
                run_id          TEXT NOT NULL,
                decision        TEXT NOT NULL,
                summary         TEXT,
                tool_calls_json TEXT,
                next_wake_json  TEXT,
                cost_json       TEXT,
                created_at      TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS agent_endpoints (
                agent_id                 TEXT PRIMARY KEY,
                transport                TEXT NOT NULL,
                endpoint                 TEXT NOT NULL,
                auth_ref                 TEXT,
                enabled                  INTEGER NOT NULL DEFAULT 0,
                allowed_event_kinds_json TEXT NOT NULL,
                created_at               TEXT NOT NULL,
                updated_at               TEXT NOT NULL
            );
            ",
        )?;
        // wake_events gained delivered_at/next_attempt_at/last_error in Phase
        // 4.5D. CREATE TABLE IF NOT EXISTS only helps fresh databases - a
        // db file from 4.5A-C already has the table without these columns,
        // so add them defensively if missing (idempotent: safe to run on a
        // database that already has them, and on one that's brand new).
        self.ensure_column("wake_events", "delivered_at", "delivered_at TEXT")?;
        self.ensure_column("wake_events", "next_attempt_at", "next_attempt_at TEXT")?;
        self.ensure_column("wake_events", "last_error", "last_error TEXT")?;
        Ok(())
    }

    fn ensure_column(&self, table: &str, column: &str, add_column_ddl: &str) -> Result<(), StoreError> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing = stmt.query_map([], |row| row.get::<_, String>(1))?.collect::<Result<Vec<String>, _>>()?;
        if !existing.iter().any(|c| c == column) {
            self.conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {add_column_ddl}"), [])?;
        }
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
