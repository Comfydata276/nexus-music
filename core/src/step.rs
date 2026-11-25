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
//! Nothing here asks the server anything, keeps a file or reads a clock. What an app must do
//! around it is this, and the order matters as much as the steps: every fault found in an
//! app's side of an exchange so far has been something taken, checked or saved at the wrong
//! moment.
//!
//! **The contract.** One exchange at a time for a sign-in, from step 1 to its last step. If
//! the sign-in changes (signed out, another account) or the setting goes off part-way,
//! whatever arrives afterwards is dropped: nothing of it is applied.
//!
//! 1. **Ask the server.** Read the list of playlists; ask [`find`] which holds the record;
//!    read that one. Nothing of the device's state is needed for this, **and none may be
//!    taken yet**: what the person does while these answers are on their way must be in
//!    what is decided.
//! 2. **With both answers in hand**, on the thread that owns the device's state, and with
//!    no waiting between here and step 6: confirm the sign-in is the one that asked and the
//!    setting is on. Take [`clock`] from the answers and remember it (it is good whatever
//!    happens next: the queue needs it too, see [`crate::handover`]).
//! 3. **Decide.** Make a [`Device`] from what the device holds *now* (its choices, with the
//!    account signed in; its dated notes; Recently played; its newest plays,
//!    [`crate::sync::newest_plays`]) and call [`settle`].
//! 4. [`Outcome::Stop`]: say why ([`Stop::words`]) and do nothing else.
//! 5. **Keep.** Put [`Plan::keep`] in the device's log of plays, **on disk, read back as
//!    whole lines**, together with every play the device has counted and not yet written.
//!    If that can't be done, stop here: nothing is taken in and nothing written.
//! 6. **Take in.** [`Choices::take`], saved; the notes ([`crate::notes::Kept::take`]);
//!    Recently played, which is [`history`] of what the device has at that moment and what
//!    the plan brought (not the plan's alone: a song started since step 3 stays). Say
//!    [`Plan::held`], if it is set, where the person will see it.
//! 7. **Write**, if [`Plan::write`] is set. In this order:
//!    1. Keep the record's text as it was found, once for each account and version of the
//!       app: a file named for both, holding [`Write::found`] exactly (the empty text too:
//!       an empty file). The file is its own proof. If it is there, leave it, and never
//!       open it for writing; if it is not, make it, only if it is still not there, and
//!       flush it. If it can't be made, send nothing.
//!    2. [`Choices::sending`], which says whether the plan still fits the choices it was
//!       made from. If it says no (a change the write carries was taken back meanwhile),
//!       send nothing and start again from step 1. Save the choices, surely, before the
//!       request is made.
//!    3. Confirm once more that the sign-in is the same and the setting on, then make the
//!       playlist if [`Write::into`] is `None`, and write [`Write::text`], with the
//!       sign-in's own connection.
//!    4. Only once the server has answered that it took the write: [`Choices::written`],
//!       saved. If no answer comes, do nothing: the changes stay as tried, and the next
//!       exchange makes them again. (A change the person makes on top of them meanwhile is
//!       kept for [`UNANSWERED`] after the write was sent, even if the two come to nothing:
//!       the write may yet arrive.)
//!
//! A change the person makes ([`Choices::change`]) is saved, surely, **before** it is shown
//! as made, and is made under the account signed in: it is only ever sent for that account.
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
use crate::sync::{LONG_AGO, ListPrefs, PLAYLIST_NAME, Shared, VERSION, merge, merge_history};
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

/// How long after a write was sent and never answered it is taken that it may still arrive.
/// The app waits 45 seconds for any answer; something between it and the server could hold
/// a request longer and deliver it all the same. Ten minutes is far past either.
pub const UNANSWERED: i64 = 10 * 60;

