//! Exchanges, start to finish, over the library's decisions: a device as an app drives one,
//! and a server that is a list of playlists.
//!
//! Nothing here is the Windows app's or the phone's. The device below does exactly what
//! `nexus_core::step` tells an app to do, in the order it says, one step at a time, so a
//! case can stop a device between any two of them: after its reads, after the server has
//! taken its write and before it hears so, in the middle of another device's exchange.

use std::collections::HashMap;

use nexus_core::notes::{Kept, Note, Notes};
use nexus_core::record::{self, Read};
use nexus_core::step::{Agreed, Answers, Change, Choices, Device, Found, Held, Outcome, Plan, Stated, Stop, Write, clock, find, history, limits, reserved, settle};
use nexus_core::time::Clock;
use nexus_core::sync::{LONG_AGO, ListPrefs, PLAYLIST_NAME, PLAYS_SHARED, Shared};

const NOW: i64 = 1_800_000_000;

// ---- a server ---------------------------------------------------------------------------------

#[derive(Default)]
struct Server {
    playlists: Vec<Stated>,
    made: Vec<String>,
    writes: Vec<(String, String)>,
    /// Its clock, as its answers give it (`None`: it gives none).
    date: Option<i64>,
    /// It leaves out of its answers about one playlist what it states in its list.
    says_little: bool,
}

impl Server {
    fn new() -> Server {
        Server { date: Some(NOW), ..Default::default() }
    }

    fn with(comment: &str) -> Server {
        let mut server = Server::new();
        server.put("r", Some(comment), Some("tester"));
        server
    }

    fn put(&mut self, id: &str, comment: Option<&str>, owner: Option<&str>) {
        self.playlists.retain(|playlist| playlist.id != id);
        self.playlists.push(Stated { id: id.into(), name: PLAYLIST_NAME.into(), owner: owner.map(Into::into), comment: comment.map(Into::into) });
    }

    fn comment(&self, id: &str) -> String {
        self.playlists.iter().find(|playlist| playlist.id == id).and_then(|playlist| playlist.comment.clone()).unwrap_or_default()
    }

    fn record(&self, id: &str) -> Shared {
        match record::read(&self.comment(id)) {
            Read::Record(record) => record.shared,
            other => panic!("the comment of {id} is {other:?}"),
        }
    }

    /// The playlist's own answer. An empty comment is left out of it, as Navidrome leaves it.
    fn answer(&self, id: &str) -> Option<Stated> {
        let mut playlist = self.playlists.iter().find(|playlist| playlist.id == id)?.clone();
        if self.says_little || playlist.comment.as_deref() == Some("") {
            playlist.comment = None;
        }
        Some(playlist)
    }

    /// Takes a write: makes the playlist if it is to be made, and puts the text in it.
    fn take(&mut self, write: &Write) -> String {
        let id = write.into.clone().unwrap_or_else(|| {
            let id = format!("new{}", self.made.len());
            self.made.push(id.clone());
            id
        });
        self.put(&id, Some(&write.text), Some("tester"));
        self.writes.push((id.clone(), write.text.clone()));
        id
    }
}

// ---- a device ---------------------------------------------------------------------------------

/// What a device read, held until it is decided on.
struct Reads {
    listed: Vec<Stated>,
    playlist: Option<Stated>,
    date: Option<i64>,
}

#[derive(Clone)]
struct Dev {
    username: String,
    /// The device's own name for the sign-in.
    account: String,
    choices: Choices,
    kept: Kept,
    history: Vec<(String, i64)>,
    /// The log of plays, oldest first, and where some were made from.
    log: Vec<(String, i64)>,
    sources: Vec<(String, i64, String)>,
    /// Whether the log can be written to the disk.
    disk: bool,
    clock: i64,
    held: Option<Held>,
}

impl Dev {
    fn new() -> Dev {
        Dev { username: "Tester".into(), account: "Tester@music".into(), choices: Choices::default(), kept: Kept::default(), history: Vec::new(), log: Vec::new(), sources: Vec::new(), disk: true, clock: NOW, held: None }
    }

    /// The process is killed and started again: what it holds is what reached its files.
    fn restarted(&self) -> Dev {
        let saved = serde_json::to_string(&self.choices).unwrap();
        Dev { choices: serde_json::from_str(&saved).unwrap(), ..self.clone() }
    }

    fn does(&mut self, change: Change) -> bool {
        self.choices.change(&self.account, change)
    }

    fn pins(&self) -> Vec<String> {
        self.choices.shown(&self.account).pins
    }

    fn read(&self, server: &Server) -> Reads {
        let listed = server.playlists.clone();
        let playlist = match find(&listed, &self.username) {
            Found::Ours(id) => server.answer(&id),
            _ => None,
        };
        Reads { listed, playlist, date: server.date }
    }

    fn answers(&self, reads: &Reads) -> Answers {
        Answers { listed: reads.listed.clone(), playlist: reads.playlist.clone(), date: reads.date, received: self.clock }
    }

    /// Step 3: with the answers in hand, from what the device holds now.
    fn decide(&self, reads: &Reads) -> Outcome {
        let plays = nexus_core::sync::newest_plays(self.log.iter());
        let device = Device {
            username: self.username.clone(),
            account: self.account.clone(),
            choices: self.choices.clone(),
            notes: self.kept.shared(None),
            history: self.history.clone(),
            play_sources: self.sources.iter().filter(|(song, at, _)| plays.contains(&(song.clone(), *at))).cloned().collect(),
            plays,
        };
        settle(&device, &self.answers(reads))
    }

    /// Steps 5 and 6: keep the plays, on disk, or stop; then take in.
    fn take(&mut self, plan: &Plan) -> bool {
        if !self.disk {
            return false;
        }
        for play in &plan.keep.plays {
            if !self.log.contains(play) {
                self.log.push(play.clone());
            }
        }
        for source in &plan.keep.sources {
            if !self.sources.contains(source) {
                self.sources.push(source.clone());
            }
        }
        self.choices.take(&plan.take);
        self.kept.take(&plan.take.notes);
        self.history = history(&self.history, &plan.take);
        self.held = plan.held;
        true
    }

