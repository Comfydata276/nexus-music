//! One device among a person's devices: everything it decides, as plain state.
//!
//! [`Device`] is the session (one sign-in with one secret), what is known of each other
//! device in the room, what has been asked of each and what is waiting to be, the device
//! being steered, a move of music in either direction, the news, and the list. It owns no
//! thread, clock, file or socket. The app tells it what has happened and when (the tick,
//! a look's answer, what the relay handed over, what was pressed) and it says what to do
//! ([`Do`]): look at the secret, keep a place in this room, send this frame, say this to
//! the person. What it needs of the app while it decides it asks through two small traits:
//!
//! - [`Player`]: what is playing and sounding on this device, its order of choices
//!   ([`super::rule::Rule`]), and "carry out this command", which the app does by the very
//!   code the same press runs.
//! - [`crate::requests::Library`]: which of these songs this device has.
//!
//! and it is handed the time (milliseconds, on a clock that goes on counting while the
//! device sleeps), the day, and random bytes from the system.
//!
//! **The rule of rules** (`docs/linking-design.md`, 7): a command is validated
//! ([`super::message::read_ask`]), authorised (the frame opened; [`Rule::admit`]), given
//! its place (the reservation), worked for (its entry found, its songs looked up), and at
//! the moment it is carried out authorised again ([`Rule::finish`], under the session it
//! arrived in). Each of those is a line of [`Device::commanded`] and [`Device::finish`].

use std::collections::HashMap;

use super::command::{Adopt, Command, MOST_ADDED, id_ok};
use super::frame::{self, ASK, Keys, TELL};
use super::look::{Looked, ends_session};
use super::message::{Ask, At, Full, Handed, Tell, Want, Why, read_ask, read_tell};
use super::moving::{Adopted, Adopting, Gave, Giving, MOVE_MS, START_MS, Taking, Took};
use super::news::News;
use super::rule::{self, Carrying, Reservation, Waiting};
use super::secret::{self, Secret};
use super::seen::{self, Seen as Remembered};
use super::steer::{Outcome, Peer, Press as Pressed, Tick, Told, Wait};
use super::view::{Press, View};
use super::words::{self, Here};
use super::{Asks, DeviceId, LOOK_AGAIN_MS, LOOK_EVERY_MS, LOOK_IDLE_MS, Nonce, Place, Token, WAIT_MS, minute, secret_holds, text};
use crate::requests::Library;

/// How long a device that was being steered may be gone from the room before the player
/// goes back to this device.
pub const LOST_MS: u64 = 45_000;
/// How far the sound has to have moved on, in seconds, to count as having started: a song
/// that has been opened is not yet sound.
pub const SOUNDS_AFTER: f64 = 0.2;

/// What a device says it is playing, in brief.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Brief {
    pub playing: bool,
    pub title: String,
    pub artist: String,
}

/// What a command needs found before it can be carried out, as the app is handed it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ready {
    Nothing,
    /// The entry of the queue it means, as the queue stands now: its place after the
    /// current song.
    Entry(usize),
    /// The songs to queue, by id: the ones this device has.
    Songs(Vec<String>),
    /// The queue to take: each song this device has, and whether it was queued by hand;
    /// and where in them to start.
    Queue(Vec<(String, bool)>, usize),
}

/// What came of asking the player to carry a command out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carried {
    /// It changed the player.
    Changed,
    /// The player is as it was: it was so already, or could not do it.
    Unchanged,
    /// The player could not say what it did (asking it went wrong part-way, in an app
    /// whose player is not in this language). It may have changed. The deciding then goes
    /// by what the player says of itself when asked afresh, counts a change that no
    /// command is named for, and tells the command's sender that it is not known.
    Unknown,
}

/// Where the sound has got to, and in which queue.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Sounding {
    /// The number of the queue the sounding song belongs to ([`Player::queue`]).
    pub queue: u64,
    /// How far into the song, in seconds.
    pub at: f64,
}

/// This device's own player, as the deciding needs it. **It is asked, and has answered
/// when the call returns**: the deciding and the player live on one thread. (For a player
/// that tells its listeners of a change before the call that made it returns, see
/// `super::turn`.)
pub trait Player {
    fn brief(&self) -> Brief;
    /// What a device steering this one shows.
    fn full(&self) -> Full;
    /// What is playing and to come, by id, the song that is playing first; nothing, if
    /// what is playing is not something another device could carry on with (a radio).
    fn handed(&self) -> Handed;
    /// Where the sound has got to in the song the player holds, if it is playing and the
    /// sound has reached that song: sound that is advancing, not a place the player has
    /// been told to go to. And which queue that song belongs to.
    fn sounding(&self) -> Option<Sounding>;
    /// The number of the queue in the player. **It changes each time a queue is put in
    /// the player whole** (a queue taken from another device, by `take` or by an `adopt`
    /// carried out) and is never used twice in a run. It is how a move knows the sound it
    /// waits for is the sound of the queue it installed, and not of the one that was
    /// there before and went on playing because the new one never went in.
    fn queue(&self) -> u64;
    /// If the sound has rolled into the next song and the window has not yet been told,
    /// brings the window up to it now (the cursor moves, and the order of choices with
    /// it). Says whether it had to.
    fn catch_up(&mut self) -> bool;
    /// Whether there is a song in the player at all.
    fn has_song(&self) -> bool;
    /// The ids of the songs to come, in the order they would play.
    fn ahead(&self) -> Vec<String>;
    /// Carries a command out, exactly as the same press on this device would be, on this
    /// device only, **and says whether it changed anything**: a Play when it was playing, a
    /// seek on a radio, a Next with nothing to go to, change nothing. What the player does
    /// for it is not counted by the player: the rule counts it, once, if it changed anything.
    fn carry_out(&mut self, command: &Command, ready: &Ready) -> Carried;
    /// **This device's queue has been handed to another of the person's devices**, which
    /// is playing it by the person's own choice: from this moment it is here as a queue
    /// taken up from that device and not touched (`crate::handover::HANDED`). It is not
    /// saved, and it follows what that device saves, until the person uses it here.
    /// Said before this device's sound is stopped for it, so that the stop is not saved
    /// as a change of this device's own. Counts nothing and claims nothing. Says whether
    /// it could be marked so: if not, the stop goes ahead all the same, and it is told.
    fn handed_on(&mut self) -> bool;
    /// The hand-over did not happen after all (this device could not be stopped): the
    /// queue is as it was before [`Player::handed_on`].
    fn handed_back(&mut self);
    /// Stops this device's own sound, and counts it as a change.
    fn pause(&mut self);
    /// Takes a queue brought from another device and plays it: each song and whether it
    /// was queued by hand, where to start, and how far in. **Says whether the queue is in
    /// the player**; if it is, [`Player::queue`] has a new number and it is counted as a
    /// change, and if not, nothing is counted and nothing is claimed.
    fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, context: &str) -> bool;

    // The order of choices (see `super::rule`), which the app keeps beside its player
    // because every way into the player counts there.
    fn generation(&self) -> u64;
    fn busy(&self, now_ms: u64) -> bool;
    fn by(&self) -> Option<Nonce>;
    fn admit(&mut self, at: At, minute: u64, now_ms: u64) -> Result<Reservation, Why>;
    fn finish(&mut self, reservation: Reservation, at: At, minute: u64, now_ms: u64, same_session: bool) -> Result<Carrying, Why>;
    /// (`by` is the command to name as having made the state, if it changed anything:
    /// none, where what it did is not known.)
    fn carried(&mut self, carrying: Carrying, changed: bool, by: Option<Nonce>);
    fn refuse(&mut self, reservation: Reservation);
    fn wait(&mut self, now_ms: u64, for_ms: u64) -> Waiting;
    fn done(&mut self, waiting: Waiting);
}

/// Who this device is to the others.
#[derive(Debug, Clone, PartialEq)]
pub struct Me {
    /// Made once for the installation. Not secret, and only ever sent sealed.
    pub device: DeviceId,
    /// What the person calls it.
    pub name: String,
    pub here: Here,
}

/// What the app hands over with each thing that happens.
pub struct Env<'a> {
    /// Milliseconds on a clock that goes on counting while the device sleeps.
    pub now_ms: u64,
    /// Today, in days since 1970: for forgetting devices not seen in a month.
    pub today: u64,
    pub me: &'a Me,
    pub player: &'a mut dyn Player,
    pub library: &'a dyn Library,
    /// Fills these bytes from the system's source of randomness, or says it could not.
    /// Nothing is ever made up another way.
    pub random: &'a mut dyn FnMut(&mut [u8]) -> bool,
}

impl Env<'_> {
    fn nonce(&mut self) -> Option<Nonce> {
        let mut bytes = [0u8; 24];
        (self.random)(&mut bytes).then_some(bytes)
    }
}