/// A change that is waiting to be shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Waiting {
    /// Which change this is, among all this device has ever made.
    pub id: u64,
    /// The account it was made under: **the server's address and the username, each exactly
    /// as signed in with**, in one text (the caller's to put together, never shortened or
    /// made "safe": two accounts must never come out the same). It is compared as text. The
    /// change is only ever sent for that account; under any other it waits, untouched.
    #[serde(default)]
    pub account: String,
    pub change: Change,
    /// A write that carried it has been sent, and it isn't yet known that the server took
    /// it. Until it is, the change stays: it will be made again at the next exchange.
    #[serde(default)]
    pub tried: bool,
    /// When that write was made ([`Write::at`]); 0 when none has been.
    #[serde(default)]
    pub tried_at: i64,
}

impl Waiting {
    /// A write that carried this was sent, has not been answered, and may still arrive.
    fn on_its_way(&self, now: i64) -> bool {
        self.tried && now.saturating_sub(self.tried_at) < UNANSWERED
    }
}

/// What a device shows: what was agreed, with the waiting changes on top.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// The changes waiting under this account, in the order they were made.
    fn waiting_for<'a>(&'a self, account: &'a str) -> impl Iterator<Item = &'a Waiting> {
        self.waiting.iter().filter(move |waits| waits.account == account)
    }

    /// The person, signed in as `account`, changed something. The change waits; an earlier
    /// one about the same thing that no write has carried yet gives way to it; and one that
    /// only puts back what was there before anything was sent is no change at all, and
    /// nothing waits. (Pinned and unpinned again before either was shared, the playlist
    /// must not be unpinned on the devices where it was pinned all along.)
    ///
    /// Returns whether anything now shown differs: if not, there is nothing to draw.
    pub fn change(&mut self, account: &str, change: Change) -> bool {
        let before = self.shown(account);
        let about = change.about();
        self.waiting.retain(|waits| waits.account != account || waits.tried || !waits.change.about().iter().all(|thing| about.contains(thing)));
        let without = self.shown(account);
        self.waiting.push(Waiting { id: self.next, account: account.to_string(), change: change.clone(), tried: false, tried_at: 0 });
        let tried = self.waiting_for(account).any(|waits| waits.tried && waits.change.about().iter().any(|thing| about.contains(thing)));
        if !tried && self.shown(account) == without {
            self.waiting.pop();
        } else {
            self.next = self.next.saturating_add(1);
        }
        self.shown(account) != before
    }

    /// What the device shows while signed in as `account`: what was agreed, with that
    /// account's waiting changes on top.
    pub fn shown(&self, account: &str) -> Shown {
        let waiting: Vec<&Waiting> = self.waiting_for(account).collect();
        let record = made(&Shared { pins: self.agreed.pins.clone(), theme: self.agreed.theme.clone(), lists: self.agreed.lists.clone(), ..Default::default() }, &waiting);
        Shown { pins: record.pins, theme: record.theme, lists: record.lists }
    }

    /// Step 6: what other devices shared is taken in. Changes that turned out to alter
    /// nothing in what the server has are done with.
    pub fn take(&mut self, take: &Take) {
        self.agreed = take.agreed.clone();
        self.waiting.retain(|waits| !take.done.contains(&waits.id));
    }

    /// Step 7.2, before the request is made: these changes are in a write whose answer
    /// isn't in yet. **Says whether the write may be sent**: a plan is only good for the
    /// choices it was made from, and if a change it carries no longer waits (the person
    /// took it back while the plan was being made or acted on), sending it would publish
    /// what was undone, with nothing left to undo it. Then nothing is marked, and the
    /// exchange is to start again.
    #[must_use = "a write that no longer fits the choices must not be sent"]
    pub fn sending(&mut self, write: &Write) -> bool {
        if !write.carried.iter().all(|id| self.waiting.iter().any(|waits| waits.id == *id)) {
            return false;
        }
        for waits in self.waiting.iter_mut().filter(|waits| write.carried.contains(&waits.id)) {
            waits.tried = true;
            waits.tried_at = write.at;
        }
        true
    }

    /// Step 7.4, once the server has answered that it took the write: what it carried is
    /// agreed, and those changes are done with. One made since still waits.
    pub fn written(&mut self, write: &Write) {
        self.agreed = write.agreed.clone();
        self.waiting.retain(|waits| !write.carried.contains(&waits.id));
    }
}

