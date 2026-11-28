//! One device's dealings with the Nexus server, as state and things to do (design 5.4, 5.5,
//! section 6 and 22.3: slice C10). **No file is opened here**, and nothing here has a
//! socket, a clock, a thread or a source of randomness: the app hands in the time and
//! random bytes with each call, and is told what to do.
//!
//! **Each app drives it by the same four calls, in the same order** (5.5):
//!
//! 1. [`Client::start`], once the app is already usable (6.1), with what was kept on the
//!    device ([`Kept`]). **A music login that was kept is handed to the player first,
//!    before anything is asked** (R1, 6.1): playing never waits on this.
//! 2. [`Client::happened`], whenever something does ([`Happened`]): a press, an answer, a
//!    failure, the network, Offline, a tick, the music server refusing the login.
//! 3. For each thing given back, **do** it. Each is one of a closed set ([`Do`]): **ask**
//!    the backend this, **keep** this, **forget** this, **show** this, **hand** the player
//!    this music login. Nothing else.
//! 4. [`Client::stop`], at the moment the account changes: nothing asked for the old one is
//!    acted on afterwards (ruled: the fence on an account switch).
//!
//! The phone reaches the same four calls through one entry point taking and giving JSON,
//! [`Client::call_json`], as it reaches the rest of this library.
//!
//! **The rules of section 6 are held here, so that neither app can get one wrong:**
//!
//! - **Playing never waits.** There is no "hold the player" among the things to do. A kept
//!   music login is handed over at `start` before any ask. A backend that is down, stalled
//!   or saying "not signed in" never takes the login away, except when the backend says
//!   this device was signed out (6.3: "it is meant to stop") or the person signs out here.
//! - **A refused music login (6.3, as ruled):** the requests made with it stop at once
//!   ([`Hand::Refused`]), what is on the device plays, and **one** refresh is asked for that
//!   login's count, in the background. A refresh that is answered is never asked again for
//!   the same count. One that went unanswered is asked again only when something says the
//!   backend may be reachable now (the network back, Offline off, another answer arriving),
//!   never on a timer and never in a loop. A new login, when it comes, is handed over at once,
//!   so the car is never left on what is on the phone.
//! - **The session goes to the backend alone, the music login to the player alone.** Every
//!   [`Ask`] is to the address the person typed, under `/nexus/api/`; the music login is in
//!   no ask.
//! - **The backend's address is the one a person typed**, never one made from the music
//!   server's address (as ruled).
//! - **A redirect is never followed**, and is never an answer of the backend's.
//! - **One ask is one request**, and an answer is taken once.
//! - **An answer for an ask no longer waited for is thrown away**: after `stop`, after a
//!   sign-out or a sign-in, after Offline was switched on.
//! - **Offline means nothing is asked.**
//! - **Signing out is lasting on the device at once**, even with no network: the session is
//!   forgotten, and the backend is told later from a separate thing kept ([`ToTell`]) (as
//!   ruled). Being signed out by the backend forgets the session and the music login and
//!   keeps everything a person keeps (downloads, settings: none of which this touches).
//! - **"Start again"** (12.3, 12.4): the session is forgotten and the person signs in again;
//!   the music login is kept, and plays.
//!
//! **What a person sees is decided here too** ([`Shown`]), in words this module owns, so the
//! two apps say the same.
//!
//! The half that sends what was listened to (C8) and Diagnostics (C6) is a seam here: see
//! [`Client::happened`] on [`Happened::Tick`]. Its tests are marked as owed.

use super::account::{
    self, ChangePassword, Credential, DeviceId, DeviceKind, DevicePage, DeviceSecret, Email, ForceOut, ForcedOut, GetMusicLogin,
    ListDevices, MusicLogin, Name, OneTimeCode, Password, PasswordChanged, Receipt, Refusal, Reply, RequestHead, Session, SignIn,
    SignInAnswer, SignOut, SignedOut,
};
use super::proto::{self, BackendId, Found, SPEAKS, Speaks, Version};
use serde::Serialize;
use std::fmt;
use zeroize::Zeroizing;

