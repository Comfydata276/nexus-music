//! The surface an app whose player and library live outside Rust (the phone) calls.
//!
//! **The phone's player is asked, and has answered when the call returns**, exactly as the
//! Windows app's is. The phone's player lives on one thread (Media3's application thread,
//! the main one), and so does the deciding: a frame arrives on a network thread, is handed
//! to that thread, and there the library opens it, decides, and calls up into the app
//! ([`PhonePlayer`]: what is sounding, carry out this command, load this queue), with the
//! result in its hand before it answers. There is nothing to wait for, so there is nothing
//! to give up, cancel or number.
//!
//! - **[`PhonePlayer`] is what the app supplies:** the questions and the acts of
//!   [`Player`], which the Windows app answers too. The order of choices
//!   ([`super::rule::Rule`]) and who chose what ([`crate::ways`]) are kept here for the
//!   phone, since nothing of them is the player's own;
//! - **[`Phone::happened`] is what the app calls,** on that thread, with each thing that
//!   happens: what the person presses, a way into the player being taken
//!   ([`Event::Changed`]: the same places the Windows app counts, listed in
//!   `docs/linking-design.md`, 5.6.1), and what the requests brought back;
//! - **what to show comes back as data** ([`Act`]): the list, the device being steered,
//!   what to say.
//!
//! **A player tells its listeners of a change before the call that made it returns**, and
//! the app's listener would tell this library. The rule for that is `super::turn`'s, and
//! [`Phone::happened`] keeps it: a change heard while the library is itself changing the
//! player is that act and is dropped ([`Answer::PartOfIt`]); anything else that waits for
//! no answer is kept and decided when the call returns, in the order it arrived
//! ([`Answer::Kept`]); a press is refused ([`Answer::Refused`]). An app need do nothing
//! about it but call, from wherever it hears.
//!
//! **What does not cross, and stays on the threads it was on.** The look, the stay at the
//! relay, each send and each look-up of songs are requests, made by `super::look::look`,
//! `super::relay::keep_place` and `super::relay::send` over the `Http` the phone's native
//! side already gives this library, on threads that side owns. It is told to start them by
//! [`Do::Look`], [`Do::Begin`], [`Do::Send`] and [`Do::Find`] ([`Came::native`]), and hands
//! what they bring to the player's thread ([`Happened::Looked`], [`Happened::FromRelay`],
//! [`Happened::Found`]). The secret never leaves Rust. The phone's copy of the library is
//! made ready on whichever thread it is read on ([`Songs::of`]) and only put in place here.
//!
//! **A sign-out, or another account, replaces everything**: the device, the order of
//! choices, the look-ups under way, the copy of the library. Nothing of the last sign-in's
//! is asked again. (Only the numbers that looks, sessions and tickets had reached carry
//! over, so that an answer still on its way to the last sign-in matches nothing in the next.)

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::command::Command;
use super::device::{Brief, Device, Do, Env, Me, Player, Ready, Shown};
use super::look::Looked;
use super::message::{At, Full, Handed, Why};
use super::relay::FromRelay;
use super::rule::{Carrying, Reservation, Rule, Waiting};
use super::seen::Seen;
use super::turn::{Entered, Kind, Turns};
use super::view::{Press, View};
use super::words::Here;
use super::{DeviceId, Nonce};
use crate::requests::{AlbumRef, Library, SongRef};
use crate::ways::{Ways, What, Who};

/// The phone's own player, as the app answers for it. **Every call is made on the thread
/// the player lives on, from inside [`Phone::happened`], and is answered before it
/// returns.** A call that cannot be answered (the app threw) is answered as nothing: no
/// song, not sounding, changed nothing.
pub trait PhonePlayer {
    fn brief(&self) -> Brief;
    /// What a device steering this one shows.
    fn full(&self) -> Full;
    /// What is playing and to come, by id, the song that is playing first; nothing, if
    /// what is playing is not something another device could carry on with (a radio).
    fn handed(&self) -> Handed;
    /// Where the sound has got to in the song the player holds, in seconds, if it is
    /// playing and the sound has reached that song.
    fn sounding_at(&self) -> Option<f64>;
    /// Whether there is a song in the player at all.
    fn has_song(&self) -> bool;
    /// The ids of the songs to come, in the order they would play.
    fn ahead(&self) -> Vec<String>;
    /// Carries a command out, exactly as the same press on the phone would be, **and says
    /// whether it changed anything**: a Play when it was playing, a seek on a radio, a Next
    /// with nothing to go to, change nothing. It is **not** an [`Event::Changed`]: it is
    /// counted here, once, if it changed anything, and whatever the player's listeners say
    /// of it meanwhile is dropped.
    fn carry_out(&mut self, command: &Command, ready: &Ready) -> bool;
    /// Stops the phone's own sound. Counted here already.
    fn pause(&mut self);
    /// Takes this queue and plays it: each song and whether it was queued by hand, where to
    /// start, and how far in. Counted here already.
    fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, context: &str);
    /// The queue is the phone's own from this moment, by the person's own doing here or at
    /// another of their devices: it is no longer one taken up from another device
    /// (`handover::Here::taken`), it changed now by the phone's clock
    /// (`handover::Here::stamp`), and it is to be saved. Said after the change it is for
    /// is in the player.
    fn claim(&mut self);
}

/// Something that happened on the phone, told to the linking.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A few times a second while the app runs. `account` names the sign-in; `wanted` is
    /// signed in, a relay set up and "Link my devices" on.
    Tick { account: String, wanted: bool },
    /// A way into the player was taken (`crate::ways`): `who` chose it (`here`, unless
    /// said: a press, a key, a notification's button, a headset's, the car's; `nobody` for
    /// what the app does by itself, as the next song starting; `guest`) and `what` it
    /// touches (`playing`, unless said; `manner` for the volume, repeat, a sleep timer
    /// being set). One for each choice: what the player does on the way is part of it.
    /// The order of choices moves on, and if the queue is the phone's own by it,
    /// [`PhonePlayer::claim`] is called.
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
    /// The phone was given another name.
    Named { name: String },
    /// Signed out, or the app is closing.
    Stop,
    /// For a test only: carry on with a command that was held ([`Do::Later`]).
    Resume { ticket: u64 },
}

/// Anything the linking is told: what happened on the phone, and what the requests it
/// asked for brought back, handed to the player's thread.
pub enum Happened {
    Event(Event),
    /// A look has finished ([`Do::Look`]). `since_ms` is when it began.
    Looked { number: u64, since_ms: u64, looked: Looked, reset: bool },
    /// The stay at the relay has something to say ([`Do::Begin`]).
    FromRelay { session: u64, from: FromRelay },
    /// The songs the music server has, of those a command brought ([`Do::Find`]).
    Found { ticket: u64, songs: Vec<String> },
    /// The phone's copy of the library changed, or was read for a new sign-in.
    Library(Arc<Songs>),
}

impl Happened {
    /// What it is to a call it arrives inside (`super::turn`).
    fn kind(&self) -> Kind {
        match self {
            Happened::Event(Event::Changed { .. }) => Kind::Change,
            Happened::Event(Event::Press { .. } | Event::Add { .. } | Event::Play { .. } | Event::PlayHere | Event::LookingUp) => Kind::Question,
            _ => Kind::News,
        }
    }
}

/// What the phone is asked to do, or to show.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "act", rename_all = "snake_case")]
pub enum Act {
    /// Something the deciding asked for that Kotlin does: say this, keep this list of
    /// devices seen, forget this page, the window is another device's or the phone's again.
    Do { ask: Do },
    /// The list, after a tick.
    Shown { shown: Shown },
    /// The device being steered, as the player shows it; nothing, if none is.
    View { view: Option<View> },
    /// Whether what was pressed was asked of another device (and so is not the phone's own).
    Handled { by_another: bool },
    /// The ticket for a look-up that has begun.
    Ticket { ticket: u64 },
}

