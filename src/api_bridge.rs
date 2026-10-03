//! Lets the bundled screens keep calling `fetch("/api/...")`: the web layer hands each request to
//! `api_request`, which forwards it to the cloud server with this device's credentials and the
//! id of the cashier signed in on the device (PIN checked locally). Server-side, each request is
//! re-authorised from the database (employee active + role + same restaurant + device not revoked).
//!
//! Offline: the screens that must keep working (Ticket Rail, Sales, Unpaid Orders) are answered
//! from this device — see offline_api.rs. Everything else fails fast with a network error, which
//! the app shows as the "You're offline" page, instead of hanging.

use crate::local_db::Db;
use crate::offline_api as offline;
use crate::staff_auth::SessionState;
use crate::sync::API_BASE;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// Never reachable through the bridge, whatever the screen asks for.
const BLOCKED_PREFIXES: [&str; 4] = ["/api/device/", "/api/super-admin", "/api/cron", "/api/staff/login"];

/// When the cloud last failed to answer (epoch ms; 0 = it is answering). Requests made within a few
/// seconds of a failure skip the network and go straight to the offline path, so a dead connection
/// doesn't make every screen wait for a timeout.
static LAST_NET_FAIL: AtomicI64 = AtomicI64::new(0);
const OFFLINE_HINT_MS: i64 = 8_000;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn recently_failed() -> bool {
    let t = LAST_NET_FAIL.load(Ordering::Relaxed);
    t != 0 && now_ms() - t < OFFLINE_HINT_MS
}
fn mark_failed() {
    LAST_NET_FAIL.store(now_ms(), Ordering::Relaxed);
}
fn mark_ok() {
    LAST_NET_FAIL.store(0, Ordering::Relaxed);
}

#[derive(Serialize)]
pub struct ApiReply {
    status: u16,
    content_type: Option<String>,
    body: String,
}

fn json_reply(status: u16, v: Value) -> ApiReply {
    ApiReply { status, content_type: Some("application/json".to_string()), body: v.to_string() }
}

pub fn check_path(path: &str) -> Result<(), String> {
    if !path.starts_with("/api/") || path.contains("..") || path.contains('\\') || path.contains("//") {
        return Err("Blocked request path".to_string());
    }
    let lower = path.to_lowercase();
    if BLOCKED_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return Err("This request is not available from the desktop app".to_string());
    }
    Ok(())
}

enum NetError {
    /// The cloud could not be reached (no internet, DNS, timeout).
    Offline(String),
    /// Something local (not activated, not signed in) — not a connectivity problem.
    Local(String),
}

fn http(app: &AppHandle, method: &reqwest::Method, path: &str, body: Option<&str>, content_type: Option<&str>) -> Result<ApiReply, NetError> {
    let token = crate::secure_store::secure_get("device_token".to_string())
        .map_err(NetError::Local)?
        .ok_or_else(|| NetError::Local("This device is not activated yet".to_string()))?;
    let employee_id = app
        .state::<SessionState>()
        .0
        .lock()
        .map_err(|e| NetError::Local(e.to_string()))?
        .as_ref()
        .map(|s| s.employee_id.clone())
        .ok_or_else(|| NetError::Local("Not signed in".to_string()))?;

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| NetError::Local(e.to_string()))?;

    let mut req = client
        .request(method.clone(), format!("{API_BASE}{path}"))
        .bearer_auth(token)
        .header("x-staff-id", employee_id);
    if let Some(b) = body {
        req = req.header("content-type", content_type.unwrap_or("application/json")).body(b.to_string());
    }

    let resp = req.send().map_err(|e| NetError::Offline(format!("Could not reach the server — {e}")))?;
    let status = resp.status().as_u16();
    let content_type = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    let body = resp.text().map_err(|e| NetError::Offline(format!("Could not read the server response — {e}")))?;
    Ok(ApiReply { status, content_type, body })
}

/// Try the cloud (unless it just failed). Ok = it answered (any HTTP status).
fn try_http(app: &AppHandle, method: &reqwest::Method, path: &str, body: Option<&str>, content_type: Option<&str>) -> Result<ApiReply, NetError> {
    if recently_failed() {
        return Err(NetError::Offline("You're offline — this needs an internet connection".to_string()));
    }
    match http(app, method, path, body, content_type) {
        Ok(r) => {
            mark_ok();
            Ok(r)
        }
        Err(NetError::Offline(e)) => {
            mark_failed();
            Err(NetError::Offline(e))
        }
        Err(e) => Err(e),
    }
}

fn net_msg(e: NetError) -> String {
    match e {
        NetError::Offline(m) | NetError::Local(m) => m,
    }
}

/// A body that names a sale rung up on this device ("local:<id>") is rewritten to the server's id
/// once that sale has uploaded; before that, the server can't act on it.
fn remap_local_sale(db: &Db, body: &str) -> Result<String, ApiReply> {
    let Ok(mut v) = serde_json::from_str::<Value>(body) else { return Ok(body.to_string()) };
    let Some(sale_id) = v["saleId"].as_str().filter(|s| s.starts_with("local:")).map(str::to_string) else {
        return Ok(body.to_string());
    };
    let conn = db.0.lock().map_err(|e| json_reply(500, json!({ "error": e.to_string() })))?;
    match offline::server_id_for_local(&conn, &sale_id) {
        Some(server_id) => {
            v["saleId"] = json!(server_id);
            Ok(v.to_string())
        }
        None => Err(json_reply(
            409,
            json!({ "error": "This order was rung up on this device and hasn't synced yet. Try again once the device is online and has synced." }),
        )),
    }
}