    /// A whole exchange. What stopped it, if anything did.
    fn exchange(&mut self, server: &mut Server) -> Option<Stop> {
        let reads = self.read(server);
        let plan = match self.decide(&reads) {
            Outcome::Stop(stop) => return Some(stop),
            Outcome::Go(plan) => plan,
        };
        if !self.take(&plan) {
            return None;
        }
        if let Some(write) = &plan.write {
            assert!(self.choices.sending(write), "decided and sent in one go: the plan fits");
            server.take(write);
            self.choices.written(write);
        }
        None
    }
}

fn plan(outcome: Outcome) -> Plan {
    match outcome {
        Outcome::Go(plan) => *plan,
        Outcome::Stop(stop) => panic!("stopped: {stop:?}"),
    }
}

fn pins(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

fn plays(entries: &[(&str, i64)]) -> Vec<(String, i64)> {
    entries.iter().map(|(id, at)| (id.to_string(), *at)).collect()
}

/// What a PC has shared: pins, a theme, a list's choices, a date, history and plays.
fn pc() -> Shared {
    Shared {
        nexus_music_sync: 1,
        pins: pins(&["p1", "p2"]),
        pins_at: 1_700_000_000,
        theme: "Ember".into(),
        theme_at: 1_700_000_100,
        lists: HashMap::from([("playlist:p1".to_string(), ListPrefs { shuffle: true, sort: 3, descending: true, sort_chosen: true, at: 1_700_000_200 })]),
        added: Notes::from([("p1".to_string(), HashMap::from([("s1".to_string(), Note { times: vec![1_700_000_300], at: 1_700_000_300 })]))]),
        history: plays(&[("s2", 1_700_000_500), ("s1", 1_700_000_400)]),
        plays: plays(&[("s2", 1_700_000_500), ("s1", 1_700_000_400)]),
        play_sources: vec![("s1".to_string(), 1_700_000_400, "playlist:p1".to_string())],
    }
}

fn text(shared: &Shared) -> String {
    record::write(shared, &[])
}

/// A device that has exchanged with the server and holds what it has.
fn in_step(server: &mut Server) -> Dev {
    let mut device = Dev::new();
    assert_eq!(device.exchange(server), None);
    device
}

// ---- rule 1: a change made on a device is kept as the change ------------------------------------

#[test]
fn a_pin_made_before_the_pcs_pins_were_read_is_added_to_them_not_put_in_their_place() {
    // The PC has two playlists pinned, on the server.
    let mut server = Server::with(&text(&pc()));
    // A device newly signed in: its first read of the server is still on its way...
    let mut device = Dev::new();
    let _on_its_way = device.read(&server);
    // ...when the person pins a third on it. All it can show is that one.
    device.does(Change::Pin("c".into()));
    assert_eq!(device.pins(), ["c"]);
    // Its process is killed: the read never comes back. Started again, it exchanges.
    let mut device = device.restarted();
    assert_eq!(device.pins(), ["c"]);
    assert_eq!(device.exchange(&mut server), None);
    // The server's pins have it added, with a time of now; nothing else has a new time.
    let record = server.record("r");
    assert_eq!((record.pins.clone(), record.pins_at), (pins(&["p1", "p2", "c"]), NOW));
    assert_eq!(Shared { pins: pc().pins, pins_at: pc().pins_at, ..record }, pc());
    assert_eq!(device.pins(), ["p1", "p2", "c"]);
    assert!(device.choices.waiting.is_empty());
}

#[test]
fn a_shuffle_tapped_before_the_lists_choices_arrived_leaves_its_sort_alone() {
    // The PC has the list sorted its own way, and not shuffled.
    let sorted = ListPrefs { shuffle: false, sort: 3, descending: true, sort_chosen: true, at: 1_700_000_200 };
    let mut server = Server::with(&text(&Shared { lists: HashMap::from([("playlist:p1".to_string(), sorted)]), ..pc() }));
    // On a device that has read none of that, the person turns shuffle on for it. (Sent as
    // the list's whole choices, stamped now, the PC's sort would be gone.)
    let mut device = Dev::new();
    device.does(Change::Shuffle { key: "playlist:p1".into(), on: true });
    device.does(Change::Shuffle { key: "liked".into(), on: true });
    assert_eq!(device.exchange(&mut server), None);
    let record = server.record("r");
    // The PC's sort for the list stands; only the shuffle is changed, as of now.
    assert_eq!(record.lists["playlist:p1"], ListPrefs { shuffle: true, at: NOW, ..sorted });
    assert_eq!(record.lists["liked"], ListPrefs { shuffle: true, at: NOW, ..Default::default() });
    assert_eq!((record.pins_at, record.theme_at), (pc().pins_at, pc().theme_at));
}

#[test]
fn an_unpin_a_sort_and_a_theme_are_made_to_what_the_server_has() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    device.does(Change::Unpin("p1".into()));
    device.does(Change::Sort { key: "playlist:p1".into(), sort: 1, descending: false });
    device.does(Change::Theme("Ocean".into()));
    assert_eq!(device.exchange(&mut server), None);
    let record = server.record("r");
    assert_eq!((record.pins.clone(), record.pins_at), (pins(&["p2"]), NOW));
    assert_eq!((record.theme.as_str(), record.theme_at), ("Ocean", NOW));
    assert_eq!(record.lists["playlist:p1"], ListPrefs { shuffle: true, sort: 1, descending: false, sort_chosen: true, at: NOW });
    // A playlist that has gone puts its list back to the usual, where the record has it.
    device.does(Change::Usual { key: "playlist:p1".into() });
    device.does(Change::Usual { key: "playlist:never-chosen".into() });
    device.clock += 10;
    server.date = Some(NOW + 10);
    assert_eq!(device.exchange(&mut server), None);
    let record = server.record("r");
    assert_eq!(record.lists["playlist:p1"], ListPrefs { at: NOW + 10, ..Default::default() });
    assert!(!record.lists.contains_key("playlist:never-chosen"));
}

#[test]
fn a_change_that_makes_no_difference_to_what_the_server_has_puts_no_time_on_anything() {
    let mut server = Server::with(&text(&pc()));
    // Pinning what the PC has pinned, the shuffle the list already has, the theme it is in:
    // the device could not know.
    let mut device = Dev::new();
    device.does(Change::Pin("p2".into()));
    device.does(Change::Shuffle { key: "playlist:p1".into(), on: true });
    device.does(Change::Theme("Ember".into()));
    assert_eq!(device.choices.waiting.len(), 3);
    assert_eq!(device.exchange(&mut server), None);
    // Nothing written at all, and nothing left waiting.
    assert!(server.writes.is_empty());
    assert_eq!(server.comment("r"), text(&pc()));
    assert!(device.choices.waiting.is_empty());
    assert_eq!(device.pins(), ["p1", "p2"]);
}

