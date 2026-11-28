//! Listening for the admin page. Only the short authorization and snapshot
//! fence use the writer. Aggregation runs on a separate WAL reader.

use std::collections::{HashMap, hash_map::Entry};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Response};
use axum::routing::post;
use hmac::{Hmac, KeyInit, Mac};
use nexus_core::link::text::{base64, from_base64};
use nexus_core::nexus::account::{AccountId, Name, NotSignedIn, RequestHead, Wait};
use nexus_core::nexus::admin::{self, Refusal, Reply, listening::*};
use nexus_core::nexus::kept::Fingerprint;
use nexus_core::nexus::proto::BackendId;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params_from_iter};
use sha2::Sha256;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

use super::music::{answer, caller, live_admin, not_dealt_with, refused};
use super::session::{Accounts, Answer};
use super::setup::derive;
use crate::events::respond;
use crate::store::{data, keeper::Keeper};

// The ranking owns at most one set of display strings per candidate, each
// already bounded by the journal's event size. Input text cannot exhaust a
// separate budget before this many starts have been considered.
const MAX_OVERVIEW_CANDIDATES: usize = 32_000;
// Leave room to skip backdated starts above the cursor's watermark.
const MAX_PAGE_CANDIDATES: usize = 40_000;
const MAX_READ_TIME: Duration = Duration::from_secs(2);
const CURSOR_PAYLOAD: usize = 48;
const CURSOR_BYTES: usize = CURSOR_PAYLOAD + 32;

#[derive(Clone)]
struct Calls {
    accounts: Arc<Accounts>,
    origin: Option<Arc<str>>,
    readers: Arc<Semaphore>,
}

pub(crate) fn router(base: Router, accounts: Arc<Accounts>, origin: Option<Arc<str>>) -> Router {
    base.merge(Router::new().route(PATH, post(listening))
        .with_state(Calls { accounts, origin, readers: Arc::new(Semaphore::new(1)) }))
}

fn busy() -> Refusal {
    Refusal::Busy { retry_after_seconds: Wait::new(1) }
}

async fn listening(State(calls): State<Calls>, headers: HeaderMap, body: Bytes) -> Response<Body> {
    let result = async {
        // The common listener has already refused app sessions before the body.
        if !crate::admin::browser_origin_allowed(&headers, calls.origin.as_deref()) {
            return Err(Refusal::Malformed);
        }
        let hash = crate::accounts::routes::bearer(&headers)
            .map(|session| Fingerprint::of(session.expose_bytes()))
            .ok_or(Refusal::NotSignedIn(NotSignedIn::SessionEnded))?;
        let request = admin::read::<Request>(&body).map_err(|_| Refusal::Malformed)?;
        calls.accounts.admin_live(&request.head.speaks, request.head.restore, &hash)?;

        let clock = calls.accounts.clock.clone();
        let head = RequestHead { speaks: request.head.speaks, restore: request.head.restore };
        let lookup = hash.clone();
        let access = calls.accounts.desk.ask(move |keeper| {
            let now = clock();
            authorize(keeper, &head, &lookup, now)?;
            Ok::<_, Refusal>(Reader {
                path: keeper.data().path().to_owned(),
                backend: keeper.backend(),
                restore: keeper.restore(),
                now: keeper.clock(now).effective(),
                key: derive(&keeper.seal_key(), b"Nexus Music listening cursor key v1\0"),
            })
        }).await.map_err(|_| busy())??;

        // Check authority first, then hold the slot through the blocking read.
        let place = calls.readers.try_acquire_owned().map_err(|_| busy())?;
        let head = RequestHead { speaks: request.head.speaks, restore: request.head.restore };
        let (changes, reply) = tokio::task::spawn_blocking(move || {
            let _place = place;
            access.read(request)
        }).await.map_err(not_dealt_with)??;

        let clock = calls.accounts.clock.clone();
        calls.accounts.desk.ask(move |keeper| {
            // Loading may finish a pending deletion. Read the change mark
            // after that repair and before recording this session's activity.
            let (_, device, mut account) = caller(keeper, &head, &hash)?;
            let current = keeper.data().meta().map_err(not_dealt_with)?.changes;
            live_admin(keeper, &mut account, head.restore, device, &hash, clock())?;
            if current != changes {
                return Err(busy());
            }
            Ok(reply)
        }).await.map_err(|_| busy())?
    }.await;
    respond(result.unwrap_or_else(refused))
}