/// The ids of the songs in the phone's copy of the library. Made ready where the copy is
/// read ([`Songs::of`]), on whichever thread that is, so that asking it costs the player's
/// thread a look-up for each song asked about and nothing for the songs it has.
#[derive(Debug, Default)]
pub struct Songs(HashSet<String>);

impl Songs {
    pub fn of(ids: impl IntoIterator<Item = String>) -> Arc<Songs> {
        Arc::new(Songs(ids.into_iter().collect()))
    }
}

impl Library for Songs {
    fn albums(&self) -> Box<dyn Iterator<Item = AlbumRef<'_>> + '_> {
        Box::new(std::iter::empty())
    }
    fn songs(&self) -> Box<dyn Iterator<Item = SongRef<'_>> + '_> {
        Box::new(std::iter::empty())
    }
    fn artists(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        Box::new(std::iter::empty())
    }
    fn has_songs(&self, ids: &[String]) -> Vec<bool> {
        ids.iter().map(|id| self.0.contains(id)).collect()
    }
}

fn here() -> Who {
    Who::Here
}

fn playing() -> What {
    What::Playing
}

/// The phone's player as the deciding asks it: the app's answers, with the order of
/// choices and whose the queue is kept beside them.
struct Held<'a> {
    rule: &'a mut Rule,
    ways: &'a mut Ways,
    theirs: &'a mut dyn PhonePlayer,
    turns: &'a Turns<Happened>,
}

