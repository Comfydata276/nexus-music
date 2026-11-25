//! The play queue between devices: whose is the newest, decided here once.
//!
//! The queue and the place in it go through the server's own saved queue. Whichever device
//! changed it last has the say, and a device that's playing keeps its own. The server keeps
//! one saved queue, puts its own clock on a save, and has no save that is conditional on
//! what was read; so a device that saves without looking replaces whatever another device
//! saved a moment before, and its save is then the newest there is. Hence:
//!
//! **A device saves its queue only if it is still the newest thing that happened**
//! ([`save`]). The app makes saves take turns, and immediately before each reads the
//! server's saved queue. If another device saved it at or after the time of the change this
//! save is for, nothing is saved: a tie goes to the other device (both times are whole
//! seconds, and the other's save is already there). A queue taken up from another device is
//! not saved until it has been changed or played on here. And a save that could not be made
//! (no network, the setting off, the clock wrong) is still owed: [`look`] says so whenever
//! it finds this device's queue newer than what the server holds.
//!
//! **Times are compared on the server's clock.** The time a saved queue changed is the
//! server's; the time this device's queue changed is kept by the app as the server would
//! have had it ([`crate::time::aligned`], with the difference measured at the last exchange),
//! and a queue taken up from another device keeps the server's time for it. While the
//! device's clock is more than five minutes from the server's, nothing is saved and nothing
//! taken up.
//!
//! Accepted, and not solved here: between the read and the save another device can still
//! save, and is overwritten.

use crate::model::{PlayQueue, Song};
use crate::time::{aligned, clock_wrong, parse_time};

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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, Copy, Default)]
pub struct Here<'a> {
    /// The name this device saves under ([`device_name`]).
    pub device: &'a str,
    /// The id of the song at the cursor, if the queue has one.
    pub current: Option<&'a str>,
    /// Something is playing.
    pub playing: bool,
    /// When this device's queue last changed or was played on, **on the server's clock**
    /// (seconds since 1970; 0 for never): `time::aligned(now, offset)` as it happened, or the
    /// saved queue's own time for one taken up from another device.
    pub stamp: i64,
    /// The queue is one taken up from another device, and nothing has been done to it or
    /// played of it here since.
    pub taken_up: bool,
    /// The server's clock minus this device's, as last measured ([`crate::time::offset`]).
    pub offset: Option<i64>,
}

/// Where to pick a saved queue up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Place {
    /// Which of its songs.
    pub cursor: usize,
    /// How far into it, in seconds: from the start of one that had all but ended.
    pub position: f64,
    /// When that queue was changed, on the server's clock: the device's queue stamp from now on.
    pub stamp: i64,
}

/// Where a saved queue had got to. It must have songs.
pub fn place(theirs: &PlayQueue) -> Place {
    let cursor = theirs.songs.iter().position(|song| song.id == theirs.current).unwrap_or(0);
    let duration = theirs.songs.get(cursor).map(|song| song.duration as f64).unwrap_or(0.0);
    let position = theirs.position as f64 / 1000.0;
    let position = if duration > 0.0 && position >= duration - 1.0 { 0.0 } else { position.max(0.0) };
    Place { cursor, position, stamp: parse_time(&theirs.changed) }
}

/// What to do about the server's saved queue, after a look at it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Then {
    Nothing,
    /// Another device changed its queue after anything was last done here, and nothing is
    /// playing here: put it in this one's place, paused.
    TakeUp(Place),
    /// This device's queue is newer than what the server holds: its save is owed. (Make it
    /// as any save is made, through [`save`], with a fresh read.)
    Save,
}

#[derive(Debug, Clone, Copy, PartialEq)]
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

/// Decides what a look at the server's saved queue leads to. `now` is this device's clock.
pub fn look(theirs: &PlayQueue, here: &Here, now: i64) -> Looked {
    if clock_wrong(here.offset) {
        return Looked { offer: None, then: Then::Nothing };
    }
    let changed = parse_time(&theirs.changed);
    let others = others(theirs, here);
    let age = aligned(now, here.offset) - changed;
    let offer = (others && (0..OFFER_FOR).contains(&age) && here.current != Some(theirs.current.as_str()))
        .then(|| theirs.songs.iter().position(|song| song.id == theirs.current).unwrap_or(0));
    if others && changed > here.stamp {
        // A device that's playing keeps its own queue, and is offered the other.
        return if here.playing { Looked { offer, then: Then::Nothing } } else { Looked { offer: None, then: Then::TakeUp(place(theirs)) } };
    }
    let owed = here.current.is_some() && !here.taken_up && here.stamp > changed;
    Looked { offer, then: if owed { Then::Save } else { Then::Nothing } }
}

