//! Keeping devices in step: what a device does at an exchange, decided here once.
//!
//! [`crate::sync`] is the record devices share and how two copies of it are combined. This
//! is everything around that which every app has to decide identically, because whatever
//! one device writes with a newer time wins on all of them:
//!
//! * **A change made on a device is kept as the change**, and becomes a value with a time on
//!   it only at an exchange, on top of what the server has then ([`Choices`], [`Change`],
//!   [`onto`]). Between devices each item (the pins, the theme, one list's choices) is a
//!   whole value and a time. A device that has not read what the others have (new, offline,
//!   its first read still on its way) holds none of it: if pinning one playlist there made
//!   "the pins" the list of that one, stamped now, every device's pins would become that one.
//! * **What the server did not say is not "empty"** ([`find`], [`settle`]).
//! * **A device writes only if it can write back everything it read** ([`crate::record`]).
//! * **The lowest-id playlist of that name that is the account's own**, looked for at every
//!   exchange ([`find`]).
//! * **Incoming plays are kept before anything is trimmed** ([`Plan::keep`]).
//! * **Nothing is shared while the device's clock is more than five minutes from the
//!   server's** ([`crate::time`], [`Held::ClockWrong`]).
//!
//! Nothing here asks the server anything, keeps a file or reads a clock. An app reads the
//! list of playlists, asks [`find`] which of them holds the record, reads that one, and
//! hands [`settle`] both answers as they were received, with what the device holds. What
//! comes back is everything the app must then do, in this order:
//!
//! 1. [`Outcome::Stop`]: nothing; say why in the log.
//! 2. [`Plan::keep`]: put these plays in the device's log, **on disk**. If that can't be
//!    done, stop here: nothing is taken in and nothing written.
//! 3. [`Plan::take`]: take in what other devices shared ([`Choices::take`] for the pins,
//!    the theme and the lists' choices; the notes and Recently played into the app's own
//!    stores).
//! 4. [`Plan::write`], if there is one: record that it is being tried
//!    ([`Choices::sending`], saved before the request is made); keep `found` if this is the
//!    first write this version makes; make the playlist if `into` is `None`; write `text`.
//! 5. Only once the server has answered that it took the write: [`Choices::written`], saved.
//!
//! **Accepted, and not solved here.** Two devices that read at the same moment both write,
//! and the second wins: Subsonic has no write that is conditional on what was read. Nothing
//! is lost for good by it, because each device still holds what it wrote as agreed, with its
//! time, and offers it again at its next exchange. And: if the answer to a write is lost
//! (the write was made, and this device never heard), and before this device next exchanges
//! another device deliberately sets the opposite, this device's change is made again on top
//! of that. Making a change again over its own result is harmless; over a later, opposite
//! choice it undoes that choice. Telling the two apart would need the server to say which
//! write a value came from.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::notes::Notes;
use crate::record::{self, Read};
use crate::sync::{LONG_AGO, ListPrefs, PLAYLIST_NAME, Shared, VERSION, merge};
use crate::time;

// ---- what a device holds of the choices that are shared -------------------------------------

/// The pins, the theme and each list's choices as a device last agreed them with the server,
/// each with the record's own time for it. This, and nothing else, is what a device offers
/// of them to the merge.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Agreed {
    pub pins: Vec<String>,
    pub pins_at: i64,
    pub theme: String,
    pub theme_at: i64,
    /// By route key, e.g. "playlist:abc".
    pub lists: HashMap<String, ListPrefs>,
}

