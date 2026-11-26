//! The play queue between devices: whose is the newest, decided here once.
//!
//! The queue and the place in it go through the server's own saved queue. Whichever device
//! changed it last has the say, and a device that's playing keeps its own. The server keeps
//! one saved queue, puts its own clock on a save, and has no save that is conditional on
//! what was read; so a device that saves without looking replaces whatever another device
//! saved a moment before, and its save is then the newest there is. Hence the rule: **a
//! device saves its queue only if it is still the newest thing that happened.**
//!
//! And the rule for what comes the other way: **a queue the person has made their own is
//! never replaced by itself.** A device puts another device's newer queue in its own
//! queue's place only while it has nothing of the person's to lose: its queue is empty, or
//! is one it took up from another device and nothing has been done to since. From the
//! first thing done to a queue here (a song added, removed or moved, the queue cleared,
//! anything played, a seek, next, previous) it is this device's own, and another device's
//! newer queue is only ever **offered** ("Continue here"), for the person to take or leave.
//!
//! **A queue that has been handed to another device is that device's.** When the person
//! moves their music from this device to another (linking: "Play here" pressed there, or
//! this device's music carried on there), the other device took the queue by the person's
//! own choice and it is theirs there. On this one it is from that moment in the same state
//! as a queue taken up from that device and not touched: **it is not saved, and it
//! follows** (what that device saves is put in its place, paused), until the person uses
//! it here. Otherwise the stop that ends the move, on a device that had played on since
//! its last save, would be saved with a fresh time and could land after the other
//! device's first save: the silent device would be the newest. [`HANDED`] is what
//! [`Here::taken`] is set to, at the moment the queue is handed on and before this
//! device's sound is stopped.
//!
//! **The contract.** What an app keeps, and saves with the queue so that it is there after a
//! restart: [`Here::stamp`], when the queue was last changed or played on here, **by this
//! device's own clock**; and [`Here::taken`], set when the queue is one taken up from
//! another device or handed to one, and cleared when anything is done to it here.
//!
//! What every app owes, the Windows app and the phone alike:
//!
//! * **Whose the queue is, is said at the person's own action**, not found out afterwards
//!   from the queue looking different. `taken` is cleared and the stamp set at the press: a
//!   row played, a song queued, moved or removed, the queue cleared, a jump in it, Play,
//!   Pause, a seek, Next, Previous, however it reaches the player. A seek while paused, or
//!   Play and then Pause inside a second, changes nothing a look at the queue would notice,
//!   and is the person's own all the same. What the app does by itself claims nothing: the
//!   next song beginning, a failed one passed over, similar songs arriving, a sleep timer,
//!   the sound going, another device's queue put in the player, a restart putting the last
//!   run's queue back.
//!   **Which is which is said once**, at each way into the player, with who chose it and
//!   what it touches ([`crate::ways`]): this and the order of choices that linking keeps
//!   both come of that one statement. A guest in a jam the device hosts, and the person at
//!   another of their devices (a command carried out here), choose as the person here does.
//! * **"Continue here", once taken, is the person's own choice**: the queue put in by it is
//!   this device's own from that moment, not `taken`.
//! * **[`Here::ids`] is always given**: the ids of the queue's songs, in order, as the
//!   device would save them ([`window`]); and where it would save nothing though something
//!   is at the cursor (a radio station), the id of that one thing. It is empty only for an
//!   empty queue, and [`Here::current`] is `None` only then too. Both or neither: a device
//!   that names a current song and no songs is in breach, and [`look`] stops a debug build
//!   on it. (A release build then goes by the current song alone, which can hide an offer.)
//! * **An offer that is showing is withdrawn** when it is taken, when the person does
//!   anything to the queue or to playing it here, when a look could not ask the server, and
//!   when sharing is switched off; and the offer of one saved queue (its time and who saved
//!   it), once taken or acted past, is never made again.
//! * **Looks are numbered**: the answer to an older one that arrives after a newer one's
//!   changes nothing.
//!
//! Then:
//!
//! *A save* (a few seconds after a change, and now and then while a song plays). Saves take
//! turns, one at a time, in the order asked.
//! 1. [`first`] says whether to read the server's saved queue first. While a song is playing
//!    here it says no (a device that is playing keeps its own queue): go to step 3.
//! 2. Read the saved queue, noting its `Date` and this device's clock as it arrived
//!    ([`Clock::measured`]): the difference between the clocks **as measured by this very
//!    read** is what the times are compared with. Ask [`save`].
//! 3. Immediately before sending, confirm the queue is still the one this save was for
//!    (nothing has been done to it since step 1), the sign-in is the same and the setting
//!    is on. Otherwise send nothing: the change that came after has a save of its own.
//! 4. [`Save::NotNow`] and a save that fails are **still owed**; nothing need be remembered
//!    for that, because step 2 of a look finds it.
//!
//! *A look* (at start, each minute, when the server comes back).
//! 1. Read the saved queue, with its `Date` as above.
//! 2. Ask [`look`]. [`Then::TakeUp`]: put that queue in the player, paused, and keep
//!    [`Place::stamp`] and [`Place::taken`] as the queue's own. [`Then::Save`]: this
//!    device's queue is newer than what the server holds, so a save is owed: make one, from
//!    step 1 of a save. [`Looked::offer`] is what to offer as "Continue here", or to
//!    withdraw: an offer stands only as long as the looks go on making it.
//!
//! *"Continue here"*, when the person takes the offer: put that queue in the player
//! ([`place`]) and play it. Playing it makes it this device's own.
//!
//! **A tie goes to the other device**, in a save and in a look alike: both times are whole
//! seconds, and the other's save is already there. A device whose clock is more than five
//! minutes from the server's neither saves nor takes up; nor does one that has not yet
//! measured the difference since it started (a time carried over from an earlier run, or an
//! earlier version, was read off a clock nobody has checked).
//!
//! Accepted, and not solved here: between the read and the save another device can still
//! save, and is overwritten.
//!
//! Accepted too: a device that is not playing saves a queue the person has just changed
//! though another device is playing, and for the seconds until that device's next save (it
//! saves as it plays, without looking) the server holds the one that is not being played.
//! A third device that looks in those seconds, with nothing of its own, takes that one up,
//! and puts it right at its next look (what it took is still `taken`, so it follows). The
//! saved queue doesn't say whether its device is playing; guessing so from how lately it
//! was saved would hold back a person's own change for longer than the mistake lasts.