/// The record with these changes made to it, and no time put on anything.
fn made(record: &Shared, waiting: &[&Waiting]) -> Shared {
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
    now.max(at.saturating_add(1))
}

/// The record with the waiting changes made to it: each on top of the item **as the record
/// has it** (the server's, merged with what this device agreed before), and only an item a
/// change really altered given a time. A change that makes no difference to what is there
/// (pinning what is pinned, the shuffle a list already has) alters nothing and puts a time on
/// nothing.
pub fn onto(record: &Shared, waiting: &[&Waiting], now: i64) -> Shared {
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    find_named(listed, username, PLAYLIST_NAME)
}

/// The same rule for a playlist of another name the app keeps for itself (the secret the
/// account's devices share is found this way: see `link::secret`).
pub fn find_named(listed: &[Stated], username: &str, name: &str) -> Found {
    let named = || listed.iter().filter(|playlist| !playlist.id.is_empty() && playlist.name == name);
    match named().filter(|playlist| playlist.ours(username)).map(|playlist| &playlist.id).min() {
        Some(id) => Found::Ours(id.clone()),
        None if named().any(|playlist| playlist.owner.is_none()) => Found::NotStated,
        None => Found::None,
    }
}

/// Whether a playlist as the server stated it is this account's own.
pub fn ours(playlist: &Stated, username: &str) -> bool {
    playlist.ours(username)
}

/// Whether a playlist of the person's own may have this name: not the record's. (Another
/// feature that keeps a playlist to itself keeps its own name, with its own words: see
/// `link::secret::reserved`.)
pub fn reserved(name: &str) -> bool {
    name.trim() == PLAYLIST_NAME
}

/// What the app says when a playlist of the person's own is given the record's name.
pub const RESERVED: &str = "Nexus Music keeps that name for itself, for keeping your devices in step. Choose another.";

// ---- an exchange ------------------------------------------------------------------------------

/// What this device holds, **at the moment the server's answers are in hand** (step 3).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Device {
    /// The account's name, as the device signs in with it.
    pub username: String,
    /// This sign-in, exactly (see [`Waiting::account`]): the waiting changes made under it
    /// are the ones this exchange may carry.
    pub account: String,
    pub choices: Choices,
    /// The dated notes of when songs were added: the person's own changes on this device,
    /// and what it has taken in from others ([`crate::notes::Kept::shared`]).
    pub notes: Notes,
    /// Recently played: song id and when, newest first.
    pub history: Vec<(String, i64)>,
    /// The newest plays in this device's log ([`crate::sync::newest_plays`]), and where
    /// those made from a playlist were made from.
    pub plays: Vec<(String, i64)>,
    pub play_sources: Vec<(String, i64, String)>,
}

/// The server's answers, as they were received.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Answers {
    /// The list of playlists.
    pub listed: Vec<Stated>,
    /// The answer for the playlist [`find`] named, if it named one.
    pub playlist: Option<Stated>,
    /// The `Date` on the later of those answers, if it had one ([`time::http_date`]), and
    /// what the device's clock said as it arrived.
    pub date: Option<i64>,
    pub received: i64,
}

/// The device's clock beside the server's, by these answers (step 2). It doesn't depend on
/// what the playlist holds: an exchange that then stops has still measured it.
pub fn clock(answers: &Answers) -> time::Clock {
    time::Clock::measured(answers.date, answers.received)
}

/// Why an exchange stopped with nothing read into the device and nothing written.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Stop {
    /// The server didn't say whose the playlist is.
    OwnerNotStated,
    /// The server's two answers disagree about the playlist's comment: one states it and
    /// the other doesn't, or they state different ones.
    Disagree,
    /// The playlist's own answer wasn't handed over, or is for another playlist, or it is no
    /// longer the record's (renamed, or another's now): look again.
    Moved,
    /// What is in the comment isn't a record.
    NotARecord,
    /// The comment, or something in the record or on the device, is larger than anything
    /// the app would have written (see [`limits`]).
    TooLarge,
    /// A time in the record, or on the device, is outside any the app would have written.
    TimeOutOfRange,
}