impl Agreed {
    /// What a version that stamped whole values held, as this arrangement has it: each
    /// choice already made is agreed at the time it has, or "long ago" where it has none.
    /// Nothing is lost and nothing is given a new time. `usual_theme` is the theme nobody
    /// chose: with no time on it, it stays nobody's choice.
    pub fn from_stamped(pins: &[String], pins_at: i64, theme: &str, theme_at: i64, lists: &HashMap<String, ListPrefs>, usual_theme: &str) -> Agreed {
        Agreed {
            pins: pins.to_vec(),
            pins_at: if pins_at == 0 && !pins.is_empty() { LONG_AGO } else { pins_at },
            theme: theme.to_string(),
            theme_at: if theme_at == 0 && theme != usual_theme { LONG_AGO } else { theme_at },
            lists: lists.iter().map(|(key, prefs)| (key.clone(), ListPrefs { at: if prefs.at == 0 { LONG_AGO } else { prefs.at }, ..*prefs })).collect(),
        }
    }

    fn of(shared: &Shared) -> Agreed {
        Agreed { pins: shared.pins.clone(), pins_at: shared.pins_at, theme: shared.theme.clone(), theme_at: shared.theme_at, lists: shared.lists.clone() }
    }
}

/// Something the person changed on this device, as the change itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Change {
    /// Pin a playlist (at the end of the pins).
    Pin(String),
    Unpin(String),
    /// Whether the list `key` plays shuffled.
    Shuffle { key: String, on: bool },
    /// The order the list `key` is shown in, chosen by the person.
    Sort { key: String, sort: usize, descending: bool },
    /// The list `key` is back to the usual choices (it has gone: a playlist deleted).
    Usual { key: String },
    Theme(String),
}

/// What a change is about. A later change about the same thing is the last word on it.
#[derive(PartialEq)]
enum About<'a> {
    Pin(&'a str),
    Shuffle(&'a str),
    Sort(&'a str),
    Theme,
}

impl Change {
    fn about(&self) -> Vec<About<'_>> {
        match self {
            Change::Pin(id) | Change::Unpin(id) => vec![About::Pin(id)],
            Change::Shuffle { key, .. } => vec![About::Shuffle(key)],
            Change::Sort { key, .. } => vec![About::Sort(key)],
            Change::Usual { key } => vec![About::Shuffle(key), About::Sort(key)],
            Change::Theme(_) => vec![About::Theme],
        }
    }

    /// Makes the change to the pins.
    fn pins(&self, pins: &mut Vec<String>) {
        match self {
            Change::Pin(id) if !pins.contains(id) => pins.push(id.clone()),
            Change::Unpin(id) => pins.retain(|pin| pin != id),
            _ => {}
        }
    }

    /// The list this change is to, and its choices with the change made (the time left as
    /// it was).
    fn list(&self, base: impl Fn(&str) -> ListPrefs) -> Option<(&str, ListPrefs)> {
        match self {
            Change::Shuffle { key, on } => Some((key, ListPrefs { shuffle: *on, ..base(key) })),
            Change::Sort { key, sort, descending } => Some((key, ListPrefs { sort: *sort, descending: *descending, sort_chosen: true, ..base(key) })),
            Change::Usual { key } => Some((key, ListPrefs { at: base(key).at, ..Default::default() })),
            _ => None,
        }
    }
}

/// A change that is waiting to be shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Waiting {
    /// Which change this is, among all this device has ever made.
    pub id: u64,
    pub change: Change,
    /// A write that carried it has been sent, and it isn't yet known that the server took
    /// it. Until it is, the change stays: it will be made again at the next exchange.
    #[serde(default)]
    pub tried: bool,
}

/// What a device shows: what was agreed, with the waiting changes on top.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shown {
    pub pins: Vec<String>,
    /// Empty when nothing was agreed and no theme is waiting: the app shows its own.
    pub theme: String,
    pub lists: HashMap<String, ListPrefs>,
}

/// What a device holds of the shared choices: what was last agreed with the server, and the
/// changes the person has made here since. An app keeps this with its settings, saved
/// **before** a change is shown as made, and shows [`Choices::shown`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Choices {
    pub agreed: Agreed,
    pub waiting: Vec<Waiting>,
    /// The number the next change will be given.
    pub next: u64,
}

