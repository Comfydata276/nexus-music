//! The surface an app whose player and library live outside Rust (the phone) calls.
//!
//! The Windows app implements [`Player`] over its own player and hands the deciding its
//! search index. The phone's player is Media3 and its library a database, both in Kotlin,
//! and nothing here may call into Kotlin while it decides. So:
//!
//! - **the player is events in and commands out.** Kotlin says what its player holds
//!   whenever that changes ([`Event::Player`]), and each time something counts in the order
//!   of choices ([`Event::Changed`], [`Event::LookingUp`], [`Event::LookedUp`]: the same
//!   places the Windows app counts, listed in `docs/linking-design.md`, 5.6.1). What the
//!   deciding wants done to the player comes back as data ([`Act::Carry`], [`Act::Pause`],
//!   [`Act::Take`]), to be done on the player's own thread;
//! - **the library is a snapshot** ([`Event::Library`]): the ids of its songs, handed
//!   across whenever the phone's copy changes;
//! - **what the person presses is an event,** and what to show is data: the list
//!   ([`Act::Shown`]), the device being steered ([`Act::View`]), what to say ([`Do::Say`]).
//!
//! Everything in an [`Event`] and an [`Act`] is plain data with serde, so one function
//! taking text and giving text ([`Phone::handle`]) is the whole of what crosses for these.
//!
//! **What does not cross.** The look, the stay at the relay and each send are requests,
//! made by `super::look::look`, `super::relay::keep_place` and `super::relay::send` over
//! the `Http` the phone's native side already gives this library, on threads that side
//! owns. It is told to start them by [`Do::Look`], [`Do::Begin`], [`Do::Send`] and
//! [`Do::Find`] (which stay on the Rust side of the phone's app: [`Phone::handle`] gives
//! them back apart), and answers with [`Phone::looked`], [`Phone::from_relay`] and
//! [`Phone::found`]. The secret never leaves Rust.
//!
//! **Nothing is decided against a player that has not caught up.** A command is carried
//! out a moment after it is decided, on another thread. Until Kotlin has said what its
//! player holds *after* doing what it was last asked ([`Event::Player`] with that act's
//! number), what the relay brings is kept, in order, and decided then: so the second of
//! two commands finds the queue as the first left it, exactly as on a device whose player
//! is asked directly, and a look is answered with what is true. A report made before the
//! act was done is not taken. A player that never reports holds nothing up for longer
//! than [`CATCH_UP_MS`].
//!
//! The one thing that is not exact: the answer to a command goes at once, and says in
//! brief what the player held before it (with play and pause, which are certain, applied).
//! The asking device reads `ok` as done and shows nothing from an answer; what it shows
//! comes from its next look, which waits as above.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use super::command::Command;
use super::device::{Brief, Device, Do, Env, Me, Player, Ready, Shown};
use super::look::Looked;
use super::message::{At, Full, Handed, Why};
use super::relay::FromRelay;
use super::rule::{Reservation, Rule, Waiting};
use super::seen::Seen;
use super::view::{Press, View};
use super::words::Here;
use super::{DeviceId, Nonce};
use crate::requests::Snapshot;
use crate::ways::{Ways, What, Who};

/// What the phone's player holds, as it last said.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Playing {
    pub brief: Brief,
    /// What a device steering this one shows.
    pub full: Full,
    /// What is playing and to come, by id, for another device to carry on with.
    pub handed: Handed,
    /// The ids of the songs to come, in the order they would play.
    pub ahead: Vec<String>,
    /// Whether there is a song in the player at all.
    pub has_song: bool,
    /// Where the sound has got to in the song the player holds, if it is sounding.
    pub sounding_at: Option<f64>,
}

/// Something that happened on the phone, told to the linking.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A few times a second while the app runs. `account` names the sign-in; `wanted` is
    /// signed in, a relay set up and "Link my devices" on.
    Tick { account: String, wanted: bool },
    /// The player holds this now. `after` is the number of the last act ([`Act::Carry`],
    /// [`Act::Pause`], [`Act::Take`]) that had been done when this was read from the
    /// player: nothing, if none has been asked yet.
    Player {
        playing: Playing,
        #[serde(default)]
        after: u64,
    },
    /// A way into the player was taken (`crate::ways`): `who` chose it (`here`, unless
    /// said: a press, a key, a notification's button, a headset's, the car's; `nobody` for
    /// what the app does by itself, as the next song starting; `guest`) and `what` it
    /// touches (`playing`, unless said; `manner` for the volume, repeat, a sleep timer
    /// being set). One for each choice: what the player does on the way is part of it.
    /// The order of choices moves on, and if the queue is the phone's own by it,
    /// [`Act::Claim`] comes back.
    Changed {
        #[serde(default = "here")]
        who: Who,
        #[serde(default = "playing")]
        what: What,
    },
    /// Something chosen is being looked up: the device is busy until [`Event::LookedUp`]
    /// with the ticket this is answered with ([`Act::Ticket`]), or a minute, whichever is first.
    LookingUp,
    LookedUp { ticket: u64 },
    /// The phone's copy of the library changed: the ids of its songs.
    Library { ids: Vec<String> },
    /// Pressed in the player or the queue while another device may be being steered.
    /// Answered with [`Act::Handled`]: if not, the press is the phone's own.
    Press { press: Press },
    Add { ids: Vec<String>, next: bool },
    Play { ids: Vec<String>, start: usize, context: String },
    /// A device was picked in the list. `in_a_jam`: the phone is in a jam with other people.
    Choose { place: String, in_a_jam: bool },
    /// "Play here". Answered with [`Act::Handled`].
    PlayHere,
    /// "Stop steering".
    LetGo,
    /// Pause, beside a device in the list.
    Pause { place: String },
    /// "Reset linking".
    Reset,
    /// "Link my devices" was set.
    Setting { on: bool },
    /// Signed out, or the app is closing.
    Stop,
    /// For a test only: carry on with a command that was held ([`Do::Later`]).
    Resume { ticket: u64 },
}

