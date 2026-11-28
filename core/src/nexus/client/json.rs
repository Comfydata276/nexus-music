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
//!                   "saved":{…}|null,"notes":[{…}…],"offline":false},
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
//! PRESS: {"sign_in":{"address":…,"email":…,"password":…,"look":{…}}}
//!        {"sign_in_with_code":{"address":…,"code":…,"password":…,"again":…,"look":{…}}}
//!        {"form_closed":{}}  {"sign_out":{}}  {"open_devices":{}}  {"force_out":{"device":12}}
//!        {"change_password":{"current":…,"new":…,"again":…}}  {"notice_seen":{}}
//! ```
//!
//! `saved` and each of `notes` are what [`Do::Keep`] gave as `{"saved":{…}}` and
//! `{"note":{…}}`, written back as they were given.
//!
//! Out: `{"generation":N,"do":[…]}`, each thing to do as [`Do`] serialises it; or
//! `{"error":"input"}` for input this cannot read, which names no part of it and changes
//! nothing: a part of what was kept that is there but does not read (the music login with
//! a member it does not know, a session that is not 64 hex digits), and an answer larger
//! than its ask allows, are this; the ask is still waited for.
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
    #[serde(default)]
    device_secret: Option<DeviceKeyIn>,
    #[serde(default, borrow)]
    saved: Option<SavedIn<'a>>,
    #[serde(default)]
    notes: Vec<NoteIn>,
    #[serde(default)]
    offline: bool,
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

fn revoke(revoke: RevokeIn) -> Option<SignOutNote> {
    Some(SignOutNote { session: SessionId::new(revoke.session)?, secret: account::SignOutSecret::from_hex(&revoke.secret.0)? })
}

/// What was kept, read back, every part of it there checked: one that is there and does not
/// read is an input error, and nothing is started from it.
fn kept(kept: KeptIn<'_>) -> Result<Kept, ()> {
    let device_secret = match kept.device_secret {
        Some(key) => {
            if key.backend_id.as_deref().is_some_and(|id| BackendId::from_hex(id).is_none()) {
                return Err(());
            }
            Some(DeviceKey { secret: DeviceSecret::from_hex(&key.secret.0).ok_or(())?, backend_id: key.backend_id })
        }
        None => None,
    };
    let saved = match kept.saved {
        Some(saved) => {
            let music = match saved.music {
                Some(raw) => {
                    serde_json::from_str::<MusicIn<'_>>(raw.get()).map_err(|_| ())?;
                    Some(account::read::<MusicLogin>(raw.get().as_bytes()).map_err(|_| ())?)
                }
                None => None,
            };
            Some(Saved {
                nexus: saved.nexus,
                backend_id: saved.backend_id,
                account: AccountId::new(saved.account).ok_or(())?,
                email: saved.email,
                name: saved.name,
                device: saved.device,
                restore: saved.restore,
                life: saved.life,
                change: saved.change,
                notice: saved.notice,
                session: match saved.session {
                    Some(hex) => Some(Session::from_hex(&hex.0).ok_or(())?),
                    None => None,
                },
                sign_out: match saved.sign_out {
                    Some(note) => Some(revoke(note).ok_or(())?),
                    None => None,
                },
                music,
            })
        }
        None => None,
    };
    let mut notes = Vec::new();
    for note in kept.notes {
        notes.push(Note {
            request: note.request,
            revoke: revoke(note.revoke).ok_or(())?,
            nexus: note.nexus,
            backend_id: note.backend_id,
            restore: note.restore,
            made_ms: note.made_ms,
        });
    }
    Ok(Kept { device_secret, saved, notes, offline: kept.offline })
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
                let (Ok(app), Ok(kept), Ok(given)) = (app(app_in), kept(kept_in), given(given_in)) else { return error() };
                self.start(app, kept, &given)
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
