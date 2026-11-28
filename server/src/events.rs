//! Listening history (design 12.5 and 13). The shared library reads batches and decides
//! individual intake and grant outcomes. SQLite owns the kept event and its identity together.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Response, StatusCode};
use axum::routing::post;
use nexus_core::nexus::account::{
    self, AccountId, DeleteListeningData, DeviceId, GrantChange, GrantIssued, GrantRequest,
    GrantSecret, ListDevices, NotAllowed, NotSignedIn, PasswordDoor, Receipt, Refusal, Reply,
    RequestHead, SignedOut, Stamp,
};
use nexus_core::nexus::events::{self, Answered, BatchAnswer, IncomingBatch, Outcome};
use nexus_core::nexus::kept::{Access, Account, AccountState, Bearer, Denied, Fingerprint, GrantIntent, GrantUse, LeaveFor};
use nexus_core::nexus::proto::SPEAKS;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tokio::sync::Semaphore;
use zeroize::Zeroize;

use crate::accounts::{check::Checked, rows, setup};
use crate::accounts::session::{Accounts, Answer, refused};
use crate::store::keeper::Keeper;

pub const STATS_PATH: &str = "/nexus/api/events/stats";
pub const STATS_PAGE: usize = 50;
const DEFAULT_DATA_BUDGET: u64 = 4_000_000_000;

// CARRIED for form-1 data created before the store included these tables.
// The life transition and deletion share a transaction. Recovery can hold old history.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS listening_events (
    row_id INTEGER PRIMARY KEY AUTOINCREMENT,
    account INTEGER NOT NULL REFERENCES accounts(number) ON DELETE CASCADE,
    device INTEGER NOT NULL,
    event_id TEXT NOT NULL CHECK (length(event_id) = 32),
    at_ms INTEGER NOT NULL,
    is_start INTEGER NOT NULL CHECK (is_start IN (0, 1)),
    line TEXT NOT NULL,
    UNIQUE (account, event_id)
) STRICT;
CREATE INDEX IF NOT EXISTS listening_recent ON listening_events(account, at_ms DESC, row_id DESC)
    WHERE is_start = 1;
CREATE TABLE IF NOT EXISTS listening_totals (
    account INTEGER PRIMARY KEY REFERENCES accounts(number) ON DELETE CASCADE,
    starts INTEGER NOT NULL
) STRICT;
CREATE TRIGGER IF NOT EXISTS listening_count_start AFTER INSERT ON listening_events
WHEN NEW.is_start = 1 BEGIN
    INSERT INTO listening_totals(account, starts) VALUES (NEW.account, 1)
    ON CONFLICT(account) DO UPDATE SET starts = starts + 1;
END;
CREATE TRIGGER IF NOT EXISTS listening_erase_life AFTER UPDATE OF life ON accounts
WHEN NEW.life > OLD.life AND NEW.history_held = 0 BEGIN
    DELETE FROM listening_events WHERE account = NEW.number;
    DELETE FROM listening_totals WHERE account = NEW.number;
END;
";

#[derive(Clone)]
pub(crate) struct Calls {
    pub accounts: Arc<Accounts>,
    pub public_origin: Option<Arc<str>>,
    uploads: Arc<Semaphore>,
    budget: u64,
}

pub(crate) fn router(base: Router, accounts: Arc<Accounts>, public_origin: Option<Arc<str>>) -> Router {
    // CARRIED until settings validates this value at startup. A malformed configured cap
    // closes the event intake to new rows while still allowing retries of kept events.
    let budget = match std::env::var("NEXUS_DATA_BUDGET") {
        Ok(value) => value.parse().unwrap_or(0),
        Err(std::env::VarError::NotPresent) => DEFAULT_DATA_BUDGET,
        Err(std::env::VarError::NotUnicode(_)) => 0,
    };
    let calls = Calls { accounts, public_origin, uploads: Arc::new(Semaphore::new(1)), budget };
    crate::diag::start_expiry(&calls.accounts);
    base.merge(
        Router::new()
            .route(events::SEND_PATH, post(send))
            .route(events::DELETE_PATH, post(delete))
            .route(account::DELETE_LISTENING_DATA, post(delete))
            .route(account::GRANT, post(grant))
            .route(STATS_PATH, post(stats))
            .route(crate::diag::SEND_PATH, post(crate::diag::send))
            .route(crate::diag::LIST_PATH, post(crate::diag::list))
            .route(crate::diag::ADMIN_PATH, post(crate::diag::admin))
            .with_state(calls),
    )
}

