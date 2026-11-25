//! The play queue between devices: whose is the newest, decided here once.
//!
//! The queue and the place in it go through the server's own saved queue. Whichever device
//! changed it last has the say, and a device that's playing keeps its own. The server keeps
//! one saved queue, puts its own clock on a save, and has no save that is conditional on
//! what was read; so a device that saves without looking replaces whatever another device
//! saved a moment before, and its save is then the newest there is. Hence the rule: **a
//! device saves its queue only if it is still the newest thing that happened.**
//!
//! **The contract.** What an app keeps, and saves with the queue so that it is there after a
//! restart: [`Here::stamp`], when the queue was last changed or played on here, **by this
//! device's own clock**; and [`Here::taken`], set when the queue is one taken up from
//! another device and cleared when anything is done to it here. Then:
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
//!    withdraw.
//!
//! **A tie goes to the other device**, in a save and in a look alike: both times are whole
//! seconds, and the other's save is already there. A device whose clock is more than five
//! minutes from the server's neither saves nor takes up; nor does one that has not yet
//! measured the difference since it started (a time carried over from an earlier run, or an
//! earlier version, was read off a clock nobody has checked).
//!
//! Accepted, and not solved here: between the read and the save another device can still
//! save, and is overwritten.

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
    /// ([`Place::taken`]).
    pub taken: Option<String>,
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
    /// and nothing is playing here: put it in this one's place, paused.
    TakeUp(Place),
    /// This device's queue is newer than what the server holds: its save is owed.
    Save,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Looked {
    /// Another device has been playing something else in the last half hour: the song of
    /// the server's queue (by its place there) to offer as "Continue here". `None` withdraws
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

/// Decides what a look at the server's saved queue leads to. `clock` is this device's clock
/// beside the server's as that very read measured it, and `now` this device's clock.
pub fn look(theirs: &PlayQueue, here: &Here, clock: Clock, now: i64) -> Looked {
    if !usable(clock) {
        return Looked { offer: None, then: Then::Nothing };
    }
    let changed = parse_time(&theirs.changed);
    let stamp = aligned(here.stamp, clock.offset());
    let others = others(theirs, here);
    let age = aligned(now, clock.offset()).saturating_sub(changed);
    let offer = (others && (0..OFFER_FOR).contains(&age) && here.current.as_deref() != Some(theirs.current.as_str()))
        .then(|| theirs.songs.iter().position(|song| song.id == theirs.current).unwrap_or(0));
    // The one this device has already taken up, untouched since.
    if others && here.taken.as_deref() == Some(theirs.changed.as_str()) {
        return Looked { offer, then: Then::Nothing };
    }
    // (A tie goes to the other device, as it does in a save.)
    if others && changed >= stamp {
        // A device that's playing keeps its own queue, and is offered the other.
        return if here.playing { Looked { offer, then: Then::Nothing } } else { Looked { offer: None, then: Then::TakeUp(place(theirs, clock)) } };
    }
    let owed = here.current.is_some() && here.taken.is_none() && stamp > changed;
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

    /// This device, its queue last changed `stamp` seconds into that day by its own clock.
    fn here(stamp: i64) -> Here {
        Here { device: ME.into(), current: Some("mine".into()), playing: false, stamp: DAY + stamp, taken: None }
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
        let Then::TakeUp(taken_up) = look(&theirs, &here(100), RIGHT, DAY + 700).then else { panic!("not taken up") };
        let taken = Here { current: Some("b".into()), stamp: taken_up.stamp, taken: Some(taken_up.taken.clone()), ..here(0) };
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

    #[test]
    fn the_saved_queue_is_taken_up_offered_or_left() {
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        // Not playing, and nothing done here since: taken up, paused, and nothing offered.
        let idle = here(100);
        assert_eq!(look(&theirs, &idle, RIGHT, DAY + 700), Looked { offer: None, then: Then::TakeUp(Place { cursor: 1, position: 30.0, stamp: DAY + 600, taken: theirs.changed.clone() }) });
        // Playing something of its own: left alone, and offered.
        let busy = Here { playing: true, ..here(100) };
        assert_eq!(look(&theirs, &busy, RIGHT, DAY + 700), Looked { offer: Some(1), then: Then::Nothing });
        // Not offered the song it is on already, nor after half an hour, nor its own queue.
        assert_eq!(look(&theirs, &Here { current: Some("b".into()), ..busy.clone() }, RIGHT, DAY + 700).offer, None);
        assert_eq!(look(&theirs, &busy, RIGHT, DAY + 600 + OFFER_FOR).offer, None);
        assert_eq!(look(&saved(ME, 600, &["a", "b"], "b", 0), &busy, RIGHT, DAY + 700), Looked { offer: None, then: Then::Nothing });
        // Changed here after the PC saved, and paused: the PC's is neither taken up (this
        // one's is the newer) nor left to stand: this device's save is owed.
        assert_eq!(look(&theirs, &here(650), RIGHT, DAY + 700), Looked { offer: Some(1), then: Then::Save });
        // A song that had all but ended is picked up from its start; one the queue doesn't
        // name, at its first song.
        assert_eq!(place(&saved(PC, 600, &["a", "b"], "b", 199_500), RIGHT).position, 0.0);
        assert_eq!(place(&saved(PC, 600, &["a", "b"], "zz", 5_000), RIGHT).cursor, 0);
    }

    #[test]
    fn a_tie_goes_to_the_other_device_when_saving_and_when_looking_alike() {
        // Changed here in the very second the PC saved its own.
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        assert_eq!(save(&theirs, &here(600), RIGHT), Save::NotNewest);
        // And so a look takes the PC's up, where nothing is playing: the two rules agree,
        // and the devices end with the same queue. (With the save refused and the look
        // waiting for something strictly newer, each kept its own for good.)
        assert!(matches!(look(&theirs, &here(600), RIGHT, DAY + 700).then, Then::TakeUp(_)));
        assert_eq!(look(&theirs, &here(601), RIGHT, DAY + 700).then, Then::Save);
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
        assert_eq!(look(&PlayQueue::default(), &Here { current: None, ..here(300) }, RIGHT, DAY + 400).then, Then::Nothing);
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
