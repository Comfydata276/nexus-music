//! Linking a person's devices the new way: this app's side of it.
//!
//! It is **off** unless the environment says `NEXUS_MUSIC_LINK=1`, and has no setting. The
//! old linking (`jam.rs`) is beside it and unchanged.
//!
//! Everything that decides is the library's (`nexus_core::link`, to
//! `docs/linking-design.md`): the look at the secret, the seal, the messages, the rule for
//! a command, what is asked of another device and what is concluded, moving music, the
//! news, the list, and every sentence the person is told. `nexus_core::link::device::Device`
//! is all of it as one piece of state that is told what happened and says what to do.
//!
//! What is here is what is this app's by nature:
//!
//! - **the order of choices:** the count is the library's (`Rule`), and what moves it on
//!   is every way into the player, each of which says who chose it in `ways` (where whose
//!   the queue is comes of the same statement); what is here is the count being moved,
//!   and the busy that a choice still being looked up makes (`looking_up`);
//! - **the clock and the random numbers** the library is handed;
//! - **the threads:** one that keeps this PC's place at the relay for as long as a session
//!   lasts, and a small pool for the look and for what is sent, each asking through the
//!   library's `Http` on one of the app's own agents;
//! - **the player and the library,** as the deciding asks them ([`Here`], and the search
//!   index);
//! - **the window:** the list under "Connect to a device", the other device's player and
//!   queue while one is being steered, and "Reset linking".

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use nexus_core::http::{Failure, Http, Request, Response};
use nexus_core::link::command::Command;
use nexus_core::link::device::{Brief, Carried, Device, Do, Env, Me, Player, Ready, Sounding, may_go, wanted};
use nexus_core::link::message::{At, Entry, Full, Handed, Why};
use nexus_core::link::relay::{self, FromRelay, Relay};
use nexus_core::link::rule::{Carrying, Reservation, Waiting};
use nexus_core::link::view::{Press, View};
use nexus_core::link::words::Here as Kind;
use nexus_core::link::{Nonce, look, secret, text};
use nexus_core::requests::{Library, Snapshot};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::config;
use crate::controller::{Controller, clock, post, with};
use crate::jam::Remote;
use crate::pool::Pool;
use crate::subsonic::Song;
use crate::ui;
use crate::ways::{What, Who};

/// Whether the new linking is on for this run of the app.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NEXUS_MUSIC_LINK").is_ok_and(|value| value == "1"))
}

/// Why a playlist of the person's own may not have this name, if it may not.
pub fn kept_name(name: &str) -> Option<&'static str> {
    secret::kept_name(name, on())
}

/// Whether a playlist of this name is left out of every ordinary listing of playlists.
pub fn unlisted(name: &str) -> bool {
    secret::unlisted(name, on())
}

/// Whether a playlist's page may be written to the cache of pages on disk, and read back
/// from it (`controller::store_page`, `controller::read_page`).
pub fn page_may_be_kept(playlist: &str, comment: &str) -> bool {
    secret::page_may_be_kept(playlist, comment)
}

/// Milliseconds since the app started, on a clock that goes on counting while the PC
/// sleeps: the rule's minute, and the life of the secret, are read off it.
pub fn now_ms() -> u64 {
    static START: OnceLock<u64> = OnceLock::new();
    let now = awake_and_asleep_ms();
    now.saturating_sub(*START.get_or_init(|| now))
}

/// Time since the system started, sleep included.
#[cfg(windows)]
fn awake_and_asleep_ms() -> u64 {
    // SAFETY: takes nothing and returns a number.
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

/// Time since the system started, sleep included: what Linux calls its boot time.
#[cfg(not(windows))]
fn awake_and_asleep_ms() -> u64 {
    static OTHERWISE: OnceLock<std::time::Instant> = OnceLock::new();
    let read = || {
        let text = std::fs::read_to_string("/proc/uptime").ok()?;
        let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
        Some((seconds * 1000.0) as u64)
    };
    // (Elsewhere there is no such file. The app is not shipped for those systems.)
    read().unwrap_or_else(|| OTHERWISE.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64)
}

/// Bytes from the system's source of randomness, or `None` if it has none to give. Nothing
/// is ever made up another way.
fn random<const N: usize>() -> Option<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes)
}

/// For a test only: how long a Pause from another device is held between being accepted
/// and being carried out, if `NEXUS_MUSIC_LINK_TEST_SLOW_PAUSE` gives a number of
/// milliseconds. With it, a run can make the secret change in between. Never set otherwise.
fn slow_pause() -> Option<u64> {
    static WAIT: OnceLock<Option<u64>> = OnceLock::new();
    *WAIT.get_or_init(|| std::env::var("NEXUS_MUSIC_LINK_TEST_SLOW_PAUSE").ok()?.parse().ok())
}

/// For a test only: with `NEXUS_MUSIC_LINK_TEST_FAIL_TAKE` set, a queue brought from another
/// device ("Play here") is not put in this PC's player, as if the player had refused it.
/// Never set otherwise.
fn fail_take() -> bool {
    static FAIL: OnceLock<bool> = OnceLock::new();
    *FAIL.get_or_init(|| std::env::var_os("NEXUS_MUSIC_LINK_TEST_FAIL_TAKE").is_some())
}

/// Writes a line of what happened to the file named by `NEXUS_MUSIC_LINK_LOG`, if any: for
/// watching two copies from outside. Never a secret, a key or a token.
pub(crate) fn log(line: impl AsRef<str>) {
    use std::io::Write;
    static FILE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    if let Some(path) = FILE.get_or_init(|| std::env::var_os("NEXUS_MUSIC_LINK_LOG").map(Into::into))
        && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(file, "{:>8} {}", now_ms(), line.as_ref());
    }
}

// ---- the order of choices ----------------------------------------------------------------
//
// It exists so that a command from another device is judged against what is true on this
// one, and for nothing else. Nothing the person does here is ever refused, delayed or
// dropped because of it: these calls only keep count. What moves it on is every way into
// the player, and those say so in `ways`, which is also where whose the queue is is said.

/// How long a look-up for something to play is given before this PC stops counting as
/// busy on its account: longer than any one request to the music server is given.
const LOOK_UP_MS: u64 = 60_000;

