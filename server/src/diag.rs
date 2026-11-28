//! Clean Diagnostics reports (design 14). The upload reader only yields text that
//! round-trips through the core's closed Report. No request or transport data is stored.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Response};
use nexus_core::nexus::account::{self, DeviceId, ListDevices, Refusal, Reply};
use nexus_core::nexus::diag::{self, MAX_AGE_MS, STORED_REPORTS, Upload, Uploaded};
use nexus_core::nexus::kept::Access;
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::accounts::session::{Accounts, refused};
use crate::events::{
    Calls, answer, authenticated, bearer, denied, finish, list_response, receipt, respond, result,
    schema, sql, unavailable,
};

pub const SEND_PATH: &str = diag::SEND_PATH;
pub const LIST_PATH: &str = "/nexus/api/diag/list";
pub const ADMIN_PATH: &str = "/nexus/api/admin/diagnostics";
pub const LIST_PAGE: usize = 50;
const EXPIRY_INTERVAL: Duration = Duration::from_secs(60);

// CARRIED for form-1 data created before the store included the report table.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS diagnostic_reports (
    row_id INTEGER PRIMARY KEY AUTOINCREMENT,
    account INTEGER NOT NULL REFERENCES accounts(number) ON DELETE CASCADE,
    device INTEGER NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    restore INTEGER NOT NULL,
    request_id BLOB NOT NULL CHECK (length(request_id) = 8),
    received_ms INTEGER NOT NULL,
    text TEXT NOT NULL,
    UNIQUE (device, restore, request_id)
) STRICT;
CREATE INDEX IF NOT EXISTS diagnostics_device ON diagnostic_reports(device, row_id DESC);
CREATE INDEX IF NOT EXISTS diagnostics_account ON diagnostic_reports(account, row_id DESC);
CREATE INDEX IF NOT EXISTS diagnostics_expiry ON diagnostic_reports(received_ms, row_id);
";

pub(crate) async fn send(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let outcome = async {
        let hash = bearer(&calls, &headers)?;
        let upload = Upload::read(&body).map_err(|_| Refusal::Malformed)?;
        let clock = calls.accounts.clock.clone();
        answer(calls.accounts.desk.ask(move |keeper| {
            let now = clock();
            let (number, device, account) = authenticated(keeper, &upload.head, &hash, Access::Write, now)?;
            let at = keeper.clock(now).effective();
            schema(keeper.data().connection(), "diagnostic_reports", SCHEMA).map_err(unavailable)?;
            let matches = keeper.write_data(now, |tx| keep(tx, number, device, upload, at))
                .map_err(unavailable)?;
            if !matches {
                return Err(Refusal::Malformed);
            }
            result(Reply::Done { result: Uploaded {}, receipt: receipt(keeper, account.state())? }, diag::write_reply)
        }).await)
    }.await;
    finish(outcome)
}

