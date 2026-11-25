//! Two devices, each a [`Device`] with a player that is a few lines of state, and a path
//! between them that can hold, repeat, hand back, throw away and delay what it carries:
//! the same things the walks do to two copies of the app, without the app.

use super::*;
use crate::link::rule::Rule;
use crate::requests::Snapshot;

/// A player: a queue of ids, where it is in it, and its order of choices.
#[derive(Default)]
struct Fake {
    rule: Rule,
    queue: Vec<String>,
    cursor: usize,
    playing: bool,
    position: f64,
    volume: f64,
    shuffle: bool,
    repeat: u8,
    now_ms: u64,
    /// When the sound started on what is in the player, if it has.
    started_ms: Option<u64>,
    /// The music server gives this device its songs. If not, nothing ever sounds.
    stream: bool,
    /// The sound has rolled into the next song and the window has not been told.
    rolled: bool,
    /// What was carried out for other devices, in order.
    did: Vec<String>,
}

impl Fake {
    fn with(queue: &[&str], playing: bool) -> Fake {
        let mut fake = Fake { queue: queue.iter().map(|id| id.to_string()).collect(), volume: 1.0, stream: true, ..Fake::default() };
        fake.play(playing);
        fake
    }

    fn play(&mut self, playing: bool) {
        self.playing = playing;
        self.started_ms = (playing && self.stream).then_some(self.now_ms);
    }

    /// The person presses Next at this device itself.
    fn own_next(&mut self) {
        if self.cursor + 1 < self.queue.len() {
            self.rule.moved();
            self.cursor += 1;
            self.position = 0.0;
            self.play(self.playing);
        }
    }
}