fn kitchen_status(app: &AppHandle, db: &Db, path: &str, body: Option<String>, content_type: Option<String>) -> Result<ApiReply, String> {
    let raw = body.unwrap_or_default();
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let (Some(sale_ref), Some(status)) = (v["saleId"].as_str().map(str::to_string), v["kitchenStatus"].as_str().map(str::to_string)) else {
        return Ok(json_reply(400, json!({ "error": "saleId and a valid kitchenStatus are required" })));
    };
    if !offline::KITCHEN_STATUSES.contains(&status.as_str()) {
        return Ok(json_reply(400, json!({ "error": "saleId and a valid kitchenStatus are required" })));
    }

    // A sale that is still only on this device: remember the move, the sync applies it later.
    let mut server_id = sale_ref.clone();
    if sale_ref.starts_with("local:") {
        let conn = db.0.lock().map_err(|e| e.to_string())?;
        match offline::server_id_for_local(&conn, &sale_ref) {
            Some(id) => server_id = id,
            None => {
                offline::record_kitchen_op(&conn, &sale_ref, &status)?;
                return Ok(json_reply(200, json!({ "success": true, "queued": true })));
            }
        }
    }

    let forwarded = json!({ "saleId": server_id, "kitchenStatus": status }).to_string();
    match try_http(app, &reqwest::Method::POST, path, Some(&forwarded), content_type.as_deref()) {
        Ok(reply) => {
            if reply.status == 200 {
                if let Ok(conn) = db.0.lock() {
                    offline::set_cache_kitchen(&conn, &server_id, &status);
                }
            }
            Ok(reply)
        }
        Err(NetError::Offline(_)) => {
            let conn = db.0.lock().map_err(|e| e.to_string())?;
            offline::record_kitchen_op(&conn, &server_id, &status)?;
            Ok(json_reply(200, json!({ "success": true, "queued": true })))
        }
        Err(NetError::Local(m)) => Err(m),
    }
}

fn handle(app: AppHandle, method: reqwest::Method, path: String, body: Option<String>, content_type: Option<String>) -> Result<ApiReply, String> {
    let db = app.state::<Db>();
    let (base, qp) = offline::split_path(&path);

    if method == reqwest::Method::POST && base == "/api/sales/kitchen-status" {
        return kitchen_status(&app, &db, &path, body, content_type);
    }

    // Other changes to a sale made on this device (cancel, take payment, edit): map its id, or say
    // plainly that it has to upload first.
    let body = match body {
        Some(b) if method != reqwest::Method::GET => match remap_local_sale(&db, &b) {
            Ok(b) => Some(b),
            Err(reply) => return Ok(reply),
        },
        other => other,
    };

    let local_get = method == reqwest::Method::GET && offline::is_local_capable_get(&path);
    match try_http(&app, &method, &path, body.as_deref(), content_type.as_deref()) {
        Ok(mut reply) => {
            if local_get && reply.status == 200 && base == "/api/sales" {
                if let Ok(conn) = db.0.lock() {
                    if let Some(adjusted) = offline::adjust_online_sales(&conn, &reply.body, &qp) {
                        reply.body = adjusted;
                    }
                }
            }
            Ok(reply)
        }
        Err(NetError::Offline(msg)) => {
            if local_get {
                let conn = db.0.lock().map_err(|e| e.to_string())?;
                if let Some(v) = offline::serve_get_local(&conn, &path) {
                    return Ok(json_reply(200, v));
                }
            }
            Err(msg)
        }
        Err(e) => Err(net_msg(e)),
    }
}

#[tauri::command]
pub async fn api_request(
    app: AppHandle,
    method: String,
    path: String,
    body: Option<String>,
    content_type: Option<String>,
) -> Result<ApiReply, String> {
    check_path(&path)?;
    let method = reqwest::Method::from_bytes(method.to_uppercase().as_bytes()).map_err(|_| "Bad method".to_string())?;
    if ![reqwest::Method::GET, reqwest::Method::POST, reqwest::Method::PUT, reqwest::Method::PATCH, reqwest::Method::DELETE].contains(&method) {
        return Err("Method not allowed".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || handle(app, method, path, body, content_type))
        .await
        .map_err(|e| e.to_string())?
}

/// Is the cloud reachable right now? A quick TCP connect (no HTTP, no login), polled by the UI to
/// decide whether to show the "You're offline" page. Also resets/sets the fast-fail hint above.
#[tauri::command]
pub async fn check_online() -> bool {
    tauri::async_runtime::spawn_blocking(|| {
        use std::net::{TcpStream, ToSocketAddrs};
        let host = API_BASE.trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/');
        let addr = if host.contains(':') { host.to_string() } else { format!("{host}:443") };
        let ok = addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .map(|a| TcpStream::connect_timeout(&a, Duration::from_secs(3)).is_ok())
            .unwrap_or(false);
        if ok {
            mark_ok();
        } else {
            mark_failed();
        }
        ok
    })
    .await
    .unwrap_or(false)
}