use serde::{Deserialize, Serialize};

use crate::model::{PlayQueue, Song};
pub use crate::time::Clock;
use crate::time::{aligned, parse_time};

/// How much of a long queue is passed on: this many songs before the current one...
pub const QUEUE_BEHIND: usize = 50;
/// ...and this many from it onwards.
pub const QUEUE_AHEAD: usize = 450;
/// For how long after another device saved its queue it is offered as "Continue here".
pub const OFFER_FOR: i64 = 30 * 60;

/// What [`Here::taken`] is while the queue is one this device has handed to another and
/// not used since (see the top of this file). It is no time a server gives a save, so no
/// saved queue is ever taken for "the one already taken up": whatever the other device
/// saves is followed.
pub const HANDED: &str = "handed to another device";

/// **The mark of a queue handed to another device belongs to that queue**: the one that
/// was in the player when it was handed on, by its number (a new number each time a whole
/// queue is put in the player). An app keeps one of these beside [`Here::taken`].
///
/// A hand-over is marked before this device's sound is stopped, and taken back if the
/// device then did not stop. It is taken back from the queue it was put on and no other:
/// if another whole queue is in the player by then, whose that queue is was settled by
/// whatever put it there, and nothing is given back.
#[derive(Debug, Default)]
pub struct Mark(Option<(u64, Option<String>)>);

impl Mark {
    /// The queue with this number is handed on. `taken` is what [`Here::taken`] was;
    /// what it is to be now is given back ([`HANDED`]).
    pub fn on(&mut self, queue: u64, taken: Option<String>) -> String {
        // (Marked twice with nothing between: what it was before the first still stands.)
        let was = match self.0.take() {
            Some((marked, was)) if marked == queue => was,
            _ => taken,
        };
        self.0 = Some((queue, was));
        HANDED.into()
    }

    /// The device did not stop, and `queue` is the number of the queue in the player now.
    /// What [`Here::taken`] is to be again, if that queue is the one that was marked.
    pub fn back(&mut self, queue: u64) -> Option<Option<String>> {
        self.0.take().filter(|(marked, _)| *marked == queue).map(|(_, was)| was)
    }
}

/// The name a device gives when it saves the queue, so it can tell its own from others'.
/// `id` is made up once by the device: eight hex digits.
pub fn device_name(id: &str) -> String {
    format!("NexusMusic-{id}")
}

/// What of a queue is saved on the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub ids: Vec<String>,
    pub current: String,
    /// How far into the current song, in milliseconds.
    pub position: u64,
}

/// What is passed on of a queue, given its songs in the order they play: at most
/// [`QUEUE_BEHIND`] before the current one and [`QUEUE_AHEAD`] from it on, radio stations
/// left out. `None` with nothing at the cursor, or a station there (not something another
/// device could pick up by id).
pub fn window(in_order: &[&Song], cursor: usize, position: f64) -> Option<Saved> {
    let current = in_order.get(cursor)?;
    if !current.radio_url.is_empty() || current.id.is_empty() {
        return None;
    }
    let from = cursor.saturating_sub(QUEUE_BEHIND);
    let ids = in_order.iter().skip(from).take(cursor - from + QUEUE_AHEAD).filter(|song| song.radio_url.is_empty() && !song.id.is_empty()).map(|song| song.id.clone()).collect();
    Some(Saved { ids, current: current.id.clone(), position: (position.max(0.0) * 1000.0) as u64 })
}

