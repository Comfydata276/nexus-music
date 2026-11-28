//! One device's dealings with the Nexus server, as state and things to do (design 5.4, 5.5,
//! section 6 and 22: slice C10; `backend-slice-notes.md`, "C10"). **No file is opened
//! here**, and nothing here has a socket, a clock, a thread or a source of randomness: the
//! app hands in the time and random bytes with each call, and is told what to do.
//!
//! **Each app drives it by the same four calls, in the same order** (5.5):
//!
//! 1. [`Client::start`], once the app is already usable (6.1), with what was kept on the
//!    device ([`Kept`]). **A music login that was kept is handed to the player first,
//!    before anything is asked** (R1, 6.1): playing never waits on this.
//! 2. [`Client::happened`], whenever something does ([`Happened`]): a press, an answer, a
//!    failure, the network, Offline, a tick, the music server refusing the login.
//! 3. For each thing given back, **do** it, **in the order given**. Each is one of a closed
//!    set ([`Do`]): **ask** the backend this, **keep** this (whole), **forget** this,
//!    **show** this, **hand** the player this, and for the sending half, **read or change
//!    the journal** ([`Do::Journal`]). Nothing else.
//! 4. [`Client::stop`], at the moment the account changes: nothing asked for the old one is
//!    acted on afterwards (ruled: an account switch fences the old account's work).
//!
//! The phone reaches the same four calls through one entry point taking and giving JSON,
//! [`Client::call_json`], as it reaches the rest of this library.
//!
//! **The rules of section 6 are held here, so that neither app can get one wrong:**
//!
//! - **Playing never waits.** There is no "hold the player" among the things to do. A kept
//!   music login is handed over at `start` before any ask. A backend that is down, stalled,
//!   saying something malformed or out of date never takes the login away. Only the
//!   backend saying, to this session, that this session has ended does (6.3's table), and
//!   the person signing out here.
//! - **A refused music login (6.3, as ruled), one rule:** every part of the app that asks
//!   the music server reports a refusal with the login's number; the first report for a
//!   number sets it aside ([`Hand::Refused`]: begin no new request with it); **one renewal
//!   for each number**, asked of the backend in the background; a login with a higher
//!   number is handed over in one step, and what waited is tried once more under it;
//!   nothing newer, and it stays set aside, renewed again only when the network comes back
//!   or an answer says the number has moved. Never in a loop, never on a timer, and **it
//!   never signs anyone out.**
//! - **The session goes to the backend alone, the music login to the player alone.** Every
//!   [`Ask`] is to the Nexus origin the person typed, under `/nexus/api/`; the music login is
//!   in no ask.
//! - **The Nexus origin is the one a person typed** (7.1), never one made from the music
//!   server's address; **the backend's id is expected there**, and once signed in an answer
//!   from another id is not the backend's: nothing more is sent, and the row says so.
//! - **A redirect is never followed**, and is never an answer of the backend's.
//! - **One ask is one request**, and an answer is taken once. **Each try at signing in is
//!   numbered** (its ask's id): only the latest try's answer is used (8.6).
//! - **An answer for an ask no longer waited for is thrown away**: after `stop`, after a
//!   sign-out or a sign-in, after Offline was switched on. A late answer for the first
//!   account cannot touch the second (11).
//! - **Offline means nothing is asked**, and nothing queued is sent: **THE APP CALLS
//!   [`Client::waits_for`] IMMEDIATELY BEFORE IT SENDS EACH [`Ask`] AND DROPS IT IF THE
//!   ANSWER IS NO.** `Offline(true)` and `stop` make it no for everything but a sign-out
//!   being told.
//! - **One `hello` each run before anything secret goes to an origin** (7.1): a session, a
//!   device secret or a note goes only to the backend id expected there. Another id is sent
//!   nothing, nothing is forgotten, the music plays on, and the person is asked to sign in.
//! - **A tick alone asks nothing of the session.** A device learns it was signed out at its
//!   next call carrying the session: at a start, the network coming back, Offline off, a
//!   press (and from step 2, what it sends).
//! - **What is kept is one thing, put in place in one step** (11): [`Keep::Saved`], whole.
//! - **Signing out is lasting on the device at once**, with or without the backend: the
//!   note that can end that one session is kept first ([`Note`], C7's revoke-only secret),
//!   then the session and the music login are forgotten; the backend is told from the note
//!   now if it can be, and again until its own backend says done or that the session is
//!   gone, or it is 90 days old (11, as ruled). Notes are kept by the session they end.
//! - **"The server's data was put back"** (12.3, 12.4): the session goes and the person
//!   signs in again; the music login is kept, and plays.
//!
//! **What a person sees is decided here too** ([`Shown`]), in words this module owns, so the
//! two apps say the same.
//!
//! **The sending half** (13.3, 14.4; C8's `events`, C6's `diag`): what was listened to, and
//! the Diagnostics report, go to the backend as [`Ask`]s like any other: under the session,
//! after this run's `hello` named the backend expected, never while Offline, fenced at every
//! account change, `waits_for` before each send. **The player and the journal's writer are
//! never in this machine's call graph:** it holds only the journal's summary and the batches
//! it asked for. What the apps do:
//!
//! - **THE JOURNAL REPORTS [`Happened::EventsWaiting`]** (its account, life, how many wait,
//!   the lowest waiting sequence) after a flush (at most once a second), at start, when its
//!   account changes at a sign-in, and after doing any [`JournalDo`] that changes what waits.
//!   The writer never waits for the machine.
//! - **[`JournalDo::ReadEvents`] IS DONE BY THE JOURNAL'S READER, NEVER ITS WRITER, AND EVERY
//!   READ IS ANSWERED** with [`Happened::EventsRead`] under its `read`, with no events when
//!   there are none (that ends the round). Likewise **[`JournalDo::ReadDiagnostics`] IS
//!   ANSWERED** with [`Happened::DiagnosticsRead`] (the report's text, `None` if it cannot),
//!   read from the record's own file, taking no lock the player takes. A read not answered
//!   stops that kind of sending until the next start.
//! - **[`JournalDo::Taken`], [`JournalDo::Refused`], [`JournalDo::LifeMoved`] and
//!   [`JournalDo::ResendAll`] ARE DONE IN THE ORDER GIVEN, AFTER THE THINGS BEFORE THEM.**
//!   `LifeMoved` always follows the [`Keep::Saved`] that carries the new life, so nothing is
//!   removed before what replaces it is kept; `ResendAll` comes before the `Keep::Saved`
//!   that drops the session at "the data was put back".
//! - **THE APP KEEPS [`Keep::DiagSchedule`] (PER DEVICE) AND HANDS IT BACK AT START**
//!   ([`Kept::diag_schedule`]), with the person's own Diagnostics choice
//!   ([`Kept::diagnostics`]; changes by [`Happened::DiagnosticsSwitch`]). It reports
//!   [`Happened::DiagnosticsFault`] when it notes something going wrong, and
//!   [`Happened::InUse`] when it starts or stops being used (`false` at each start).
//!
//! **When.** Events: a round begins on a new summary, on the backend being reachable again
//! (a start, the network back, Offline off, an answer after it could not be reached), or on
//! an answer once what kept it back is gone (the `hello` read, a sign-in). **NEVER ON A BARE
//! TICK.** Up to 500 events and 512 KiB to a request, each answered by itself; the round reads
//! on from after what it sent until a read is empty. An answer that takes or refuses
//! nothing, "busy", a refusal of the whole call, a failure or an answer that does not read
//! **holds** sending, without trying again by itself: until the backend is reachable again
//! (never from that same answer), or, while it answers, more of this account's events wait
//! than when it held, past any "come back in" it said. Diagnostics: on a tick only, because
//! 14.4 names its timing (C6's [`diag::Schedule`]: a few minutes after a fault, else daily
//! while in use, never within ten minutes, thirty a day); the switch is
//! [`diag::enabled`]`(the person's choice, hello's default)`, and nothing is sent until a
//! `hello` read this run has said that default (absent there is "on"). One request; a failure
//! wastes that attempt only.
//!
//! **The fence.** An event written under one account is never sent under another: a read is
//! asked for the signed-in account and life, and an [`Happened::EventsRead`] for another
//! account or life, or from before a stop, a sign-out, Offline or a sign-in, is dropped. An
//! [`Happened::EventsWaiting`] for any account or life but the signed-in one's is ignored
//! entirely. **When any answer says the life moved**, everything read, queued or held under
//! the earlier life is retired at once: its batch fails `waits_for`, its read and its answer
//! are dropped, and the new life's events go at the next thing that changes. **When the
//! Diagnostics switch becomes off** (the person's choice, or a `hello` whose default is off
//! with no choice made), a read out is dropped when it answers and an upload queued fails
//! `waits_for`. A read answered empty after a summary came in while it was out is read once
//! more, from the same place.
//!
//! **"The data was put back"** (13.3), from whichever answer says it (a call under the
//! session, or a sign-out note's telling): [`JournalDo::ResendAll`] names the BACKEND. Every
//! account's journal kept under that backend id is told at once, and each account's events
//! are sent again under that account only, when it is next signed in. The account signed in
//! now and its session are not touched by a note's answer.