/// What the phone is asked to do.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "act", rename_all = "snake_case")]
pub enum Act {
    /// Something the deciding asked for that Kotlin does: say this, keep this list of
    /// devices seen, forget this page, the window is another device's or the phone's again.
    Do { ask: Do },
    /// The queue is the phone's own from this moment, by the person's own doing here or at
    /// another of their devices: it is no longer one taken up from another device
    /// (`handover::Here::taken`), it changed now by the phone's clock
    /// (`handover::Here::stamp`), and it is to be saved. Before the act it goes with.
    Claim,
    /// Carry this command out on the player, exactly as the same press would be, and then
    /// say what the player holds ([`Event::Player`], `after` this number). It has taken its
    /// place in the order of choices already, and claimed the queue if it does: carrying it
    /// out is **not** an [`Event::Changed`].
    Carry { number: u64, command: Command, ready: Ready },
    /// Stop the phone's own sound, and say what the player holds. Counted already.
    Pause { number: u64 },
    /// Take this queue and play it, and say what the player holds: each song and whether
    /// it was queued by hand, where to start, and how far in. Counted already.
    Take { number: u64, songs: Vec<(String, bool)>, begin: usize, position: f64, context: String },
    /// The list, after a tick.
    Shown { shown: Shown },
    /// The device being steered, as the player shows it; nothing, if none is.
    View { view: Option<View> },
    /// Whether what was pressed was asked of another device (and so is not the phone's own).
    Handled { by_another: bool },
    /// The ticket for a look-up that has begun.
    Ticket { ticket: u64 },
}

/// The phone's player, as the deciding asks it: what Kotlin last said, the order of
/// choices, and what is to be done to the player, waiting to be handed back.
#[derive(Default)]
struct Held {
    rule: Rule,
    playing: Playing,
    /// The look-ups under way: each one's ticket, when it began, and its place in the
    /// order of choices. No more than [`MOST_LOOK_UPS`], and none older than its minute.
    waits: Vec<(u64, u64, Waiting)>,
    tickets: u64,
    acts: Vec<Act>,
    /// The number of the last act asked of the player, and of the last it has said it did.
    asked: u64,
    done: u64,
    /// Who chose what is being done to the player, from which the order of choices and
    /// whose the queue is are both worked out.
    ways: Ways,
}

fn here() -> Who {
    Who::Here
}

fn playing() -> What {
    What::Playing
}

impl Held {
    fn ask(&mut self) -> u64 {
        self.asked += 1;
        self.asked
    }
}

impl Player for Held {
    fn brief(&self) -> Brief {
        self.playing.brief.clone()
    }
    fn full(&self) -> Full {
        self.playing.full.clone()
    }
    fn handed(&self) -> Handed {
        self.playing.handed.clone()
    }
    fn sounding_at(&self) -> Option<f64> {
        self.playing.sounding_at
    }
    /// (The phone says the next song has started as its player says so: there is no
    /// window here that lags the sound for this to bring up.)
    fn catch_up(&mut self) -> bool {
        false
    }
    fn has_song(&self) -> bool {
        self.playing.has_song
    }
    fn ahead(&self) -> Vec<String> {
        self.playing.ahead.clone()
    }
    fn carry_out(&mut self, command: &Command, ready: &Ready) {
        match command {
            Command::Play => self.playing.brief.playing = self.playing.has_song,
            Command::Pause => self.playing.brief.playing = false,
            _ => {}
        }
        // (Whatever was sounding was sounding before this: what sounds after it is for
        // the player to say.)
        self.playing.sounding_at = None;
        // The person chose it at another of their devices; the rule has counted it.
        self.ways.begin(Who::Elsewhere, true);
        let owed = self.ways.touch(Who::Here, command.touches());
        self.ways.end();
        if owed.claim {
            self.acts.push(Act::Claim);
        }
        let number = self.ask();
        self.acts.push(Act::Carry { number, command: command.clone(), ready: ready.clone() });
    }
    /// (The end of a move of the phone's music to another device: the app's own doing,
    /// which claims nothing. The device that took the queue claimed it.)
    fn pause(&mut self) {
        if self.ways.touch(Who::Nobody, What::Playing).count {
            self.rule.moved();
        }
        (self.playing.brief.playing, self.playing.sounding_at) = (false, None);
        let number = self.ask();
        self.acts.push(Act::Pause { number });
    }
    /// ("Play here": the person's own choice, at the phone.)
    fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, context: &str) {
        let owed = self.ways.touch(Who::Here, What::Playing);
        if owed.count {
            self.rule.moved();
        }
        if owed.claim {
            self.acts.push(Act::Claim);
        }
        self.playing.sounding_at = None;
        let number = self.ask();
        self.acts.push(Act::Take { number, songs: songs.to_vec(), begin, position, context: context.to_string() });
    }
    fn generation(&self) -> u64 {
        self.rule.generation()
    }
    fn busy(&self, now_ms: u64) -> bool {
        self.rule.busy(now_ms)
    }
    fn by(&self) -> Option<Nonce> {
        self.rule.by()
    }
    fn admit(&mut self, at: At, minute: u64, now_ms: u64) -> Result<Reservation, Why> {
        self.rule.admit(at, minute, now_ms)
    }
    fn finish(&mut self, reservation: Reservation, at: At, minute: u64, same_session: bool, nonce: Nonce) -> Result<(), Why> {
        self.rule.finish(reservation, at, minute, same_session, nonce)
    }
    fn refuse(&mut self, reservation: Reservation) {
        self.rule.refuse(reservation)
    }
    fn wait(&mut self, now_ms: u64, for_ms: u64) -> Waiting {
        self.rule.wait(now_ms, for_ms)
    }
    fn done(&mut self, waiting: Waiting) {
        self.rule.done(waiting)
    }
}