impl Controller {
    /// What a command from another device would act on has changed, by a press or by
    /// itself: the order of choices moves on one place. Called from the one place that
    /// knows of every way into the player (`ways`), and from nowhere else.
    pub(crate) fn link_moved(&self) {
        let mut rule = self.link_rule.borrow_mut();
        rule.moved();
        log(format!("order: moved on to {}", rule.generation()));
    }

    /// Something to play has been chosen and its songs are still to be looked up: until
    /// `looked_up`, or the look-up's time is up, this PC is busy to other devices. To the
    /// person at it, nothing is different.
    pub fn looking_up(&self) -> Waiting {
        log("order: busy, something chosen here is being looked up");
        self.link_rule.borrow_mut().wait(now_ms(), LOOK_UP_MS)
    }

    /// The look-up is over, whatever it found.
    pub fn looked_up(&self, waiting: Waiting) {
        let mut rule = self.link_rule.borrow_mut();
        rule.done(waiting);
        log(format!("order: a look-up is over{}", if rule.busy(now_ms()) { ", another is still under way" } else { "" }));
    }
}

// ---- the network, as the library asks for it --------------------------------------------

/// How long a call to the music server is given, where the library names no shorter time.
const SERVER_PATIENCE: Duration = Duration::from_secs(45);

/// The music server, for one look: one of the app's own agents, that follows no redirect.
/// Once the sign-in the look is for has ended it makes no request at all.
struct LookHttp {
    agent: crate::corehttp::Agent,
    /// Set when the sign-in ends.
    over: Arc<AtomicBool>,
    /// Offline mode: nothing is asked of the server.
    offline: Box<dyn Fn() -> bool + Send + Sync>,
}

impl Http for LookHttp {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        // (Signing out is not leaving a page: it stops a look whatever it has written.)
        if self.over.load(Ordering::Relaxed) {
            return Err(Failure::Stopped);
        }
        if (self.offline)() {
            return Err(Failure::Reach("Offline mode is on.".into()));
        }
        self.agent.send(&Request { patience: request.patience.or(Some(SERVER_PATIENCE)), ..request.clone() })
    }
}

/// The relay, for one session's stay in its room. A request is made whenever it is asked
/// for (a poll is never cut short); a wait ends, and says to stop, once the session has.
struct RoomHttp {
    agent: crate::corehttp::Agent,
    stop: Arc<AtomicBool>,
}

impl Http for RoomHttp {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        self.agent.send(request)
    }

    fn pause(&self, wait: Duration) -> Result<(), Failure> {
        let stopped = || self.stop.load(Ordering::Relaxed);
        let mut left = wait;
        while !left.is_zero() && !stopped() {
            let step = left.min(Duration::from_millis(100));
            std::thread::sleep(step);
            left -= step;
        }
        if stopped() { Err(Failure::Stopped) } else { Ok(()) }
    }
}

// ---- what this app holds -------------------------------------------------------------------

/// A session's own threads' view of it: told to stop, and when its secret was last read.
struct Live {
    session: u64,
    stop: Arc<AtomicBool>,
    since: Arc<AtomicU64>,
    relay: Arc<Relay>,
    room: String,
}

#[derive(Default)]
struct Inner {
    device: Device,
    /// The device has been given what the settings remember.
    started: bool,
    pool: Option<Arc<Pool>>,
    live: Option<Live>,
    /// Set when the sign-in ends, for the look that is on its way to see.
    look_over: Arc<AtomicBool>,
    /// Set when the sign-in ends, for whatever is asking the music server for songs a
    /// command brought: a new one for each sign-in.
    sign_in_over: Arc<AtomicBool>,
    /// Songs the music server was asked for because this PC's copy of the library lacked
    /// them: held while the command that brought them is carried out, and then let go of.
    found: HashMap<String, Song>,
}

/// The device being steered, as the window shows it. Kept apart from the deciding, so that
/// the player can be drawn while the deciding is at work.
struct Shown {
    name: String,
    view: Option<View>,
    /// Its songs as this PC's own library knows them, by id; `None` for one it doesn't
    /// have. Kept so each is looked up once.
    known: HashMap<String, Option<Song>>,
}

#[derive(Default)]
pub struct Link {
    inner: RefCell<Inner>,
    remote: RefCell<Option<Shown>>,
    /// The song the sound was found to have rolled into before the window had been told:
    /// the engine's own word of it, when it comes, is not acted on twice.
    caught: Cell<u64>,
    /// The mark of the queue handed to another device: which queue, and whose it was
    /// before (`nexus_core::handover::Mark`).
    handed: RefCell<nexus_core::handover::Mark>,
}

/// This PC's own player, as the deciding asks it. Each command is carried out by the
/// function the same press runs.
struct Here<'a> {
    ctl: &'a Rc<Controller>,
    found: &'a HashMap<String, Song>,
}

fn entry_of(song: &Song) -> Entry {
    Entry { id: song.id.clone(), title: song.title.clone(), artist: song.artist_label().to_string(), album: song.album.clone(), duration: song.duration, queued: song.queued }
}

impl Here<'_> {
    /// These songs as this PC has them, in this order, each queued by hand or not as it says.
    fn songs<'s>(&self, ids: impl Iterator<Item = (&'s String, bool)>) -> Vec<Song> {
        let index = crate::search::current();
        let known: HashMap<&str, &Song> = index.iter().flat_map(|index| index.library.songs.iter()).map(|song| (song.id.as_str(), song)).collect();
        ids.filter_map(|(id, queued)| known.get(id.as_str()).copied().or_else(|| self.found.get(id)).map(|song| Song { queued, ..song.clone() })).filter(|song| song.radio_url.is_empty()).collect()
    }
}