use super::account::{
    self, AccountId, ChangePassword, Credential, DeviceId, DeviceKind, DevicePage, DeviceSecret, Email, ForceOut, ForcedOut,
    GetMusicLogin, ListDevices, MusicLogin, MusicLoginResult, Name, NexusOrigin, NotAllowed, NotSignedIn, OneTimeCode, Password,
    PasswordChanged, Receipt, Refusal, Reply, RequestHead, Session, SessionId, SignIn, SignInAnswer, SignOut, SignOutNote, SignedOut,
};
use super::diag;
use super::events::{self, EventId, Placed};
use super::proto::{self, BackendId, Found, SPEAKS, Speaks, Version};
use serde::Serialize;
use std::fmt;
use zeroize::Zeroizing;

mod json;
mod app_calls;
pub use app_calls::{AppResult, AppScope, AppSession};
mod machine;
mod sending;
mod words;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------------------

/// How long `hello` may take, from the first byte sent to the last byte read.
// the design does not say. Chosen: a server that has not answered in this long is not one a
// person is waiting for.
pub const HELLO_WITHIN_MS: u64 = 10_000;
/// How long signing in may take. A password may wait its turn at the backend (8.3): ten
/// seconds in the queue, and up to a minute after a wrong password at its door.
// the design does not say. Chosen to cover both, with room.
pub const SIGN_IN_WITHIN_MS: u64 = 90_000;
/// How long any other call may take.
// the design does not say.
pub const CALL_WITHIN_MS: u64 = 15_000;
/// How often the backend is told again of a sign-out it has not taken, besides every
/// start, the network coming back and Offline going off.
// the design does not say.
pub const TELL_EVERY_MS: u64 = 60 * 60 * 1000;
/// How long a [`Note`] is tried: the session it would end is gone by then anyway (8.7).
pub const NOTE_LIFE_MS: u64 = account::SESSION_IDLE_MS;
/// The most sign-outs waiting to be told at once, each keyed by the session it ends. When
/// a fifth comes, the oldest expired goes first, else the oldest.
pub const NOTES_MAX: usize = 4;
/// The most a typed address or email is taken to be: longer is refused before anything is
/// kept or shown of it.
pub const TYPED_MAX_BYTES: usize = 1024;
/// How often the app is to call `happened` with [`Happened::Tick`]. A tick sooner or later
/// costs nothing: the machine keeps its own times.
pub const TICK_MS: u64 = 60 * 1000;
/// The most a `hello` answer may be (proto's own bound).
pub const HELLO_MAX_BYTES: usize = proto::HELLO_MAX_BYTES;
/// The most asks waited for at once. Each kind of ask is one at a time, so this is never
/// reached; it is the bound that makes that certain.
const FLIGHT_MAX: usize = 8;
/// The most pages of the devices list read, one after another.
// the design does not say. account allows 256 devices to a page.
const DEVICE_PAGES_MAX: usize = 4;

// ---------------------------------------------------------------------------------------
// What the app hands in
// ---------------------------------------------------------------------------------------

/// Who this app is, handed in at `start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct App {
    pub version: Version,
    pub kind: DeviceKind,
    /// What this device is called in the person's list of devices.
    pub device_name: Name,
}

/// Handed in with every call: the time, and fresh random bytes.
#[derive(Debug, Clone)]
pub struct Given {
    /// Milliseconds since 1970, UTC, on the device's own clock. Used for what is shown
    /// ("not reachable since 14:02", "used yesterday") and for spacing checks out; never
    /// for a deadline (each [`Ask`] carries its own as a length of time).
    pub now_ms: u64,
    /// Minutes east of UTC where the device is, now: only for showing a time of day.
    pub local_offset_minutes: i32,
    /// 32 bytes from the system's source of randomness, or `None` if it gave none. Nothing
    /// is ever made up another way: what needs them waits for a call that has them.
    pub random: Option<Random>,
}

/// 32 random bytes. Their debug form shows nothing of them.
#[derive(Clone)]
pub struct Random(Zeroizing<[u8; 32]>);

impl Random {
    pub fn new(bytes: [u8; 32]) -> Self {
        Random(Zeroizing::new(bytes))
    }
}

impl fmt::Debug for Random {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Random(..)")
    }
}

/// The device's own secret (8.7), and the backend it is this device's secret for. **It is
/// per backend id**: one made before any sign-in (`backend_id: None`) may go to the first;
/// after that it goes only to that backend, and a sign-in to another id is made as a new
/// device, with a new secret kept only if that sign-in succeeds. The old one is never sent
/// to another id.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceKey {
    pub secret: DeviceSecret,
    /// The backend's id, 32 lower-case hex digits, once a sign-in with it has succeeded.
    pub backend_id: Option<String>,
}

/// What was kept on the device, read back at `start`. Anything missing is `None`.
#[derive(Debug, Default)]
pub struct Kept {
    /// Where secrets are kept: the device's own secret, and its backend. Beside [`Saved`].
    pub device_secret: Option<DeviceKey>,
    /// Where secrets are kept, as one thing: what signing in left (11).
    pub saved: Option<Saved>,
    /// Where secrets are kept: sign-outs the backend has not yet taken, at most
    /// [`NOTES_MAX`] (more are trimmed, oldest first), each kept and forgotten by its
    /// backend and the session it ends.
    pub notes: Vec<Note>,
    /// Whether Offline mode is on (the app's own setting).
    pub offline: bool,
    /// What was kept but did not read (an unknown member, a wrong or older form): treated as
    /// absent, and forgotten where it is kept (as ruled). What is kept can never stop a start.
    pub unreadable: Vec<Forget>,
    /// The person's own choice for sending Diagnostics from this device (14.3): `None` until
    /// they choose, and then the server's default no longer counts.
    pub diagnostics: Option<bool>,
    /// When Diagnostics were last sent from this device (14.4), as [`Keep::DiagSchedule`]
    /// gave it: per device, not per account.
    pub diag_schedule: Option<DiagSchedule>,
}

/// C6's [`diag::Schedule`] as data, to keep across restarts: when the device started being
/// used, the first fault not yet sent for, and the attempts of the last day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagSchedule {
    pub started_ms: u64,
    #[serde(default)]
    pub fault_ms: Option<u64>,
    #[serde(default)]
    pub attempts: Vec<u64>,
}