impl Player for Held<'_> {
    fn brief(&self) -> Brief {
        self.theirs.brief()
    }
    fn full(&self) -> Full {
        self.theirs.full()
    }
    fn handed(&self) -> Handed {
        self.theirs.handed()
    }
    fn sounding_at(&self) -> Option<f64> {
        self.theirs.sounding_at()
    }
    /// (The phone says the next song has started as its player says so: there is no
    /// window here that lags the sound for this to bring up.)
    fn catch_up(&mut self) -> bool {
        false
    }
    fn has_song(&self) -> bool {
        self.theirs.has_song()
    }
    fn ahead(&self) -> Vec<String> {
        self.theirs.ahead()
    }
    /// The person chose it at another of their devices (but for the stop that ends a move,
    /// which is nobody's): the queue is theirs by it, if it changed what a saved queue
    /// holds. The rule counts it.
    fn carry_out(&mut self, command: &Command, ready: &Ready) -> bool {
        self.ways.begin(command.chosen_by(), true);
        let changed = {
            let _changing = self.turns.changing();
            self.theirs.carry_out(command, ready)
        };
        if changed && self.ways.touch(Who::Here, command.touches()).claim {
            self.theirs.claim();
        }
        self.ways.end();
        changed
    }
    /// (The end of a move of the phone's music to another device, or of one that did not
    /// start: the app's own doing, which claims nothing.)
    fn pause(&mut self) {
        if self.ways.touch(Who::Nobody, What::Playing).count {
            self.rule.moved();
        }
        let _changing = self.turns.changing();
        self.theirs.pause();
    }
    /// ("Play here": the person's own choice, at the phone. The queue is the phone's own
    /// once it is in the player.)
    fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, context: &str) {
        let owed = self.ways.touch(Who::Here, What::Playing);
        if owed.count {
            self.rule.moved();
        }
        {
            let _changing = self.turns.changing();
            self.theirs.take(songs, begin, position, context);
        }
        if owed.claim {
            self.theirs.claim();
        }
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
    fn finish(&mut self, reservation: Reservation, at: At, minute: u64, now_ms: u64, same_session: bool) -> Result<Carrying, Why> {
        self.rule.finish(reservation, at, minute, now_ms, same_session)
    }
    fn carried(&mut self, carrying: Carrying, changed: bool, nonce: Nonce) {
        self.rule.carried(carrying, changed, nonce)
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

/// What came of something that happened: what crosses to Kotlin, and what the Rust side of
/// the phone's app does itself (the requests).
#[derive(Debug, Default, PartialEq)]
pub struct Came {
    pub acts: Vec<Act>,
    /// [`Do::Look`], [`Do::StopLook`], [`Do::Begin`], [`Do::Renew`], [`Do::End`],
    /// [`Do::Send`], [`Do::Find`], [`Do::Later`] and [`Do::Log`].
    pub native: Vec<Do>,
}

/// What a call was answered with.
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// It was decided, with whatever arrived while it was (in the order it arrived): this
    /// came of all of it.
    Came(Came),
    /// The library is inside a call (this arrived from one of the app's listeners, while
    /// the player was being asked): kept, and decided when that call returns. What comes of
    /// it is in that call's answer.
    Kept,
    /// The library is changing the player and this is that change, heard back: dropped.
    PartOfIt,
    /// The library is inside a call and this needs its answer now: nothing was done. (A
    /// press never comes from a player's listener. One that does is the app's mistake.)
    Refused,
}

/// Everything of one sign-in's.
struct Inner {
    device: Device,
    rule: Rule,
    ways: Ways,
    /// The look-ups under way: each one's ticket, when it began, and its place in the
    /// order of choices. No more than [`MOST_LOOK_UPS`], and none older than its minute.
    waits: Vec<(u64, u64, Waiting)>,
    tickets: u64,
    library: Arc<Songs>,
    me: Me,
    /// The sign-in all of this belongs to, once a tick has said.
    account: Option<String>,
}

/// The linking, for an app whose player and library are not in Rust. It belongs to the
/// thread the player lives on, and is called from nowhere else.
pub struct Phone {
    turns: Turns<Happened>,
    inner: RefCell<Inner>,
}

impl Inner {
    /// Hands the deciding something, with the player to ask, and sorts what it asks for.
    #[allow(clippy::too_many_arguments)]
    fn with(&mut self, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, theirs: &mut dyn PhonePlayer, turns: &Turns<Happened>, came: &mut Came, act: impl FnOnce(&mut Device, &mut Env) -> Vec<Do>) {
        let Inner { device, rule, ways, library, me, .. } = self;
        let mut player = Held { rule, ways, theirs, turns };
        let asked = act(device, &mut Env { now_ms, today, me, player: &mut player, library: &**library, random });
        for ask in asked {
            match ask {
                Do::Say(_) | Do::Remember(_) | Do::Forget { .. } | Do::Steering(_) => came.acts.push(Act::Do { ask }),
                Do::Draw => came.acts.push(Act::View { view: self.device.view().cloned() }),
                native => came.native.push(native),
            }
        }
    }

    /// Something was pressed that may be for a device being steered: asked of that device
    /// ([`Act::Handled`] says so), or the phone's own.
    #[allow(clippy::too_many_arguments)]
    fn pressed(&mut self, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, theirs: &mut dyn PhonePlayer, turns: &Turns<Happened>, came: &mut Came, press: impl FnOnce(&mut Device, &mut Env) -> Option<Vec<Do>>) {
        let mut by_another = false;
        self.with(now_ms, today, random, theirs, turns, came, |device, env| {
            let asked = press(device, env);
            by_another = asked.is_some();
            asked.unwrap_or_default()
        });
        came.acts.push(Act::Handled { by_another });
    }

    /// The sign-in is over: what was under way under it is ended, and then **everything
    /// is replaced**. Nothing of the last sign-in's is asked again.
    fn sign_in_over(&mut self, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, theirs: &mut dyn PhonePlayer, turns: &Turns<Happened>, came: &mut Came) {
        self.with(now_ms, today, random, theirs, turns, came, |device, env| device.stop(env));
        *self = Inner { device: self.device.succeed(), rule: Rule::default(), ways: Ways::default(), waits: Vec::new(), tickets: self.tickets, library: Arc::default(), me: self.me.clone(), account: None };
    }

    /// One thing that happened, decided.
    #[allow(clippy::too_many_arguments)]
    fn one(&mut self, what: Happened, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, theirs: &mut dyn PhonePlayer, turns: &Turns<Happened>, came: &mut Came) {
        let event = match what {
            Happened::Event(event) => event,
            Happened::Looked { number, since_ms, looked, reset } => return self.with(now_ms, today, random, theirs, turns, came, |device, env| device.looked(number, since_ms, looked, reset, env)),
            Happened::FromRelay { session, from } => {
                return match from {
                    FromRelay::Says(note) => self.device.said(session, note),
                    FromRelay::NoRandom => self.with(now_ms, today, random, theirs, turns, came, |device, env| device.out_of_random(env)),
                    FromRelay::Entered { place, token } => self.device.entered(session, place, token),
                    FromRelay::Heard { places, frames } => self.with(now_ms, today, random, theirs, turns, came, |device, env| device.heard(session, &places, &frames, env)),
                    FromRelay::Left => self.device.left(session),
                };
            }
            Happened::Found { ticket, songs } => return self.with(now_ms, today, random, theirs, turns, came, |device, env| device.found(ticket, songs, env)),
            Happened::Library(songs) => {
                self.library = songs;
                return;
            }
        };
        match event {
            Event::Tick { account, wanted } => {
                // Another account: the last one's is over before anything of this one's begins.
                if self.account.as_ref().is_some_and(|was| *was != account) {
                    self.sign_in_over(now_ms, today, &mut *random, &mut *theirs, turns, came);
                }
                self.account = Some(account.clone());
                self.with(now_ms, today, random, theirs, turns, came, |device, env| device.tick(&account, wanted, env));
                let (shown, logged) = self.device.shown(now_ms);
                came.native.extend(logged);
                came.acts.push(Act::Shown { shown });
            }
            Event::Changed { who, what } => {
                let owed = self.ways.touch(who, what);
                if owed.count {
                    self.rule.moved();
                }
                if owed.claim {
                    theirs.claim();
                }
            }
            Event::LookingUp => {
                // (One whose minute is up counts for nothing already, and one that was
                // never said to be over is not kept for ever: the list has a bound.)
                while let Some(at) = self.waits.iter().position(|(_, began, _)| now_ms.saturating_sub(*began) >= LOOK_UP_MS).or((self.waits.len() >= MOST_LOOK_UPS).then_some(0)) {
                    let (.., waiting) = self.waits.remove(at);
                    self.rule.done(waiting);
                }
                self.tickets = self.tickets.wrapping_add(1);
                let waiting = self.rule.wait(now_ms, LOOK_UP_MS);
                self.waits.push((self.tickets, now_ms, waiting));
                came.acts.push(Act::Ticket { ticket: self.tickets });
            }
            Event::LookedUp { ticket } => {
                if let Some(at) = self.waits.iter().position(|(known, ..)| *known == ticket) {
                    let (.., waiting) = self.waits.remove(at);
                    self.rule.done(waiting);
                }
            }
            Event::Press { press } => self.pressed(now_ms, today, random, theirs, turns, came, |device, env| device.press(press, env)),
            Event::Add { ids, next } => self.pressed(now_ms, today, random, theirs, turns, came, |device, env| device.add(&ids, next, env)),
            Event::Play { ids, start, context } => self.pressed(now_ms, today, random, theirs, turns, came, |device, env| device.play(&ids, start, &context, env)),
            Event::PlayHere => self.pressed(now_ms, today, random, theirs, turns, came, |device, env| device.play_here(env)),
            Event::Choose { place, in_a_jam } => self.with(now_ms, today, random, theirs, turns, came, |device, env| device.choose(&place, in_a_jam, env)),
            Event::Pause { place } => self.with(now_ms, today, random, theirs, turns, came, |device, env| device.pause(&place, env)),
            Event::LetGo => self.with(now_ms, today, random, theirs, turns, came, |device, _| device.let_go()),
            Event::Reset => self.with(now_ms, today, random, theirs, turns, came, |device, env| device.reset(env)),
            Event::Setting { on } => self.with(now_ms, today, random, theirs, turns, came, |device, env| device.setting(on, env)),
            Event::Named { name } => self.me.name = name,
            Event::Stop => self.sign_in_over(now_ms, today, random, theirs, turns, came),
            Event::Resume { ticket } => self.with(now_ms, today, random, theirs, turns, came, |device, env| device.resume(ticket, env)),
        }
    }
}

impl Phone {
    /// `device` is the id made once for the installation; `name` what the person calls the
    /// phone; `remembered` the devices seen before, as last kept ([`Do::Remember`]).
    pub fn new(device: DeviceId, name: &str, remembered: Vec<Seen>) -> Phone {
        let me = Me { device, name: name.to_string(), here: Here::Phone };
        Phone { turns: Turns::default(), inner: RefCell::new(Inner { device: Device::new(remembered), rule: Rule::default(), ways: Ways::default(), waits: Vec::new(), tickets: 0, library: Arc::default(), me, account: None }) }
    }

    /// Something happened. Called on the thread the player lives on; `player` is asked
    /// there, from inside this call. `now_ms` is milliseconds on a clock that goes on
    /// counting while the phone sleeps (`elapsedRealtime`), `today` the day since 1970, and
    /// `random` fills bytes from the system's source of randomness or says it could not.
    ///
    /// **It may be called from inside itself** (the player's listeners, told of a change
    /// while the player is being asked): see [`Answer`].
    pub fn happened(&self, what: Happened, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, player: &mut dyn PhonePlayer) -> Answer {
        let kind = what.kind();
        let mut turn = match self.turns.enter(what, kind) {
            Entered::Mine(turn) => turn,
            Entered::Kept => return Answer::Kept,
            Entered::PartOfIt => return Answer::PartOfIt,
            Entered::Refused => return Answer::Refused,
        };
        // (The turn is this call's, so nothing else is inside.)
        let Ok(mut inner) = self.inner.try_borrow_mut() else { return Answer::Refused };
        let mut came = Came::default();
        while let Some(what) = turn.next() {
            inner.one(what, now_ms, today, &mut *random, &mut *player, &self.turns, &mut came);
        }
        Answer::Came(came)
    }

    /// The same for an [`Event`] as JSON, giving the [`Act`]s as a JSON list and, apart,
    /// what the Rust side does itself. An event that cannot be read does nothing and says
    /// nothing; one that is kept or dropped says nothing here; `None` is [`Answer::Refused`].
    pub fn handle(&self, event: &str, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, player: &mut dyn PhonePlayer) -> Option<(String, Vec<Do>)> {
        let nothing = || Some(("[]".to_string(), Vec::new()));
        let Ok(event) = serde_json::from_str::<Event>(event) else { return nothing() };
        match self.happened(Happened::Event(event), now_ms, today, random, player) {
            Answer::Came(came) => Some((serde_json::to_string(&came.acts).unwrap_or_else(|_| "[]".to_string()), came.native)),
            Answer::Kept | Answer::PartOfIt => nothing(),
            Answer::Refused => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::{self, ASK, Keys};
    use super::super::message::{Ask, Entry, Want, read_tell};
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

    const TICK: &str = r#"{"event":"tick","account":"the account","wanted":true}"#;
    const CHANGED: &str = r#"{"event":"changed"}"#;

    fn ids(from: &[usize]) -> Vec<String> {
        from.iter().map(|n| format!("s{n}")).collect()
    }

    /// What the app's listeners tell the linking of as the player tells them: inside the
    /// call named, which is where a real player tells its listeners of what that call did.
    struct Tells<'p> {
        phone: &'p Phone,
        inside: &'static str,
        what: RefCell<Vec<Happened>>,
        /// What each was answered with.
        answers: RefCell<Vec<&'static str>>,
    }

    /// The linking is inside a call: it asks nothing of whoever arrives meanwhile.
    struct NotAsked;

    impl PhonePlayer for NotAsked {
        fn brief(&self) -> Brief {
            unreachable!("asked from inside a call")
        }
        fn full(&self) -> Full {
            unreachable!("asked from inside a call")
        }
        fn handed(&self) -> Handed {
            unreachable!("asked from inside a call")
        }
        fn sounding_at(&self) -> Option<f64> {
            unreachable!("asked from inside a call")
        }
        fn has_song(&self) -> bool {
            unreachable!("asked from inside a call")
        }
        fn ahead(&self) -> Vec<String> {
            unreachable!("asked from inside a call")
        }
        fn carry_out(&mut self, _: &Command, _: &Ready) -> bool {
            unreachable!("asked from inside a call")
        }
        fn pause(&mut self) {
            unreachable!("asked from inside a call")
        }
        fn take(&mut self, _: &[(String, bool)], _: usize, _: f64, _: &str) {
            unreachable!("asked from inside a call")
        }
        fn claim(&mut self) {
            unreachable!("asked from inside a call")
        }
    }

    /// The phone's player: a few lines that do what they are asked, at once, and tell their
    /// listeners so before they return.
    #[derive(Default)]
    struct Mock<'p> {
        /// What it holds, the first of them the one in the player.
        songs: Vec<String>,
        /// How far its sound has got, if it is sounding.
        sound: Option<f64>,
        volume: f64,
        /// What is playing is a radio station: there is nowhere in it to seek to.
        radio: bool,
        /// What was asked of it, in order.
        asked: Vec<String>,
        tells: Option<Tells<'p>>,
    }

    impl<'p> Mock<'p> {
        fn with(songs: Vec<String>, sound: Option<f64>) -> Mock<'p> {
            Mock { songs, sound, volume: 1.0, ..Mock::default() }
        }

        /// From here its listeners tell the linking of these, inside this call.
        fn tells(&mut self, phone: &'p Phone, inside: &'static str, what: Vec<Happened>) {
            self.tells = Some(Tells { phone, inside, what: RefCell::new(what), answers: RefCell::new(Vec::new()) });
        }

        fn tell(&self, inside: &str) {
            let Some(tells) = self.tells.as_ref().filter(|tells| tells.inside == inside) else { return };
            let what: Vec<Happened> = tells.what.borrow_mut().drain(..).collect();
            for what in what {
                let answer = tells.phone.happened(what, 0, 0, &mut |_| unreachable!("asked from inside a call"), &mut NotAsked);
                tells.answers.borrow_mut().push(match answer {
                    Answer::Came(_) => "decided",
                    Answer::Kept => "kept",
                    Answer::PartOfIt => "part of it",
                    Answer::Refused => "refused",
                });
            }
        }

        /// What its listeners were answered with.
        fn told(&self) -> Vec<&'static str> {
            self.tells.as_ref().map(|tells| tells.answers.borrow().clone()).unwrap_or_default()
        }

        fn claims(&self) -> usize {
            self.asked.iter().filter(|asked| *asked == "claim").count()
        }
    }

    impl PhonePlayer for Mock<'_> {
        fn brief(&self) -> Brief {
            self.tell("brief");
            Brief { playing: self.sound.is_some(), title: self.songs.first().cloned().unwrap_or_default(), artist: String::new() }
        }
        fn full(&self) -> Full {
            let entry = |id: &String| Entry { id: id.clone(), title: id.clone(), ..Default::default() };
            Full::fitted(self.sound.unwrap_or(0.0), self.songs.first().map(entry), self.songs.iter().skip(1).map(entry), "", false, 0, self.volume)
        }
        fn handed(&self) -> Handed {
            if self.radio { Handed::default() } else { Handed::fitted(&self.songs, 0, self.sound.unwrap_or(0.0), "the queue") }
        }
        fn sounding_at(&self) -> Option<f64> {
            self.sound
        }
        fn has_song(&self) -> bool {
            !self.songs.is_empty()
        }
        fn ahead(&self) -> Vec<String> {
            self.songs.iter().skip(1).cloned().collect()
        }
        fn carry_out(&mut self, command: &Command, ready: &Ready) -> bool {
            self.asked.push(command.name().to_string());
            let changed = match (command, ready) {
                (Command::Pause | Command::Moved, _) => self.sound.take().is_some(),
                (Command::Play, _) if self.sound.is_none() => {
                    self.sound = Some(0.0);
                    true
                }
                (Command::Next, _) if self.songs.len() > 1 => {
                    self.songs.rotate_left(1);
                    true
                }
                (Command::Seek { to }, _) if !self.radio => {
                    self.sound = Some(*to);
                    true
                }
                (Command::Volume { to }, _) => std::mem::replace(&mut self.volume, *to) != *to,
                (Command::Remove { .. }, Ready::Entry(entry)) => {
                    self.songs.remove(entry + 1);
                    true
                }
                (Command::Add { .. }, Ready::Songs(songs)) => {
                    self.songs.extend(songs.iter().cloned());
                    true
                }
                (Command::Adopt(adopt), Ready::Queue(songs, begin)) => {
                    self.songs = songs.iter().skip(*begin).map(|(id, _)| id.clone()).collect();
                    self.sound = Some(adopt.position);
                    true
                }
                _ => false,
            };
            if changed {
                self.tell("carry_out");
            }
            changed
        }
        fn pause(&mut self) {
            self.asked.push("pause, its own".into());
            self.sound = None;
            self.tell("pause");
        }
        fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, _context: &str) {
            self.asked.push(format!("take {} from {begin}", songs.len()));
            self.songs = songs.iter().skip(begin).map(|(id, _)| id.clone()).collect();
            self.sound = Some(position);
            self.tell("take");
        }
        fn claim(&mut self) {
            self.asked.push("claim".into());
        }
    }

    fn came(answer: Answer) -> Came {
        match answer {
            Answer::Came(came) => came,
            other => panic!("it was not decided: {other:?}"),
        }
    }

    fn secret() -> Secret {
        Secret::new([7; 32])
    }

    /// The number of a sign-in's first look, on a phone that has had no other sign-in.
    const FIRST_LOOK: u64 = 1;

    /// A phone signed in, in the room, with eight songs in its player, the first playing.
    fn phone(n: &mut u64) -> (Phone, Mock<'static>, u64, [u8; 16]) {
        let phone = Phone::new([9; 16], "Phone", Vec::new());
        let all = ids(&[0, 1, 2, 3, 4, 5, 6, 7]);
        let mut player = Mock::with(all.clone(), Some(3.0));
        let (acts, native) = phone.handle(TICK, 1_000, 100, &mut random(n), &mut player).unwrap();
        assert!(acts.contains(r#""act":"shown""#) && matches!(native.as_slice(), [Do::Look { number: FIRST_LOOK, reset: false, .. }]), "{acts} {native:?}");
        let looked = Happened::Looked { number: FIRST_LOOK, since_ms: 1_000, looked: Looked::Secret { secret: secret(), id: "k".into(), wrote: None }, reset: false };
        let begun = came(phone.happened(looked, 1_100, 100, &mut random(n), &mut player));
        let Some(Do::Begin { session, .. }) = begun.native.iter().find(|ask| matches!(ask, Do::Begin { .. })).cloned() else { panic!("no session: {begun:?}") };
        let place = [5; 16];
        phone.happened(Happened::FromRelay { session, from: FromRelay::Entered { place, token: "a-token".into() } }, 1_200, 100, &mut random(n), &mut player);
        phone.happened(Happened::Library(Songs::of(all)), 1_300, 100, &mut random(n), &mut player);
        (phone, player, session, place)
    }

    /// A frame from another device in the room, sealed for the phone.
    fn from_pc(n: u8, to: &[u8; 16], ask: Ask) -> (String, String) {
        let from = [1; 16];
        (text::place(&from), frame::seal(&Keys::of(&secret()), ASK, &[n; 24], &from, to, ask.text().as_bytes()).unwrap())
    }

    fn command(n: u8, place: &[u8; 16], command: Command, generation: u64) -> (String, String) {
        from_pc(n, place, Ask::Do { command, at: At { generation, minute: 0 } })
    }

    /// The relay hands the phone these frames, on the player's thread.
    fn hear(phone: &Phone, player: &mut Mock, session: u64, place: &[u8; 16], frames: Vec<(String, String)>, now: u64, n: &mut u64) -> Came {
        let from = FromRelay::Heard { places: vec![text::place(place), text::place(&[1; 16])], frames };
        came(phone.happened(Happened::FromRelay { session, from }, now, 100, &mut random(n), player))
    }

    fn sends(native: &[Do]) -> usize {
        native.iter().filter(|ask| matches!(ask, Do::Send { .. })).count()
    }

    fn logged(native: &[Do], has: &str) -> bool {
        native.iter().any(|ask| matches!(ask, Do::Log(line) if line.contains(has)))
    }

    /// The order of choices on the phone: where it has got to, the command that made the
    /// state, and whether the phone is busy.
    fn order(phone: &Phone, now: u64) -> (u64, Option<Nonce>, bool) {
        let inner = phone.inner.borrow();
        (inner.rule.generation(), inner.rule.by(), inner.rule.busy(now))
    }

    #[test]
    fn a_command_is_carried_out_and_answered_before_the_call_that_brought_it_returns() {
        let mut n = 0;
        let (phone, mut player, session, place) = phone(&mut n);
        // Next, from a PC: the rule lets it through, the player is asked and has done it,
        // it is counted, the queue is the phone's by it, and its sender is answered from
        // the player as it now is. All of it in the one call.
        let came = hear(&phone, &mut player, session, &place, vec![command(1, &place, Command::Next, 1)], 2_000, &mut n);
        assert!(came.acts.is_empty() && sends(&came.native) == 1 && logged(&came.native, "next from") && logged(&came.native, "carried out"), "{came:?}");
        assert_eq!((player.asked.as_slice(), player.songs[0].as_str()), (["next".to_string(), "claim".to_string()].as_slice(), "s1"));
        assert_eq!(order(&phone, 2_000), (2, Some([1; 24]), false));
        // The same frame again: refused, and nothing for the player.
        let came = hear(&phone, &mut player, session, &place, vec![command(1, &place, Command::Next, 1)], 2_100, &mut n);
        assert!(came.acts.is_empty() && logged(&came.native, "refused, Changed") && sends(&came.native) == 1 && player.asked.len() == 2);
        // An entry of the queue is found for it, as the queue now stands: the second of
        // two commands finds the queue as the first left it.
        let came = hear(&phone, &mut player, session, &place, vec![command(2, &place, Command::Remove { at: 4, id: "s3".into() }, 2)], 2_200, &mut n);
        assert!(logged(&came.native, "carried out") && !player.songs.contains(&"s3".to_string()) && player.claims() == 2);
        // The volume is how it sounds: done, counted, and the queue no more the phone's for it.
        let came = hear(&phone, &mut player, session, &place, vec![command(3, &place, Command::Volume { to: 0.5 }, 3)], 2_300, &mut n);
        assert_eq!((sends(&came.native), order(&phone, 2_300).0, player.claims(), player.volume), (1, 4, 2, 0.5));
        // The player found it had nothing to do (Play, and it was playing): answered as
        // that, nothing moves, nothing is claimed, and the command made no state.
        let came = hear(&phone, &mut player, session, &place, vec![command(4, &place, Command::Play, 4)], 2_400, &mut n);
        assert!(sends(&came.native) == 1 && logged(&came.native, "nothing to do"));
        assert_eq!((order(&phone, 2_400), player.claims()), ((4, Some([3; 24]), false), 2));
        // And one it could not do (a seek, on a radio): refused.
        player.radio = true;
        let came = hear(&phone, &mut player, session, &place, vec![command(5, &place, Command::Seek { to: 30.0 }, 4)], 2_500, &mut n);
        assert!(sends(&came.native) == 1 && logged(&came.native, "refused, Cannot"));
        assert_eq!(order(&phone, 2_500), (4, Some([3; 24]), false));
        // Two in one hand-over from the relay are each decided against the player as the
        // one before left it.
        player.radio = false;
        let two = vec![command(6, &place, Command::Pause, 4), command(7, &place, Command::Play, 5)];
        let came = hear(&phone, &mut player, session, &place, two, 2_600, &mut n);
        assert_eq!((sends(&came.native), order(&phone, 2_600), player.sound), (2, (6, Some([7; 24]), false), Some(0.0)));
    }

    /// A real player tells its listeners of a change before the call that made it returns,
    /// and the app's listener tells the linking of every change it hears.
    #[test]
    fn what_the_player_s_listeners_say_inside_a_command_is_that_command() {
        let mut n = 0;
        let (phone, player, session, place) = phone(&mut n);
        let mut player: Mock = player;
        let changed = || Happened::Event(Event::Changed { who: Who::Here, what: What::Playing });
        player.tells(&phone, "carry_out", vec![changed(), changed()]);
        let came = hear(&phone, &mut player, session, &place, vec![command(1, &place, Command::Pause, 1)], 2_000, &mut n);
        assert_eq!(player.told(), ["part of it", "part of it"]);
        // It is counted once, as the command; the state is the one that command made (a
        // press on the phone would have made it nobody's); and the queue is claimed once.
        assert_eq!((order(&phone, 2_000), player.claims(), sends(&came.native)), ((2, Some([1; 24]), false), 1, 1));
        // The next command, made against that state, goes through.
        let came = hear(&phone, &mut player, session, &place, vec![command(2, &place, Command::Play, 2)], 2_100, &mut n);
        assert!(logged(&came.native, "carried out") && order(&phone, 2_100).0 == 3);
    }

    #[test]
    fn what_else_arrives_inside_a_call_is_decided_when_it_returns_in_order() {
        let mut n = 0;
        let (phone, player, session, place) = phone(&mut n);
        let mut player: Mock = player;
        // While the player is only being asked what it holds, to answer a look, its
        // listeners tell of the next song having begun by itself. That is not the linking's
        // doing. A look-up ends as well; and the app (wrongly) sends a press from there.
        let told = vec![
            Happened::Event(Event::Changed { who: Who::Nobody, what: What::Playing }),
            Happened::Event(Event::Press { press: Press::Next }),
            Happened::Event(Event::LookingUp),
            Happened::Event(Event::Changed { who: Who::Here, what: What::Playing }),
        ];
        player.tells(&phone, "brief", told);
        let came = hear(&phone, &mut player, session, &place, vec![from_pc(1, &place, Ask::Look(Want::Brief))], 2_000, &mut n);
        assert_eq!(player.told(), ["kept", "refused", "refused", "kept"]);
        // The look was answered with the state as it was when the player was asked...
        let Some(Do::Send { frame: sent, .. }) = came.native.iter().find(|ask| matches!(ask, Do::Send { .. })) else { panic!("no answer: {came:?}") };
        let opened = frame::open(&Keys::of(&secret()), sent, &place, &[1; 16]).expect("the answer opens");
        assert_eq!(read_tell(&opened.message).map(|tell| tell.at.generation), Some(1));
        // ...and then what was kept was decided, in the order it arrived, before the call
        // returned: the song that began by itself is counted and claims nothing, the
        // person's own press is counted and the queue is theirs.
        assert_eq!((order(&phone, 2_000), player.asked.as_slice()), ((3, None, false), ["claim".to_string()].as_slice()));
        // A command made against the state the look was answered with is refused.
        let came = hear(&phone, &mut player, session, &place, vec![command(2, &place, Command::Pause, 1)], 2_100, &mut n);
        assert!(logged(&came.native, "refused, Changed"));
    }

    #[test]
    fn what_counts_in_the_order_of_choices_is_told_as_it_happens() {
        let mut n = 0;
        let (phone, mut player, session, place) = phone(&mut n);
        let pause = |n: u8, generation: u64| command(n, &place, Command::Pause, generation);
        // A press on the phone itself: a command made before it is refused.
        // (It is the person's, and on what plays, unless said: the queue is the phone's own.)
        assert_eq!(phone.handle(CHANGED, 2_000, 100, &mut random(&mut n), &mut player).unwrap().0, "[]");
        assert_eq!((order(&phone, 2_000).0, player.claims()), (2, 1));
        // What the app does by itself, and what only changes how it sounds, are counted
        // and claim nothing.
        for (event, generation) in [(r#"{"event":"changed","who":"nobody"}"#, 3), (r#"{"event":"changed","what":"manner"}"#, 4), (r#"{"event":"changed","who":"guest","what":"manner"}"#, 5)] {
            phone.handle(event, 2_000, 100, &mut random(&mut n), &mut player);
            assert_eq!((order(&phone, 2_000).0, player.claims()), (generation, 1), "{event}");
        }
        let came = hear(&phone, &mut player, session, &place, vec![pause(1, 1)], 2_100, &mut n);
        assert!(came.acts.is_empty() && logged(&came.native, "refused, Changed"));
        // Something chosen is being looked up: busy until it is said to be over.
        let (acts, _) = phone.handle(r#"{"event":"looking_up"}"#, 2_200, 100, &mut random(&mut n), &mut player).unwrap();
        assert_eq!(acts, r#"[{"act":"ticket","ticket":1}]"#);
        let came = hear(&phone, &mut player, session, &place, vec![pause(2, 5)], 2_300, &mut n);
        assert!(logged(&came.native, "refused, Busy"));
        phone.handle(r#"{"event":"looked_up","ticket":1}"#, 2_400, 100, &mut random(&mut n), &mut player);
        let came = hear(&phone, &mut player, session, &place, vec![pause(3, 5)], 2_500, &mut n);
        // Carried out, and counted once.
        assert!(logged(&came.native, "carried out") && player.sound.is_none());
        assert_eq!(order(&phone, 2_500), (6, Some([3; 24]), false));
    }

    #[test]
    fn look_ups_that_are_never_said_to_be_over_do_not_pile_up() {
        let mut n = 0;
        let (phone, mut player, ..) = phone(&mut n);
        let waits = |phone: &Phone| phone.inner.borrow().waits.iter().map(|(ticket, ..)| *ticket).collect::<Vec<u64>>();
        // A thousand begun inside a second, none ended: sixteen are kept, the latest.
        for at in 0..1_000u64 {
            phone.handle(r#"{"event":"looking_up"}"#, 5_000 + at, 100, &mut random(&mut n), &mut player);
        }
        assert_eq!((waits(&phone).len(), waits(&phone)[0], order(&phone, 6_000).2), (MOST_LOOK_UPS, 985, true));
        // The latest can still be ended by its ticket; one that was dropped is not known.
        phone.handle(r#"{"event":"looked_up","ticket":1000}"#, 6_000, 100, &mut random(&mut n), &mut player);
        phone.handle(r#"{"event":"looked_up","ticket":3}"#, 6_000, 100, &mut random(&mut n), &mut player);
        assert_eq!(waits(&phone).len(), MOST_LOOK_UPS - 1);
        // A minute on, none of them counts, and the next to begin clears them away.
        assert!(!order(&phone, 5_999 + LOOK_UP_MS).2);
        phone.handle(r#"{"event":"looking_up"}"#, 6_000 + LOOK_UP_MS, 100, &mut random(&mut n), &mut player);
        assert_eq!((waits(&phone).len(), order(&phone, 6_001 + LOOK_UP_MS).2), (1, true));
    }

    #[test]
    fn a_press_is_the_phone_s_own_unless_another_device_is_being_steered() {
        let mut n = 0;
        let (phone, mut player, ..) = phone(&mut n);
        for event in [r#"{"event":"press","press":"next"}"#, r#"{"event":"press","press":{"seek":12.5}}"#, r#"{"event":"add","ids":["s1"],"next":false}"#, r#"{"event":"play","ids":["s1"],"start":0,"context":""}"#, r#"{"event":"play_here"}"#] {
            let (acts, native) = phone.handle(event, 3_000, 100, &mut random(&mut n), &mut player).unwrap();
            assert_eq!((acts.as_str(), native.len()), (r#"[{"act":"handled","by_another":false}]"#, 0), "{event}");
        }
        // What cannot be read does nothing; and each event that takes nothing is read.
        assert_eq!(phone.handle("not json", 3_000, 100, &mut random(&mut n), &mut player), Some(("[]".to_string(), Vec::new())));
        assert_eq!(phone.handle(r#"{"event":"press","press":"somersault"}"#, 3_000, 100, &mut random(&mut n), &mut player).unwrap().0, "[]");
        for event in [r#"{"event":"let_go"}"#, r#"{"event":"reset"}"#, r#"{"event":"setting","on":true}"#, r#"{"event":"choose","place":"nowhere","in_a_jam":false}"#, r#"{"event":"pause","place":"nowhere"}"#, r#"{"event":"named","name":"Pocket"}"#] {
            assert!(serde_json::from_str::<Event>(event).is_ok(), "{event}");
        }
        // Nothing of all that was asked of the player.
        assert!(player.asked.is_empty());
        // It calls itself what it is.
        phone.happened(Happened::FromRelay { session: 0, from: FromRelay::NoRandom }, 3_200, 100, &mut random(&mut n), &mut player);
        let (acts, _) = phone.handle(TICK, 3_300, 100, &mut random(&mut n), &mut player).unwrap();
        assert!(acts.contains("This phone couldn't make random numbers, so linking is off."), "{acts}");
    }

    /// The person signs out, or signs in as someone else: everything is replaced, and what
    /// was still on its way to the last sign-in finds nothing to act on.
    #[test]
    fn a_sign_out_or_another_account_replaces_everything() {
        for over in [r#"{"event":"stop"}"#, r#"{"event":"tick","account":"somebody else","wanted":true}"#] {
            let mut n = 0;
            let (phone, mut player, session, place) = phone(&mut n);
            // Of this sign-in's: something chosen here, songs a command brought being
            // looked up on the music server, and something chosen here being looked up.
            phone.handle(CHANGED, 2_000, 100, &mut random(&mut n), &mut player);
            let add = Command::Add { ids: vec!["x1".into()], next: false };
            let came = hear(&phone, &mut player, session, &place, vec![command(1, &place, add, 2)], 2_100, &mut n);
            let Some(Do::Find { ticket, .. }) = came.native.iter().find(|ask| matches!(ask, Do::Find { .. })).cloned() else { panic!("nothing to find: {came:?}") };
            phone.handle(r#"{"event":"looking_up"}"#, 2_200, 100, &mut random(&mut n), &mut player);
            assert_eq!(order(&phone, 2_300), (2, None, true));
            // It is over. What was under way under it is ended...
            let (acts, native) = phone.handle(over, 2_400, 100, &mut random(&mut n), &mut player).unwrap();
            assert!(native.contains(&Do::End { session }) && sends(&native) == 0, "{over}: {acts} {native:?}");
            // ...and nothing of it is left: the order of choices, the look-ups, the library.
            assert_eq!(order(&phone, 2_400), (1, None, false), "{over}");
            {
                let inner = phone.inner.borrow();
                assert!(inner.waits.is_empty() && inner.library.0.is_empty() && inner.me.name == "Phone", "{over}");
            }
            let asked = player.asked.len();
            // What was on its way to it finds nothing: the songs that were found, the end
            // of the look-up, frames under its session, the answer to a look it began.
            assert_eq!(came_of(phone.happened(Happened::Found { ticket, songs: vec!["x1".into()] }, 2_500, 100, &mut random(&mut n), &mut player)), (0, 0), "{over}");
            phone.handle(r#"{"event":"looked_up","ticket":1}"#, 2_500, 100, &mut random(&mut n), &mut player);
            let came = hear(&phone, &mut player, session, &place, vec![command(2, &place, Command::Pause, 1)], 2_500, &mut n);
            assert_eq!((came.acts.len(), came.native.len()), (0, 0), "{over}");
            let late = Happened::Looked { number: FIRST_LOOK, since_ms: 2_000, looked: Looked::Secret { secret: secret(), id: "k".into(), wrote: None }, reset: false };
            assert_eq!(came_of(phone.happened(late, 2_500, 100, &mut random(&mut n), &mut player)), (0, 0), "{over}");
            assert_eq!((player.asked.len(), order(&phone, 2_500)), (asked, (1, None, false)), "{over}");
            // The next sign-in begins with a look of its own, under a number the last one
            // never used, and the next ticket is not one the last sign-in handed out.
            // (One look: where the tick that ended the last sign-in was itself the first
            // of the next, it was asked for there, and is not asked for again.)
            let (_, next) = phone.handle(r#"{"event":"tick","account":"somebody else","wanted":true}"#, 2_600, 100, &mut random(&mut n), &mut player).unwrap();
            let looks: Vec<u64> = native.iter().chain(&next).filter_map(|ask| if let Do::Look { number, .. } = ask { Some(*number) } else { None }).collect();
            assert!(looks.len() == 1 && looks[0] > FIRST_LOOK, "{over}: {native:?} {next:?}");
            assert_eq!(phone.handle(r#"{"event":"looking_up"}"#, 2_700, 100, &mut random(&mut n), &mut player).unwrap().0, r#"[{"act":"ticket","ticket":2}]"#);
        }
    }

    /// How many acts and asks came of something.
    fn came_of(answer: Answer) -> (usize, usize) {
        let came = came(answer);
        (came.acts.iter().filter(|act| !matches!(act, Act::Shown { .. })).count(), came.native.len())
    }

    /// Two phones of one account in one room, each with a player that does what it is
    /// asked and says so; what one sends, the other hears.
    struct Pair<'p> {
        phones: &'p [Phone; 2],
        players: [Mock<'p>; 2],
        sessions: [u64; 2],
        places: [[u8; 16]; 2],
        acts: [Vec<Act>; 2],
        now: u64,
        n: u64,
    }

    fn two_phones() -> [Phone; 2] {
        [Phone::new([1; 16], "Phone A", Vec::new()), Phone::new([2; 16], "Phone B", Vec::new())]
    }

    impl<'p> Pair<'p> {
        fn new(phones: &'p [Phone; 2]) -> Pair<'p> {
            let (mut n, mut sessions) = (0, [0; 2]);
            let places = [[5; 16], [6; 16]];
            // A is playing nothing; B is playing.
            let mut players = [Mock::with(["a0", "a1", "a2"].map(String::from).to_vec(), None), Mock::with(["s0", "s1", "s2", "s3"].map(String::from).to_vec(), Some(10.0))];
            for who in [0, 1] {
                let (phone, player) = (&phones[who], &mut players[who]);
                let (_, native) = phone.handle(TICK, 1_000, 100, &mut random(&mut n), player).unwrap();
                let Some(Do::Look { number, .. }) = native.first().cloned() else { panic!("no look") };
                let looked = Happened::Looked { number, since_ms: 1_000, looked: Looked::Secret { secret: secret(), id: "k".into(), wrote: None }, reset: false };
                let begun = came(phone.happened(looked, 1_000, 100, &mut random(&mut n), player));
                let Some(Do::Begin { session, .. }) = begun.native.iter().find(|ask| matches!(ask, Do::Begin { .. })).cloned() else { panic!("no session") };
                phone.happened(Happened::FromRelay { session, from: FromRelay::Entered { place: places[who], token: "a-token".into() } }, 1_000, 100, &mut random(&mut n), player);
                let all = ["a0", "a1", "a2", "s0", "s1", "s2", "s3"].map(String::from);
                phone.happened(Happened::Library(Songs::of(all)), 1_000, 100, &mut random(&mut n), player);
                sessions[who] = session;
            }
            let mut pair = Pair { phones, players, sessions, places, acts: [Vec::new(), Vec::new()], now: 1_000, n };
            for who in [0, 1] {
                let room = FromRelay::Heard { places: places.iter().map(text::place).collect(), frames: Vec::new() };
                pair.happened(who, Happened::FromRelay { session: sessions[who], from: room });
            }
            pair.pass(2_000);
            pair
        }

        fn happened(&mut self, who: usize, what: Happened) -> Vec<Act> {
            let came = came(self.phones[who].happened(what, self.now, 100, &mut random(&mut self.n), &mut self.players[who]));
            let acts = came.acts.clone();
            self.came(who, came);
            acts
        }

        fn event(&mut self, who: usize, event: &str) -> String {
            let event = serde_json::from_str::<Event>(event).expect("an event");
            serde_json::to_string(&self.happened(who, Happened::Event(event))).unwrap()
        }

        /// Carries what a phone sends to the other, and answers what it asks to be found.
        fn came(&mut self, who: usize, came: Came) {
            self.acts[who].extend(came.acts);
            for ask in came.native {
                match ask {
                    Do::Send { session, to, frame, .. } if session == self.sessions[who] && to == self.places[1 - who] => {
                        let heard = FromRelay::Heard { places: self.places.iter().map(text::place).collect(), frames: vec![(text::place(&self.places[who]), frame)] };
                        self.happened(1 - who, Happened::FromRelay { session: self.sessions[1 - who], from: heard });
                    }
                    Do::Find { ticket, ids } => {
                        self.happened(who, Happened::Found { ticket, songs: ids });
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
                    if let Some(at) = &mut self.players[who].sound {
                        *at += 0.25;
                    }
                    self.event(who, TICK);
                }
            }
        }

        fn said(&self, who: usize) -> Vec<&str> {
            self.acts[who].iter().filter_map(|act| if let Act::Do { ask: Do::Say(words) } = act { Some(words.as_str()) } else { None }).collect()
        }

        /// What a phone's player was asked, in order.
        fn asked(&self, who: usize) -> Vec<&str> {
            self.players[who].asked.iter().map(String::as_str).collect()
        }
    }

    #[test]
    fn one_phone_steers_another_and_brings_its_music_over() {
        let phones = two_phones();
        let mut pair = Pair::new(&phones);
        let changed = || Happened::Event(Event::Changed { who: Who::Here, what: What::Playing });
        // Each lists the other, with what it is doing.
        let shown = pair.event(0, TICK);
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
        // B's queue is B's own by it: the person chose it, at A.
        assert_eq!((pair.asked(1), pair.asked(0), pair.players[1].songs[0].as_str()), (vec!["next", "claim"], vec![], "s1"));
        // "Play here": A takes B's queue as data, and when A's sound has started, B is
        // told the music has moved. (Each player's listeners tell the linking of what the
        // player did, inside the call that asked it: the queue loaded on A, the stop on B.)
        pair.players[0].tells(&phones[0], "take", vec![changed()]);
        pair.players[1].tells(&phones[1], "carry_out", vec![changed()]);
        assert!(pair.event(0, r#"{"event":"play_here"}"#).ends_with(r#"{"act":"handled","by_another":true}]"#));
        pair.pass(1_500);
        // The queue is A's own by taking it, said once it is in the player; B's going
        // quiet was nobody's choice there and claims nothing, so that A, which is playing,
        // is the one whose queue is newest.
        assert_eq!((pair.asked(0), pair.asked(1)), (vec!["take 4 from 0", "claim"], vec!["next", "claim", "moved"]));
        assert_eq!((pair.said(0).last().copied(), pair.players[1].sound, pair.players[0].songs[0].as_str()), (Some("Playing here. Phone B is paused."), None, "s1"));
        assert!(pair.acts[0].contains(&Act::Do { ask: Do::Steering(None) }));
        assert_eq!((pair.players[0].told(), pair.players[1].told()), (vec!["part of it"], vec!["part of it"]));
        // And given back: A, playing, picks B, which takes the queue and plays; A is stopped
        // when B's sound has started. B's queue is B's by taking it; A's stop is the app's own.
        pair.players[0].tells(&phones[0], "pause", vec![changed()]);
        pair.event(0, &serde_json::json!({"event": "choose", "place": place, "in_a_jam": false}).to_string());
        pair.pass(1_500);
        assert_eq!((pair.said(0).last().copied(), pair.players[0].sound), (Some("Playing on Phone B."), None));
        assert_eq!((pair.asked(0), pair.asked(1)), (vec!["take 4 from 0", "claim", "pause, its own"], vec!["next", "claim", "moved", "adopt", "claim"]));
        assert_eq!(pair.players[0].told(), ["part of it"]);
    }

    /// What a frame costs the player's thread: opened, decided, answered. Not a test of
    /// anything: it prints. Run it as the phone's library is built (`native/Cargo.toml`
    /// there: for size, with everything in one unit):
    ///
    /// ```text
    /// CARGO_PROFILE_RELEASE_OPT_LEVEL=z CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    ///   cargo test -p nexus-core --release what_a_frame_costs -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn what_a_frame_costs_the_player_s_thread() {
        use std::time::Instant;
        const TIMES: usize = 2_000;
        let mut n = 0;
        let (phone, mut player, session, place) = phone(&mut n);
        // A queue of five hundred songs in the player, and a library of fifty thousand.
        player.songs = (0..500).map(|n| format!("song-{n:05}-0123456789abcdef")).collect();
        let library: Vec<String> = (0..50_000).map(|n| format!("song-{n:05}-0123456789abcdef")).collect();
        phone.happened(Happened::Library(Songs::of(library.clone())), 1_400, 100, &mut random(&mut n), &mut player);
        let nonce = |at: usize| {
            let mut nonce = [0u8; 24];
            nonce[..8].copy_from_slice(&(at as u64 + 1_000).to_le_bytes());
            nonce
        };
        let seal = |at: usize, ask: Ask| (text::place(&[1; 16]), frame::seal(&Keys::of(&secret()), ASK, &nonce(at), &[1; 16], &place, ask.text().as_bytes()).unwrap());
        let mut now = 10_000;
        let mut time = |what: &str, player: &mut Mock, frames: Vec<(String, String)>, n: &mut u64, sounds: bool| {
            let bytes = frames[0].1.len();
            let (mut spent, mut sent) = (std::time::Duration::ZERO, 0);
            for frame in frames {
                // (A second apart: a device answers only so many asks in a second. And the
                // secret is read again every few minutes, as it would be.)
                now += 1_000;
                if now % 240_000 == 0 {
                    let again = Happened::Looked { number: FIRST_LOOK, since_ms: now, looked: Looked::Secret { secret: secret(), id: "k".into(), wrote: None }, reset: false };
                    phone.happened(again, now, 100, &mut random(n), player);
                }
                let from = FromRelay::Heard { places: vec![text::place(&place), text::place(&[1; 16])], frames: vec![frame] };
                let began = Instant::now();
                let answer = phone.happened(Happened::FromRelay { session, from }, now, 100, &mut random(n), player);
                spent += began.elapsed();
                sent += sends(&came(answer).native);
                // (A queue taken over is answered when it sounds: it sounds, untimed.)
                for _ in 0..if sounds { 2 } else { 0 } {
                    player.sound = player.sound.map(|at| at + 0.5);
                    sent += sends(&came(phone.happened(Happened::Event(serde_json::from_str(TICK).unwrap()), now, 100, &mut random(n), player)).native);
                }
            }
            assert!(sent >= TIMES, "{what}: each was answered ({sent})");
            eprintln!("{what:<58} {:>7.1} microseconds   (a frame of {bytes} characters)", spent.as_secs_f64() * 1e6 / TIMES as f64);
        };
        let at_frame = |batch: usize, at: usize, generation: u64| At { generation: generation + at as u64, minute: super::super::minute(10_000 + 1_000 * (batch * TIMES + at + 1) as u64) };
        time("a look, answered in brief", &mut player, (0..TIMES).map(|at| seal(at, Ask::Look(Want::Brief))).collect(), &mut n, false);
        time("a look, answered with everything the player shows", &mut player, (0..TIMES).map(|at| seal(at, Ask::Look(Want::Full))).collect(), &mut n, false);
        time("a look, answered with the queue to carry on with", &mut player, (0..TIMES).map(|at| seal(at, Ask::Look(Want::Queue))).collect(), &mut n, false);
        let generation = order(&phone, 0).0;
        time("Next, carried out", &mut player, (0..TIMES).map(|at| seal(at, Ask::Do { command: Command::Next, at: at_frame(3, at, generation) })).collect(), &mut n, false);
        let generation = order(&phone, 0).0;
        let add = |at: usize| Command::Add { ids: (0..super::super::command::MOST_ADDED).map(|song| library[(at * 7 + song) % library.len()].clone()).collect(), next: false };
        time("Add, of as many songs as one command carries", &mut player, (0..TIMES).map(|at| seal(at, Ask::Do { command: add(at), at: at_frame(4, at, generation) })).collect(), &mut n, false);
        player.songs.truncate(500);
        let generation = order(&phone, 0).0;
        let (adopt, left) = super::super::command::Adopt::fitted(&library[..10_000], 0, 0, 12.0, "an album").unwrap();
        eprintln!("(a queue to take over: {} songs fit one message, {left} of ten thousand are left behind)", adopt.songs.len());
        time("a queue to take over, as long as one message carries", &mut player, (0..TIMES).map(|at| seal(at, Ask::Do { command: Command::Adopt(adopt.clone()), at: at_frame(5, at, generation) })).collect(), &mut n, true);
        assert_eq!(player.asked.iter().filter(|asked| *asked == "adopt").count(), TIMES);
        // A tick, with one other device in the room.
        let began = Instant::now();
        for at in 0..TIMES as u64 {
            phone.handle(TICK, now + at, 100, &mut random(&mut n), &mut player);
        }
        eprintln!("{:<58} {:>7.1} microseconds", "a tick", began.elapsed().as_secs_f64() * 1e6 / TIMES as f64);
        // The library put in place (made ready elsewhere), and the one before it let go of.
        let ready: Vec<Arc<Songs>> = (0..20).map(|_| Songs::of(library.clone())).collect();
        let began = Instant::now();
        for songs in ready {
            phone.happened(Happened::Library(songs), now, 100, &mut random(&mut n), &mut player);
        }
        eprintln!("{:<58} {:>7.1} microseconds", "a library of fifty thousand songs put in place", began.elapsed().as_secs_f64() * 1e6 / 20.0);
    }
}