fn same_choices(a: &ListPrefs, b: &ListPrefs) -> bool {
    (a.shuffle, a.sort, a.descending, a.sort_chosen) == (b.shuffle, b.sort, b.descending, b.sort_chosen)
}

impl Choices {
    /// The person changed something. The change waits; an earlier one about the same thing
    /// that no write has carried yet gives way to it; and one that only puts back what was
    /// there before anything was sent is no change at all, and nothing waits. (Pinned and
    /// unpinned again before either was shared, the playlist must not be unpinned on the
    /// devices where it was pinned all along.)
    ///
    /// Returns whether anything now shown differs: if not, there is nothing to save or draw.
    pub fn change(&mut self, change: Change) -> bool {
        let before = self.shown();
        let about = change.about();
        self.waiting.retain(|waits| waits.tried || !waits.change.about().iter().all(|thing| about.contains(thing)));
        let without = self.shown();
        self.waiting.push(Waiting { id: self.next, change: change.clone(), tried: false });
        let tried = self.waiting.iter().any(|waits| waits.tried && waits.change.about().iter().any(|thing| about.contains(thing)));
        if !tried && self.shown() == without {
            self.waiting.pop();
        } else {
            self.next += 1;
        }
        self.shown() != before
    }

    /// What the device shows.
    pub fn shown(&self) -> Shown {
        let record = made(&Shared { pins: self.agreed.pins.clone(), theme: self.agreed.theme.clone(), lists: self.agreed.lists.clone(), ..Default::default() }, &self.waiting);
        Shown { pins: record.pins, theme: record.theme, lists: record.lists }
    }

    /// Step 3: what other devices shared is taken in. Changes that turned out to alter
    /// nothing in what the server has are done with.
    pub fn take(&mut self, take: &Take) {
        self.agreed = take.agreed.clone();
        self.waiting.retain(|waits| !take.done.contains(&waits.id));
    }

    /// Step 4, before the request is made: these changes are in a write whose answer isn't
    /// in yet.
    pub fn sending(&mut self, write: &Write) {
        for waits in self.waiting.iter_mut().filter(|waits| write.carried.contains(&waits.id)) {
            waits.tried = true;
        }
    }

    /// Step 5, once the server has answered that it took the write: what it carried is
    /// agreed, and those changes are done with. One made since still waits.
    pub fn written(&mut self, write: &Write) {
        self.agreed = write.agreed.clone();
        self.waiting.retain(|waits| !write.carried.contains(&waits.id));
    }

    /// A theme another device chose, found by a look before the window opens (nothing is
    /// written by such a look). Taken as agreed if it is newer than what was.
    pub fn theme_seen(&mut self, theme: &str, at: i64) -> bool {
        let newer = (at, theme) > (self.agreed.theme_at, self.agreed.theme.as_str());
        if newer {
            self.agreed.theme = theme.to_string();
            self.agreed.theme_at = at;
        }
        newer
    }
}

/// The record with these changes made to it, and no time put on anything.
fn made(record: &Shared, waiting: &[Waiting]) -> Shared {
    let mut out = record.clone();
    for waits in waiting {
        waits.change.pins(&mut out.pins);
        if let Some((key, prefs)) = waits.change.list(|key| out.lists.get(key).copied().unwrap_or_default()) {
            // (A list nobody has chosen anything for, put "back to the usual", stays out.)
            if out.lists.contains_key(key) || !same_choices(&prefs, &ListPrefs::default()) {
                out.lists.insert(key.to_string(), prefs);
            }
        }
        if let Change::Theme(name) = &waits.change {
            out.theme = name.clone();
        }
    }
    out
}

/// A time for an item changed at `now`: after the time of what it was changed from, whatever
/// clock put that on, so the change is the newer on every device.
fn after(at: i64, now: i64) -> i64 {
    now.max(at + 1)
}