/// This device's side of the comparison.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Here {
    /// The name this device saves under ([`device_name`]).
    pub device: String,
    /// The id of the song at the cursor, if the queue has one.
    pub current: Option<String>,
    /// Something is playing.
    pub playing: bool,
    /// When this device's queue last changed or was played on, **by this device's own
    /// clock** as it happened (seconds since 1970; 0 for never). For a queue taken up from
    /// another device, [`Place::stamp`].
    pub stamp: i64,
    /// Set while the queue is one taken up from another device with nothing done to it or
    /// played of it here since: when that device saved it, as the server said
    /// ([`Place::taken`]). And while it is one handed to another device and not used here
    /// since: [`HANDED`].
    pub taken: Option<String>,
    /// The songs of this device's queue, in order, as it would save them ([`window`]'s
    /// `ids`): what tells another device's queue with the same song current from the same
    /// queue. **Always given** (see the contract at the top): empty only for an empty queue,
    /// where `current` is `None` too; for a queue nothing of which would be saved (a radio
    /// station at the cursor), the id of what is at the cursor.
    pub ids: Vec<String>,
}

/// Where to pick a saved queue up, and what to keep with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    /// Which of its songs.
    pub cursor: usize,
    /// How far into it, in seconds: from the start of one that had all but ended.
    pub position: f64,
    /// When that queue was changed, as this device's clock would have had it: the queue's
    /// stamp from now on.
    pub stamp: i64,
    /// When it was saved, as the server said: [`Here::taken`] from now on, until something
    /// is done to the queue here.
    pub taken: String,
}

/// Where a saved queue had got to.
pub fn place(theirs: &PlayQueue, clock: Clock) -> Place {
    let cursor = theirs.songs.iter().position(|song| song.id == theirs.current).unwrap_or(0);
    let duration = theirs.songs.get(cursor).map(|song| song.duration as f64).unwrap_or(0.0);
    let position = theirs.position as f64 / 1000.0;
    let position = if duration > 0.0 && position >= duration - 1.0 { 0.0 } else { position.max(0.0) };
    let stamp = parse_time(&theirs.changed).saturating_sub(clock.offset().unwrap_or(0));
    Place { cursor, position, stamp, taken: theirs.changed.clone() }
}

/// What to do about the server's saved queue, after a look at it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Then {
    Nothing,
    /// Another device changed its queue at or after the time anything was last done here,
    /// and this device has nothing of the person's to lose by it (its queue is empty, or
    /// is one taken up from another device and left alone): put it in this one's place,
    /// paused.
    TakeUp(Place),
    /// This device's queue is newer than what the server holds: its save is owed.
    Save,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Looked {
    /// The song of the server's queue (by its place there) to offer as "Continue here":
    /// another device's queue, different from the one here, that is not taken up by itself.
    /// To a device that isn't playing it is offered for as long as it is the newer of the
    /// two; to one that is playing, for half an hour after it was saved. `None` withdraws
    /// any offer made before.
    pub offer: Option<usize>,
    pub then: Then,
}

fn others(theirs: &PlayQueue, here: &Here) -> bool {
    !theirs.songs.is_empty() && theirs.changed_by != here.device
}

/// Whether the clock is one to compare times by.
fn usable(clock: Clock) -> bool {
    clock != Clock::Unmeasured && !clock.wrong()
}

/// Whether the server's queue is the one this device has: the same song current and, where
/// the device says what its queue holds, the same songs around it.
fn same_queue(theirs: &PlayQueue, here: &Here) -> bool {
    // (A device that names a current song and no songs is in breach of the contract: a
    // debug build stops on it in `look`. A release build goes by the current song alone.)
    here.current.as_deref() == Some(theirs.current.as_str()) && (here.ids.is_empty() || here.ids.iter().eq(theirs.songs.iter().map(|song| &song.id)))
}

/// Decides what a look at the server's saved queue leads to. `clock` is this device's clock
/// beside the server's as that very read measured it, and `now` this device's clock.
pub fn look(theirs: &PlayQueue, here: &Here, clock: Clock, now: i64) -> Looked {
    debug_assert!(
        here.current.is_none() == here.ids.is_empty(),
        "a device says what its queue holds, always: a current song and its songs, or neither (see the contract at the top of handover.rs)"
    );
    if !usable(clock) {
        return Looked { offer: None, then: Then::Nothing };
    }
    let changed = parse_time(&theirs.changed);
    let stamp = aligned(here.stamp, clock.offset());
    let others = others(theirs, here);
    // The one this device has already taken up, untouched since.
    if others && here.taken.as_deref() == Some(theirs.changed.as_str()) {
        return Looked { offer: None, then: Then::Nothing };
    }
    // (A tie goes to the other device, as it does in a save.)
    let newer = others && changed >= stamp;
    // The queue here is the person's own: something is in it, and it is not one taken up
    // from another device and left alone.
    let own = here.current.is_some() && here.taken.is_none();
    if newer && !here.playing && !own {
        return Looked { offer: None, then: Then::TakeUp(place(theirs, clock)) };
    }
    // Whatever is not taken up is offered, if it is another queue than the one here: while
    // a song plays here, what another device was playing in the last half hour; otherwise
    // whatever is newer than the queue here, for as long as it is.
    let age = aligned(now, clock.offset()).saturating_sub(changed);
    // (Offered to a device that isn't playing only if it is strictly the newer: a queue
    // saved in the very second something was done here is not newer than that, and offering
    // it would bring back an offer the person has just acted past.)
    let offered = others && !same_queue(theirs, here) && if here.playing { (0..OFFER_FOR).contains(&age) } else { changed > stamp };
    let offer = offered.then(|| theirs.songs.iter().position(|song| song.id == theirs.current).unwrap_or(0));
    let owed = !newer && own && stamp > changed;
    Looked { offer, then: if owed { Then::Save } else { Then::Nothing } }
}