/// How long a look-up for something to play is given before the phone stops counting as
/// busy on its account.
const LOOK_UP_MS: u64 = 60_000;

/// The most look-ups counted as under way at once. (A phone looks one thing up at a time;
/// this is for one that says it has begun and never says it has ended.)
const MOST_LOOK_UPS: usize = 16;

/// How long the player is given to say it has done what it was asked, before what the
/// relay brought meanwhile is decided against what it last said.
pub const CATCH_UP_MS: u64 = 2_000;

/// The most frames kept while the player catches up. (More than the relay lets a room
/// send in that time.)
const MOST_KEPT: usize = 256;

/// The linking, for an app whose player and library are not in Rust.
pub struct Phone {
    device: Device,
    player: Held,
    library: Snapshot,
    me: Me,
    /// What the relay brought while the player had not caught up: the session, who was in
    /// the room, and the frame.
    kept: VecDeque<(u64, Vec<String>, (String, String))>,
    /// When the act the player has not caught up with was asked.
    asked_ms: u64,
}

/// What came of something that happened: what crosses to Kotlin, and what the Rust side of
/// the phone's app does itself (the requests).
#[derive(Debug, Default, PartialEq)]
pub struct Came {
    pub acts: Vec<Act>,
    /// [`Do::Look`], [`Do::StopLook`], [`Do::Begin`], [`Do::Renew`], [`Do::End`],
    /// [`Do::Send`], [`Do::Find`], [`Do::Later`] and [`Do::Log`].
    pub native: Vec<Do>,
}

impl Phone {
    /// `device` is the id made once for the installation; `name` what the person calls the
    /// phone; `remembered` the devices seen before, as last kept ([`Do::Remember`]).
    pub fn new(device: DeviceId, name: &str, remembered: Vec<Seen>) -> Phone {
        Phone { device: Device::new(remembered), player: Held::default(), library: Snapshot::default(), me: Me { device, name: name.to_string(), here: Here::Phone }, kept: VecDeque::new(), asked_ms: 0 }
    }

    /// The phone's name was changed.
    pub fn rename(&mut self, name: &str) {
        self.me.name = name.to_string();
    }

    fn with(&mut self, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, act: impl FnOnce(&mut Device, &mut Env) -> Vec<Do>) -> Came {
        let Phone { device, player, library, me, .. } = self;
        let before = player.asked;
        let asked = act(device, &mut Env { now_ms, today, me, player, library, random });
        if self.player.asked != before {
            self.asked_ms = now_ms;
        }
        let mut came = Came { acts: std::mem::take(&mut self.player.acts), native: Vec::new() };
        for ask in asked {
            match ask {
                Do::Say(_) | Do::Remember(_) | Do::Forget { .. } | Do::Steering(_) => came.acts.push(Act::Do { ask }),
                Do::Draw => came.acts.push(Act::View { view: self.device.view().cloned() }),
                native => came.native.push(native),
            }
        }
        came
    }

    /// Decides what was kept while the player caught up, for as long as it stays caught up.
    fn decide_kept(&mut self, came: &mut Came, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool) {
        while self.player.asked == self.player.done {
            let Some((session, places, frame)) = self.kept.pop_front() else { break };
            let more = self.with(now_ms, today, &mut *random, |device, env| device.heard(session, &places, &[frame], env));
            came.acts.extend(more.acts);
            came.native.extend(more.native);
        }
    }

