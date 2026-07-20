//! ctcl-mcp: Local MCP server (Phase 4.5C) - a stdio-transport binary an
//! Agent Runtime spawns as its own local child process (whitepaper
//! §9.1/§12.1's "Local MCP" deployment form).
//!
//! Opens the SAME SQLite file `ctcl-desktop` uses by default: the whole
//! point of this server is to see the live Triggers/WakeEvents the desktop
//! app's background `trigger_engine.rs` thread produces, so a disconnected,
//! empty database would make every `ctcl.list_wake_events` call pointless.
//! Override with `--db <path>` for testing or a non-default install.

mod server;

use ctcl_store::Store;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use server::CtclMcpServer;
use std::sync::{Arc, Mutex};

fn parse_db_path() -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == "--db")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "ctcl-desktop-data.sqlite3".to_string())
}

#[tokio::main]
async fn main() {
    let db_path = parse_db_path();
    let store = Store::open(&db_path).unwrap_or_else(|e| {
        eprintln!("ctcl-mcp: failed to open local store at {db_path}: {e}");
        std::process::exit(1);
    });
    let store = Arc::new(Mutex::new(store));
    let server = CtclMcpServer::new(store);

    let service = server.serve(stdio()).await.unwrap_or_else(|e| {
        eprintln!("ctcl-mcp: failed to start serving: {e}");
        std::process::exit(1);
    });

    if let Err(e) = service.waiting().await {
        eprintln!("ctcl-mcp: service loop ended with error: {e}");
        std::process::exit(1);
    }
}
