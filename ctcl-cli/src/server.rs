//! A tiny local-only HTTP server so a non-technical user can check what
//! ctcl-core does from a browser instead of a terminal. Deliberately minimal
//! (tiny_http, synchronous, no async runtime) - this is a Phase 0 preview, not
//! the real local API/permission model that Phase 2 calls for.

use ctcl_core::{from_ns, now_view, to_ns};
use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

const INDEX_HTML: &str = include_str!("index.html");

pub fn serve(port: u16) {
    let addr = format!("127.0.0.1:{port}");
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    println!("CTCL desktop preview running at http://{addr}/  (this machine only - press Ctrl+C to stop)");
    open_browser(&format!("http://{addr}/"));

    for mut request in server.incoming_requests() {
        let url = request.url().to_string();
        let method = request.method().clone();

        let (status, is_json, body) = match (&method, url.as_str()) {
            (Method::Get, "/") | (Method::Get, "/index.html") => (200, false, INDEX_HTML.to_string()),
            (Method::Get, "/api/now") => match now_view() {
                Ok(v) => (200, true, json!({ "ok": true, "data": v }).to_string()),
                Err(e) => (400, true, error_body(&e)),
            },
            (Method::Post, "/api/convert") => {
                let mut buf = String::new();
                let _ = request.as_reader().read_to_string(&mut buf);
                let parsed: serde_json::Value = serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);
                let value = parsed.get("value").and_then(|v| v.as_str()).unwrap_or("");
                let from = parsed.get("from").and_then(|v| v.as_str()).unwrap_or("rfc3339");
                let to = parsed.get("to").and_then(|v| v.as_str()).unwrap_or("rfc3339");
                let tz = parsed.get("tz").and_then(|v| v.as_str());
                match to_ns(value, from).and_then(|ns| from_ns(ns, to, tz).map(|out| (ns, out))) {
                    Ok((ns, out)) => (
                        200,
                        true,
                        json!({ "ok": true, "data": { "canonical_unix_ns": ns.to_string(), "output": out } }).to_string(),
                    ),
                    Err(e) => (400, true, error_body(&e)),
                }
            }
            _ => (
                404,
                true,
                json!({ "ok": false, "error": { "code": "NOT_FOUND", "message": format!("no route: {url}") } }).to_string(),
            ),
        };

        let content_type = if is_json { "application/json; charset=utf-8" } else { "text/html; charset=utf-8" };
        let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap();
        let response = Response::from_string(body).with_status_code(status).with_header(header);
        let _ = request.respond(response);
    }
}

fn error_body(e: &ctcl_core::CtclError) -> String {
    json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string() } }).to_string()
}

/// Open the user's default browser to `url` - so double-clicking the .exe is
/// enough; nobody has to type a URL, let alone a command. Best-effort: if the
/// platform command isn't available, the server still runs and prints the URL.
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();

    if result.is_err() {
        eprintln!("(could not auto-open a browser - open {url} manually)");
    }
}