/// What the device asks of the app.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Do {
    /// Write this down, if anything is being written down (for watching copies from
    /// outside). Never a secret, a key or a token.
    Log(String),
    /// Say this to the person where they are looking (a toast).
    Say(String),
    /// Look at the secret on the music server (`super::look::look`), away from this
    /// thread, and answer with [`Device::looked`] under this number.
    Look { number: u64, used: Option<String>, reset: bool },
    /// The sign-in has ended: the look that is under way makes no further request.
    StopLook,
    /// A session has begun: keep a place in this room (`super::relay::keep_place`) for as
    /// long as it lasts, and answer under its number.
    Begin { session: u64, room: String, since_ms: u64 },
    /// The session's secret was read again: it is good for ten minutes from here.
    Renew { session: u64, since_ms: u64 },
    /// The session is over: give up its place, and send nothing more that was queued under it.
    End { session: u64 },
    /// Send this frame (`super::relay::send`), unless the session has ended or run out of
    /// time when its turn comes ([`may_go`]).
    Send { session: u64, token: Token, to: Place, frame: String },
    /// Some of the songs a command brings are not in this device's copy of the library:
    /// ask the music server for them, and answer with [`Device::found`] the ones it has.
    /// **For no longer than `within_ms`** (`super::find::find` gives each request what is
    /// left of it): that is when the command is given up, and whatever is found after it
    /// is nobody's.
    Find { ticket: u64, ids: Vec<String>, within_ms: u64 },
    /// For a test only: carry on with what this ticket holds after this long
    /// ([`Device::resume`]).
    Later { ticket: u64, after_ms: u64 },
    /// The devices seen have changed: keep this list.
    Remember(Vec<Remembered>),
    /// A page of this playlist that was written to disk before now holds the secret: it goes.
    Forget { playlist: String },
    /// The player in the window is now another device's (its name), or this one's again.
    Steering(Option<String>),
    /// What the device being steered is doing has changed: draw it again.
    Draw,
}

/// Whether something queued to be sent under a session may go when its turn comes: only if
/// the session has not ended and its secret is still inside its time. Both, because a
/// session that has run out is no session, whether or not anything has ended it yet.
pub fn may_go(session_ended: bool, since_ms: u64, now_ms: u64) -> bool {
    !session_ended && secret_holds(since_ms, now_ms)
}

/// Whether this device should be linked with the person's others at all: signed in, a
/// relay set up, random numbers to be had, and "Link my devices" on.
pub fn wanted(signed_in: bool, relay: bool, link_my_devices: bool) -> bool {
    signed_in && relay && link_my_devices
}

/// One line of the list.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Row {
    /// Its place at the relay, as text: what picking it names. Empty for one that is not here.
    pub place: String,
    pub name: String,
    /// What it's doing, in a few words.
    pub about: String,
    pub playing: bool,
    /// It's the device the player here is steering.
    pub active: bool,
    /// It's here to be picked. Otherwise it is one seen before that isn't here just now.
    pub ready: bool,
}

/// What the panel shows.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Shown {
    /// This device has its place in the room.
    pub entered: bool,
    pub rows: Vec<Row>,
    /// What there is to say: what became of the last thing asked, or why nothing is linked.
    pub note: String,
    /// The device being steered, by name; empty for none.
    pub steering: String,
}

/// What was last asked of a device, so that what became of it can be said.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Out {
    /// Pause, from the list of devices.
    Pause,
    /// Something pressed while steering it: the screen shows what became of it.
    Steer,
    /// Songs to queue there, and how many.
    Add(usize),
    /// A queue to play there now, while steering it; and how many of its songs were left behind.
    PlayThere(usize),
    /// This device's music, to carry on there; how many songs were sent, and how many left behind.
    Give(Giving, usize, usize),
    /// The pause that completes bringing its music here.
    TakePause,
}

#[derive(Default)]
struct Seen {
    peer: Peer,
    /// When it was last looked at.
    looked_ms: Option<u64>,
    /// What the command that is out was for. Kept until it is known to have been carried
    /// out: a refusal, or "not known", can still be put right by a success that arrives later.
    out: Option<Out>,
}

/// One sign-in with one secret. When either changes the session ends, and everything in
/// flight under it is dropped: its number no longer matches.
struct Session {
    number: u64,
    secret: Secret,
    keys: Keys,
    /// When the look that read the secret began. Ten minutes on, the secret is not used.
    since_ms: u64,
    /// This device's place in the room and the token for it, once it has entered.
    place: Option<(Place, Token)>,
    /// The other places in the room, in the relay's order, and what is known of each.
    others: Vec<Place>,
    peers: HashMap<Place, Seen>,
}

impl Session {
    /// Where a device is in the room now: the place that last answered as it. A copy that
    /// was started again is at two places for a while, and its latest answer is the one
    /// that counts.
    fn place_of(&self, device: &DeviceId) -> Option<Place> {
        let answered = |place: &Place| self.peers.get(place)?.peer.shown().filter(|shown| shown.tell.device == *device).map(|shown| shown.sent_ms);
        self.others.iter().filter_map(|place| answered(place).map(|at| (at, *place))).max_by_key(|(at, _)| *at).map(|(_, place)| place)
    }
}

/// One of the person's other devices is being steered from here.
struct Steering {
    device: DeviceId,
    name: String,
    view: Option<View>,
    /// Since when it has had no place in the room.
    lost_ms: Option<u64>,
}

/// Music is being brought here from another device.
enum Take {
    /// Its queue has been asked for.
    Asked { place: Place, nonce: Nonce, name: String, pressed_ms: u64 },
    /// Its queue is loaded here, and this device's sound is awaited: `heard_from` is where
    /// in the song the sound was when it was first heard on it. This device is busy to the
    /// others until then (`waiting`): what it holds is about to become what is sounding.
    /// (`queue`: the number the player gave the queue as it went in. Only that queue's
    /// sound counts.)
    Loaded { place: Place, name: String, taking: Taking, queue: u64, heard_from: Option<f64>, waiting: Waiting },
}

/// A queue another device sent has been loaded here. It is answered when it sounds.
struct Adoption {
    session: u64,
    from: Place,
    nonce: Nonce,
    adopting: Adopting,
    /// This device is busy to the others until then.
    waiting: Waiting,
    /// The number the player gave the queue as it went in: only that queue's sound counts.
    queue: u64,
    heard_from: Option<f64>,
}

/// A command that has been accepted and whose work is still under way away from here.
struct Pending {
    session: u64,
    from: Place,
    nonce: Nonce,
    command: Command,
    at: At,
    reservation: Reservation,
    /// What it needs, if that is already found (a command held for a test).
    ready: Option<Ready>,
    /// When it is given up, if its work has not come back: as long as the rule keeps the
    /// device busy with it, and no longer.
    until_ms: u64,
}

#[derive(Default)]
pub struct Device {
    /// The sign-in all of this belongs to.
    account: String,
    /// The playlist the secret was last read from under this sign-in.
    used: Option<String>,
    /// A look is on its way: there is never more than one for a sign-in.
    looking: bool,
    /// Goes up when the sign-in changes, so the answer to an earlier look is dropped.
    looks: u64,
    /// When the last look finished.
    looked_ms: Option<u64>,
    /// "Reset linking" was pressed: the next look writes a new secret.
    reset: bool,
    session: Option<Session>,
    sessions: u64,
    asks: Asks,
    note: String,
    /// The system could not give random numbers: linking is off for this run.
    off: bool,
    steering: Option<Steering>,
    take: Option<Take>,
    adoption: Option<Adoption>,
    pending: HashMap<u64, Pending>,
    tickets: u64,
    news: News,
    remembered: Vec<Remembered>,
    /// The day the remembered devices were last looked through for ones to forget.
    forgot_on: u64,
    /// What there is to tell the person that could not be said where it came about: said
    /// at the next tick.
    unsaid: Vec<String>,
    /// The queue another device sent that this one took and that has not sounded: the
    /// command that brought it made the state, and has not been carried out to its end.
    /// It is cleared when the queue sounds, and stays if it never does.
    unfinished: Option<Nonce>,
    /// What the list last showed, to say when it changes.
    listed: Vec<Row>,
    /// For a test only: how long a Pause from another device is held between being
    /// accepted and being carried out.
    pub slow_pause_ms: Option<u64>,
}

/// What is owed to the person when a device is let go of (it has left the room, this
/// device has a new place in it, the session is over) with something asked of it still
/// unanswered that they are waiting to hear of: the stop that ends "Play here". The music
/// is playing here; whether the other stopped will not now be heard.
fn unheard(seen: &Seen) -> Option<String> {
    (seen.out == Some(Out::TakePause) && seen.peer.waiting()).then(|| words::took(Outcome::Unknown, &name_of(seen)))
}

fn name_of(seen: &Seen) -> String {
    seen.peer.shown().map_or_else(|| "A device".to_string(), |shown| shown.tell.name.clone())
}

/// One row for each device. A copy that has been started again is in the room twice for
/// a while: at its new place, and at its old one until the relay gives that up. It is
/// shown once, as its latest answer has it, where it was first in the list.
fn latest_of_each<T>(rows: Vec<(DeviceId, u64, T)>) -> Vec<T> {
    let mut kept: Vec<(DeviceId, u64, T)> = Vec::new();
    for (device, heard_ms, row) in rows {
        match kept.iter_mut().find(|(other, _, _)| *other == device) {
            Some(earlier) if heard_ms >= earlier.1 => *earlier = (device, heard_ms, row),
            Some(_) => {}
            None => kept.push((device, heard_ms, row)),
        }
    }
    kept.into_iter().map(|(_, _, row)| row).collect()
}

/// Which entry after the current song a command means: the one at `at`, if it is still
/// the song its sender saw there, or else the nearest copy of that song.
pub fn entry(ahead: &[String], at: u64, id: &str) -> Option<usize> {
    let at = at as usize;
    if ahead.get(at).is_some_and(|there| there == id) {
        return Some(at);
    }
    ahead.iter().enumerate().filter(|(_, there)| there.as_str() == id).map(|(place, _)| place).min_by_key(|place| place.abs_diff(at))
}

