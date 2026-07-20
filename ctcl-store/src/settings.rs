//! App settings: everything the Temporal Port App whitepaper's Phase 2+
//! (Local Gateway, Capability Scope, Security Model) and later phases (Device
//! Clock Observer, Triggers) expose as a controllable variable. Persisted as
//! one JSON document so the schema can grow without migrations.
//!
//! Honesty discipline, matching the rest of this project: a field that isn't
//! backed by real behavior yet is labelled `implemented: false` in
//! `Settings::status()` rather than silently doing nothing when toggled.

use crate::{Store, StoreError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const SETTINGS_KEY: &str = "app_settings";

/// The whitepaper's own capability-scope list (§12.1), verbatim.
pub const ALL_SCOPES: &[&str] = &[
    "instant.read",
    "instant.create",
    "convert.execute",
    "systems.read",
    "systems.write",
    "groups.read",
    "groups.write",
    "triggers.read",
    "triggers.write",
    "device_clock.read",
    "history.read",
    "wake_events.read",
    "wake_events.ack",
    "triggers.cancel",
    "wake_events.complete",
    "decision_receipts.write",
    "agents.read",
    "agents.write",
    "agent_wake.dispatch",
];

/// §12.2 "Granted Capability -> min": only low-risk read/execute scopes are
/// granted by default; write/trigger/device scopes require explicit opt-in.
const DEFAULT_GRANTED: &[&str] = &["instant.read", "convert.execute", "systems.read", "groups.read"];

fn default_scopes() -> BTreeMap<String, bool> {
    ALL_SCOPES.iter().map(|s| (s.to_string(), DEFAULT_GRANTED.contains(s))).collect()
}

fn generate_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// Every field falls back to Settings::default()'s value if absent from a
// saved JSON blob. Without this, a Settings document persisted by an OLDER
// build (missing a field this phase adds - `wake_delivery_enabled` today,
// but the same latent risk already existed for every field Phase 3/4 added)
// would fail to deserialize AT ALL on the next `get_settings()` call,
// breaking nearly every operation in the app. Struct-level default reuses
// the existing `impl Default for Settings` below rather than per-field
// literals, so it stays in sync automatically.
#[serde(default)]
pub struct Settings {
    // ---- Phase 2: Local Gateway / Localhost API (whitepaper §7.2) ----
    /// §7.2 "默認關閉" - disabled by default. No socket is bound at all while false.
    pub local_api_enabled: bool,
    pub local_api_port: u16,
    /// Simple bearer token - loopback-only, not a substitute for real OAuth if
    /// this ever needs to serve beyond localhost (it shouldn't).
    pub local_api_token: String,

    // ---- Phase 2: Capability Scope (§12.1/§12.2) ----
    pub scopes: BTreeMap<String, bool>,

    // ---- Phase 2/§13: Security Model ----
    pub audit_log_enabled: bool,

    // ---- Phase 3: Device Clock Observer (whitepaper §4.2/§10) ----
    // Off by default, same discipline as the local API (§7.2 "default off").
    // When on, a background thread samples (wall clock, monotonic clock) every
    // device_clock_sample_interval_s and classifies drift / sleep-wake /
    // rollback via ctcl_store::device_observer::classify_gap.
    pub device_clock_observer_enabled: bool,
    pub device_clock_drift_threshold_s: f64,
    pub device_clock_sample_interval_s: u64,

    // ---- Phase 4: Trigger Engine (whitepaper §4.3/§9.4) ----
    // Off by default, same discipline as the local API and device observer.
    // When on, a background thread polls ctcl_store::trigger::Store::due_triggers
    // every trigger_check_interval_s and dispatches fired actions.
    pub triggers_enabled: bool,
    pub trigger_check_interval_s: u64,

    // ---- Phase 4.5D: Wake Delivery Worker (whitepaper §8) ----
    // Off by default, same discipline as every other background thread.
    // When on, polls ctcl_store::wake_event::Store::due_for_delivery every
    // wake_delivery_check_interval_s and actively pushes to any ENABLED
    // agent_endpoints row - but only if the agent_wake.dispatch scope is
    // ALSO granted (checked by the thread itself, not just at the API
    // layer) - three independent gates before anything gets dispatched:
    // this toggle, the scope, and the per-endpoint enabled flag.
    pub wake_delivery_enabled: bool,
    pub wake_delivery_check_interval_s: u64,

    // ---- §12.3: Local Data Protection - NOT YET IMPLEMENTED ----
    pub encrypted_storage_enabled: bool,
    pub retention_days: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            local_api_enabled: false,
            local_api_port: 4180,
            local_api_token: generate_token(),
            scopes: default_scopes(),
            audit_log_enabled: true,
            device_clock_observer_enabled: false,
            device_clock_drift_threshold_s: 5.0,
            device_clock_sample_interval_s: 20,
            triggers_enabled: false,
            trigger_check_interval_s: 5,
            wake_delivery_enabled: false,
            wake_delivery_check_interval_s: 5,
            encrypted_storage_enabled: false,
            retention_days: None,
        }
    }
}

/// One row per setting the UI needs to honestly render as "real" vs "coming
/// later" - the Settings panel is the visual roadmap Neo asked for.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureStatus {
    pub key: &'static str,
    pub phase: &'static str,
    pub implemented: bool,
}

