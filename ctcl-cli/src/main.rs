//! CTCL Temporal Port CLI - Phase 0's command-line surface. Mirrors the hosted
//! Worker's GET /v1/now and POST /v1/convert for local, offline use. This is the
//! device's own local clock, not the verified/synchronized instant the hosted
//! commoninstant.org API provides - see the `note` field in `ctcl now`'s output.
//!
//! `instant`/`system`/`group` subcommands are backed by a local SQLite file
//! (ctcl-store) - the offline equivalent of the Worker's CTCL_KV registry.

use clap::{Parser, Subcommand};
use ctcl_core::{from_ns, now_view, to_ns};
use serde_json::json;

mod commands;
mod server;

const DEFAULT_DB: &str = "ctcl-data.sqlite3";

#[derive(Parser)]
#[command(name = "ctcl", version, about = "CTCL Temporal Port - local reference instant + time transformation")]
struct Cli {
    /// Path to the local SQLite store (created if it doesn't exist)
    #[arg(long, global = true, default_value = DEFAULT_DB)]
    db: String,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print the local device instant (encodings + timescales)
    Now,
    /// Convert a time value across encodings/timezones
    Convert {
        #[arg(long)]
        value: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        tz: Option<String>,
    },
    /// Run a local-only web preview at http://127.0.0.1:<port>/ (no terminal needed after this)
    Serve {
        #[arg(long, default_value_t = 4179)]
        port: u16,
    },
    /// Register/retrieve a shared reference instant (persisted, survives restarts)
    Instant {
        #[command(subcommand)]
        action: InstantAction,
    },
    /// Persistent custom temporal systems (constant-rate world clocks, life-history, etc.)
    System {
        #[command(subcommand)]
        action: SystemAction,
    },
    /// Temporal Groups - "One Instant, Many Systems"
    Group {
        #[command(subcommand)]
        action: GroupAction,
    },
}

#[derive(Subcommand)]
enum InstantAction {
    /// Register an instant (default: right now)
    Register {
        #[arg(long)]
        value: Option<String>,
        #[arg(long, default_value = "unix_s")]
        from: String,
        #[arg(long)]
        label: Option<String>,
    },
    /// Retrieve a previously-registered instant by id
    Get { id: String },
}

#[derive(Subcommand)]
enum SystemAction {
    /// Create (or overwrite) a constant-rate custom system
    Create {
        #[arg(long)]
        id: String,
        /// Epoch, in unix seconds
        #[arg(long)]
        epoch: String,
        /// Rate multiplier (1.0 = real time, 20.0 = 20x, etc.)
        #[arg(long, default_value_t = 1.0)]
        rate: f64,
        #[arg(long, default_value_t = 0.0)]
        offset: f64,
    },
    Get { id: String },
    List,
    /// Current local time in the system, evaluated against the device's wall clock
    Now { id: String },
}

#[derive(Subcommand)]
enum GroupAction {
    /// Create (or update - bumps version) a Temporal Group
    Create {
        #[arg(long)]
        id: String,
        /// Comma-separated members: "utc"|"posix"|"tai"|"gps"|"tz:<IANA>"|<system id>
        #[arg(long, value_delimiter = ',')]
        members: Vec<String>,
    },
    Get { id: String },
    List,
    /// Project one instant across every member of the group
    Expand {
        id: String,
        #[arg(long)]
        instant_id: Option<String>,
        #[arg(long)]
        value: Option<String>,
        #[arg(long, default_value = "unix_s")]
        from: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Now => match now_view() {
            Ok(view) => {
                let out = json!({
                    "ok": true,
                    "data": {
                        "encodings": view.encodings,
                        "timescales": view.timescales,
                        "source": "local_device_clock",
                        "note": "This is the local device wall clock, not a verified/synchronized instant. For the hosted, honesty-labelled reference instant, call GET https://commoninstant.org/v1/now instead.",
                    }
                });
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            }
            Err(e) => print_error(&e.code().to_string(), &e.to_string()),
        },
        Commands::Convert { value, from, to, tz } => match to_ns(&value, &from) {
            Ok(ns) => match from_ns(ns, &to, tz.as_deref()) {
                Ok(out_value) => {
                    let out = json!({
                        "ok": true,
                        "data": {
                            "input": {"value": value, "encoding": from},
                            "output": {"value": out_value, "encoding": to, "timezone": tz},
                            "canonical_unix_ns": ns.to_string(),
                        }
                    });
                    println!("{}", serde_json::to_string_pretty(&out).unwrap());
                }
                Err(e) => print_error(&e.code().to_string(), &e.to_string()),
            },
            Err(e) => print_error(&e.code().to_string(), &e.to_string()),
        },
        Commands::Serve { port } => server::serve(port),

        Commands::Instant { action } => match action {
            InstantAction::Register { value, from, label } => commands::instant_register(&cli.db, value, &from, label),
            InstantAction::Get { id } => commands::instant_get(&cli.db, &id),
        },
        Commands::System { action } => match action {
            SystemAction::Create { id, epoch, rate, offset } => commands::system_create(&cli.db, &id, &epoch, rate, offset),
            SystemAction::Get { id } => commands::system_get(&cli.db, &id),
            SystemAction::List => commands::system_list(&cli.db),
            SystemAction::Now { id } => commands::system_now(&cli.db, &id),
        },
        Commands::Group { action } => match action {
            GroupAction::Create { id, members } => commands::group_create(&cli.db, &id, members),
            GroupAction::Get { id } => commands::group_get(&cli.db, &id),
            GroupAction::List => commands::group_list(&cli.db),
            GroupAction::Expand { id, instant_id, value, from } => commands::group_expand(&cli.db, &id, instant_id, value, &from),
        },
    }
}

fn print_error(code: &str, message: &str) {
    let out = json!({ "ok": false, "error": { "code": code, "message": message } });
    eprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(1);
}
