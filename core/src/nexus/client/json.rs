//! The one entry point the phone's binding calls (5.5: "one function for each call, taking
//! and giving JSON"): the same four calls as the Windows app makes directly, as one call
//! taking a JSON envelope and giving one back. The phone's `native/` layer is then one
//! function, `nexusClient(bytes) -> bytes`, holding one [`Client`].
//!
//! In:
//!
//! ```text
//! {"start":{"app":{"version":"0.41.0","kind":"phone","device_name":"…"},
//!           "kept":{"device_secret":"<64 hex>"|null,"saved":{…}|null,"note":{…}|null,"offline":false},
//!           "given":{"now_ms":…,"local_offset_minutes":600,"random":"<64 hex>"|null}}}
//! {"happened":{"event":EVENT,"given":{…}}}
//! {"stop":{}}
//!
//! EVENT: {"pressed":PRESS}
//!        {"answered":{"ask":7,"status":200,"body":"<the body as text>"}}   (or "body_bytes":[…]
//!                                                                         when it is not UTF-8)
//!        {"failed":{"ask":7,"failure":{"how":"connect","inside":true,"permission":false}}}
//!        {"network":{"up":true}}  {"offline":true}  {"tick":{}}  {"music_refused":{"count":3}}
//! PRESS: {"sign_in":{"address":…,"email":…,"password":…}}
//!        {"sign_in_with_code":{"address":…,"code":…,"password":…,"again":…}}
//!        {"sign_out":{}}  {"open_devices":{}}  {"force_out":{"device":12}}
//!        {"change_password":{"current":…,"new":…,"again":…}}  {"notice_seen":{}}
//! ```
//!
//! `saved` is what [`Do::Keep`] gave as `{"saved":{…}}`, written back as it was given.
//!
//! Out: `{"generation":N,"do":[…]}`, each thing to do as [`Do`] serialises it; or
//! `{"error":"input"}` for input this cannot read, which names no part of it.

use super::*;
use serde::Deserialize;
use serde_json::value::RawValue;
use zeroize::Zeroize;

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

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Call {
    Start { app: AppIn, kept: KeptIn, given: GivenIn },
    Happened { event: EventIn, given: GivenIn },
    Stop {},
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
struct KeptIn {
    #[serde(default)]
    device_secret: Option<Hex>,
    #[serde(default)]
    saved: Option<SavedIn>,
    #[serde(default)]
    note: Option<Note>,
    #[serde(default)]
    offline: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedIn {
    nexus: String,
    backend_id: String,
    account: String,
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
    music: Option<Box<RawValue>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum EventIn {
    Pressed(PressIn),
    Answered {
        ask: u64,
        status: u16,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        body_bytes: Option<Vec<u8>>,
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
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum PressIn {
    SignIn { address: String, email: String, password: Typed },
    SignInWithCode { address: String, code: Typed, password: Typed, again: Typed },
    SignOut {},
    OpenDevices {},
    ForceOut { device: u64 },
    ChangePassword { current: Typed, new: Typed, again: Typed },
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

/// What was kept, read back. A part that cannot be read is left out (and so, for a session,
/// signs nobody in), never guessed at; the music login, if it reads, is still handed over.
fn kept(kept: KeptIn) -> Kept {
    let device_secret = kept.device_secret.and_then(|hex| DeviceSecret::from_hex(&hex.0));
    let saved = kept.saved.map(|saved| {
        let music = saved.music.and_then(|raw| account::read::<MusicLogin>(raw.get().as_bytes()).ok());
        let mut raw_session = saved.session;
        let session = raw_session.as_ref().and_then(|hex| Session::from_hex(&hex.0));
        if let Some(hex) = &mut raw_session {
            hex.0.zeroize();
        }
        Saved {
            nexus: saved.nexus,
            backend_id: saved.backend_id,
            account: saved.account,
            email: saved.email,
            name: saved.name,
            device: saved.device,
            restore: saved.restore,
            life: saved.life,
            change: saved.change,
            notice: saved.notice,
            session,
            music,
        }
    });
    Kept { device_secret, saved, note: kept.note, offline: kept.offline }
}

fn event(event: EventIn) -> Result<Happened, ()> {
    Ok(match event {
        EventIn::Pressed(press) => Happened::Pressed(match press {
            PressIn::SignIn { address, email, password } => Press::SignIn { address, email, password },
            PressIn::SignInWithCode { address, code, password, again } => Press::SignInWithCode { address, code, password, again },
            PressIn::SignOut {} => Press::SignOut,
            PressIn::OpenDevices {} => Press::OpenDevices,
            PressIn::ForceOut { device } => Press::ForceOut { device },
            PressIn::ChangePassword { current, new, again } => Press::ChangePassword { current, new, again },
            PressIn::NoticeSeen {} => Press::NoticeSeen,
        }),
        EventIn::Answered { ask, status, body, body_bytes } => {
            let bytes = match (body, body_bytes) {
                (Some(text), None) => text.into_bytes(),
                (None, Some(bytes)) => bytes,
                (None, None) => Vec::new(),
                (Some(_), Some(_)) => return Err(()),
            };
            Happened::Answered { ask, status, body: AnswerBody::new(bytes) }
        }
        EventIn::Failed { ask, failure } => Happened::Failed { ask, failure },
        EventIn::Network { up } => Happened::Network { up },
        EventIn::Offline(on) => Happened::Offline(on),
        EventIn::Tick {} => Happened::Tick,
        EventIn::MusicRefused { count } => Happened::MusicRefused { count },
    })
}

#[derive(Serialize)]
struct Out<'a> {
    generation: u64,
    #[serde(rename = "do")]
    things: &'a [Do],
}

fn error() -> Vec<u8> {
    br#"{"error":"input"}"#.to_vec()
}

impl Client {
    /// The four calls as one: a JSON envelope in, the things to do out (see the module).
    /// Nothing in the input is ever echoed back in an error.
    pub fn call_json(&mut self, input: &[u8]) -> Vec<u8> {
        let Ok(call) = serde_json::from_slice::<Call>(input) else { return error() };
        let things = match call {
            Call::Start { app: app_in, kept: kept_in, given: given_in } => {
                let (Ok(app), Ok(given)) = (app(app_in), given(given_in)) else { return error() };
                self.start(app, kept(kept_in), &given)
            }
            Call::Happened { event: event_in, given: given_in } => {
                let (Ok(event), Ok(given)) = (event(event_in), given(given_in)) else { return error() };
                self.happened(event, &given)
            }
            Call::Stop {} => {
                self.stop();
                Vec::new()
            }
        };
        serde_json::to_vec(&Out { generation: self.generation, things: &things }).unwrap_or_else(|_| error())
    }
}