/// The record with the waiting changes made to it: each on top of the item **as the record
/// has it** (the server's, merged with what this device agreed before), and only an item a
/// change really altered given a time. A change that makes no difference to what is there
/// (pinning what is pinned, the shuffle a list already has) alters nothing and puts a time on
/// nothing.
pub fn onto(record: &Shared, waiting: &[Waiting], now: i64) -> Shared {
    let mut out = made(record, waiting);
    if out.pins != record.pins {
        out.pins_at = after(record.pins_at, now);
    }
    if out.theme != record.theme {
        out.theme_at = after(record.theme_at, now);
    }
    for (key, prefs) in out.lists.iter_mut() {
        let was = record.lists.get(key).copied().unwrap_or_default();
        if !same_choices(prefs, &was) {
            prefs.at = after(was.at, now);
        }
    }
    out
}

// ---- what the server said ---------------------------------------------------------------------

/// A playlist as the server stated it, for the one use where "the server didn't say" must
/// not be taken for "there is none". What an answer left out is `None`.
///
/// Navidrome leaves a playlist's owner and comment out of its answers for a client it has
/// been told to say little to, and leaves an empty comment out of every answer.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Stated {
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    pub comment: Option<String>,
}

impl Stated {
    fn ours(&self, username: &str) -> bool {
        self.owner.as_ref().is_some_and(|owner| owner.to_lowercase() == username.to_lowercase())
    }
}

/// Which of the playlists the server listed holds the record.
#[derive(Debug, Clone, PartialEq)]
pub enum Found {
    /// This one: read it, and hand its answer to [`settle`].
    Ours(String),
    /// There is none. [`settle`] says whether one is to be made.
    None,
    /// There is one of that name, and the server did not say whose it is.
    NotStated,
}

/// The playlist the record is in: the one with the lowest id (compared as bytes) among those
/// of that name that are the account's own. **Looked for afresh at every exchange**: a device
/// that went on using an id it remembered, after a lower one appeared (two devices that each
/// made one, having found none), would never meet the devices that use the lower one again.
///
/// A playlist is the account's own only if the server says so. One whose owner is not
/// stated may be anybody's: with none known to be ours, nothing is made beside it and
/// nothing written into it.
pub fn find(listed: &[Stated], username: &str) -> Found {
    let named = || listed.iter().filter(|playlist| !playlist.id.is_empty() && playlist.name == PLAYLIST_NAME);
    match named().filter(|playlist| playlist.ours(username)).map(|playlist| &playlist.id).min() {
        Some(id) => Found::Ours(id.clone()),
        None if named().any(|playlist| playlist.owner.is_none()) => Found::NotStated,
        None => Found::None,
    }
}

/// Whether a playlist of the person's own may have this name: not the record's.
pub fn reserved(name: &str) -> bool {
    name.trim() == PLAYLIST_NAME
}

/// What the app says when a playlist of the person's own is given the record's name.
pub const RESERVED: &str = "Nexus Music keeps that name for itself, for keeping your devices in step. Choose another.";

// ---- an exchange ------------------------------------------------------------------------------

/// What this device holds, as an exchange begins.
#[derive(Debug, Clone, Default)]
pub struct Device<'a> {
    /// The account's name, as the device signs in with it.
    pub username: &'a str,
    pub choices: Choices,
    /// The dated notes of when songs were added: the person's own changes on this device,
    /// and what it has taken in from others ([`crate::notes::Kept::shared`]).
    pub notes: Notes,
    /// Recently played: song id and when, newest first.
    pub history: Vec<(String, i64)>,
    /// The latest plays in this device's log, and where those made from a playlist were
    /// made from.
    pub plays: Vec<(String, i64)>,
    pub play_sources: Vec<(String, i64, String)>,
}

/// The server's answers, as they were received.
#[derive(Debug, Clone, Default)]
pub struct Answers<'a> {
    /// The list of playlists.
    pub listed: &'a [Stated],
    /// The answer for the playlist [`find`] named, if it named one.
    pub playlist: Option<&'a Stated>,
    /// The `Date` on the later of those answers, if it had one ([`time::http_date`]), and
    /// what the device's clock said as it arrived.
    pub date: Option<i64>,
    pub received: i64,
}