mod json;
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
/// How long signing in may take: the backend works passwords out two at a time with eight
/// waiting (8.2), each a fraction of a second.
// the design does not say. Chosen to cover a full queue on a slow machine.
pub const SIGN_IN_WITHIN_MS: u64 = 30_000;
/// How long any other call may take.
// the design does not say.
pub const CALL_WITHIN_MS: u64 = 15_000;
/// How often a signed-in device asks the backend whether its session still stands, while
/// the app is running and Offline is off. The answer also carries the music login's count.
// the design does not say how often a device asks in step 1, when nothing is being sent.
// Chosen: rarely; every start, the network coming back and Offline going off ask too.
pub const CHECK_EVERY_MS: u64 = 6 * 60 * 60 * 1000;
/// How often the backend is told again of a sign-out it has not heard of yet, besides every
/// start, the network coming back and Offline going off.
// the design does not say.
pub const TELL_EVERY_MS: u64 = 60 * 60 * 1000;
/// How often the app is to call `happened` with [`Happened::Tick`]. A tick sooner or later
/// costs nothing: the machine keeps its own times.
pub const TICK_MS: u64 = 60 * 1000;
/// The most a `hello` answer may be (proto's own bound).
pub const HELLO_MAX_BYTES: usize = proto::HELLO_MAX_BYTES;
/// The most any other answer may be: account's bound for every body.
pub const ANSWER_MAX_BYTES: usize = account::BODY_MAX_BYTES;
/// What a backend must name in its `hello` for an app to sign in to it (design 18:
/// `"has":["accounts"]`).
// to be proto's: proto still names only stage 1's "plays".
pub const HAS_ACCOUNTS: &str = "accounts";
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

/// What was kept on the device, read back at `start`. Anything missing is `None`.
#[derive(Debug, Default)]
pub struct Kept {
    /// Where secrets are kept: the device's own secret (8.6).
    pub device_secret: Option<DeviceSecret>,
    /// Where secrets are kept: the session.
    pub session: Option<Session>,
    /// Where secrets are kept: the music login (9.4).
    pub music: Option<MusicLogin>,
    /// In a file of the app's: which backend, as whom, and the numbers last seen (11).
    pub account: Option<AccountFile>,
    /// Where secrets are kept: a sign-out the backend has not yet been told of.
    pub to_tell: Option<ToTell>,
    /// Whether Offline mode is on (the app's own setting).
    pub offline: bool,
}

/// What is kept in a file about the account signed in to (design 11). Nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct AccountFile {
    /// The address the person typed, as `https://host` or `https://host:port`.
    pub backend: String,
    /// The backend's id, from its `hello`: 32 lower-case hex digits.
    pub backend_id: String,
    pub email: String,
    pub name: String,
    /// This device's number at the backend.
    pub device: u64,
    /// The numbers from the latest answer's stamp (12.3).
    pub restore: u64,
    pub life: u64,
    pub change: u64,
    /// The music login's count as last seen (9.4).
    pub music_count: u64,
    /// What this server records (13.6), until the person has seen it.
    pub notice: Option<String>,
    /// Signed in with a one-time code: the session gives leave only to choose a password.
    pub password_setup: bool,
}

/// A sign-out the backend is still to be told of. Kept where secrets are kept, because it
/// holds the session that was ended here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToTell {
    pub session: Session,
    /// The address the person typed.
    pub backend: String,
    pub restore: u64,
}

/// Something that happened.
#[derive(Debug)]
pub enum Happened {
    /// The person pressed something.
    Pressed(Press),
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
    /// A tick: about every [`TICK_MS`].
    Tick,
    /// The music server refused the music login of this count as "wrong name or password"
    /// (Subsonic error 40). Only that: a server out of reach is not this.
    MusicRefused { count: u64 },
}

/// What the person pressed.
#[derive(Debug)]
pub enum Press {
    /// Sign in with an email and a password.
    SignIn { address: String, email: String, password: Typed },
    /// Sign in with the one-time code the admin gave (8.4). The code takes the place of both
    /// the email and the password.
    SignInWithCode { address: String, code: Typed },
    /// After a code: the password chosen, typed twice.
    ChoosePassword { password: Typed, again: Typed },
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
    /// A connection was opened and could not be made secure.
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
/// player back.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Do {
    /// Send this one request, once, and answer with [`Happened::Answered`] or
    /// [`Happened::Failed`] under its id.
    Ask(Ask),
    /// Keep this where it says, replacing what was there.
    Keep(Keep),
    /// Forget this.
    Forget(Forget),
    /// Show this: the whole of what this part of the app shows, each time it changes.
    Show(Shown),
    /// Hand the player this.
    Hand(Hand),
}

