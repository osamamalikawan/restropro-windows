//! Local answers for the screens that must keep working with no internet (Ticket Rail, Sales,
//! Unpaid Orders). The screens keep calling `fetch("/api/...")`; `api_bridge` asks the cloud
//! first and falls back to this module when it can't be reached:
//!
//!  - lists of orders come from the last copy of recent sales (downloaded by the sync and kept
//!    fresh by every successful online request) PLUS the sales rung up on this device that have
//!    not uploaded yet (shown immediately, even while online),
//!  - Ticket Rail moves are recorded in `kitchen_ops` and uploaded by the next sync.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;

const KEEP_RECENT: usize = 400;
const KEEP_UNPAID: usize = 300;
pub const KITCHEN_STATUSES: [&str; 3] = ["New", "Preparing", "Completed"];

// ---------------------------------------------------------------- tiny helpers

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn split_path(path: &str) -> (&str, HashMap<String, String>) {
    let (base, query) = path.split_once('?').unwrap_or((path, ""));
    let mut map = HashMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(percent_decode(k), percent_decode(v));
    }
    (base, map)
}

fn ts(v: &Value) -> i64 {
    v["created_at"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok().or_else(|| DateTime::parse_from_rfc3339(&s.replacen(' ', "T", 1)).ok()))
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

pub fn cache_get(conn: &Connection, kind: &str) -> Option<Value> {
    conn.query_row("SELECT json FROM cache WHERE kind = ?", [kind], |r| r.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn cache_put(conn: &Connection, kind: &str, v: &Value) -> Result<(), String> {
    conn.execute(
        "INSERT INTO cache (kind, json) VALUES (?1, ?2) ON CONFLICT(kind) DO UPDATE SET json = excluded.json",
        params![kind, v.to_string()],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- kitchen ops

/// sale ref (server id, or "local:<clientSaleId>") -> most recent pending Ticket Rail status.
fn pending_kitchen(conn: &Connection) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT sale_ref, kitchen_status FROM kitchen_ops WHERE status = 'pending' ORDER BY op_id") {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
            for row in rows.flatten() {
                map.insert(row.0, row.1);
            }
        }
    }
    map
}

pub fn record_kitchen_op(conn: &Connection, sale_ref: &str, status: &str) -> Result<(), String> {
    if !KITCHEN_STATUSES.contains(&status) {
        return Err("Invalid kitchen status".to_string());
    }
    conn.execute(
        "INSERT INTO kitchen_ops (sale_ref, kitchen_status, created_at) VALUES (?1, ?2, ?3)",
        params![sale_ref, status, Utc::now().to_rfc3339()],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// Reflect a confirmed move in the saved copy of the sales.
pub fn set_cache_kitchen(conn: &Connection, sale_id: &str, status: &str) {
    if let Some(Value::Array(mut rows)) = cache_get(conn, "sales") {
        let mut changed = false;
        for r in rows.iter_mut() {
            if r["id"].as_str() == Some(sale_id) {
                r["kitchen_status"] = json!(status);
                changed = true;
            }
        }
        if changed {
            let _ = cache_put(conn, "sales", &Value::Array(rows));
        }
    }
}

// ---------------------------------------------------------------- sales copy

/// Merge rows into the saved copy (by id, newest wins), keep it bounded. Used by the sync
/// snapshot and by every successful online `GET /api/sales`.
pub fn merge_sales_into_cache(conn: &Connection, rows: &[Value]) -> Result<(), String> {
    let mut by_id: HashMap<String, Value> = HashMap::new();
    if let Some(Value::Array(old)) = cache_get(conn, "sales") {
        for r in old {
            if let Some(id) = r["id"].as_str() {
                by_id.insert(id.to_string(), r);
            }
        }
    }
    for r in rows {
        if let Some(id) = r["id"].as_str() {
            by_id.insert(id.to_string(), r.clone());
        }
    }
    let mut all: Vec<Value> = by_id.into_values().collect();
    all.sort_by(|a, b| ts(b).cmp(&ts(a)));
    let mut kept: Vec<Value> = Vec::new();
    let mut unpaid_extra = 0;
    for (i, r) in all.into_iter().enumerate() {
        if i < KEEP_RECENT {
            kept.push(r);
        } else if r["status"].as_str() == Some("unpaid") && unpaid_extra < KEEP_UNPAID {
            unpaid_extra += 1;
            kept.push(r);
        }
    }
    cache_put(conn, "sales", &Value::Array(kept))
}

fn num(v: &Value) -> f64 {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0)
}

/// Sales rung up on this device that the server doesn't have yet, shaped like `GET /api/sales`
/// rows. Their id is "local:<clientSaleId>"; the order number is the provisional "L<n>".
fn synth_pending_sales(conn: &Connection) -> Vec<Value> {
    let tables = cache_get(conn, "tables").and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare("SELECT seq, client_sale_id, payload, created_at FROM sales_outbox WHERE status = 'pending' ORDER BY seq") else {
        return out;
    };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))) else {
        return out;
    };
    for (seq, client_id, payload, created) in rows.flatten() {
        let Ok(p) = serde_json::from_str::<Value>(&payload) else { continue };
        let items: Vec<Value> = p["items"].as_array().cloned().unwrap_or_default();
        let items_sum: f64 = items.iter().map(|i| num(&i["price"]) * num(&i["qty"])).sum();
        let delivery = num(&p["deliveryCharge"]);
        let total = if p["total"].is_null() { items_sum + delivery } else { num(&p["total"]) };
        let payments: Vec<Value> = p["payments"].as_array().cloned().unwrap_or_default();
        let paid: f64 = payments.iter().map(|x| num(&x["amount"])).sum();
        let table_no = p["tableId"]
            .as_str()
            .and_then(|id| tables.iter().find(|t| t["id"].as_str() == Some(id)))
            .and_then(|t| t["number"].as_str().map(str::to_string).or_else(|| Some(t["number"].to_string())));
        let customer_name = p["customerName"].as_str().filter(|s| !s.is_empty());
        out.push(json!({
            "id": format!("local:{client_id}"),
            "order_no": format!("L{seq}"),
            "display_id": crate::local_db::display_id(conn, seq),
            "order_type": p["orderType"],
            "subtotal": items_sum,
            "tax": (total - items_sum - delivery).max(0.0),
            "total": total,
            "delivery_charge": delivery,
            "status": if paid + 0.005 >= total { "completed" } else { "unpaid" },
            "kitchen_status": "New",
            "created_at": p["soldAt"].as_str().map(str::to_string).unwrap_or(created),
            "customer_id": p["customerId"],
            "customers": customer_name.map(|n| json!({ "name": n, "phone": p["customerPhone"].as_str().unwrap_or("") })),
            "employees": p["cashierName"].as_str().map(|n| json!({ "name": n })),
            "tables": table_no.map(|n| json!({ "number": n })),
            "sale_items": items.iter().map(|i| json!({ "product_id": i["productId"], "name": i["name"], "unit_price": num(&i["price"]), "quantity": num(&i["qty"]) })).collect::<Vec<_>>(),
            "sale_payments": payments.iter().map(|x| json!({ "method": x["method"], "amount": num(&x["amount"]) })).collect::<Vec<_>>(),
            "pending_upload": true,
        }));
    }
    out
}