/// Why an exchange stopped with nothing read into the device and nothing written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stop {
    /// The server didn't say whose the playlist is.
    OwnerNotStated,
    /// The list of playlists states a comment for the playlist and its own answer has none.
    Disagree,
    /// The playlist's own answer wasn't handed over, or is for another playlist, or it is no
    /// longer the record's (renamed, or another's now): look again.
    Moved,
    /// What is in the comment isn't a record.
    NotARecord,
}

impl Stop {
    /// For the log, or to show.
    pub fn words(self) -> &'static str {
        match self {
            Stop::OwnerNotStated => "The server didn't say whose the playlist “Nexus Music sync” is, so nothing was read from it and nothing written.",
            Stop::Disagree => "The server's two answers about the playlist “Nexus Music sync” disagree about what is in it, so nothing was read from it and nothing written.",
            Stop::Moved => "The playlist “Nexus Music sync” changed as it was being read. It will be looked at again.",
            Stop::NotARecord => "The playlist “Nexus Music sync” holds something that isn't the shared record, so it was left as it is. Empty its comment to start the record again.",
        }
    }
}

/// Why what the server has was taken in and nothing written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Held {
    /// This device's clock is more than five minutes from the server's.
    ClockWrong,
    /// The record was written by a later version of the app.
    LaterLayout,
    /// The record holds something this version couldn't write back as it found it.
    MoreThanKnown,
}

impl Held {
    /// For the log, or to show.
    pub fn words(self) -> &'static str {
        match self {
            Held::ClockWrong => "This device's clock is wrong, so its changes aren't being shared.",
            Held::LaterLayout => "The shared record was written by a newer version of the app, so this one reads it and doesn't change it.",
            Held::MoreThanKnown => "The shared record holds more than this version knows, so this one reads it and doesn't change it.",
        }
    }
}

/// Plays to put in the device's log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plays {
    pub plays: Vec<(String, i64)>,
    pub sources: Vec<(String, i64, String)>,
}

/// What other devices shared, for this one to take in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Take {
    /// What is now agreed ([`Choices::take`]).
    pub agreed: Agreed,
    /// Changes that altered nothing in what the server has, and are done with.
    pub done: Vec<u64>,
    /// The server's notes of when songs were added ([`crate::notes::Kept::take`]).
    pub notes: Notes,
    /// Recently played, this device's and the others' together, newest first.
    pub history: Vec<(String, i64)>,
}

/// A write to make.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Write {
    /// The playlist to write into; `None` to make one of the record's name first.
    pub into: Option<String>,
    pub text: String,
    /// The comment as it was found, to keep before the first write a version makes.
    pub found: String,
    /// The changes this write carries ([`Choices::sending`], [`Choices::written`]).
    pub carried: Vec<u64>,
    /// What is agreed once the server has taken it.
    pub agreed: Agreed,
}

/// Everything to do after an exchange's reads, in order (see the top of this file).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// The playlist the record is in, if there is one.
    pub playlist: Option<String>,
    /// The server's clock minus this device's ([`time::offset`]).
    pub offset: Option<i64>,
    pub keep: Plays,
    pub take: Take,
    pub write: Option<Write>,
    /// Set when there is something the device would have written and may not.
    pub held: Option<Held>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Stop(Stop),
    Go(Box<Plan>),
}

/// Whether a record says anything of the person's: a device with nothing to say makes no
/// record on the server, and fills no empty one.
pub fn says(record: &Shared) -> bool {
    !record.pins.is_empty()
        || record.pins_at != 0
        || record.theme_at != 0
        || !record.lists.is_empty()
        || !record.added.is_empty()
        || !record.history.is_empty()
        || !record.plays.is_empty()
        || !record.play_sources.is_empty()
}