impl Stop {
    /// For the log, or to show.
    pub fn words(self) -> &'static str {
        match self {
            Stop::OwnerNotStated => "The server didn't say whose the playlist “Nexus Music sync” is, so nothing was read from it and nothing written.",
            Stop::Disagree => "The server's two answers about the playlist “Nexus Music sync” disagree about what is in it, so nothing was read from it and nothing written. It will be looked at again.",
            Stop::Moved => "The playlist “Nexus Music sync” changed as it was being read. It will be looked at again.",
            Stop::NotARecord => "The playlist “Nexus Music sync” holds something that isn't the shared record, so it was left as it is. Empty its comment to start the record again.",
            Stop::TooLarge => "The shared record is larger than anything Nexus Music would have written, so it was left as it is.",
            Stop::TimeOutOfRange => "A time in the shared record is out of range, so nothing was read from it and nothing written.",
        }
    }
}

/// Why what the server has was taken in and nothing written.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Held {
    /// This device's clock is more than five minutes from the server's.
    ClockWrong,
    /// The record was written by a later version of the app.
    LaterLayout,
    /// The record holds something this version couldn't write back as it found it.
    MoreThanKnown,
    /// What this device would write is larger than a record that is read (see [`limits`]).
    TooLarge,
    /// What this device would write has a time in it outside any that is read.
    TimeOutOfRange,
}

impl Held {
    /// For the log, or to show.
    pub fn words(self) -> &'static str {
        match self {
            Held::ClockWrong => "This device's clock is wrong, so its changes aren't being shared.",
            Held::LaterLayout => "The shared record was written by a newer version of the app, so this one reads it and doesn't change it.",
            Held::MoreThanKnown => "The shared record holds more than this version knows, so this one reads it and doesn't change it.",
            Held::TooLarge => "What this device has to add would make the shared record larger than Nexus Music reads, so it hasn't been written.",
            Held::TimeOutOfRange => "What this device has to add would put a time in the shared record that is out of range, so it hasn't been written.",
        }
    }
}

/// Plays to put in the device's log.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Plays {
    pub plays: Vec<(String, i64)>,
    pub sources: Vec<(String, i64, String)>,
}

/// What other devices shared, for this one to take in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Take {
    /// What is now agreed ([`Choices::take`]).
    pub agreed: Agreed,
    /// Changes that altered nothing in what the server has, and are done with.
    pub done: Vec<u64>,
    /// The server's notes of when songs were added ([`crate::notes::Kept::take`]).
    pub notes: Notes,
    /// Recently played **as the server's record has it**: to be put together with what the
    /// device has when it is taken in ([`history`]), not put in its place.
    pub history: Vec<(String, i64)>,
}

/// Recently played, once a plan is taken in (step 6): what the device has at that moment
/// and what the plan brought, newest first. A song that started playing after the plan was
/// made is in `mine`, and stays.
pub fn history(mine: &[(String, i64)], take: &Take) -> Vec<(String, i64)> {
    merge_history(mine, &take.history)
}

/// A write to make. **Good only for the choices it was made from**: [`Choices::sending`]
/// says, at the moment of sending, whether it still is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Write {
    /// The playlist to write into; `None` to make one of the record's name first.
    pub into: Option<String>,
    pub text: String,
    /// The comment as it was found, to keep before the first write a version makes (step
    /// 7.1). It may be the empty text: that is kept too.
    pub found: String,
    /// The changes this write carries ([`Choices::sending`], [`Choices::written`]).
    pub carried: Vec<u64>,
    /// When it was decided, by the server's clock where that is known.
    #[serde(default)]
    pub at: i64,
    /// What is agreed once the server has taken it.
    pub agreed: Agreed,
}