fn matches_filters(row: &Value, qp: &HashMap<String, String>) -> bool {
    if let Some(st) = qp.get("status").filter(|s| !s.is_empty()) {
        if row["status"].as_str() != Some(st.as_str()) {
            return false;
        }
    }
    if let Some(cid) = qp.get("customerId").filter(|s| !s.is_empty()) {
        if row["customer_id"].as_str() != Some(cid.as_str()) {
            return false;
        }
    }
    if let Some(q) = qp.get("q").map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()) {
        let order = row["order_no"].as_str().map(str::to_string).unwrap_or_else(|| row["order_no"].to_string()).to_lowercase();
        let name = row["customers"]["name"].as_str().unwrap_or("").to_lowercase();
        let phone = row["customers"]["phone"].as_str().unwrap_or("").to_lowercase();
        if !(order.contains(&q) || name.contains(&q) || phone.contains(&q)) {
            return false;
        }
    }
    true
}

fn apply_overlay(rows: &mut [Value], overlay: &HashMap<String, String>) {
    if overlay.is_empty() {
        return;
    }
    for r in rows.iter_mut() {
        if let Some(status) = r["id"].as_str().and_then(|id| overlay.get(id)) {
            r["kitchen_status"] = json!(status);
        }
    }
}

fn page_params(qp: &HashMap<String, String>) -> (usize, usize) {
    let limit = qp.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100).clamp(1, 1000);
    let offset = qp.get("offset").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    (limit, offset)
}