/// The songs of a queue sent by another device that this device has, each with whether it
/// was queued by hand (the `queued` straight after the one to start with), and where in
/// them to start. Songs this library doesn't have are left out.
pub fn known(library: &dyn Library, ids: &[String], start: usize, queued: usize) -> (Vec<(String, bool)>, usize) {
    let has = library.has_songs(ids);
    let (mut songs, mut begin) = (Vec::new(), 0);
    for (place, id) in ids.iter().enumerate() {
        if place == start {
            begin = songs.len();
        }
        if has.get(place) == Some(&true) {
            songs.push((id.clone(), place > start && place <= start + queued));
        }
    }
    let begin = begin.min(songs.len().saturating_sub(1));
    (songs, begin)
}

impl Device {
    /// A device that remembers these others from before.
    pub fn new(remembered: Vec<Remembered>) -> Device {
        Device { remembered: seen::loaded(remembered), ..Device::default() }
    }

    /// This device as it is for another sign-in: **nothing of the last sign-in's is in
    /// it.** Carried over are the numbers its looks, sessions and tickets had reached, so
    /// that an answer still on its way to the last sign-in matches nothing in the next;
    /// what is the installation's and not a sign-in's (the devices seen, that the system
    /// gave no random numbers in this run); and what is for a test.
    pub fn succeed(&self) -> Device {
        Device { looks: self.looks + 1, sessions: self.sessions, tickets: self.tickets, remembered: self.remembered.clone(), forgot_on: self.forgot_on, off: self.off, slow_pause_ms: self.slow_pause_ms, ..Device::default() }
    }

    /// The session, if `number` is the one that is live **and its secret is inside its
    /// time**. Asked at every use of a session: when a look's answer would renew it, when
    /// something is sent, when a frame is opened, and at the moment the player is changed.
    fn live(&self, number: u64, now_ms: u64) -> Option<&Session> {
        self.session.as_ref().filter(|session| session.number == number && secret_holds(session.since_ms, now_ms))
    }

    fn live_mut(&mut self, number: u64, now_ms: u64) -> Option<&mut Session> {
        self.session.as_mut().filter(|session| session.number == number && secret_holds(session.since_ms, now_ms))
    }

    /// Whether a session with this number is live now.
    pub fn is_live(&self, session: u64, now_ms: u64) -> bool {
        self.live(session, now_ms).is_some()
    }

    fn peer_mut(&mut self, place: &Place) -> Option<&mut Seen> {
        self.session.as_mut()?.peers.get_mut(place)
    }

    fn name(&self, place: &Place) -> Option<String> {
        self.session.as_ref()?.peers.get(place).map(name_of)
    }

    /// The place of the device being steered, if it has one in the room now.
    fn steered_place(&self) -> Option<Place> {
        self.session.as_ref()?.place_of(&self.steering.as_ref()?.device)
    }

    /// The name of the device the player here is steering, if it is steering one.
    pub fn steered(&self) -> Option<&str> {
        self.steering.as_ref().map(|steering| steering.name.as_str())
    }

    /// What the device being steered is doing, as the player shows it.
    pub fn view(&self) -> Option<&View> {
        self.steering.as_ref()?.view.as_ref()
    }

    /// Ends the session, if there is one. Says whether there was one.
    ///
    /// **The end of a session ends everything that was waiting under it**, here and now:
    /// a command that was accepted and whose songs are still being looked up, a queue that
    /// was taken and has not sounded yet. None of them can be answered under another
    /// session, and none is left keeping this device busy to the others.
    fn end_session(&mut self, env: &mut Env, out: &mut Vec<Do>) -> bool {
        let Some(session) = self.session.take() else { return false };
        self.sessions += 1;
        // (What was asked of the others under it will not now be answered.)
        for note in session.peers.values().filter_map(unheard) {
            self.say(note, out);
        }
        match self.take.take() {
            // A queue that was asked for under it will not be answered under another.
            Some(Take::Asked { .. }) | None => {}
            // One that was taken and is starting here stays: it is the person's choice,
            // and in the player. But the other device cannot now be told to stop, and the
            // person is told that.
            Some(Take::Loaded { name, waiting, .. }) => {
                env.player.done(waiting);
                out.push(Do::Log(format!("take: the session ended before {name} could be asked to stop")));
                self.say(words::took_and_linking_ended(&name), out);
            }
        }
        let dropped = self.pending.len();
        for (_, pending) in self.pending.drain() {
            env.player.refuse(pending.reservation);
        }
        if dropped > 0 {
            out.push(Do::Log(format!("session: {dropped} command{} accepted under it and not yet carried out: let go", if dropped == 1 { "" } else { "s" })));
        }
        if let Some(adoption) = self.adoption.take() {
            env.player.done(adoption.waiting);
            out.push(Do::Log(format!("adopt from {}: the session ended before it sounded, nobody is answered", text::place(&adoption.from))));
        }
        self.news.forget();
        out.push(Do::End { session: session.number });
        true
    }

    /// Gives up each command that was accepted and has outlived its time ([`rule::OPEN_MS`]):
    /// its sender is told, and whatever its work then brings back is nobody's.
    fn give_up_the_overdue(&mut self, env: &mut Env, out: &mut Vec<Do>) {
        let overdue: Vec<u64> = self.pending.iter().filter(|(_, pending)| pending.until_ms <= env.now_ms).map(|(ticket, _)| *ticket).collect();
        for ticket in overdue {
            let Some(Pending { session, from, nonce, command, at, reservation, .. }) = self.pending.remove(&ticket) else { continue };
            env.player.refuse(reservation);
            self.refused(session, from, nonce, &command, at, Why::Late, env, out);
        }
    }

    /// Everything of this sign-in's is over: the session, and any look at the secret that
    /// is still on its way, whose answer must start nothing.
    fn stop_all(&mut self, env: &mut Env, out: &mut Vec<Do>) {
        self.end_session(env, out);
        if self.looking {
            out.push(Do::StopLook);
        }
        self.looks += 1;
        self.looking = false;
        self.looked_ms = None;
        self.reset = false;
        if self.steering.take().is_some() {
            out.push(Do::Steering(None));
        }
    }

    /// Ends everything of this sign-in's: at sign-out, when linking is no longer wanted,
    /// and when the account changes.
    pub fn stop(&mut self, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        if self.session.is_some() {
            out.push(Do::Log("session: ended, with the sign-in it belonged to".into()));
        }
        self.stop_all(env, &mut out);
        out
    }

    /// "Link my devices" was set. Off is off at once: everything of this sign-in's ends
    /// here, in the same step as the setting is saved, so that a frame already on its way
    /// to this thread finds no session to act under.
    pub fn setting(&mut self, link_my_devices: bool, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        if !link_my_devices {
            if self.session.is_some() {
                out.push(Do::Log("session: ended, \"Link my devices\" was switched off".into()));
            }
            self.stop_all(env, &mut out);
        }
        out
    }

    /// Called a few times a second. `account` names the sign-in (it changes when the
    /// person signs in as someone else); `wanted` is [`wanted`].
    pub fn tick(&mut self, account: &str, wanted: bool, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        let now = env.now_ms;
        for note in std::mem::take(&mut self.unsaid) {
            self.say(note, &mut out);
        }
        if self.account != account {
            // Another sign-in: nothing of the last one's carries over.
            out.extend(self.stop(env));
            self.account = account.to_string();
            self.used = None;
        }
        // A device unseen for thirty days is forgotten by the calendar, whether or not
        // another is ever seen (looked at once a day).
        if std::mem::replace(&mut self.forgot_on, env.today) != env.today
            && let Some(kept) = seen::without_the_forgotten(&self.remembered, env.today)
        {
            self.remembered = kept;
            out.push(Do::Remember(self.remembered.clone()));
        }
        if !wanted || self.off {
            out.extend(self.stop(env));
            return out;
        }
        if self.session.as_ref().is_some_and(|session| !secret_holds(session.since_ms, now)) {
            out.push(Do::Log("session: the secret has not been read for ten minutes".into()));
            if self.end_session(env, &mut out) {
                out.push(Do::Log("session: ended".into()));
            }
            self.note = words::SERVER_SILENT.into();
        }
        if !self.looking && (self.reset || self.looked_ms.is_none_or(|at| now.saturating_sub(at) >= LOOK_AGAIN_MS)) {
            self.looking = true;
            out.push(Do::Look { number: self.looks, used: self.used.clone(), reset: std::mem::take(&mut self.reset) });
        }
        self.give_up_the_overdue(env, &mut out);
        self.peers(env, &mut out);
        self.moves(env, &mut out);
        self.tell_news(env, &mut out);
        out
    }

    /// "Reset linking" was pressed. **The session ends at the press**, not when the look
    /// that writes the new secret comes back: from this moment nothing is sent, opened or
    /// carried out under the old secret on this device. A look that was on its way is
    /// dropped (what it reads is the old secret, and must begin nothing); the one that
    /// writes the new secret starts at once.
    pub fn reset(&mut self, env: &mut Env) -> Vec<Do> {
        let mut out = vec![Do::Log("reset: asked for".into())];
        if self.end_session(env, &mut out) {
            out.push(Do::Log("session: ended, linking is being reset".into()));
        }
        if std::mem::take(&mut self.looking) {
            out.push(Do::StopLook);
            self.looks += 1;
        }
        self.reset = true;
        self.note = words::RESETTING.into();
        out
    }