/// Everything to do after an exchange's reads, in order (see the top of this file).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// The most that is taken in, of each thing. Each is far more than any version of the app
/// writes (the record it writes is a few tens of thousands of characters: 100 songs of
/// history, 500 plays, a choice or two for each list the person has ever sorted), and is
/// there so that a comment nobody meant, or a device's own files gone wrong, is stopped at
/// rather than worked through.
pub mod limits {
    /// Characters in the comment.
    pub const TEXT: usize = 4 * 1024 * 1024;
    /// Pinned playlists.
    pub const PINS: usize = 10_000;
    /// Lists with choices of their own.
    pub const LISTS: usize = 100_000;
    /// Playlists with notes, and notes (songs) in all of them together.
    pub const NOTED: usize = 500_000;
    /// Copies of one song in one playlist.
    pub const COPIES: usize = 10_000;
    /// Entries of Recently played, and plays.
    pub const HISTORY: usize = 10_000;
    pub const PLAYS: usize = 100_000;
    /// Characters in one id, name or key.
    pub const NAME: usize = 4_096;
    /// The latest time anything may carry: the first second of the year 3000. Nothing is
    /// earlier than 0.
    pub const TIME: i64 = 32_503_680_000;
}

fn in_range(time: i64) -> bool {
    (0..=limits::TIME).contains(&time)
}

/// Why a record (the server's, or what the device offers) is not one to work with.
fn unfit(record: &Shared) -> Option<Stop> {
    let notes: usize = record.added.values().map(|notes| notes.len()).sum();
    let names = record
        .pins
        .iter()
        .chain(record.lists.keys())
        .chain(record.added.keys())
        .chain(record.added.values().flat_map(|notes| notes.keys()))
        .chain(record.history.iter().map(|(id, _)| id))
        .chain(record.plays.iter().map(|(id, _)| id))
        .chain(record.play_sources.iter().flat_map(|(id, _, from)| [id, from]))
        .chain([&record.theme]);
    let large = record.pins.len() > limits::PINS
        || record.lists.len() > limits::LISTS
        || record.added.len() > limits::NOTED
        || notes > limits::NOTED
        || record.added.values().flat_map(|notes| notes.values()).any(|note| note.times.len() > limits::COPIES)
        || record.history.len() > limits::HISTORY
        || record.plays.len() > limits::PLAYS
        || record.play_sources.len() > limits::PLAYS
        || names.into_iter().any(|name| name.len() > limits::NAME);
    if large {
        return Some(Stop::TooLarge);
    }
    let times = [record.pins_at, record.theme_at]
        .into_iter()
        .chain(record.lists.values().map(|prefs| prefs.at))
        .chain(record.added.values().flat_map(|notes| notes.values()).flat_map(|note| note.times.iter().copied().chain([note.at])))
        .chain(record.history.iter().map(|(_, at)| *at))
        .chain(record.plays.iter().map(|(_, at)| *at))
        .chain(record.play_sources.iter().map(|(_, at, _)| *at));
    times.into_iter().any(|time| !in_range(time)).then_some(Stop::TimeOutOfRange)
}

/// The device's notes of when songs were added, as far as they are worth putting in the
/// record. Word that a song went is passed on only where the record has a note of that
/// song: a date to take back. (A device keeps word of every removal it makes, not knowing
/// what the record has: [`crate::notes::Kept::edited`].)
fn worth_saying(mine: &Notes, theirs: &Notes) -> Notes {
    let mut out = mine.clone();
    for (playlist, notes) in &mut out {
        let noted = theirs.get(playlist);
        notes.retain(|song, note| !note.times.is_empty() || noted.is_some_and(|noted| noted.contains_key(song)));
    }
    out.retain(|_, notes| !notes.is_empty());
    out
}