/// Decides an exchange: what to keep, take in and write, given what the device holds and
/// what the server answered.
pub fn settle(device: &Device, answers: &Answers) -> Outcome {
    // Which playlist, and what is in it: nothing the server didn't say is made up.
    let (playlist, comment) = match find(answers.listed, device.username) {
        Found::NotStated => return Outcome::Stop(Stop::OwnerNotStated),
        Found::None => (None, ""),
        Found::Ours(id) => {
            let Some(answer) = answers.playlist.filter(|answer| answer.id == id && answer.name == PLAYLIST_NAME) else { return Outcome::Stop(Stop::Moved) };
            if answer.owner.is_none() {
                return Outcome::Stop(Stop::OwnerNotStated);
            }
            if !answer.ours(device.username) {
                return Outcome::Stop(Stop::Moved);
            }
            let comment = match &answer.comment {
                Some(comment) => comment.as_str(),
                // Navidrome leaves an empty comment out of every answer, so its absence alone
                // says nothing; an answer that says little altogether leaves the owner out
                // too, and has stopped above. But if the list of playlists states a comment
                // for this playlist, there is one, and this answer is not saying.
                None if answers.listed.iter().any(|listed| listed.id == id && listed.comment.as_ref().is_some_and(|comment| !comment.is_empty())) => {
                    return Outcome::Stop(Stop::Disagree);
                }
                None => "",
            };
            (Some(id), comment)
        }
    };
    let offset = time::offset(answers.date, answers.received);
    // Times are put on things, and compared, by the server's clock where it is known.
    let now = time::aligned(answers.received, offset);
    let (theirs, unknown, mut held) = match record::read(comment) {
        Read::Unreadable => return Outcome::Stop(Stop::NotARecord),
        Read::Empty => (Shared::default(), Vec::new(), None),
        Read::Later { shared, .. } => (shared.unwrap_or_default(), Vec::new(), Some(Held::LaterLayout)),
        Read::Record(record) => {
            let held = (!record.whole).then_some(Held::MoreThanKnown);
            (record.shared, record.unknown, held)
        }
    };
    if time::clock_wrong(offset) {
        held = Some(Held::ClockWrong);
    }
    let agreed = &device.choices.agreed;
    let mine = Shared {
        nexus_music_sync: VERSION,
        pins: agreed.pins.clone(),
        pins_at: agreed.pins_at,
        theme: agreed.theme.clone(),
        theme_at: agreed.theme_at,
        lists: agreed.lists.clone(),
        added: device.notes.clone(),
        history: device.history.clone(),
        plays: device.plays.clone(),
        play_sources: device.play_sources.clone(),
    };
    let merged = merge(&mine, &theirs, now);
    // What the person changed here is made to the record now, on top of what the server
    // has: never to what the device happened to hold.
    let record = onto(&merged, &device.choices.waiting, now);
    let waiting: Vec<u64> = device.choices.waiting.iter().map(|waits| waits.id).collect();
    let adds = record != theirs && (!comment.is_empty() || says(&record));
    let write = (adds && held.is_none()).then(|| Write {
        into: playlist.clone(),
        text: record::write(&record, &unknown),
        found: comment.to_string(),
        carried: waiting.clone(),
        agreed: Agreed::of(&record),
    });
    Outcome::Go(Box::new(Plan {
        playlist,
        offset,
        // The server's plays as they came, all of them: what is written back is cut to the
        // latest, and a play that falls outside that has to be in this device's log first.
        keep: Plays { plays: theirs.plays.clone(), sources: theirs.play_sources.clone() },
        take: Take {
            agreed: Agreed::of(&merged),
            // With nothing to write, a change that waited has been found to alter nothing.
            // (Held back, it still waits.)
            done: if write.is_none() && held.is_none() && record == merged { waiting } else { Vec::new() },
            notes: theirs.added.clone(),
            history: merged.history.clone(),
        },
        // Nothing to say, and nothing it would have written: not held back from anything.
        held: held.filter(|held| *held == Held::ClockWrong || adds),
        write,
    }))
}