impl Player for Here<'_> {
    fn brief(&self) -> Brief {
        let pb = self.ctl.playback.borrow();
        let now = pb.current();
        Brief { playing: pb.playing && now.is_some(), title: now.map(|song| song.title.clone()).unwrap_or_default(), artist: now.map(|song| song.artist_label().to_string()).unwrap_or_default() }
    }

    fn full(&self) -> Full {
        let pb = self.ctl.playback.borrow();
        // (As many of what is to come as a tell carries are looked at, and no more: the
        // rest are counted from how many there are.)
        let coming = pb.order.iter().skip(pb.cursor + 1).filter_map(|&index| pb.queue.get(index)).map(entry_of);
        Full::fitted(pb.position, pb.current().map(entry_of), coming, pb.order.len().saturating_sub(pb.cursor + 1), &pb.context, pb.shuffle, pb.repeat, f64::from(pb.volume))
    }

    fn handed(&self) -> Handed {
        let pb = self.ctl.playback.borrow();
        if pb.current().is_none_or(|song| !song.radio_url.is_empty()) {
            return Handed::default();
        }
        let ids: Vec<String> = pb.order.iter().skip(pb.cursor).filter_map(|&index| pb.queue.get(index)).filter(|song| song.radio_url.is_empty()).map(|song| song.id.clone()).collect();
        Handed::fitted(&ids, pb.queued_ahead(), pb.position, &pb.context)
    }

    fn sounding(&self) -> Option<Sounding> {
        // (The engine's own place in the song the player holds: sound that has come out,
        // of the queue that is in the player now.)
        self.ctl.sounding_at().map(|at| Sounding { queue: self.ctl.queue_number.get(), at })
    }

    fn queue(&self) -> u64 {
        self.ctl.queue_number.get()
    }

    fn catch_up(&mut self) -> bool {
        let Some(token) = self.ctl.rolled_ahead() else { return false };
        self.ctl.started(token);
        self.ctl.link.borrow().caught.set(token);
        true
    }

    fn has_song(&self) -> bool {
        self.ctl.playback.borrow().current().is_some()
    }

    fn ahead(&self) -> Vec<String> {
        let pb = self.ctl.playback.borrow();
        pb.order.iter().skip(pb.cursor + 1).filter_map(|&index| pb.queue.get(index)).map(|song| song.id.clone()).collect()
    }

    fn to_come(&self) -> usize {
        let pb = self.ctl.playback.borrow();
        pb.order.len().saturating_sub(pb.cursor + 1)
    }

    fn carry_out(&mut self, command: &Command, ready: &Ready) -> Carried {
        let ctl = self.ctl;
        // **What is so already is said as that, and is not done again** (the functions
        // below would count a volume set to what it is, or a queue cleared of nothing, as
        // a change). It is found here, before anything is asked of the player, so that
        // "the player took no way in" below means one thing only: it refused.
        let already = {
            let pb = ctl.playback.borrow();
            match (command, ready) {
                (Command::Play, _) => pb.playing,
                (Command::Pause | Command::Moved, _) => !pb.playing,
                (Command::Volume { to }, _) => !pb.muted && (pb.volume - (*to as f32).clamp(0.0, 1.0)).abs() < 0.001,
                (Command::Shuffle { on }, _) => pb.shuffle == *on,
                (Command::Repeat { mode }, _) => pb.repeat == (*mode).min(2),
                (Command::Clear { queued: true }, _) => pb.queued_ahead() == 0,
                (Command::Clear { queued: false }, _) => pb.order.len() <= pb.cursor + 1 + pb.queued_ahead(),
                // An entry put down where it is, as what it is.
                (Command::Move { to, queued, .. }, Ready::Entry(from)) => {
                    let last = pb.order.len().saturating_sub(pb.cursor + 2);
                    pb.cursor + 1 + from < pb.order.len() && !pb.moves_ahead(*from, (*to as usize).min(last), *queued)
                }
                _ => false,
            }
        };
        if already {
            return Carried::AlreadySo;
        }
        // The person chose this at another of their devices. The rule counts it, once, if
        // the player changes anything for it, so the player's own counting is not added;
        // and the queue is theirs by it as it would be by the same press here.
        // (The stop that ends a move of music to the device that asks is nobody's.)
        let _carrying = ctl.for_another(command.chosen_by(), true);
        match (command, ready) {
            (Command::Play, _) => ctl.set_playing_here(true),
            (Command::Pause | Command::Moved, _) => ctl.set_playing_here(false),
            (Command::Next, _) => ctl.next_track_here(),
            (Command::Previous, _) => ctl.previous_track_here(),
            (Command::Seek { to }, _) => ctl.seek_here(*to),
            (Command::Volume { to }, _) => ctl.set_volume_here(*to as f32),
            (Command::Jump { .. }, Ready::Entry(entry)) => ctl.jump_to_upcoming_here(*entry),
            (Command::Remove { .. }, Ready::Entry(entry)) => ctl.remove_upcoming_here(*entry),
            (Command::Move { to, queued, .. }, Ready::Entry(from)) => {
                let last = {
                    let pb = ctl.playback.borrow();
                    pb.order.len().saturating_sub(pb.cursor + 2)
                };
                ctl.move_upcoming_here(*from, (*to as usize).min(last), *queued);
            }
            (Command::Clear { queued }, _) => ctl.clear_upcoming_here(*queued),
            (Command::Shuffle { on }, _) => ctl.set_shuffle_from_afar(*on),
            (Command::Repeat { mode }, _) => ctl.set_repeat_from_afar(*mode),
            (Command::Add { next, .. }, Ready::Songs(ids)) => ctl.enqueue_here(self.songs(ids.iter().map(|id| (id, false))), *next),
            (Command::Adopt(adopt), Ready::Queue(songs, begin)) => ctl.load_queue_here(self.songs(songs.iter().map(|(id, queued)| (id, *queued))), *begin, adopt.position, &adopt.context),
            _ => {}
        }
        // Each of those takes its way into the player only once it knows something will
        // change (`ways`): whether one was taken is whether anything did. One that took
        // none, for something that was not so already, could not do it.
        if ctl.ways.taken() { Carried::Changed } else { Carried::Refused }
    }

    /// This PC's queue is another device's now. In `nexus_core::handover`'s terms it is
    /// one taken up from that device and not touched: not saved, and it follows what that
    /// device saves, until the person uses it here (`claim_queue`). It is another queue
    /// for a save that was already prepared, which is then not sent.
    ///
    /// **And it is written before this returns**, where the next start of the app will
    /// find it (`save_queue_surely`, the queue's place): on a PC that was quiet already
    /// nothing else would write it, since the stop that follows changes nothing; and
    /// "written" is the file being on disk, not its being in line for the saver.
    fn handed_on(&mut self) {
        log("queue: handed to another of the person's devices: not saved from here, and it follows, until it is used here");
        let mark = self.ctl.link.borrow().handed.borrow_mut().on(self.ctl.queue_number.get(), self.ctl.queue_owner.taken());
        self.ctl.queue_owner.taken_up(mark);
        if !self.ctl.save_queue_surely() {
            log("queue: handed on, and that could not be written down in time: it is written with the next save");
        }
    }

    /// This PC did not stop: the queue that was marked is whose it was. (If another whole
    /// queue is in the player by now, whose that is was settled when it was put there.)
    fn handed_back(&mut self) {
        let was = self.ctl.link.borrow().handed.borrow_mut().back(self.ctl.queue_number.get());
        if let Some(was) = was {
            log("queue: this PC did not stop, so its queue is not handed on after all");
            self.ctl.queue_owner.handed_back(was);
            self.ctl.save_queue_surely();
        }
    }

    /// (The end of a move of this PC's music to another device: the app's own doing. The
    /// person's choice was that the queue go there, and that device claimed it as it took
    /// it; a claim here a moment later would make this PC, now silent, the newest.)
    fn pause(&mut self) {
        self.ctl.came_in(Who::Nobody, What::Playing);
        self.ctl.pause_here();
    }

    /// (Whether it went in is the player's own word: its queue has a new number.)
    fn take(&mut self, songs: &[(String, bool)], begin: usize, position: f64, context: &str) {
        if fail_take() {
            return;
        }
        self.ctl.load_queue_here(self.songs(songs.iter().map(|(id, queued)| (id, *queued))), begin, position, context);
    }

    fn generation(&self) -> u64 {
        self.ctl.link_rule.borrow().generation()
    }

    fn busy(&self, now_ms: u64) -> bool {
        self.ctl.link_rule.borrow().busy(now_ms)
    }

    fn by(&self) -> Option<Nonce> {
        self.ctl.link_rule.borrow().by()
    }

    fn admit(&mut self, at: At, minute: u64, now_ms: u64) -> Result<Reservation, Why> {
        self.ctl.link_rule.borrow_mut().admit(at, minute, now_ms)
    }

    fn finish(&mut self, reservation: Reservation, at: At, minute: u64, now_ms: u64, same_session: bool) -> Result<Carrying, Why> {
        self.ctl.link_rule.borrow_mut().finish(reservation, at, minute, now_ms, same_session)
    }

    fn carried(&mut self, carrying: Carrying, changed: bool, by: Option<Nonce>) {
        self.ctl.link_rule.borrow_mut().carried(carrying, changed, by);
    }

    fn refuse(&mut self, reservation: Reservation) {
        self.ctl.link_rule.borrow_mut().refuse(reservation);
    }

    fn wait(&mut self, now_ms: u64, for_ms: u64) -> Waiting {
        self.ctl.link_rule.borrow_mut().wait(now_ms, for_ms)
    }

    fn done(&mut self, waiting: Waiting) {
        self.ctl.link_rule.borrow_mut().done(waiting);
    }
}