    /// A look has finished. `since_ms` is when it began; `reset` is whether it was one
    /// that writes a new secret.
    pub fn looked(&mut self, number: u64, since_ms: u64, looked: Looked, reset: bool, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        // (Signed out since, or no longer wanted: the answer is nobody's.)
        if self.looks != number {
            return out;
        }
        self.looking = false;
        self.looked_ms = Some(env.now_ms);
        // A look that found the record is not one to use ends the session that used it
        // now, not when its ten minutes are up.
        if ends_session(&looked) && self.end_session(env, &mut out) {
            out.push(Do::Log("session: ended, the record is no longer one to use".into()));
        }
        let reset_done = reset && matches!(&looked, Looked::Secret { wrote: Some(secret::Why::Reset), .. });
        // A reset that got as far as writing and no further has still ended the old secret.
        let reset_half_done = reset && matches!(&looked, Looked::Invalid);
        let begin = match looked {
            Looked::Secret { secret, id, wrote } => {
                out.push(Do::Forget { playlist: id.clone() });
                self.used = Some(id);
                if wrote == Some(secret::Why::Public) {
                    self.note = words::WAS_PUBLIC.into();
                }
                match self.session.as_mut().filter(|session| session.secret == secret && secret_holds(session.since_ms, env.now_ms)) {
                    Some(session) => {
                        session.since_ms = since_ms;
                        out.push(Do::Renew { session: session.number, since_ms });
                        out.push(Do::Log("look: the secret is as it was".into()));
                        None
                    }
                    None => Some(secret),
                }
            }
            Looked::Stopped(stop) => {
                self.note = words::stopped(stop, env.me.here);
                out.push(Do::Log(format!("look: stopped ({stop:?})")));
                None
            }
            Looked::Failed => {
                out.push(Do::Log("look: the music server could not be asked".into()));
                None
            }
            Looked::Invalid => {
                self.note = words::NEEDS_RESETTING.into();
                None
            }
            Looked::NoRandom => {
                self.no_random(env, &mut out);
                None
            }
        };
        if reset_half_done {
            self.note = words::RESET_HALF_DONE.into();
        } else if reset && !reset_done {
            self.note = words::RESET_NOT_DONE.into();
        }
        if let Some(secret) = begin {
            // A secret that is not the one in use: a new session, and the end of the old one.
            if self.end_session(env, &mut out) {
                out.push(Do::Log("session: ended".into()));
            }
            let keys = Keys::of(&secret);
            self.sessions += 1;
            self.note.clear();
            out.push(Do::Begin { session: self.sessions, room: keys.room().to_string(), since_ms });
            self.session = Some(Session { number: self.sessions, secret, keys, since_ms, place: None, others: Vec::new(), peers: HashMap::new() });
            out.push(Do::Log("session: begun with a secret a look read".into()));
            if reset_done {
                out.push(Do::Log("reset: a new secret was written and read back".into()));
                self.note = words::RESET_DONE.into();
            }
        }
        out
    }

    fn no_random(&mut self, env: &mut Env, out: &mut Vec<Do>) {
        if self.end_session(env, out) {
            out.push(Do::Log("session: ended".into()));
        }
        self.note = words::no_random(env.me.here);
        self.off = true;
    }

    /// The system gave no random bytes where some were needed: linking is off for this run.
    pub fn out_of_random(&mut self, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        self.no_random(env, &mut out);
        out
    }

    /// The relay had something for the person (why no place can be had).
    pub fn said(&mut self, session: u64, note: &str) {
        if self.session.as_ref().is_some_and(|live| live.number == session) {
            self.note = note.to_string();
        }
    }

    /// This device has a place in the room.
    pub fn entered(&mut self, session: u64, place: Place, token: Token) {
        self.news.forget();
        if let Some(live) = self.session.as_mut().filter(|live| live.number == session) {
            // A new place: whatever was asked from the old one cannot be answered to it.
            live.place = Some((place, token));
            live.others.clear();
            self.unsaid.extend(live.peers.drain().filter_map(|(_, seen)| unheard(&seen)));
        }
    }

    /// The relay has forgotten this device's place.
    pub fn left(&mut self, session: u64) {
        if let Some(live) = self.session.as_mut().filter(|live| live.number == session) {
            live.place = None;
            live.others.clear();
            self.unsaid.extend(live.peers.drain().filter_map(|(_, seen)| unheard(&seen)));
        }
    }

    /// Seals a message for another place, to be sent. Its nonce.
    fn send(&mut self, to: Place, kind: u8, message: String, env: &mut Env, out: &mut Vec<Do>) -> Option<Nonce> {
        let Some(nonce) = env.nonce() else {
            self.no_random(env, out);
            return None;
        };
        // Under the session that is live now, and only inside its time.
        let number = self.session.as_ref()?.number;
        let session = self.live(number, env.now_ms)?;
        let (place, token) = session.place.clone()?;
        let sealed = frame::seal(&session.keys, kind, &nonce, &place, &to, message.as_bytes())?;
        out.push(Do::Send { session: number, token, to, frame: sealed });
        Some(nonce)
    }

    /// What this device says of itself.
    ///
    /// **While a command is accepted and being worked for** (its songs are being looked
    /// up), the state is busy and names that command in `by`, as it does for one that was
    /// carried out and has not sounded yet: its sender, looking because no answer has
    /// come, sees that it arrived and is under way, and waits on. (Only while busy: a
    /// state that named it and was not busy would say it was done.)
    fn state(&self, re: Option<Nonce>, ok: Option<bool>, why: Option<Why>, env: &Env) -> Tell {
        let brief = env.player.brief();
        let busy = env.player.busy(env.now_ms);
        let open = self.pending.values().find(|pending| busy && pending.until_ms > env.now_ms).map(|pending| pending.nonce);
        let by = open.or(env.player.by());
        // The command named was carried out to its end: it is not one still being worked
        // for, and not a queue that was taken and has not sounded (or never did).
        let done = by.is_some() && open.is_none() && by != self.unfinished;
        Tell {
            re,
            ok,
            why,
            device: env.me.device,
            name: env.me.name.clone(),
            at: At { generation: env.player.generation(), minute: minute(env.now_ms) },
            busy,
            by,
            done,
            playing: brief.playing,
            title: brief.title,
            artist: brief.artist,
            steering: self.steering.as_ref().map(|steering| steering.name.clone()).unwrap_or_default(),
            full: None,
            queue: None,
        }
    }

    /// What the relay handed over: who is in the room, and what was in this device's mailbox.
    pub fn heard(&mut self, session: u64, places: &[String], frames: &[(String, String)], env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        let mine = {
            let Some(live) = self.live_mut(session, env.now_ms) else { return out };
            let Some((mine, _)) = live.place.clone() else { return out };
            live.others = places.iter().filter_map(|place| text::read_place(place)).filter(|place| *place != mine).take(15).collect();
            let others = live.others.clone();
            // (A device that has left the room will not answer what was asked of it.)
            let gone: Vec<String> = live.peers.iter().filter(|(place, _)| !others.contains(place)).filter_map(|(_, seen)| unheard(seen)).collect();
            live.peers.retain(|place, _| others.contains(place));
            self.unsaid.extend(gone);
            mine
        };
        for note in std::mem::take(&mut self.unsaid) {
            self.say(note, &mut out);
        }
        for (from, sealed) in frames {
            let Some(from) = text::read_place(from) else { continue };
            // (Checked for each frame: the one before may have ended the session.)
            let Some(live) = self.live(session, env.now_ms) else { return out };
            let Some(opened) = frame::open(&live.keys, sealed, &from, &mine) else {
                out.push(Do::Log(format!("frame from {}: does not open, dropped", text::place(&from))));
                continue;
            };
            match opened.kind {
                ASK => self.asked(session, from, opened.nonce, &opened.message, env, &mut out),
                TELL => self.told(session, from, &opened.message, env, &mut out),
                _ => {}
            }
        }
        out
    }

    // ---- what another device asks of this one ----------------------------------------------

    /// What is sounding is what a command would act on: before anything from another
    /// device is judged, answered or carried out, the window is brought up to the sound.
    fn catch_up(env: &mut Env, out: &mut Vec<Do>) {
        if env.player.catch_up() {
            out.push(Do::Log("the sound has rolled into the next song: the window is brought up to it".into()));
        }
    }

    fn asked(&mut self, session: u64, from: Place, nonce: Nonce, message: &[u8], env: &mut Env, out: &mut Vec<Do>) {
        if !self.asks.allow(env.now_ms) {
            return out.push(Do::Log("ask: more than this device answers in a second, dropped".into()));
        }
        Device::catch_up(env, out);
        match read_ask(message) {
            None => {
                out.push(Do::Log(format!("ask from {}: not one this version carries out", text::place(&from))));
                let answer = self.state(Some(nonce), Some(false), Some(Why::Invalid), env);
                self.answer(session, from, answer, env, out);
            }
            Some(Ask::Look(want)) => {
                let mut state = self.state(Some(nonce), None, None, env);
                match want {
                    Want::Brief => {}
                    Want::Full => state.full = Some(env.player.full()),
                    Want::Queue => state.queue = Some(env.player.handed()),
                }
                self.answer(session, from, state, env, out);
            }
            Some(Ask::Do { command, at }) => self.commanded(session, from, nonce, command, at, env, out),
        }
    }