    /// Something happened on the phone. `now_ms` is milliseconds on a clock that goes on
    /// counting while the phone sleeps (`elapsedRealtime`), `today` the day since 1970, and
    /// `random` fills bytes from the system's source of randomness or says it could not.
    pub fn happened(&mut self, event: Event, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool) -> Came {
        let handled = |came: Option<Came>| {
            let by_another = came.is_some();
            let mut came = came.unwrap_or_default();
            came.acts.push(Act::Handled { by_another });
            came
        };
        match event {
            Event::Tick { account, wanted } => {
                let mut came = Came::default();
                if self.player.asked != self.player.done && now_ms.saturating_sub(self.asked_ms) >= CATCH_UP_MS {
                    came.native.push(Do::Log(format!("the player has not said it did what it was asked (act {}): going on without it", self.player.asked)));
                    self.player.done = self.player.asked;
                }
                self.decide_kept(&mut came, now_ms, today, &mut *random);
                let ticked = self.with(now_ms, today, random, |device, env| device.tick(&account, wanted, env));
                came.acts.extend(ticked.acts);
                came.native.extend(ticked.native);
                let (shown, logged) = self.device.shown(now_ms);
                came.native.extend(logged);
                came.acts.push(Act::Shown { shown });
                came
            }
            Event::Player { playing, after } => {
                let mut came = Came::default();
                // (Read from the player before it did what it was last asked: not how it is.)
                if after >= self.player.asked {
                    (self.player.playing, self.player.done) = (playing, self.player.asked);
                    self.decide_kept(&mut came, now_ms, today, random);
                }
                came
            }
            Event::Changed { who, what } => {
                let owed = self.player.ways.touch(who, what);
                if owed.count {
                    self.player.rule.moved();
                }
                Came { acts: if owed.claim { vec![Act::Claim] } else { Vec::new() }, native: Vec::new() }
            }
            Event::LookingUp => {
                // (One whose minute is up counts for nothing already, and one that was
                // never said to be over is not kept for ever: the list has a bound.)
                let player = &mut self.player;
                while let Some(at) = player.waits.iter().position(|(_, began, _)| now_ms.saturating_sub(*began) >= LOOK_UP_MS).or((player.waits.len() >= MOST_LOOK_UPS).then_some(0)) {
                    let (.., waiting) = player.waits.remove(at);
                    player.rule.done(waiting);
                }
                player.tickets = player.tickets.wrapping_add(1);
                let waiting = player.rule.wait(now_ms, LOOK_UP_MS);
                player.waits.push((player.tickets, now_ms, waiting));
                Came { acts: vec![Act::Ticket { ticket: player.tickets }], native: Vec::new() }
            }
            Event::LookedUp { ticket } => {
                if let Some(at) = self.player.waits.iter().position(|(known, ..)| *known == ticket) {
                    let (.., waiting) = self.player.waits.remove(at);
                    self.player.rule.done(waiting);
                }
                Came::default()
            }
            Event::Library { ids } => {
                self.library.ids = ids;
                Came::default()
            }
            Event::Press { press } => {
                let mut asked = None;
                let came = self.with(now_ms, today, random, |device, env| {
                    asked = device.press(press, env);
                    asked.clone().unwrap_or_default()
                });
                handled(asked.map(|_| came))
            }
            Event::Add { ids, next } => {
                let mut asked = None;
                let came = self.with(now_ms, today, random, |device, env| {
                    asked = device.add(&ids, next, env);
                    asked.clone().unwrap_or_default()
                });
                handled(asked.map(|_| came))
            }
            Event::Play { ids, start, context } => {
                let mut asked = None;
                let came = self.with(now_ms, today, random, |device, env| {
                    asked = device.play(&ids, start, &context, env);
                    asked.clone().unwrap_or_default()
                });
                handled(asked.map(|_| came))
            }
            Event::PlayHere => {
                let mut asked = None;
                let came = self.with(now_ms, today, random, |device, env| {
                    asked = device.play_here(env);
                    asked.clone().unwrap_or_default()
                });
                handled(asked.map(|_| came))
            }
            Event::Choose { place, in_a_jam } => self.with(now_ms, today, random, |device, env| device.choose(&place, in_a_jam, env)),
            Event::Pause { place } => self.with(now_ms, today, random, |device, env| device.pause(&place, env)),
            Event::LetGo => self.with(now_ms, today, random, |device, _| device.let_go()),
            Event::Reset => self.with(now_ms, today, random, |device, _| device.reset()),
            Event::Setting { on } => self.with(now_ms, today, random, |device, _| device.setting(on)),
            Event::Stop => self.with(now_ms, today, random, |device, _| device.stop()),
            Event::Resume { ticket } => self.with(now_ms, today, random, |device, env| device.resume(ticket, env)),
        }
    }

    /// The same, as text in and text out: an [`Event`] as JSON, and the [`Act`]s as a JSON
    /// list. What the Rust side does itself is given back apart. An event that cannot be
    /// read does nothing and says nothing.
    pub fn handle(&mut self, event: &str, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool) -> (String, Vec<Do>) {
        let Ok(event) = serde_json::from_str::<Event>(event) else { return ("[]".to_string(), Vec::new()) };
        let came = self.happened(event, now_ms, today, random);
        (serde_json::to_string(&came.acts).unwrap_or_else(|_| "[]".to_string()), came.native)
    }

    /// A look has finished ([`Do::Look`]).
    pub fn looked(&mut self, number: u64, since_ms: u64, looked: Looked, reset: bool, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool) -> Came {
        self.with(now_ms, today, random, |device, env| device.looked(number, since_ms, looked, reset, env))
    }

    /// The stay at the relay has something to say ([`Do::Begin`]).
    pub fn from_relay(&mut self, session: u64, from: FromRelay, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool) -> Came {
        match from {
            FromRelay::Says(note) => {
                self.device.said(session, note);
                Came::default()
            }
            FromRelay::NoRandom => self.with(now_ms, today, random, |device, env| device.out_of_random(env.me.here)),
            FromRelay::Entered { place, token } => {
                self.device.entered(session, place, token);
                Came::default()
            }
            FromRelay::Heard { places, frames } => {
                // (Who is in the room is taken at once; what they sent, in order, when the
                // player has caught up.)
                let mut came = self.with(now_ms, today, &mut *random, |device, env| device.heard(session, &places, &[], env));
                for frame in frames {
                    if self.kept.len() < MOST_KEPT {
                        self.kept.push_back((session, places.clone(), frame));
                    }
                }
                self.decide_kept(&mut came, now_ms, today, random);
                came
            }
            FromRelay::Left => {
                self.device.left(session);
                Came::default()
            }
        }
    }

    /// The songs the music server has, of those a command brought ([`Do::Find`]).
    pub fn found(&mut self, ticket: u64, songs: Vec<String>, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool) -> Came {
        self.with(now_ms, today, random, |device, env| device.found(ticket, songs, env))
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::{self, ASK, Keys};
    use super::super::message::Ask;
    use super::super::secret::Secret;
    use super::super::text;
    use super::*;

    fn random(n: &mut u64) -> impl FnMut(&mut [u8]) -> bool + '_ {
        move |bytes| {
            *n += 1;
            for (at, byte) in bytes.iter_mut().enumerate() {
                *byte = n.to_le_bytes()[at % 8] ^ (at as u8).wrapping_mul(29);
            }
            true
        }
    }