/// What a press in the player or the queue panel is, to the deciding.
fn press_of(what: &Remote) -> Press {
    match what {
        Remote::Toggle => Press::Toggle,
        Remote::Play(on) => Press::Play(*on),
        Remote::Next => Press::Next,
        Remote::Previous => Press::Previous,
        Remote::Seek(to) => Press::Seek(*to),
        Remote::Remove(at) => Press::Remove(*at),
        Remote::Move(from, to, queued) => Press::Move(*from, *to, *queued),
        Remote::Clear(queued) => Press::Clear(*queued),
        Remote::Jump(at) => Press::Jump(*at),
        Remote::Shuffle => Press::Shuffle,
        Remote::Repeat => Press::Repeat,
        Remote::Volume(to) => Press::Volume(f64::from(*to)),
    }
}

fn today() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs() / 86_400)
}

impl Controller {
    /// This installation among the person's devices: an id made once, and its name.
    fn link_me(&self) -> Me {
        // (Read, and let go of, before anything is written: the settings are borrowed for
        // as long as what was read from them is being matched on.)
        let kept = text::read_id(&self.settings.borrow().link_device);
        let device = match kept {
            Some(id) => id,
            None => {
                // (Not secret, and only ever sent sealed: if the system has no random numbers
                // to give, nothing is sent at all, so what this falls back on is never seen.)
                let id = random::<16>().unwrap_or_default();
                self.settings.borrow_mut().link_device = text::base64(&id);
                self.save_settings_in_passing();
                id
            }
        };
        Me { device, name: self.this_device().1, here: Kind::Pc }
    }

    /// Hands the deciding something that happened, with the player, the library, the time
    /// and the random numbers it may ask for. `None` if the deciding is already at work
    /// (nothing here calls back into it while it is).
    fn link_with<T>(self: &Rc<Self>, event: impl FnOnce(&mut Device, &mut Env) -> T) -> Option<T> {
        // (With the switch off there is no new linking: nothing of it runs, whoever calls,
        // and this device is given no id for it.)
        if !on() {
            return None;
        }
        let me = self.link_me();
        let link = self.link.borrow();
        let mut inner = link.inner.try_borrow_mut().ok()?;
        let Inner { device, found, .. } = &mut *inner;
        let index = crate::search::current();
        let nothing = Snapshot::default();
        let library: &dyn Library = match &index {
            Some(index) => &**index,
            None => &nothing,
        };
        let mut player = Here { ctl: self, found };
        let mut fill = |bytes: &mut [u8]| getrandom::fill(bytes).is_ok();
        let mut env = Env { now_ms: now_ms(), today: today(), me: &me, player: &mut player, library, random: &mut fill };
        Some(event(device, &mut env))
    }

    /// The same, and then does what the deciding asked.
    fn link_event(self: &Rc<Self>, event: impl FnOnce(&mut Device, &mut Env) -> Vec<Do>) {
        if let Some(asked) = self.link_with(event) {
            self.link_do(asked);
        }
    }