/// One request to the backend.
///
/// The app sends it **exactly once**, to exactly this address, with exactly these headers
/// and body; **follows no redirect** (a 3xx is handed back as an answer); reads the body
/// whole, up to `max_body` bytes, and no further (larger is [`How::Cut`]); and gives up
/// after `within_ms`. An ask is sent even if, by the time it goes, the account has changed:
/// only its answer is then thrown away. The app may cancel one the machine no longer waits
/// for ([`Client::waits_for`]).
#[derive(Debug, Serialize)]
pub struct Ask {
    pub id: u64,
    /// The generation it was asked under ([`Client::generation`]).
    pub generation: u64,
    pub method: Method,
    /// Always `https://` and the address the person typed, then a path under `/nexus/api/`.
    pub url: String,
    /// The `Authorization` header's value, when there is one: the session, as
    /// `Bearer …`. Never anything else, and never in the address.
    pub authorization: Option<Hidden>,
    /// The JSON body, sent as `application/json`.
    pub body: Option<Hidden>,
    pub within_ms: u64,
    pub max_body: usize,
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

/// What to keep, and where.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    /// Where secrets are kept. Made once; never forgotten.
    DeviceSecret(DeviceSecret),
    /// Where secrets are kept.
    Session(Session),
    /// Where secrets are kept.
    Music(MusicLogin),
    /// In the app's file for it.
    Account(AccountFile),
    /// Where secrets are kept.
    ToTell(ToTell),
}

/// What to forget: from where it was kept, and from memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Forget {
    Session,
    /// From where secrets are kept **and from the player**. What is on the device
    /// (downloads, kept copies) is not touched and goes on playing.
    Music,
    Account,
    ToTell,
}

