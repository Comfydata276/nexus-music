//! The one entry point the phone's binding calls (5.5: "one function for each call, taking
//! and giving JSON"): the same four calls as the Windows app makes directly, as one call
//! taking a JSON envelope and giving one back. The phone's `native/` layer is then one
//! function, `nexusClient(bytes) -> bytes`, holding one [`Client`] per handle.
//!
//! In (at most [`CALL_JSON_MAX_BYTES`]; every object refuses a field it does not know):
//!
//! ```text
//! {"start":{"app":{"version":"0.41.0","kind":"phone","device_name":"…"},
//!           "kept":{"device_secret":{"secret":"<64 hex>","backend_id":"<32 hex>"|null}|null,
//!                   "saved":{…}|null,"notes":[{…}…],"offline":false,
//!                   "diagnostics":true|false|null,
//!                   "diag_schedule":{"started_ms":…,"fault_ms":…|null,"attempts":[…]}|null},
//!           "given":{"now_ms":…,"local_offset_minutes":600,"random":"<64 hex>"|null}}}
//! {"happened":{"event":EVENT,"given":{…}}}
//! {"stop":{}}
//! {"waits_for":{"ask":7}}          gives {"generation":N,"do":[],"waits":true|false}
//!
//! EVENT: {"pressed":PRESS}
//!        {"answered":{"ask":7,"status":200,"body":"<the body as text>"}}   (or "body_bytes":[…]
//!                                                                         when it is not UTF-8)
//!        {"failed":{"ask":7,"failure":{"how":"connect","inside":true,"permission":false}}}
//!        {"network":{"up":true}}  {"offline":true}  {"tick":{}}
//!        {"music_refused":{"count":3,"generation":5}}
//!        {"music_looked":{"look":{"inside":true,"permission":false},"count":3,"generation":5}}
//!        {"events_waiting":{"account":ACCOUNT,"life":1,"pending":12,"from_sequence":40}}
//!        {"events_read":{"read":4,"account":ACCOUNT,"life":1,"events":[LINE…]}}
//!        {"diagnostics_read":{"read":5,"text":"Nexus Music 0.41.0 on phone …"|null}}
//!        {"diagnostics_switch":true|false|null}  {"diagnostics_fault":{"at_ms":…}}
//!        {"in_use":true|false}
//! ACCOUNT: {"backend_id":"<32 hex>","account":21}
//! LINE:    one journal line's JSON object, exactly as `events::Placed::line` wrote it (without
//!          its LF); a line that does not read is left out of the batch
//! PRESS: {"sign_in":{"address":…,"email":…,"password":…,"look":{…}}}
//!        {"sign_in_with_code":{"address":…,"code":…,"password":…,"again":…,"look":{…}}}
//!        {"form_closed":{}}  {"sign_out":{}}  {"open_devices":{}}  {"force_out":{"device":12}}
//!        {"change_password":{"current":…,"new":…,"again":…}}  {"notice_seen":{}}
//! ```
//!
//! `saved`, each of `notes` and `diag_schedule` are what [`Do::Keep`] gave as
//! `{"saved":{…}}`, `{"note":{…}}` and `{"diag_schedule":{…}}`, written back as they were
//! given. `diagnostics` is the app's own setting (the person's choice, `null` until made).
//!
//! The sending half's things to do come as `{"journal":{"read_events":{…}}}`,
//! `{"journal":{"read_diagnostics":{"read":5}}}`, `{"journal":{"taken":{…}}}`,
//! `{"journal":{"refused":{…}}}`, `{"journal":{"life_moved":{…}}}` and
//! `{"journal":{"resend_all":{"backend_id":"<32 hex>"}}}` ([`JournalDo`]), event ids as 32
//! lower-case hex digits.
//!
//! Out: `{"generation":N,"do":[…]}`, each thing to do as [`Do`] serialises it; or
//! `{"error":"input"}` for input this cannot read, which names no part of it and changes
//! nothing: an envelope that is not one of these, and an answer larger than its ask allows
//! (the ask is still waited for). **What was kept never stops a start**: a part of it that
//! is there and does not read (the music login with a member it does not know, a session
//! that is not 64 hex digits, an older form) is treated as absent and given back as a
//! `Forget` (`saved`, `device_secret`, or `unreadable_note` by its place in `notes`).
//!
//! Secrets read here (passwords, a session, the music login, an answer's body) go straight
//! into buffers that are wiped when dropped, on the error path too. What the JVM side holds
//! (its strings and byte arrays) cannot be wiped from here, and is not. Nor is what the JSON
//! decoder keeps while it reads: the same bytes are already in the JVM's memory, which
//! cannot be wiped, so wiping that would buy nothing (ruled).