    fn link_pool(&self) -> Arc<Pool> {
        self.link.borrow().inner.borrow_mut().pool.get_or_insert_with(|| Arc::new(Pool::new("link", 2, false))).clone()
    }

    /// Does what the deciding asked for. Nothing of the deciding is held while it is done.
    fn link_do(self: &Rc<Self>, asked: Vec<Do>) {
        for ask in asked {
            match ask {
                Do::Log(line) => log(line),
                Do::Say(words) => self.toast(words),
                Do::Look { number, used, reset } => {
                    let over = Arc::new(AtomicBool::new(false));
                    self.link.borrow().inner.borrow_mut().look_over = over.clone();
                    let (since, client) = (now_ms(), self.client());
                    self.link_pool().spawn(move || {
                        let looked = match client {
                            Some(client) => {
                                let offline = client.clone();
                                let http = LookHttp { agent: crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects()), over, offline: Box::new(move || offline.is_offline()) };
                                look::look(&http, &client.link_account(), used, reset, &mut random::<32>, &mut |line| log(line))
                            }
                            None => look::Looked::Failed,
                        };
                        post(move |ctl| ctl.link_event(|device, env| device.looked(number, since, looked, reset, env)));
                    });
                }
                Do::StopLook => self.link.borrow().inner.borrow().look_over.store(true, Ordering::Relaxed),
                Do::Begin { session, room, since_ms } => self.link_begin(session, room, since_ms),
                Do::Renew { session, since_ms } => {
                    if let Some(live) = self.link.borrow().inner.borrow().live.as_ref().filter(|live| live.session == session) {
                        live.since.store(since_ms, Ordering::Relaxed);
                    }
                }
                Do::End { session } => {
                    let link = self.link.borrow();
                    let mut inner = link.inner.borrow_mut();
                    if let Some(live) = inner.live.take_if(|live| live.session == session) {
                        live.stop.store(true, Ordering::Relaxed);
                    }
                }
                Do::Send { session, token, to, frame } => {
                    let sending = self.link.borrow().inner.borrow().live.as_ref().filter(|live| live.session == session).map(|live| (live.relay.clone(), live.room.clone(), live.stop.clone(), live.since.clone()));
                    let Some((relay, room, stop, since)) = sending else { continue };
                    self.link_pool().spawn(move || {
                        // The session may have ended, or run out of time, while this waited
                        // its turn: then it is not sent.
                        if !may_go(stop.load(Ordering::Relaxed), since.load(Ordering::Relaxed), now_ms()) {
                            return log("send: the session ended or ran out first, not sent");
                        }
                        let status = relay::send(&crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects()), &relay, &room, token.as_str(), &to, &frame);
                        if status != Some(200) {
                            log(format!("send: the relay answered {status:?}"));
                        }
                    });
                }
                // (On the app's own pool for the music server, not the two threads that send
                // to the relay and read the secret: a long look-up holds neither of those
                // up. And not for longer than the command it is for is waited on: the
                // deciding gives the command up then, and a look-up of fifty songs at a
                // server that has stopped answering must not go on holding a worker. No
                // request of it outlives the command: `nexus_core::link::find`.)
                Do::Find { ticket, ids, until_ms } => {
                    let account = self.settings.borrow().account();
                    // What is left of the command's time, counted here and now from the
                    // moment it ends: the time spent waiting for a worker is part of it.
                    let (left, began) = (Duration::from_millis(until_ms.saturating_sub(now_ms())), std::time::Instant::now());
                    // (And it asks nothing more once this sign-in has ended.)
                    let over = self.link.borrow().inner.borrow().sign_in_over.clone();
                    self.request(
                        move |client| {
                            // These are the songs this PC's copy of the library lacks:
                            // each is asked of the music server, given what is then left.
                            let offline = client.clone();
                            let http = LookHttp { agent: crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects()), over, offline: Box::new(move || offline.is_offline()) };
                            let found = nexus_core::link::find::find(&http, &client.link_account(), &ids, &mut || left.saturating_sub(began.elapsed()));
                            Ok(found.into_iter().filter(|song| song.radio_url.is_empty()).collect::<Vec<Song>>())
                        },
                        move |ctl, songs| {
                            // Before anything it found is kept: is a command still waiting
                            // for it, under the sign-in it was started for? One that was
                            // given up, and one from before a change of account, is not.
                            let waited_for = ctl.settings.borrow().account() == account && ctl.link.borrow().inner.borrow().device.awaits(ticket);
                            if !waited_for {
                                return log("found: nothing is waiting for these songs any longer, not kept");
                            }
                            let songs = songs.unwrap_or_default();
                            let found: Vec<String> = songs.iter().map(|song| song.id.clone()).collect();
                            // (Kept for as long as the command they are for is being
                            // carried out, and no longer.)
                            ctl.link.borrow().inner.borrow_mut().found = songs.into_iter().map(|song| (song.id.clone(), song)).collect();
                            ctl.link_event(|device, env| device.found(ticket, found, env));
                            ctl.link.borrow().inner.borrow_mut().found.clear();
                        },
                    )
                }
                Do::Later { ticket, after_ms } => {
                    let ctl = self.clone();
                    slint::Timer::single_shot(Duration::from_millis(after_ms), move || ctl.link_event(|device, env| device.resume(ticket, env)));
                }
                Do::Remember(list) => {
                    self.settings.borrow_mut().link_seen = list;
                    self.save_settings_in_passing();
                }
                Do::Forget { playlist } => self.invalidate(&format!("playlist:{playlist}")),
                Do::Steering(name) => {
                    {
                        let link = self.link.borrow();
                        let mut remote = link.remote.borrow_mut();
                        *remote = match (name, remote.take()) {
                            // (The same device under a new name: what is shown of it stays.)
                            (Some(name), Some(shown)) => Some(Shown { name, ..shown }),
                            (Some(name), None) => Some(Shown { name, view: None, known: HashMap::new() }),
                            (None, _) => None,
                        };
                    }
                    // The player and the queue panel are the other device's, or this PC's again.
                    self.show_joined();
                    self.track_changed();
                }
                Do::Draw => {
                    {
                        let link = self.link.borrow();
                        let view = link.inner.borrow().device.view().cloned();
                        if let Some(shown) = link.remote.borrow_mut().as_mut() {
                            shown.view = view;
                        }
                    }
                    self.show_joined();
                    self.link_show_remote();
                }
            }
        }
    }

    /// A session has begun: this PC keeps a place in its room, on a thread of its own, for
    /// as long as the session lasts.
    fn link_begin(self: &Rc<Self>, session: u64, room: String, since_ms: u64) {
        let (address, Some(key)) = (self.settings.borrow().jam_relay.clone(), config::load_secret(&self.paths, "jam-key")) else { return };
        let relay = Arc::new(Relay { base: address.trim_end_matches('/').to_string(), key });
        let stop = Arc::new(AtomicBool::new(false));
        let live = Live { session, stop: stop.clone(), since: Arc::new(AtomicU64::new(since_ms)), relay: relay.clone(), room: room.clone() };
        if let Some(earlier) = self.link.borrow().inner.borrow_mut().live.replace(live) {
            earlier.stop.store(true, Ordering::Relaxed);
        }
        let _ = std::thread::Builder::new().name("link-room".into()).spawn(move || {
            let http = RoomHttp { agent: crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects()), stop };
            let mut tell = |from: FromRelay| {
                post(move |ctl| match from {
                    FromRelay::Says(note) => ctl.link.borrow().inner.borrow_mut().device.said(session, note),
                    FromRelay::NoRandom => ctl.link_event(|device, env| device.out_of_random(env)),
                    FromRelay::Entered { place, token } => ctl.link.borrow().inner.borrow_mut().device.entered(session, place, token),
                    FromRelay::Heard { places, frames } => ctl.link_event(|device, env| device.heard(session, &places, &frames, env)),
                    FromRelay::Left => ctl.link.borrow().inner.borrow_mut().device.left(session),
                });
                // (Posted to the window's thread, which takes everything.)
                true
            };
            relay::keep_place(&http, &relay, &room, &mut random::<16>, &mut tell, &mut |line| log(line));
        });
    }

    /// Called a few times a second: keeps the secret fresh, this device in the room, and
    /// what is known of the others up to date.
    pub fn link_tick(self: &Rc<Self>) {
        if !on() {
            return;
        }
        {
            let link = self.link.borrow();
            let mut inner = link.inner.borrow_mut();
            if !std::mem::replace(&mut inner.started, true) {
                inner.device = Device::new(self.settings.borrow().link_seen.clone());
                inner.device.slow_pause_ms = slow_pause();
            }
        }
        let account = self.settings.borrow().account();
        let wanted = wanted(self.client().is_some(), self.can_host_jam(), self.settings.borrow().link_devices);
        self.link_event(|device, env| device.tick(&account, wanted, env));
        // The progress bar moves by itself between one look at the other device and the next.
        let position = self.link.borrow().remote.borrow().as_ref().and_then(|shown| shown.view.as_ref()).map(|view| view.position(now_ms()));
        if let Some(position) = position {
            self.ui.global::<ui::Player>().set_position(position as f32);
        }
        self.link_show();
    }

    /// "Link my devices" was changed in Settings. Where the new linking is switched on,
    /// off takes effect at once, not at the next tick.
    pub fn link_setting(self: &Rc<Self>, link_my_devices: bool) {
        if !on() {
            return;
        }
        self.link_event(|device, env| device.setting(link_my_devices, env));
        self.link_show();
    }

    /// Ends everything of this sign-in's: at sign-out, and as the app closes.
    pub fn link_stop(self: &Rc<Self>) {
        // Whatever is still asking the music server for songs under this sign-in asks
        // nothing more.
        let over = std::mem::take(&mut self.link.borrow().inner.borrow_mut().sign_in_over);
        over.store(true, Ordering::Relaxed);
        self.link_event(|device, env| device.stop(env));
    }

    /// "Reset linking" was pressed.
    pub fn link_reset(self: &Rc<Self>) {
        if !on() {
            return;
        }
        self.link_event(|device, env| device.reset(env));
        self.link_show();
    }

    /// Whether the engine's word that this song has started was acted on already, when the
    /// window was brought up to the sound for another device's sake. Asked once for each
    /// such word.
    pub fn link_caught_up(&self, token: u64) -> bool {
        if !on() || token == 0 {
            return false;
        }
        let link = self.link.borrow();
        let caught = link.caught.get() == token;
        if caught {
            link.caught.set(0);
        }
        caught
    }

    // ---- steering another device from here ---------------------------------------------------

    /// The name of the device the player on this PC is steering, if it is steering one.
    pub fn link_steered(&self) -> Option<String> {
        self.link.try_borrow().ok()?.remote.try_borrow().ok()?.as_ref().map(|shown| shown.name.clone())
    }

    /// Stops steering, leaving the other device as it is. The player is this PC's own
    /// again. Says whether there was anything to stop.
    pub fn link_let_go(self: &Rc<Self>) -> bool {
        if !on() || self.link_steered().is_none() {
            return false;
        }
        let asked = self.link.borrow().inner.borrow_mut().device.let_go();
        let was = !asked.is_empty();
        self.link_do(asked);
        was
    }

    /// While another device is being steered, shows it in the player and the queue panel in
    /// place of this PC's own queue, and returns true. Otherwise does nothing.
    pub fn link_show_remote(&self) -> bool {
        let now_ms = now_ms();
        let drawn = {
            let Ok(link) = self.link.try_borrow() else { return false };
            let Ok(mut remote) = link.remote.try_borrow_mut() else { return false };
            let Some(Shown { view, known, .. }) = remote.as_mut() else { return false };
            view.as_ref().map(|view| {
                let mut row = |entry: &Entry| {
                    let song = known.entry(entry.id.clone()).or_insert_with(|| crate::search::current().and_then(|index| index.library.songs.iter().find(|song| song.id == entry.id).cloned())).clone();
                    let mut row = match &song {
                        Some(song) => self.track_row(song),
                        None => ui::Track { id: entry.id.as_str().into(), title: entry.title.as_str().into(), artist: entry.artist.as_str().into(), album: entry.album.as_str().into(), duration: clock(entry.duration).into(), progress: -1.0, ..Default::default() },
                    };
                    // Whether it can play offline is this PC's business, not the other's.
                    row.unavailable = false;
                    row
                };
                let now = view.full.now.as_ref().map(&mut row);
                let rows: Vec<ui::Track> = view.full.queue.iter().map(&mut row).collect();
                let ahead = view.full.queue.iter().take_while(|entry| entry.queued).count();
                (now, rows, ahead, view.playing, view.position(now_ms), view.full.now.as_ref().map_or(0.0, |song| f64::from(song.duration)), view.full.context.clone(), view.full.shuffle, view.full.repeat, view.full.volume)
            })
        };
        let player = self.ui.global::<ui::Player>();
        let (now, rows, ahead, playing, position, duration, context, shuffle, repeat, volume) = drawn.unwrap_or_else(|| (None, Vec::new(), 0, false, 0.0, 0.0, String::new(), false, 0, f64::from(player.get_volume())));
        player.set_has_track(now.is_some());
        let now = now.unwrap_or_default();
        player.set_track_id(now.id.clone());
        player.set_track(now);
        player.set_duration(duration as f32);
        player.set_position(position as f32);
        player.set_context(context.into());
        player.set_playing(playing);
        player.set_shuffle(shuffle);
        player.set_repeat(i32::from(repeat));
        player.set_volume(volume as f32);
        player.set_muted(false);
        player.set_buffering(false);
        player.set_reconnecting(false);
        player.set_stalled(false);
        player.set_format("".into());
        self.show_queue(rows, ahead);
        true
    }

    /// While another device is being steered: the song a row of its queue stands for (-1 for
    /// the one playing), as this PC's library knows it. `None` when none is being steered.
    pub fn link_remote_song(&self, index: i32) -> Option<Option<Song>> {
        let link = self.link.try_borrow().ok()?;
        let remote = link.remote.try_borrow().ok()?;
        let shown = remote.as_ref()?;
        let entry = shown.view.as_ref().and_then(|view| if index < 0 { view.full.now.as_ref() } else { view.full.queue.get(index as usize) });
        Some(entry.and_then(|entry| shown.known.get(&entry.id).cloned().flatten()))
    }

    /// What was pressed is asked of the device being steered, if one is: then this returns
    /// true, and the press is not this PC's own. (If the deciding is at work, which no
    /// press arrives during, the press is still not this PC's own.)
    fn link_steering(self: &Rc<Self>, ask: impl FnOnce(&mut Device, &mut Env) -> Option<Vec<Do>>) -> bool {
        if self.link_steered().is_none() {
            return false;
        }
        match self.link_with(ask) {
            Some(Some(asked)) => self.link_do(asked),
            Some(None) => return false,
            None => {}
        }
        true
    }

    /// While another device is being steered, asks it to do `what` and returns true.
    /// Otherwise returns false, and the caller does it to this PC's own playback (or asks
    /// the jam it is in).
    pub fn link_remote(self: &Rc<Self>, what: &Remote) -> bool {
        let press = press_of(what);
        self.link_steering(|device, env| device.press(press, env))
    }

    /// While another device is being steered, queues songs there and returns true.
    pub fn link_remote_add(self: &Rc<Self>, songs: &[Song], play_next: bool) -> bool {
        let ids: Vec<String> = songs.iter().filter(|song| song.radio_url.is_empty()).map(|song| song.id.clone()).collect();
        self.link_steering(|device, env| device.add(&ids, play_next, env))
    }

    /// While another device is being steered, playing something here plays it there, as it
    /// would here, taking the queue's place; and this returns true.
    pub fn link_remote_play(self: &Rc<Self>, songs: &[Song], start: usize, context: &str) -> bool {
        // (A radio station is not something another device can be sent: it has no id to go by.)
        let ids: Vec<String> = songs.iter().map(|song| if song.radio_url.is_empty() { song.id.clone() } else { String::new() }).collect();
        self.link_steering(|device, env| device.play(&ids, start, context, env))
    }

    /// Brings what the device being steered is playing over to this PC, and stops steering
    /// it. Says whether a device was being steered.
    pub fn link_play_here(self: &Rc<Self>) -> bool {
        self.link_steering(|device, env| device.play_here(env))
    }

    /// A device was picked in the list. Another device: this PC starts steering it, and if
    /// music is playing here it carries on over there. This PC: what the other was playing
    /// carries on here.
    fn link_choose(self: &Rc<Self>, place: &str) {
        if place.is_empty() {
            self.link_play_here();
            return;
        }
        let in_a_jam = self.in_a_jam();
        if !in_a_jam {
            // (A device being steered the old way is let go of.)
            self.leave_jam();
        }
        self.link_event(|device, env| device.choose(place, in_a_jam, env));
        self.link_show();
    }

    /// Shows the other devices and what each is doing.
    fn link_show(self: &Rc<Self>) {
        let Ok((shown, asked)) = self.link.borrow().inner.try_borrow_mut().map(|mut inner| inner.device.shown(now_ms())) else { return };
        self.link_do(asked);
        let list: Vec<ui::LinkedDevice> = shown.rows.into_iter().map(|row| ui::LinkedDevice { place: row.place.into(), name: row.name.into(), about: row.about.into(), playing: row.playing, active: row.active, ready: row.ready }).collect();
        let global = self.ui.global::<ui::Linked>();
        global.set_on(true);
        global.set_entered(shown.entered);
        global.set_note(shown.note.into());
        global.set_steering(shown.steering.into());
        // (Only when it has changed: the list is redrawn a few times a second otherwise.)
        let before: Vec<ui::LinkedDevice> = slint::Model::iter(&global.get_list()).collect();
        if before != list {
            global.set_list(ModelRc::new(VecModel::from(list)));
        }
    }

    pub fn wire_link(self: &Rc<Self>) {
        let linked = self.ui.global::<ui::Linked>();
        linked.on_pause(|place| {
            with(|ctl| {
                ctl.link_event(|device, env| device.pause(place.as_str(), env));
                ctl.link_show();
            });
        });
        linked.on_choose(|place| {
            with(|ctl| ctl.link_choose(place.as_str()));
        });
        linked.on_reset(|| {
            with(|ctl| ctl.link_reset());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The secret is the comment of a playlist, and a playlist's page holds its comment.
    #[test]
    fn a_page_for_a_playlist_the_app_keeps_for_itself_is_never_written_to_disk() {
        use crate::pages::PageData;
        use crate::subsonic::Playlist;
        const RECORD: &str = r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#;
        let dir = std::env::temp_dir().join(format!("nexus-link-page-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let page = |name: &str, comment: &str| PageData { title: name.to_string(), playlist: Some(Playlist { id: "p".into(), name: name.into(), comment: comment.into(), ..Default::default() }), ..Default::default() };
        for (name, comment, kept) in [
            ("Morning mix", "For the walk to work", true),
            ("Nexus Music link", RECORD, false),
            ("Nexus Music sync", "", false),
            // A playlist of the person's own that only has the name, or whose description
            // only looks like a record: theirs, and kept as any other.
            ("Nexus Music link", "", true),
            ("Nexus Music link", "Songs for linking arms to", true),
            ("Road trip", r#"{"nexus_music_link":null}"#, true),
            ("Road trip", r#"{"nexus_music_link":1,"secret":"not one"}"#, true),
            // Renamed on the music server's own page: an ordinary playlist to every listing,
            // and its comment is still the record.
            ("Renamed elsewhere", RECORD, false),
            ("A later layout", r#"{"nexus_music_link":2,"key":"…"}"#, false),
        ] {
            let file = dir.join(format!("{name}-{}", comment.len()));
            crate::controller::store_page(&file, &page(name, comment));
            assert_eq!(file.exists(), kept, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A page written to disk before any of this was known (by a version that listed the
    /// playlist, or while it had another name) is not read back: it is thrown away.
    #[test]
    fn a_page_on_disk_that_holds_the_record_is_thrown_away_unread() {
        use crate::pages::PageData;
        use crate::subsonic::Playlist;
        let dir = std::env::temp_dir().join(format!("nexus-link-page-read-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write = |file: &str, name: &str, comment: &str| {
            let page = PageData { title: name.to_string(), playlist: Some(Playlist { id: "p".into(), name: name.into(), comment: comment.into(), ..Default::default() }), ..Default::default() };
            let path = dir.join(file);
            // (As an earlier version wrote it: straight to disk.)
            std::fs::write(&path, serde_json::to_vec(&page).unwrap()).unwrap();
            path
        };
        let ordinary = write("ordinary", "Morning mix", "For the walk to work");
        assert!(crate::controller::read_page(&ordinary).is_some() && ordinary.exists());
        for (file, name, comment) in [("named", "Nexus Music link", r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#), ("renamed", "Renamed elsewhere", r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#)] {
            let path = write(file, name, comment);
            assert!(crate::controller::read_page(&path).is_none(), "{name} was read back");
            assert!(!path.exists(), "{name} is still on disk");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The music server's `Http` for a look: once the sign-in has ended it makes no request,
    /// and says so in the one way that ends a look.
    #[test]
    fn a_look_s_http_makes_no_request_once_the_sign_in_is_over_or_the_app_is_offline() {
        let over = Arc::new(AtomicBool::new(false));
        let offline = Arc::new(AtomicBool::new(true));
        let said = offline.clone();
        // (An address nothing listens at: a request that was made would say so, not this.)
        let http = LookHttp { agent: crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects()), over: over.clone(), offline: Box::new(move || said.load(Ordering::Relaxed)) };
        let request = Request::get("http://127.0.0.1:9/rest/getPlaylists");
        assert!(matches!(http.send(&request), Err(Failure::Reach(why)) if why == "Offline mode is on."));
        over.store(true, Ordering::Relaxed);
        assert!(matches!(http.send(&request), Err(Failure::Stopped)));
        offline.store(false, Ordering::Relaxed);
        assert!(matches!(http.send(&request), Err(Failure::Stopped)));
    }

    /// The relay's `Http` for a stay in a room: a wait ends as soon as the session does, and
    /// says to stop; and a wait of no length is the question "has it ended?".
    #[test]
    fn a_stay_s_http_stops_its_waits_when_the_session_ends() {
        let stop = Arc::new(AtomicBool::new(false));
        let http = RoomHttp { agent: crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects()), stop: stop.clone() };
        assert_eq!(http.pause(Duration::ZERO), Ok(()));
        let began = std::time::Instant::now();
        assert_eq!(http.pause(Duration::from_millis(250)), Ok(()));
        assert!(began.elapsed() >= Duration::from_millis(250));
        let ender = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            ender.store(true, Ordering::Relaxed);
        });
        let began = std::time::Instant::now();
        assert_eq!(http.pause(Duration::from_secs(60)), Err(Failure::Stopped));
        assert!(began.elapsed() < Duration::from_secs(5));
        assert_eq!(http.pause(Duration::ZERO), Err(Failure::Stopped));
    }

    #[test]
    fn each_press_in_the_player_is_the_same_press_to_the_deciding() {
        assert_eq!(press_of(&Remote::Toggle), Press::Toggle);
        assert_eq!((press_of(&Remote::Play(true)), press_of(&Remote::Play(false))), (Press::Play(true), Press::Play(false)));
        assert_eq!((press_of(&Remote::Next), press_of(&Remote::Previous)), (Press::Next, Press::Previous));
        assert_eq!((press_of(&Remote::Seek(61.5)), press_of(&Remote::Volume(0.5))), (Press::Seek(61.5), Press::Volume(0.5)));
        assert_eq!((press_of(&Remote::Remove(2)), press_of(&Remote::Jump(3))), (Press::Remove(2), Press::Jump(3)));
        assert_eq!((press_of(&Remote::Move(4, 0, true)), press_of(&Remote::Clear(false))), (Press::Move(4, 0, true), Press::Clear(false)));
        assert_eq!((press_of(&Remote::Shuffle), press_of(&Remote::Repeat)), (Press::Shuffle, Press::Repeat));
    }
}