    /// A command from another device, as it arrives: checked against the rule before any
    /// work is done for it, then whatever it needs is found, and then it is carried out.
    fn commanded(&mut self, session: u64, from: Place, nonce: Nonce, command: Command, at: At, env: &mut Env, out: &mut Vec<Do>) {
        let now = env.now_ms;
        let reservation = match env.player.admit(at, minute(now), now) {
            Ok(reservation) => reservation,
            Err(why) => return self.refused(session, from, nonce, &command, at, why, env, out),
        };
        let cannot = |device: &mut Device, env: &mut Env, out: &mut Vec<Do>| {
            env.player.refuse(reservation);
            device.refused(session, from, nonce, &command, at, Why::Cannot, env, out);
        };
        let entry_of = |env: &Env, at: u64, id: &str| entry(&env.player.ahead(), at, id).map(Ready::Entry);
        // What it needs found, or that it cannot be done. Nothing is changed here.
        let ready = match &command {
            Command::Play | Command::Pause | Command::Moved | Command::Next | Command::Previous | Command::Seek { .. } | Command::Clear { .. } if !env.player.has_song() => None,
            Command::Jump { at, id } | Command::Remove { at, id } => entry_of(env, *at, id),
            Command::Move { from, id, .. } => entry_of(env, *from, id),
            Command::Add { ids, .. } => {
                let has = env.library.has_songs(ids);
                if has.iter().all(|has| *has) && has.len() == ids.len() {
                    Some(Ready::Songs(ids.clone()))
                } else {
                    // The songs are looked up away from this thread; the command stays the
                    // one that is open, and this device is busy to the others, until they
                    // are back.
                    self.tickets += 1;
                    self.pending.insert(self.tickets, Pending { session, from, nonce, command: command.clone(), at, reservation, ready: None, until_ms: now.saturating_add(rule::OPEN_MS) });
                    return out.push(Do::Find { ticket: self.tickets, ids: ids.clone(), within_ms: rule::OPEN_MS });
                }
            }
            // It checks it can play before anything is changed: a queue none of whose songs
            // this library has is not taken.
            Command::Adopt(adopt) => match known(env.library, &adopt.songs, adopt.index, adopt.queued) {
                (songs, _) if songs.is_empty() => None,
                (songs, begin) => Some(Ready::Queue(songs, begin)),
            },
            _ => Some(Ready::Nothing),
        };
        let Some(ready) = ready else { return cannot(self, env, out) };
        // (For a test only: a Pause is carried out a while after it is accepted, so that
        // something can be made to happen in between.)
        if let (Command::Pause, Some(wait)) = (&command, self.slow_pause_ms) {
            out.push(Do::Log(format!("pause from {}: accepted, and held {wait} ms before it is carried out (a test)", text::place(&from))));
            self.tickets += 1;
            self.pending.insert(self.tickets, Pending { session, from, nonce, command, at, reservation, ready: Some(ready), until_ms: now.saturating_add(rule::OPEN_MS) });
            return out.push(Do::Later { ticket: self.tickets, after_ms: wait });
        }
        self.finish(session, from, nonce, command, at, reservation, ready, env, out);
    }

    /// Whether a command is still waiting for what this ticket's look-up brings. An app
    /// asks before it keeps anything the look-up found: one that was given up, or that
    /// belonged to a sign-in that has ended, waits for nothing.
    pub fn awaits(&self, ticket: u64) -> bool {
        self.pending.contains_key(&ticket)
    }

    /// The songs a command brought that the music server has, of those this device's copy
    /// of the library lacked ([`Do::Find`]): every song the command names that can be
    /// played here, by id, the ones the library had and the ones that were found.
    pub fn found(&mut self, ticket: u64, songs: Vec<String>, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        let Some(pending) = self.pending.remove(&ticket) else { return out };
        let Pending { session, from, nonce, command, at, reservation, .. } = pending;
        if songs.is_empty() {
            env.player.refuse(reservation);
            self.refused(session, from, nonce, &command, at, Why::Cannot, env, &mut out);
        } else {
            self.finish(session, from, nonce, command, at, reservation, Ready::Songs(songs), env, &mut out);
        }
        out
    }

    /// Carries on with what a ticket holds ([`Do::Later`]).
    pub fn resume(&mut self, ticket: u64, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        if let Some(Pending { session, from, nonce, command, at, reservation, ready: Some(ready), .. }) = self.pending.remove(&ticket) {
            self.finish(session, from, nonce, command, at, reservation, ready, env, &mut out);
        }
        out
    }

    /// The moment the player is changed for a command from another device: everything is
    /// checked again, under the session the command arrived in, and then it is carried out
    /// the way the same press on this device would be.
    #[allow(clippy::too_many_arguments)]
    fn finish(&mut self, session: u64, from: Place, nonce: Nonce, command: Command, at: At, reservation: Reservation, ready: Ready, env: &mut Env, out: &mut Vec<Do>) {
        // (The sound may have rolled on while the work was done.)
        Device::catch_up(env, out);
        let now = env.now_ms;
        let same_session = self.is_live(session, now);
        let carrying = match env.player.finish(reservation, at, minute(now), now, same_session) {
            Ok(carrying) => carrying,
            Err(why) => return self.refused(session, from, nonce, &command, at, why, env, out),
        };
        // The music has moved to the device that asks. Its queue is marked as that device's
        // before the sound is stopped, so that the stop is not saved as a change of this
        // device's own; and the mark stands only if the sound did stop.
        let handing = command == Command::Moved;
        let marked = handing && env.player.handed_on();
        if handing && !marked {
            out.push(Do::Log("queue: it could not be marked as handed on".into()));
        }
        // What came of it is known as the player returns: the rule is told, and the
        // command's sender answered, from the player as it now is.
        let before = env.player.queue();
        let carried = env.player.carry_out(&command, &ready);
        let changed = match (&command, carried) {
            // A queue to take over is in the player if the player's queue has a new
            // number, whatever was said: that is asked afresh, and gone by.
            (Command::Adopt(_), _) => env.player.queue() != before,
            (_, carried) => carried != Carried::Unchanged,
        };
        // (What it did is not known: a change, and no command is named for it.)
        let unknown = carried == Carried::Unknown && !matches!(command, Command::Adopt(_));
        env.player.carried(carrying, changed, (!unknown).then_some(nonce));
        if handing && env.player.brief().playing {
            // It was told to stop and is still sounding: it is not acknowledged as
            // stopped, and its queue is its own still.
            if marked {
                env.player.handed_back();
            }
            out.push(Do::Log(format!("moved from {}: this device could not be stopped, and is still playing", text::place(&from))));
            return self.refused(session, from, nonce, &command, at, Why::Cannot, env, out);
        }
        if unknown && !handing {
            out.push(Do::Log(format!("{} from {} against generation {}: the player could not say what came of it; answered as not known", command.name(), text::place(&from), at.generation)));
            let answer = self.state(Some(nonce), Some(false), Some(Why::Unknown), env);
            return self.answer(session, from, answer, env, out);
        }
        if !changed {
            // The player did nothing. Either it was already as the command asked, which is
            // no fault and is said as such; or it could not do it. Nothing has moved.
            return match command {
                Command::Play | Command::Pause | Command::Moved | Command::Volume { .. } | Command::Shuffle { .. } | Command::Repeat { .. } | Command::Clear { .. } => {
                    out.push(Do::Log(format!("{} from {} against generation {}: nothing to do, it was so already", command.name(), text::place(&from), at.generation)));
                    let answer = self.state(Some(nonce), Some(true), Some(Why::Nothing), env);
                    self.answer(session, from, answer, env, out);
                }
                _ => self.refused(session, from, nonce, &command, at, Why::Cannot, env, out),
            };
        }
        out.push(Do::Log(format!("{} from {} against generation {}: carried out", command.name(), text::place(&from), at.generation)));
        if matches!(command, Command::Adopt(_)) {
            // It answers "carried out" when its sound has started, and is busy until then.
            let waiting = env.player.wait(now, START_MS + 5_000);
            let adoption = Adoption { session, from, nonce, adopting: Adopting::begin(env.player.generation(), now), waiting, queue: env.player.queue(), heard_from: None };
            self.unfinished = Some(nonce);
            if let Some(earlier) = self.adoption.replace(adoption) {
                env.player.done(earlier.waiting);
            }
            return;
        }
        let answer = self.state(Some(nonce), Some(true), None, env);
        self.answer(session, from, answer, env, out);
    }

    #[allow(clippy::too_many_arguments)]
    fn refused(&mut self, session: u64, from: Place, nonce: Nonce, command: &Command, at: At, why: Why, env: &mut Env, out: &mut Vec<Do>) {
        out.push(Do::Log(format!(
            "{} from {} against generation {}: refused, {why:?} (this device is at {}{})",
            command.name(),
            text::place(&from),
            at.generation,
            env.player.generation(),
            if env.player.busy(env.now_ms) { ", busy" } else { "" }
        )));
        let answer = self.state(Some(nonce), Some(false), Some(why), env);
        self.answer(session, from, answer, env, out);
    }

    /// Answers an ask, if the session it arrived under is still the live one: an answer is
    /// not sent under another session, to a place that session has never heard of.
    fn answer(&mut self, session: u64, to: Place, answer: Tell, env: &mut Env, out: &mut Vec<Do>) {
        if !self.is_live(session, env.now_ms) {
            return out.push(Do::Log("answer: the session it belonged to has ended, not sent".into()));
        }
        self.send(to, TELL, answer.text(), env, out);
    }

