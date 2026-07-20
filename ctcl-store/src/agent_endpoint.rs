//! Agent Endpoint registry (Phase 4.5D, whitepaper §6.2/§10.3): where an
//! Agent Runtime can be reached for ACTIVE delivery (push), as opposed to it
//! polling CTCL itself (Phase 4.5B - still works with zero endpoints
//! registered; active delivery is additive, never a replacement).
//! Registering an endpoint is not enough on its own to make CTCL push to
//! it: it's always created disabled (§9.1 "預設停用"), and even once
//! enabled, `wake_delivery.rs`'s background thread only dispatches to it if
//! the `agent_wake.dispatch` capability scope is ALSO granted - two
//! independent gates before CTCL will spawn a process or make an outbound
//! HTTP call on an agent's behalf.

use crate::{Store, StoreError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEndpoint {
    pub agent_id: String,
    /// "local_process" | "loopback_http" - the only two transports this
    /// phase implements. §8.4's own priority order puts these first;
    /// `remote_webhook` is explicitly deferred (§9.4), `queue_adapter` is
    /// unspecified - neither is built here.
    pub transport: String,
    /// local_process: the canonical absolute path to a user-registered
    /// executable, verified to exist at registration time (§9.1: "路徑需
    /// 正規化" + "可執行檔必須由使用者明確登記"). loopback_http: a
    /// `http://127.0.0.1:<port>/...` (or `localhost`) URL (§9.2: "綁定
    /// localhost" - plain HTTP only, this project's dispatcher has no TLS).
    pub endpoint: String,
    /// loopback_http only: sent as `Authorization: Bearer <auth_ref>`
    /// (§9.2's "雙方 Bearer Token"). Unused for local_process.
    pub auth_ref: Option<String>,
    pub enabled: bool,
    pub allowed_event_kinds: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Store {
    /// Re-registering an existing `agent_id` updates its config but always
    /// resets `enabled` back to `false` - unlike a Trigger's "re-post
    /// rearms to active immediately," an endpoint always needs the
    /// deliberate follow-up `set_agent_endpoint_enabled(true)` call, since a
    /// changed `endpoint`/transport is exactly the moment a stale enabled
    /// flag pointing at the OLD target would be most dangerous.
    pub fn create_agent_endpoint(
        &self,
        agent_id: &str,
        transport: &str,
        endpoint: &str,
        auth_ref: Option<&str>,
        allowed_event_kinds: &[String],
    ) -> Result<AgentEndpoint, StoreError> {
        if agent_id.trim().is_empty() {
            return Err(StoreError::InvalidInput("agent_id must not be empty".into()));
        }
        let resolved_endpoint = match transport {
            "local_process" => {
                let canonical = std::fs::canonicalize(endpoint).map_err(|e| {
                    StoreError::InvalidInput(format!("local_process endpoint '{endpoint}' is not a real, reachable executable path: {e}"))
                })?;
                canonical.to_string_lossy().into_owned()
            }
            "loopback_http" => {
                let host = url_host(endpoint).ok_or_else(|| {
                    StoreError::InvalidInput(format!("loopback_http endpoint '{endpoint}' is not a valid http:// URL"))
                })?;
                if host != "127.0.0.1" && host != "localhost" && host != "[::1]" {
                    return Err(StoreError::InvalidInput(format!(
                        "loopback_http endpoint must be on 127.0.0.1/localhost (whitepaper §9.2 'bind localhost'), got host '{host}'"
                    )));
                }
                endpoint.to_string()
            }
            other => {
                return Err(StoreError::InvalidInput(format!(
                    "unsupported transport '{other}' - only local_process and loopback_http are implemented (remote_webhook is explicitly deferred, §9.4)"
                )));
            }
        };
        let allowed_json = serde_json::to_string(allowed_event_kinds)?;
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO agent_endpoints (agent_id, transport, endpoint, auth_ref, enabled, allowed_event_kinds_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?6)
             ON CONFLICT(agent_id) DO UPDATE SET transport=excluded.transport, endpoint=excluded.endpoint,
                auth_ref=excluded.auth_ref, enabled=0, allowed_event_kinds_json=excluded.allowed_event_kinds_json, updated_at=excluded.updated_at",
            rusqlite::params![agent_id, transport, resolved_endpoint, auth_ref, allowed_json, now],
        )?;
        self.get_agent_endpoint(agent_id)
    }

    pub fn get_agent_endpoint(&self, agent_id: &str) -> Result<AgentEndpoint, StoreError> {
        self.conn
            .query_row(
                "SELECT agent_id, transport, endpoint, auth_ref, enabled, allowed_event_kinds_json, created_at, updated_at FROM agent_endpoints WHERE agent_id = ?1",
                [agent_id],
                Self::row_to_agent_endpoint,
            )
            .map_err(|_| StoreError::InvalidInput(format!("unknown agent endpoint: {agent_id}")))?
    }

    pub fn list_agent_endpoints(&self) -> Result<Vec<AgentEndpoint>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT agent_id, transport, endpoint, auth_ref, enabled, allowed_event_kinds_json, created_at, updated_at FROM agent_endpoints ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([], Self::row_to_agent_endpoint)?.collect::<Result<Vec<_>, rusqlite::Error>>()?;
        rows.into_iter().collect()
    }

    pub fn set_agent_endpoint_enabled(&self, agent_id: &str, enabled: bool) -> Result<AgentEndpoint, StoreError> {
        let changed = self.conn.execute(
            "UPDATE agent_endpoints SET enabled=?1, updated_at=?2 WHERE agent_id=?3",
            rusqlite::params![enabled as i64, chrono::Utc::now().to_rfc3339(), agent_id],
        )?;
        if changed == 0 {
            return Err(StoreError::InvalidInput(format!("unknown agent endpoint: {agent_id}")));
        }
        self.get_agent_endpoint(agent_id)
    }

    fn row_to_agent_endpoint(row: &rusqlite::Row) -> rusqlite::Result<Result<AgentEndpoint, StoreError>> {
        let agent_id: String = row.get(0)?;
        let transport: String = row.get(1)?;
        let endpoint: String = row.get(2)?;
        let auth_ref: Option<String> = row.get(3)?;
        let enabled: i64 = row.get(4)?;
        let allowed_json: String = row.get(5)?;
        let created_at: String = row.get(6)?;
        let updated_at: String = row.get(7)?;
        Ok((|| {
            Ok(AgentEndpoint {
                agent_id,
                transport,
                endpoint,
                auth_ref,
                enabled: enabled != 0,
                allowed_event_kinds: serde_json::from_str(&allowed_json)?,
                created_at,
                updated_at,
            })
        })())
    }
}

