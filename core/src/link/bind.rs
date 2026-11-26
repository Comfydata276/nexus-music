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
//! [`Do::Look`], [`Do::Begin`], [`Do::Send`] and [`Do::Find`] ([`Came::native`]), and puts
//! what they bring where it waits for the player's thread ([`Inbox`]: [`Happened::Looked`],
//! [`Happened::FromRelay`], [`Happened::Found`]), which works through it in
//! [`Phone::turn`]. The secret never leaves Rust. The phone's copy of the library is
//! made ready on whichever thread it is read on ([`Songs::of`]), waits in the same place,
//! and is only put in place here.
//!
//! **A sign-out, or another account, replaces everything**: the device, the order of
//! choices, the look-ups under way, the copy of the library. Nothing of the last sign-in's
//! is asked again. (Only the numbers that looks, sessions and tickets had reached carry
//! over, so that an answer still on its way to the last sign-in matches nothing in the next.)

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::command::Command;
use super::device::{Brief, Carried, Device, Do, Env, Me, Player, Ready, Shown, Sounding};
use super::look::Looked;
use super::message::{At, Full, Handed, Why};
use super::relay::FromRelay;
use super::rule::{Carrying, Reservation, Rule, Waiting};
use super::seen::Seen;
use super::turn::{Entered, Kind, MOST_KEPT, MOST_KEPT_BYTES, Turn, Turns};
use super::view::{Press, View};
use super::words::Here;
use super::{DeviceId, Nonce};
use crate::requests::{AlbumRef, Library, SongRef};
use crate::ways::{Ways, What, Who};

/// The phone's own player, as the app answers for it. **Every call is made on the thread
/// the player lives on, from inside [`Phone::happened`], and is answered before it
/// returns.**
///
/// **None of these can fail: each is a total function of the player's state.** There is
/// no answer for "could not be asked", and the app never gives one in its place: no zero
/// for a queue's number, no "not playing" for a state it could not read, no "not done" for
/// an act that threw. If a call into the player does fail (an exception on the app's side,
/// a read that could not be made), the app's side of this **stops the call by unwinding**
/// (a panic), and that is a fault ([`Act::Fault`]): everything is replaced and linking is
/// off on the phone until the app is next started.
pub trait PhonePlayer {
    fn brief(&self) -> Brief;
    /// What a device steering this one shows.
    fn full(&self) -> Full;
    /// What is playing and to come, by id, the song that is playing first; nothing, if
    /// what is playing is not something another device could carry on with (a radio).
    fn handed(&self) -> Handed;
    /// Where the sound has got to in the song the player holds, and the number of the
    /// queue that song belongs to: **audio that is advancing** for the current entry of
    /// the current queue. Not that the player has been told to play, and not a place it
    /// has been told to go to.
    fn sounding(&self) -> Option<Sounding>;
    /// The number of the queue in the player: a new one each time a queue is put in the
    /// player whole, never used twice (see [`Player::queue`]).
    fn queue(&self) -> u64;
    /// Whether there is a song in the player at all.
    fn has_song(&self) -> bool;
    /// The ids of the songs to come, in the order they would play. The whole of them:
    /// asked only when a command names an entry that is not where it says.
    fn ahead(&self) -> Vec<String>;
    /// The id of the song `at` places after the current one; `None` if the queue does
    /// not reach so far. (One song: what a command that names an entry costs when the
    /// entry is where its sender saw it.)
    fn entry_at(&self, at: usize) -> Option<String>;
    /// How many songs are to come, which the player knows without walking its queue.
    fn to_come(&self) -> usize;
    /// Carries a command out, exactly as the same press on the phone would be, **and says
    /// which of three things came of it** ([`Carried`]): it changed the player; it was so
    /// already (Play while the player is set to play, Pause while it is not, the volume,
    /// shuffle or repeat at what was asked, nothing to clear); or it refused (the app's
    /// own rules would not start playing, a seek where there is nowhere to seek to, a
    /// Next with nothing to go to, an entry that is not there). What is said is what
    /// happened to the player, not what was asked of it. It is **not** an
    /// [`Event::Changed`]: it is counted here, once, if it changed anything, and
    /// whatever the player's listeners say of it meanwhile is dropped.
    fn carry_out(&mut self, command: &Command, ready: &Ready) -> Carried;
    /// The phone's queue has been handed to another of the person's devices: from this
    /// moment it is one taken up from that device and not touched
    /// (`handover::Here::taken` is `handover::HANDED`). It is not saved, and it follows
    /// what that device saves, until [`PhonePlayer::claim`]. Called before the stop that
    /// goes with it, so that the stop is not saved as the phone's own change.
    ///
    /// **Whether the mark is in the queue's file**: the write's own result, within the
    /// short time the app allows. False: nothing is marked, and the hand-over is refused
    /// before the phone is stopped.
    fn handed_on(&mut self) -> bool;
    /// The hand-over did not happen (the phone did not stop): the queue is as it was
    /// before [`PhonePlayer::handed_on`].
    fn handed_back(&mut self);
    /// Stops the phone's own sound. Counted here already. **Whether the phone is then not
    /// set to play** (its intent: a phone whose sound a call holds back, still set to
    /// play, has not stopped).
    fn pause(&mut self) -> bool;
    /// Takes this queue and plays it: each song and whether it was queued by hand, where to
    /// start, and how far in. Whether the queue is in the player afterwards is what
    /// [`PhonePlayer::queue`] says: a new number if it is, and it is counted and claimed
    /// here only then. A player that does not take it (its own rules refuse) leaves the
    /// number as it was, and that is an answer, not a failure.
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
}

impl Happened {
    /// What it is to a call it arrives inside (`super::turn`). `account` is the sign-in
    /// the linking is for now, if a tick has said: a tick that names another is the end of it.
    fn kind(&self, account: Option<&str>) -> Kind {
        match self {
            Happened::Event(Event::Stop) => Kind::End,
            Happened::Event(Event::Tick { account: named, .. }) if account.is_some_and(|now| now != named.as_str()) => Kind::End,
            Happened::Event(Event::Changed { .. }) => Kind::Change,
            Happened::Event(Event::Press { .. } | Event::Add { .. } | Event::Play { .. } | Event::PlayHere | Event::LookingUp) => Kind::Question,
            _ => Kind::News,
        }
    }

    /// How large it is, as it is held: for the bounds on what waits. Every text in it is
    /// counted, with what a text costs to hold beside its characters.
    fn size(&self) -> usize {
        const TEXT: usize = std::mem::size_of::<String>();
        let texts = |texts: &[String]| texts.iter().map(|text| TEXT + text.len()).sum::<usize>();
        std::mem::size_of::<Happened>()
            + match self {
                Happened::Event(Event::Add { ids, .. }) => texts(ids),
                Happened::Event(Event::Play { ids, context, .. }) => texts(ids) + context.len(),
                Happened::Event(Event::Tick { account: text, .. } | Event::Choose { place: text, .. } | Event::Pause { place: text } | Event::Named { name: text }) => text.len(),
                Happened::Event(_) => 0,
                Happened::Looked { looked: Looked::Secret { id, .. }, .. } => id.len(),
                Happened::Looked { .. } => 0,
                Happened::FromRelay { from: FromRelay::Heard { places, frames }, .. } => texts(places) + frames.iter().map(|(from, frame)| 2 * TEXT + from.len() + frame.len()).sum::<usize>(),
                Happened::FromRelay { from: FromRelay::Entered { token, .. }, .. } => token.as_str().len(),
                Happened::FromRelay { .. } => 0,
                Happened::Found { songs, .. } => texts(songs),
            }
    }
}

/// **What the requests bring back, waiting for the player's thread.** The look, the stay
/// at the relay and each look-up of songs run on threads of their own ([`Came::native`]);
/// what they bring is put here, from whichever thread it is on, and the player's thread
/// works through it in [`Phone::turn`]. It is the one place such things wait, and the
/// one drain of it is there.
///
/// **It is bounded, by number ([`MOST_KEPT`]) and by size ([`MOST_KEPT_BYTES`], each
/// thing counted at what it holds), and nothing is dropped at the bound.** What does not
/// fit is handed back to the thread that brought it ([`Inbox::put`]), which keeps it,
/// waits, and offers it again: the stay at the relay does that by itself
/// (`super::relay::keep_place`: frames not taken are not counted as handed over). So
/// what waits is this much, and one thing in the hand of each thread that brings.
///
/// **The phone's copy of the library does not wait in line.** There is one place for
/// it, the newest sign-in's copy stays in it, and a copy read for an earlier sign-in
/// cannot push a later one's out ([`Inbox::library`]).
#[derive(Default)]
pub struct Inbox {
    waiting: Mutex<Brought>,
}

/// What is waiting in an [`Inbox`].
#[derive(Default)]
struct Brought {
    things: VecDeque<(Happened, usize)>,
    bytes: usize,
    library: Option<(u64, Arc<Songs>)>,
}