#[test]
fn a_change_gets_a_time_after_the_one_it_replaces_whatever_clock_put_that_on() {
    // A PC whose clock runs four minutes ahead of the server's pinned something just now.
    let ahead = Shared { pins_at: NOW + 240, ..pc() };
    let mut server = Server::with(&text(&ahead));
    let mut device = Dev::new();
    device.does(Change::Pin("c".into()));
    assert_eq!(device.exchange(&mut server), None);
    // The later change is the newer one on every device.
    assert_eq!(server.record("r").pins_at, NOW + 241);
    // And "now" is the server's: a device three minutes slow puts the server's time on.
    let mut slow = Dev::new();
    slow.clock = NOW + 1_000 - 180;
    server.date = Some(NOW + 1_000);
    slow.does(Change::Theme("Rose".into()));
    assert_eq!(slow.exchange(&mut server), None);
    assert_eq!(server.record("r").theme_at, NOW + 1_000);
}

#[test]
fn a_change_is_forgotten_only_once_its_write_is_known_to_have_succeeded() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    device.does(Change::Pin("c".into()));
    // The exchange gets as far as sending the write, and the request fails: the server never
    // had it.
    let reads = device.read(&server);
    let plan = plan(device.decide(&reads));
    assert!(device.take(&plan));
    let write = plan.write.unwrap();
    assert!(device.choices.sending(&write));
    // The change still waits, and is still on the screen, through a restart.
    let mut device = device.restarted();
    assert_eq!((device.choices.waiting.len(), device.choices.waiting[0].tried), (1, true));
    assert_eq!(device.pins(), ["p1", "p2", "c"]);
    assert_eq!(server.record("r").pins, ["p1", "p2"]);
    // The next exchange carries it again, and then it is done with.
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!(server.record("r").pins, ["p1", "p2", "c"]);
    assert!(device.choices.waiting.is_empty());
    assert_eq!(device.choices.agreed.pins, ["p1", "p2", "c"]);
}

#[test]
fn a_change_made_while_an_exchange_is_out_stays_on_screen_and_goes_with_the_next() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    device.does(Change::Pin("c".into()));
    let reads = device.read(&server);
    let plan = plan(device.decide(&reads));
    assert!(device.take(&plan));
    let write = plan.write.unwrap();
    assert!(device.choices.sending(&write));
    // While the write is on its way the person pins another, and unpins the first.
    device.does(Change::Pin("d".into()));
    device.does(Change::Unpin("c".into()));
    assert_eq!(device.pins(), ["p1", "p2", "d"]);
    server.take(&write);
    device.choices.written(&write);
    // What the write carried is agreed; what was done meanwhile still waits, and shows.
    assert_eq!(device.choices.agreed.pins, ["p1", "p2", "c"]);
    assert_eq!(device.pins(), ["p1", "p2", "d"]);
    assert_eq!(device.choices.waiting.len(), 2);
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!(server.record("r").pins, ["p1", "p2", "d"]);
    assert!(device.choices.waiting.is_empty());
}

#[test]
fn the_last_word_on_one_thing_is_the_one_that_waits_and_one_taken_back_unsent_is_no_change() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    // Three sorts for one list: one waits.
    for sort in [1, 2, 4] {
        assert!(device.does(Change::Sort { key: "liked".into(), sort, descending: false }));
    }
    assert_eq!(device.choices.waiting.len(), 1);
    // Back to what was there before anything was sent: nothing waits.
    device.does(Change::Shuffle { key: "playlist:p1".into(), on: false });
    device.does(Change::Shuffle { key: "playlist:p1".into(), on: true });
    assert_eq!(device.choices.waiting.len(), 1);
    assert!(!device.does(Change::Theme("Ember".into())));
    assert_eq!(device.choices.waiting.len(), 1);

    // Pinned and unpinned again on a device that has read nothing, where a PC has had it
    // pinned all along: the PC's pin is not taken away by a change that was never one.
    let mut fresh = Dev::new();
    assert!(fresh.does(Change::Pin("p1".into())));
    assert!(fresh.does(Change::Unpin("p1".into())));
    assert!(fresh.choices.waiting.is_empty());
    assert_eq!(fresh.exchange(&mut server), None);
    assert_eq!(server.record("r").pins, ["p1", "p2"]);
    // Once a write has carried the pin, taking it back is a change like any other.
    let mut device = in_step(&mut server);
    device.does(Change::Pin("c".into()));
    let reads = device.read(&server);
    let write = plan(device.decide(&reads)).write.unwrap();
    assert!(device.choices.sending(&write));
    device.does(Change::Unpin("c".into()));
    assert_eq!(device.choices.waiting.len(), 2);
    assert_eq!(device.pins(), ["p1", "p2"]);
}

#[test]
fn the_answer_to_a_write_is_lost_and_the_other_device_then_sets_the_opposite() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    let mut other = in_step(&mut server);
    // The device pins C. The server takes the write; the answer never arrives, and the
    // process dies.
    device.does(Change::Pin("c".into()));
    let reads = device.read(&server);
    let plan = plan(device.decide(&reads));
    assert!(device.take(&plan));
    let write = plan.write.unwrap();
    assert!(device.choices.sending(&write));
    server.take(&write);
    let lost = device.restarted();
    assert_eq!(server.record("r").pins, ["p1", "p2", "c"]);

    // If nothing else happens, the change is made again over its own result, which alters
    // nothing: no write, no new time, and it is done with.
    let mut quiet = lost.clone();
    let before = server.comment("r");
    assert_eq!(quiet.exchange(&mut server), None);
    assert_eq!(server.comment("r"), before);
    assert!(quiet.choices.waiting.is_empty());
    assert_eq!(quiet.pins(), ["p1", "p2", "c"]);

    // But the other device sees C pinned, and the person there deliberately unpins it, a
    // minute later.
    other.clock = NOW + 60;
    server.date = Some(NOW + 60);
    assert_eq!(other.exchange(&mut server), None);
    other.does(Change::Unpin("c".into()));
    assert_eq!(other.exchange(&mut server), None);
    assert_eq!((server.record("r").pins, server.record("r").pins_at), (pins(&["p1", "p2"]), NOW + 60));

    // The device starts again, a minute after that, still holding "pin C" as tried and
    // unanswered. ACCEPTED, AND WRITTEN DOWN (step.rs, the top): the change is made again,
    // on top of the other device's unpin, as the newer. C is pinned on both.
    let mut device = lost;
    device.clock = NOW + 120;
    server.date = Some(NOW + 120);
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!((server.record("r").pins, server.record("r").pins_at), (pins(&["p1", "p2", "c"]), NOW + 120));
    assert!(device.choices.waiting.is_empty());
    assert_eq!(other.exchange(&mut server), None);
    assert_eq!(other.pins(), ["p1", "p2", "c"]);
    // Nothing else of the other device's is touched by it: its pins were unpinned-from, not
    // replaced.
    assert_eq!(Shared { pins: pc().pins, pins_at: pc().pins_at, ..server.record("r") }, pc());
}