fn keep(tx: &Transaction<'_>, account: u64, device: DeviceId, upload: Upload, now: u64) -> rusqlite::Result<(bool, bool)> {
    let restore = sql(upload.head.restore)?;
    let device = sql(device.get())?;
    let request = upload.request_id.get().to_be_bytes();
    let existing = tx.query_row(
        "SELECT text = ?4 FROM diagnostic_reports
         WHERE device = ?1 AND restore = ?2 AND request_id = ?3",
        params![device, restore, request, upload.text()],
        |row| row.get::<_, bool>(0),
    ).optional()?;
    if let Some(matches) = existing {
        return Ok((matches, false));
    }
    tx.execute(
        "INSERT INTO diagnostic_reports(account, device, restore, request_id, received_ms, text)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![sql(account)?, device, restore, request, sql(now)?, upload.text()],
    )?;
    let mut recent = tx.prepare(
        "SELECT row_id, received_ms FROM diagnostic_reports WHERE device = ?1
         ORDER BY row_id DESC LIMIT ?2",
    )?;
    let rows = recent.query_map(params![device, (STORED_REPORTS + 1) as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let times: Vec<_> = rows.iter().map(|&(_, received)| received as u64).collect();
    let mut retained = diag::reports_to_keep(&times, now).peekable();
    let mut remove = tx.prepare("DELETE FROM diagnostic_reports WHERE row_id = ?1")?;
    for (index, &(id, _)) in rows.iter().enumerate() {
        if retained.next_if_eq(&index).is_none() {
            remove.execute([id])?;
        }
    }
    Ok((true, true))
}

// CARRIED until the desk calls Keeper::idle itself (S9). Enqueue one idle turn
// immediately at startup, then at most once per minute. The store owns the work.
pub(crate) fn start_expiry(accounts: &Arc<Accounts>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    let accounts = Arc::downgrade(accounts);
    runtime.spawn(async move {
        let mut interval = tokio::time::interval(EXPIRY_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(accounts) = accounts.upgrade() else { break };
            let clock = accounts.clock.clone();
            let _ = accounts.desk.ask(move |keeper| {
                schema(keeper.data().connection(), "diagnostic_reports", SCHEMA).map_err(unavailable)?;
                keeper.idle(clock()).map_err(unavailable)
            }).await;
        }
    });
}

/// Device numbers and server receipt times for the signed-in account.
/// The text never goes back through an app call (14.5).
pub(crate) async fn list(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    read_page(calls, headers, body, false).await
}

/// Admin's browser page, one report at a time. The read has no target account
/// supplied by the caller, and the authorization comes from the live admin role.
pub(crate) async fn admin(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    read_page(calls, headers, body, true).await
}

async fn read_page(calls: Calls, headers: HeaderMap, body: Bytes, admin: bool) -> Response<Body> {
    let outcome = async {
        let hash = bearer(&calls, &headers)?;
        if admin && !crate::admin::is_browser(&headers) {
            return Err(denied());
        }
        let request = account::read::<ListDevices>(&body).map_err(|_| Refusal::Malformed)?;
        let clock = calls.accounts.clock.clone();
        let (page, stamp) = answer(calls.accounts.desk.ask(move |keeper| {
            let now = clock();
            let (number, _, account) = authenticated(keeper, &request.head, &hash, Access::Read, now)?;
            if admin && !account.state().admin {
                return Err(denied());
            }
            let at = keeper.clock(now).effective();
            let db = keeper.data().connection();
            schema(db, "diagnostic_reports", SCHEMA).map_err(unavailable)?;
            let page = page(db, if admin { None } else { Some(number) }, request.after, at).map_err(unavailable)?;
            Ok((page, receipt(keeper, account.state())?))
        }).await)?;
        list_response(page, stamp)
    }.await;
    outcome.unwrap_or_else(|reason| respond(refused(reason)))
}

fn page(db: &Connection, owner: Option<u64>, after: Option<DeviceId>, now: u64) -> rusqlite::Result<String> {
    let after = after.map_or(Ok(i64::MAX), |id| sql(id.get()))?;
    let cutoff = sql(now.saturating_sub(MAX_AGE_MS))?;
    let now = sql(now)?;
    let mut page = String::from(r#"{"reports":["#);
    let mut next = None;
    if let Some(owner) = owner {
        let mut query = db.prepare(
            "SELECT row_id, device, received_ms FROM diagnostic_reports
             WHERE account = ?1 AND row_id < ?2 AND received_ms BETWEEN ?3 AND ?4
             ORDER BY row_id DESC LIMIT ?5",
        )?;
        let mut rows = query.query(params![sql(owner)?, after, cutoff, now, (LIST_PAGE + 1) as i64])?;
        for index in 0..LIST_PAGE {
            let Some(row) = rows.next()? else { break };
            let id: i64 = row.get(0)?;
            let device: i64 = row.get(1)?;
            let time: i64 = row.get(2)?;
            if index != 0 {
                page.push(',');
            }
            let _ = write!(page, r#"{{"id":{id},"device":{device},"received_ms":{time}}}"#);
            if index + 1 == LIST_PAGE && rows.next()?.is_some() {
                next = Some(id);
            }
        }
    } else {
        let mut query = db.prepare(
            "SELECT row_id, account, device, received_ms, json_quote(text) FROM diagnostic_reports
             WHERE row_id < ?1 AND received_ms BETWEEN ?2 AND ?3 ORDER BY row_id DESC LIMIT 2",
        )?;
        let mut rows = query.query(params![after, cutoff, now])?;
        if let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let owner: i64 = row.get(1)?;
            let device: i64 = row.get(2)?;
            let time: i64 = row.get(3)?;
            let text: String = row.get(4)?;
            let _ = write!(page, r#"{{"id":{id},"account":{owner},"device":{device},"received_ms":{time},"text":{text}}}"#);
            if rows.next()?.is_some() {
                next = Some(id);
            }
        }
    }
    let next = next.map_or_else(|| "null".into(), |id| id.to_string());
    let _ = write!(page, r#"],"next":{next}}}"#);
    Ok(page)
}