impl Player for Fake {
    fn brief(&self) -> Brief {
        Brief { playing: self.playing && !self.queue.is_empty(), title: self.queue.get(self.cursor).cloned().unwrap_or_default(), artist: String::new() }
    }
    fn full(&self) -> Full {
        let entry = |id: &String| crate::link::message::Entry { id: id.clone(), title: id.clone(), duration: 300, ..Default::default() };
        Full::fitted(self.position, self.queue.get(self.cursor).map(entry), self.queue.iter().skip(self.cursor + 1).map(entry), "", self.shuffle, self.repeat, self.volume)
    }
    fn handed(&self) -> Handed {
        Handed::fitted(&self.queue[self.cursor.min(self.queue.len())..], 0, self.position, "the queue")
    }
    fn sounding_at(&self) -> Option<f64> {
        self.started_ms.filter(|_| self.playing).map(|started| self.position + (self.now_ms - started) as f64 / 1000.0)
    }
    fn catch_up(&mut self) -> bool {
        if std::mem::take(&mut self.rolled) {
            self.rule.moved();
            self.cursor += 1;
            return true;
        }
        false
    }
    fn has_song(&self) -> bool {
        !self.queue.is_empty()
    }
    fn ahead(&self) -> Vec<String> {
        self.queue.iter().skip(self.cursor + 1).cloned().collect()
    }
    fn carry_out(&mut self, command: &Command, ready: &Ready) {
        self.did.push(command.name().to_string());
        match (command, ready) {
            (Command::Play, _) => self.play(true),
            (Command::Pause, _) => self.play(false),
            (Command::Next, _) => self.cursor = (self.cursor + 1).min(self.queue.len().saturating_sub(1)),
            (Command::Previous, _) => self.position = 0.0,
            (Command::Seek { to }, _) => self.position = *to,
            (Command::Volume { to }, _) => self.volume = *to,
            (Command::Jump { .. }, Ready::Entry(entry)) => {
                self.cursor += entry + 1;
                self.play(true);
            }
            (Command::Remove { .. }, Ready::Entry(entry)) => {
                self.queue.remove(self.cursor + 1 + entry);
            }
            (Command::Move { to, .. }, Ready::Entry(from)) => {
                let moved = self.queue.remove(self.cursor + 1 + from);
                self.queue.insert((self.cursor + 1 + *to as usize).min(self.queue.len()), moved);
            }
            (Command::Clear { .. }, _) => self.queue.truncate(self.cursor + 1),
            (Command::Shuffle { on }, _) => self.shuffle = *on,
            (Command::Repeat { mode }, _) => self.repeat = *mode,
            (Command::Add { .. }, Ready::Songs(songs)) => self.queue.extend(songs.iter().cloned()),
            (Command::Adopt(adopt), Ready::Queue(songs, begin)) => {
                self.queue = songs.iter().map(|(id, _)| id.clone()).collect();
                self.cursor = *begin;
                self.position = adopt.position;
                self.play(true);
            }
            other => panic!("carried out with the wrong thing found: {other:?}"),
        }
    }
    fn pause(&mut self) {
        self.rule.moved();
        self.play(false);
    }
    fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, _context: &str) {
        self.rule.moved();
        self.queue = songs.iter().map(|(id, _)| id.clone()).collect();
        self.cursor = begin;
        self.position = position;
        self.play(true);
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

/// One of the two: its deciding, its player, and what it was told to do and say.
struct Copy {
    device: Device,
    player: Fake,
    me: Me,
    library: Snapshot,
    session: u64,
    place: Option<Place>,
    places: u8,
    randoms: u64,
    said: Vec<String>,
    log: Vec<String>,
    /// The device its window shows, if not itself.
    window: Option<String>,
    looks: Vec<(u64, bool)>,
    later: Vec<u64>,
    remembered: Vec<Remembered>,
    ended: Vec<u64>,
}

#[derive(Clone)]
struct Frame {
    from: Place,
    to: Place,
    text: String,
}

/// What the path between them does with what it carries.
#[derive(Default)]
struct Path {
    hold_commands: bool,
    hold_tells: bool,
    eat_tells: bool,
    commands: Vec<Frame>,
    tells: Vec<Frame>,
    last_command: Option<Frame>,
}

struct World {
    copies: [Copy; 2],
    now: u64,
    path: Path,
    /// The songs the music server has (asked when a device's own copy of the library lacks one).
    server: Vec<String>,
}

const A: usize = 0;
const B: usize = 1;
const SONGS: [&str; 12] = ["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "a0", "a1", "a2", "x1"];

fn secret(n: u8) -> Secret {
    Secret::new([n; 32])
}

impl World {
    /// Two devices signed in to one account, each in the room and knowing of the other. B
    /// has eight songs in its player; A has three of its own.
    fn new(b_playing: bool) -> World {
        let copy = |n: u8, name: &str, player: Fake| Copy {
            device: Device::default(),
            player,
            me: Me { device: [n; 16], name: name.into(), here: Here::Pc },
            library: Snapshot { ids: SONGS.iter().map(|id| id.to_string()).collect(), ..Default::default() },
            session: 0,
            place: None,
            places: n * 16,
            randoms: u64::from(n) << 56,
            said: Vec::new(),
            log: Vec::new(),
            window: None,
            looks: Vec::new(),
            later: Vec::new(),
            remembered: Vec::new(),
            ended: Vec::new(),
        };
        let mut world = World {
            copies: [copy(1, "Copy A", Fake::with(&["a0", "a1", "a2"], false)), copy(2, "Copy B", Fake::with(&["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"], b_playing))],
            now: 1_000,
            path: Path::default(),
            server: Vec::new(),
        };
        // (The first tick asks for a look; its answer begins the session.)
        world.tick();
        for who in [A, B] {
            world.looked(who, Looked::Secret { secret: secret(7), id: "k".into(), wrote: None }, false);
        }
        world.hear_places();
        world.pass(2_000);
        world
    }

    fn with<T>(&mut self, who: usize, act: impl FnOnce(&mut Device, &mut Env) -> T) -> T {
        let now = self.now;
        let Copy { device, player, me, library, randoms, .. } = &mut self.copies[who];
        player.now_ms = now;
        let mut random = |bytes: &mut [u8]| {
            *randoms += 1;
            for (at, byte) in bytes.iter_mut().enumerate() {
                *byte = randoms.to_le_bytes()[at % 8] ^ (at as u8).wrapping_mul(37);
            }
            true
        };
        let mut env = Env { now_ms: now, today: 100, me, player, library, random: &mut random };
        act(device, &mut env)
    }

    /// Does what a device asked for.
    fn act(&mut self, who: usize, asked: Vec<Do>) {
        // (What is to be sent goes when the rest has been done: a send is not answered
        // before the device that asked for it has finished what it was doing.)
        let mut to_send = Vec::new();
        for ask in asked {
            match ask {
                Do::Log(line) => self.copies[who].log.push(line),
                Do::Say(words) => self.copies[who].said.push(words),
                Do::Look { number, reset, .. } => self.copies[who].looks.push((number, reset)),
                Do::StopLook | Do::Renew { .. } | Do::Forget { .. } | Do::Draw => {}
                Do::Begin { session, .. } => {
                    // (The relay gives it a place, new for each session.)
                    let copy = &mut self.copies[who];
                    copy.places += 1;
                    (copy.session, copy.place) = (session, Some([copy.places; 16]));
                    copy.device.entered(session, [copy.places; 16], "a-token".into());
                }
                Do::End { session } => {
                    let copy = &mut self.copies[who];
                    copy.ended.push(session);
                    if copy.session == session {
                        copy.place = None;
                    }
                }
                Do::Send { session, to, frame, .. } => {
                    // (Queued under a session that has ended: not sent.)
                    if let (true, Some(from)) = (self.copies[who].session == session, self.copies[who].place) {
                        to_send.push(Frame { from, to, text: frame });
                    }
                }
                Do::Find { ticket, ids } => {
                    let has = self.copies[who].library.has_songs(&ids);
                    let found: Vec<String> = ids.iter().zip(has).filter(|(id, has)| *has || self.server.contains(id)).map(|(id, _)| id.clone()).collect();
                    let did = self.with(who, |device, env| device.found(ticket, found, env));
                    self.act(who, did);
                }
                Do::Later { ticket, .. } => self.copies[who].later.push(ticket),
                Do::Remember(list) => self.copies[who].remembered = list,
                Do::Steering(name) => self.copies[who].window = name,
            }
        }
        for frame in to_send {
            self.carry(frame);
        }
    }

    /// What the path's own eyes can tell of a frame: an ask or a tell, and of an ask
    /// whether it is long enough to be a command. (It cannot read one.)
    fn kind(frame: &Frame) -> &'static str {
        let bytes = text::from_base64(&frame.text).unwrap_or_default();
        match bytes.get(4) {
            Some(2) => "tell",
            Some(1) if bytes.len() - 45 > 30 => "command",
            _ => "look",
        }
    }

    fn carry(&mut self, frame: Frame) {
        match World::kind(&frame) {
            "command" if self.path.hold_commands => self.path.commands.push(frame),
            "command" => {
                self.path.last_command = Some(frame.clone());
                self.deliver(frame);
            }
            "tell" if self.path.eat_tells => {}
            "tell" if self.path.hold_tells => self.path.tells.push(frame),
            _ => self.deliver(frame),
        }
    }

    /// Hands a frame to whichever device has the place it is for. (One for a place nobody
    /// has is the relay's to refuse.)
    fn deliver(&mut self, frame: Frame) {
        if let Some(who) = [A, B].into_iter().find(|who| self.copies[*who].place == Some(frame.to)) {
            self.deliver_to(who, frame);
        }
    }

    fn deliver_to(&mut self, who: usize, frame: Frame) {
        let session = self.copies[who].session;
        let places = self.room();
        let did = self.with(who, |device, env| device.heard(session, &places, &[(text::place(&frame.from), frame.text.clone())], env));
        self.act(who, did);
    }

    fn room(&self) -> Vec<String> {
        // (Both devices are in one room only while they hold one secret; the tests that
        // split them say who hears of whom themselves.)
        self.copies.iter().filter_map(|copy| copy.place).map(|place| text::place(&place)).collect()
    }

    fn hear_places(&mut self) {
        let places = self.room();
        for who in [A, B] {
            let session = self.copies[who].session;
            let did = self.with(who, |device, env| device.heard(session, &places, &[], env));
            self.act(who, did);
        }
    }

    fn looked(&mut self, who: usize, looked: Looked, reset: bool) {
        let (number, now) = (self.copies[who].looks.last().map_or(0, |(number, _)| *number), self.now);
        let did = self.with(who, |device, env| device.looked(number, now, looked, reset, env));
        self.act(who, did);
    }

    fn tick(&mut self) {
        for who in [A, B] {
            let did = self.with(who, |device, env| device.tick("the account", true, env));
            self.act(who, did);
        }
    }

    /// Time passes, a quarter of a second at a tick.
    fn pass(&mut self, ms: u64) {
        let until = self.now + ms;
        while self.now < until {
            self.now = (self.now + 250).min(until);
            self.tick();
        }
    }

    fn release(&mut self) {
        for frame in std::mem::take(&mut self.path.commands) {
            self.path.last_command = Some(frame.clone());
            self.deliver(frame);
        }
    }

    fn release_tells(&mut self) {
        for frame in std::mem::take(&mut self.path.tells) {
            self.deliver(frame);
        }
    }

    fn b_place(&self) -> String {
        text::place(&self.copies[B].place.unwrap())
    }

    /// A picks B in its list.
    fn choose(&mut self) {
        let place = self.b_place();
        let did = self.with(A, |device, env| device.choose(&place, false, env));
        self.act(A, did);
        self.pass(500);
    }

    fn press(&mut self, press: Press) -> bool {
        let did = self.with(A, |device, env| device.press(press, env));
        let steering = did.is_some();
        self.act(A, did.unwrap_or_default());
        steering
    }

    /// Does in A's window the thing that asks B for this command.
    fn ask(&mut self, name: &str) {
        let ids = |ids: &[&str]| -> Vec<String> { ids.iter().map(|id| id.to_string()).collect() };
        let did = match name {
            "add" => self.with(A, |device, env| device.add(&ids(&["x1"]), false, env)),
            "adopt" => self.with(A, |device, env| device.play(&ids(&["a1", "a2"]), 0, "an album", env)),
            _ => {
                let press = match name {
                    "play" | "pause" => Press::Toggle,
                    "next" => Press::Next,
                    "previous" => Press::Previous,
                    "seek" => Press::Seek(20.0),
                    "volume" => Press::Volume(0.3),
                    "jump" => Press::Jump(1),
                    "move" => Press::Move(2, 0, true),
                    "remove" => Press::Remove(1),
                    "clear" => Press::Clear(false),
                    "shuffle" => Press::Shuffle,
                    "repeat" => Press::Repeat,
                    other => panic!("no way to ask for {other}"),
                };
                self.with(A, |device, env| device.press(press, env))
            }
        };
        self.act(A, did.expect("A is steering B"));
    }

    fn log(&self, who: usize, has: &str) -> Vec<&String> {
        self.copies[who].log.iter().filter(|line| line.contains(has)).collect()
    }

    fn last(&self, who: usize, has: &str) -> String {
        self.log(who, has).last().map(|line| line.to_string()).unwrap_or_default()
    }

    fn row(&mut self, who: usize, name: &str) -> Option<Row> {
        let now = self.now;
        let (shown, did) = self.copies[who].device.shown(now);
        self.act(who, did);
        shown.rows.into_iter().find(|row| row.name == name)
    }
}

const COMMANDS: [&str; 14] = ["play", "pause", "next", "previous", "seek", "volume", "jump", "move", "remove", "clear", "shuffle", "repeat", "add", "adopt"];

/// A steering B, with B as this command needs it and A's screen showing B as it is.
fn steering(name: &str) -> World {
    let mut world = World::new(name == "pause");
    world.choose();
    world.pass(1_000);
    assert_eq!(world.copies[A].window.as_deref(), Some("Copy B"));
    assert!(world.copies[A].device.view().is_some(), "A has not been shown B's player");
    world
}

#[test]
fn two_devices_find_each_other_and_each_shows_what_the_other_plays() {
    let mut world = World::new(true);
    assert_eq!(world.row(A, "Copy B").map(|row| (row.about, row.playing, row.ready, row.active)), Some(("Playing s0".to_string(), true, true, false)));
    assert_eq!(world.row(B, "Copy A").map(|row| row.about), Some("Paused on a0".to_string()));
    assert_eq!(world.last(A, "shown:"), "shown: Copy B: Playing s0");
    // Each has remembered the other by name.
    assert_eq!(world.copies[A].remembered.iter().map(|seen| seen.name.as_str()).collect::<Vec<_>>(), ["Copy B"]);
}

#[test]
fn every_command_is_carried_out_as_asked_once() {
    for name in COMMANDS {
        let mut world = steering(name);
        let before = world.copies[B].player.rule.generation();
        world.ask(name);
        world.pass(1_000);
        assert!(world.log(B, &format!("{name} from")).iter().any(|line| line.ends_with("carried out")), "{name}: {:?}", world.copies[B].log);
        assert_eq!(world.copies[B].player.did, [name], "{name}");
        // It took one place in B's order of choices, and no more for what the player did.
        assert_eq!(world.copies[B].player.rule.generation(), before + 1, "{name}");
        assert!(world.last(A, "outcome of").ends_with("Done"), "{name}: {:?}", world.copies[A].log);
        // And what it did: a few, by what the player then holds.
        let player = &world.copies[B].player;
        match name {
            "play" => assert!(player.playing),
            "pause" => assert!(!player.playing),
            "next" => assert_eq!(player.cursor, 1),
            "seek" => assert_eq!(player.position, 20.0),
            "volume" => assert_eq!(player.volume, 0.3),
            "jump" => assert_eq!((player.cursor, player.playing), (2, true)),
            "move" => assert_eq!(&player.queue[..4], ["s0", "s3", "s1", "s2"]),
            "remove" => assert_eq!(&player.queue[..3], ["s0", "s1", "s3"]),
            "clear" => assert_eq!(player.queue, ["s0"]),
            "shuffle" => assert!(player.shuffle),
            "repeat" => assert_eq!(player.repeat, 1),
            "add" => assert_eq!(player.queue.last().map(String::as_str), Some("x1")),
            "adopt" => assert_eq!((player.queue.clone(), player.playing), (vec!["a1".to_string(), "a2".to_string()], true)),
            _ => {}
        }
        if name == "add" {
            assert_eq!(world.copies[A].said, ["Added to the queue on Copy B"]);
        }
    }
}

#[test]
fn every_command_delivered_again_is_refused_and_handed_back_does_not_open() {
    for name in COMMANDS {
        let mut world = steering(name);
        world.ask(name);
        world.pass(1_000);
        let frame = world.path.last_command.clone().expect("a command was carried");
        // The path delivers the same frame again.
        world.deliver(frame.clone());
        assert!(world.last(B, &format!("{name} from")).contains("refused, Changed"), "{name}: {:?}", world.copies[B].log);
        assert_eq!(world.copies[B].player.did, [name], "{name} was carried out twice");
        // And hands it back to its sender, as if from the device it was for.
        world.deliver_to(A, Frame { from: frame.to, to: frame.from, text: frame.text });
        assert!(world.last(A, "does not open").contains("dropped"), "{name}");
    }
}

#[test]
fn every_command_is_refused_once_something_is_chosen_at_the_device() {
    for name in COMMANDS {
        let mut world = steering(name);
        world.path.hold_commands = true;
        world.ask(name);
        world.copies[B].player.own_next();
        world.release();
        assert!(world.last(B, &format!("{name} from")).contains("refused, Changed"), "{name}: {:?}", world.copies[B].log);
        assert!(world.copies[B].player.did.is_empty(), "{name}");
        assert_eq!(world.copies[A].said.last().map(String::as_str), Some("Copy B changed before that reached it. Nothing was done."), "{name}");
    }
}

#[test]
fn every_command_is_refused_while_the_device_is_busy() {
    for name in COMMANDS {
        let mut world = steering(name);
        world.path.hold_commands = true;
        world.ask(name);
        // Something chosen at B is being looked up.
        let now = world.now;
        let waiting = world.copies[B].player.rule.wait(now, 60_000);
        world.release();
        assert!(world.last(B, &format!("{name} from")).contains("refused, Busy") && world.last(B, &format!("{name} from")).ends_with(", busy)"), "{name}: {:?}", world.copies[B].log);
        assert!(world.copies[B].player.did.is_empty(), "{name}");
        assert_eq!(world.copies[A].said.last().map(String::as_str), Some("Copy B was in the middle of something. Nothing was done."), "{name}");
        // While it is busy A's list says so, and nothing is sent against that.
        world.pass(6_000);
        assert_eq!(world.copies[A].device.view().map(|_| ()), Some(()));
        world.copies[B].player.rule.done(waiting);
    }
}

#[test]
fn every_command_is_refused_when_it_arrives_more_than_two_minutes_late() {
    for name in COMMANDS {
        let mut world = steering(name);
        world.path.hold_commands = true;
        world.ask(name);
        world.pass(14_000);
        // Twelve seconds unanswered: A looked once, and B was as it had been.
        assert_eq!(world.copies[A].said.last().map(String::as_str), Some("That hadn't reached Copy B when checked."), "{name}");
        world.pass(111_000);
        world.release();
        assert!(world.last(B, &format!("{name} from")).contains("refused, Late"), "{name}: {:?}", world.copies[B].log);
        assert!(world.copies[B].player.did.is_empty(), "{name}");
        assert_eq!(world.copies[A].said.last().map(String::as_str), Some("That took too long to reach Copy B. Nothing was done."), "{name}");
    }
    // One held less than that, with nothing touched, is carried out when it arrives; and
    // "it hadn't reached" is put right.
    let mut world = steering("next");
    world.path.hold_commands = true;
    world.ask("next");
    world.pass(40_000);
    world.release();
    assert_eq!(world.copies[B].player.did, ["next"]);
    assert!(world.last(A, "outcome of").ends_with("Done"));
}

// ---- every way into the player, as the deciding sees it ------------------------------------

/// A command to B is held on the path while something happens at B, and then delivered.
fn held_while(name: &str, at_b: impl FnOnce(&mut World)) -> World {
    let mut world = steering(name);
    world.path.hold_commands = true;
    world.ask(name);
    at_b(&mut world);
    world.release();
    world.path.hold_commands = false;
    world
}

#[test]
fn a_press_that_changes_nothing_takes_no_place_and_one_that_changes_something_does() {
    // Next with nothing to go to, Play with nothing loaded: the order of choices is as it
    // was, and a command made before the press is carried out after it.
    let world = held_while("pause", |_| {});
    assert_eq!(world.copies[B].player.did, ["pause"]);
    // Anything that changes what plays or is queued (a press, a key, the tray, a media
    // key, a song that would not play being passed over, a timer's pause, the queue
    // followed, the sound output lost): the app counts it, and the command is refused.
    let world = held_while("pause", |world| world.copies[B].player.rule.moved());
    assert!(world.last(B, "pause from").contains("refused, Changed"));
}

#[test]
fn a_choice_whose_songs_are_being_looked_up_makes_the_device_busy_until_they_arrive() {
    // Play on an album, Play Next, Add to Queue, a radio, a guest's request in a jam, a
    // device linked the old way, autoplay's songs when a queue has run out: from the choice
    // until its work is done the device is busy, and afterwards it has changed.
    let mut waiting = None;
    let mut world = held_while("next", |world| waiting = Some(world.copies[B].player.rule.wait(world.now, 60_000)));
    assert!(world.last(B, "next from").contains("refused, Busy"));
    assert_eq!(world.row(A, "Copy B").map(|row| row.active), Some(true));
    // The look-up answers: what was chosen is carried out, which moves B on.
    world.copies[B].player.rule.done(waiting.unwrap());
    world.copies[B].player.rule.moved();
    world.pass(6_000);
    world.ask("next");
    world.pass(1_000);
    assert_eq!(world.copies[B].player.did, ["next"], "{:?} {:?} {:?}", world.copies[A].said, world.copies[A].log, world.copies[B].log);
    // A look-up that never answers stops counting after a minute.
    let mut world = steering("next");
    let now = world.now;
    world.copies[B].player.rule.wait(now, 60_000);
    world.pass(6_000);
    world.ask("next");
    assert_eq!(world.copies[A].said.last().map(String::as_str), Some("Copy B is in the middle of something. Press again in a moment."));
    world.pass(60_000);
    world.ask("next");
    world.pass(1_000);
    assert_eq!(world.copies[B].player.did, ["next"]);
}

#[test]
fn a_command_that_lands_as_the_sound_rolls_into_the_next_song_is_refused() {
    // The sound is on the next song and B's window has not been told: the window is
    // brought up to the sound before the command is judged.
    for name in ["seek", "previous", "next", "pause"] {
        let world = held_while(name, |world| world.copies[B].player.rolled = true);
        assert!(world.last(B, &format!("{name} from")).contains("refused, Changed"), "{name}");
        assert_eq!((world.copies[B].player.cursor, world.copies[B].player.did.len()), (1, 0), "{name}");
        assert_eq!(world.log(B, "the sound has rolled into the next song").len(), 1);
    }
    // A look that lands then is answered with the song that is sounding.
    let mut world = steering("next");
    world.copies[B].player.rolled = true;
    world.pass(6_000);
    assert_eq!(world.copies[A].device.view().and_then(|view| view.full.now.as_ref()).map(|song| song.id.as_str()), Some("s1"));
}

#[test]
fn a_command_that_cannot_be_done_changes_nothing_and_leaves_the_device_free() {
    // An entry that has gone from the queue; nothing in the player; songs nobody has.
    let mut world = steering("remove");
    world.path.hold_commands = true;
    world.ask("remove");
    // (Its queue is emptied behind the order's back, as no way into the player does.)
    world.copies[B].player.queue.truncate(1);
    world.release();
    assert!(world.last(B, "remove from").contains("refused, Cannot"));
    assert_eq!(world.copies[A].said.last().map(String::as_str), Some("Copy B couldn't do that."));
    assert!(!world.copies[B].player.rule.busy(world.now));
    let mut world = steering("next");
    world.path.hold_commands = true;
    world.ask("next");
    world.copies[B].player.queue.clear();
    world.release();
    assert!(world.last(B, "next from").contains("refused, Cannot"));
    // A queue of songs B's library has none of is not taken.
    let mut world = steering("adopt");
    world.copies[B].library.ids.retain(|id| id.starts_with('s'));
    world.ask("adopt");
    assert!(world.last(B, "adopt from").contains("refused, Cannot"));
    assert_eq!(world.copies[B].player.queue.len(), 8);
}

#[test]
fn songs_a_device_s_own_copy_of_the_library_lacks_are_asked_of_the_music_server() {
    // B's copy has not caught up with a song added today; the music server has it.
    let mut world = steering("add");
    world.copies[B].library.ids.retain(|id| id != "x1");
    world.server = vec!["x1".into()];
    world.ask("add");
    world.pass(1_000);
    assert_eq!(world.copies[B].player.queue.last().map(String::as_str), Some("x1"));
    // Nobody has it: nothing is queued, and B is not left busy.
    let mut world = steering("add");
    world.copies[B].library.ids.retain(|id| id != "x1");
    world.ask("add");
    world.pass(1_000);
    assert!(world.last(B, "add from").contains("refused, Cannot"));
    assert!(!world.copies[B].player.rule.busy(world.now) && world.copies[B].player.did.is_empty());
}

#[test]
fn presses_made_while_one_is_unanswered_go_one_at_a_time() {
    let mut world = steering("next");
    world.path.hold_commands = true;
    world.ask("next");
    world.ask("next");
    world.ask("volume");
    // Only the first is on the path.
    assert_eq!(world.path.commands.len(), 1);
    world.path.hold_commands = false;
    world.release();
    world.pass(1_000);
    assert_eq!(world.copies[B].player.did, ["next", "next", "volume"]);
    assert_eq!(world.copies[B].player.cursor, 2);
}

#[test]
fn a_press_is_the_device_s_own_unless_another_is_being_steered() {
    let mut world = World::new(true);
    assert!(!world.press(Press::Next), "a press went to another device with none being steered");
    assert!(world.with(A, |device, env| device.add(&["s1".to_string()], false, env)).is_none());
    assert!(world.with(A, |device, env| device.play(&["s1".to_string()], 0, "", env)).is_none());
    assert!(world.with(A, |device, env| device.play_here(env)).is_none());
    world.choose();
    world.pass(1_000);
    assert!(world.press(Press::Next));
    // Radio stations are not something to queue on another device.
    let did = world.with(A, |device, env| device.add(&[String::new()], false, env));
    assert_eq!(did, Some(vec![Do::Say(words::NOT_RADIO.into())]));
    // "Stop steering" lets go, and leaves B as it is.
    let did = world.copies[A].device.let_go();
    world.act(A, did);
    assert_eq!(world.copies[A].window, None);
    assert!(!world.press(Press::Next) && world.copies[B].player.playing);
}

// ---- news, the list, and the session ---------------------------------------------------------

#[test]
fn a_device_that_changes_is_looked_at_then_and_not_at_its_next_turn() {
    let mut world = World::new(false);
    world.pass(3_000);
    // B is played at B itself.
    world.copies[B].player.rule.moved();
    world.copies[B].player.play(true);
    world.pass(500);
    assert_eq!(world.row(A, "Copy B").map(|row| row.about), Some("Playing s0".to_string()));
    // News delivered again later is a reason to look, and what is shown is the answer.
    world.copies[B].player.rule.moved();
    world.copies[B].player.play(false);
    world.pass(5_000);
    assert_eq!(world.row(A, "Copy B").map(|row| row.about), Some("Paused on s0".to_string()));
}

#[test]
fn a_device_seen_before_is_listed_when_it_is_not_here() {
    let mut world = World::new(true);
    let remembered = world.copies[A].remembered.clone();
    // A is started again, alone.
    let mut alone = Device::new(remembered);
    let (shown, _) = alone.shown(0);
    assert_eq!(shown.rows, [Row { place: String::new(), name: "Copy B".into(), about: "Looking for it…".into(), playing: false, active: false, ready: false }]);
    // B leaves the room A is in.
    world.copies[B].place = None;
    world.hear_places();
    let row = world.row(A, "Copy B").unwrap();
    assert_eq!((row.about.as_str(), row.ready, row.place.as_str()), ("Not here just now", false, ""));
}

#[test]
fn a_device_being_steered_that_goes_is_waited_for_and_then_let_go() {
    let mut world = steering("next");
    world.copies[B].place = None;
    world.hear_places();
    world.pass(44_000);
    assert_eq!(world.copies[A].window.as_deref(), Some("Copy B"));
    // What is pressed meanwhile has nowhere to go, and is still not A's own.
    assert!(world.press(Press::Next));
    world.pass(2_000);
    assert_eq!(world.copies[A].window, None);
    assert_eq!(world.copies[A].said.last().map(String::as_str), Some("Copy B can't be reached any more, so the player is back on this PC."));
}

#[test]
fn link_my_devices_off_and_signing_out_end_everything_at_once() {
    for end in ["setting", "sign-out", "another account"] {
        let mut world = steering("next");
        world.path.hold_commands = true;
        world.ask("next");
        let session = world.copies[A].session;
        let did = match end {
            "setting" => world.copies[A].device.setting(false),
            "sign-out" => world.copies[A].device.stop(),
            _ => world.with(A, |device, env| device.tick("somebody else", true, env)),
        };
        // The session is over, the window is A's own, and a look under way is told to stop.
        assert!(did.contains(&Do::End { session }) && did.contains(&Do::Steering(None)), "{end}: {did:?}");
        world.act(A, did);
        assert!(!world.copies[A].device.is_live(session, world.now), "{end}");
        // What arrives for the old session afterwards is nobody's.
        let before = world.copies[A].log.len();
        let did = world.with(A, |device, env| device.heard(session, &[], &[], env));
        assert!(did.is_empty() && world.copies[A].log.len() == before, "{end}");
        if end == "setting" {
            assert!(world.copies[A].device.setting(true).is_empty());
        }
    }
    // A look that was on its way is stopped, and its answer starts nothing.
    let mut device = Device::default();
    let mut world = World::new(false);
    let did = world.with(A, |_, env| device.tick("the account", true, env));
    // (Its first tick is for a sign-in it has not seen before: the look is that sign-in's.)
    assert!(matches!(did.as_slice(), [Do::Look { number: 1, reset: false, .. }]), "{did:?}");
    assert_eq!(device.stop(), [Do::StopLook]);
    let did = world.with(A, |_, env| device.looked(1, 0, Looked::Secret { secret: secret(7), id: "k".into(), wrote: None }, false, env));
    assert!(did.is_empty());
}

#[test]
fn a_secret_is_used_for_ten_minutes_and_a_look_that_reads_it_again_renews_it() {
    let mut world = World::new(false);
    let session = world.copies[A].session;
    // Two minutes after the last look finished, the next is asked for.
    world.pass(crate::link::LOOK_AGAIN_MS);
    assert_eq!(world.copies[A].looks.len(), 2);
    world.looked(A, Looked::Secret { secret: secret(7), id: "k".into(), wrote: None }, false);
    assert_eq!(world.last(A, "look:"), "look: the secret is as it was");
    assert!(world.copies[A].device.is_live(session, world.now + crate::link::SECRET_LIFE_MS - 1));
    // The music server stops answering: looks conclude nothing, and ten minutes after the
    // last one that read the secret began, the session ends.
    for _ in 0..5 {
        world.pass(crate::link::LOOK_AGAIN_MS);
        world.looked(A, Looked::Failed, false);
    }
    assert_eq!(world.copies[A].ended, [session]);
    let now = world.now;
    assert_eq!(world.copies[A].device.shown(now).0.note, words::SERVER_SILENT);
    // A record found to be no good ends it there and then.
    let session = world.copies[B].session;
    world.looked(B, Looked::Invalid, false);
    assert_eq!(world.copies[B].ended, [session]);
    assert_eq!(world.copies[B].device.shown(now).0.note, words::NEEDS_RESETTING);
    let mut world = World::new(false);
    world.looked(A, Looked::Stopped(secret::Stop::HasSongs), false);
    assert_eq!(world.copies[A].ended.len(), 1);
    // With no random bytes to be had, linking is off for the run.
    let mut world = World::new(false);
    world.looked(A, Looked::NoRandom, false);
    let looks = world.copies[A].looks.len();
    world.pass(crate::link::LOOK_AGAIN_MS * 3);
    assert_eq!(world.copies[A].looks.len(), looks);
    let now = world.now;
    assert_eq!(world.copies[A].device.shown(now).0.note, "This PC couldn't make random numbers, so linking is off.");
}

// ---- moving music ------------------------------------------------------------------------------

/// A playing its own three songs, B paused on its eight; B is picked in A's list.
fn giving(prepare: impl FnOnce(&mut World)) -> World {
    let mut world = World::new(false);
    world.copies[A].player.play(true);
    world.pass(3_000);
    prepare(&mut world);
    world.choose();
    world
}

fn said(world: &World) -> &str {
    world.copies[A].said.last().map_or("", String::as_str)
}

#[test]
fn music_given_to_another_device_starts_there_and_then_stops_here() {
    let mut world = giving(|_| {});
    world.pass(1_000);
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Playing on Copy B.", false, true));
    assert_eq!((world.copies[B].player.queue.clone(), world.copies[A].window.as_deref()), (vec!["a0".to_string(), "a1".into(), "a2".into()], Some("Copy B")));
    assert_eq!(world.last(B, "adopt from"), format!("adopt from {}: its sound has started", text::place(&world.copies[A].place.unwrap())));
    // B said "carried out" only once it sounded; until then it was busy.
    assert_eq!(world.log(B, "adopt from").len(), 2);
}

#[test]
fn a_queue_held_on_the_path_is_said_not_to_have_arrived_and_completes_if_it_then_does_in_time() {
    let mut world = giving(|world| world.path.hold_commands = true);
    world.pass(14_000);
    assert_eq!((said(&world), world.copies[A].player.playing), ("That hadn't reached Copy B when checked. Still playing here; Copy B may yet start.", true));
    world.release();
    world.pass(1_000);
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Playing on Copy B.", false, true));
    // Past thirty seconds, this one does not stop by itself.
    let mut world = giving(|world| world.path.hold_commands = true);
    world.pass(40_000);
    world.release();
    world.pass(1_000);
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Copy B has started, too late for this PC to stop by itself. Both are playing.", true, true));
    assert_eq!(world.copies[A].window, None);
}

#[test]
fn an_answer_that_is_lost_or_late_is_made_up_for_by_looking() {
    // Lost: A looks at twelve seconds, and B's state names the command and is not busy.
    let mut world = giving(|world| world.path.eat_tells = true);
    world.pass(6_000);
    world.path.eat_tells = false;
    world.pass(8_000);
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Playing on Copy B.", false, true));
    // Held past everything: not known, said so; and when it comes, too late to stop here.
    let mut world = giving(|world| world.path.hold_tells = true);
    world.pass(26_000);
    assert_eq!((said(&world), world.copies[A].player.playing), ("Copy B didn't answer. Still playing here; check whether Copy B started too.", true));
    world.pass(10_000);
    world.path.hold_tells = false;
    world.release_tells();
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Copy B has started, too late for this PC to stop by itself. Both are playing.", true, true));
}

#[test]
fn a_device_that_was_touched_meanwhile_is_left_as_it_is() {
    // This one: Next is pressed here while the other's answer is on its way.
    let mut world = giving(|world| world.path.hold_tells = true);
    world.copies[A].player.own_next();
    world.path.hold_tells = false;
    world.release_tells();
    assert_eq!(said(&world), "Copy B has started. This PC was given something else to do meanwhile, so it was left as it is: both may be playing.");
    assert!(world.copies[A].player.playing && world.copies[A].window.is_none());
    // The other: Next is pressed there after it took the queue and before it sounded.
    let mut world = giving(|world| world.copies[B].player.stream = false);
    world.copies[B].player.own_next();
    world.pass(1_000);
    assert_eq!((said(&world), world.copies[A].player.playing), ("Copy B was given something else to do before it started. Still playing here.", true));
    assert_eq!(world.last(B, "adopt from"), format!("adopt from {}: it did not start", text::place(&world.copies[A].place.unwrap())));
}

#[test]
fn a_device_whose_sound_never_starts_pauses_and_says_it_could_not() {
    let mut world = giving(|world| world.copies[B].player.stream = false);
    // Until it gives up it is busy, and names the command: not done yet.
    world.pass(14_000);
    assert!(world.copies[B].player.rule.busy(world.now) && world.copies[A].player.playing && world.copies[A].said.len() == 1);
    world.pass(8_000);
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Copy B couldn't play that. Still playing here.", true, false), "{:?} {:?}", world.copies[A].log, world.copies[B].log);
    assert!(!world.copies[B].player.rule.busy(world.now));
    // And one that is busy, or has none of the songs, is asked nothing more.
    let mut world = giving(|world| world.copies[B].library.ids.clear());
    world.pass(1_000);
    assert_eq!(said(&world), "Copy B couldn't play that. Still playing here.");
    assert_eq!(world.copies[B].player.queue.len(), 8);
}

/// A steering B, which is playing; "Play here" is pressed on A.
fn taking(prepare: impl FnOnce(&mut World)) -> World {
    let mut world = steering("pause");
    prepare(&mut world);
    let did = world.with(A, |device, env| device.play_here(env));
    world.act(A, did.expect("A is steering B"));
    world
}

#[test]
fn music_taken_from_another_device_starts_here_and_then_stops_there() {
    let mut world = taking(|_| {});
    world.pass(1_000);
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Playing here. Copy B is paused.", true, false));
    assert_eq!((world.copies[A].player.queue.len(), world.copies[A].window.clone()), (8, None));
    // The pause quoted the state the queue was taken from.
    assert!(world.last(A, "take:").contains("asked Copy B to pause, against generation 1"));
}

#[test]
fn a_taking_that_is_interrupted_leaves_the_other_playing_and_says_so() {
    // Its queue never comes.
    let mut world = taking(|world| world.path.eat_tells = true);
    world.pass(13_000);
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Copy B didn't answer, so nothing was moved. It may still be playing.", false, true));
    assert_eq!(world.copies[A].window, None);
    // This device's sound never starts.
    let mut world = taking(|world| world.copies[A].player.stream = false);
    world.pass(31_000);
    assert_eq!((said(&world), world.copies[B].player.playing), ("This PC didn't start in time, so Copy B was left playing.", true));
    // Pause is pressed here before the first sound.
    let mut world = taking(|world| world.copies[A].player.stream = false);
    world.copies[A].player.pause();
    world.pass(1_000);
    assert_eq!((said(&world), world.copies[B].player.playing), ("Something else was chosen here, so Copy B was left as it is: it is still playing.", true));
    // This device has none of the songs.
    let mut world = taking(|world| world.copies[A].library.ids.clear());
    world.pass(1_000);
    assert_eq!((said(&world), world.copies[A].player.queue.len()), ("This PC's library has none of those songs. Copy B is still playing.", 3));
    // The other is not playing anything: it is just let go of.
    let mut world = steering("next");
    let did = world.with(A, |device, env| device.play_here(env));
    world.act(A, did.unwrap());
    assert_eq!((world.copies[A].window.clone(), world.copies[A].player.queue.len(), world.copies[A].said.len()), (None, 3, 0));
}

#[test]
fn the_pause_that_completes_a_taking_is_judged_like_any_other() {
    // Held on the path while Next is pressed at the other.
    let mut world = taking(|world| world.path.hold_commands = true);
    world.pass(1_000);
    world.copies[B].player.own_next();
    world.release();
    assert_eq!((said(&world), world.copies[B].player.playing), ("Playing here. Copy B was given something else to do before it could be paused, so it may still be playing.", true));
    // Never delivered.
    let mut world = taking(|world| world.path.hold_commands = true);
    world.pass(14_000);
    assert_eq!((said(&world), world.copies[A].player.playing, world.copies[B].player.playing), ("Playing here. Copy B didn't answer, so it may still be playing.", true, true));
}

// ---- reset, and the secret changing under a command ------------------------------------------------

#[test]
fn reset_linking_with_a_command_in_flight() {
    let mut world = steering("next");
    world.path.hold_commands = true;
    world.ask("next");
    let did = world.copies[A].device.reset();
    world.act(A, did);
    world.pass(250);
    assert_eq!(world.copies[A].looks.last(), Some(&(1, true)));
    let old = world.copies[A].session;
    // The look wrote a new secret and read it back.
    world.looked(A, Looked::Secret { secret: secret(8), id: "k".into(), wrote: Some(secret::Why::Reset) }, true);
    assert!(world.copies[A].ended.contains(&old) && world.copies[A].session != old);
    let now = world.now;
    assert_eq!(world.copies[A].device.shown(now).0.note, words::RESET_DONE);
    // B is still on the old secret, in the old room: a command sealed under it is B's to
    // carry out until B's own next look (up to two minutes), as the design says.
    let frame = world.path.commands[0].clone();
    world.release();
    assert_eq!(world.copies[B].player.did, ["next"]);
    // B looks, and moves to the new secret: the same frame does not open there.
    world.looked(B, Looked::Secret { secret: secret(8), id: "k".into(), wrote: None }, false);
    world.deliver_to(B, frame);
    assert!(world.last(B, "does not open").contains("dropped"));
    assert_eq!(world.copies[B].player.did, ["next"]);
    // Under the new secret they find each other again.
    world.hear_places();
    world.pass(2_000);
    assert_eq!(world.row(B, "Copy A").map(|row| row.ready), Some(true));
    // A reset that could not be written, and one written and not read back.
    let mut world = World::new(false);
    let did = world.copies[A].device.reset();
    world.act(A, did);
    world.pass(250);
    world.looked(A, Looked::Failed, true);
    let now = world.now;
    assert_eq!((world.copies[A].device.shown(now).0.note, world.copies[A].ended.len()), (words::RESET_NOT_DONE.to_string(), 0));
    let did = world.copies[A].device.reset();
    world.act(A, did);
    world.pass(250);
    world.looked(A, Looked::Invalid, true);
    assert_eq!((world.copies[A].device.shown(now).0.note, world.copies[A].ended.len()), (words::RESET_HALF_DONE.to_string(), 1));
}

#[test]
fn a_command_accepted_under_one_secret_is_not_carried_out_under_another() {
    // (For a test: B holds a Pause between accepting it and carrying it out.)
    let mut world = steering("pause");
    world.copies[B].device.slow_pause_ms = Some(30_000);
    world.ask("pause");
    let ticket = world.copies[B].later[0];
    assert!(world.copies[B].player.rule.busy(world.now) && world.copies[B].player.playing);
    // The secret is reset meanwhile, and B has looked.
    world.looked(B, Looked::Secret { secret: secret(8), id: "k".into(), wrote: None }, false);
    let did = world.with(B, |device, env| device.resume(ticket, env));
    world.act(B, did);
    assert!(world.last(B, "pause from").contains("refused, Cannot"));
    assert!(world.copies[B].player.playing && !world.copies[B].player.rule.busy(world.now));
    assert_eq!(world.last(B, "answer:"), "answer: the session it belonged to has ended, not sent");
}

#[test]
fn a_device_answers_twenty_asks_a_second_and_no_more() {
    let mut world = World::new(true);
    let (from, to) = (world.copies[A].place.unwrap(), world.copies[B].place.unwrap());
    let keys = Keys::of(&secret(7));
    let look = |n: u8| frame::seal(&keys, ASK, &[n; 24], &from, &to, Ask::Look(Want::Brief).text().as_bytes()).unwrap();
    for n in 0..30 {
        world.deliver_to(B, Frame { from, to, text: look(n) });
    }
    assert_eq!(world.log(B, "more than this device answers in a second").len(), 10);
    // An ask that is not one this version carries out is answered so, and does nothing.
    world.pass(1_000);
    let odd = frame::seal(&keys, ASK, &[99; 24], &from, &to, br#"{"t":"do","do":"toggle","at":[1,0]}"#).unwrap();
    world.deliver_to(B, Frame { from, to, text: odd });
    assert_eq!(world.log(B, "not one this version carries out").len(), 1);
}

#[test]
fn an_entry_is_found_where_it_was_seen_or_at_its_nearest_copy() {
    let ahead: Vec<String> = ["a", "b", "c", "b", "d"].iter().map(|id| id.to_string()).collect();
    assert_eq!(entry(&ahead, 1, "b"), Some(1));
    // The queue has moved on by one: the song seen at 2 is now at 1 or 3; the nearer.
    assert_eq!((entry(&ahead, 2, "b"), entry(&ahead, 4, "b"), entry(&ahead, 0, "d")), (Some(1), Some(3), Some(4)));
    assert_eq!((entry(&ahead, 0, "z"), entry(&[], 0, "a"), entry(&ahead, 99, "a")), (None, None, Some(0)));
    // The songs of a queue a library has, with the ones queued by hand still so.
    let library = Snapshot { ids: vec!["a".into(), "c".into(), "d".into()], ..Default::default() };
    let ids: Vec<String> = ["a", "b", "c", "d"].iter().map(|id| id.to_string()).collect();
    assert_eq!(known(&library, &ids, 0, 2), (vec![("a".to_string(), false), ("c".to_string(), true), ("d".to_string(), false)], 0));
    assert_eq!(known(&library, &ids, 1, 1), (vec![("a".to_string(), false), ("c".to_string(), true), ("d".to_string(), false)], 1));
    assert_eq!(known(&Snapshot::default(), &ids, 0, 0), (vec![], 0));
}