#[test]
fn a_change_taken_back_while_the_server_is_being_asked_is_never_sent() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    // The person pins C, and an exchange begins: the server is asked (step 1).
    device.does(Change::Pin("c".into()));
    let reads = device.read(&server);
    // While the answers are on their way the person unpins C again. Neither had been sent:
    // the two are no change, and nothing waits.
    device.does(Change::Unpin("c".into()));
    assert!(device.choices.waiting.is_empty());
    // The answers arrive, and the decision is made from what is true now: nothing to write.
    let decided = plan(device.decide(&reads));
    assert!(decided.write.is_none());

    // An app that had decided first and acted later (the plan made while C was still
    // pinned) is refused at the moment of sending: the plan no longer fits the choices.
    let mut hasty = in_step(&mut server);
    hasty.does(Change::Pin("c".into()));
    let reads = hasty.read(&server);
    let stale = plan(hasty.decide(&reads));
    assert!(hasty.take(&stale));
    hasty.does(Change::Unpin("c".into()));
    let write = stale.write.unwrap();
    assert!(!hasty.choices.sending(&write), "a pin that was taken back would have been sent, and counted as agreed");
    // Nothing was sent, nothing is marked as tried, and the next exchange has nothing to say.
    assert!(server.writes.is_empty() && hasty.choices.waiting.is_empty());
    assert_eq!(hasty.exchange(&mut server), None);
    assert!(server.writes.is_empty());
    assert_eq!((server.record("r").pins, hasty.pins()), (pins(&["p1", "p2"]), pins(&["p1", "p2"])));
    // A change made since that the write does not carry is no reason to refuse it.
    let mut busy = in_step(&mut server);
    busy.does(Change::Pin("c".into()));
    let reads = busy.read(&server);
    let write = plan(busy.decide(&reads)).write.unwrap();
    busy.does(Change::Theme("Rose".into()));
    assert!(busy.choices.sending(&write));
}

#[test]
fn a_song_started_while_the_server_was_being_asked_stays_in_recently_played() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    device.history.insert(0, ("x".into(), NOW - 100));
    let reads = device.read(&server);
    let decided = plan(device.decide(&reads));
    // Y starts playing after the decision and before it is taken in.
    device.history.insert(0, ("y".into(), NOW + 1));
    assert!(device.take(&decided));
    // Recently played is what the device has now with what the others had: not the plan's.
    assert_eq!(device.history, plays(&[("y", NOW + 1), ("x", NOW - 100), ("s2", 1_700_000_500), ("s1", 1_700_000_400)]));
}

#[test]
fn a_change_waits_for_the_account_it_was_made_under() {
    // Two accounts on one device, each with a record on its own server.
    let (mut theirs, mut other) = (Server::with(&text(&pc())), Server::with(&text(&Shared { pins: pins(&["q1"]), pins_at: 1_700_000_900, ..pc() })));
    let mut device = in_step(&mut theirs);
    // Signed in as the first, with sharing off (or the server away), the person pins C.
    device.does(Change::Pin("c".into()));
    // Then signs in as the second.
    device.account = "Other@elsewhere".into();
    // The first account's change is not shown there, and not sent there: it would go into
    // the second account's record as a choice made now.
    assert_eq!(device.exchange(&mut other), None);
    assert!(other.writes.is_empty());
    assert_eq!((other.record("r").pins, device.pins()), (pins(&["q1"]), pins(&["q1"])));
    // Nor does a change made as the second cancel or carry it.
    device.does(Change::Unpin("c".into()));
    device.does(Change::Pin("q2".into()));
    assert_eq!(device.exchange(&mut other), None);
    assert_eq!(other.record("r").pins, pins(&["q1", "q2"]));
    assert_eq!(device.choices.waiting.len(), 1);
    assert_eq!((device.choices.waiting[0].account.as_str(), &device.choices.waiting[0].change), ("Tester@music", &Change::Pin("c".into())));
    // It waits, untouched, until the first signs in again: then it is made, to what that
    // account's record has then.
    device.account = "Tester@music".into();
    assert_eq!(device.exchange(&mut theirs), None);
    assert!(theirs.record("r").pins.contains(&"c".to_string()));
    assert!(device.choices.waiting.is_empty());
}

#[test]
fn what_an_earlier_version_held_is_agreed_at_the_times_it_had() {
    // A settings file from a version that stamped whole values: pins with a time, a theme
    // chosen before sharing existed, one list with a time and one without.
    let lists = HashMap::from([("liked".to_string(), ListPrefs { sort: 2, sort_chosen: true, at: 500, ..Default::default() }), ("songs".to_string(), ListPrefs { shuffle: true, ..Default::default() })]);
    let agreed = Agreed::from_stamped(&pins(&["a"]), 400, "Rose", 0, &lists, "Violet");
    assert_eq!((agreed.pins_at, agreed.theme.as_str(), agreed.theme_at), (400, "Rose", LONG_AGO));
    assert_eq!((agreed.lists["liked"].at, agreed.lists["songs"].at), (500, LONG_AGO));
    // The theme nobody chose, and no pins: nobody's choice, and no time.
    let usual = Agreed::from_stamped(&[], 0, "Violet", 0, &HashMap::new(), "Violet");
    assert_eq!((usual.pins_at, usual.theme_at), (0, 0));
    // Against a PC's record with newer choices it loses item by item, and adds the list the
    // PC had nothing for; nothing of it is given a new time.
    let mut server = Server::with(&text(&pc()));
    let mut device = Dev::new();
    device.choices.agreed = agreed;
    assert_eq!(device.pins(), ["a"]);
    assert_eq!(device.exchange(&mut server), None);
    let record = server.record("r");
    assert_eq!((record.pins.clone(), record.theme.as_str()), (pc().pins, "Ember"));
    assert_eq!((record.lists["liked"].at, record.lists["songs"].at, record.lists.len()), (500, LONG_AGO, 3));
    assert_eq!(device.pins(), ["p1", "p2"]);
}