    /// The moves that wait on this device's own sound: a queue another device sent, which
    /// is answered when it sounds; and music being brought here, whose other end is paused
    /// when this one sounds. And the device being steered, if it has gone.
    fn moves(&mut self, env: &mut Env, out: &mut Vec<Do>) {
        let now = env.now_ms;
        // This device's own sound has started on the queue a move is waiting on once it
        // has moved on from where it was first heard: the song being open is not yet sound.
        let sounding = {
            let sound = env.player.sounding();
            let heard = match (&mut self.adoption, &mut self.take) {
                (Some(adoption), _) => Some((&mut adoption.heard_from, adoption.queue)),
                (None, Some(Take::Loaded { heard_from, queue, .. })) => Some((heard_from, *queue)),
                _ => None,
            };
            // (Only the sound of the queue that was installed for the move: the queue
            // that was there before, still playing, is not it.)
            let at = heard.as_ref().and_then(|(_, queue)| sound.filter(|sound| sound.queue == *queue)).map(|sound| sound.at);
            match (heard.map(|(heard, _)| heard), at) {
                (Some(heard), Some(at)) => at - *heard.get_or_insert(at) >= SOUNDS_AFTER,
                (Some(heard), None) => {
                    *heard = None;
                    false
                }
                (None, _) => false,
            }
        };
        let generation = env.player.generation();
        // A queue that was sent here.
        let live = self.session.as_ref().map(|session| session.number);
        let step = match &self.adoption {
            // (The session it came under has ended: there is nobody to answer.)
            Some(adoption) if live != Some(adoption.session) => Some(None),
            Some(adoption) => adoption.adopting.poll(sounding, generation, now).map(Some),
            None => None,
        };
        if let Some((adoption, adopted)) = step.and_then(|adopted| self.adoption.take().map(|adoption| (adoption, adopted))) {
            let (ok, why) = match adopted {
                Some(Adopted::Started) => {
                    self.unfinished = None;
                    (true, None)
                }
                Some(Adopted::Changed) => (false, Some(Why::Changed)),
                Some(Adopted::Failed) => {
                    // No sound came: it stops trying, and says so.
                    env.player.pause();
                    (false, Some(Why::Cannot))
                }
                None => (false, None),
            };
            env.player.done(adoption.waiting);
            if adopted.is_some() {
                out.push(Do::Log(format!("adopt from {}: {}", text::place(&adoption.from), if ok { "its sound has started" } else { "it did not start" })));
                let answer = self.state(Some(adoption.nonce), Some(ok), why, env);
                self.answer(adoption.session, adoption.from, answer, env, out);
            }
        }
        // Music being brought here.
        let step = match &self.take {
            Some(Take::Loaded { taking, .. }) => taking.poll(sounding, generation, now).map(Some),
            // Its queue never came.
            Some(Take::Asked { pressed_ms, .. }) if now.saturating_sub(*pressed_ms) >= WAIT_MS => Some(None),
            _ => None,
        };
        match step.and_then(|took| self.take.take().map(|take| (take, took))) {
            Some((Take::Asked { name, .. }, _)) => {
                self.let_go_into(out);
                self.say(words::queue_never_came(&name), out);
            }
            Some((Take::Loaded { place, name, waiting, .. }, Some(took))) => {
                // However it ends, this device is no longer in the middle of it.
                env.player.done(waiting);
                match took {
                    Took::Pause(at) => {
                        // The other stops now that this one sounds: against the state its
                        // queue was taken from, so that if it has been given anything else
                        // to do since, it is left to it. It is told the music has moved, not
                        // asked to pause as a press would: its going quiet is nobody's choice
                        // there, and claims nothing.
                        match self.send(place, ASK, Ask::Do { command: Command::Moved, at }.text(), env, out) {
                            Some(nonce) => {
                                out.push(Do::Log(format!("take: {} sounds; asked {name} to pause, against generation {}", env.me.here.this_lower(), at.generation)));
                                if let Some(seen) = self.peer_mut(&place) {
                                    seen.peer.sent(nonce, at, now);
                                    seen.out = Some(Out::TakePause);
                                }
                            }
                            // It could not be sent (the session has gone, or this device has
                            // no place in the room): the person is told what is true.
                            None => {
                                out.push(Do::Log(format!("take: {} sounds; {name} could not be asked to stop", env.me.here.this_lower())));
                                self.say(words::took_and_linking_ended(&name), out);
                            }
                        }
                    }
                    Took::ThisChanged => self.say(words::chosen_here_meanwhile(&name), out),
                    Took::NotStarted => {
                        // It is stopped, not left to start sounding later with the other
                        // still playing: the app's own doing.
                        env.player.pause();
                        self.say(words::did_not_start_here(&name, env.me.here), out);
                    }
                }
            }
            _ => {}
        }
        // The device being steered.
        let here = self.steered_place().is_some();
        let lost = match self.steering.as_mut() {
            Some(steering) if here => {
                steering.lost_ms = None;
                None
            }
            Some(steering) => {
                let since = *steering.lost_ms.get_or_insert(now);
                (now.saturating_sub(since) >= LOST_MS).then(|| steering.name.clone())
            }
            None => None,
        };
        if let Some(name) = lost {
            self.let_go_into(out);
            self.say(words::gone(&name, env.me.here), out);
        }
    }

    /// Tells the other devices, unasked, that what this one is doing has changed: so that
    /// they look now, not at their next turn. Bounded by `super::news`.
    fn tell_news(&mut self, env: &mut Env, out: &mut Vec<Do>) {
        use std::hash::{Hash, Hasher};
        let Some(others) = self.session.as_ref().filter(|session| session.place.is_some()).map(|session| session.others.clone()) else { return };
        let state = self.state(None, None, None, env);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (state.at.generation, state.busy, state.playing, &state.title, &state.artist, &state.name, &state.steering).hash(&mut hasher);
        if !self.news.due(hasher.finish(), others.len(), env.now_ms) {
            return;
        }
        let text = state.text();
        for place in others {
            self.send(place, TELL, text.clone(), env, out);
        }
    }

    // ---- what this device asks of another --------------------------------------------------

    /// Says something to the person: where they are looking, and under the list.
    fn say(&mut self, note: String, out: &mut Vec<Do>) {
        out.push(Do::Log(format!("said: {note}")));
        self.note = note.clone();
        out.push(Do::Say(note));
    }

    /// Another device says something of itself.
    fn told(&mut self, session: u64, from: Place, message: &[u8], env: &mut Env, out: &mut Vec<Do>) {
        let Some(mut tell) = read_tell(message) else {
            return out.push(Do::Log(format!("tell from {}: breaks its shape, dropped", text::place(&from))));
        };
        let now = env.now_ms;
        if tell.news() {
            // News is a reason to look, and nothing more.
            let look = self.live_mut(session, now).and_then(|live| live.peers.get_mut(&from)).is_some_and(|seen| seen.peer.news(now));
            if look {
                self.look_at(from, false, env, out);
            }
            return;
        }
        // (The queue, if it carries one, is for the move that asked for it and is not kept.)
        let (handed, re, at, playing, device) = (tell.queue.take(), tell.re, tell.at, tell.playing, tell.device);
        let (told, name, sent_for) = {
            let Some(seen) = self.live_mut(session, now).and_then(|live| live.peers.get_mut(&from)) else { return };
            let told = seen.peer.told(tell);
            let sent_for = seen.out;
            if matches!(told, Told::Settled(Outcome::Done) | Told::ShownAndSettled(Outcome::Done)) {
                seen.out = None;
            }
            (told, name_of(seen), sent_for)
        };
        if matches!(told, Told::Shown | Told::ShownAndSettled(_)) {
            // It answered under this name: it is remembered, so that the list can show it
            // at once another time.
            let id = text::base64(&device);
            if seen::is_news(&self.remembered, &id, &name, env.today) {
                self.remembered = seen::remembered(std::mem::take(&mut self.remembered), Some((&id, &name)), env.today);
                out.push(Do::Remember(self.remembered.clone()));
            }
            self.show_view(from, now, out);
            // The queue that was asked for, to bring its music here: if it has come in
            // time. One that comes after that is not taken, whether or not a tick has
            // yet said so.
            match &self.take {
                Some(Take::Asked { place, nonce, pressed_ms, .. }) if *place == from && Some(*nonce) == re && now.saturating_sub(*pressed_ms) < WAIT_MS => self.took(from, handed, at, playing, env, out),
                Some(Take::Asked { place, nonce, name, .. }) if *place == from && Some(*nonce) == re => {
                    let name = name.clone();
                    self.take = None;
                    self.let_go_into(out);
                    self.say(words::queue_never_came(&name), out);
                }
                _ => {}
            }
        }
        match told {
            Told::Dropped => out.push(Do::Log(format!("tell from {name}: answers nothing waited on, dropped"))),
            Told::Shown => {}
            Told::Settled(outcome) | Told::ShownAndSettled(outcome) => {
                self.outcome(from, &name, sent_for, outcome, env, out);
                // What was pressed while it was unanswered goes next, if it was carried out.
                if outcome == Outcome::Done {
                    self.next(from, env, out);
                }
                // The answer to a command does not change what is shown: look again.
                self.look_at(from, false, env, out);
            }
        }
    }