pub(crate) fn respond(answer: Answer) -> Response<Body> {
    json(answer.status, answer.body.bytes().to_vec())
}

fn json(status: u16, body: Vec<u8>) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
    response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

pub(crate) fn result<T, E>(reply: Reply<T>, encode: impl FnOnce(&Reply<T>) -> Result<account::WireBody, E>) -> Result<Answer, Refusal> {
    let status = reply.status();
    let body = encode(&reply).map_err(unavailable)?;
    Ok(Answer { status, body })
}

pub(crate) fn answer<T>(outcome: Result<Result<T, Refusal>, crate::accounts::desk::Busy>) -> Result<T, Refusal> {
    outcome?
}

pub(crate) fn finish(outcome: Result<Answer, Refusal>) -> Response<Body> {
    respond(outcome.unwrap_or_else(refused))
}

pub(crate) fn unavailable<E>(_: E) -> Refusal {
    Refusal::NotDealtWith
}

pub(crate) fn denied() -> Refusal {
    Refusal::NotAllowed(NotAllowed::Denied)
}

pub(crate) fn busy() -> Refusal {
    Refusal::Busy { retry_after_seconds: None }
}

fn ended() -> Refusal {
    Refusal::NotSignedIn(NotSignedIn::SessionEnded)
}

pub(crate) fn bearer(calls: &Calls, headers: &HeaderMap) -> Result<Fingerprint, Refusal> {
    if crate::admin::is_browser(headers) && !crate::admin::browser_origin_allowed(headers, calls.public_origin.as_deref()) {
        return Err(Refusal::Malformed);
    }
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next().ok_or_else(ended)?;
    if values.next().is_some() {
        return Err(ended());
    }
    let session = account::Session::from_authorization(value.to_str().map_err(|_| ended())?).ok_or_else(ended)?;
    Ok(Fingerprint::of(session.expose_bytes()))
}

// CARRIED until S9 exposes these S8 helpers.
fn signed_in(keeper: &mut Keeper, head: &RequestHead, hash: &Fingerprint) -> Result<(u64, DeviceId, Account), Refusal> {
    if head.speaks.low() > SPEAKS.high() || head.speaks.high() < SPEAKS.low() {
        return Err(Refusal::NotAllowed(NotAllowed::Incompatible { speaks: SPEAKS }));
    }
    if head.restore != keeper.restore() {
        return Err(Refusal::NotSignedIn(NotSignedIn::DataPutBack));
    }
    let (number, device) = rows::session_by_hash(keeper.data().connection(), hash.as_bytes())
        .map_err(unavailable)?
        .ok_or_else(ended)?;
    let account = keeper.load(number).map_err(unavailable)?;
    Ok((number, device, account))
}

fn said(reason: Denied) -> Refusal {
    match reason {
        Denied::NotSignedIn => ended(),
        Denied::StartAgain => Refusal::NotSignedIn(NotSignedIn::DataPutBack),
        Denied::AccountOff => Refusal::NotAllowed(NotAllowed::AccountOff),
        Denied::WrongPassword => Refusal::NotAllowed(NotAllowed::WrongLogin),
        Denied::HistoryHeld | Denied::WrongPurpose | Denied::LeaveRequired | Denied::AdminRequired => denied(),
        Denied::Time => busy(),
        Denied::Exhausted | Denied::ReusedSession | Denied::DeviceInUse => Refusal::NotDealtWith,
    }
}

pub(crate) fn receipt(keeper: &Keeper, account: &AccountState) -> Result<Receipt, Refusal> {
    let position = account.position(keeper.data().meta().map_err(unavailable)?.changes);
    let tag = keeper.stamp_key().stamp(&position);
    Ok(Receipt {
        stamp: Stamp { backend: position.backend, restore: position.restore, account: position.account, life: position.life, change: position.change, tag },
        music_login_count: account.music_login,
    })
}

fn arrive(keeper: &mut Keeper, head: &RequestHead, hash: &Fingerprint, device: DeviceId, account: &mut Account, access: Access, now: u64) -> Result<(), Refusal> {
    account.request(head.restore, keeper.clock(now), |turn| {
        turn.arrive(Bearer { device, hash, grant: None }, access).map(|_| ())
    })
        .apply(|effects| keeper.persist(effects, &[], now))
        .map_err(unavailable)?
        .map_err(said)
}

pub(crate) fn authenticated(keeper: &mut Keeper, head: &RequestHead, hash: &Fingerprint, access: Access, now: u64) -> Result<(u64, DeviceId, Account), Refusal> {
    let (number, device, mut account) = signed_in(keeper, head, hash)?;
    arrive(keeper, head, hash, device, &mut account, access, now)?;
    Ok((number, device, account))
}