// ---- rule 2: what the server did not say is not "empty" -----------------------------------------

#[test]
fn a_playlist_whose_owner_is_not_stated_is_not_ours() {
    for says in [false, true] {
        let mut device = Dev::new();
        if says {
            device.does(Change::Pin("mine".into()));
        }
        // Neither owner nor comment stated (a client the server says little to): read as
        // "ours, and empty", the device's own record would go over everything in it.
        let mut neither = Server::new();
        neither.put("a", None, None);
        assert_eq!(device.clone().exchange(&mut neither), Some(Stop::OwnerNotStated));
        assert!(neither.writes.is_empty() && neither.made.is_empty());
        // The comment stated and the owner not: the same.
        let mut no_owner = Server::new();
        no_owner.put("a", Some(&text(&pc())), None);
        let mut stopped = device.clone();
        assert_eq!(stopped.exchange(&mut no_owner), Some(Stop::OwnerNotStated));
        assert!(no_owner.writes.is_empty() && no_owner.made.is_empty());
        // Nothing was read out of it either.
        assert_eq!(stopped.pins(), device.pins());
        // The list says it is ours, and the playlist's own answer doesn't say.
        let mut later = Server::with(&text(&pc()));
        let mut reads = device.read(&later);
        reads.playlist.as_mut().unwrap().owner = None;
        assert_eq!(device.decide(&reads), Outcome::Stop(Stop::OwnerNotStated));
        // One stated to be ours beside one not stated: ours is used.
        later.put("b", Some("whose?"), None);
        assert_eq!(device.clone().exchange(&mut later), None);
        // Another account's playlist of that name is nothing to do with this one.
        let mut theirs = Server::new();
        theirs.put("a", Some(&text(&pc())), Some("someone-else"));
        assert_eq!(device.clone().exchange(&mut theirs), None);
        assert_eq!(theirs.comment("a"), text(&pc()));
        assert_eq!(theirs.made.len(), usize::from(says));
    }
}

#[test]
fn an_empty_comment_the_server_leaves_out_of_its_answer_is_an_empty_one() {
    // A sync playlist just made (by this device, whose write then failed; by another): it
    // comes back with its owner and no comment. Taken for "not said", it could never be
    // filled.
    let mut server = Server::new();
    server.put("a", Some(""), Some("tester"));
    assert_eq!(server.answer("a").unwrap().comment, None);
    // A device with nothing to say leaves it as it is...
    assert_eq!(Dev::new().exchange(&mut server), None);
    assert!(server.writes.is_empty() && server.made.is_empty());
    // ...and one with something of the person's fills it, in that playlist.
    let mut device = Dev::new();
    device.does(Change::Pin("p".into()));
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!((server.record("a").pins, server.made.len()), (pins(&["p"]), 0));
}

#[test]
fn answers_that_disagree_about_the_comment_stop_the_exchange() {
    // The list of playlists states the record; the playlist's own answer has its owner and
    // no comment. It is not empty: this answer isn't saying.
    let mut server = Server::with(&text(&pc()));
    server.says_little = true;
    let mut device = Dev::new();
    device.does(Change::Pin("mine".into()));
    assert_eq!(device.exchange(&mut server), Some(Stop::Disagree));
    assert!(server.writes.is_empty());
    assert_eq!(server.comment("r"), text(&pc()));
    assert_eq!(device.pins(), ["mine"]);
    // An answer for another playlist than the one the list names, or none: look again.
    let mut reads = device.read(&server);
    reads.playlist = None;
    assert_eq!(device.decide(&reads), Outcome::Stop(Stop::Moved));
    reads.playlist = Some(Stated { id: "z".into(), name: PLAYLIST_NAME.into(), owner: Some("tester".into()), comment: Some(String::new()) });
    assert_eq!(device.decide(&reads), Outcome::Stop(Stop::Moved));
    // Renamed between the two reads.
    reads.playlist = Some(Stated { id: "r".into(), name: "Road trip".into(), owner: Some("tester".into()), comment: Some(text(&pc())) });
    assert_eq!(device.decide(&reads), Outcome::Stop(Stop::Moved));
    // Both answers state a comment, and not the same one: one of them is not what the
    // server holds now, and there is no telling which.
    server.says_little = false;
    let mut reads = device.read(&server);
    reads.playlist.as_mut().unwrap().comment = Some(text(&Shared { pins: pins(&["older"]), ..pc() }));
    assert_eq!(device.decide(&reads), Outcome::Stop(Stop::Disagree));
    assert!(server.writes.is_empty());
    // The same one in both is the ordinary case.
    assert!(matches!(device.decide(&device.read(&server)), Outcome::Go(_)));
}

#[test]
fn the_clock_is_measured_whatever_the_playlist_holds() {
    // An exchange that stops for another reason has still read the server's clock: the
    // queue needs it too.
    let mut server = Server::with("do not delete: my notes");
    server.date = Some(NOW + 90);
    let device = Dev::new();
    let reads = device.read(&server);
    assert_eq!(device.decide(&reads), Outcome::Stop(Stop::NotARecord));
    assert_eq!(clock(&device.answers(&reads)), Clock::Offset(90));
    server.date = None;
    assert_eq!(clock(&device.answers(&device.read(&server))), Clock::NoDate);
}