    /// Says what became of something asked of another device.
    fn outcome(&mut self, place: Place, name: &str, sent_for: Option<Out>, outcome: Outcome, env: &mut Env, out: &mut Vec<Do>) {
        let sent_for = sent_for.unwrap_or(Out::Steer);
        out.push(Do::Log(format!("outcome of {sent_for:?} to {name}: {outcome:?}")));
        let note = match (sent_for, outcome) {
            (Out::Pause, Outcome::Done) => words::paused(name),
            (Out::Steer | Out::PlayThere(0), Outcome::Done) => String::new(),
            (Out::PlayThere(left), Outcome::Done) => words::sent_as_many_as_fit(name, left),
            (Out::Add(count), Outcome::Done) => words::added(count, name),
            (Out::Give(giving, sent, left), outcome) => {
                let gave = giving.ends(outcome, env.player.generation(), env.now_ms);
                if gave == Gave::Moved {
                    // The other has started: the queue is that device's now, this one
                    // stops, and the player is the other's. (Marked before the stop, and
                    // only kept if this device did stop.)
                    let marked = env.player.handed_on();
                    if !marked {
                        out.push(Do::Log("queue: it could not be marked as handed on".into()));
                    }
                    env.player.pause();
                    if env.player.brief().playing {
                        if marked {
                            env.player.handed_back();
                        }
                        out.push(Do::Log(format!("give: {name} has started and this device could not be stopped")));
                        words::gave_and_did_not_stop(name, env.me.here)
                    } else {
                        self.steer(place, env, out);
                        words::gave(gave, name, left, sent, env.me.here)
                    }
                } else {
                    words::gave(gave, name, left, sent, env.me.here)
                }
            }
            (Out::TakePause, outcome) => words::took(outcome, name),
            (_, outcome) => words::outcome(outcome, name),
        };
        if note.is_empty() {
            return;
        }
        out.push(Do::Log(format!("said: {note}")));
        self.note = note.clone();
        // Under the list for a Pause pressed there; anything else is said where the person is looking.
        if sent_for != Out::Pause {
            out.push(Do::Say(note));
        }
    }

    /// Sends a look to a place. `checking` says it is the one look that asks what became of
    /// a command that has gone unanswered.
    fn look_at(&mut self, to: Place, checking: bool, env: &mut Env, out: &mut Vec<Do>) {
        // The device being steered is asked for everything the player shows.
        let want = if self.steered_place() == Some(to) { Want::Full } else { Want::Brief };
        let Some(nonce) = self.send(to, ASK, Ask::Look(want).text(), env, out) else { return };
        let now = env.now_ms;
        if let Some(seen) = self.peer_mut(&to) {
            seen.looked_ms = Some(now);
            if checking { seen.peer.checking(nonce, now) } else { seen.peer.looked(nonce, now) }
        }
    }

    /// Looks at each of the other devices from time to time (the one being steered every
    /// few seconds), and gives up waiting for the answer to a command when it has waited
    /// long enough.
    fn peers(&mut self, env: &mut Env, out: &mut Vec<Do>) {
        let now = env.now_ms;
        let steered = self.steered_place();
        let (mut to_look, mut settled) = (Vec::new(), Vec::new());
        {
            let Some(session) = self.session.as_mut().filter(|session| session.place.is_some()) else { return };
            for place in session.others.clone() {
                let seen = session.peers.entry(place).or_default();
                let every = if steered == Some(place) { LOOK_EVERY_MS } else { LOOK_IDLE_MS };
                match seen.peer.tick(now) {
                    Tick::Check => to_look.push((place, true)),
                    Tick::Settled(outcome) => settled.push((place, name_of(seen), seen.out, outcome)),
                    Tick::Nothing if seen.looked_ms.is_none_or(|at| now.saturating_sub(at) >= every) => to_look.push((place, false)),
                    Tick::Nothing => {}
                }
            }
        }
        for (place, name, sent_for, outcome) in settled {
            self.outcome(place, &name, sent_for, outcome, env, out);
        }
        for (place, checking) in to_look {
            self.look_at(place, checking, env, out);
        }
    }

    /// Asks a device to do something: now, if nothing asked of it is unanswered and there
    /// is a fresh state to make it against; after the command that is out, if one is; or
    /// not at all.
    fn ask(&mut self, to: Place, command: Command, sent_for: Out, env: &mut Env, out: &mut Vec<Do>) -> Pressed {
        let now = env.now_ms;
        let Some(press) = self.peer_mut(&to).map(|seen| seen.peer.press(&command, now)) else { return Pressed::No(Wait::Look) };
        let Pressed::Send(at) = press else { return press };
        let name = command.name();
        let Some(nonce) = self.send(to, ASK, Ask::Do { command, at }.text(), env, out) else { return Pressed::No(Wait::Look) };
        if let Some(seen) = self.peer_mut(&to) {
            out.push(Do::Log(format!("{name}: asked {}, against generation {} in minute {}", name_of(seen), at.generation, at.minute)));
            seen.peer.sent(nonce, at, now);
            seen.out = Some(sent_for);
        }
        press
    }

    /// Sends the next press that was waiting for a device, now that the one before it has
    /// been carried out: against the state in that answer.
    fn next(&mut self, to: Place, env: &mut Env, out: &mut Vec<Do>) {
        let now = env.now_ms;
        let Some((command, at)) = self.peer_mut(&to).and_then(|seen| seen.peer.next(now)) else { return };
        let sent_for = match &command {
            Command::Add { ids, .. } => Out::Add(ids.len()),
            _ => Out::Steer,
        };
        let name = command.name();
        let Some(nonce) = self.send(to, ASK, Ask::Do { command, at }.text(), env, out) else { return };
        if let Some(seen) = self.peer_mut(&to) {
            out.push(Do::Log(format!("{name}: asked {} next, against generation {} in minute {}", name_of(seen), at.generation, at.minute)));
            seen.peer.sent(nonce, at, now);
            seen.out = Some(sent_for);
        }
    }

    /// Says why something could not be asked of a device just now, and looks at it.
    fn not_sent(&mut self, to: Place, name: &str, wait: Wait, aloud: bool, env: &mut Env, out: &mut Vec<Do>) {
        let note = words::not_sent(wait, name);
        if wait != Wait::Unanswered && self.name(&to).is_some() {
            self.look_at(to, false, env, out);
        }
        self.note = note.clone();
        if aloud {
            out.push(Do::Say(note));
        }
    }

    fn unanswered(&self, place: &Place) -> bool {
        self.session.as_ref().and_then(|session| session.peers.get(place)).is_some_and(|seen| seen.peer.waiting())
    }

    /// Pause was pressed for another device, in the list.
    pub fn pause(&mut self, place: &str, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        let Some(to) = text::read_place(place) else { return out };
        let Some(name) = self.name(&to) else { return out };
        if self.unanswered(&to) {
            self.not_sent(to, &name, Wait::Unanswered, false, env, &mut out);
            return out;
        }
        match self.ask(to, Command::Pause, Out::Pause, env, &mut out) {
            Pressed::Send(_) | Pressed::Waits => self.note = words::asked_to_pause(&name),
            Pressed::No(wait) => self.not_sent(to, &name, wait, false, env, &mut out),
        }
        out
    }

    // ---- steering another device from here ---------------------------------------------------

    /// From here the player and the queue panel are that device's.
    fn steer(&mut self, place: Place, env: &mut Env, out: &mut Vec<Do>) {
        let found = self.session.as_ref().and_then(|session| session.peers.get(&place)).and_then(|seen| seen.peer.shown()).map(|shown| (shown.tell.device, shown.tell.name.clone()));
        let Some((device, name)) = found else { return };
        out.push(Do::Log(format!("steering {name}")));
        out.push(Do::Steering(Some(name.clone())));
        self.steering = Some(Steering { device, name, view: None, lost_ms: None });
        self.look_at(place, false, env, out);
    }

    fn let_go_into(&mut self, out: &mut Vec<Do>) -> bool {
        if matches!(self.take, Some(Take::Asked { .. })) {
            self.take = None;
        }
        let was = self.steering.take().is_some();
        if was {
            out.push(Do::Log("steering: stopped".into()));
            out.push(Do::Steering(None));
        }
        was
    }

    /// Stops steering, leaving the other device as it is. The player is this device's own
    /// again. Nothing, if none was being steered.
    pub fn let_go(&mut self) -> Vec<Do> {
        let mut out = Vec::new();
        self.let_go_into(&mut out);
        out
    }

    /// A fresh state arrived from a place: if that is the device being steered, the
    /// player shows it.
    fn show_view(&mut self, from: Place, now: u64, out: &mut Vec<Do>) {
        if self.steered_place() != Some(from) {
            return;
        }
        let fresh = self.session.as_ref().and_then(|session| session.peers.get(&from)).and_then(|seen| seen.peer.shown()).and_then(|shown| shown.tell.full.clone().map(|full| (full, shown.tell.playing, shown.tell.name.clone())));
        if let (Some((full, playing, name)), Some(steering)) = (fresh, self.steering.as_mut()) {
            steering.view = Some(View { full, playing, heard_ms: now });
            if steering.name != name {
                out.push(Do::Steering(Some(name.clone())));
                steering.name = name;
            }
            out.push(Do::Draw);
        }
    }