impl DiagSchedule {
    fn of(schedule: &diag::Schedule) -> DiagSchedule {
        DiagSchedule { started_ms: schedule.started_ms(), fault_ms: schedule.fault_ms(), attempts: schedule.attempts().collect() }
    }

    fn schedule(&self) -> Option<diag::Schedule> {
        diag::Schedule::restore(self.started_ms, self.fault_ms, &self.attempts)
    }
}

/// Whose journal: the backend and the account a journal's folder is kept under (11, 13.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalAccount {
    pub backend_id: BackendId,
    pub account: AccountId,
}

impl Serialize for JournalAccount {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut out = serializer.serialize_struct("JournalAccount", 2)?;
        out.serialize_field("backend_id", &self.backend_id.to_hex())?;
        out.serialize_field("account", &self.account)?;
        out.end()
    }
}

/// What signing in leaves on a device: **one thing, put in place in one step** (11). A
/// write that is cut short must leave the one before whole. What a device keeps for the
/// person (its folder) is kept under `backend_id` and `account`, never under the music
/// server's name and a user's name.
#[derive(Debug, Serialize)]
pub struct Saved {
    /// The Nexus origin the person typed, as `https://host` or `https://host:port`.
    pub nexus: String,
    /// The backend's id, from its `hello`, expected at that origin: 32 lower-case hex digits.
    pub backend_id: String,
    /// Who the person is to the backend: the account's number, never given again (8.1).
    pub account: AccountId,
    pub email: String,
    pub name: String,
    /// This device's number at the backend.
    pub device: u64,
    /// The numbers from the latest answer's stamp (12.3).
    pub restore: u64,
    pub life: u64,
    pub change: u64,
    /// What this server records (13.6), until the person has seen it.
    pub notice: Option<String>,
    /// The session. `None` after "the server's data was put back": the music login stays,
    /// and plays, until the person signs in again.
    pub session: Option<Session>,
    /// What can end this session and nothing else, for signing out without it (11).
    pub sign_out: Option<SignOutNote>,
    /// The music login, with its number (9.4).
    pub music: Option<MusicLogin>,
}

/// A sign-out the backend has not yet taken (11): it can end that one session and do
/// nothing else, and nothing in it can sign anyone in. It holds the revoke-only secret the
/// sign-in answer gave ([`SignOutNote`]), never the session.
#[derive(Debug, Serialize)]
pub struct Note {
    /// The number of this sign-out (12.2), the same for every telling of it. Never 0.
    pub request: u64,
    /// The session it ends, and the secret that ends it.
    pub revoke: SignOutNote,
    /// The Nexus origin it is to be told at, and the id of the backend it was made for:
    /// it is sent nowhere else (7.1).
    pub nexus: String,
    pub backend_id: String,
    pub restore: u64,
    /// When the person signed out: the note is given up [`NOTE_LIFE_MS`] after.
    pub made_ms: u64,
}

impl Note {
    /// The session it ends.
    pub fn session(&self) -> u64 {
        self.revoke.session.get()
    }

    /// What it is kept and forgotten by: its backend and the session it ends. Two backends
    /// may each give out the same session number.
    pub fn key(&self) -> (&str, u64) {
        (&self.backend_id, self.session())
    }

    fn forget(&self) -> Forget {
        Forget::Note { backend_id: self.backend_id.clone(), session: self.session() }
    }
}

/// Something that happened. Its debug form shows no Diagnostics text and no event's
/// content (its events show as `Placed(..)`): lengths and counts only.
pub enum Happened {
    /// The person pressed something.
    Pressed(Press),
    /// Start one typed feature POST. The caller's request number only correlates effects.
    AppPost { request: u64, scope: AppScope, path: String, body: Typed },
    /// The caller stopped waiting. An ask still waiting at the send is retired.
    AppCancelled { request: u64 },
    /// An answer came back to an ask: its status and its whole body, as the transport read
    /// it (never more than the ask's `max_body`; a body cut short is [`How::Cut`] instead).
    /// A redirect is handed in as it came: it is never followed.
    Answered { ask: u64, status: u16, body: AnswerBody },
    /// An ask had no answer.
    Failed { ask: u64, failure: Failure },
    /// The device's network went away or came back.
    Network { up: bool },
    /// Offline mode was switched on or off.
    Offline(bool),
    /// A tick: about every [`TICK_MS`]. A tick alone asks nothing of the session; it tells
    /// waiting sign-outs at their pace, and sends Diagnostics when C6's schedule says. It
    /// never sends events.
    Tick,
    /// A part of the app that asks the music server (a stream, a range opened again, the
    /// next song fetched ahead, a download, a cover, a page) was refused "wrong name or
    /// password" with the music login of this number, handed under this `generation`
    /// ([`Hand`]). Only that: a network failure, a proxy error or a busy server is not this
    /// (6.3's table). A report under another generation is dropped.
    MusicRefused { count: u64, generation: u64 },
    /// On the phone: where the address of the music login of this number, handed under this
    /// generation, leads on this network, and whether the permission is held (6.4; P7's
    /// note: judged apart from the backend's address). A look at another login is dropped.
    MusicLooked { look: Look, count: u64, generation: u64 },
    /// The journal's summary for one account (13.2): after a flush (at most once a second),
    /// at start, after doing a [`JournalDo`] that changes what waits, and when the journal's
    /// account changes at a sign-in. `pending`: the events waiting in that life (not taken,
    /// not refused); `from_sequence`: the lowest of their sequence numbers. Only these
    /// numbers reach the machine. One for an account not signed in is not acted on.
    EventsWaiting { account: JournalAccount, life: u64, pending: u64, from_sequence: u64 },
    /// The answer to [`JournalDo::ReadEvents`] of this `read`: the waiting events read, in
    /// sequence order (none: nothing more to send). Dropped unless it is the read waited for
    /// and its account and life are the signed-in account's now.
    EventsRead { read: u64, account: JournalAccount, life: u64, events: Vec<Placed> },
    /// The answer to [`JournalDo::ReadDiagnostics`] of this `read`: the report's text as
    /// "What is sent" shows it, or `None` if it could not be read.
    DiagnosticsRead { read: u64, text: Option<String> },
    /// The person's own choice for sending Diagnostics from this device changed (`None`:
    /// back to the server's default). The app keeps it itself, and hands it in at start.
    DiagnosticsSwitch(Option<bool>),
    /// The app noted something going wrong, at this time (14.4: a few minutes after).
    DiagnosticsFault { at_ms: u64 },
    /// The app is in use, or no longer (14.4: daily "while in use"). `false` at each start.
    InUse(bool),
}

impl fmt::Debug for Happened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Happened::AppPost { request, scope, path, .. } => f.debug_struct("AppPost").field("request", request).field("scope", scope).field("path", path).finish(),
            Happened::AppCancelled { request } => f.debug_struct("AppCancelled").field("request", request).finish(),
            Happened::Pressed(press) => f.debug_tuple("Pressed").field(press).finish(),
            Happened::Answered { ask, status, body } => f.debug_struct("Answered").field("ask", ask).field("status", status).field("body", body).finish(),
            Happened::Failed { ask, failure } => f.debug_struct("Failed").field("ask", ask).field("failure", failure).finish(),
            Happened::Network { up } => f.debug_struct("Network").field("up", up).finish(),
            Happened::Offline(on) => f.debug_tuple("Offline").field(on).finish(),
            Happened::Tick => f.write_str("Tick"),
            Happened::MusicRefused { count, generation } => f.debug_struct("MusicRefused").field("count", count).field("generation", generation).finish(),
            Happened::MusicLooked { look, count, generation } => {
                f.debug_struct("MusicLooked").field("look", look).field("count", count).field("generation", generation).finish()
            }
            Happened::EventsWaiting { account, life, pending, from_sequence } => f
                .debug_struct("EventsWaiting")
                .field("account", account)
                .field("life", life)
                .field("pending", pending)
                .field("from_sequence", from_sequence)
                .finish(),
            Happened::EventsRead { read, account, life, events } => {
                f.debug_struct("EventsRead").field("read", read).field("account", account).field("life", life).field("events", &events.len()).finish()
            }
            Happened::DiagnosticsRead { read, text } => {
                f.debug_struct("DiagnosticsRead").field("read", read).field("text_bytes", &text.as_ref().map(String::len)).finish()
            }
            Happened::DiagnosticsSwitch(choice) => f.debug_tuple("DiagnosticsSwitch").field(choice).finish(),
            Happened::DiagnosticsFault { at_ms } => f.debug_struct("DiagnosticsFault").field("at_ms", at_ms).finish(),
            Happened::InUse(on) => f.debug_tuple("InUse").field(on).finish(),
        }
    }
}