#[test]
fn nothing_the_library_is_given_makes_it_fail() {
    let device = Dev::new();
    let with = |change: &dyn Fn(&mut Shared)| {
        let mut record = pc();
        change(&mut record);
        Server::with(&text(&record))
    };
    // A time no version would have written, anywhere in the record: stopped at, with
    // nothing read into the device and nothing written. (Taken in, a time at the end of
    // what a number can hold could never be bettered, and one more second could not be
    // added to it.)
    let times: [&dyn Fn(&mut Shared); 7] = [
        &|r| r.pins_at = i64::MAX,
        &|r| r.theme_at = -1,
        &|r| r.lists.get_mut("playlist:p1").unwrap().at = limits::TIME + 1,
        &|r| r.added.get_mut("p1").unwrap().get_mut("s1").unwrap().at = i64::MIN,
        &|r| r.added.get_mut("p1").unwrap().get_mut("s1").unwrap().times = vec![i64::MAX],
        &|r| r.history[0].1 = i64::MAX,
        &|r| r.plays[1].1 = -5,
    ];
    for change in times {
        let mut server = with(change);
        let mut stopped = device.clone();
        stopped.does(Change::Pin("c".into()));
        assert_eq!(stopped.exchange(&mut server), Some(Stop::TimeOutOfRange));
        assert!(server.writes.is_empty() && stopped.pins() == ["c"]);
    }
    // The same on the device's side, and for its clock and the server's.
    let mut server = Server::with(&text(&pc()));
    let mut own = device.clone();
    own.history = plays(&[("x", i64::MAX)]);
    assert_eq!(own.exchange(&mut server), Some(Stop::TimeOutOfRange));
    for (clock, date) in [(i64::MAX, Some(NOW)), (i64::MIN, Some(NOW)), (NOW, Some(i64::MAX)), (NOW, Some(i64::MIN)), (-1, None)] {
        server.date = date;
        let mut odd = device.clone();
        odd.clock = clock;
        odd.does(Change::Pin("c".into()));
        assert!(matches!(odd.exchange(&mut server), Some(Stop::TimeOutOfRange) | None), "{clock} {date:?}");
        assert!(server.writes.is_empty(), "{clock} {date:?}");
    }
    // More than anything the app would have written.
    let sizes: [&dyn Fn(&mut Shared); 4] = [
        &|r| r.pins = (0..limits::PINS + 1).map(|n| n.to_string()).collect(),
        &|r| r.plays = (0..limits::PLAYS as i64 + 1).map(|n| ("s".to_string(), n)).collect(),
        &|r| r.history = (0..limits::HISTORY as i64 + 1).map(|n| (n.to_string(), n)).collect(),
        &|r| r.theme = "x".repeat(limits::NAME + 1),
    ];
    for change in sizes {
        let mut server = with(change);
        assert_eq!(device.clone().exchange(&mut server), Some(Stop::TooLarge));
        assert!(server.writes.is_empty());
    }
    let mut server = Server::with(&format!(r#"{{"nexus_music_sync":1,"junk":"{}"}}"#, "x".repeat(limits::TEXT)));
    assert_eq!(device.clone().exchange(&mut server), Some(Stop::TooLarge));
    // And a change's time is one more than the latest there can be, at the most.
    let mut server = with(&|r| r.pins_at = limits::TIME);
    let mut last = device.clone();
    last.does(Change::Pin("c".into()));
    assert_eq!(last.exchange(&mut server), None);
    assert_eq!(server.record("r").pins_at, limits::TIME + 1);
}

#[test]
fn a_device_with_nothing_to_say_makes_no_record_and_fills_no_empty_one() {
    // No playlist at all: none is made.
    let mut none = Server::new();
    assert_eq!(Dev::new().exchange(&mut none), None);
    assert!(none.made.is_empty() && none.writes.is_empty());
    // The usual theme, with no time on it, is nobody's choice: still nothing.
    let mut usual = Dev::new();
    usual.choices.agreed = Agreed::from_stamped(&[], 0, "Violet", 0, &HashMap::new(), "Violet");
    assert_eq!(usual.exchange(&mut none), None);
    assert!(none.made.is_empty());
    // With one play of the person's, the record is made, with a request made once.
    let mut device = Dev::new();
    device.log.push(("s".into(), NOW - 5));
    assert_eq!(device.exchange(&mut none), None);
    assert_eq!((none.made.len(), none.record("new0").plays), (1, plays(&[("s", NOW - 5)])));
    // And a second device then finds it, and makes no other.
    let mut second = Dev::new();
    second.does(Change::Pin("p".into()));
    assert_eq!(second.exchange(&mut none), None);
    assert_eq!((none.made.len(), none.record("new0").pins), (1, pins(&["p"])));
}

// ---- rule 3: a device writes only if it can write back everything it read ------------------------

#[test]
fn a_fresh_device_leaves_the_pcs_record_exactly_as_it_is() {
    let mut server = Server::with(&text(&pc()));
    let mut device = Dev::new();
    assert_eq!(device.exchange(&mut server), None);
    assert!(server.writes.is_empty());
    // It holds what the PC had, and what it then holds gives nothing to write either.
    assert_eq!(device.pins(), ["p1", "p2"]);
    assert_eq!(device.choices.shown(&device.account).theme, "Ember");
    assert_eq!(device.history, pc().history);
    assert_eq!(device.log.len(), 2);
    assert_eq!(device.kept.dates("p1", &["s1"]), [1_700_000_300]);
    assert_eq!(device.exchange(&mut server), None);
    assert!(server.writes.is_empty());
    assert_eq!(server.comment("r"), text(&pc()));
}

#[test]
fn a_comment_that_is_not_a_record_is_left_alone() {
    for comment in ["do not delete: my notes", "  ", "{}", r#"{"pins":["x"]}"#] {
        let mut server = Server::with(comment);
        let mut device = Dev::new();
        device.does(Change::Pin("mine".into()));
        assert_eq!(device.exchange(&mut server), Some(Stop::NotARecord), "{comment:?}");
        assert!(server.writes.is_empty() && server.made.is_empty());
        assert_eq!(server.comment("r"), comment);
        // The change still waits for the day the comment is a record, or empty.
        assert_eq!(device.choices.waiting.len(), 1);
    }
}

#[test]
fn a_record_in_a_later_layout_is_read_and_never_written() {
    let later = format!(r#"{{"nexus_music_sync":2,"pins":["from-later"],"pins_at":{},"more":[1,2,3]}}"#, NOW - 50);
    let mut server = Server::with(&later);
    let mut device = Dev::new();
    device.does(Change::Pin("mine".into()));
    assert_eq!(device.exchange(&mut server), None);
    assert!(server.writes.is_empty());
    assert_eq!(server.comment("r"), later);
    // What is understood of it is taken in, the change still waits on top of it, and the
    // device says why it isn't sharing.
    assert_eq!(device.pins(), ["from-later", "mine"]);
    assert_eq!((device.held, device.choices.waiting.len()), (Some(Held::LaterLayout), 1));
    // A later layout whose fields this version can make nothing of: the same, with nothing
    // taken in.
    let mut server = Server::with(r#"{"nexus_music_sync":3,"pins":{"all":"changed"}}"#);
    let mut device = Dev::new();
    device.does(Change::Pin("mine".into()));
    assert_eq!(device.exchange(&mut server), None);
    assert!(server.writes.is_empty());
    assert_eq!((device.pins(), device.held), (pins(&["mine"]), Some(Held::LaterLayout)));
}

#[test]
fn fields_this_version_does_not_know_are_written_back_as_they_were_or_the_record_is_not_written() {
    // At the top of the record: carried through a write, exactly.
    let extra = text(&pc()).replacen("{", r#"{"devices":{"pc":[1,2,{"x":null}],"n":1.50},"#, 1);
    let mut server = Server::with(&extra);
    let mut device = Dev::new();
    device.does(Change::Pin("c".into()));
    assert_eq!(device.exchange(&mut server), None);
    assert!(server.comment("r").ends_with(r#","devices":{"pc":[1,2,{"x":null}],"n":1.50}}"#));
    assert_eq!(server.record("r").pins, ["p1", "p2", "c"]);
    // Inside a list's choices, where there is nowhere to keep it: read, and not written.
    let nested = text(&pc()).replace(r#""shuffle":true,"#, r#""shuffle":true,"view":"grid","#);
    assert_ne!(nested, text(&pc()));
    let mut server = Server::with(&nested);
    let mut device = Dev::new();
    device.does(Change::Pin("c".into()));
    assert_eq!(device.exchange(&mut server), None);
    assert!(server.writes.is_empty());
    assert_eq!((device.pins(), device.held), (pins(&["p1", "p2", "c"]), Some(Held::MoreThanKnown)));
    // With nothing it would have written, it isn't held back from anything.
    let mut fresh = Dev::new();
    assert_eq!(fresh.exchange(&mut server), None);
    assert_eq!(fresh.held, None);
}

// ---- rule 4: the lowest-id playlist of that name that is ours, every time ------------------------

#[test]
fn the_lowest_id_that_is_ours_is_used_at_every_exchange_not_one_remembered() {
    let mut server = Server::new();
    server.put("m", Some(&text(&pc())), Some("TESTER"));
    server.put("z", Some(r#"{"nexus_music_sync":1,"pins":["other"],"pins_at":5}"#), Some("tester"));
    server.put("a", Some(r#"{"nexus_music_sync":1,"pins":["not-ours"],"pins_at":9999999999}"#), Some("guest"));
    let mut device = Dev::new();
    device.does(Change::Pin("c".into()));
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!(server.writes.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), ["m"]);
    assert_eq!(server.record("m").pins, ["p1", "p2", "c"]);
    // A lower one appears (another device made it, having found none). The device has been
    // exchanging with "m", and goes to the lower all the same; what it holds goes with it.
    server.put("b", Some(""), Some("tester"));
    device.does(Change::Pin("d".into()));
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!(server.writes.last().unwrap().0, "b");
    assert_eq!(server.record("b").pins, ["p1", "p2", "c", "d"]);
    assert_eq!(server.record("b").plays, pc().plays);
    // Ids compare as bytes, as every version has compared them.
    let listed = ["é", "z", "Z2", "a10", "a9"].map(|id| Stated { id: id.into(), name: PLAYLIST_NAME.into(), owner: Some("tester".into()), comment: None });
    assert_eq!(find(&listed, "tester"), Found::Ours("Z2".into()));
    // The name is kept for the record.
    assert!(reserved("Nexus Music sync") && reserved("  Nexus Music sync ") && !reserved("Nexus Music") && !reserved("nexus music sync"));
}

// ---- rule 5: incoming plays are kept before anything is trimmed -----------------------------------

#[test]
fn incoming_plays_are_kept_before_the_outgoing_five_hundred_are_cut() {
    // The record holds one old play of a PC's that this device has never seen; the device
    // has five hundred newer ones of its own.
    let old = ("pc-only".to_string(), NOW - 1_000_000);
    let mut server = Server::with(&text(&Shared { plays: vec![old.clone()], play_sources: vec![(old.0.clone(), old.1, "playlist:p1".into())], ..pc() }));
    let mut device = Dev::new();
    device.log = (0..PLAYS_SHARED as i64).map(|n| (format!("s{n}"), NOW - 1_000 + n)).collect();
    let reads = device.read(&server);
    let plan = plan(device.decide(&reads));
    // What is to be written no longer has room for it...
    let Read::Record(written) = record::read(&plan.write.as_ref().unwrap().text) else { panic!() };
    assert_eq!(written.shared.plays.len(), PLAYS_SHARED);
    assert!(!written.shared.plays.contains(&old));
    // ...so it is handed over to be kept first, with where it was played from.
    assert_eq!(plan.keep.plays, [old.clone()]);
    assert_eq!(plan.keep.sources.len(), 1);
    // And if the device can't keep it (its disk is full), nothing is written.
    device.disk = false;
    assert_eq!(device.exchange(&mut server), None);
    assert!(server.writes.is_empty());
    device.disk = true;
    assert_eq!(device.exchange(&mut server), None);
    assert!(device.log.contains(&old));
    assert_eq!(server.writes.len(), 1);
}

// ---- rule 6: a date is put only on the device's own confirmed change -----------------------------

#[test]
fn what_a_device_notices_in_a_playlist_is_never_put_on_record() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    // It looks at the playlist and finds a song another device (or another app) added.
    device.kept.notice("p1", &["s1"], NOW);
    device.kept.notice("p1", &["s1", "s9"], NOW + 5);
    assert_eq!(device.kept.dates("p1", &["s1", "s9"]), [1_700_000_300, NOW + 5]);
    assert_eq!(device.exchange(&mut server), None);
    assert!(server.writes.is_empty());
    // It adds one itself, and the server takes it: that one is dated, and shared.
    device.kept.added("p1", &["s7".to_string()], NOW + 9);
    assert_eq!(device.exchange(&mut server), None);
    let added = server.record("r").added;
    assert_eq!(added["p1"]["s7"], Note { times: vec![NOW + 9], at: NOW + 9 });
    assert!(!added["p1"].contains_key("s9"));
    assert_eq!(added["p1"]["s1"], pc().added["p1"]["s1"]);
}

// ---- rule 7: the clock ----------------------------------------------------------------------------

#[test]
fn a_device_whose_clock_is_wrong_takes_in_and_shares_nothing() {
    for wrong in [3_600, -3_600, 301, -301] {
        let mut server = Server::with(&text(&pc()));
        let mut device = Dev::new();
        device.clock = NOW + wrong;
        device.does(Change::Pin("c".into()));
        device.log.push(("heard".into(), NOW + wrong - 10));
        assert_eq!(device.exchange(&mut server), None);
        // Nothing written; what the PC has is taken in; the change waits, and shows.
        assert!(server.writes.is_empty(), "{wrong}");
        assert_eq!(device.held, Some(Held::ClockWrong));
        assert_eq!(device.pins(), ["p1", "p2", "c"]);
        assert_eq!((device.choices.waiting.len(), device.choices.waiting[0].tried), (1, false));
        assert_eq!(device.log.len(), 3);
        // The clock put right: it goes, on top of the PC's, and nothing is held.
        device.clock = NOW;
        assert_eq!(device.exchange(&mut server), None);
        assert_eq!((server.record("r").pins, device.held), (pins(&["p1", "p2", "c"]), None));
    }
    // Five minutes exactly is allowed. A server that gives no date can't be compared with.
    for (clock, date) in [(NOW + 300, Some(NOW)), (NOW - 300, Some(NOW)), (NOW + 86_400, None)] {
        let mut server = Server::with(&text(&pc()));
        server.date = date;
        let mut device = Dev::new();
        device.clock = clock;
        device.does(Change::Pin("c".into()));
        assert_eq!(device.exchange(&mut server), None);
        assert_eq!((server.writes.len(), device.held), (1, None));
    }
    // Wrong, with nothing waiting, is still said.
    let mut server = Server::with(&text(&pc()));
    let mut device = Dev::new();
    device.clock = NOW + 3_600;
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!(device.held, Some(Held::ClockWrong));
}

// ---- devices together -----------------------------------------------------------------------------

#[test]
fn a_write_that_gets_no_answer_is_safe_to_make_again() {
    let mut server = Server::with(&text(&pc()));
    let mut device = Dev::new();
    device.log.push(("mine".into(), NOW - 5));
    device.history = plays(&[("mine", NOW - 5)]);
    // The write reaches the server and its answer is lost.
    let reads = device.read(&server);
    let plan = plan(device.decide(&reads));
    assert!(device.take(&plan));
    server.take(plan.write.as_ref().unwrap());
    let after = server.comment("r");
    // The next exchange reads what the server has, and finds nothing to add.
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!((server.writes.len(), server.comment("r")), (1, after));
}

#[test]
fn two_devices_interleaved_at_every_step_converge_and_lose_nothing() {
    // Each device has a change, a play and a dated song of its own. Their reads and writes
    // cross in every order; the server has no write that is conditional on what was read,
    // so the later of two crossed writes can go without the other's part. After one more
    // look by each, both hold everything, and so does the server.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Step {
        Read(usize),
        Write(usize),
    }
    use Step::{Read as R, Write as W};
    for order in [[R(0), W(0), R(1), W(1)], [R(0), R(1), W(0), W(1)], [R(0), R(1), W(1), W(0)], [R(1), R(0), W(0), W(1)], [R(1), R(0), W(1), W(0)], [R(1), W(1), R(0), W(0)]] {
        let mut server = Server::with(&text(&pc()));
        let mut devices = [in_step(&mut server), in_step(&mut server)];
        devices[0].does(Change::Pin("zero".into()));
        devices[0].log.push(("heard-on-zero".into(), NOW - 20));
        devices[0].kept.added("p1", &["added-on-zero".to_string()], NOW - 30);
        devices[1].does(Change::Sort { key: "liked".into(), sort: 2, descending: true });
        devices[1].log.push(("heard-on-one".into(), NOW - 10));
        devices[1].history = plays(&[("heard-on-one", NOW - 10)]);
        let mut plans: [Option<Plan>; 2] = [None, None];
        for step in order {
            match step {
                R(n) => {
                    let reads = devices[n].read(&server);
                    let decided = plan(devices[n].decide(&reads));
                    assert!(devices[n].take(&decided));
                    if let Some(write) = &decided.write {
                        assert!(devices[n].choices.sending(write));
                    }
                    plans[n] = Some(decided);
                }
                W(n) => {
                    if let Some(write) = plans[n].take().and_then(|plan| plan.write) {
                        server.take(&write);
                        devices[n].choices.written(&write);
                    }
                }
            }
        }
        // One more look each, twice round: by then nothing is left to write.
        for _ in 0..2 {
            for device in &mut devices {
                assert_eq!(device.exchange(&mut server), None);
            }
        }
        let writes = server.writes.len();
        for device in &mut devices {
            assert_eq!(device.exchange(&mut server), None);
        }
        assert_eq!(server.writes.len(), writes, "{order:?}: still writing");
        let record = server.record("r");
        assert_eq!(record.pins, ["p1", "p2", "zero"], "{order:?}");
        assert_eq!((record.lists["liked"].sort, record.lists["liked"].descending), (2, true), "{order:?}");
        assert!(record.plays.contains(&("heard-on-zero".to_string(), NOW - 20)) && record.plays.contains(&("heard-on-one".to_string(), NOW - 10)), "{order:?}");
        assert!(record.added["p1"].contains_key("added-on-zero") && record.added["p1"].contains_key("s1"), "{order:?}");
        assert_eq!(record.history[0], ("heard-on-one".to_string(), NOW - 10), "{order:?}");
        for device in &devices {
            assert_eq!(device.pins(), ["p1", "p2", "zero"], "{order:?}");
            assert_eq!(device.choices.shown(&device.account).lists["liked"].sort, 2, "{order:?}");
            assert_eq!(device.log.len(), 4, "{order:?}");
            assert!(device.choices.waiting.is_empty(), "{order:?}");
        }
    }
}

#[test]
fn what_is_taken_in_is_offered_back_with_the_time_it_came_with() {
    let mut server = Server::with(&text(&pc()));
    let mut device = in_step(&mut server);
    // The record is lost from the server (the playlist deleted by hand, and made again
    // empty). The device puts back what it holds, every item at the time it had.
    server.put("r", Some(""), Some("tester"));
    assert_eq!(device.exchange(&mut server), None);
    assert_eq!(server.record("r"), pc());
}