pub(crate) fn sql(number: u64) -> rusqlite::Result<i64> {
    i64::try_from(number).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, i64::MAX))
}

pub(crate) fn schema(db: &Connection, table: &str, migration: &str) -> rusqlite::Result<()> {
    let present: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1)",
        [table],
        |row| row.get(0),
    )?;
    if !present {
        let tx = db.unchecked_transaction()?;
        tx.execute_batch(migration)?;
        tx.commit()?;
    }
    Ok(())
}

struct Prepared {
    id: events::EventId,
    event: Result<StoredEvent, Outcome>,
}

struct StoredEvent {
    at_ms: u64,
    is_start: bool,
    line: String,
}

fn prepare(batch: IncomingBatch) -> (RequestHead, u64, Vec<Prepared>) {
    let rows = batch.events.into_iter().map(|(id, event)| {
        let event = event.map_err(|_| Outcome::Refused).and_then(|placed| {
            let line = placed.line().ok().and_then(|bytes| String::from_utf8(bytes).ok()).ok_or(Outcome::NotDealtWith)?;
            Ok(StoredEvent {
                at_ms: placed.record().at_ms(),
                is_start: matches!(placed.record().kind(), events::Kind::Start { .. }),
                line,
            })
        });
        Prepared { id, event }
    }).collect();
    (batch.head, batch.life, rows)
}

async fn send(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let outcome = async {
        let hash = bearer(&calls, &headers)?;
        // At most one batch can parse, wait for the writer, or occupy it. The body and
        // its validation are outside the one writer, and parsing is off the async workers.
        let permit = calls.uploads.clone().try_acquire_owned().map_err(|_| busy())?;
        let (permit, (head, life, rows)) = tokio::task::spawn_blocking(move || {
            let batch = IncomingBatch::read(&body).map_err(|_| Refusal::Malformed)?;
            Ok::<_, Refusal>((permit, prepare(batch)))
        }).await.map_err(unavailable)??;
        let clock = calls.accounts.clock.clone();
        let budget = calls.budget;
        answer(calls.accounts.desk.ask(move |keeper| {
            let _permit = permit;
            let now = clock();
            let (number, device, mut account) = signed_in(keeper, &head, &hash)?;
            let current_life = account.state().life;
            let access = if life == current_life { Access::WriteHistory } else { Access::Read };
            arrive(keeper, &head, &hash, device, &mut account, access, now)?;
            let answers = if life == current_life {
                schema(keeper.data().connection(), "listening_events", SCHEMA).map_err(unavailable)?;
                keeper.write_data(now, |tx| insert(tx, number, device, current_life, life, budget, rows))
                    .map_err(unavailable)?
            } else {
                // The client's core reader interprets the stamp as LifeMoved before
                // applying any outcomes.
                Vec::new()
            };
            let result_body = BatchAnswer::new(answers).map_err(unavailable)?;
            result(Reply::Done { result: result_body, receipt: receipt(keeper, account.state())? }, events::write_reply)
        }).await)
    }.await;
    finish(outcome)
}

const DATA_BYTES: &str = "SELECT (p.page_count - f.freelist_count) * s.page_size
    FROM pragma_page_count p, pragma_freelist_count f, pragma_page_size s";

/// Bytes occupied by live SQLite pages. Freed pages can take new events without growing
/// the file. This also counts data owned by other slices.
pub fn data_bytes(db: &Connection) -> rusqlite::Result<u64> {
    measured_bytes(&mut db.prepare(DATA_BYTES)?)
}

fn measured_bytes(query: &mut rusqlite::Statement<'_>) -> rusqlite::Result<u64> {
    let bytes: i64 = query.query_row([], |row| row.get(0))?;
    u64::try_from(bytes).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, bytes))
}

