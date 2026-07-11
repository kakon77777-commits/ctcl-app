//! CTCL Temporal Port CLI - Phase 0's command-line surface. Mirrors the hosted
//! Worker's GET /v1/now and POST /v1/convert for local, offline use. This is the
//! device's own local clock, not the verified/synchronized instant the hosted
//! commoninstant.org API provides - see the `note` field in `ctcl now`'s output.

use clap::{Parser, Subcommand};
use ctcl_core::{from_ns, now_view, to_ns};
use serde_json::json;

mod server;

#[derive(Parser)]
#[command(name = "ctcl", version, about = "CTCL Temporal Port - local reference instant + time transformation")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Print the local device instant (encodings + timescales)
    Now,
    /// Convert a time value across encodings/timezones
    Convert {
        /// The value to convert, e.g. "1783420000.5" or an RFC3339 string
        #[arg(long)]
        value: String,
        /// Input encoding: unix_s | unix_ms | unix_us | unix_ns | rfc3339
        #[arg(long)]
        from: String,
        /// Output encoding: unix_s | unix_ms | unix_us | unix_ns | rfc3339
        #[arg(long)]
        to: String,
        /// Output IANA timezone (only affects rfc3339 output), e.g. Asia/Taipei
        #[arg(long)]
        tz: Option<String>,
    },
    /// Run a local-only web preview at http://127.0.0.1:<port>/ (no terminal needed after this)
    Serve {
        #[arg(long, default_value_t = 4179)]
        port: u16,
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
    }
}

fn print_error(code: &str, message: &str) {
    let out = json!({ "ok": false, "error": { "code": code, "message": message } });
    eprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(1);
}