impl Inbox {
    fn waiting(&self) -> std::sync::MutexGuard<'_, Brought> {
        self.waiting.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Something a request brought back, from any thread. **`Err` hands it back**: there
    /// is no room for it just now, and whoever brought it offers it again in a while.
    /// (Only what requests bring: something that happened on the phone is told on the
    /// player's own thread, [`Phone::happened`], and is never taken here.)
    pub fn put(&self, what: Happened) -> Result<(), Happened> {
        let size = what.size();
        let mut waiting = self.waiting();
        if matches!(what, Happened::Event(_)) || waiting.things.len() >= MOST_KEPT || waiting.bytes.saturating_add(size) > MOST_KEPT_BYTES {
            return Err(what);
        }
        waiting.bytes += size;
        waiting.things.push_back((what, size));
        Ok(())
    }

    /// The phone's copy of the library, read for the sign-in with this number (the one
    /// that was current when the reading began: [`Phone::sign_in`]), from any thread.
    pub fn library(&self, sign_in: u64, songs: Arc<Songs>) {
        let mut waiting = self.waiting();
        // (A copy read for an earlier sign-in than the one whose copy is waiting is of a
        // sign-in that is over: it is let go of here, and displaces nothing.)
        if waiting.library.as_ref().is_none_or(|(held, _)| *held <= sign_in) {
            waiting.library = Some((sign_in, songs));
        }
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        let waiting = self.waiting();
        waiting.things.is_empty() && waiting.library.is_none()
    }

    fn take(&self) -> Option<Happened> {
        let mut waiting = self.waiting();
        let (what, size) = waiting.things.pop_front()?;
        waiting.bytes = waiting.bytes.saturating_sub(size);
        Some(what)
    }

    fn take_library(&self) -> Option<(u64, Arc<Songs>)> {
        self.waiting().library.take()
    }

    /// Everything waiting is let go of: the linking it was for is over (the service that
    /// had it is going, or it has stopped for a fault).
    pub fn clear(&self) {
        *self.waiting() = Brought::default();
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
    /// **Linking has stopped on this phone**, until the app is next started:
    /// a call into the player failed, or something inside the library did
    /// ([`Phone::happened`]). The library asks nothing more of the player. The app puts
    /// right what only it can, since the library can no longer ask: if its queue is marked
    /// as handed to another device and the phone is the one playing, the app takes the
    /// mark back, as [`PhonePlayer::handed_back`] would.
    Fault,
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
    /// The number the player last gave for its queue.
    queue: Cell<u64>,
}

impl Held<'_> {
    /// **The sign-in ended while this call was under way** (the app said so from inside
    /// one of the calls up). The player may be another account's already: for what is left
    /// of the call nothing more is asked of it and nothing is done to it. Every question
    /// is answered as nothing, every act as not done.
    ///
    /// **Looked at before every call into the player, and again after it**: the app can
    /// say so from inside any of them, a question as well as an act, and what a call
    /// answered is not gone by, nor anything more done on the strength of it, if the
    /// sign-in ended while it was being made.
    fn over(&self) -> bool {
        self.turns.ended()
    }

    /// Asks the player something, for this sign-in: nothing, if the sign-in has ended,
    /// or ended while it was being asked.
    fn ask<T: Default>(&self, ask: impl FnOnce(&dyn PhonePlayer) -> T) -> T {
        if self.over() {
            return T::default();
        }
        let answer = ask(&*self.theirs);
        if self.over() { T::default() } else { answer }
    }

    /// The queue is the phone's own from now.
    fn claim(&mut self) {
        if !self.over() {
            self.theirs.claim();
        }
    }
}