/// On the phone, where an address leads on this network and whether the local-network
/// permission is held (6.4). Each `None` where the app did not or cannot look; a PC hands
/// in `Look::default()`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Look {
    #[serde(default)]
    pub inside: Option<bool>,
    #[serde(default)]
    pub permission: Option<bool>,
}

impl Look {
    /// It leads inside the house and the permission is not held: Android would drop the
    /// connection silently.
    fn blocked(self) -> bool {
        self.inside == Some(true) && self.permission == Some(false)
    }
}

/// What the person pressed.
#[derive(Debug)]
pub enum Press {
    /// Sign in with an email and a password. `look`: on the phone, where the typed address
    /// leads on this network and whether the permission is held, looked at before the press
    /// (6.4); on a PC, nothing. A press while an earlier one's `hello` is still out
    /// replaces it (that one's password is dropped, its answer asks nothing); a press while
    /// the sign-in itself is out does nothing.
    SignIn { address: String, email: String, password: Typed, look: Look },
    /// Sign in for the first time with the one-time code the admin gave (8.5): the code and
    /// the password chosen, typed twice, redeemed together in one call.
    SignInWithCode { address: String, code: Typed, password: Typed, again: Typed, look: Look },
    /// The sign-in form was closed or dismissed: a try still at its `hello` is dropped, with
    /// what was typed.
    FormClosed,
    /// Sign out on this device.
    SignOut,
    /// The list of devices was opened, or "refresh" pressed on it.
    OpenDevices,
    /// "Force out" on one device of the list.
    ForceOut { device: u64 },
    /// Change the password: the present one, and the new one typed twice.
    ChangePassword { current: Typed, new: Typed, again: Typed },
    /// The notice of what this server records (13.6) was seen.
    NoticeSeen,
}

/// Text a person typed that is secret: a password or a code. Its debug form shows nothing.
pub struct Typed(Zeroizing<String>);

impl Typed {
    pub fn new(text: &str) -> Self {
        Typed(Zeroizing::new(text.to_owned()))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Typed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Typed(..)")
    }
}

/// An answer's body, as read. Its debug form gives only its length: it may hold a session.
pub struct AnswerBody(Zeroizing<Vec<u8>>);

impl AnswerBody {
    pub fn new(bytes: Vec<u8>) -> Self {
        AnswerBody(Zeroizing::new(bytes))
    }
}

impl fmt::Debug for AnswerBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AnswerBody({} bytes)", self.0.len())
    }
}

/// Why an ask had no answer: how far it got, and on the phone where the address led.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Failure {
    pub how: How,
    /// Whether the address led inside the house on this network, if the app looked (6.4).
    #[serde(default)]
    pub inside: Option<bool>,
    /// Whether the phone holds the permission for the local network, where it has one.
    #[serde(default)]
    pub permission: Option<bool>,
}

/// How far an ask got before it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    /// The name was not found.
    Name,
    /// No connection was opened (refused, timed out, or dropped by the system).
    Connect,
    /// A connection was opened and could not be made secure: a certificate that is wrong.
    /// Never mistaken for a permission that is missing (P7's note).
    Secure,
    /// Secure, and no whole answer came in time.
    Timeout,
    /// The answer was cut short, or larger than the ask allowed.
    Cut,
}

impl How {
    /// Whether a connection was ever opened (6.4: the local-network check is made again
    /// after any failure in which none was).
    fn connected(self) -> bool {
        !matches!(self, How::Name | How::Connect)
    }
}

// ---------------------------------------------------------------------------------------
// What the app is told to do
// ---------------------------------------------------------------------------------------

/// The closed set of things to do. **There is no other**, and none of these holds the
/// player back from what is on the device.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Do {
    /// Send this one request, once, and answer with [`Happened::Answered`] or
    /// [`Happened::Failed`] under its id.
    Ask(Ask),
    /// The feature's whole transport answer is usable only after this effect is reached.
    AppResult { request: u64, result: AppResult },
    /// Keep this where it says, whole, replacing what was there, in one step.
    Keep(Keep),
    /// Forget this.
    Forget(Forget),
    /// Show this: the whole of what this part of the app shows, each time it changes.
    Show(Shown),
    /// Hand the player this.
    Hand(Hand),
    /// Read this from the journal or the Diagnostics record, or change this in the journal
    /// (13, 14). **Never on a path that plays, and never by the journal's writer waiting.**
    Journal(JournalDo),
}

/// What the journal's reader reads, and what the journal changes, for the sending half.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalDo {
    /// Read the events of this account and life that are still waiting (not taken, not
    /// refused) with a sequence of at least `from_sequence`, in sequence order, at most
    /// `max_events` of them and `max_bytes` of their lines; answer with
    /// [`Happened::EventsRead`] under this `read`. **The reader, never the writer, and every
    /// read is answered**, with no events when there are none.
    ReadEvents { read: u64, account: JournalAccount, life: u64, from_sequence: u64, max_events: usize, max_bytes: usize },
    /// Make the Diagnostics report's text ([`diag::Report::text`], exactly what "What is
    /// sent" shows) from the record's own file, taking no lock the player takes (14.4);
    /// answer with [`Happened::DiagnosticsRead`] under this `read`.
    ReadDiagnostics { read: u64 },
    /// The backend has these: mark them taken. The file is kept [`events::KEEP_MS`] after
    /// its last is taken, then removed ([`events::remove_files`]).
    Taken { account: JournalAccount, life: u64, ids: Vec<EventId>, at_ms: u64 },
    /// The backend refused these for good: never send them again, and count them in
    /// [`events::LossCounts::refused`].
    Refused { account: JournalAccount, life: u64, ids: Vec<EventId> },
    /// The account's life is now `life` (its person deleted their listening data, 12.5):
    /// write under it from now, and remove unsent what was written under an earlier life,
    /// counting none of it as lost. Always after the [`Keep::Saved`] that carries `life`.
    LifeMoved { account: JournalAccount, life: u64 },
    /// The backend's data was put back from a copy (13.3): every event still held for this
    /// backend, every account's, taken ones too, waits to be sent again (a refused one
    /// stays refused). Each goes under its own account, when that account is signed in.
    ResendAll {
        #[serde(serialize_with = "hex_id")]
        backend_id: BackendId,
    },
}

/// A backend id as its 32 lower-case hex digits.
fn hex_id<S: serde::Serializer>(id: &BackendId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&id.to_hex())
}