/// Whether to save this device's queue, given the server's saved queue as read immediately
/// before.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Save {
    /// It is the newest thing that happened: save it.
    Save,
    /// It isn't this device's to save, or another device's is as new or newer: leave the
    /// server's as it is. Nothing is owed.
    NotNewest,
    /// Nothing may be saved now (the clock is wrong). The save is still owed.
    NotNow,
}

pub fn save(theirs: &PlayQueue, here: &Here) -> Save {
    if clock_wrong(here.offset) {
        return Save::NotNow;
    }
    if here.taken_up || (others(theirs, here) && parse_time(&theirs.changed) >= here.stamp) {
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

    fn song(id: &str) -> Song {
        Song { id: id.into(), title: format!("T{id}"), duration: 200, ..Default::default() }
    }

    /// A queue saved by `device` at `at` seconds into the second of January 1970.
    fn saved(device: &str, at: i64, ids: &[&str], current: &str, position: u64) -> PlayQueue {
        let changed = format!("1970-01-02T00:{:02}:{:02}Z", at / 60, at % 60);
        PlayQueue { songs: ids.iter().map(|id| song(id)).collect(), current: current.into(), position, changed, changed_by: device.into() }
    }

    fn here(stamp: i64) -> Here<'static> {
        Here { device: ME, current: Some("mine"), playing: false, stamp: DAY + stamp, taken_up: false, offset: Some(0) }
    }

    #[test]
    fn a_queue_another_device_saved_since_is_not_saved_over() {
        // The person changed the queue here at 00:05:00, paused; in the three seconds before
        // its save goes, a PC saved its own at 00:05:02.
        assert_eq!(save(&saved(PC, 302, &["a", "b"], "a", 0), &here(300)), Save::NotNewest);
        // One the PC saved before the change here is older than it: this device's is saved.
        assert_eq!(save(&saved(PC, 299, &["a", "b"], "a", 0), &here(300)), Save::Save);
        // In the same second, the other device's stands: a tie goes to the one already there.
        assert_eq!(save(&saved(PC, 300, &["a", "b"], "a", 0), &here(300)), Save::NotNewest);
        // Its own, read back, is no reason not to save again; nor is nothing saved at all,
        // nor an empty queue under another's name.
        assert_eq!(save(&saved(ME, 302, &["a"], "a", 0), &here(300)), Save::Save);
        assert_eq!(save(&PlayQueue::default(), &here(300)), Save::Save);
        assert_eq!(save(&saved(PC, 302, &[], "", 0), &here(300)), Save::Save);
    }

    #[test]
    fn times_are_compared_on_the_servers_clock() {
        // A device two minutes fast changes its queue when its own clock says 00:07:00; the
        // server's says 00:05:00, and that is the time the app keeps for the change.
        let fast = Some(-120);
        let stamp = aligned(DAY + 420, fast);
        assert_eq!(stamp, DAY + 300);
        let device = Here { stamp, offset: fast, ..here(0) };
        // A PC saves two seconds later. Compared with the device's own clock its save would
        // look two minutes older than the change, and be saved over.
        assert_eq!(save(&saved(PC, 302, &["a"], "a", 0), &device), Save::NotNewest);
        // A device two minutes slow is not kept from saving what really is the newer.
        let slow = Some(120);
        let device = Here { stamp: aligned(DAY + 180, slow), offset: slow, ..here(0) };
        assert_eq!(save(&saved(PC, 299, &["a"], "a", 0), &device), Save::Save);
        // The half hour of "Continue here" is counted on the server's clock too.
        let playing = Here { playing: true, stamp: aligned(DAY + 2_300, fast), offset: fast, ..here(0) };
        assert_eq!(look(&saved(PC, 600, &["a", "b"], "b", 0), &playing, DAY + 600 + 1_799 + 120).offer, Some(1));
        assert_eq!(look(&saved(PC, 600, &["a", "b"], "b", 0), &playing, DAY + 600 + 1_800 + 120).offer, None);
    }

    #[test]
    fn nothing_is_saved_or_taken_up_while_the_clock_is_wrong() {
        let wrong = Here { offset: Some(-3_600), ..here(300) };
        assert_eq!(save(&saved(PC, 299, &["a"], "a", 0), &wrong), Save::NotNow);
        assert_eq!(save(&PlayQueue::default(), &wrong), Save::NotNow);
        assert_eq!(look(&saved(PC, 600, &["a", "b"], "b", 0), &wrong, DAY + 700), Looked { offer: None, then: Then::Nothing });
        // A server that gives no date can't be compared with: the clock is taken to be right.
        let unknown = Here { offset: None, ..here(300) };
        assert_eq!(save(&saved(PC, 299, &["a"], "a", 0), &unknown), Save::Save);
    }

    #[test]
    fn a_queue_taken_up_from_another_device_is_not_saved_as_this_ones_own() {
        // The PC's queue, saved at 00:10:00, was taken up here: the stamp is the PC's.
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        let taken = Here { current: Some("b"), stamp: place(&theirs).stamp, taken_up: true, ..here(0) };
        // A save asked for before it was taken up comes due: what is in the player is the
        // PC's queue, untouched, and it is not passed on as this device's, changed now.
        assert_eq!(save(&theirs, &taken), Save::NotNewest);
        assert_eq!(look(&theirs, &taken, DAY + 700).then, Then::Nothing);
        // Nor if the server's saved queue has gone since (cleared from another app): it
        // was never this device's, and nothing of it is owed.
        assert_eq!(save(&PlayQueue::default(), &taken), Save::NotNewest);
        assert_eq!(look(&PlayQueue::default(), &taken, DAY + 700).then, Then::Nothing);
        // Played on here, it is this device's to save.
        let played = Here { taken_up: false, stamp: DAY + 660, ..taken };
        assert_eq!(save(&theirs, &played), Save::Save);
    }

    #[test]
    fn the_saved_queue_is_taken_up_offered_or_left() {
        let theirs = saved(PC, 600, &["a", "b", "c"], "b", 30_000);
        // Not playing, and nothing done here since: taken up, paused, and nothing offered.
        let idle = here(100);
        assert_eq!(look(&theirs, &idle, DAY + 700), Looked { offer: None, then: Then::TakeUp(Place { cursor: 1, position: 30.0, stamp: DAY + 600 }) });
        // Playing something of its own: left alone, and offered.
        let busy = Here { playing: true, ..here(100) };
        assert_eq!(look(&theirs, &busy, DAY + 700), Looked { offer: Some(1), then: Then::Nothing });
        // Not offered the song it is on already, nor after half an hour, nor its own queue.
        assert_eq!(look(&theirs, &Here { current: Some("b"), ..busy }, DAY + 700).offer, None);
        assert_eq!(look(&theirs, &busy, DAY + 600 + OFFER_FOR).offer, None);
        assert_eq!(look(&saved(ME, 600, &["a", "b"], "b", 0), &busy, DAY + 700), Looked { offer: None, then: Then::Nothing });
        // Changed here after the PC saved, and paused: the PC's is neither taken up (this
        // one's is the newer) nor left to stand: this device's save is owed.
        assert_eq!(look(&theirs, &here(650), DAY + 700), Looked { offer: Some(1), then: Then::Save });
        // In the same second: not taken up, and not saved over.
        assert_eq!(look(&theirs, &here(600), DAY + 700).then, Then::Nothing);
        assert_eq!(save(&theirs, &here(600)), Save::NotNewest);
        // A song that had all but ended is picked up from its start; one the queue doesn't
        // name, at its first song.
        assert_eq!(place(&saved(PC, 600, &["a", "b"], "b", 199_500)), Place { cursor: 1, position: 0.0, stamp: DAY + 600 });
        assert_eq!(place(&saved(PC, 600, &["a", "b"], "zz", 5_000)).cursor, 0);
    }

    #[test]
    fn a_save_that_could_not_be_made_is_still_owed() {
        // Changed here at 00:05:00 with no network; the save was never made. Each look since
        // finds the server holding something older, this device's own or another's or
        // nothing: the save is owed.
        for theirs in [saved(ME, 200, &["old"], "old", 0), saved(PC, 250, &["a"], "a", 0), PlayQueue::default()] {
            assert_eq!(look(&theirs, &here(300), DAY + 400).then, Then::Save, "{}", theirs.changed_by);
        }
        // Once it is there (saved three seconds after the change), nothing is.
        assert_eq!(look(&saved(ME, 303, &["mine"], "mine", 0), &here(300), DAY + 400).then, Then::Nothing);
        // With nothing in the queue there is nothing to owe.
        assert_eq!(look(&PlayQueue::default(), &Here { current: None, ..here(300) }, DAY + 400).then, Then::Nothing);
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