impl Settings {
    pub fn status() -> Vec<FeatureStatus> {
        vec![
            FeatureStatus { key: "local_api", phase: "Phase 2", implemented: true },
            FeatureStatus { key: "scopes", phase: "Phase 2", implemented: true },
            FeatureStatus { key: "audit_log", phase: "Phase 2", implemented: true },
            FeatureStatus { key: "device_clock_observer", phase: "Phase 3", implemented: true },
            FeatureStatus { key: "triggers", phase: "Phase 4", implemented: true },
            FeatureStatus { key: "wake_events", phase: "Phase 4.5A", implemented: true },
            FeatureStatus { key: "decision_receipts", phase: "Phase 4.5B", implemented: true },
            FeatureStatus { key: "wake_delivery", phase: "Phase 4.5D", implemented: true },
            FeatureStatus { key: "encrypted_storage", phase: "\u{00a7}12.3", implemented: false },
            FeatureStatus { key: "retention_policy", phase: "\u{00a7}12.3", implemented: false },
        ]
    }

    pub fn is_granted(&self, scope: &str) -> bool {
        self.scopes.get(scope).copied().unwrap_or(false)
    }
}

impl Store {
    pub fn get_settings(&self) -> Result<Settings, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [SETTINGS_KEY], |r| r.get(0))
            .ok();
        match raw {
            Some(json) => Ok(serde_json::from_str(&json)?),
            None => {
                let defaults = Settings::default();
                self.save_settings(&defaults)?;
                Ok(defaults)
            }
        }
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), StoreError> {
        let json = serde_json::to_string(settings)?;
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![SETTINGS_KEY, json],
        )?;
        Ok(())
    }

    pub fn regenerate_api_token(&self) -> Result<Settings, StoreError> {
        let mut settings = self.get_settings()?;
        settings.local_api_token = generate_token();
        self.save_settings(&settings)?;
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_by_default_per_whitepaper_7_2() {
        let store = Store::open(":memory:").unwrap();
        let settings = store.get_settings().unwrap();
        assert!(!settings.local_api_enabled, "local API must default to disabled");
        assert!(settings.is_granted("instant.read"));
        assert!(!settings.is_granted("systems.write"), "write scopes must default to off (§12.2)");
        assert!(!settings.is_granted("triggers.write"));
        assert!(!settings.device_clock_observer_enabled, "device observer must default to disabled, same discipline as the local API");
    }

    #[test]
    fn device_clock_observer_is_marked_implemented() {
        let statuses = Settings::status();
        let observer = statuses.iter().find(|s| s.key == "device_clock_observer").unwrap();
        assert!(observer.implemented, "Phase 3 shipped - this must flip to true, not stay a roadmap placeholder");
    }

    #[test]
    fn settings_persist_across_reopen() {
        let store = Store::open(":memory:").unwrap(); // note: :memory: doesn't persist across Store::open calls, this checks same-connection persistence
        let mut settings = store.get_settings().unwrap();
        settings.local_api_enabled = true;
        settings.local_api_port = 5555;
        store.save_settings(&settings).unwrap();

        let reloaded = store.get_settings().unwrap();
        assert!(reloaded.local_api_enabled);
        assert_eq!(reloaded.local_api_port, 5555);
    }

    #[test]
    fn regenerate_token_changes_it() {
        let store = Store::open(":memory:").unwrap();
        let before = store.get_settings().unwrap().local_api_token;
        let after = store.regenerate_api_token().unwrap().local_api_token;
        assert_ne!(before, after);
    }

    #[test]
    fn all_scopes_have_a_default_value() {
        let store = Store::open(":memory:").unwrap();
        let settings = store.get_settings().unwrap();
        for scope in ALL_SCOPES {
            assert!(settings.scopes.contains_key(*scope), "missing default for {scope}");
        }
    }

    /// A settings JSON blob saved by an OLDER build - before Phase 4.5D
    /// added wake_delivery_enabled/wake_delivery_check_interval_s - must
    /// still deserialize, falling back to Settings::default()'s values for
    /// the fields it doesn't have, rather than erroring out on every
    /// get_settings() call (which would break nearly everything in the app
    /// the next time Neo opens a database from before this phase shipped).
    #[test]
    fn get_settings_tolerates_a_json_blob_missing_newer_fields() {
        let store = Store::open(":memory:").unwrap();
        // Written directly into the settings table, bypassing save_settings
        // (which would just re-serialize a full modern Settings) - this is
        // what a REAL pre-4.5D blob on disk actually looks like: no
        // device_clock_*/triggers_*/wake_delivery_*/encrypted_storage_*/
        // retention_days fields at all.
        let old_shape = r#"{"local_api_enabled":true,"local_api_port":4180,"local_api_token":"abc123","scopes":{"instant.read":true},"audit_log_enabled":true}"#;
        store
            .conn
            .execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![SETTINGS_KEY, old_shape],
            )
            .unwrap();

        let loaded = store.get_settings().unwrap();
        assert!(loaded.local_api_enabled, "fields present in the old blob must still be honored");
        assert!(!loaded.wake_delivery_enabled, "a field the old blob never had must fall back to Settings::default(), not error");
        assert_eq!(loaded.wake_delivery_check_interval_s, 5);
    }
}