    /// Something was pressed in the player or the queue panel. While another device is
    /// being steered it is asked of that device, and this says so (`Some`); otherwise
    /// `None`, and the press is this device's own.
    pub fn press(&mut self, press: Press, env: &mut Env) -> Option<Vec<Do>> {
        let steering = self.steering.as_ref()?;
        let mut out = Vec::new();
        // (Steering a device that has no place in the room just now: what is pressed has
        // nowhere to go, and is still not this device's own.)
        let (place, name) = (self.steered_place().unwrap_or_default(), steering.name.clone());
        let Some(command) = steering.view.as_ref().and_then(|view| press.command(view)) else { return Some(out) };
        match self.ask(place, command.clone(), Out::Steer, env, &mut out) {
            Pressed::Send(_) | Pressed::Waits => {
                // The player shows what was asked for straight away; the other device's
                // word on it follows a moment later.
                if let Some(view) = self.steering.as_mut().and_then(|steering| steering.view.as_mut()) {
                    view.at_once(&command, env.now_ms);
                }
                out.push(Do::Draw);
            }
            Pressed::No(wait) => self.not_sent(place, &name, wait, true, env, &mut out),
        }
        Some(out)
    }

    /// Songs were queued (Add to queue, Play next). While another device is being steered
    /// they are queued there; otherwise `None`.
    pub fn add(&mut self, ids: &[String], next: bool, env: &mut Env) -> Option<Vec<Do>> {
        let name = self.steering.as_ref()?.name.clone();
        let mut out = Vec::new();
        let place = self.steered_place().unwrap_or_default();
        // (An id that cannot be carried is a radio station's, or none.)
        let ids: Vec<String> = ids.iter().filter(|id| id_ok(id)).take(MOST_ADDED).cloned().collect();
        if ids.is_empty() {
            out.push(Do::Say(words::NOT_RADIO.into()));
            return Some(out);
        }
        let count = ids.len();
        if let Pressed::No(wait) = self.ask(place, Command::Add { ids, next }, Out::Add(count), env, &mut out) {
            self.not_sent(place, &name, wait, true, env, &mut out);
        }
        Some(out)
    }

    /// Something was played (an album, a song in a list). While another device is being
    /// steered it plays there, as it would here, taking the queue's place; otherwise `None`.
    /// A song that cannot be carried (a radio station) is given as an empty id.
    pub fn play(&mut self, ids: &[String], start: usize, context: &str, env: &mut Env) -> Option<Vec<Do>> {
        let name = self.steering.as_ref()?.name.clone();
        let mut out = Vec::new();
        let place = self.steered_place().unwrap_or_default();
        let Some((adopt, left)) = Adopt::fitted(ids, start, 0, 0.0, context) else {
            out.push(Do::Say(words::cannot_play_there(&name)));
            return Some(out);
        };
        if let Pressed::No(wait) = self.ask(place, Command::Adopt(adopt), Out::PlayThere(left), env, &mut out) {
            self.not_sent(place, &name, wait, true, env, &mut out);
        }
        Some(out)
    }

    /// A device was picked in the list: this one starts steering it, and if music is
    /// playing here it carries on over there, this one stopping once that one has started.
    /// `in_a_jam` is whether this device is in a jam with other people, when it steers nothing.
    pub fn choose(&mut self, place: &str, in_a_jam: bool, env: &mut Env) -> Vec<Do> {
        let mut out = Vec::new();
        let Some(to) = text::read_place(place) else { return out };
        let Some(name) = self.name(&to) else { return out };
        if self.steered_place() == Some(to) {
            return out;
        }
        if in_a_jam {
            out.push(Do::Say(words::IN_A_JAM.into()));
            return out;
        }
        // Music playing here moves over there; otherwise that device is steered as it is.
        let playing = self.steering.is_none() && env.player.brief().playing;
        let handed = playing.then(|| env.player.handed()).filter(|handed| !handed.songs.is_empty());
        let Some(handed) = handed else {
            self.let_go_into(&mut out);
            self.steer(to, env, &mut out);
            return out;
        };
        let Some((adopt, _)) = Adopt::fitted(&handed.songs, 0, handed.queued, handed.position, &handed.context) else { return out };
        if self.unanswered(&to) {
            self.not_sent(to, &name, Wait::Unanswered, true, env, &mut out);
            return out;
        }
        let giving = Giving::begin(env.player.generation(), env.now_ms);
        let sent = adopt.songs.len();
        match self.ask(to, Command::Adopt(adopt), Out::Give(giving, sent, handed.more as usize), env, &mut out) {
            Pressed::Send(_) => {
                self.note = words::moving(&name);
                out.push(Do::Say(words::moving(&name)));
            }
            Pressed::Waits => {}
            Pressed::No(wait) => self.not_sent(to, &name, wait, true, env, &mut out),
        }
        out
    }

    /// "Play here": brings what the device being steered is playing over to this one, and
    /// stops steering it. `None` if no device was being steered.
    pub fn play_here(&mut self, env: &mut Env) -> Option<Vec<Do>> {
        let name = self.steering.as_ref()?.name.clone();
        let mut out = Vec::new();
        let place = self.steered_place().unwrap_or_default();
        // Its queue is asked for; what is done with it is in `took`.
        match self.send(place, ASK, Ask::Look(Want::Queue).text(), env, &mut out) {
            Some(nonce) => {
                let now = env.now_ms;
                if let Some(seen) = self.peer_mut(&place) {
                    seen.peer.looked(nonce, now);
                }
                self.take = Some(Take::Asked { place, nonce, name: name.clone(), pressed_ms: now });
                out.push(Do::Log(format!("take: asked {name} for its queue")));
            }
            None => {
                self.let_go_into(&mut out);
                self.say(words::cannot_be_reached_to_move(&name), &mut out);
            }
        }
        Some(out)
    }

    /// The queue that was asked for has arrived: it is taken and started here. The other
    /// device is paused once this one sounds (`moves`).
    fn took(&mut self, from: Place, handed: Option<Handed>, at: At, playing: bool, env: &mut Env, out: &mut Vec<Do>) {
        let Some(Take::Asked { name, pressed_ms, .. }) = self.take.take() else { return };
        // The player is this device's own from here, whatever comes of it.
        self.let_go_into(out);
        let Some(handed) = handed.filter(|handed| playing && !handed.songs.is_empty()) else {
            // Nothing was playing there to bring over: it is just let go of.
            return out.push(Do::Log(format!("take: {name} was not playing anything")));
        };
        let (songs, begin) = known(env.library, &handed.songs, 0, handed.queued);
        if songs.is_empty() {
            return self.say(words::none_of_those_songs(&name, env.me.here), out);
        }
        // Whether it went in is asked of the player afresh, and gone by: its queue has a
        // new number, or it has not.
        let before = env.player.queue();
        let said = env.player.take(&songs, begin, handed.position, &handed.context);
        let queue = env.player.queue();
        if queue == before {
            // Nothing was installed: nothing is waited for, and the other device is not
            // told to stop on the strength of what was playing here before.
            out.push(Do::Log(format!("take: {name}'s queue could not be put in the player here{}", if said { " (though the player said it was)" } else { "" })));
            return self.say(words::could_not_load_here(&name, env.me.here), out);
        }
        let taking = Taking::begin(at, env.player.generation(), pressed_ms);
        // Busy to the other devices until it sounds or is given up (a little longer than
        // the take itself is given, so that the wait is never what ends first).
        let waiting = env.player.wait(env.now_ms, MOVE_MS + 5_000);
        out.push(Do::Log(format!("take: loaded {name}'s queue here; it is paused when {} sounds", env.me.here.this_lower())));
        self.take = Some(Take::Loaded { place: from, name, taking, queue, heard_from: None, waiting });
    }

    /// The list: the other devices and what each is doing, and after them the ones seen
    /// before that are not here now. Says, the first time and whenever it changes, what
    /// it shows ([`Do::Log`]).
    pub fn shown(&mut self, now_ms: u64) -> (Shown, Vec<Do>) {
        let steered = self.steering.as_ref().map(|steering| steering.device);
        let (mut rows, mut here) = (Vec::new(), Vec::new());
        if let Some(session) = &self.session {
            for place in &session.others {
                let Some(shown) = session.peers.get(place).and_then(|seen| seen.peer.shown()) else { continue };
                let tell = &shown.tell;
                here.push(text::base64(&tell.device));
                let row = Row { place: text::place(place), name: tell.name.clone(), about: words::about(tell, now_ms.saturating_sub(shown.sent_ms) / 1000), playing: tell.playing, active: steered == Some(tell.device), ready: true };
                rows.push((tell.device, shown.sent_ms, row));
            }
        }
        let entered = self.session.as_ref().is_some_and(|session| session.place.is_some());
        let mut rows = latest_of_each(rows);
        // The devices seen before and not here now: listed at once, and not to be picked.
        for known in self.remembered.iter().filter(|known| !here.contains(&known.id)) {
            rows.push(Row { place: String::new(), name: known.name.clone(), about: if entered { words::NOT_HERE } else { words::LOOKING_FOR_IT }.into(), playing: false, active: false, ready: false });
        }
        let mut out = Vec::new();
        if rows != self.listed {
            out.push(Do::Log(format!("shown: {}", if rows.is_empty() { "no other devices".to_string() } else { rows.iter().map(|row| format!("{}: {}", row.name, row.about)).collect::<Vec<_>>().join("; ") })));
            self.listed = rows.clone();
        }
        (Shown { entered, rows, note: self.note.clone(), steering: self.steered().unwrap_or_default().to_string() }, out)
    }
}

#[cfg(test)]
mod tests;