/// Decides an exchange (step 3): what to keep, take in and write, given what the device
/// holds now and what the server answered. It cannot fail and does not panic, whatever it
/// is given.
pub fn settle(device: &Device, answers: &Answers) -> Outcome {
    // Which playlist, and what is in it: nothing the server didn't say is made up.
    let (playlist, comment) = match find(&answers.listed, &device.username) {
        Found::NotStated => return Outcome::Stop(Stop::OwnerNotStated),
        Found::None => (None, ""),
        Found::Ours(id) => {
            let Some(answer) = answers.playlist.as_ref().filter(|answer| answer.id == id && answer.name == PLAYLIST_NAME) else { return Outcome::Stop(Stop::Moved) };
            if answer.owner.is_none() {
                return Outcome::Stop(Stop::OwnerNotStated);
            }
            if !answer.ours(&device.username) {
                return Outcome::Stop(Stop::Moved);
            }
            // What the list of playlists states for it, if it states it.
            let listed = answers.listed.iter().find(|listed| listed.id == id).and_then(|listed| listed.comment.as_deref());
            let comment = match (&answer.comment, listed) {
                // Two answers that state it, differently (one of them, it may be, as the
                // empty text): one of them is not what the server holds now (an answer kept
                // by something on the way, a write in between), and there is no telling
                // which.
                (Some(comment), Some(listed)) if comment != listed => return Outcome::Stop(Stop::Disagree),
                (Some(comment), _) => comment.as_str(),
                // Navidrome leaves an empty comment out of every answer, so its absence
                // says "empty" at most; an answer that says little altogether leaves the
                // owner out too, and has stopped above. If the list of playlists states a
                // comment that isn't empty, there is one, and this answer is not saying.
                (None, Some(listed)) if !listed.is_empty() => return Outcome::Stop(Stop::Disagree),
                (None, _) => "",
            };
            (Some(id), comment)
        }
    };
    if comment.len() > limits::TEXT {
        return Outcome::Stop(Stop::TooLarge);
    }
    let offset = time::offset(answers.date, answers.received);
    // Times are put on things, and compared, by the server's clock where it is known.
    let now = time::aligned(answers.received, offset);
    if !in_range(now) || !in_range(answers.received) {
        return Outcome::Stop(Stop::TimeOutOfRange);
    }
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
        added: worth_saying(&device.notes, &theirs.added),
        history: device.history.clone(),
        plays: device.plays.clone(),
        play_sources: device.play_sources.clone(),
    };
    if let Some(stop) = unfit(&theirs).or_else(|| unfit(&mine)) {
        return Outcome::Stop(stop);
    }
    let merged = merge(&mine, &theirs, now);
    // What the person changed here, under this account, is made to the record now, on top
    // of what the server has: never to what the device happened to hold. Changes made under
    // another account are not this exchange's: they wait for theirs.
    let waiting: Vec<&Waiting> = device.choices.waiting_for(&device.account).collect();
    let record = onto(&merged, &waiting, now);
    // A write that was sent and never answered may still arrive (it was held up on its way;
    // the app stopped waiting). While it could, a change the person has made on top of what
    // it carried is kept even when the two come to nothing against what the server has
    // now: if the write does arrive, the later change is still there to be made on top of
    // it. (Pinned, the write held up, unpinned: let go as "nothing to do", the pin would
    // arrive afterwards and stay.) A change with nothing made on top of it is not kept so.
    let unsettled = waiting.iter().any(|waits| waits.on_its_way(now)) && waiting.iter().any(|waits| !waits.tried);
    let waiting: Vec<u64> = waiting.iter().map(|waits| waits.id).collect();
    let adds = record != theirs && (!comment.is_empty() || says(&record));
    // What would be written is checked as what is read is checked: a record this device
    // would refuse at its next read (a time one second past the last allowed, one pin more
    // than is taken in) is not written, here to be stopped at by every device after.
    let text = (adds && held.is_none()).then(|| record::write(&record, &unknown));
    if let Some(text) = &text {
        held = match unfit(&record) {
            Some(Stop::TimeOutOfRange) => Some(Held::TimeOutOfRange),
            Some(_) => Some(Held::TooLarge),
            None if text.len() > limits::TEXT => Some(Held::TooLarge),
            None => None,
        };
    }
    let write = text.filter(|_| held.is_none()).map(|text| Write {
        into: playlist.clone(),
        text,
        found: comment.to_string(),
        carried: waiting.clone(),
        at: now,
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
            done: if write.is_none() && held.is_none() && record == merged && !unsettled { waiting } else { Vec::new() },
            notes: theirs.added.clone(),
            history: theirs.history.clone(),
        },
        // Nothing to say, and nothing it would have written: not held back from anything.
        held: held.filter(|held| *held == Held::ClockWrong || adds),
        write,
    }))
}