fn authorize(keeper: &mut Keeper, head: &RequestHead, hash: &Fingerprint, now: u64) -> Result<(), Refusal> {
    let (_, device, mut account) = caller(keeper, head, hash)?;
    live_admin(keeper, &mut account, head.restore, device, hash, now)
}

struct Reader {
    path: PathBuf,
    backend: BackendId,
    restore: u64,
    now: u64,
    key: Zeroizing<[u8; 32]>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Overview,
    People,
    Recent,
}

impl Mode {
    fn byte(self) -> u8 {
        match self {
            Self::Overview => 0,
            Self::People => 1,
            Self::Recent => 2,
        }
    }
}

#[derive(Clone, Copy)]
struct Window {
    end: u64,
    watermark: u64,
    floor: (u64, u64),
    after: (u64, u64),
}

struct Scope<'a> {
    reader: &'a Reader,
    account: Option<AccountId>,
    days: Days,
}

impl Scope<'_> {
    fn mac(&self, mode: Mode, fields: &[u8]) -> Result<Hmac<Sha256>, Refusal> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.reader.key.as_ref()).map_err(not_dealt_with)?;
        mac.update(b"Nexus listening cursor v2\0");
        mac.update(self.reader.backend.as_bytes());
        mac.update(&self.reader.restore.to_be_bytes());
        mac.update(&self.account.map_or(0, AccountId::get).to_be_bytes());
        mac.update(&self.days.get().to_be_bytes());
        mac.update(&[mode.byte()]);
        mac.update(fields);
        Ok(mac)
    }

    fn cursor(&self, mode: Mode, window: Window, after: (u64, u64)) -> Result<Cursor, Refusal> {
        let mut raw = [0; CURSOR_BYTES];
        for (chunk, value) in raw[..CURSOR_PAYLOAD].chunks_exact_mut(8)
            .zip([window.end, window.watermark, window.floor.0, window.floor.1, after.0, after.1])
        {
            chunk.copy_from_slice(&value.to_be_bytes());
        }
        let tag = self.mac(mode, &raw[..CURSOR_PAYLOAD])?.finalize().into_bytes();
        raw[CURSOR_PAYLOAD..].copy_from_slice(&tag);
        Cursor::new(base64(&raw)).ok_or(Refusal::NotDealtWith)
    }

    fn resume(&self, mode: Mode, cursor: &Cursor) -> Result<Window, Refusal> {
        let raw: [u8; CURSOR_BYTES] = from_base64(cursor.as_str())
            .and_then(|bytes| bytes.try_into().ok()).ok_or(Refusal::Malformed)?;
        let (fields, tag) = raw.split_at(CURSOR_PAYLOAD);
        self.mac(mode, fields)?.verify_slice(tag).map_err(|_| Refusal::Malformed)?;
        let word = |at: usize| fields[at..at + 8].try_into().map(u64::from_be_bytes)
            .map_err(|_| Refusal::Malformed);
        Ok(Window {
            end: word(0)?, watermark: word(8)?,
            floor: (word(16)?, word(24)?), after: (word(32)?, word(40)?),
        })
    }
}

impl Reader {
    fn read(self, request: Request) -> Result<(u64, Answer), Refusal> {
        let scope = Scope { reader: &self, account: request.account, days: request.days };
        let continuation = match &request.page {
            None => None,
            Some(Page::People(cursor)) => Some((Mode::People, scope.resume(Mode::People, cursor)?)),
            Some(Page::Recent(cursor)) => Some((Mode::Recent, scope.resume(Mode::Recent, cursor)?)),
        };

        // Keeper validated the path at startup. Opening its live WAL read-only
        // here avoids the backup helper's full integrity check on every page.
        let mut db = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
            .map_err(not_dealt_with)?;
        db.busy_timeout(Duration::from_millis(100)).map_err(not_dealt_with)?;
        let snapshot = db.transaction().map_err(not_dealt_with)?;
        let meta = data::meta_of(&snapshot).map_err(not_dealt_with)?;
        if meta.backend != self.backend || meta.restore != self.restore {
            return Err(Refusal::NotSignedIn(NotSignedIn::DataPutBack));
        }
        let (mode, window) = match continuation {
            Some(page) => page,
            None => {
                let watermark = snapshot.query_row(
                    "SELECT row_id FROM listening_events ORDER BY row_id DESC LIMIT 1",
                    [], |row| row.get::<_, i64>(0),
                ).optional().map_err(not_dealt_with)?.unwrap_or(0) as u64;
                (Mode::Overview, Window {
                    end: self.now, watermark, floor: (scope.days.start_ms(self.now), 0),
                    after: (self.now, i64::MAX as u64),
                })
            }
        };
        let result = scan(&snapshot, &scope, mode, window)?;
        snapshot.rollback().map_err(not_dealt_with)?;
        Ok((meta.changes, result))
    }
}