use super::*;
use serde::Deserialize;
use serde_json::value::RawValue;

/// The most one call's JSON may be: room for the largest answer an ask allows (account's
/// list ceiling), written as text with every byte escaped.
pub const CALL_JSON_MAX_BYTES: usize = 6 * account::LIST_REPLY_MAX_BYTES + 64 * 1024;

impl<'de> Deserialize<'de> for Typed {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|text| Typed(Zeroizing::new(text)))
    }
}

/// A secret written as hex, wiped once read.
struct Hex(Zeroizing<String>);

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|text| Hex(Zeroizing::new(text)))
    }
}

/// An answer's body as text, wiped once read.
struct BodyText(Zeroizing<String>);

impl<'de> Deserialize<'de> for BodyText {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|text| BodyText(Zeroizing::new(text)))
    }
}

/// An answer's body as bytes, wiped once read.
struct BodyBytes(Zeroizing<Vec<u8>>);

impl<'de> Deserialize<'de> for BodyBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Vec::<u8>::deserialize(deserializer).map(|bytes| BodyBytes(Zeroizing::new(bytes)))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Call<'a> {
    Start {
        app: AppIn,
        #[serde(borrow)]
        kept: KeptIn<'a>,
        given: GivenIn,
    },
    Happened {
        event: EventIn,
        given: GivenIn,
    },
    Stop {},
    WaitsFor {
        ask: u64,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppIn {
    version: String,
    kind: KindIn,
    device_name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum KindIn {
    Pc,
    Phone,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GivenIn {
    now_ms: u64,
    #[serde(default)]
    local_offset_minutes: i32,
    #[serde(default)]
    random: Option<Hex>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeptIn<'a> {
    /// Each part is taken as it was kept and read by itself, so that one that does not read
    /// is forgotten and never stops the start.
    #[serde(default, borrow)]
    device_secret: Option<&'a RawValue>,
    #[serde(default, borrow)]
    saved: Option<&'a RawValue>,
    #[serde(default, borrow)]
    notes: Vec<&'a RawValue>,
    #[serde(default)]
    offline: bool,
    #[serde(default)]
    diagnostics: Option<bool>,
    #[serde(default, borrow)]
    diag_schedule: Option<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountIn {
    backend_id: String,
    account: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceKeyIn {
    secret: Hex,
    #[serde(default)]
    backend_id: Option<String>,
}

/// The music login as kept: every member named, nothing else, before account reads it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct MusicIn<'a> {
    #[serde(borrow)]
    address: &'a RawValue,
    #[serde(borrow)]
    name: &'a RawValue,
    #[serde(borrow)]
    salt: &'a RawValue,
    #[serde(borrow)]
    token: &'a RawValue,
    count: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeIn {
    session: u64,
    secret: Hex,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedIn<'a> {
    nexus: String,
    backend_id: String,
    account: u64,
    email: String,
    name: String,
    device: u64,
    restore: u64,
    life: u64,
    change: u64,
    #[serde(default)]
    notice: Option<String>,
    #[serde(default)]
    session: Option<Hex>,
    #[serde(default)]
    sign_out: Option<RevokeIn>,
    /// Borrowed from the input, so no owned copy of the salt and token is made here.
    #[serde(default, borrow)]
    music: Option<&'a RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteIn {
    request: u64,
    revoke: RevokeIn,
    nexus: String,
    backend_id: String,
    restore: u64,
    made_ms: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum EventIn {
    Pressed(PressIn),
    Answered {
        ask: u64,
        status: u16,
        #[serde(default)]
        body: Option<BodyText>,
        #[serde(default)]
        body_bytes: Option<BodyBytes>,
    },
    Failed {
        ask: u64,
        failure: Failure,
    },
    Network {
        up: bool,
    },
    Offline(bool),
    Tick {},
    MusicRefused {
        count: u64,
        generation: u64,
    },
    MusicLooked {
        look: Look,
        count: u64,
        generation: u64,
    },
    EventsWaiting {
        account: AccountIn,
        life: u64,
        pending: u64,
        from_sequence: u64,
    },
    EventsRead {
        read: u64,
        account: AccountIn,
        life: u64,
        events: Vec<Box<RawValue>>,
    },
    DiagnosticsRead {
        read: u64,
        #[serde(default)]
        text: Option<String>,
    },
    DiagnosticsSwitch(Option<bool>),
    DiagnosticsFault {
        at_ms: u64,
    },
    InUse(bool),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PressIn {
    SignIn {
        address: String,
        email: String,
        password: Typed,
        #[serde(default)]
        look: Look,
    },
    SignInWithCode {
        address: String,
        code: Typed,
        password: Typed,
        again: Typed,
        #[serde(default)]
        look: Look,
    },
    FormClosed {},
    SignOut {},
    OpenDevices {},
    ForceOut {
        device: u64,
    },
    ChangePassword {
        current: Typed,
        new: Typed,
        again: Typed,
    },
    NoticeSeen {},
}

fn random(hex: Option<Hex>) -> Result<Option<Random>, ()> {
    let Some(hex) = hex else { return Ok(None) };
    let secret = DeviceSecret::from_hex(&hex.0).ok_or(())?;
    Ok(Some(Random::new(*secret.expose_bytes())))
}

fn given(given: GivenIn) -> Result<Given, ()> {
    Ok(Given { now_ms: given.now_ms, local_offset_minutes: given.local_offset_minutes, random: random(given.random)? })
}

fn app(app: AppIn) -> Result<App, ()> {
    Ok(App {
        version: Version::parse(&app.version).ok_or(())?,
        kind: match app.kind {
            KindIn::Pc => DeviceKind::Pc,
            KindIn::Phone => DeviceKind::Phone,
        },
        device_name: Name::new(&app.device_name).ok_or(())?,
    })
}

fn journal_account(account: AccountIn) -> Result<JournalAccount, ()> {
    Ok(JournalAccount { backend_id: BackendId::from_hex(&account.backend_id).ok_or(())?, account: AccountId::new(account.account).ok_or(())? })
}

fn revoke(revoke: RevokeIn) -> Option<SignOutNote> {
    Some(SignOutNote { session: SessionId::new(revoke.session)?, secret: account::SignOutSecret::from_hex(&revoke.secret.0)? })
}

fn device_key(raw: &RawValue) -> Option<DeviceKey> {
    let key: DeviceKeyIn = serde_json::from_str(raw.get()).ok()?;
    if key.backend_id.as_deref().is_some_and(|id| BackendId::from_hex(id).is_none()) {
        return None;
    }
    Some(DeviceKey { secret: DeviceSecret::from_hex(&key.secret.0)?, backend_id: key.backend_id })
}

/// What signing in left, read strictly, its music login included: all of it or nothing.
fn saved(raw: &RawValue) -> Option<Saved> {
    let saved: SavedIn<'_> = serde_json::from_str(raw.get()).ok()?;
    let music = match saved.music {
        Some(raw) => {
            serde_json::from_str::<MusicIn<'_>>(raw.get()).ok()?;
            Some(account::read::<MusicLogin>(raw.get().as_bytes()).ok()?)
        }
        None => None,
    };
    Some(Saved {
        nexus: saved.nexus,
        backend_id: saved.backend_id,
        account: AccountId::new(saved.account)?,
        email: saved.email,
        name: saved.name,
        device: saved.device,
        restore: saved.restore,
        life: saved.life,
        change: saved.change,
        notice: saved.notice,
        session: match saved.session {
            Some(hex) => Some(Session::from_hex(&hex.0)?),
            None => None,
        },
        sign_out: match saved.sign_out {
            Some(note) => Some(revoke(note)?),
            None => None,
        },
        music,
    })
}

fn note(raw: &RawValue) -> Option<Note> {
    let note: NoteIn = serde_json::from_str(raw.get()).ok()?;
    Some(Note {
        request: note.request,
        revoke: revoke(note.revoke)?,
        nexus: note.nexus,
        backend_id: note.backend_id,
        restore: note.restore,
        made_ms: note.made_ms,
    })
}

/// What was kept, read back: each part strictly and by itself (as ruled). A part that is
/// there and does not read (an unknown member, a wrong or older form) is treated as absent
/// and forgotten; what does read is used, whatever the others are.
fn kept(kept: KeptIn<'_>) -> Kept {
    let mut unreadable = Vec::new();
    let device_secret = kept.device_secret.filter(|raw| raw.get() != "null").and_then(|raw| {
        let key = device_key(raw);
        if key.is_none() {
            unreadable.push(Forget::DeviceSecret);
        }
        key
    });
    let saved = kept.saved.filter(|raw| raw.get() != "null").and_then(|raw| {
        let found = saved(raw);
        if found.is_none() {
            unreadable.push(Forget::Saved);
        }
        found
    });
    let mut notes = Vec::new();
    for (index, raw) in kept.notes.into_iter().enumerate() {
        match note(raw) {
            Some(found) => notes.push(found),
            None => unreadable.push(Forget::UnreadableNote { index }),
        }
    }
    let diag_schedule = kept.diag_schedule.filter(|raw| raw.get() != "null").and_then(|raw| {
        let schedule = serde_json::from_str::<DiagSchedule>(raw.get()).ok();
        if schedule.is_none() {
            unreadable.push(Forget::DiagSchedule);
        }
        schedule
    });
    Kept { device_secret, saved, notes, offline: kept.offline, unreadable, diagnostics: kept.diagnostics, diag_schedule }
}

fn event(event: EventIn) -> Result<Happened, ()> {
    Ok(match event {
        EventIn::Pressed(press) => Happened::Pressed(match press {
            PressIn::SignIn { address, email, password, look } => Press::SignIn { address, email, password, look },
            PressIn::SignInWithCode { address, code, password, again, look } => Press::SignInWithCode { address, code, password, again, look },
            PressIn::FormClosed {} => Press::FormClosed,
            PressIn::SignOut {} => Press::SignOut,
            PressIn::OpenDevices {} => Press::OpenDevices,
            PressIn::ForceOut { device } => Press::ForceOut { device },
            PressIn::ChangePassword { current, new, again } => Press::ChangePassword { current, new, again },
            PressIn::NoticeSeen {} => Press::NoticeSeen,
        }),
        EventIn::Answered { ask, status, body, body_bytes } => {
            let bytes = match (body, body_bytes) {
                // Moved, not copied: the wiping buffer is handed on whole.
                (Some(mut text), None) => Zeroizing::new(std::mem::take(&mut *text.0).into_bytes()),
                (None, Some(bytes)) => bytes.0,
                (None, None) => Zeroizing::new(Vec::new()),
                (Some(_), Some(_)) => return Err(()),
            };
            Happened::Answered { ask, status, body: AnswerBody(bytes) }
        }
        EventIn::Failed { ask, failure } => Happened::Failed { ask, failure },
        EventIn::Network { up } => Happened::Network { up },
        EventIn::Offline(on) => Happened::Offline(on),
        EventIn::Tick {} => Happened::Tick,
        EventIn::MusicRefused { count, generation } => Happened::MusicRefused { count, generation },
        EventIn::MusicLooked { look, count, generation } => Happened::MusicLooked { look, count, generation },
        EventIn::EventsWaiting { account, life, pending, from_sequence } => {
            Happened::EventsWaiting { account: journal_account(account)?, life, pending, from_sequence }
        }
        EventIn::EventsRead { read, account, life, events } => Happened::EventsRead {
            read,
            account: journal_account(account)?,
            life,
            events: events.iter().filter_map(|line| Placed::read(line.get().as_bytes()).ok()).collect(),
        },
        EventIn::DiagnosticsRead { read, text } => Happened::DiagnosticsRead { read, text },
        EventIn::DiagnosticsSwitch(choice) => Happened::DiagnosticsSwitch(choice),
        EventIn::DiagnosticsFault { at_ms } => Happened::DiagnosticsFault { at_ms },
        EventIn::InUse(on) => Happened::InUse(on),
    })
}

#[derive(Serialize)]
struct Out<'a> {
    generation: u64,
    #[serde(rename = "do")]
    things: &'a [Do],
    #[serde(skip_serializing_if = "Option::is_none")]
    waits: Option<bool>,
}

fn error() -> Vec<u8> {
    br#"{"error":"input"}"#.to_vec()
}

impl Client {
    /// The four calls as one: a JSON envelope in, the things to do out (see the module).
    /// Input this cannot read changes nothing and is never echoed back.
    pub fn call_json(&mut self, input: &[u8]) -> Vec<u8> {
        if input.len() > CALL_JSON_MAX_BYTES {
            return error();
        }
        let Ok(text) = std::str::from_utf8(input) else { return error() };
        let Ok(call) = serde_json::from_str::<Call<'_>>(text) else { return error() };
        let things = match call {
            Call::Start { app: app_in, kept: kept_in, given: given_in } => {
                let (Ok(app), Ok(given)) = (app(app_in), given(given_in)) else { return error() };
                self.start(app, kept(kept_in), &given)
            }
            Call::Happened { event: event_in, given: given_in } => {
                let (Ok(event), Ok(given)) = (event(event_in), given(given_in)) else { return error() };
                // An answer larger than its ask allows is refused before the ask is touched,
                // so that the right one can still be handed in for it.
                if let Happened::Answered { ask, body, .. } = &event
                    && self.max_body_for(*ask).is_some_and(|max_body| body.0.len() > max_body)
                {
                    return error();
                }
                self.happened(event, &given)
            }
            Call::Stop {} => {
                self.stop();
                Vec::new()
            }
            Call::WaitsFor { ask } => {
                let waits = Some(self.waits_for(ask));
                return serde_json::to_vec(&Out { generation: self.generation, things: &[], waits }).unwrap_or_else(|_| error());
            }
        };
        serde_json::to_vec(&Out { generation: self.generation, things: &things, waits: None }).unwrap_or_else(|_| error())
    }
}