/// Extracts the host from a `http://host[:port]/...` URL without pulling in
/// a full URL-parsing crate for one field. Deliberately narrow (`http://`
/// only, no `https://` - this project's dispatcher has no TLS support) and
/// only cares whether the host is a loopback address, per §9.2.
fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let host_port = rest.split('/').next()?;
    let host = host_port.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_port);
    Some(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn real_executable_path() -> String {
        std::env::current_exe().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn create_local_process_endpoint_canonicalizes_the_path_and_starts_disabled() {
        let store = Store::open(":memory:").unwrap();
        let path = real_executable_path();
        let ep = store.create_agent_endpoint("agent:primary", "local_process", &path, None, &[]).unwrap();
        assert_eq!(ep.transport, "local_process");
        assert!(!ep.enabled, "must start disabled per §9.1 'default off'");
        assert!(std::path::Path::new(&ep.endpoint).is_absolute(), "endpoint should be canonicalized to an absolute path");
    }

    #[test]
    fn create_local_process_endpoint_rejects_a_nonexistent_path() {
        let store = Store::open(":memory:").unwrap();
        let err = store.create_agent_endpoint("agent:primary", "local_process", "C:/definitely/not/a/real/path.exe", None, &[]).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)));
    }

    #[test]
    fn create_loopback_http_endpoint_accepts_127_0_0_1_and_localhost() {
        let store = Store::open(":memory:").unwrap();
        let a = store.create_agent_endpoint("agent:a", "loopback_http", "http://127.0.0.1:4400/wake", Some("secret"), &[]).unwrap();
        assert_eq!(a.transport, "loopback_http");
        assert_eq!(a.auth_ref.as_deref(), Some("secret"));
        let b = store.create_agent_endpoint("agent:b", "loopback_http", "http://localhost:4400/wake", None, &[]).unwrap();
        assert_eq!(b.endpoint, "http://localhost:4400/wake");
    }

    #[test]
    fn create_loopback_http_endpoint_rejects_a_non_loopback_host() {
        let store = Store::open(":memory:").unwrap();
        let err = store.create_agent_endpoint("agent:a", "loopback_http", "http://example.com/wake", None, &[]).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "must reject a non-loopback host per §9.2");
    }

    #[test]
    fn create_agent_endpoint_rejects_unsupported_transports() {
        let store = Store::open(":memory:").unwrap();
        let err = store.create_agent_endpoint("agent:a", "remote_webhook", "https://example.com", None, &[]).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "remote_webhook is explicitly deferred by §9.4, not silently accepted");
    }

    #[test]
    fn enable_then_disable_round_trips() {
        let store = Store::open(":memory:").unwrap();
        store.create_agent_endpoint("agent:a", "loopback_http", "http://127.0.0.1:4400/wake", None, &[]).unwrap();
        let enabled = store.set_agent_endpoint_enabled("agent:a", true).unwrap();
        assert!(enabled.enabled);
        let disabled = store.set_agent_endpoint_enabled("agent:a", false).unwrap();
        assert!(!disabled.enabled);
    }

    #[test]
    fn re_registering_an_endpoint_resets_enabled_to_false() {
        let store = Store::open(":memory:").unwrap();
        store.create_agent_endpoint("agent:a", "loopback_http", "http://127.0.0.1:4400/wake", None, &[]).unwrap();
        store.set_agent_endpoint_enabled("agent:a", true).unwrap();

        let re_registered = store.create_agent_endpoint("agent:a", "loopback_http", "http://127.0.0.1:5500/wake", None, &[]).unwrap();
        assert!(!re_registered.enabled, "changing the target must not silently keep delivery enabled");
        assert_eq!(re_registered.endpoint, "http://127.0.0.1:5500/wake");
    }

    #[test]
    fn set_enabled_on_unknown_agent_errors() {
        let store = Store::open(":memory:").unwrap();
        let err = store.set_agent_endpoint_enabled("agent:ghost", true).unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)));
    }

    #[test]
    fn list_returns_every_registered_endpoint() {
        let store = Store::open(":memory:").unwrap();
        store.create_agent_endpoint("agent:a", "loopback_http", "http://127.0.0.1:4400/wake", None, &[]).unwrap();
        store.create_agent_endpoint("agent:b", "local_process", &real_executable_path(), None, &[]).unwrap();
        assert_eq!(store.list_agent_endpoints().unwrap().len(), 2);
    }
}