#[derive(Default)]
struct Results {
    people: HashMap<AccountId, Option<Counter>>,
    artists: HashMap<Arc<Text>, u64>,
    albums: HashMap<(Arc<Text>, Arc<Text>), u64>,
    songs: HashMap<(Arc<Text>, Arc<Text>, Arc<Text>), u64>,
    recent: Vec<Play>,
    starts: u64,
}

struct Counter {
    name: Name,
    starts: u64,
}

fn count<K: Eq + std::hash::Hash>(map: &mut HashMap<K, u64>, key: K) {
    *map.entry(key).or_default() += 1;
}

fn scan(db: &Connection, scope: &Scope<'_>, mode: Mode, mut window: Window) -> Result<Answer, Refusal> {
    let started = Instant::now();
    let mut found = Results::default();
    if window.end == 0 {
        return found.reply(scope, mode, window, None);
    }
    let capacity = if mode == Mode::Overview { MAX_OVERVIEW_CANDIDATES } else { MAX_PAGE_CANDIDATES };
    let (through_ms, through_id) = if mode == Mode::Recent {
        (window.after.0, window.after.1 - 1)
    } else {
        (window.end - 1, i64::MAX as u64)
    };
    let mut params = vec![window.floor.0, window.floor.1, through_ms, through_id, capacity as u64 + 1];
    let (index, filter) = match scope.account {
        Some(account) if account.get() > i64::MAX as u64 => return found.reply(scope, mode, window, None),
        Some(account) => {
            params.push(account.get());
            ("listening_recent", "AND account = ?6")
        }
        None => ("listening_by_time", ""),
    };
    let params = params.into_iter().map(i64::try_from).collect::<Result<Vec<_>, _>>().map_err(not_dealt_with)?;
    let track = if mode == Mode::People { "" } else {
        ", json_extract(line, '$.event.title'), json_extract(line, '$.event.artist'), json_extract(line, '$.event.album')"
    };
    let mut query = db.prepare(&format!(
        "SELECT row_id, account, at_ms {track} FROM listening_events INDEXED BY {index}
         WHERE is_start = 1 {filter}
           AND (at_ms, row_id) >= (?1, ?2) AND (at_ms, row_id) <= (?3, ?4)
         ORDER BY at_ms DESC, row_id DESC LIMIT ?5",
    )).map_err(not_dealt_with)?;
    let mut rows = query.query(params_from_iter(params)).map_err(not_dealt_with)?;
    let mut people = db.prepare("SELECT shown FROM accounts WHERE number = ?1 AND history_held = 0")
        .map_err(not_dealt_with)?;
    let mut seen = 0;
    let mut last = (window.end, 0);
    while let Some(row) = rows.next().map_err(not_dealt_with)? {
        if seen >= capacity || started.elapsed() >= MAX_READ_TIME {
            if mode == Mode::Overview {
                // Both cursors must use the same inclusive lower edge as the
                // displayed totals, even after deletion or a backdated send.
                window.floor = last;
                return found.reply(scope, mode, window, Some(Limit::NewestStarts));
            }
            return Results::default().reply(scope, mode, window, Some(Limit::ContinuationWork));
        }
        seen += 1;
        let row_id = number(row, 0)?;
        let at_ms = number(row, 2)?;
        last = (at_ms, row_id);
        if row_id > window.watermark {
            continue;
        }
        let account = AccountId::new(number(row, 1)?).ok_or(Refusal::NotDealtWith)?;
        let person = match found.people.entry(account) {
            Entry::Occupied(person) => person.into_mut(),
            Entry::Vacant(person) => {
                let name = people.query_row([account.get() as i64], |row| row.get::<_, String>(0))
                    .optional().map_err(not_dealt_with)?;
                let counter = name.map(|name| Name::new(&name).map(|name| Counter { name, starts: 0 })
                    .ok_or(Refusal::NotDealtWith)).transpose()?;
                person.insert(counter)
            }
        };
        let Some(person) = person else { continue };
        person.starts += 1;
        found.starts += 1;
        if mode == Mode::People {
            continue;
        }

        let take_recent = found.recent.len() <= PAGE_SIZE;
        if mode == Mode::Overview || take_recent {
            let title: String = row.get(3).map_err(not_dealt_with)?;
            let artist: String = row.get(4).map_err(not_dealt_with)?;
            let album: String = row.get(5).map_err(not_dealt_with)?;
            let text = |value| Text::new(value).ok_or(Refusal::NotDealtWith);
            let (title, artist, album) = (text(title)?, text(artist)?, text(album)?);
            if take_recent {
                found.recent.push(Play {
                    id: row_id, at_ms, account, name: person.name.clone(),
                    title: title.clone(), artist: artist.clone(), album: album.clone(),
                });
            }
            if mode == Mode::Overview {
                let (title, artist, album) = (Arc::new(title), Arc::new(artist), Arc::new(album));
                count(&mut found.artists, artist.clone());
                count(&mut found.albums, (artist.clone(), album.clone()));
                count(&mut found.songs, (artist, album, title));
            }
        }
        if mode == Mode::Recent && found.recent.len() > PAGE_SIZE {
            break;
        }
    }

    found.reply(scope, mode, window, None)
}