/// `GET /api/sales` answered entirely from this device.
pub fn sales_list_local(conn: &Connection, qp: &HashMap<String, String>) -> Value {
    let mut rows: Vec<Value> = cache_get(conn, "sales").and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let pending = synth_pending_sales(conn);
    rows.extend(pending);
    let mut overlay = pending_kitchen(conn);
    // a Ticket Rail move made on a not-yet-uploaded sale is recorded against its local id
    apply_overlay(&mut rows, &overlay);
    overlay.clear();

    let mut filtered: Vec<Value> = rows.into_iter().filter(|r| matches_filters(r, qp)).collect();
    filtered.sort_by(|a, b| ts(b).cmp(&ts(a)));
    let (limit, offset) = page_params(qp);
    let total = filtered.len();
    let page: Vec<Value> = filtered.into_iter().skip(offset).take(limit).collect();
    json!({ "sales": page, "hasMore": total > offset + limit, "offline": true })
}

/// Adjust a successful online `GET /api/sales` body: remember the rows, show sales that are still
/// only on this device, and apply Ticket Rail moves not uploaded yet.
pub fn adjust_online_sales(conn: &Connection, body: &str, qp: &HashMap<String, String>) -> Option<String> {
    let mut v: Value = serde_json::from_str(body).ok()?;
    let rows = v["sales"].as_array()?.clone();
    let _ = merge_sales_into_cache(conn, &rows);

    let overlay = pending_kitchen(conn);
    let mut page = rows;
    apply_overlay(&mut page, &overlay);

    let (_, offset) = page_params(qp);
    let unfiltered = !qp.contains_key("q") && !qp.contains_key("customerId");
    if offset == 0 && unfiltered {
        let mut pending: Vec<Value> = synth_pending_sales(conn).into_iter().filter(|r| matches_filters(r, qp)).collect();
        apply_overlay(&mut pending, &overlay);
        if !pending.is_empty() {
            pending.sort_by(|a, b| ts(b).cmp(&ts(a)));
            pending.extend(page);
            page = pending;
        }
    }
    v["sales"] = Value::Array(page);
    Some(v.to_string())
}

// ---------------------------------------------------------------- other cached reads

/// Offline answers for the small lookups those screens also fetch (same JSON shapes as the web
/// routes). `None` = not something we can answer locally.
pub fn serve_get_local(conn: &Connection, path: &str) -> Option<Value> {
    let (base, qp) = split_path(path);
    let list = |kind: &str| cache_get(conn, kind).unwrap_or_else(|| json!([]));
    match base {
        "/api/sales" => Some(sales_list_local(conn, &qp)),
        "/api/tables" => Some(json!({ "tables": list("tables") })),
        "/api/delivery-areas" => Some(json!({ "areas": list("areas") })),
        "/api/payment-methods" => Some(json!({ "methods": list("payment_methods") })),
        "/api/settings" => Some(cache_get(conn, "settings").unwrap_or_else(|| json!({ "restaurant": null, "settings": {} }))),
        _ => None,
    }
}

pub fn is_local_capable_get(path: &str) -> bool {
    let (base, _) = split_path(path);
    matches!(base, "/api/sales" | "/api/tables" | "/api/delivery-areas" | "/api/payment-methods" | "/api/settings")
}

/// Server id of a sale that was rung up on this device ("local:<clientSaleId>"), once uploaded.
pub fn server_id_for_local(conn: &Connection, local_ref: &str) -> Option<String> {
    let client_id = local_ref.strip_prefix("local:")?;
    conn.query_row("SELECT server_sale_id FROM sales_outbox WHERE client_sale_id = ?", [client_id], |r| r.get::<_, Option<String>>(0))
        .optional()
        .ok()
        .flatten()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// The cashier edited an existing customer in the checkout popup: show it in the saved customer
/// list straight away (the server record is updated when the sale uploads).
pub fn update_cached_customer(conn: &Connection, customer_id: &str, update: &Value) {
    let Some(Value::Array(mut rows)) = cache_get(conn, "customers") else { return };
    let mut changed = false;
    for r in rows.iter_mut() {
        if r["id"].as_str() != Some(customer_id) {
            continue;
        }
        for (src, dst, allow_empty) in [("name", "name", false), ("phone", "phone", false), ("address", "address", true), ("areaId", "area_id", false)] {
            if let Some(v) = update.get(src).filter(|v| v.is_string()) {
                if allow_empty || v.as_str().map_or(false, |t| !t.is_empty()) {
                    r[dst] = v.clone();
                    changed = true;
                }
            }
        }
    }
    if changed {
        let _ = cache_put(conn, "customers", &Value::Array(rows));
    }
}