/// Whether a save begins by reading the server's saved queue.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum First {
    /// Read it, and ask [`save`].
    Read,
    /// Save without reading: a song is playing here, and a device that is playing keeps its
    /// own queue. (The saves of where a song has got to come every few seconds; the
    /// read-before-save is for a change made while nothing plays.)
    Save,
}

pub fn first(here: &Here) -> First {
    if here.playing && here.taken.is_none() { First::Save } else { First::Read }
}

/// Whether to save this device's queue, given the server's saved queue as read immediately
/// before.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Save {
    /// It is the newest thing that happened: save it.
    Save,
    /// It isn't this device's to save, or another device's is as new or newer: leave the
    /// server's as it is. Nothing is owed.
    NotNewest,
    /// Nothing may be saved now (the clock is wrong, or has not been measured). The save is
    /// still owed.
    NotNow,
}

/// `clock` is this device's clock beside the server's as the read of `theirs` measured it.
pub fn save(theirs: &PlayQueue, here: &Here, clock: Clock) -> Save {
    if !usable(clock) {
        return Save::NotNow;
    }
    if here.taken.is_some() || (others(theirs, here) && parse_time(&theirs.changed) >= aligned(here.stamp, clock.offset())) {
        return Save::NotNewest;
    }
    Save::Save
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "NexusMusic-feedf00d";
    const PC: &str = "NexusMusic-0badc0de";
    /// 1970-01-02T00:00:00Z.
    const DAY: i64 = 86_400;
    /// A device whose clock agrees with the server's.
    const RIGHT: Clock = Clock::Offset(0);

    fn song(id: &str) -> Song {
        Song { id: id.into(), title: format!("T{id}"), duration: 200, ..Default::default() }
    }

    /// A queue saved by `device` at `at` seconds into the second of January 1970.
    fn saved(device: &str, at: i64, ids: &[&str], current: &str, position: u64) -> PlayQueue {
        let changed = format!("1970-01-02T00:{:02}:{:02}Z", at / 60, at % 60);
        PlayQueue { songs: ids.iter().map(|id| song(id)).collect(), current: current.into(), position, changed, changed_by: device.into() }
    }

    /// This device, its queue last changed `stamp` seconds into that day by its own clock:
    /// a queue of the person's own, with the song "mine" current.
    fn here(stamp: i64) -> Here {
        Here { device: ME.into(), current: Some("mine".into()), playing: false, stamp: DAY + stamp, taken: None, ids: vec!["mine".into(), "next".into()] }
    }

    /// The same device with nothing in its queue.
    fn empty(stamp: i64) -> Here {
        Here { current: None, ids: Vec::new(), ..here(stamp) }
    }

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn a_queue_another_device_saved_since_is_not_saved_over() {
        // The person changed the queue here at 00:05:00, paused; in the three seconds before
        // its save goes, a PC saved its own at 00:05:02.
        assert_eq!(save(&saved(PC, 302, &["a", "b"], "a", 0), &here(300), RIGHT), Save::NotNewest);
        // One the PC saved before the change here is older than it: this device's is saved.
        assert_eq!(save(&saved(PC, 299, &["a", "b"], "a", 0), &here(300), RIGHT), Save::Save);
        // In the same second, the other device's stands: a tie goes to the one already there.
        assert_eq!(save(&saved(PC, 300, &["a", "b"], "a", 0), &here(300), RIGHT), Save::NotNewest);
        // Its own, read back, is no reason not to save again; nor is nothing saved at all,
        // nor an empty queue under another's name.
        assert_eq!(save(&saved(ME, 302, &["a"], "a", 0), &here(300), RIGHT), Save::Save);
        assert_eq!(save(&PlayQueue::default(), &here(300), RIGHT), Save::Save);
        assert_eq!(save(&saved(PC, 302, &[], "", 0), &here(300), RIGHT), Save::Save);
    }

    #[test]
    fn the_time_of_a_change_is_the_devices_own_and_is_set_beside_the_servers_when_compared() {
        // A device two minutes fast changes its queue when its own clock says 00:07:00 (the
        // server's says 00:05:00). That is the time it keeps: its own.
        let fast = Clock::Offset(-120);
        let device = here(420);
        // A PC saves two seconds later, at 00:05:02 by the server. Compared as it stands,
        // the device's time looks two minutes newer, and the PC's queue would be saved over.
        assert_eq!(save(&saved(PC, 302, &["a"], "a", 0), &device, fast), Save::NotNewest);
        assert_eq!(save(&saved(PC, 299, &["a"], "a", 0), &device, fast), Save::Save);
        // It makes no difference when the clock was measured: the same change, kept through
        // a restart and compared by a later reading of the difference, is judged the same.
        assert_eq!(save(&saved(PC, 302, &["a"], "a", 0), &device.clone(), Clock::Offset(-121)), Save::NotNewest);
        // A device two minutes slow is not kept from saving what really is the newer.
        assert_eq!(save(&saved(PC, 299, &["a"], "a", 0), &here(180), Clock::Offset(120)), Save::Save);
        // The half hour of "Continue here" is counted on the server's clock too.
        let playing = Here { playing: true, stamp: DAY + 2_300, ..here(0) };
        assert_eq!(look(&saved(PC, 600, &["a", "b"], "b", 0), &playing, fast, DAY + 600 + 1_799 + 120).offer, Some(1));
        assert_eq!(look(&saved(PC, 600, &["a", "b"], "b", 0), &playing, fast, DAY + 600 + 1_800 + 120).offer, None);
        // A queue taken up from another device keeps a stamp this device's clock would have
        // given it, so the next comparison sets it beside the server's again and finds a tie.
        let theirs = saved(PC, 600, &["a", "b"], "b", 0);
        let taken = place(&theirs, fast);
        assert_eq!((taken.stamp, taken.taken.as_str()), (DAY + 600 + 120, "1970-01-02T00:10:00Z"));
    }

    #[test]
    fn nothing_is_saved_or_taken_up_until_the_clock_has_been_measured_or_while_it_is_wrong() {
        // No answer has come from the server since the app started: the time on the queue
        // was read off a clock nobody has checked (this run's, or an earlier version's).
        assert_eq!(save(&saved(PC, 299, &["a"], "a", 0), &here(300), Clock::Unmeasured), Save::NotNow);
        assert_eq!(look(&saved(PC, 600, &["a", "b"], "b", 0), &here(300), Clock::Unmeasured, DAY + 700), Looked { offer: None, then: Then::Nothing });
        // More than five minutes out.
        let wrong = Clock::Offset(-3_600);
        assert_eq!(save(&saved(PC, 299, &["a"], "a", 0), &here(300), wrong), Save::NotNow);
        assert_eq!(save(&PlayQueue::default(), &here(300), wrong), Save::NotNow);
        assert_eq!(look(&saved(PC, 600, &["a", "b"], "b", 0), &here(300), wrong, DAY + 700), Looked { offer: None, then: Then::Nothing });
        // A server that gives no date can't be compared with: the clock is taken to be right.
        assert_eq!(save(&saved(PC, 299, &["a"], "a", 0), &here(300), Clock::NoDate), Save::Save);
        assert_eq!(Clock::measured(None, 5), Clock::NoDate);
        assert_eq!(Clock::measured(Some(65), 5), Clock::Offset(60));
    }

    #[test]
    fn a_queue_taken_up_from_another_device_is_not_saved_as_this_ones_own() {
        // The PC's queue, saved at 00:10:00, was taken up here.
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        let Then::TakeUp(taken_up) = look(&theirs, &empty(100), RIGHT, DAY + 700).then else { panic!("not taken up") };
        let taken = Here { current: Some("b".into()), ids: ids(&["a", "b", "c"]), stamp: taken_up.stamp, taken: Some(taken_up.taken.clone()), ..here(0) };
        // A save asked for before it was taken up comes due: what is in the player is the
        // PC's queue, untouched, and it is not passed on as this device's, changed now.
        assert_eq!(first(&taken), First::Read);
        assert_eq!(save(&theirs, &taken, RIGHT), Save::NotNewest);
        // The next look finds the same queue on the server, and does not take it up again.
        assert_eq!(look(&theirs, &taken, RIGHT, DAY + 700).then, Then::Nothing);
        assert_eq!(look(&theirs, &taken, Clock::Offset(1), DAY + 700).then, Then::Nothing);
        // Nor is it saved if the server's saved queue has gone since (cleared from another
        // app): it was never this device's, and nothing of it is owed.
        assert_eq!(save(&PlayQueue::default(), &taken, RIGHT), Save::NotNewest);
        assert_eq!(look(&PlayQueue::default(), &taken, RIGHT, DAY + 700).then, Then::Nothing);
        // All of which is so after a restart, the two things kept having been kept.
        let restarted: Here = serde_json::from_str(&serde_json::to_string(&taken).unwrap()).unwrap();
        assert_eq!(save(&PlayQueue::default(), &restarted, RIGHT), Save::NotNewest);
        // Played on here, it is this device's to save. A newer save by that device is
        // taken up as any is.
        let played = Here { taken: None, stamp: DAY + 660, ..taken.clone() };
        assert_eq!(save(&theirs, &played, RIGHT), Save::Save);
        assert!(matches!(look(&saved(PC, 700, &["z"], "z", 0), &taken, RIGHT, DAY + 800).then, Then::TakeUp(_)));
    }

    /// The mark is of one queue. Taken back, it gives that queue back to whoever had it;
    /// and it is not taken back from another queue that has been put in the player since.
    #[test]
    fn a_hand_over_is_taken_back_from_the_queue_it_was_marked_on_and_no_other() {
        let mut mark = Mark::default();
        // Nothing was marked: nothing to give back.
        assert_eq!(mark.back(4), None);
        // Its own queue, marked and taken back: its own again. Once.
        assert_eq!(mark.on(4, None), HANDED);
        assert_eq!((mark.back(4), mark.back(4)), (Some(None), None));
        // One taken up from another device and not touched: that device's again.
        mark.on(4, Some("2026-11-26T10:00:00Z".into()));
        assert_eq!(mark.back(4), Some(Some("2026-11-26T10:00:00Z".into())));
        // Marked twice (the stop was asked for twice): what it was before the first.
        mark.on(4, None);
        mark.on(4, Some(HANDED.into()));
        assert_eq!(mark.back(4), Some(None));
        // Another whole queue is in the player by the time it is taken back: nothing is
        // given back, then or later.
        mark.on(4, None);
        assert_eq!((mark.back(5), mark.back(4)), (None, None));
        // And a mark left on a queue that has gone says nothing of the next one marked.
        mark.on(4, Some("theirs".into()));
        mark.on(6, None);
        assert_eq!(mark.back(6), Some(None));
    }

    /// The person moved their music from this device to the PC. This device had played on
    /// since its last save, and stops now: nobody's choice here.
    #[test]
    fn a_queue_handed_to_another_device_is_not_saved_and_follows_until_it_is_used_here() {
        let handed = |playing: bool| Here { taken: Some(HANDED.into()), playing, ..here(100) };
        // Its own last save is what the server holds (the PC has not saved yet), or the
        // PC's first save is, made a moment before this device was told the music had
        // moved: either way it does not save. Not even while its sound is still going.
        let own = saved(ME, 90, &["mine", "next"], "mine", 0);
        let theirs = saved(PC, 101, &["mine", "next"], "mine", 30_000);
        for playing in [false, true] {
            assert_eq!(first(&handed(playing)), First::Read);
            for held in [&own, &theirs, &PlayQueue::default()] {
                assert_eq!(save(held, &handed(playing), RIGHT), Save::NotNewest, "playing {playing}");
            }
        }
        // (Left as its own, the stop would have been a change to save, and saved: the
        // silent device the newest.)
        assert_eq!(save(&own, &here(100), RIGHT), Save::Save);
        // It follows: what the PC saves is put in its place, paused, and from then on it
        // is a queue taken up from the PC like any other. Nothing is offered meanwhile, and
        // no save is owed.
        assert_eq!(look(&own, &handed(false), RIGHT, DAY + 105), Looked { offer: None, then: Then::Nothing });
        let followed = look(&theirs, &handed(false), RIGHT, DAY + 105);
        assert_eq!(followed, Looked { offer: None, then: Then::TakeUp(place(&theirs, RIGHT)) });
        let next = saved(PC, 160, &["mine", "next"], "next", 5_000);
        let taken_up = Here { taken: Some(theirs.changed.clone()), stamp: DAY + 101, ..here(0) };
        assert_eq!(look(&next, &taken_up, RIGHT, DAY + 165).then, Then::TakeUp(place(&next, RIGHT)));
        // The mark is no time a server gives: the PC's save is never "the one already taken up".
        assert_ne!(theirs.changed, HANDED);
        // Until the person uses it here: then it is this device's own, changed now, and saved.
        let used = Here { taken: None, playing: true, ..here(170) };
        assert_eq!((first(&used), save(&next, &Here { playing: false, ..used.clone() }, RIGHT)), (First::Save, Save::Save));
    }

    #[test]
    fn the_saved_queue_is_taken_up_offered_or_left() {
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        let place_there = Place { cursor: 1, position: 30.0, stamp: DAY + 600, taken: theirs.changed.clone() };
        // Nothing in the queue here: taken up, paused, and nothing offered.
        assert_eq!(look(&theirs, &empty(100), RIGHT, DAY + 700), Looked { offer: None, then: Then::TakeUp(place_there.clone()) });
        // A queue of the person's own, not playing: left as it is, and the other offered,
        // for as long as it is the newer. Not for half an hour: the next morning too.
        let idle = here(100);
        assert_eq!(look(&theirs, &idle, RIGHT, DAY + 700), Looked { offer: Some(1), then: Then::Nothing });
        assert_eq!(look(&theirs, &idle, RIGHT, DAY + 600 + 20 * 3_600), Looked { offer: Some(1), then: Then::Nothing });
        // Playing something of its own: left alone, and offered what another device was
        // playing in the last half hour.
        let busy = Here { playing: true, ..here(100) };
        assert_eq!(look(&theirs, &busy, RIGHT, DAY + 700), Looked { offer: Some(1), then: Then::Nothing });
        assert_eq!(look(&theirs, &busy, RIGHT, DAY + 600 + OFFER_FOR).offer, None);
        // Nobody is offered the queue they have already, nor their own.
        for device in [&idle, &busy] {
            assert_eq!(look(&theirs, &Here { current: Some("b".into()), ids: ids(&["a", "b", "c"]), ..device.clone() }, RIGHT, DAY + 700).offer, None);
            assert_eq!(look(&saved(ME, 600, &["a", "b"], "b", 0), device, RIGHT, DAY + 700), Looked { offer: None, then: Then::Nothing });
        }
        // Changed here after the PC saved, and paused: the PC's is the older, so it is
        // neither taken up nor offered, and it is not left to stand: this device's save is
        // owed.
        assert_eq!(look(&theirs, &here(650), RIGHT, DAY + 700), Looked { offer: None, then: Then::Save });
        // A song that had all but ended is picked up from its start; one the queue doesn't
        // name, at its first song.
        assert_eq!(place(&saved(PC, 600, &["a", "b"], "b", 199_500), RIGHT).position, 0.0);
        assert_eq!(place(&saved(PC, 600, &["a", "b"], "zz", 5_000), RIGHT).cursor, 0);
    }

    #[test]
    fn the_same_song_in_another_queue_is_another_queue() {
        // Both devices are on the song "b". Where this one says what is in its queue, it is
        // offered the other's only if the songs around "b" are not the same ones.
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        let on_b = Here { current: Some("b".into()), ..here(100) };
        assert_eq!(look(&theirs, &Here { ids: ids(&["a", "b", "c"]), ..on_b.clone() }, RIGHT, DAY + 700).offer, None);
        // The person added a song here: another queue, and the PC's is offered.
        assert_eq!(look(&theirs, &Here { ids: ids(&["a", "b", "c", "x"]), ..on_b.clone() }, RIGHT, DAY + 700).offer, Some(1));
        // Or the PC's list was changed after the song both are on.
        assert_eq!(look(&saved(PC, 600, &["a", "b", "z"], "b", 0), &Here { ids: ids(&["a", "b", "c"]), ..on_b.clone() }, RIGHT, DAY + 700).offer, Some(1));
        // The same while a song plays here, within the half hour.
        let playing = Here { playing: true, ids: ids(&["a", "b", "c", "x"]), ..on_b.clone() };
        assert_eq!(look(&theirs, &playing, RIGHT, DAY + 700).offer, Some(1));
        assert_eq!(look(&theirs, &Here { ids: ids(&["a", "b", "c"]), ..playing }, RIGHT, DAY + 700).offer, None);
        // What is in the queue here never decides whether another's is taken up: only
        // whether there is anything of the person's to lose.
        assert_eq!(look(&theirs, &Here { ids: ids(&["a", "b", "c"]), ..on_b.clone() }, RIGHT, DAY + 700).then, Then::Nothing);
    }

    #[test]
    fn a_queue_the_person_has_made_their_own_is_never_replaced_by_itself() {
        // What happened on a phone: a PC is playing, and saves its queue as it goes. The
        // phone, not playing, took that queue up (00:10:00), and has followed it since.
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        let Then::TakeUp(taken_up) = look(&theirs, &empty(0), RIGHT, DAY + 610).then else { panic!("not taken up") };
        let following = Here { current: Some("b".into()), ids: ids(&["a", "b", "c"]), stamp: taken_up.stamp, taken: Some(taken_up.taken.clone()), ..here(0) };
        // Left alone it goes on following: the PC's next save is taken up as the last was.
        assert!(matches!(look(&saved(PC, 620, &["a", "b", "c"], "b", 50_000), &following, RIGHT, DAY + 625).then, Then::TakeUp(_)));

        // The person adds a song to it at 00:11:00. From then it is this device's own queue.
        let own = Here { taken: None, stamp: DAY + 660, ids: ids(&["a", "b", "c", "x"]), ..following.clone() };
        // Its save goes (the PC's last save is the older)...
        assert_eq!(save(&saved(PC, 640, &["a", "b", "c"], "b", 70_000), &own, RIGHT), Save::Save);
        // ...and the PC, playing, saves over it twenty seconds later without a look. The
        // look after that finds the PC's queue the newer. It is not put in this one's
        // place: the song just added is still here.
        // The PC's is offered instead: the same song, but not the same songs after it.
        let again = saved(PC, 680, &["a", "b", "c"], "b", 110_000);
        assert_eq!(look(&again, &own, RIGHT, DAY + 700), Looked { offer: Some(1), then: Then::Nothing });
        // Nor does this device save over the PC again: the two don't take turns.
        assert_eq!(save(&again, &own, RIGHT), Save::NotNewest);
        // When the PC moves on to another song, that is offered, and still nothing is done
        // by itself, however many looks go by.
        let moved = saved(PC, 800, &["a", "b", "c"], "c", 0);
        assert_eq!(look(&moved, &own, RIGHT, DAY + 815), Looked { offer: Some(2), then: Then::Nothing });
        assert_eq!(look(&moved, &own, RIGHT, DAY + 830), Looked { offer: Some(2), then: Then::Nothing });
        // All of which is so after a restart: what says the queue is the person's own is
        // what the device keeps with it.
        let restarted: Here = serde_json::from_str(&serde_json::to_string(&own).unwrap()).unwrap();
        assert_eq!(look(&moved, &restarted, RIGHT, DAY + 815).then, Then::Nothing);

        // A queue is the person's own from the first thing done to it, whatever that was
        // and however long ago: one played here yesterday is not replaced either.
        assert_eq!(look(&moved, &here(-80_000), RIGHT, DAY + 815).then, Then::Nothing);
        // A device with nothing in its queue has nothing of the person's to lose: taken up.
        assert!(matches!(look(&moved, &empty(-80_000), RIGHT, DAY + 815).then, Then::TakeUp(_)));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "a device says what its queue holds")]
    fn a_device_that_names_a_current_song_and_no_songs_is_stopped_in_a_debug_build() {
        // (Left to pass, it would be judged by its current song alone, and an offer of the
        // same song in another queue would be hidden.)
        let silent = Here { ids: Vec::new(), ..here(100) };
        look(&saved(PC, 600, &["a", "b", "c"], "b", 0), &silent, RIGHT, DAY + 700);
    }

    #[test]
    fn a_tie_goes_to_the_other_device_when_saving_and_when_looking_alike() {
        // Changed here in the very second the PC saved its own.
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        assert_eq!(save(&theirs, &here(600), RIGHT), Save::NotNewest);
        // And so a look counts the PC's as the newer: the two rules agree. Where there is
        // nothing of the person's to lose it is taken up; a queue of the person's own is
        // offered it. (With the save refused and the look waiting for something strictly
        // newer, this device's change was neither on the server nor answered by anything.)
        assert!(matches!(look(&theirs, &empty(600), RIGHT, DAY + 700).then, Then::TakeUp(_)));
        // A queue of the person's own keeps its place either way. And the PC's is not
        // offered at a tie: it is not newer than what was done here in that same second, and
        // an offer the person has just acted past must not come straight back.
        assert_eq!(look(&theirs, &here(600), RIGHT, DAY + 700), Looked { offer: None, then: Then::Nothing });
        assert_eq!(look(&theirs, &here(599), RIGHT, DAY + 700), Looked { offer: Some(1), then: Then::Nothing });
        assert_eq!(look(&theirs, &here(601), RIGHT, DAY + 700), Looked { offer: None, then: Then::Save });
    }

    #[test]
    fn a_save_that_could_not_be_made_is_still_owed_and_is_after_a_restart() {
        // Changed here at 00:05:00 with no network; the save was never made. Each look since
        // finds the server holding something older, this device's own or another's or
        // nothing: the save is owed.
        for theirs in [saved(ME, 200, &["old"], "old", 0), saved(PC, 250, &["a"], "a", 0), PlayQueue::default()] {
            assert_eq!(look(&theirs, &here(300), RIGHT, DAY + 400).then, Then::Save, "{}", theirs.changed_by);
            // What says so is what the device keeps with its queue: read back after a
            // restart, it says the same.
            let kept: Here = serde_json::from_str(&serde_json::to_string(&here(300)).unwrap()).unwrap();
            assert_eq!(look(&theirs, &kept, RIGHT, DAY + 400).then, Then::Save);
        }
        // Once it is there (saved three seconds after the change), nothing is.
        assert_eq!(look(&saved(ME, 303, &["mine"], "mine", 0), &here(300), RIGHT, DAY + 400).then, Then::Nothing);
        // With nothing in the queue there is nothing to owe.
        assert_eq!(look(&PlayQueue::default(), &empty(300), RIGHT, DAY + 400).then, Then::Nothing);
    }

    #[test]
    fn while_a_song_plays_the_saves_of_its_place_do_not_each_read_the_server_first() {
        assert_eq!(first(&Here { playing: true, ..here(300) }), First::Save);
        // A change made while nothing plays reads first.
        assert_eq!(first(&here(300)), First::Read);
    }

    #[test]
    fn a_long_queue_is_passed_on_as_fifty_behind_and_four_hundred_and_fifty_ahead() {
        let songs: Vec<Song> = (0..1_000).map(|n| Song { id: format!("s{n}"), radio_url: if n == 620 { "http://station".into() } else { String::new() }, ..Default::default() }).collect();
        let in_order: Vec<&Song> = songs.iter().collect();
        let saved = window(&in_order, 100, 12.5).unwrap();
        assert_eq!((saved.ids.len(), saved.ids[0].as_str(), saved.ids[499].as_str()), (500, "s50", "s549"));
        assert_eq!((saved.current.as_str(), saved.position), ("s100", 12_500));
        // A station in the queue is left out of what is passed on.
        let late = window(&in_order, 600, 0.0).unwrap();
        assert_eq!((late.ids.len(), late.ids[0].as_str(), late.ids[448].as_str()), (449, "s550", "s999"));
        assert!(!late.ids.contains(&"s620".to_string()));
        // Near the start there is less behind; nothing at the cursor, or a station there, is
        // nothing to pass on.
        assert_eq!(window(&in_order, 3, 0.0).unwrap().ids.len(), 453);
        assert_eq!(window(&in_order, 620, 0.0), None);
        assert_eq!(window(&in_order, 1_000, 0.0), None);
        assert_eq!(device_name("0badc0de"), PC);
    }
}
