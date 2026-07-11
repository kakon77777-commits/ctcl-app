//! Handlers for the persistent instant/system/group subcommands. Mirrors the
//! hosted Worker's POST/GET /v1/instants, /v1/systems, /v1/temporal-groups,
//! backed by ctcl-store's local SQLite file instead of Cloudflare KV.

use ctcl_core::{to_ns, Rate};
use ctcl_store::Store;
use serde_json::json;

fn print_ok(data: serde_json::Value) {
    println!("{}", serde_json::to_string_pretty(&json!({ "ok": true, "data": data })).unwrap());
}

fn print_store_err(e: &ctcl_store::StoreError) -> ! {
    eprintln!(
        "{}",
        serde_json::to_string_pretty(&json!({ "ok": false, "error": { "code": e.code(), "message": e.to_string() } })).unwrap()
    );
    std::process::exit(1);
}

fn open_store(db_path: &str) -> Store {
    Store::open(db_path).unwrap_or_else(|e| {
        eprintln!("failed to open store at {db_path}: {e}");
        std::process::exit(1);
    })
}

pub fn instant_register(db_path: &str, value: Option<String>, from: &str, label: Option<String>) {
    let store = open_store(db_path);
    let (ns, from_wall_clock) = match value {
        Some(v) => match to_ns(&v, from) {
            Ok(ns) => (ns, false),
            Err(e) => { eprintln!("invalid value: {e}"); std::process::exit(1); }
        },
        None => (ctcl_core::now_ns(), true),
    };
    match store.register_instant(ns, label.as_deref(), from_wall_clock) {
        Ok(rec) => print_ok(json!(rec)),
        Err(e) => print_store_err(&e),
    }
}

pub fn instant_get(db_path: &str, id: &str) {
    let store = open_store(db_path);
    match store.get_instant(id) {
        Ok(rec) => print_ok(json!(rec)),
        Err(e) => print_store_err(&e),
    }
}

pub fn system_create(db_path: &str, id: &str, epoch_unix_s: &str, rate: f64, offset: f64) {
    let store = open_store(db_path);
    let epoch_ns = match to_ns(epoch_unix_s, "unix_s") {
        Ok(ns) => ns,
        Err(e) => { eprintln!("invalid epoch: {e}"); std::process::exit(1); }
    };
    let epoch_sec = epoch_ns as f64 / ctcl_core::encoding::NS_PER_S as f64;
    match store.create_system(id, None, epoch_sec, Rate::Constant { value: rate }, offset) {
        Ok(rec) => print_ok(json!(rec)),
        Err(e) => print_store_err(&e),
    }
}

pub fn system_get(db_path: &str, id: &str) {
    let store = open_store(db_path);
    match store.get_system(id) {
        Ok(rec) => print_ok(json!(rec)),
        Err(e) => print_store_err(&e),
    }
}

pub fn system_list(db_path: &str) {
    let store = open_store(db_path);
    match store.list_systems() {
        Ok(ids) => print_ok(json!({ "systems": ids })),
        Err(e) => print_store_err(&e),
    }
}

pub fn system_now(db_path: &str, id: &str) {
    let store = open_store(db_path);
    let now_sec = ctcl_core::now_ns() as f64 / ctcl_core::encoding::NS_PER_S as f64;
    match store.system_now(id, now_sec) {
        Ok((local, extra)) => print_ok(json!({ "system_id": id, "system_time": local, "extra": extra })),
        Err(e) => print_store_err(&e),
    }
}

pub fn group_create(db_path: &str, id: &str, members: Vec<String>) {
    let store = open_store(db_path);
    match store.create_group(id, &members, None) {
        Ok(rec) => print_ok(json!(rec)),
        Err(e) => print_store_err(&e),
    }
}

pub fn group_get(db_path: &str, id: &str) {
    let store = open_store(db_path);
    match store.get_group(id) {
        Ok(rec) => print_ok(json!(rec)),
        Err(e) => print_store_err(&e),
    }
}

pub fn group_list(db_path: &str) {
    let store = open_store(db_path);
    match store.list_groups() {
        Ok(ids) => print_ok(json!({ "groups": ids })),
        Err(e) => print_store_err(&e),
    }
}

pub fn group_expand(db_path: &str, id: &str, instant_id: Option<String>, value: Option<String>, from: &str) {
    let store = open_store(db_path);
    let ns = if let Some(iid) = instant_id {
        match store.get_instant(&iid) {
            Ok(rec) => rec.unix_ns.parse::<i128>().unwrap(),
            Err(e) => print_store_err(&e),
        }
    } else if let Some(v) = value {
        match to_ns(&v, from) {
            Ok(ns) => ns,
            Err(e) => { eprintln!("invalid value: {e}"); std::process::exit(1); }
        }
    } else {
        ctcl_core::now_ns()
    };
    match store.expand_group(id, ns) {
        Ok(result) => print_ok(result),
        Err(e) => print_store_err(&e),
    }
}