fn number(row: &rusqlite::Row<'_>, column: usize) -> Result<u64, Refusal> {
    let value: i64 = row.get(column).map_err(not_dealt_with)?;
    u64::try_from(value).map_err(not_dealt_with)
}

fn first_n<T>(mut rows: Vec<T>, n: usize, cmp: impl Fn(&T, &T) -> std::cmp::Ordering) -> Vec<T> {
    if rows.len() > n {
        rows.select_nth_unstable_by(n, &cmp);
        rows.truncate(n);
    }
    rows.sort_unstable_by(cmp);
    rows
}

fn top<K: Ord>(counts: HashMap<K, u64>) -> Vec<(K, u64)> {
    first_n(counts.into_iter().collect(), TOP_SIZE, |a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)))
}

fn bounded<T, const N: usize>(rows: Vec<T>) -> Result<Rows<T, N>, Refusal> {
    Rows::new(rows).ok_or(Refusal::NotDealtWith)
}

impl Results {
    fn reply(mut self, scope: &Scope<'_>, mode: Mode, window: Window, limit: Option<Limit>) -> Result<Answer, Refusal> {
        let page_limit = limit.filter(|_| mode != Mode::Overview);
        let recent = if mode == Mode::People {
            None
        } else {
            let more = self.recent.len() > PAGE_SIZE;
            self.recent.truncate(PAGE_SIZE);
            let next = self.recent.last().filter(|_| more).map(|last| {
                scope.cursor(Mode::Recent, window, (last.at_ms, last.id))
            }).transpose()?;
            Some(RecentPage { rows: bounded(self.recent)?, next, limit: page_limit })
        };
        if mode == Mode::Recent {
            return Ok(answer(Reply::Done(recent.ok_or(Refusal::NotDealtWith)?)));
        }

        let mut people = first_n(self.people.into_iter().filter_map(|(account, counter)| {
            let Counter { name, starts } = counter?;
            let (prior_count, prior_account) = window.after;
            (mode == Mode::Overview || starts < prior_count || (starts == prior_count && account.get() > prior_account))
                .then_some(Person { account, name, starts })
        }).collect(), PAGE_SIZE + 1, |a, b| b.starts.cmp(&a.starts).then_with(|| a.account.get().cmp(&b.account.get())));
        let more = people.len() > PAGE_SIZE;
        people.truncate(PAGE_SIZE);
        let next = people.last().filter(|_| more).map(|last| {
            scope.cursor(Mode::People, window, (last.starts, last.account.get()))
        }).transpose()?;
        let people = PeoplePage { rows: bounded(people)?, next, limit: page_limit };
        if mode == Mode::People {
            return Ok(answer(Reply::Done(people)));
        }

        Ok(answer(Reply::Done(Overview {
            start_ms: scope.days.start_ms(window.end),
            end_ms: window.end,
            starts: self.starts,
            limit,
            artists: bounded(top(self.artists).into_iter().map(|(artist, starts)| Artist {
                artist: Arc::unwrap_or_clone(artist), starts,
            }).collect())?,
            albums: bounded(top(self.albums).into_iter().map(|((artist, album), starts)| Album {
                artist: Arc::unwrap_or_clone(artist), album: Arc::unwrap_or_clone(album), starts,
            }).collect())?,
            songs: bounded(top(self.songs).into_iter().map(|((artist, album, title), starts)| Song {
                artist: Arc::unwrap_or_clone(artist), album: Arc::unwrap_or_clone(album), title: Arc::unwrap_or_clone(title), starts,
            }).collect())?,
            people,
            recent: recent.ok_or(Refusal::NotDealtWith)?,
        })))
    }
}