impl Player for Held<'_> {
    fn brief(&self) -> Brief {
        self.ask(|player| player.brief())
    }
    fn full(&self) -> Full {
        self.ask(|player| player.full())
    }
    fn handed(&self) -> Handed {
        self.ask(|player| player.handed())
    }
    fn sounding(&self) -> Option<Sounding> {
        self.ask(|player| player.sounding())
    }
    /// (After the end, the number it last gave for this sign-in: nothing has been put in
    /// for this sign-in since.)
    fn queue(&self) -> u64 {
        if let Some(number) = self.ask(|player| Some(player.queue())) {
            self.queue.set(number);
        }
        self.queue.get()
    }
    /// (The phone says the next song has started as its player says so: there is no
    /// window here that lags the sound for this to bring up.)
    fn catch_up(&mut self) -> bool {
        false
    }
    fn has_song(&self) -> bool {
        self.ask(|player| player.has_song())
    }
    fn ahead(&self) -> Vec<String> {
        self.ask(|player| player.ahead())
    }
    fn entry_at(&self, at: usize) -> Option<String> {
        self.ask(|player| player.entry_at(at))
    }
    fn to_come(&self) -> usize {
        self.ask(|player| player.to_come())
    }
    /// The person chose it at another of their devices (but for the stop that ends a move,
    /// which is nobody's): the queue is theirs by it, if it changed what a saved queue
    /// holds. The rule counts it.
    ///
    /// **Only what is in the player is claimed.** A queue to take over is in the player if
    /// the player's queue has a new number.
    fn carry_out(&mut self, command: &Command, ready: &Ready) -> Carried {
        let before = self.queue();
        // (The sign-in may have ended inside that read: then nothing is changed.)
        if self.over() {
            return Carried::Refused;
        }
        self.ways.begin(command.chosen_by(), true);
        let carried = {
            let _changing = self.turns.changing();
            self.theirs.carry_out(command, ready)
        };
        let changed = match command {
            Command::Adopt(_) => self.queue() != before,
            _ => carried == Carried::Changed,
        };
        // (Or inside the act, or the read after it: then what it answered is nobody's,
        // nothing is claimed for it, and it is said to have done nothing.)
        if self.over() {
            self.ways.end();
            return Carried::Refused;
        }
        if changed && self.ways.touch(Who::Here, command.touches()).claim {
            self.claim();
        }
        self.ways.end();
        carried
    }
    fn handed_on(&mut self) -> bool {
        // (A sign-in that is over hands nothing on.)
        !self.over() && self.theirs.handed_on()
    }
    fn handed_back(&mut self) {
        if !self.over() {
            self.theirs.handed_back();
        }
    }
    /// (The end of a move of the phone's music to another device, or of one that did not
    /// start: the app's own doing, which claims nothing.)
    fn pause(&mut self) -> bool {
        if self.over() {
            return false;
        }
        if self.ways.touch(Who::Nobody, What::Playing).count {
            self.rule.moved();
        }
        let _changing = self.turns.changing();
        self.theirs.pause()
    }
    /// ("Play here": the person's own choice, at the phone. The queue is the phone's own
    /// once it is in the player.)
    ///
    /// **Only a queue that went in is counted and claimed**: whether it did is asked of
    /// the player afterwards (its queue has a new number).
    fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, context: &str) {
        let before = self.queue();
        if self.over() {
            return;
        }
        {
            let _changing = self.turns.changing();
            self.theirs.take(songs, begin, position, context);
        }
        if self.queue() != before && !self.over() {
            let owed = self.ways.touch(Who::Here, What::Playing);
            if owed.count {
                self.rule.moved();
            }
            if owed.claim {
                self.claim();
            }
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
    fn carried(&mut self, carrying: Carrying, changed: bool, by: Option<Nonce>) {
        self.rule.carried(carrying, changed, by)
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
    /// **More is waiting than this call worked through** (a call hands over what the
    /// library had kept, and a bounded number of things more: `super::turn`): kept by the
    /// library, or in the [`Inbox`] the call was given. The app makes another call now,
    /// [`Phone::turn`], which needs nothing new to carry on.
    pub more: bool,
    /// The number of the sign-in the linking is for now ([`Phone::sign_in`]).
    pub sign_in: u64,
}

/// What a call was answered with.
#[derive(Debug, PartialEq)]
pub enum Answer {
    /// It was decided, with whatever arrived while it was (in the order it arrived): this
    /// came of all of it.
    Came(Came),
    /// The library is inside a call (this arrived from one of the app's listeners, while
    /// the player was being asked): kept, and decided when that call returns. What comes of
    /// it is in that call's answer. **The end of a sign-in** that arrives so is applied
    /// before anything else: from that moment the call in flight asks nothing more of the
    /// player, what came of it is dropped, and whatever else was kept is dropped unread.
    Kept,
    /// The library is changing the player and this is that change, heard back: dropped.
    PartOfIt,
    /// The library is inside a call and this needs its answer now, or more is waiting
    /// than is kept: nothing was done. (A press never comes from a player's listener. One
    /// that does is the app's mistake.) The end of a sign-in is never refused.
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
}

/// The linking, for an app whose player and library are not in Rust. It belongs to the
/// thread the player lives on, and is called from nowhere else. (What other threads bring
/// waits in an [`Inbox`], which is theirs to put things in.)
pub struct Phone {
    turns: Turns<Happened>,
    inner: RefCell<Inner>,
    /// The sign-in all of it belongs to, once a tick has said. (Beside the rest, so that
    /// what arrives while a call is under way can be told from the end of the sign-in.)
    account: RefCell<Option<String>>,
    /// **Linking is off after a fault**, and nothing puts it on again. (A new start of the
    /// app makes a new one of these; an app that makes a second one in the same run, as a
    /// phone's service made again does, does not make it after a fault.)
    off: Cell<bool>,
    here: Here,
    /// The number of the sign-in all of it belongs to: a new one each time a sign-in ends.
    sign_in: Cell<u64>,
}

/// What a call brings with it.
struct Call<'a> {
    now_ms: u64,
    today: u64,
    random: &'a mut dyn FnMut(&mut [u8]) -> bool,
    theirs: &'a mut dyn PhonePlayer,
    turns: &'a Turns<Happened>,
    account: &'a RefCell<Option<String>>,
}

impl Inner {
    fn new(device: Device, me: Me, tickets: u64) -> Inner {
        Inner { device, rule: Rule::default(), ways: Ways::default(), waits: Vec::new(), tickets, library: Arc::default(), me }
    }

    /// Everything, replaced: nothing of the sign-in that was is in it. (See
    /// [`Device::succeed`] for the numbers that carry over, and why.)
    fn afresh(&self) -> Inner {
        Inner::new(self.device.succeed(), self.me.clone(), self.tickets)
    }

    /// Hands the deciding something, with the player to ask, and sorts what it asks for.
    fn with(&mut self, call: &mut Call, came: &mut Came, act: impl FnOnce(&mut Device, &mut Env) -> Vec<Do>) {
        let Inner { device, rule, ways, library, me, .. } = self;
        let mut player = Held { rule, ways, theirs: &mut *call.theirs, turns: call.turns, queue: Cell::new(0) };
        let asked = act(device, &mut Env { now_ms: call.now_ms, today: call.today, me, player: &mut player, library: &**library, random: &mut *call.random });
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
    fn pressed(&mut self, call: &mut Call, came: &mut Came, press: impl FnOnce(&mut Device, &mut Env) -> Option<Vec<Do>>) {
        let mut by_another = false;
        self.with(call, came, |device, env| {
            let asked = press(device, env);
            by_another = asked.is_some();
            asked.unwrap_or_default()
        });
        came.acts.push(Act::Handled { by_another });
    }

    /// The sign-in is over: what was under way under it is ended, and then **everything
    /// is replaced**. Nothing of the last sign-in's is asked again.
    fn sign_in_over(&mut self, call: &mut Call, came: &mut Came) {
        self.with(call, came, |device, env| device.stop(env));
        *self = self.afresh();
        *call.account.borrow_mut() = None;
    }

    /// One thing that happened, decided.
    fn one(&mut self, what: Happened, call: &mut Call, came: &mut Came) {
        let event = match what {
            Happened::Event(event) => event,
            Happened::Looked { number, since_ms, looked, reset } => return self.with(call, came, |device, env| device.looked(number, since_ms, looked, reset, env)),
            Happened::FromRelay { session, from } => {
                return match from {
                    FromRelay::Says(note) => self.device.said(session, note),
                    FromRelay::NoRandom => self.with(call, came, |device, env| device.out_of_random(session, env)),
                    FromRelay::Entered { place, token } => self.device.entered(session, place, token),
                    FromRelay::Heard { places, frames } => self.with(call, came, |device, env| device.heard(session, &places, &frames, env)),
                    FromRelay::Left => self.device.left(session),
                };
            }
            Happened::Found { ticket, songs } => return self.with(call, came, |device, env| device.found(ticket, songs, env)),
        };
        match event {
            Event::Tick { account, wanted } => {
                // Another account: the last one's is over before anything of this one's begins.
                let was = call.account.borrow().clone();
                if was.is_some_and(|was| was != account) {
                    self.sign_in_over(call, came);
                }
                *call.account.borrow_mut() = Some(account.clone());
                self.with(call, came, |device, env| device.tick(&account, wanted, env));
                let (shown, logged) = self.device.shown(call.now_ms, self.me.here);
                came.native.extend(logged);
                came.acts.push(Act::Shown { shown });
            }
            Event::Changed { who, what } => {
                let owed = self.ways.touch(who, what);
                if owed.count {
                    self.rule.moved();
                }
                if owed.claim {
                    call.theirs.claim();
                }
            }
            Event::LookingUp => {
                // (One whose minute is up counts for nothing already, and one that was
                // never said to be over is not kept for ever: the list has a bound.)
                let now_ms = call.now_ms;
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
            Event::Press { press } => self.pressed(call, came, |device, env| device.press(press, env)),
            Event::Add { ids, next } => self.pressed(call, came, |device, env| device.add(&ids, next, env)),
            Event::Play { ids, start, context } => self.pressed(call, came, |device, env| device.play(&ids, start, &context, env)),
            Event::PlayHere => self.pressed(call, came, |device, env| device.play_here(env)),
            Event::Choose { place, in_a_jam } => self.with(call, came, |device, env| device.choose(&place, in_a_jam, env)),
            Event::Pause { place } => self.with(call, came, |device, env| device.pause(&place, env)),
            Event::LetGo => self.with(call, came, |device, _| device.let_go()),
            Event::Reset => self.with(call, came, |device, env| device.reset(env)),
            Event::Setting { on } => self.with(call, came, |device, env| device.setting(on, env)),
            Event::Named { name } => self.me.name = name,
            Event::Stop => self.sign_in_over(call, came),
            Event::Resume { ticket } => self.with(call, came, |device, env| device.resume(ticket, env)),
        }
    }
}

impl Phone {
    /// `device` is the id made once for the installation; `name` what the person calls the
    /// phone; `remembered` the devices seen before, as last kept ([`Do::Remember`]).
    pub fn new(device: DeviceId, name: &str, remembered: Vec<Seen>) -> Phone {
        let me = Me { device, name: name.to_string(), here: Here::Phone };
        Phone { turns: Turns::default(), inner: RefCell::new(Inner::new(Device::new(remembered), me, 0)), account: RefCell::new(None), off: Cell::new(false), here: Here::Phone, sign_in: Cell::new(1) }
    }

    /// Something happened. Called on the thread the player lives on; `player` is asked
    /// there, from inside this call. `now_ms` is milliseconds on a clock that goes on
    /// counting while the phone sleeps (`elapsedRealtime`), `today` the day since 1970, and
    /// `random` fills bytes from the system's source of randomness or says it could not.
    ///
    /// **It may be called from inside itself** (the player's listeners, told of a change
    /// while the player is being asked): see [`Answer`].
    ///
    /// **A fault.** If anything inside it stops unexpectedly (a panic: a call into the
    /// player that failed, see [`PhonePlayer`], or a fault of this library's own), it does
    /// not stay half-way, and it does not go on with an answer it did not get. Everything
    /// it held is replaced, as at a sign-out; the session and the look are ended for the
    /// app's side; one line is said of it ([`Do::Log`]); the app is told ([`Act::Fault`]);
    /// and **linking is off on this phone until the app is next started**:
    /// until then nothing is asked of the player, nothing is looked for and nothing is
    /// sent, so a fault that is still there cannot come round again at every tick. A
    /// press is the phone's own ([`Act::Handled`]), a tick says why the list is empty.
    pub fn happened(&self, what: Happened, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, player: &mut dyn PhonePlayer) -> Answer {
        if self.off.get() {
            return Answer::Came(self.while_off(&what));
        }
        let kind = what.kind(self.account.borrow().as_deref());
        // The sign-in has another number from the moment it is said to be over, wherever
        // that is said from: whatever the app begins next is the next sign-in's.
        if kind == Kind::End {
            self.sign_in.set(self.sign_in.get() + 1);
        }
        let size = what.size();
        match self.turns.enter(what, kind, size) {
            Entered::Mine(turn) => self.run(turn, None, now_ms, today, random, player),
            Entered::Kept => Answer::Kept,
            Entered::PartOfIt => Answer::PartOfIt,
            Entered::Refused => Answer::Refused,
        }
    }

    /// **The one drain.** Works through what the library kept from the calls before, and
    /// then what the requests have brought ([`Inbox`]): a bounded number of things, the
    /// oldest first. With nothing new it carries on with what was kept. The app calls it
    /// on the player's thread whenever something has been put in the inbox, and again
    /// whenever an answer says there is more ([`Came::more`]): it has no loop of its own.
    ///
    /// [`Answer::Refused`] if it is called from inside a call (it never needs to be).
    pub fn turn(&self, inbox: &Inbox, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, player: &mut dyn PhonePlayer) -> Answer {
        if self.off.get() {
            // (Off after a fault: what the requests still bring is nobody's.)
            inbox.clear();
            return Answer::Came(Came { sign_in: self.sign_in.get(), ..Came::default() });
        }
        match self.turns.carry_on() {
            Some(turn) => self.run(turn, Some(inbox), now_ms, today, random, player),
            None => Answer::Refused,
        }
    }

    /// A turn, worked through: what the library kept first, and then, if the turn was
    /// given an inbox, what is waiting there, up to the turn's bound.
    fn run(&self, turn: Turn<'_, Happened>, inbox: Option<&Inbox>, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, player: &mut dyn PhonePlayer) -> Answer {
        let decided = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut turn = turn;
            // (The turn is this call's, so nothing else is inside.)
            let mut inner = self.inner.try_borrow_mut().ok()?;
            let mut call = Call { now_ms, today, random, theirs: player, turns: &self.turns, account: &self.account };
            let mut came = Came::default();
            loop {
                let what = match (turn.next(), inbox) {
                    (Some(what), _) => what,
                    // (More is kept than this turn hands over, or nothing was brought.)
                    (None, _) if turn.left() > 0 => break,
                    (None, None) => break,
                    // Everything kept has been decided, the end of a sign-in among it if
                    // there was one: so the copy of the library that is waiting is put in
                    // place for the sign-in there now is, and not for one that was about
                    // to be replaced.
                    (None, Some(inbox)) => {
                        if let Some((sign_in, songs)) = inbox.take_library() {
                            match sign_in == self.sign_in.get() {
                                true => inner.library = songs,
                                false => came.native.push(Do::Log("library: a copy read for a sign-in that is over, dropped".into())),
                            }
                        }
                        match turn.spend().then(|| inbox.take()).flatten() {
                            Some(what) => what,
                            None => break,
                        }
                    }
                };
                inner.one(what, &mut call, &mut came);
                // The sign-in ended while this was being decided. **All the call had come
                // to is of the sign-in that is over, and is dropped**: what came of this,
                // and of whatever the call had decided before it. Only what ends
                // something on the app's side is kept, since that is still to be ended.
                // (The end itself is next.)
                if turn.overtaken() {
                    came.acts.retain(|act| matches!(act, Act::Do { ask: Do::Steering(None) }));
                    came.native.retain(|ask| matches!(ask, Do::End { .. } | Do::StopLook));
                    came.native.push(Do::Log("the sign-in ended while something was being decided: what the call had come to is dropped".into()));
                }
            }
            came.more = turn.left() > 0 || inbox.is_some_and(|inbox| !inbox.is_empty());
            came.sign_in = self.sign_in.get();
            Some(came)
        }));
        match decided {
            Ok(Some(came)) => Answer::Came(came),
            Ok(None) => Answer::Refused,
            Err(stopped) => Answer::Came(self.start_again(stopped.as_ref())),
        }
    }

    /// **The number of the sign-in the linking is for now**: a new one each time a
    /// sign-in ends (a sign-out, another account, a fault), **from the moment the app
    /// says so**, wherever it says so from. It is in every answer ([`Came::sign_in`]).
    /// What an app reads for a sign-in away from the player's thread (its copy of the
    /// library) is handed over with the number that was current when the reading began
    /// ([`Inbox::library`]): so an app that says the sign-in is over and then begins to
    /// read for the next one asks for the number in between.
    pub fn sign_in(&self) -> u64 {
        self.sign_in.get()
    }

    /// What something comes to while linking is off after a fault: a tick says why the
    /// list is empty, a press is the phone's own, and nothing else is anything. Nothing
    /// ends it.
    fn while_off(&self, what: &Happened) -> Came {
        let mut came = Came { sign_in: self.sign_in.get(), ..Came::default() };
        match what {
            Happened::Event(Event::Tick { .. }) => came.acts.push(Act::Shown { shown: Shown { note: super::words::fault(self.here), ..Shown::default() } }),
            Happened::Event(Event::Press { .. } | Event::Add { .. } | Event::Play { .. } | Event::PlayHere) => came.acts.push(Act::Handled { by_another: false }),
            _ => {}
        }
        came
    }

    /// Something stopped unexpectedly: a fault. Nothing is left as it was at that moment:
    /// what was kept for later is dropped, the session and any look are ended for the
    /// app's side, everything of the sign-in is replaced, and linking is off.
    fn start_again(&self, stopped: &(dyn std::any::Any + Send)) -> Came {
        let said = stopped.downcast_ref::<&str>().map(|text| text.to_string()).or_else(|| stopped.downcast_ref::<String>().cloned()).unwrap_or_default();
        self.turns.forget();
        self.off.set(true);
        self.sign_in.set(self.sign_in.get() + 1);
        let mut came = Came { sign_in: self.sign_in.get(), ..Came::default() };
        came.acts.push(Act::Fault);
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            if let Some(session) = inner.device.session_number() {
                came.native.push(Do::End { session });
            }
            came.native.push(Do::StopLook);
            if inner.device.steered().is_some() {
                came.acts.push(Act::Do { ask: Do::Steering(None) });
            }
            *inner = inner.afresh();
        }
        came.native.push(Do::Log(format!("linking: it stopped unexpectedly ({}). Everything it held was let go of, and linking is off here until the app is next started.", super::text::cut(&said, 200))));
        came
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

    /// As the phone's app calls: an [`Event`] as JSON, giving the [`Act`]s as a JSON list
    /// and, apart, what the Rust side does itself. An event that cannot be read does
    /// nothing and says nothing; one that is kept or dropped says nothing here; `None` is
    /// [`Answer::Refused`].
    trait AsTheAppCalls {
        fn handle(&self, event: &str, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, player: &mut dyn PhonePlayer) -> Option<(String, Vec<Do>)>;
        /// Puts a copy of the library, read for this sign-in, where such copies wait, and
        /// takes a turn.
        fn library(&self, sign_in: u64, songs: Arc<Songs>, now_ms: u64, n: &mut u64, player: &mut dyn PhonePlayer) -> Came;
    }

    impl AsTheAppCalls for Phone {
        fn handle(&self, event: &str, now_ms: u64, today: u64, random: &mut dyn FnMut(&mut [u8]) -> bool, player: &mut dyn PhonePlayer) -> Option<(String, Vec<Do>)> {
            let nothing = || Some(("[]".to_string(), Vec::new()));
            let Ok(event) = serde_json::from_str::<Event>(event) else { return nothing() };
            match self.happened(Happened::Event(event), now_ms, today, random, player) {
                Answer::Came(came) => Some((serde_json::to_string(&came.acts).unwrap_or_else(|_| "[]".to_string()), came.native)),
                Answer::Kept | Answer::PartOfIt => nothing(),
                Answer::Refused => None,
            }
        }

        fn library(&self, sign_in: u64, songs: Arc<Songs>, now_ms: u64, n: &mut u64, player: &mut dyn PhonePlayer) -> Came {
            let inbox = Inbox::default();
            inbox.library(sign_in, songs);
            came(self.turn(&inbox, now_ms, 100, &mut random(n), player))
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
        /// How many of that call go by first.
        after: Cell<usize>,
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
        fn sounding(&self) -> Option<Sounding> {
            unreachable!("asked from inside a call")
        }
        fn queue(&self) -> u64 {
            unreachable!("asked from inside a call")
        }
        fn has_song(&self) -> bool {
            unreachable!("asked from inside a call")
        }
        fn ahead(&self) -> Vec<String> {
            unreachable!("asked from inside a call")
        }
        fn entry_at(&self, _: usize) -> Option<String> {
            unreachable!("asked from inside a call")
        }
        fn to_come(&self) -> usize {
            unreachable!("asked from inside a call")
        }
        fn carry_out(&mut self, _: &Command, _: &Ready) -> Carried {
            unreachable!("asked from inside a call")
        }
        fn handed_on(&mut self) -> bool {
            unreachable!("asked from inside a call")
        }
        fn handed_back(&mut self) {
            unreachable!("asked from inside a call")
        }
        fn pause(&mut self) -> bool {
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
        tells: Vec<Tells<'p>>,
        /// The number of the queue it holds: a new one each time a queue is put in whole.
        queue: u64,
        /// What goes wrong on the app's side, if anything does (see `Wrong`).
        wrong: Option<Wrong>,
    }

    /// A way the app's side of a call can go wrong.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Wrong {
        /// A queue to take does not go into the player: nothing changes.
        TakeNotInstalled,
        /// A command is carried out, and then the call into the app fails.
        ThrewAfterChanging,
        /// Told to stop, the player goes on sounding (by a command or by the phone itself).
        DoesNotStop,
        /// Told to stop, it does, and something has started it again when it is asked.
        StopsAndStartsAgain,
        /// The app's own rules will not start playing (a call is on).
        MayNotPlay,
        /// The app cannot record whose the queue is: the call fails.
        CannotRecord,
        /// Asked what it is playing, the call fails.
        Panics,
    }

    impl<'p> Mock<'p> {
        fn with(songs: Vec<String>, sound: Option<f64>) -> Mock<'p> {
            Mock { songs, sound, volume: 1.0, queue: 1, ..Mock::default() }
        }

        /// From here its listeners tell the linking of these, inside this call.
        fn tells(&mut self, phone: &'p Phone, inside: &'static str, what: Vec<Happened>) {
            self.tells_after(phone, inside, 0, what);
        }

        /// The same, inside that call once `after` of them have gone by.
        fn tells_after(&mut self, phone: &'p Phone, inside: &'static str, after: usize, what: Vec<Happened>) {
            self.tells.clear();
            self.tells_too(phone, inside, after, what);
        }

        /// And these as well as what it tells of already.
        fn tells_too(&mut self, phone: &'p Phone, inside: &'static str, after: usize, what: Vec<Happened>) {
            self.tells.push(Tells { phone, inside, after: Cell::new(after), what: RefCell::new(what), answers: RefCell::new(Vec::new()) });
        }

        fn tell(&self, inside: &str) {
            for tells in self.tells.iter().filter(|tells| tells.inside == inside) {
                if tells.after.get() > 0 {
                    tells.after.set(tells.after.get() - 1);
                    continue;
                }
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
        }

        /// What its listeners were answered with.
        fn told(&self) -> Vec<&'static str> {
            self.tells.iter().flat_map(|tells| tells.answers.borrow().clone()).collect()
        }

        fn claims(&self) -> usize {
            self.asked.iter().filter(|asked| *asked == "claim").count()
        }
    }

    impl PhonePlayer for Mock<'_> {
        fn brief(&self) -> Brief {
            if self.wrong == Some(Wrong::Panics) {
                panic!("the phone's player fell over");
            }
            self.tell("brief");
            Brief { playing: self.sound.is_some(), title: self.songs.first().cloned().unwrap_or_default(), artist: String::new() }
        }
        fn full(&self) -> Full {
            let entry = |id: &String| Entry { id: id.clone(), title: id.clone(), ..Default::default() };
            Full::fitted(self.sound.unwrap_or(0.0), self.songs.first().map(entry), self.songs.iter().skip(1).map(entry), self.songs.len().saturating_sub(1), "", false, 0, self.volume)
        }
        fn handed(&self) -> Handed {
            if self.radio { Handed::default() } else { Handed::fitted(&self.songs, 0, self.sound.unwrap_or(0.0), "the queue") }
        }
        fn sounding(&self) -> Option<Sounding> {
            self.sound.map(|at| Sounding { queue: self.queue, at })
        }
        fn queue(&self) -> u64 {
            self.tell("queue");
            self.queue
        }
        fn has_song(&self) -> bool {
            !self.songs.is_empty()
        }
        fn ahead(&self) -> Vec<String> {
            self.songs.iter().skip(1).cloned().collect()
        }
        fn entry_at(&self, at: usize) -> Option<String> {
            self.songs.get(1 + at).cloned()
        }
        fn to_come(&self) -> usize {
            self.songs.len().saturating_sub(1)
        }
        fn carry_out(&mut self, command: &Command, ready: &Ready) -> Carried {
            self.asked.push(command.name().to_string());
            let (changed, already, refused) = (Carried::Changed, Carried::AlreadySo, Carried::Refused);
            let carried = match (command, ready) {
                (Command::Pause | Command::Moved, _) if self.sound.is_none() => already,
                (Command::Pause | Command::Moved, _) if self.wrong == Some(Wrong::DoesNotStop) => refused,
                (Command::Pause | Command::Moved, _) if self.wrong == Some(Wrong::StopsAndStartsAgain) => changed,
                (Command::Pause | Command::Moved, _) => {
                    self.sound = None;
                    changed
                }
                (Command::Play, _) if self.sound.is_some() => already,
                (Command::Play, _) if self.wrong == Some(Wrong::MayNotPlay) => refused,
                (Command::Play, _) => {
                    self.sound = Some(0.0);
                    changed
                }
                (Command::Next, _) if self.songs.len() > 1 => {
                    self.songs.rotate_left(1);
                    changed
                }
                (Command::Seek { to }, _) if !self.radio => {
                    self.sound = Some(*to);
                    changed
                }
                (Command::Volume { to }, _) if self.volume == *to => already,
                (Command::Volume { to }, _) => {
                    self.volume = *to;
                    changed
                }
                (Command::Remove { .. }, Ready::Entry(entry)) => {
                    self.songs.remove(entry + 1);
                    changed
                }
                (Command::Add { .. }, Ready::Songs(songs)) => {
                    self.songs.extend(songs.iter().cloned());
                    changed
                }
                (Command::Adopt(adopt), Ready::Queue(songs, begin)) => {
                    self.songs = songs.iter().skip(*begin).map(|(id, _)| id.clone()).collect();
                    self.sound = Some(adopt.position);
                    self.queue += 1;
                    changed
                }
                _ => refused,
            };
            if carried == changed {
                self.tell("carry_out");
            }
            if self.wrong == Some(Wrong::ThrewAfterChanging) {
                panic!("the app threw after the player changed");
            }
            carried
        }
        fn handed_on(&mut self) -> bool {
            self.asked.push(format!("handed on, {}", if self.sound.is_some() { "sounding" } else { "silent" }));
            true
        }
        fn handed_back(&mut self) {
            self.asked.push("handed back".into());
        }
        fn pause(&mut self) -> bool {
            self.asked.push("pause, its own".into());
            if !matches!(self.wrong, Some(Wrong::DoesNotStop | Wrong::StopsAndStartsAgain)) {
                self.sound = None;
                self.tell("pause");
            }
            self.sound.is_none()
        }
        fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, _context: &str) {
            self.asked.push(format!("take {} from {begin}", songs.len()));
            if self.wrong == Some(Wrong::TakeNotInstalled) {
                return;
            }
            self.songs = songs.iter().skip(begin).map(|(id, _)| id.clone()).collect();
            self.sound = Some(position);
            self.queue += 1;
            self.tell("take");
        }
        fn claim(&mut self) {
            self.asked.push("claim".into());
            if self.wrong == Some(Wrong::CannotRecord) {
                panic!("the app could not record whose the queue is");
            }
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
        phone.library(1, Songs::of(all), 1_300, n, &mut player);
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
        let (phone, mut player, session, _) = phone(&mut n);
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
        // It calls itself what it is. (Said by the live session's stay: the same word
        // from a stay that has been ended switches nothing off.)
        phone.happened(Happened::FromRelay { session: session + 7, from: FromRelay::NoRandom }, 3_100, 100, &mut random(&mut n), &mut player);
        let (acts, _) = phone.handle(TICK, 3_150, 100, &mut random(&mut n), &mut player).unwrap();
        assert!(!acts.contains("couldn't make random numbers"), "{acts}");
        phone.happened(Happened::FromRelay { session, from: FromRelay::NoRandom }, 3_200, 100, &mut random(&mut n), &mut player);
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
                phone.library(1, Songs::of(all), 1_000, &mut n, player);
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
                    Do::Find { ticket, ids, .. } => {
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
        // And B is told its queue is A's now before it is asked to stop, so that the stop
        // is not saved there as a change of B's own.
        assert_eq!((pair.asked(0), pair.asked(1)), (vec!["take 4 from 0", "claim"], vec!["next", "claim", "handed on, sounding", "moved"]));
        assert_eq!((pair.said(0).last().copied(), pair.players[1].sound, pair.players[0].songs[0].as_str()), (Some("Playing here. Phone B is paused."), None, "s1"));
        assert!(pair.acts[0].contains(&Act::Do { ask: Do::Steering(None) }));
        assert_eq!((pair.players[0].told(), pair.players[1].told()), (vec!["part of it"], vec!["part of it"]));
        // And given back: A, playing, picks B, which takes the queue and plays; A is stopped
        // when B's sound has started. B's queue is B's by taking it; A's stop is the app's own.
        pair.players[0].tells(&phones[0], "pause", vec![changed()]);
        pair.event(0, &serde_json::json!({"event": "choose", "place": place, "in_a_jam": false}).to_string());
        pair.pass(1_500);
        assert_eq!((pair.said(0).last().copied(), pair.players[0].sound), (Some("Playing on Phone B."), None));
        assert_eq!((pair.asked(0), pair.asked(1)), (vec!["take 4 from 0", "claim", "handed on, sounding", "pause, its own"], vec!["next", "claim", "handed on, sounding", "moved", "adopt", "claim"]));
        assert_eq!(pair.players[0].told(), ["part of it"]);
    }

    /// "Play here" on a phone whose player does not take the queue, while what it held
    /// before goes on sounding: that sound is not the sound of the queue that was taken.
    /// Nothing is claimed and the other device is not told to stop.
    #[test]
    fn only_a_queue_that_went_in_is_claimed_and_only_its_sound_stops_the_other() {
        {
            let phones = two_phones();
            let mut pair = Pair::new(&phones);
            let place = text::place(&pair.places[1]);
            pair.event(0, &serde_json::json!({"event": "choose", "place": place, "in_a_jam": false}).to_string());
            pair.pass(500);
            // (A is sounding what it held before, as if the car had started it.)
            pair.players[0].sound = Some(1.0);
            pair.players[0].wrong = Some(Wrong::TakeNotInstalled);
            pair.event(0, r#"{"event":"play_here"}"#);
            pair.pass(2_000);
            assert_eq!((pair.asked(0), pair.asked(1)), (vec!["take 4 from 0"], vec![]));
            assert_eq!((pair.said(0).last().copied(), pair.players[1].sound.is_some(), order(&phones[0], pair.now).2), (Some("This phone couldn't load that. Phone B is still playing."), true, false));
        }
    }

    /// A phone told its music has moved that goes on sounding is not acknowledged as
    /// stopped, and its queue is its own still: what was marked is taken back.
    #[test]
    fn a_phone_that_does_not_stop_is_not_said_to_have_stopped() {
        let phones = two_phones();
        let mut pair = Pair::new(&phones);
        let place = text::place(&pair.places[1]);
        pair.players[1].wrong = Some(Wrong::DoesNotStop);
        pair.event(0, &serde_json::json!({"event": "choose", "place": place, "in_a_jam": false}).to_string());
        pair.pass(500);
        pair.event(0, r#"{"event":"play_here"}"#);
        pair.pass(1_500);
        assert_eq!(pair.asked(1), ["handed on, sounding", "moved", "handed back"]);
        assert_eq!((pair.said(0).last().copied(), pair.players[1].sound.is_some()), (Some("Playing here. Phone B couldn't be stopped, so it is still playing too."), true));
        // And the phone that gives its music away and does not stop when the other has started.
        let phones = two_phones();
        let mut pair = Pair::new(&phones);
        pair.players.swap(0, 1);
        pair.players[0].wrong = Some(Wrong::DoesNotStop);
        pair.pass(500);
        pair.event(0, &serde_json::json!({"event": "choose", "place": place, "in_a_jam": false}).to_string());
        pair.pass(1_500);
        assert_eq!(pair.asked(0), ["handed on, sounding", "pause, its own", "handed back"]);
        assert_eq!((pair.said(0).last().copied(), pair.players[0].sound.is_some(), pair.players[1].sound.is_some()), (Some("Phone B has started, and this phone couldn't be stopped. Both are playing."), true, true));
        assert!(!pair.acts[0].iter().any(|act| matches!(act, Act::Do { ask: Do::Steering(Some(_)) })));
    }

    /// The tell a phone sent to the other place in the room, opened.
    fn answered(came: &Came, place: &[u8; 16]) -> super::super::message::Tell {
        let Some(Do::Send { frame: sent, .. }) = came.native.iter().find(|ask| matches!(ask, Do::Send { .. })) else { panic!("no answer: {came:?}") };
        let opened = frame::open(&Keys::of(&secret()), sent, place, &[1; 16]).expect("the answer opens");
        read_tell(&opened.message).expect("a tell")
    }

    /// The phone's own rules will not start playing (`mayPlay()` says no: a call is on).
    /// That is a refusal, said as one: not "it was so already", which would be answered
    /// as done and let the sender's next press go.
    #[test]
    fn a_phone_that_may_not_play_refuses_and_does_not_say_it_was_so_already() {
        let mut n = 0;
        let (phone, mut player, session, place) = phone(&mut n);
        player.sound = None;
        player.wrong = Some(Wrong::MayNotPlay);
        let came = hear(&phone, &mut player, session, &place, vec![command(1, &place, Command::Play, 1)], 2_000, &mut n);
        let tell = answered(&came, &place);
        assert_eq!((tell.ok, tell.why, tell.by, tell.done, tell.playing), (Some(false), Some(Why::Cannot), None, false, false));
        assert_eq!((order(&phone, 2_000), player.claims()), ((1, None, false), 0));
        // Play while it is playing is so already, and is said as that.
        player.wrong = None;
        player.sound = Some(1.0);
        let came = hear(&phone, &mut player, session, &place, vec![command(2, &place, Command::Play, 1)], 2_100, &mut n);
        let tell = answered(&came, &place);
        assert_eq!((tell.ok, tell.why, order(&phone, 2_100)), (Some(true), Some(Why::Nothing), (1, None, false)));
    }

    /// Told its music has moved, the phone pauses and something starts it again before
    /// it is asked whether it stopped. The command changed something and the phone has
    /// not stopped: its answer says it could not, and does not also name the command as
    /// carried out; the queue is the phone's own still.
    #[test]
    fn a_stop_is_not_recorded_until_the_phone_says_it_has_stopped() {
        let mut n = 0;
        let (phone, mut player, session, place) = phone(&mut n);
        player.wrong = Some(Wrong::StopsAndStartsAgain);
        let came = hear(&phone, &mut player, session, &place, vec![command(1, &place, Command::Moved, 1)], 2_000, &mut n);
        let tell = answered(&came, &place);
        assert_eq!((tell.ok, tell.why, tell.by, tell.done, tell.playing, tell.at.generation), (Some(false), Some(Why::Cannot), None, false, true, 2));
        assert_eq!(player.asked, ["handed on, sounding", "moved", "handed back"]);
        assert_eq!(order(&phone, 2_000), (2, None, false));
    }

    /// A call up that changed the player and then failed, at whatever point: there is no
    /// answer for it, and none is made up. It is a fault. Nothing is sent to the command's
    /// sender (which finds out by asking, and is told nothing by a device that is off),
    /// and the phone's player is asked nothing more.
    #[test]
    fn a_call_into_the_player_that_fails_is_a_fault_whatever_it_had_done() {
        for (wrong, command_sent, asked) in [(Wrong::ThrewAfterChanging, Command::Next, vec!["next"]), (Wrong::CannotRecord, Command::Pause, vec!["pause", "claim"])] {
            let mut n = 0;
            let (phone, mut player, session, place) = phone(&mut n);
            player.wrong = Some(wrong);
            let came = hear(&phone, &mut player, session, &place, vec![command(1, &place, command_sent, 1), command(2, &place, Command::Play, 2)], 2_000, &mut n);
            assert!(came.acts.contains(&Act::Fault) && sends(&came.native) == 0 && came.native.contains(&Do::End { session }), "{wrong:?}: {came:?}");
            assert_eq!(player.asked, asked, "{wrong:?}");
            let (acts, native) = phone.handle(TICK, 2_250, 100, &mut random(&mut n), &mut player).unwrap();
            assert!(native.is_empty() && acts.contains("Linking has stopped on this phone"), "{wrong:?}: {acts} {native:?}");
        }
        // And the same for something chosen on the phone itself, which the app could not record.
        let mut n = 0;
        let (phone, mut player, ..) = phone(&mut n);
        player.wrong = Some(Wrong::CannotRecord);
        let (acts, native) = phone.handle(CHANGED, 2_300, 100, &mut random(&mut n), &mut player).unwrap();
        assert!(acts.contains(r#""act":"fault""#) && logged(&native, "linking: it stopped unexpectedly (the app could not record whose the queue is)"), "{acts} {native:?}");
    }

    /// The app says the sign-in is over from inside one of the library's own calls up (a
    /// listener that heard the player change, and found the account gone). It has switched
    /// the player already, or is about to: from that moment nothing more is asked of the
    /// player for the old sign-in, nothing that was being decided is answered, and the
    /// rest of what the relay handed over is not carried out on the next account's player.
    #[test]
    fn the_end_of_the_sign_in_arriving_inside_a_call_is_applied_at_once() {
        for another_account in [false, true] {
            let mut n = 0;
            let (phone, player, session, place) = phone(&mut n);
            let mut player: Mock = player;
            let end = if another_account { Event::Tick { account: "somebody else".into(), wanted: true } } else { Event::Stop };
            player.tells(&phone, "carry_out", vec![Happened::Event(end)]);
            // Three commands in one hand-over; the first is being carried out when it ends.
            let frames = vec![command(1, &place, Command::Pause, 1), command(2, &place, Command::Play, 2), command(3, &place, Command::Next, 3)];
            let came = hear(&phone, &mut player, session, &place, frames, 2_000, &mut n);
            assert_eq!(player.told(), ["kept"], "another account: {another_account}");
            // Not the claim for the first, and nothing at all for the others.
            assert_eq!(player.asked, ["pause"], "another account: {another_account}");
            assert!(sends(&came.native) == 0 && came.native.contains(&Do::End { session }), "{came:?}");
            assert!(logged(&came.native, "the sign-in ended while something was being decided"), "{came:?}");
            assert_eq!(came.sign_in, 2);
            assert_eq!(order(&phone, 2_000), (1, None, false));
            // Where it was another account, that account's own look begins in the same call.
            assert_eq!(came.native.iter().any(|ask| matches!(ask, Do::Look { .. })), another_account, "{came:?}");
        }
    }

    /// The app says the sign-in is over from inside a call that only **reads** the player
    /// (it is asked the number of its queue, a listener runs, and finds the account
    /// gone). The fence is looked at again after every call into the player, before the
    /// next thing is done with what it answered: the command that was about to be carried
    /// out is not carried out on a player that may be the next account's.
    #[test]
    fn the_end_of_the_sign_in_arriving_inside_a_read_stops_the_act_that_was_to_follow() {
        // A command: the queue's number is read once by the deciding and once more
        // immediately before the player is changed. It ends inside the second.
        let mut n = 0;
        let (phone, player, session, place) = phone(&mut n);
        let mut player: Mock = player;
        player.tells_after(&phone, "queue", 1, vec![Happened::Event(Event::Stop)]);
        let came = hear(&phone, &mut player, session, &place, vec![command(1, &place, Command::Pause, 1)], 2_000, &mut n);
        assert_eq!((player.told(), player.asked.clone(), player.sound.is_some()), (vec!["kept"], Vec::<String>::new(), true), "{came:?}");
        assert!(sends(&came.native) == 0 && came.native.contains(&Do::End { session }), "{came:?}");
        // "Play here": the queue that was asked for arrives, and it ends inside the read
        // that comes before the queue is put in the player.
        let phones = two_phones();
        let mut pair = Pair::new(&phones);
        let place = text::place(&pair.places[1]);
        pair.event(0, &serde_json::json!({"event": "choose", "place": place, "in_a_jam": false}).to_string());
        pair.pass(500);
        pair.players[0].tells(&phones[0], "queue", vec![Happened::Event(Event::Stop)]);
        pair.event(0, r#"{"event":"play_here"}"#);
        pair.pass(1_000);
        assert_eq!((pair.players[0].told(), pair.asked(0), pair.players[0].songs[0].as_str()), (vec!["kept"], vec![], "a0"));
    }

    /// The sign-in ends while the second of two things in one call is being decided. What
    /// came of the first is of the sign-in that is over too: **the whole of what the call
    /// had come to is dropped**, not only the part that was under way. (But for what ends
    /// something on the app's side, which is still to be ended.)
    #[test]
    fn a_call_overtaken_by_the_end_of_the_sign_in_drops_all_it_had_come_to() {
        let mut n = 0;
        let (phone, player, session, place) = phone(&mut n);
        let mut player: Mock = player;
        // A look is being answered, and while it is the relay hands over a command; while
        // that command is carried out, the app says the sign-in is over.
        let from = FromRelay::Heard { places: vec![text::place(&place), text::place(&[1; 16])], frames: vec![command(2, &place, Command::Pause, 1)] };
        player.tells(&phone, "brief", vec![Happened::FromRelay { session, from }]);
        player.tells_too(&phone, "carry_out", 0, vec![Happened::Event(Event::Stop)]);
        let came = hear(&phone, &mut player, session, &place, vec![from_pc(1, &place, Ask::Look(Want::Brief))], 2_000, &mut n);
        assert_eq!((player.told(), player.asked.clone()), (vec!["kept", "kept"], vec!["pause".to_string()]));
        assert!(sends(&came.native) == 0 && came.acts.is_empty(), "{came:?}");
        assert!(came.native.contains(&Do::End { session }) && !logged(&came.native, "pause from"), "{came:?}");
        assert_eq!((came.sign_in, phone.sign_in()), (2, 2));
    }

    /// What an app reads for a sign-in away from the player's thread carries the number
    /// of that sign-in, not the account's name. Signing out and in again **as the same
    /// account** is another sign-in: a worker still reading the library for the one
    /// before cannot give its copy to this one.
    #[test]
    fn a_copy_of_the_library_read_for_the_sign_in_before_is_dropped_though_the_account_is_the_same() {
        let mut n = 0;
        let (phone, mut player, ..) = phone(&mut n);
        let has = |phone: &Phone| phone.inner.borrow().library.0.len();
        let before = phone.sign_in();
        assert_eq!((before, has(&phone)), (1, 8));
        phone.handle(r#"{"event":"stop"}"#, 2_000, 100, &mut random(&mut n), &mut player);
        phone.handle(TICK, 2_100, 100, &mut random(&mut n), &mut player);
        assert_eq!((phone.sign_in(), has(&phone)), (2, 0));
        // The old worker finishes, late.
        let late = phone.library(before, Songs::of(ids(&[0, 1, 2])), 2_200, &mut n, &mut player);
        assert!(has(&phone) == 0 && logged(&late.native, "library: a copy read for a sign-in that is over, dropped"), "{late:?}");
        // This sign-in's own copy is put in place, before a tick or after one.
        phone.library(2, Songs::of(ids(&[0, 1])), 2_300, &mut n, &mut player);
        assert_eq!(has(&phone), 2);
        phone.handle(r#"{"event":"stop"}"#, 2_400, 100, &mut random(&mut n), &mut player);
        phone.library(3, Songs::of(ids(&[0, 1, 2, 3])), 2_500, &mut n, &mut player);
        assert_eq!(has(&phone), 4);
    }

    /// The end of a sign-in is never refused and never waits behind what was kept: with
    /// as much waiting as is kept at all, it is taken, and what was waiting is dropped.
    #[test]
    fn the_end_of_the_sign_in_is_not_refused_however_much_is_waiting() {
        let mut n = 0;
        let (phone, player, session, place) = phone(&mut n);
        let mut player: Mock = player;
        let mut told: Vec<Happened> = (0..300).map(|ticket| Happened::Event(Event::LookedUp { ticket })).collect();
        told.push(Happened::Event(Event::Stop));
        told.push(Happened::Event(Event::Changed { who: Who::Here, what: What::Playing }));
        player.tells(&phone, "brief", told);
        let came = hear(&phone, &mut player, session, &place, vec![from_pc(1, &place, Ask::Look(Want::Brief))], 2_000, &mut n);
        let answers = player.told();
        let count = |of: &str| answers.iter().filter(|answer| **answer == of).count();
        assert_eq!((count("kept"), count("refused"), answers[300], answers[301]), (super::super::turn::MOST_KEPT + 2, 300 - super::super::turn::MOST_KEPT, "kept", "kept"));
        assert!(came.native.contains(&Do::End { session }) && sends(&came.native) == 0, "{came:?}");
        // (What was chosen on the phone after it was over is the next sign-in's first choice.)
        assert_eq!((order(&phone, 2_000), player.asked.as_slice()), ((2, None, false), ["claim".to_string()].as_slice()));
    }

    /// A call into the player cannot fail. If one does (the app threw, a read could not be
    /// made), or anything else inside the library stops unexpectedly, that is a fault: it
    /// is not answered with a made-up value and it is not tried again. Everything the
    /// linking held is replaced, as at a sign-out, with one line said of it; nothing more
    /// is asked of the player and nothing is sent; and **linking stays off on this phone
    /// until the app is next started**, so that a fault that is still there cannot come
    /// round again at every tick, and nothing the app does next (a sign-out, another
    /// account) has to be told from a fresh start to bring it back.
    #[test]
    fn a_fault_puts_linking_off_until_the_app_starts_again_and_asks_nothing_more_of_the_player() {
        let mut n = 0;
        let (phone, mut player, session, place) = phone(&mut n);
        phone.handle(CHANGED, 2_000, 100, &mut random(&mut n), &mut player);
        phone.handle(r#"{"event":"looking_up"}"#, 2_000, 100, &mut random(&mut n), &mut player);
        assert_eq!(order(&phone, 2_000), (2, None, true));
        player.wrong = Some(Wrong::Panics);
        let came = hear(&phone, &mut player, session, &place, vec![from_pc(1, &place, Ask::Look(Want::Brief))], 2_100, &mut n);
        assert!(came.native.contains(&Do::End { session }) && came.native.contains(&Do::StopLook) && sends(&came.native) == 0, "{came:?}");
        assert!(logged(&came.native, "linking: it stopped unexpectedly (the phone's player fell over)"), "{came:?}");
        assert_eq!(came.native.iter().filter(|ask| matches!(ask, Do::Log(_))).count(), 1);
        // The app is told, so that it can put right what only it can (8.1).
        assert!(came.acts.contains(&Act::Fault), "{came:?}");
        // Nothing is left of what it held, and it is not left thinking it is inside a call.
        assert_eq!(order(&phone, 2_200), (1, None, false));
        // The fault is still there. Nothing finds that out: at tick after tick the player
        // is asked nothing, no look begins, nothing is sent, and the list says why.
        let asked = player.asked.len();
        for tick in 0..8 {
            let (acts, native) = phone.handle(TICK, 2_200 + 250 * tick, 100, &mut random(&mut n), &mut player).unwrap();
            assert!(native.is_empty() && acts.contains("Linking has stopped on this phone until the app is next opened."), "{acts} {native:?}");
        }
        // What the old session's stay at the relay still brings, what is chosen on the
        // phone, a press: none of it is decided, and a press is the phone's own.
        let came = hear(&phone, &mut player, session, &place, vec![command(2, &place, Command::Pause, 1)], 4_300, &mut n);
        assert!(came.native.is_empty() && came.acts.is_empty(), "{came:?}");
        assert_eq!(phone.handle(CHANGED, 4_400, 100, &mut random(&mut n), &mut player).unwrap().0, "[]");
        assert_eq!(phone.handle(r#"{"event":"press","press":"next"}"#, 4_400, 100, &mut random(&mut n), &mut player).unwrap().0, r#"[{"act":"handled","by_another":false}]"#);
        assert_eq!((player.asked.len(), order(&phone, 4_500)), (asked, (1, None, false)));
        // Nothing in this run of the app brings it back: not a sign-out and the same
        // account again, and not another account. (The fault may be gone; nobody asks.)
        player.wrong = None;
        assert_eq!(phone.handle(r#"{"event":"stop"}"#, 5_000, 100, &mut random(&mut n), &mut player).unwrap(), ("[]".to_string(), vec![]));
        let (acts, native) = phone.handle(TICK, 5_100, 100, &mut random(&mut n), &mut player).unwrap();
        assert!(native.is_empty() && acts.contains("Linking has stopped on this phone"), "{acts} {native:?}");
        let (acts, native) = phone.handle(r#"{"event":"tick","account":"somebody else","wanted":true}"#, 5_400, 100, &mut random(&mut n), &mut player).unwrap();
        assert!(native.is_empty() && acts.contains("Linking has stopped on this phone"), "{acts} {native:?}");
        assert_eq!(player.asked.len(), asked);
    }

    /// A call works through what was waiting and a bounded number of what arrives while
    /// it runs, and says when there is more: an app's callback that feeds it as fast as it
    /// runs does not keep it running.
    #[test]
    fn a_call_says_when_more_arrived_than_it_works_through() {
        use super::super::turn::MOST_MORE;
        let mut n = 0;
        let (phone, player, session, place) = phone(&mut n);
        let mut player: Mock = player;
        let told = (0..MOST_MORE + 6).map(|_| Happened::Event(Event::Changed { who: Who::Nobody, what: What::Playing })).collect();
        player.tells(&phone, "brief", told);
        let first = hear(&phone, &mut player, session, &place, vec![from_pc(1, &place, Ask::Look(Want::Brief))], 2_000, &mut n);
        assert_eq!((first.more, order(&phone, 2_000).0), (true, 1 + MOST_MORE as u64));
        // The rest is first in the next call, which then has nothing left over.
        let next = came(phone.happened(Happened::Event(serde_json::from_str(TICK).unwrap()), 2_100, 100, &mut random(&mut n), &mut player));
        assert_eq!((next.more, order(&phone, 2_100).0), (false, 1 + MOST_MORE as u64 + 6));
    }

    /// The songs a look-up found, as its thread hands them over.
    fn found(ticket: u64) -> Happened {
        Happened::Found { ticket, songs: vec!["s1".into()] }
    }

    /// **The one drain.** What a call leaves for the next is carried on with by a turn
    /// that brings nothing new: the app does not have to wait for something else to
    /// happen, and has no loop of its own to get wrong.
    #[test]
    fn a_turn_with_nothing_new_carries_on_with_what_the_library_kept() {
        use super::super::turn::MOST_MORE;
        let mut n = 0;
        let (phone, player, session, place) = phone(&mut n);
        let mut player: Mock = player;
        let told = (0..2 * MOST_MORE + 6).map(|_| Happened::Event(Event::Changed { who: Who::Nobody, what: What::Playing })).collect();
        player.tells(&phone, "brief", told);
        let first = hear(&phone, &mut player, session, &place, vec![from_pc(1, &place, Ask::Look(Want::Brief))], 2_000, &mut n);
        assert_eq!((first.more, order(&phone, 2_000).0), (true, 1 + MOST_MORE as u64));
        // Nothing has been brought: the inbox is empty. The turn carries on, a bounded
        // number of things at a time, and says when there is no more.
        let inbox = Inbox::default();
        let next = came(phone.turn(&inbox, 2_100, 100, &mut random(&mut n), &mut player));
        assert_eq!((next.more, order(&phone, 2_100).0), (true, 1 + 2 * MOST_MORE as u64));
        let last = came(phone.turn(&inbox, 2_200, 100, &mut random(&mut n), &mut player));
        assert_eq!((last.more, order(&phone, 2_200).0), (false, 1 + 2 * MOST_MORE as u64 + 6));
        let none = came(phone.turn(&inbox, 2_300, 100, &mut random(&mut n), &mut player));
        assert_eq!((none.more, none.sign_in, order(&phone, 2_300).0), (false, 1, 1 + 2 * MOST_MORE as u64 + 6));
    }

    /// What the requests bring waits in one bounded place, counted at its real size, and
    /// is worked through a bounded number of things at a turn, the oldest first. What
    /// does not fit is handed back to whoever brought it: nothing is dropped.
    #[test]
    fn what_the_requests_bring_waits_in_a_bounded_place_and_is_worked_through_a_turn_at_a_time() {
        use super::super::turn::MOST_MORE;
        let mut n = 0;
        let (phone, mut player, session, place) = phone(&mut n);
        let inbox = Inbox::default();
        // By number.
        for ticket in 0..MOST_KEPT as u64 {
            assert!(inbox.put(found(ticket)).is_ok(), "{ticket}");
        }
        let Err(Happened::Found { ticket: 7_000, .. }) = inbox.put(found(7_000)) else { panic!("one more than may wait was taken") };
        // A turn takes so many, and says there is more; when room has been made, what
        // was handed back is taken.
        let first = came(phone.turn(&inbox, 2_000, 100, &mut random(&mut n), &mut player));
        assert_eq!((first.more, inbox.waiting().things.len()), (true, MOST_KEPT - MOST_MORE));
        assert!(inbox.put(found(7_000)).is_ok());
        let mut turns = 1;
        loop {
            turns += 1;
            if !came(phone.turn(&inbox, 2_000, 100, &mut random(&mut n), &mut player)).more {
                break;
            }
        }
        assert_eq!((turns, inbox.is_empty(), inbox.waiting().bytes), ((MOST_KEPT + 1).div_ceil(MOST_MORE), true, 0));
        // By size: each thing is counted at what it holds.
        let large = |seq: u8| {
            let frame = "x".repeat(300_000);
            Happened::FromRelay { session, from: FromRelay::Heard { places: vec![text::place(&place)], frames: vec![(text::place(&[seq; 16]), frame)] } }
        };
        assert!(inbox.put(large(1)).is_ok());
        assert!(inbox.waiting().bytes > 300_000 && inbox.waiting().bytes < 301_000, "{}", inbox.waiting().bytes);
        assert!(inbox.put(large(2)).is_ok() && inbox.put(large(3)).is_ok() && inbox.put(large(4)).is_err());
        // (There is room still for what is small.)
        assert!(inbox.put(found(1)).is_ok() && inbox.waiting().bytes <= MOST_KEPT_BYTES);
        // Something that happened on the phone is not brought by a request, and is not taken.
        inbox.clear();
        assert!(inbox.put(Happened::Event(Event::Stop)).is_err());
        // In the order it was brought: two commands are each decided against the player
        // as the one before left it.
        for (n, command_sent, generation) in [(1, Command::Pause, 1), (2, Command::Play, 2)] {
            let from = FromRelay::Heard { places: vec![text::place(&place), text::place(&[1; 16])], frames: vec![command(n, &place, command_sent, generation)] };
            assert!(inbox.put(Happened::FromRelay { session, from }).is_ok());
        }
        let both = came(phone.turn(&inbox, 3_000, 100, &mut random(&mut n), &mut player));
        assert_eq!((sends(&both.native), both.more, order(&phone, 3_000)), (2, false, (3, Some([2; 24]), false)));
    }

    /// The phone's copy of the library does not wait in line, and a copy read for an
    /// earlier sign-in cannot push out one read for a later sign-in, whichever of the
    /// two threads gets there first.
    #[test]
    fn a_stale_copy_of_the_library_cannot_displace_a_newer_one() {
        let mut n = 0;
        let (phone, mut player, ..) = phone(&mut n);
        let has = |phone: &Phone| phone.inner.borrow().library.0.len();
        phone.handle(r#"{"event":"stop"}"#, 2_000, 100, &mut random(&mut n), &mut player);
        assert_eq!((phone.sign_in(), has(&phone)), (2, 0));
        let inbox = Inbox::default();
        // This sign-in's copy is waiting when the old worker finishes.
        inbox.library(2, Songs::of(ids(&[0, 1])));
        inbox.library(1, Songs::of(ids(&[0, 1, 2, 3, 4])));
        phone.turn(&inbox, 2_100, 100, &mut random(&mut n), &mut player);
        assert_eq!(has(&phone), 2);
        // The other way about: the old worker's is waiting, and this sign-in's replaces it.
        inbox.library(1, Songs::of(ids(&[0, 1, 2, 3, 4])));
        inbox.library(2, Songs::of(ids(&[0, 1, 2])));
        phone.turn(&inbox, 2_200, 100, &mut random(&mut n), &mut player);
        assert_eq!(has(&phone), 3);
        // Alone, the old worker's is dropped when it is looked at, and said.
        inbox.library(1, Songs::of(ids(&[0])));
        let late = came(phone.turn(&inbox, 2_300, 100, &mut random(&mut n), &mut player));
        assert!(has(&phone) == 3 && logged(&late.native, "library: a copy read for a sign-in that is over, dropped"), "{late:?}");
        // The sign-in has its next number from the moment it is said to be over, even
        // from inside a call: what the app begins then is tagged for the next sign-in,
        // and is not put in place before the end has been applied.
        // (As a call leaves it that had handed over all it may when the app said so: the
        // end is kept, first for the next turn.)
        let running = phone.turns.carry_on().expect("nothing is running");
        assert_eq!(phone.happened(Happened::Event(Event::Stop), 2_400, 100, &mut random(&mut n), &mut player), Answer::Kept);
        drop(running);
        assert_eq!((phone.sign_in(), has(&phone)), (3, 3));
        inbox.library(phone.sign_in(), Songs::of(ids(&[0, 1, 2, 3])));
        phone.turn(&inbox, 2_500, 100, &mut random(&mut n), &mut player);
        assert_eq!(has(&phone), 4);
    }

    /// While linking is off after a fault, what the requests still bring is let go of
    /// unread, and nothing waits.
    #[test]
    fn while_linking_is_off_what_the_requests_bring_is_let_go_of() {
        let mut n = 0;
        let (phone, mut player, session, place) = phone(&mut n);
        player.wrong = Some(Wrong::Panics);
        hear(&phone, &mut player, session, &place, vec![from_pc(1, &place, Ask::Look(Want::Brief))], 2_100, &mut n);
        let inbox = Inbox::default();
        assert!(inbox.put(found(1)).is_ok());
        inbox.library(phone.sign_in(), Songs::of(ids(&[0])));
        let nothing = came(phone.turn(&inbox, 2_200, 100, &mut random(&mut n), &mut player));
        assert!(nothing.acts.is_empty() && nothing.native.is_empty() && !nothing.more && inbox.is_empty(), "{nothing:?}");
        assert_eq!(phone.inner.borrow().library.0.len(), 0);
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
        phone.library(1, Songs::of(library.clone()), 1_400, &mut n, &mut player);
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
            phone.library(1, songs, now, &mut n, &mut player);
        }
        eprintln!("{:<58} {:>7.1} microseconds", "a library of fifty thousand songs put in place", began.elapsed().as_secs_f64() * 1e6 / 20.0);
    }
}