/// One request to the backend.
///
/// The app sends it **exactly once**, to exactly this address (checked at dispatch against
/// what is about to be sent), with exactly these headers and body; **follows no redirect**
/// (a 3xx is handed back as an answer); reads the body whole, up to `max_body` bytes, and no
/// further (larger is [`How::Cut`]); and gives up after `within_ms`.
///
/// **THE APP CALLS [`Client::waits_for`] IMMEDIATELY BEFORE IT SENDS AN ASK, ON THE SAME
/// THREAD AS EVERY OTHER CALL, AND DROPS THE ASK UNSENT IF THE ANSWER IS NO.** `Offline(true)`
/// and `stop` make it no for every ask but a sign-out being told; so do a sign-out and a
/// new sign-in for what was asked before them. That is how nothing queued goes out after
/// Offline was switched on, or with a former account's credentials.
#[derive(Debug, Serialize)]
pub struct Ask {
    pub id: u64,
    /// The generation it was asked under ([`Client::generation`]).
    pub generation: u64,
    pub method: Method,
    /// The Nexus origin the person typed, then one of `proto`'s and `account`'s paths.
    pub url: String,
    /// The `Authorization` header's value, when there is one: the session, as
    /// `Bearer …`. Never anything else, and never in the address.
    pub authorization: Option<Hidden>,
    /// The JSON body, sent as `application/json`.
    pub body: Option<Hidden>,
    pub within_ms: u64,
    pub max_body: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_request: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Method {
    Get,
    Post,
}

/// Text that may hold a secret (a session, a password). Its debug form shows nothing; it is
/// written out in full only where the app must send it.
pub struct Hidden(Zeroizing<String>);

impl Hidden {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Hidden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hidden(..)")
    }
}

impl Serialize for Hidden {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// What to keep, and where: each whole, in one step.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// Where secrets are kept, beside [`Saved`], replacing the one before. Forgotten only if
    /// what was kept of it does not read.
    DeviceSecret(DeviceKey),
    /// Where secrets are kept, as one thing, replacing the one before.
    Saved(Saved),
    /// Where secrets are kept, by its backend and the session it ends ([`Note::key`]),
    /// beside any other.
    Note(Note),
    /// Beside the device's settings, per device: when Diagnostics were sent (14.4).
    DiagSchedule(DiagSchedule),
}

/// What to forget: from where it was kept, and from memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Forget {
    /// Everything signing in left, the music login **and the player's copy of it**
    /// included. What is on the device (downloads, kept copies) is not touched and goes on
    /// playing.
    Saved,
    /// The note for this backend's session.
    Note { backend_id: String, session: u64 },
    /// The device's secret: only when what was kept of it did not read. A new one is made
    /// when it is next needed.
    DeviceSecret,
    /// A note that did not read, by its place in the `notes` handed to `start`.
    UnreadableNote { index: usize },
    /// The Diagnostics schedule, when what was kept of it did not read: a new one is kept.
    DiagSchedule,
    /// The person's Diagnostics choice, when what was kept of it did not read: taken as no
    /// choice made.
    Diagnostics,
}

/// What the player is handed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hand {
    /// Use this music login from now on, in place of any other, in one step; and try once
    /// more, under it, what waited. Reports about it ([`Happened::MusicRefused`],
    /// [`Happened::MusicLooked`]) carry this `generation` back.
    Login { login: MusicLogin, generation: u64 },
    /// The login of this number is set aside: **begin no new request with it**, anywhere
    /// (a stream already open goes on). What is on the device plays as usual and the car
    /// shows it; covers go blank, downloads wait. A login of another number is not touched.
    Refused { count: u64, generation: u64 },
    /// There is no music login now (a different account was signed in with none set, or
    /// none usable): drop the one the player has and begin no request to the music server.
    /// What is on the device plays as usual.
    NoLogin { generation: u64 },
}

// ---------------------------------------------------------------------------------------
// What a person sees
// ---------------------------------------------------------------------------------------