/// What the player is handed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hand {
    /// Use this music login from now on.
    Login(MusicLogin),
    /// The login of this count was refused: send nothing more with it, now. What is on the
    /// device plays as usual, the car included; a new login is handed over as soon as there
    /// is one. (A login of another count is not touched.)
    Refused { count: u64 },
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
    /// A sentence about the music login, when there is something to say (6.3).
    pub music: Option<String>,
    /// What this server records (13.6), to be shown until seen.
    pub notice: Option<String>,
    pub devices: DevicesShown,
    pub password: PasswordShown,
    /// Whether a music login is held, so that the player can reach the music server.
    pub plays: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Screen {
    /// The sign-in form. (Music that is held still plays: see [`Shown::plays`].)
    SignIn,
    /// Signed in with a code: choose a password.
    ChoosePassword,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInState {
    Idle,
    Working,
    WrongLogin,
    TooManyTries,
    NeedsHttps,
    BadAddress,
    BadEmail,
    NoPassword,
    NoNexusServer,
    CantReach,
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
    /// Signed out by the backend: forced out, the password changed elsewhere, the account
    /// switched off.
    ForcedOut,
    /// The backend was put back from a copy (12.4).
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
    TooManyTries,
    CantReach,
}

// ---------------------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------------------

/// One device's dealings with the backend. Made with [`Client::default`], driven by
/// [`Client::start`], [`Client::happened`] and [`Client::stop`].
#[derive(Default)]
pub struct Client {
    /// Bumped by every start, stop, sign-in and ending: the fence.
    generation: u64,
    next_ask: u64,
    live: Option<Live>,
    /// The last thing shown, so that `Show` is given only when it changes.
    shown: Option<Shown>,
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
    device_secret: Option<DeviceSecret>,
    session: Option<Session>,
    account: Option<AccountFile>,
    music: Option<MusicLogin>,
    to_tell: Option<ToTell>,
    flight: Vec<Flight>,
    form: Form,
    reach: Reach,
    /// When the session was last asked about.
    checked_ms: Option<u64>,
    told_ms: Option<u64>,
    renewal: Renewal,
    devices: Devices,
    password: PasswordShown,
    music_words: Option<String>,
    /// The backend speaks no number this app does: nothing more is asked until a start.
    too_old: bool,
}

struct Flight {
    id: u64,
    what: Purpose,
}

enum Purpose {
    Hello(Pending),
    SignIn { origin: String, backend_id: BackendId, email_typed: String },
    ChoosePassword { new: Typed },
    /// The devices list, page by page. `opened`: the person is looking at it; otherwise it
    /// is the check on the session ([`CHECK_EVERY_MS`]).
    Devices { page: usize, opened: bool },
    /// A refresh of the music login, for this count: `refused`, because the music server
    /// refused the login of that count; otherwise because an answer said the count rose.
    Music { count: u64, refused: bool },
    ForceOut { device: u64 },
    ChangePassword,
    Tell,
}

impl Purpose {
    fn same_kind(&self, other: &Purpose) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

/// What a sign-in press holds while `hello` is asked.
struct Pending {
    origin: String,
    credential: Pressed,
    email_typed: String,
}

enum Pressed {
    Password { email: Email, password: Password },
    Code(OneTimeCode),
}

struct Form {
    state: SignInState,
    words: String,
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

/// The rule for a refused music login (6.3, as ruled): one refresh for each count.
#[derive(Default)]
struct Renewal {
    /// The count of a login the music server refused, until a newer one is held.
    refused: Option<u64>,
    /// The refused count the backend has answered a refresh for: never asked again.
    refreshed: Option<u64>,
    /// The risen count the backend has answered a refresh for: never asked again.
    rose: Option<u64>,
    /// A refresh that went unanswered, to be asked again when the backend may be
    /// reachable (never on a timer): its count, and whether it was for a refusal.
    owed: Option<(u64, bool)>,
}

struct Devices {
    state: ListState,
    rows: Vec<DeviceRow>,
    words: String,
    /// Rows read so far, while more pages are being read.
    reading: Vec<DeviceRow>,
}

/// What an answer to a session's call came to (12.1): done, refused (final), or nothing.
enum Outcome<T> {
    Done(T, Seen),
    Refused(Why, Option<Seen>),
    Nothing,
}

/// The numbers a receipt carries (12.3).
#[derive(Clone, Copy)]
struct Seen {
    backend: BackendId,
    restore: u64,
    life: u64,
    change: u64,
    music_count: u64,
}

impl Seen {
    fn of(receipt: &Receipt) -> Seen {
        Seen {
            backend: receipt.stamp.backend,
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
    WrongLogin,
    TooManyTries { wait_ms: u64 },
    AccountOff,
    Busy,
    StartAgain,
    NotSignedIn,
    NoSuchDevice,
    PasswordNotAllowed,
    PasswordSetupOnly,
    NoMusicLogin,
    Incompatible { low: u32, high: u32 },
    BadRequest,
}

impl Why {
    fn of(refusal: &Refusal) -> Why {
        match refusal {
            Refusal::WrongLogin => Why::WrongLogin,
            Refusal::TooManyTries { wait } => Why::TooManyTries { wait_ms: wait.ms() },
            Refusal::AccountOff => Why::AccountOff,
            Refusal::Busy => Why::Busy,
            Refusal::StartAgain => Why::StartAgain,
            Refusal::NotSignedIn => Why::NotSignedIn,
            Refusal::NoSuchDevice => Why::NoSuchDevice,
            Refusal::PasswordNotAllowed => Why::PasswordNotAllowed,
            Refusal::PasswordSetupOnly => Why::PasswordSetupOnly,
            Refusal::BadRequest => Why::BadRequest,
            Refusal::NoMusicLogin => Why::NoMusicLogin,
            Refusal::Incompatible { speaks } => Why::Incompatible { low: speaks.low(), high: speaks.high() },
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

/// The address a person typed, as the origin every ask is made to: `https://host[:port]`.
// to be origin's: `origin::Address::typed` is the one reading of a typed address. Until it
// lands, proto's `hello_url` is the reading of the host and port, and this adds only what
// a person types: no scheme means `https://`; `http://` is refused as needing HTTPS.
fn typed_origin(text: &str) -> Result<String, SignInState> {
    let text = text.trim();
    if text.is_empty() {
        return Err(SignInState::BadAddress);
    }
    let lower = text.get(..7).map(|start| start.to_ascii_lowercase());
    if lower.as_deref() == Some("http://") {
        return Err(SignInState::NeedsHttps);
    }
    let full = if text.contains("://") { text.to_owned() } else { format!("https://{text}") };
    let hello = proto::hello_url(&full).ok_or(SignInState::BadAddress)?;
    hello.strip_suffix(proto::HELLO).map(str::to_owned).ok_or(SignInState::BadAddress)
}

/// The tidying of a typed one-time code.
// to be account's: a person may type it in small letters, or with spaces or dashes between
// groups. the design does not say; account takes only the exact twelve.
fn tidy_code(text: &str) -> Option<OneTimeCode> {
    let tidied: Zeroizing<String> = Zeroizing::new(text.chars().filter(|c| !matches!(c, ' ' | '-')).map(|c| c.to_ascii_uppercase()).collect());
    OneTimeCode::new(&tidied)
}

impl Client {
    /// The generation now: every [`Ask`] carries the one it was made under. An answer to an
    /// ask of an earlier generation is thrown away.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the machine still waits for the answer to this ask. One it no longer waits
    /// for may be cancelled; its answer would be thrown away.
    pub fn waits_for(&self, ask: u64) -> bool {
        self.live.as_ref().is_some_and(|live| live.flight.iter().any(|flight| flight.id == ask))
    }

    /// **Start**, after the app is already usable: hand over what was kept. A kept music
    /// login is handed to the player first, before anything else. Starting again without a
    /// stop stops first.
    pub fn start(&mut self, app: App, kept: Kept, given: &Given) -> Vec<Do> {
        self.stop();
        self.generation = self.generation.wrapping_add(1);
        let mut out = Vec::new();
        let Kept { device_secret, session, music, account, to_tell, offline } = kept;
        // R1: the player first.
        if let Some(login) = &music {
            out.push(Do::Hand(Hand::Login(copy(login))));
        }
        let account = account.filter(|file| typed_origin(&file.backend).is_ok() && BackendId::from_hex(&file.backend_id).is_some());
        let mut live = Live {
            app,
            offline,
            device_secret,
            session: None,
            account: None,
            music,
            to_tell: to_tell.filter(|tell| typed_origin(&tell.backend).is_ok()),
            flight: Vec::new(),
            form: Form::idle(),
            reach: Reach::Unknown,
            checked_ms: None,
            told_ms: None,
            renewal: Renewal::default(),
            devices: Devices::not_asked(),
            password: PasswordShown::idle(),
            music_words: None,
            too_old: false,
        };
        match (session, account) {
            (Some(session), Some(account)) => {
                live.form.email = account.email.clone();
                live.form.address = account.backend.clone();
                live.session = Some(session);
                live.account = Some(account);
            }
            (Some(_), None) => {
                // A session with no account to say where it is from cannot be used.
                out.push(Do::Forget(Forget::Session));
            }
            (None, Some(_)) => out.push(Do::Forget(Forget::Account)),
            (None, None) => {}
        }
        if live.device_secret.is_none()
            && let Some(random) = &given.random
        {
            let secret = DeviceSecret::from_bytes(*random.0);
            out.push(Do::Keep(Keep::DeviceSecret(secret.clone())));
            live.device_secret = Some(secret);
        }
        self.live = Some(live);
        self.after_reach(given, &mut out, true);
        self.show(given, &mut out);
        out
    }

    /// **Stop**, at the moment the account changes: nothing asked before is acted on, and
    /// nothing more is done until the next `start`.
    pub fn stop(&mut self) {
        if self.live.take().is_some() {
            self.generation = self.generation.wrapping_add(1);
        }
        self.shown = None;
    }

    /// **Happened**: tell the machine, and be told what to do.
    pub fn happened(&mut self, event: Happened, given: &Given) -> Vec<Do> {
        let mut out = Vec::new();
        if self.live.is_none() {
            return out;
        }
        match event {
            Happened::Pressed(press) => self.pressed(press, given, &mut out),
            Happened::Answered { ask, status, body } => self.answered(ask, status, &body, given, &mut out),
            Happened::Failed { ask, failure } => self.failed(ask, failure, given, &mut out),
            Happened::Network { up } => {
                if up {
                    self.after_reach(given, &mut out, true);
                }
            }
            Happened::Offline(on) => self.offline(on, given, &mut out),
            Happened::Tick => self.tick(given, &mut out),
            Happened::MusicRefused { count } => self.music_refused(count, given, &mut out),
        }
        self.show(given, &mut out);
        out
    }
}

impl Form {
    fn idle() -> Form {
        Form { state: SignInState::Idle, words: String::new(), address: String::new(), email: String::new(), ask_permission: false }
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

mod machine;