    /// A phone signed in, in the room, with eight songs in its player.
    fn phone(n: &mut u64) -> (Phone, u64, [u8; 16]) {
        let mut phone = Phone::new([9; 16], "Phone", Vec::new());
        let ids: Vec<String> = (0..8).map(|n| format!("s{n}")).collect();
        let (acts, native) = phone.handle(r#"{"event":"tick","account":"the account","wanted":true}"#, 1_000, 100, &mut random(n));
        assert!(acts.contains(r#""act":"shown""#) && matches!(native.as_slice(), [Do::Look { reset: false, .. }]), "{acts} {native:?}");
        let Do::Look { number, .. } = native[0].clone() else { unreachable!() };
        let came = phone.looked(number, 1_000, Looked::Secret { secret: Secret::new([7; 32]), id: "k".into(), wrote: None }, false, 1_100, 100, &mut random(n));
        let Some(Do::Begin { session, .. }) = came.native.iter().find(|ask| matches!(ask, Do::Begin { .. })).cloned() else { panic!("no session: {came:?}") };
        let place = [5; 16];
        phone.from_relay(session, FromRelay::Entered { place, token: "a-token".into() }, 1_200, 100, &mut random(n));
        phone.handle(&playing(&ids, 0).to_string(), 1_300, 100, &mut random(n));
        phone.handle(&serde_json::json!({"event": "library", "ids": ids}).to_string(), 1_300, 100, &mut random(n));
        (phone, session, place)
    }

    /// What Kotlin says its player holds: these songs, the first of them playing, having
    /// done every act up to this one.
    fn playing(ids: &[String], after: u64) -> serde_json::Value {
        serde_json::json!({"event": "player", "after": after, "playing": {"brief": {"playing": true, "title": ids[0]}, "ahead": ids[1..], "has_song": true, "sounding_at": 3.0,
            "handed": {"songs": ids, "position": 3.0, "context": "an album"}}})
    }

    /// A frame from another device in the room, sealed for the phone.
    fn from_pc(n: u8, to: &[u8; 16], ask: Ask) -> (String, String) {
        let from = [1; 16];
        (text::place(&from), frame::seal(&Keys::of(&Secret::new([7; 32])), ASK, &[n; 24], &from, to, ask.text().as_bytes()).unwrap())
    }

    #[test]
    fn a_command_for_the_phone_comes_out_as_data_for_its_player() {
        let mut n = 0;
        let (mut phone, session, place) = phone(&mut n);
        let at = At { generation: 1, minute: 0 };
        let heard = |frames| FromRelay::Heard { places: vec![text::place(&place), text::place(&[1; 16])], frames };
        // Next, from a PC: the phone is told to carry it out, and answers it.
        let came = phone.from_relay(session, heard(vec![from_pc(1, &place, Ask::Do { command: Command::Next, at })]), 2_000, 100, &mut random(&mut n));
        assert_eq!(came.acts, [Act::Claim, Act::Carry { number: 1, command: Command::Next, ready: Ready::Nothing }]);
        assert!(came.native.iter().any(|ask| matches!(ask, Do::Send { .. })) && came.native.iter().any(|ask| matches!(ask, Do::Log(line) if line.ends_with("carried out"))));
        assert_eq!(serde_json::to_string(&came.acts).unwrap(), r#"[{"act":"claim"},{"act":"carry","number":1,"command":{"do":"next"},"ready":"nothing"}]"#);
        let ids: Vec<String> = (1..8).map(|n| format!("s{n}")).collect();
        phone.handle(&playing(&ids, 1).to_string(), 2_050, 100, &mut random(&mut n));
        // The same frame again: nothing for the player.
        let came = phone.from_relay(session, heard(vec![from_pc(1, &place, Ask::Do { command: Command::Next, at })]), 2_100, 100, &mut random(&mut n));
        assert!(came.acts.is_empty() && came.native.iter().any(|ask| matches!(ask, Do::Log(line) if line.contains("refused, Changed"))));
        // An entry of the queue is found for it; songs are named by id; a queue is taken as data.
        let at = At { generation: 2, minute: 0 };
        let came = phone.from_relay(session, heard(vec![from_pc(2, &place, Ask::Do { command: Command::Remove { at: 4, id: "s3".into() }, at })]), 2_200, 100, &mut random(&mut n));
        assert_eq!(came.acts, [Act::Claim, Act::Carry { number: 2, command: Command::Remove { at: 4, id: "s3".into() }, ready: Ready::Entry(1) }]);
        assert_eq!(serde_json::to_string(&came.acts[1..]).unwrap(), r#"[{"act":"carry","number":2,"command":{"do":"remove","with":{"at":4,"id":"s3"}},"ready":{"entry":1}}]"#);
        // The volume is how it sounds: carried out, and the queue is no more the phone's for it.
        phone.handle(&playing(&ids, 2).to_string(), 2_250, 100, &mut random(&mut n));
        let at = At { generation: 3, minute: 0 };
        let came = phone.from_relay(session, heard(vec![from_pc(3, &place, Ask::Do { command: Command::Volume { to: 0.5 }, at })]), 2_300, 100, &mut random(&mut n));
        assert_eq!(came.acts, [Act::Carry { number: 3, command: Command::Volume { to: 0.5 }, ready: Ready::Nothing }]);
    }

    #[test]
    fn what_counts_in_the_order_of_choices_is_told_as_it_happens() {
        let mut n = 0;
        let (mut phone, session, place) = phone(&mut n);
        let heard = |frames| FromRelay::Heard { places: vec![text::place(&place), text::place(&[1; 16])], frames };
        let pause = |n: u8, generation: u64| from_pc(n, &place, Ask::Do { command: Command::Pause, at: At { generation, minute: 0 } });
        // A press on the phone itself: a command made before it is refused.
        // (It is the person's, and on what plays, unless said: the queue is the phone's own.)
        assert_eq!(phone.handle(r#"{"event":"changed"}"#, 2_000, 100, &mut random(&mut n)).0, r#"[{"act":"claim"}]"#);
        // What the app does by itself, and what only changes how it sounds, are counted
        // and claim nothing.
        for (event, generation) in [(r#"{"event":"changed","who":"nobody"}"#, 3), (r#"{"event":"changed","what":"manner"}"#, 4), (r#"{"event":"changed","who":"guest","what":"manner"}"#, 5)] {
            assert_eq!((phone.handle(event, 2_000, 100, &mut random(&mut n)).0.as_str(), phone.player.rule.generation()), ("[]", generation), "{event}");
        }
        let came = phone.from_relay(session, heard(vec![pause(1, 1)]), 2_100, 100, &mut random(&mut n));
        assert!(came.acts.is_empty() && came.native.iter().any(|ask| matches!(ask, Do::Log(line) if line.contains("refused, Changed"))));
        // Something chosen is being looked up: busy until it is said to be over.
        let (acts, _) = phone.handle(r#"{"event":"looking_up"}"#, 2_200, 100, &mut random(&mut n));
        assert_eq!(acts, r#"[{"act":"ticket","ticket":1}]"#);
        let came = phone.from_relay(session, heard(vec![pause(2, 5)]), 2_300, 100, &mut random(&mut n));
        assert!(came.native.iter().any(|ask| matches!(ask, Do::Log(line) if line.contains("refused, Busy"))));
        phone.handle(r#"{"event":"looked_up","ticket":1}"#, 2_400, 100, &mut random(&mut n));
        let came = phone.from_relay(session, heard(vec![pause(3, 5)]), 2_500, 100, &mut random(&mut n));
        assert_eq!(came.acts, [Act::Claim, Act::Carry { number: 1, command: Command::Pause, ready: Ready::Nothing }]);
        // Carrying it out was counted when it was decided: the phone says what its player
        // holds afterwards, and that is not another change.
        assert_eq!(phone.player.rule.generation(), 6);
    }

    #[test]
    fn look_ups_that_are_never_said_to_be_over_do_not_pile_up() {
        let mut n = 0;
        let (mut phone, ..) = phone(&mut n);
        // A thousand begun inside a second, none ended: sixteen are kept, the latest.
        for at in 0..1_000u64 {
            phone.handle(r#"{"event":"looking_up"}"#, 5_000 + at, 100, &mut random(&mut n));
        }
        assert_eq!((phone.player.waits.len(), phone.player.waits[0].0, phone.player.rule.busy(6_000)), (MOST_LOOK_UPS, 985, true));
        // The latest can still be ended by its ticket; one that was dropped is not known.
        phone.handle(r#"{"event":"looked_up","ticket":1000}"#, 6_000, 100, &mut random(&mut n));
        phone.handle(r#"{"event":"looked_up","ticket":3}"#, 6_000, 100, &mut random(&mut n));
        assert_eq!(phone.player.waits.len(), MOST_LOOK_UPS - 1);
        // A minute on, none of them counts, and the next to begin clears them away.
        assert!(!phone.player.rule.busy(5_999 + LOOK_UP_MS));
        phone.handle(r#"{"event":"looking_up"}"#, 6_000 + LOOK_UP_MS, 100, &mut random(&mut n));
        assert_eq!((phone.player.waits.len(), phone.player.rule.busy(6_001 + LOOK_UP_MS)), (1, true));
    }

    #[test]
    fn a_press_is_the_phone_s_own_unless_another_device_is_being_steered() {
        let mut n = 0;
        let (mut phone, ..) = phone(&mut n);
        for event in [r#"{"event":"press","press":"next"}"#, r#"{"event":"press","press":{"seek":12.5}}"#, r#"{"event":"add","ids":["s1"],"next":false}"#, r#"{"event":"play","ids":["s1"],"start":0,"context":""}"#, r#"{"event":"play_here"}"#] {
            let (acts, native) = phone.handle(event, 3_000, 100, &mut random(&mut n));
            assert_eq!((acts.as_str(), native.len()), (r#"[{"act":"handled","by_another":false}]"#, 0), "{event}");
        }
        // What cannot be read does nothing; and each event that takes nothing is read.
        assert_eq!(phone.handle("not json", 3_000, 100, &mut random(&mut n)), ("[]".to_string(), Vec::new()));
        assert_eq!(phone.handle(r#"{"event":"press","press":"somersault"}"#, 3_000, 100, &mut random(&mut n)).0, "[]");
        for event in [r#"{"event":"let_go"}"#, r#"{"event":"reset"}"#, r#"{"event":"setting","on":true}"#, r#"{"event":"choose","place":"nowhere","in_a_jam":false}"#, r#"{"event":"pause","place":"nowhere"}"#] {
            assert!(serde_json::from_str::<Event>(event).is_ok(), "{event}");
        }
        // Signing out ends the session: that is for the Rust side to act on.
        let (_, native) = phone.handle(r#"{"event":"stop"}"#, 3_100, 100, &mut random(&mut n));
        assert!(native.iter().any(|ask| matches!(ask, Do::End { .. })));
        // It calls itself what it is.
        let came = phone.from_relay(0, FromRelay::NoRandom, 3_200, 100, &mut random(&mut n));
        let (shown, _) = phone.device.shown(3_200);
        assert_eq!((shown.note.as_str(), came.acts.len()), ("This phone couldn't make random numbers, so linking is off.", 0));
    }

    #[test]
    fn what_arrives_before_the_player_has_caught_up_is_decided_when_it_has() {
        let mut n = 0;
        let (mut phone, session, place) = phone(&mut n);
        let heard = |frames| FromRelay::Heard { places: vec![text::place(&place), text::place(&[1; 16])], frames };
        let remove = |n: u8, id: &str, generation: u64| from_pc(n, &place, Ask::Do { command: Command::Remove { at: 1, id: id.into() }, at: At { generation, minute: 0 } });
        let ids = |from: &[usize]| from.iter().map(|n| format!("s{n}")).collect::<Vec<_>>();
        // The first of what is to come is taken out; and, before the player has said it
        // did that, the next: which is kept.
        let came = phone.from_relay(session, heard(vec![remove(1, "s1", 1)]), 2_000, 100, &mut random(&mut n));
        assert_eq!(came.acts, [Act::Claim, Act::Carry { number: 1, command: Command::Remove { at: 1, id: "s1".into() }, ready: Ready::Entry(0) }]);
        let came = phone.from_relay(session, heard(vec![remove(2, "s2", 2)]), 2_010, 100, &mut random(&mut n));
        assert_eq!(came, Came::default());
        // What the player held before it did the first is not how it is.
        assert_eq!(phone.handle(&playing(&ids(&[0, 1, 2, 3]), 0).to_string(), 2_020, 100, &mut random(&mut n)).0, "[]");
        // It says it has: and the second finds the queue as the first left it. (Against
        // the queue as it was, it would have taken out the song after the one meant.)
        let (acts, native) = phone.handle(&playing(&ids(&[0, 2, 3]), 1).to_string(), 2_030, 100, &mut random(&mut n));
        assert_eq!(acts, r#"[{"act":"claim"},{"act":"carry","number":2,"command":{"do":"remove","with":{"at":1,"id":"s2"}},"ready":{"entry":0}}]"#);
        assert!(native.iter().any(|ask| matches!(ask, Do::Send { .. })));
        // A player that never says so holds nothing up for long: at two seconds what was
        // kept is decided against what it last said.
        let came = phone.from_relay(session, heard(vec![remove(3, "s3", 3)]), 2_100, 100, &mut random(&mut n));
        assert_eq!(came, Came::default());
        let tick = r#"{"event":"tick","account":"the account","wanted":true}"#;
        assert!(!phone.handle(tick, 3_000, 100, &mut random(&mut n)).0.contains("carry"));
        let (acts, native) = phone.handle(tick, 4_040, 100, &mut random(&mut n));
        assert!(acts.contains(r#"{"act":"carry","number":3,"command":{"do":"remove","with":{"at":1,"id":"s3"}},"ready":{"entry":1}}"#), "{acts}");
        assert!(native.iter().any(|ask| matches!(ask, Do::Log(line) if line.contains("has not said it did"))));
    }

    /// Two phones of one account in one room, each with a few lines for a player that does
    /// what it is asked and says so; what one sends, the other hears.
    struct Pair {
        phones: [Phone; 2],
        sessions: [u64; 2],
        places: [[u8; 16]; 2],
        /// What each player holds: its songs, the first of them playing or not, and how
        /// far its sound has got.
        songs: [Vec<String>; 2],
        sounds: [Option<f64>; 2],
        acts: [Vec<Act>; 2],
        now: u64,
        n: u64,
    }

    impl Pair {
        fn new() -> Pair {
            let (mut n, mut sessions) = (0, [0; 2]);
            let places = [[5; 16], [6; 16]];
            let phones = [0, 1].map(|who| {
                let mut phone = Phone::new([who as u8 + 1; 16], ["Phone A", "Phone B"][who], Vec::new());
                let (_, native) = phone.handle(r#"{"event":"tick","account":"the account","wanted":true}"#, 1_000, 100, &mut random(&mut n));
                let Some(Do::Look { number, .. }) = native.first().cloned() else { panic!("no look") };
                let came = phone.looked(number, 1_000, Looked::Secret { secret: Secret::new([7; 32]), id: "k".into(), wrote: None }, false, 1_000, 100, &mut random(&mut n));
                let Some(Do::Begin { session, .. }) = came.native.iter().find(|ask| matches!(ask, Do::Begin { .. })).cloned() else { panic!("no session") };
                phone.from_relay(session, FromRelay::Entered { place: places[who], token: "a-token".into() }, 1_000, 100, &mut random(&mut n));
                let ids: Vec<String> = ["a0", "a1", "a2", "s0", "s1", "s2", "s3"].iter().map(|id| id.to_string()).collect();
                phone.happened(Event::Library { ids }, 1_000, 100, &mut random(&mut n));
                sessions[who] = session;
                phone
            });
            let songs = [["a0", "a1", "a2"].map(String::from).to_vec(), ["s0", "s1", "s2", "s3"].map(String::from).to_vec()];
            let mut pair = Pair { phones, sessions, places, songs, sounds: [None, Some(10.0)], acts: [Vec::new(), Vec::new()], now: 1_000, n };
            for who in [0, 1] {
                pair.report(who, 0);
                let room = FromRelay::Heard { places: places.iter().map(text::place).collect(), frames: Vec::new() };
                let came = pair.phones[who].from_relay(sessions[who], room, 1_000, 100, &mut random(&mut pair.n));
                pair.came(who, came);
            }
            pair.pass(2_000);
            pair
        }

        /// The player says what it holds, having done every act up to this one.
        fn report(&mut self, who: usize, after: u64) {
            let songs = &self.songs[who];
            let entry = |id: &String| super::super::message::Entry { id: id.clone(), title: id.clone(), ..Default::default() };
            let playing = Playing {
                brief: Brief { playing: self.sounds[who].is_some(), title: songs.first().cloned().unwrap_or_default(), artist: String::new() },
                full: Full::fitted(self.sounds[who].unwrap_or(0.0), songs.first().map(entry), songs.iter().skip(1).map(entry), "", false, 0, 1.0),
                handed: Handed::fitted(songs, 0, self.sounds[who].unwrap_or(0.0), "the queue"),
                ahead: songs.iter().skip(1).cloned().collect(),
                has_song: !songs.is_empty(),
                sounding_at: self.sounds[who],
            };
            let came = self.phones[who].happened(Event::Player { playing, after }, self.now, 100, &mut random(&mut self.n));
            self.came(who, came);
        }

        fn event(&mut self, who: usize, event: &str) -> String {
            let event = serde_json::from_str::<Event>(event).expect("an event");
            let came = self.phones[who].happened(event, self.now, 100, &mut random(&mut self.n));
            let acts = serde_json::to_string(&came.acts).unwrap();
            self.came(who, came);
            acts
        }

        /// Does what a phone was asked: its player's part, and carrying what it sends.
        fn came(&mut self, who: usize, came: Came) {
            for act in came.acts {
                let done = match &act {
                    Act::Carry { number, command, ready } => {
                        match (command, ready) {
                            (Command::Pause, _) => self.sounds[who] = None,
                            (Command::Play, _) => self.sounds[who] = Some(0.0),
                            (Command::Next, _) if self.songs[who].len() > 1 => drop(self.songs[who].remove(0)),
                            (Command::Adopt(adopt), Ready::Queue(songs, begin)) => {
                                self.songs[who] = songs.iter().skip(*begin).map(|(id, _)| id.clone()).collect();
                                self.sounds[who] = Some(adopt.position);
                            }
                            _ => {}
                        }
                        Some(*number)
                    }
                    Act::Pause { number } => {
                        self.sounds[who] = None;
                        Some(*number)
                    }
                    Act::Take { number, songs, begin, position, .. } => {
                        self.songs[who] = songs.iter().skip(*begin).map(|(id, _)| id.clone()).collect();
                        self.sounds[who] = Some(*position);
                        Some(*number)
                    }
                    _ => None,
                };
                self.acts[who].push(act);
                if let Some(number) = done {
                    self.report(who, number);
                }
            }
            for ask in came.native {
                match ask {
                    Do::Send { session, to, frame, .. } if session == self.sessions[who] && to == self.places[1 - who] => {
                        let heard = FromRelay::Heard { places: self.places.iter().map(text::place).collect(), frames: vec![(text::place(&self.places[who]), frame)] };
                        let came = self.phones[1 - who].from_relay(self.sessions[1 - who], heard, self.now, 100, &mut random(&mut self.n));
                        self.came(1 - who, came);
                    }
                    Do::Find { ticket, ids } => {
                        let came = self.phones[who].found(ticket, ids, self.now, 100, &mut random(&mut self.n));
                        self.came(who, came);
                    }
                    _ => {}
                }
            }
        }

        /// Time passes, a quarter of a second at a tick; what is sounding goes on sounding.
        fn pass(&mut self, ms: u64) {
            let until = self.now + ms;
            while self.now < until {
                self.now += 250;
                for who in [0, 1] {
                    if let Some(at) = &mut self.sounds[who] {
                        *at += 0.25;
                        let after = self.phones[who].player.asked;
                        self.report(who, after);
                    }
                    self.event(who, r#"{"event":"tick","account":"the account","wanted":true}"#);
                }
            }
        }

        fn said(&self, who: usize) -> Vec<&str> {
            self.acts[who].iter().filter_map(|act| if let Act::Do { ask: Do::Say(words) } = act { Some(words.as_str()) } else { None }).collect()
        }

        fn carried(&self, who: usize) -> Vec<&str> {
            self.acts[who].iter().filter_map(|act| if let Act::Carry { command, .. } = act { Some(command.name()) } else { None }).collect()
        }
    }

    #[test]
    fn one_phone_steers_another_and_brings_its_music_over() {
        let mut pair = Pair::new();
        // Each lists the other, with what it is doing.
        let shown = pair.event(0, r#"{"event":"tick","account":"the account","wanted":true}"#);
        assert!(shown.contains(r#""name":"Phone B","about":"Playing s0","playing":true,"active":false,"ready":true"#), "{shown}");
        // A, which is playing nothing, picks B: its player is B's now, and shows what B plays.
        let place = text::place(&pair.places[1]);
        let acts = pair.event(0, &serde_json::json!({"event": "choose", "place": place, "in_a_jam": false}).to_string());
        assert!(acts.contains(r#"{"act":"do","ask":{"steering":"Phone B"}}"#), "{acts}");
        pair.pass(500);
        assert!(pair.acts[0].iter().any(|act| matches!(act, Act::View { view: Some(view) } if view.playing && view.full.now.as_ref().is_some_and(|now| now.id == "s0"))));
        // Next, pressed on A, is B's to do; and A's own player is asked nothing.
        let acts = pair.event(0, r#"{"event":"press","press":"next"}"#);
        assert!(acts.starts_with(r#"[{"act":"view","view":{"full":{"#) && acts.ends_with(r#"{"act":"handled","by_another":true}]"#), "{acts}");
        pair.pass(500);
        assert_eq!((pair.carried(1), pair.carried(0), pair.songs[1][0].as_str()), (vec!["next"], vec![], "s1"));
        // "Play here": A takes B's queue as data, and when A's sound has started, B is paused.
        assert!(pair.event(0, r#"{"event":"play_here"}"#).ends_with(r#"{"act":"handled","by_another":true}]"#));
        pair.pass(1_500);
        assert!(pair.acts[0].iter().any(|act| matches!(act, Act::Take { songs, begin: 0, .. } if songs.len() == 3 && songs[0].0 == "s1")), "{:?}", pair.acts[0]);
        assert_eq!((pair.said(0).last().copied(), pair.carried(1), pair.sounds[1]), (Some("Playing here. Phone B is paused."), vec!["next", "pause"], None));
        assert!(pair.acts[0].contains(&Act::Do { ask: Do::Steering(None) }));
        // And given back: A, playing, picks B, which takes the queue and plays; A is stopped
        // when B's sound has started.
        pair.event(0, &serde_json::json!({"event": "choose", "place": place, "in_a_jam": false}).to_string());
        pair.pass(1_500);
        assert_eq!((pair.said(0).last().copied(), pair.carried(1), pair.sounds[0]), (Some("Playing on Phone B."), vec!["next", "pause", "adopt"], None));
        assert!(matches!(pair.acts[0].iter().rev().find(|act| matches!(act, Act::Pause { .. } | Act::Take { .. })), Some(Act::Pause { .. })));
    }
}