/// Everything the backend's part of the app shows, as the core decides it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Shown {
    pub screen: Screen,
    pub sign_in: SignInShown,
    /// The one row under Settings (17): "Nexus server: connected", and the rest.
    pub row: String,
    /// Who is signed in, when someone is: "Ada (ada@example.org)".
    pub who: Option<String>,
    /// A sentence about the music login, when there is something to say (6.3, 9.4).
    pub music: Option<String>,
    /// The car's short line while a login is set aside (6.3): "Online music is unavailable
    /// until your sign-in is renewed".
    pub car: Option<String>,
    /// What this server records (13.6), to be shown until seen.
    pub notice: Option<String>,
    pub devices: DevicesShown,
    pub password: PasswordShown,
    /// The phone is to show its screen that explains and asks for the local-network
    /// permission, for the music server's address (6.4). Never set on a PC.
    pub music_ask_permission: bool,
    /// The same, for the backend's address, whatever asked and whatever screen is up: a
    /// session's call, a renewal or a note failed before a connection opened, with the
    /// address inside and the permission not held (6.4). Never set on a PC.
    pub ask_permission: bool,
    /// Whether a music login is held, so that the player can reach the music server.
    pub plays: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    /// The sign-in form. (Music that is held still plays: see [`Shown::plays`].)
    SignIn,
    /// Signed in.
    SignedIn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignInShown {
    pub state: SignInState,
    /// The sentence for it; empty for none.
    pub words: String,
    /// The address and the email to put back in the form.
    pub address: String,
    pub email: String,
    /// The phone is to show its screen that explains and asks for the local-network
    /// permission (6.4). Never set on a PC.
    pub ask_permission: bool,
    /// The backend said "busy, come back in this many seconds" (8.3): the exact seconds,
    /// each time it says so. **The form disables its button and counts these down; the
    /// person presses again.** Nothing tries again by itself.
    pub wait_s: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInState {
    Idle,
    Working,
    WrongLogin,
    NeedsHttps,
    BadAddress,
    BadEmail,
    NoPassword,
    NoNexusServer,
    CantReach,
    NotSecure,
    NeedsPermission,
    AccountOff,
    Busy,
    AppTooOld,
    ServerTooOld,
    Offline,
    NoRandom,
    PasswordsDiffer,
    PasswordNotAllowed,
    /// Signed out on this device.
    SignedOut,
    /// The backend said this session has ended: forced out, the password changed
    /// elsewhere, the account switched off, 90 days unused.
    ForcedOut,
    /// The backend's data was put back from a copy (12.4).
    StartAgain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DevicesShown {
    pub state: ListState,
    pub rows: Vec<DeviceRow>,
    pub words: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ListState {
    NotAsked,
    Working,
    Listed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceRow {
    pub id: u64,
    pub name: String,
    pub kind: DeviceKind,
    pub this_one: bool,
    /// The session signed in on it, which forcing it out ends (8.7); `None` when it is
    /// signed out.
    pub session: Option<u64>,
    /// "This device", "Used yesterday", "Signed out".
    pub seen: String,
    pub can_force_out: bool,
    /// Force out was pressed on it and is under way.
    pub working: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PasswordShown {
    pub state: PasswordState,
    pub words: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PasswordState {
    Idle,
    Working,
    Changed,
    WrongCurrent,
    NotAllowed,
    Differ,
    Busy,
    CantReach,
}

// ---------------------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------------------

/// One device's dealings with the backend. Made with [`Client::default`], driven by
/// [`Client::start`], [`Client::happened`] and [`Client::stop`].
#[derive(Default)]
pub struct Client {
    /// Moved by every start, stop, sign-in and ending: the fence.
    generation: u64,
    next_ask: u64,
    live: Option<Live>,
    /// The last thing shown, so that `Show` is given only when it changes.
    shown: Option<Shown>,
    /// Sign-outs being told when the account changed or Offline went on: still waited for
    /// (the one exception to the fence), and their answers dropped.
    parting: Vec<Parting>,
    /// The backend id last seen at each origin this run, from every `hello` and every
    /// stamped answer (7.1, as ruled: the one value). Nothing that carries a session, a
    /// device secret or a note goes to an origin unless it names the backend expected; a
    /// sign-out's ask is no longer waited for once its origin names another.
    seen_at: Vec<(String, BackendId)>,
    /// What each origin's `hello` said Diagnostics start as (14.3), this run: read at a
    /// start's `hello` and a sign-in's, and lasting across `stop` as `seen_at` does.
    defaults_at: Vec<(String, Option<bool>)>,
    /// The number of the last read asked of the journal or the Diagnostics record.
    next_read: u64,
}

/// A sign-out told across the fence, with where it goes and for which backend.
struct Parting {
    ask: u64,
    origin: String,
    backend: BackendId,
    session: u64,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client").field("generation", &self.generation).field("live", &self.live.is_some()).finish()
    }
}

/// Everything between `start` and `stop`.
struct Live {
    app: App,
    offline: bool,
    device_secret: Option<DeviceKey>,
    saved: Option<Saved>,
    notes: Vec<Note>,
    flight: Vec<Flight>,
    form: Form,
    reach: Reach,
    /// When the waiting sign-outs were last told.
    told_ms: Option<u64>,
    /// The generation the account now signed in was published under: what [`Hand`] carries,
    /// and what reports about the login must carry back.
    account_generation: u64,
    /// The backend's address led inside and the permission is not held (6.4).
    backend_blocked: bool,
    /// The session's check is due: at a start, the network coming back, Offline off. Never
    /// from a tick.
    check_due: bool,
    /// An answer came after the backend could not be reached: what was owed is asked once
    /// that answer's own part is done.
    restored: bool,
    renewal: Renewal,
    devices: Devices,
    password: PasswordShown,
    music_words: Option<String>,
    /// The handed music server's address leads inside and the permission is not held.
    music_blocked: bool,
    /// The music login's number this device knows of: the one held, or the one an answer
    /// carried when it said there was none to hand. A receipt above it means a new one.
    music_seen: u64,
    /// The backend at this origin speaks no number this app does: nothing more is asked of
    /// it until a start or a sign-in elsewhere.
    too_old: Option<String>,
    /// Once signed in, a backend of another id answered at the Nexus origin: nothing more is
    /// sent to it (7.1), and the person is asked to sign in again.
    stranger: bool,
    /// The sending half's events (13.3).
    sending: sending::Sending,
    /// Diagnostics (14).
    diagnostics: sending::Diagnostics,
}

struct Flight {
    id: u64,
    what: Purpose,
    /// The most its answer may be: a larger one is taken as cut.
    max_body: usize,
}

enum Purpose {
    Hello(Pending),
    /// A try at signing in; its ask's id is the try's number. `secret`: the device secret
    /// it carries, kept for this backend if it succeeds.
    SignIn { origin: NexusOrigin, backend_id: BackendId, secret: DeviceSecret },
    /// `hello` at an origin whose backend is expected, before anything secret goes there.
    Verify { origin: String },
    /// The devices list, page by page. `opened`: the person is looking at it; otherwise it
    /// is the check on the session made at a start, the network coming back or Offline off.
    Devices { page: usize, opened: bool },
    /// A renewal of the music login, for this number: `refused`, because the music server
    /// refused the login of that number; otherwise because an answer said the number rose.
    Music { count: u64, refused: bool },
    /// Forcing out this device's session.
    ForceOut { device: u64, session: u64 },
    ChangePassword,
    /// Telling a backend of a sign-out, from the note for its session, with no session (11).
    Tell { backend: BackendId, session: u64, origin: String },
    /// A batch of the journal's events, for this account (13.3).
    Events { account: JournalAccount, batch: events::Batch },
    /// The Diagnostics report (14.4).
    Diagnostics,
    App { request: u64, endpoint: app_calls::Endpoint, life: u64 },
}

impl Purpose {
    fn same_kind(&self, other: &Purpose) -> bool {
        match (self, other) {
            (Purpose::Hello(_) | Purpose::SignIn { .. }, Purpose::Hello(_) | Purpose::SignIn { .. }) => true,
            (Purpose::Tell { backend, session, .. }, Purpose::Tell { backend: other_backend, session: other, .. }) => {
                session == other && backend == other_backend
            }
            (Purpose::Verify { origin }, Purpose::Verify { origin: other }) => origin == other,
            (Purpose::App { request, .. }, Purpose::App { request: other, .. }) => request == other,
            (left, right) => std::mem::discriminant(left) == std::mem::discriminant(right),
        }
    }

    /// A sign-out being told: the one ask the fence does not reach.
    fn parting(&self) -> bool {
        matches!(self, Purpose::Tell { .. })
    }
}

/// What a sign-in press holds while `hello` is asked.
struct Pending {
    origin: NexusOrigin,
    credential: Pressed,
}

enum Pressed {
    Password { email: Email, password: Password },
    /// A code and the password chosen, redeemed together in one call (8.5).
    Code { code: OneTimeCode, new_password: Password },
}

struct Form {
    state: SignInState,
    words: String,
    /// The exact seconds of the last "come back in", while that is what the form says.
    wait_s: Option<u64>,
    address: String,
    email: String,
    ask_permission: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    Unknown,
    Up,
    Down { since_ms: u64 },
}

/// The one rule for a refused music login (6.3, as ruled): one renewal for each number.
#[derive(Default)]
struct Renewal {
    /// The number of a login the music server refused: set aside until a newer one is held.
    refused: Option<u64>,
    /// The refused number the backend has answered a renewal for: not asked again, but for
    /// the network coming back.
    refreshed: Option<u64>,
    /// The risen number the backend has answered a renewal for: never asked again.
    rose: Option<u64>,
    /// A renewal that went unanswered, to be asked again when the backend may be reachable
    /// (never on a timer): its number, and whether it was for a refusal.
    owed: Option<(u64, bool)>,
    /// The renewal was owed by the answer being dealt with now: that answer's own settling
    /// does not ask it again (it is asked at the next reachability event).
    owed_now: bool,
    /// The highest number an answer announced while a renewal was under way: asked for
    /// once that renewal's outcome is in, if it is still above what is held.
    announced: Option<u64>,
}

struct Devices {
    state: ListState,
    rows: Vec<DeviceRow>,
    words: String,
    /// Rows read so far, while more pages are being read.
    reading: Vec<DeviceRow>,
}

/// What an answer to a call came to (7.1, 12.1): done, refused (final), or nothing.
enum Outcome<T> {
    Done(T, Seen),
    Refused(Why, Option<Seen>),
    Nothing,
}

/// The numbers a receipt carries (12.3).
#[derive(Clone, Copy)]
struct Seen {
    backend: BackendId,
    account: AccountId,
    restore: u64,
    life: u64,
    change: u64,
    music_count: u64,
}

impl Seen {
    fn of(receipt: &Receipt) -> Seen {
        Seen {
            backend: receipt.stamp.backend,
            account: receipt.stamp.account,
            restore: receipt.stamp.restore,
            life: receipt.stamp.life,
            change: receipt.stamp.change,
            music_count: receipt.music_login_count,
        }
    }
}

/// A refusal, as this side acts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// "This session has ended, sign in again" (8.7).
    SessionEnded,
    /// "The server's data was put back, sign in again" (12.3).
    DataPutBack,
    Denied,
    WrongLogin,
    AccountOff,
    NoSuchDevice,
    PasswordNotAllowed,
    Incompatible { low: u32, high: u32 },
    /// "Busy, come back", in this many seconds if it said.
    Busy { seconds: Option<u64> },
    Malformed,
    /// "Not dealt with": nothing was done, and it may be asked again.
    NotDealtWith,
}

impl Why {
    fn of(refusal: &Refusal) -> Why {
        match refusal {
            Refusal::Malformed => Why::Malformed,
            Refusal::NotSignedIn(NotSignedIn::SessionEnded) => Why::SessionEnded,
            Refusal::NotSignedIn(NotSignedIn::DataPutBack) => Why::DataPutBack,
            Refusal::NotAllowed(NotAllowed::Denied) => Why::Denied,
            Refusal::NotAllowed(NotAllowed::WrongLogin) => Why::WrongLogin,
            Refusal::NotAllowed(NotAllowed::AccountOff) => Why::AccountOff,
            Refusal::NotAllowed(NotAllowed::NoSuchDevice) => Why::NoSuchDevice,
            Refusal::NotAllowed(NotAllowed::PasswordNotAllowed) => Why::PasswordNotAllowed,
            Refusal::NotAllowed(NotAllowed::Incompatible { speaks }) => Why::Incompatible { low: speaks.low(), high: speaks.high() },
            Refusal::Busy { retry_after_seconds } => Why::Busy { seconds: retry_after_seconds.map(account::Wait::seconds) },
            Refusal::NotDealtWith => Why::NotDealtWith,
        }
    }
}

fn outcome<T>(read: Result<Reply<T>, account::BadMessage>) -> Outcome<T> {
    match read {
        Ok(Reply::Done { result, receipt }) => Outcome::Done(result, Seen::of(&receipt)),
        Ok(Reply::Refused(refusal)) => Outcome::Refused(Why::of(&refusal), None),
        Ok(Reply::RefusedWithReceipt(refused)) => Outcome::Refused(Why::of(refused.reason()), Some(Seen::of(refused.receipt()))),
        Err(_) => Outcome::Nothing,
    }
}

/// A copy of a music login (account's type is not `Clone`, so that copies are made only
/// where meant: one to keep, one for the player).
fn copy(login: &MusicLogin) -> MusicLogin {
    MusicLogin { address: login.address.clone(), name: login.name.clone(), salt: login.salt.clone(), token: login.token.clone(), count: login.count }
}

/// A copy of a sign-out note (account's type is not `Clone`).
fn copy_note(note: &SignOutNote) -> SignOutNote {
    SignOutNote { session: note.session, secret: note.secret.clone() }
}

impl Note {
    fn copy(&self) -> Note {
        Note {
            request: self.request,
            revoke: copy_note(&self.revoke),
            nexus: self.nexus.clone(),
            backend_id: self.backend_id.clone(),
            restore: self.restore,
            made_ms: self.made_ms,
        }
    }
}

impl Saved {
    /// A copy, to be kept whole.
    fn copy(&self) -> Saved {
        Saved {
            nexus: self.nexus.clone(),
            backend_id: self.backend_id.clone(),
            account: self.account,
            email: self.email.clone(),
            name: self.name.clone(),
            device: self.device,
            restore: self.restore,
            life: self.life,
            change: self.change,
            notice: self.notice.clone(),
            session: self.session.clone(),
            sign_out: self.sign_out.as_ref().map(copy_note),
            music: self.music.as_ref().map(copy),
        }
    }
}

/// An address in its one spelling: account's [`NexusOrigin`] as it writes itself, so that
/// `https://name/` and `https://name` are one address (as ruled). Text that is not an
/// address is left as it is (it matches nothing that is).
fn canonical(text: &str) -> String {
    NexusOrigin::typed(text).map(|origin| origin.origin().to_string()).unwrap_or_else(|_| text.to_owned())
}

/// The address a person typed, as the Nexus origin every ask is made to (7.1): read by
/// account's [`NexusOrigin::typed`], on origin's one reading of an address. `http://` (or any
/// scheme but https) is "needs HTTPS"; anything else it refuses is "not an address".
fn typed_origin(text: &str) -> Result<NexusOrigin, SignInState> {
    let text = text.trim();
    if text.is_empty() {
        return Err(SignInState::BadAddress);
    }
    NexusOrigin::typed(text).map_err(|wrong| match wrong {
        crate::origin::AddressError::Scheme => SignInState::NeedsHttps,
        _ => SignInState::BadAddress,
    })
}

/// Whether what this device kept under the person it was signed in to before this version
/// (downloads, settings, the queue) becomes the person's who signs in now (11): only if
/// the music login handed over names the same music server and the same user. Asked once,
/// at the first sign-in of this version; afterwards the folder is the person's by
/// [`Saved::backend_id`] and [`Saved::account`], and another account handed the same user
/// later starts with nothing.
///
/// The address is compared as the music server's base: scheme and host in small letters,
/// the port as written, the path exactly, a single trailing `/` not counting. The user
/// exactly.
// to be origin's: the one tidying of a music server's address is `origin::MusicBase`; until
// it is used here, this is the narrowest reading that can only say "the same" of the same.
pub fn carries_over(before_address: &str, before_user: &str, handed: &MusicLogin) -> bool {
    fn base(address: &str) -> Option<String> {
        let address = address.trim();
        let (scheme, rest) = address.split_once("://")?;
        let end = rest.find('/').unwrap_or(rest.len());
        let (authority, path) = rest.split_at(end);
        if authority.is_empty() || authority.contains('@') || rest.contains(['?', '#']) {
            return None;
        }
        let path = path.strip_suffix('/').unwrap_or(path);
        Some(format!("{}://{}{}", scheme.to_ascii_lowercase(), authority.to_ascii_lowercase(), path))
    }
    match (base(before_address), base(handed.address.as_str())) {
        (Some(before), Some(now)) => before == now && before_user == handed.name.as_str(),
        _ => false,
    }
}

impl Client {
    /// The generation now: every [`Ask`] carries the one it was made under. An answer to an
    /// ask of an earlier generation is thrown away.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the machine still waits for the answer to this ask. **The app asks this
    /// immediately before it sends each [`Ask`] and drops the ask unsent if it is no**: after
    /// `Offline(true)` and `stop` it is no for everything but a sign-out being told, and for
    /// that it is no once its origin has been seen to name another backend.
    pub fn waits_for(&self, ask: u64) -> bool {
        if let Some(parting) = self.parting.iter().find(|parting| parting.ask == ask) {
            return self.seen(&parting.origin) == Some(parting.backend);
        }
        let Some(flight) = self.live.as_ref().and_then(|live| live.flight.iter().find(|flight| flight.id == ask)) else { return false };
        match &flight.what {
            Purpose::Tell { backend, origin, .. } => self.seen(origin) == Some(*backend),
            _ => true,
        }
    }

    /// The backend id last seen at this origin this run.
    fn seen(&self, origin: &str) -> Option<BackendId> {
        let origin = canonical(origin);
        self.seen_at.iter().find(|(at, _)| *at == origin).map(|(_, id)| *id)
    }

    /// Records the backend id an answer at this origin named.
    fn saw(&mut self, origin: &str, id: BackendId) {
        let origin = canonical(origin);
        self.seen_at.retain(|(at, _)| *at != origin);
        self.seen_at.push((origin, id));
    }

    /// **Start**, after the app is already usable: hand over what was kept. A kept music
    /// login is handed to the player first, before anything else, unless what was kept says
    /// it was signed out. Starting again without a stop stops first.
    pub fn start(&mut self, app: App, kept: Kept, given: &Given) -> Vec<Do> {
        self.stop();
        self.generation = self.generation.wrapping_add(1);
        let mut out = Vec::new();
        let Kept { device_secret, mut saved, mut notes, offline, unreadable, diagnostics, diag_schedule } = kept;
        // Every address in its one spelling from here on (as ruled).
        if let Some(found) = saved.as_mut() {
            found.nexus = canonical(&found.nexus);
        }
        for note in &mut notes {
            note.nexus = canonical(&note.nexus);
        }
        // A sign-out cut between keeping its note and forgetting what signing in left: the
        // note says it, and it is finished now. That login is not handed over.
        if let Some(found) = &saved
            && let Some(revoke) = &found.sign_out
            && notes.iter().any(|note| note.backend_id == found.backend_id && note.revoke.session == revoke.session)
        {
            out.push(Do::Forget(Forget::Saved));
            saved = None;
        }
        // R1: the player first, whatever else is wrong with what was kept.
        if let Some(login) = saved.as_ref().and_then(|saved| saved.music.as_ref()) {
            out.push(Do::Hand(Hand::Login { login: copy(login), generation: self.generation }));
        }
        // What did not read is forgotten where it is kept, after the player has its login.
        out.extend(unreadable.into_iter().map(Do::Forget));
        if let Some(found) = &mut saved
            && (NexusOrigin::typed(&found.nexus).is_err() || BackendId::from_hex(&found.backend_id).is_none())
        {
            // A session with no backend to say where it is from cannot be used; the music
            // login still plays.
            if found.session.take().is_some() {
                found.sign_out = None;
                found.notice = None;
                out.push(Do::Keep(Keep::Saved(found.copy())));
            }
        }
        // A note that cannot be told, is too old to matter, or is past the four newest, is
        // forgotten where it is kept too, not only left out of memory.
        let mut notes = notes;
        notes.sort_by(|left, right| right.made_ms.cmp(&left.made_ms));
        let mut kept_notes: Vec<Note> = Vec::new();
        for note in notes {
            let usable = NexusOrigin::typed(&note.nexus).is_ok()
                && BackendId::from_hex(&note.backend_id).is_some()
                && note.request != 0
                && given.now_ms.checked_sub(note.made_ms).is_none_or(|age| age < NOTE_LIFE_MS)
                && !kept_notes.iter().any(|kept| kept.key() == note.key());
            if usable && kept_notes.len() < NOTES_MAX {
                kept_notes.push(note);
            } else {
                out.push(Do::Forget(note.forget()));
            }
        }
        let mut form = Form::idle();
        if let Some(found) = &saved {
            form.email = found.email.clone();
            form.address = found.nexus.clone();
            if found.session.is_none() {
                form.state = SignInState::StartAgain;
            }
        }
        let music_seen = saved.as_ref().and_then(|saved| saved.music.as_ref()).map_or(0, |login| login.count);
        let diagnostics = sending::Diagnostics::from_kept(diagnostics, diag_schedule, given.now_ms, &mut out);
        let mut live = Live {
            app,
            offline,
            device_secret,
            saved,
            notes: kept_notes,
            flight: Vec::new(),
            form,
            reach: Reach::Unknown,
            told_ms: None,
            account_generation: self.generation,
            backend_blocked: false,
            check_due: false,
            restored: false,
            renewal: Renewal::default(),
            devices: Devices::not_asked(),
            password: PasswordShown::idle(),
            music_words: None,
            music_blocked: false,
            music_seen,
            too_old: None,
            stranger: false,
            sending: sending::Sending::default(),
            diagnostics,
        };
        if live.form.state != SignInState::Idle {
            live.form.words = words::sign_in(live.form.state, live.app.kind, 0);
        }
        if live.device_secret.is_none()
            && let Some(random) = &given.random
        {
            // Made before any sign-in: for whichever backend is signed in to first.
            let key = DeviceKey { secret: DeviceSecret::from_bytes(*random.0), backend_id: None };
            out.push(Do::Keep(Keep::DeviceSecret(key.clone())));
            live.device_secret = Some(key);
        }
        self.live = Some(live);
        self.after_reach(given, &mut out, Reached::Start);
        self.show(given, &mut out);
        out
    }

    /// **Stop**, at the moment the account changes: nothing asked before is acted on or sent
    /// (but a sign-out being told, to the backend it is for), and nothing more is done until
    /// the next `start`.
    pub fn stop(&mut self) {
        if let Some(live) = self.live.take() {
            self.generation = self.generation.wrapping_add(1);
            for flight in live.flight {
                if let Purpose::Tell { backend, origin, session } = flight.what {
                    self.parting.push(Parting { ask: flight.id, origin, backend, session });
                }
            }
            let over = self.parting.len().saturating_sub(NOTES_MAX);
            self.parting.drain(..over);
        }
        self.shown = None;
    }

    /// **Happened**: tell the machine, and be told what to do. Before `start` and after
    /// `stop`, nothing.
    pub fn happened(&mut self, event: Happened, given: &Given) -> Vec<Do> {
        let mut out = Vec::new();
        if let Happened::Answered { ask, .. } | Happened::Failed { ask, .. } = &event {
            // A sign-out told across the fence: its answer is dropped (the note it came from
            // is told again from what is kept).
            if let Some(index) = self.parting.iter().position(|parting| parting.ask == *ask) {
                self.parting.remove(index);
                return out;
            }
        }
        if self.live.is_none() {
            if let Happened::AppPost { request, .. } = event {
                out.push(Do::AppResult { request, result: AppResult::Stopped });
            }
            return out;
        }
        match event {
            Happened::AppPost { request, scope, path, body } => {
                let result = self.post_for_app(request, scope, &path, body, &mut out);
                if result != AppResult::Checked {
                    out.push(Do::AppResult { request, result });
                }
            }
            Happened::AppCancelled { request } => {
                if let Some(live) = self.live.as_mut() {
                    live.flight.retain(|flight| !matches!(flight.what, Purpose::App { request: pending, .. } if pending == request));
                }
            }
            Happened::Pressed(press) => self.pressed(press, given, &mut out),
            Happened::Answered { ask, status, body } => self.answered(ask, status, &body, given, &mut out),
            Happened::Failed { ask, failure } => self.failed(ask, failure, given, &mut out),
            Happened::Network { up } => {
                if up {
                    self.after_reach(given, &mut out, Reached::Network);
                }
            }
            Happened::Offline(on) => self.offline(on, given, &mut out),
            Happened::Tick => self.tick(given, &mut out),
            Happened::MusicRefused { count, generation } => self.music_refused(count, generation, &mut out),
            Happened::MusicLooked { look, count, generation } => {
                if let Some(live) = self.live.as_mut()
                    && generation == live.account_generation
                    && live.held_count() == Some(count)
                {
                    live.music_blocked = look.blocked() && live.app.kind == DeviceKind::Phone;
                }
            }
            Happened::EventsWaiting { account, life, pending, from_sequence } => self.events_waiting(account, life, pending, from_sequence, given, &mut out),
            Happened::EventsRead { read, account, life, events } => self.events_read(read, account, life, events, &mut out),
            Happened::DiagnosticsRead { read, text } => self.diagnostics_read(read, text, given, &mut out),
            Happened::DiagnosticsSwitch(choice) => self.diagnostics_switch(choice),
            Happened::DiagnosticsFault { at_ms } => self.diagnostics_fault(at_ms, &mut out),
            Happened::InUse(on) => self.in_use(on),
        }
        self.show(given, &mut out);
        out
    }

    /// The most the answer to this ask may be, while it is waited for: what the binding
    /// checks an answer against before handing it in.
    pub fn max_body_for(&self, ask: u64) -> Option<usize> {
        self.live.as_ref()?.flight.iter().find(|flight| flight.id == ask).map(|flight| flight.max_body)
    }
}

/// Why the backend may be reachable now.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reached {
    Start,
    /// The network came back: the one thing besides a moved number that renews a login
    /// the backend had nothing newer for (6.3).
    Network,
    OfflineOff,
    /// An answer came after the backend could not be reached.
    Answer,
}

impl Form {
    fn idle() -> Form {
        Form { state: SignInState::Idle, words: String::new(), wait_s: None, address: String::new(), email: String::new(), ask_permission: false }
    }
}

impl Devices {
    fn not_asked() -> Devices {
        Devices { state: ListState::NotAsked, rows: Vec::new(), words: String::new(), reading: Vec::new() }
    }
}

impl PasswordShown {
    fn idle() -> PasswordShown {
        PasswordShown { state: PasswordState::Idle, words: String::new() }
    }
}