fn insert(
    tx: &Transaction<'_>,
    account: u64,
    device: DeviceId,
    current_life: u64,
    life: u64,
    budget: u64,
    rows: Vec<Prepared>,
) -> rusqlite::Result<(Vec<Answered>, bool)> {
    let account = sql(account)?;
    let device = sql(device.get())?;
    let mut size = tx.prepare(DATA_BYTES)?;
    let mut room = measured_bytes(&mut size)? < budget;
    let mut find = tx.prepare("SELECT device FROM listening_events WHERE account = ?1 AND event_id = ?2")?;
    let mut add = tx.prepare(
        "INSERT INTO listening_events(account, device, event_id, at_ms, is_start, line)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut answers = Vec::with_capacity(rows.len());
    let mut changed = false;
    for Prepared { id, event } in rows {
        let outcome = match event {
            Err(outcome) => outcome,
            Ok(event) => {
                let event_id = id.to_hex();
                let owner: Option<i64> = find.query_row(params![account, event_id], |row| row.get(0)).optional()?;
                let existing = match owner {
                    None => events::Existing::None,
                    Some(owner) if owner == device => events::Existing::SameDevice,
                    Some(_) => events::Existing::AnotherDevice,
                };
                match events::intake(current_life, life, existing, room) {
                    events::Intake::Answer(outcome) => outcome,
                    events::Intake::Insert => {
                        // Keep prior accepted rows if this insert needs a page beyond
                        // the budget. An SQL error still rolls back the entire batch.
                        tx.execute_batch("SAVEPOINT listening_event")?;
                        add.execute(params![account, device, event_id, sql(event.at_ms)?, event.is_start, event.line])?;
                        let used = measured_bytes(&mut size)?;
                        room = used < budget;
                        if used <= budget {
                            tx.execute_batch("RELEASE listening_event")?;
                            changed = true;
                            Outcome::Taken
                        } else {
                            tx.execute_batch("ROLLBACK TO listening_event; RELEASE listening_event")?;
                            Outcome::NotDealtWith
                        }
                    }
                }
            }
        };
        answers.push(Answered { id, outcome });
    }
    Ok((answers, changed))
}

fn secret_body<T>(body: Bytes, read: impl FnOnce(&[u8]) -> Result<T, account::BadMessage>) -> Result<T, account::BadMessage> {
    let parsed = read(&body);
    if let Ok(mut own) = body.try_into_mut() {
        own.as_mut().zeroize();
    }
    parsed
}

async fn grant(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let parsed = secret_body(body, account::read::<GrantRequest>);
    let outcome = async {
        let hash = bearer(&calls, &headers)?;
        let request = parsed.map_err(|_| Refusal::Malformed)?;
        issue_grant(&calls.accounts, hash, request).await
    }.await;
    finish(outcome)
}

fn target(state: &AccountState, account: AccountId, revision: u64) -> Result<(), Refusal> {
    if state.id == account && state.revision == revision { Ok(()) } else { Err(denied()) }
}

async fn issue_grant(accounts: &Accounts, hash: Fingerprint, request: GrantRequest) -> Result<Answer, Refusal> {
    let GrantRequest { head, change, password } = request;
    let GrantChange::DeleteListeningData { account: wanted, target_revision, request_id } = change;
    let look = hash.clone();
    let clock = accounts.clock.clone();
    let restore = head.restore;
    let (number, device, email, check, stored) = answer(accounts.desk.ask(move |keeper| {
        let now = clock();
        let (number, device, mut account) = signed_in(keeper, &head, &look)?;
        target(account.state(), wanted, target_revision)?;
        let stored = keeper.data().password(number).map_err(unavailable)?;
        let expected = stored.as_ref().map_or_else(|| Fingerprint::from_bytes([0; 32]), |stored| Fingerprint::from_bytes(stored.hash));
        let email = account.state().email.clone();
        let check = account.request(restore, keeper.clock(now), |turn| {
            turn.check_leave(Bearer { device, hash: &look, grant: None }, GrantIntent { request_id, change: LeaveFor::DeleteHistory }, expected)
        }).apply(|effects| keeper.persist(effects, &[], now)).map_err(unavailable)?.map_err(said)?;
        Ok((number, device, email, check, stored))
    }).await)?;
    let Checked::Match { derived, .. } = accounts.workers.check(&email, PasswordDoor::KnownDevice, Some(device), password, stored).await?
        else { return Err(Refusal::NotAllowed(NotAllowed::WrongLogin)) };
    let clock = accounts.clock.clone();
    answer(accounts.desk.ask(move |keeper| {
        let now = clock();
        let checked = check.check(derived, keeper.clock(now)).map_err(said)?;
        let mut account = keeper.load(number).map_err(unavailable)?;
        let grant = GrantSecret::from_bytes(*setup::random::<32>().ok_or(Refusal::NotDealtWith)?);
        account.request(restore, keeper.clock(now), |turn| turn.grant_leave(checked, &grant))
            .apply(|effects| keeper.persist(effects, &[], now)).map_err(unavailable)?.map_err(said)?;
        result(Reply::Done { result: GrantIssued { grant }, receipt: receipt(keeper, account.state())? }, account::encode)
    }).await)
}

async fn delete(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let outcome = async {
        let hash = bearer(&calls, &headers)?;
        let request = secret_body(body, account::read::<DeleteListeningData>).map_err(|_| Refusal::Malformed)?;
        let clock = calls.accounts.clock.clone();
        answer(calls.accounts.desk.ask(move |keeper| {
            let now = clock();
            let (_, device, mut account) = signed_in(keeper, &request.head, &hash)?;
            target(account.state(), request.change.account, request.change.target_revision)?;
            schema(keeper.data().connection(), "listening_events", SCHEMA).map_err(unavailable)?;
            let grant = Some(GrantUse { request_id: request.request_id, secret: &request.grant });
            account.request(request.head.restore, keeper.clock(now), |turn| turn.delete_history(Bearer { device, hash: &hash, grant }))
                .apply(|effects| keeper.persist(effects, &[], now)).map_err(unavailable)?.map_err(said)?;
            result(Reply::Done { result: SignedOut {}, receipt: receipt(keeper, account.state())? }, account::encode)
        }).await)
    }.await;
    finish(outcome)
}

// CARRIED page shapes. Reuse the existing strict request reader for {head, after}.
// Encode the stamped envelope through core; result contents consist of SQLite numbers
// and JSON already encoded by the core's event writer or SQLite's json_quote.
pub(crate) fn list_response(page: String, receipt: Receipt) -> Result<Response<Body>, Refusal> {
    let envelope = account::encode(&Reply::Done { result: SignedOut {}, receipt }).map_err(unavailable)?;
    let envelope = std::str::from_utf8(envelope.bytes()).map_err(unavailable)?;
    let suffix = envelope.strip_prefix(r#"{"done":{"result":{}"#).ok_or(Refusal::NotDealtWith)?;
    let body = format!(r#"{{"done":{{"result":{page}{suffix}"#).into_bytes();
    if body.len() > account::LIST_REPLY_MAX_BYTES {
        return Err(Refusal::NotDealtWith);
    }
    Ok(json(account::DONE_STATUS, body))
}

async fn stats(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let outcome = async {
        let hash = bearer(&calls, &headers)?;
        let request = account::read::<ListDevices>(&body).map_err(|_| Refusal::Malformed)?;
        let clock = calls.accounts.clock.clone();
        let (page, stamp) = answer(calls.accounts.desk.ask(move |keeper| {
            let (number, _, account) = authenticated(keeper, &request.head, &hash, Access::ReadHistory, clock())?;
            let db = keeper.data().connection();
            schema(db, "listening_events", SCHEMA).map_err(unavailable)?;
            let page = stats_page(db, number, request.after).map_err(unavailable)?;
            Ok((page, receipt(keeper, account.state())?))
        }).await)?;
        list_response(page, stamp)
    }.await;
    outcome.unwrap_or_else(|reason| respond(refused(reason)))
}

fn stats_page(db: &Connection, number: u64, after: Option<DeviceId>) -> rusqlite::Result<String> {
    let number = sql(number)?;
    let total = db.query_row("SELECT starts FROM listening_totals WHERE account = ?1", [number], |row| row.get::<_, i64>(0))
        .optional()?.unwrap_or(0);
    let cursor = match after {
        None => Some((i64::MAX, i64::MAX)),
        Some(id) => db.query_row(
            "SELECT at_ms, row_id FROM listening_events WHERE row_id = ?1 AND account = ?2 AND is_start = 1",
            params![sql(id.get())?, number],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?,
    };
    let mut page = format!(r#"{{"starts":{total},"recent":["#);
    let mut next = None;
    if let Some((time, id)) = cursor {
        let mut query = db.prepare(
            "SELECT row_id, device, line FROM listening_events
             WHERE account = ?1 AND is_start = 1 AND (at_ms, row_id) < (?2, ?3)
             ORDER BY at_ms DESC, row_id DESC LIMIT ?4",
        )?;
        let mut rows = query.query(params![number, time, id, (STATS_PAGE + 1) as i64])?;
        for index in 0..STATS_PAGE {
            let Some(row) = rows.next()? else { break };
            let id: i64 = row.get(0)?;
            let device: i64 = row.get(1)?;
            let line: String = row.get(2)?;
            if index != 0 {
                page.push(',');
            }
            let _ = write!(page, r#"{{"device":{device},"event":{}}}"#, line.trim_end());
            if index + 1 == STATS_PAGE && rows.next()?.is_some() {
                next = Some(id);
            }
        }
    }
    let next = next.map_or_else(|| "null".into(), |id| id.to_string());
    let _ = write!(page, r#"],"next":{next}}}"#);
    Ok(page)
}
