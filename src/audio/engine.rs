//! The playback engine: one thread that owns the output device, decodes tracks and feeds a
//! short ring buffer the device callback drains.
//!
//! Gapless playback falls out of the design: when a track ends and the next one is already
//! open, its samples go straight into the same ring with no flush in between.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

use super::dsp::{Controls, EQ_BANDS, Effects, soft_clip};
use super::source::{Download, Interrupt};
use crate::diagnostics::{FaultShape, SoundOutput};
use crate::subsonic::Client;

/// How long after pausing an exclusive device is handed back for other apps to use.
#[cfg(windows)]
const EXCLUSIVE_RELEASE_AFTER: Duration = Duration::from_secs(3);
/// How much decoded audio is kept ahead of the device, in seconds.
const RING_SECONDS: f64 = 1.0;
/// Seconds of pause after which the device stream is suspended so Windows can sleep.
const SUSPEND_AFTER: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct TrackRequest {
    /// Identifies this load in events and position reports. Never 0.
    pub token: u64,
    /// A downloaded copy to play instead of streaming.
    pub local: Option<PathBuf>,
    /// When streaming: where to keep the file once it has all arrived, so it needn't be
    /// fetched again, and the fewest bytes the whole song can come to.
    pub keep: Option<(PathBuf, u64)>,
    /// An internet radio stream: endless, with no seeking.
    pub live: bool,
    pub url: String,
    /// True when the server sends the original file, which supports byte ranges.
    pub ranged: bool,
    /// When asking for a conversion: the size of the song's file on the server (0 if not
    /// known). An answer of exactly that many bytes, with byte ranges, is the file itself.
    pub size: u64,
    /// File extension, to help format detection.
    pub suffix: String,
    /// Linear gain applied while decoding (volume normalisation).
    pub gain: f32,
    /// Where in the track to begin.
    pub start_secs: f64,
    /// Added to reported positions; used when the server already skipped ahead for us.
    pub base_secs: f64,
    /// The track's length, to know when a crossfade into the next one should begin.
    pub duration_secs: f64,
    /// It is the next track of the same album as the one before it. Such tracks run on as
    /// the album has them, without a crossfade: a live album or a continuous mix mustn't be
    /// blended at every track mark.
    pub continues: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    /// This track is now the one coming out of the speakers.
    Started(u64),
    /// Everything queued in the engine has finished playing.
    Finished,
    /// `network` is whether fetching the track is what went wrong, as against playing it.
    Failed { token: u64, message: String, unsupported: bool, network: bool, shape: FaultShape },
    /// A radio station announced what it's playing.
    StreamTitle { token: u64, title: String },
    /// The output device was (re)opened: how it's being driven, in words.
    Output { status: String, exclusive: bool, kind: SoundOutput },
    /// There is nothing to play through.
    NoDevice,
}

struct Segment {
    token: u64,
    /// Device frame, counted from when the app started, at which this track begins.
    start_frame: u64,
    /// Track time at `start_frame`.
    offset: f64,
}

/// The segment audible once `played` frames have gone out, discarding the ones left behind.
fn audible(segments: &mut VecDeque<Segment>, played: u64) -> Option<&Segment> {
    while segments.len() > 1 && segments[1].start_frame <= played {
        segments.pop_front();
    }
    segments.front().filter(|s| s.start_frame <= played)
}

/// Room for a second of stereo at any sample rate a device will take (a power of two).
const RING_SLOTS: usize = 1 << 20;

/// Decoded audio on its way to the device: interleaved stereo at the device's sample rate.
/// The decoder's thread writes and the device's callback reads, and neither ever waits for
/// the other: there is no lock, only counts of how much has been written and read.
struct Ring {
    slots: Box<[AtomicU32]>,
    /// Samples written and read since the app started.
    written: AtomicU64,
    read: AtomicU64,
    /// Everything written before this was thrown away by a seek or a new track; the reader
    /// skips to here.
    void_before: AtomicU64,
    /// How many frames may wait at once: a second of audio at the device's rate.
    capacity_frames: AtomicUsize,
}

impl Ring {
    fn new() -> Ring {
        Ring {
            slots: (0..RING_SLOTS).map(|_| AtomicU32::new(0)).collect(),
            written: AtomicU64::new(0),
            read: AtomicU64::new(0),
            void_before: AtomicU64::new(0),
            capacity_frames: AtomicUsize::new(48_000),
        }
    }

    fn set_capacity(&self, frames: usize) {
        self.capacity_frames.store(frames.clamp(1, RING_SLOTS / 2), Ordering::Relaxed);
    }

    /// Frames written since the app started.
    fn written_frames(&self) -> u64 {
        self.written.load(Ordering::Acquire) / 2
    }

    /// Frames that have gone to the device, or been thrown away, since the app started.
    fn played_frames(&self) -> u64 {
        self.read.load(Ordering::Acquire).max(self.void_before.load(Ordering::Acquire)) / 2
    }

    fn waiting_frames(&self) -> usize {
        (self.written_frames() - self.played_frames()) as usize
    }

    fn free_frames(&self) -> usize {
        self.capacity_frames.load(Ordering::Relaxed).saturating_sub(self.waiting_frames())
    }

    /// Throws away what hasn't been played. For the writer's side.
    fn clear(&self) {
        self.void_before.store(self.written.load(Ordering::Relaxed), Ordering::Release);
    }

    /// Adds samples. For the writer's side, which checks `free_frames` first.
    fn push(&self, samples: impl Iterator<Item = f32>) {
        let mut at = self.written.load(Ordering::Relaxed);
        for sample in samples {
            self.slots[at as usize & (RING_SLOTS - 1)].store(sample.to_bits(), Ordering::Relaxed);
            at += 1;
        }
        self.written.store(at, Ordering::Release);
    }

    /// Moves up to `frames` frames into `out`. For the reader's side. Returns how many frames
    /// are still waiting afterwards.
    fn pop(&self, out: &mut Vec<f32>, frames: usize) -> usize {
        let read = self.read.load(Ordering::Relaxed).max(self.void_before.load(Ordering::Acquire));
        let waiting = ((self.written.load(Ordering::Acquire) - read) / 2) as usize;
        let take = frames.min(waiting);
        out.extend((0..take as u64 * 2).map(|i| f32::from_bits(self.slots[(read + i) as usize & (RING_SLOTS - 1)].load(Ordering::Relaxed))));
        self.read.store(read + take as u64 * 2, Ordering::Release);
        waiting - take
    }
}

/// Which device to play through, and how.
#[derive(Clone, Default, PartialEq)]
struct OutputChoice {
    /// A cpal device id; empty for the system's default device.
    device: String,
    /// Take the device for this app alone and feed it each song at its own sample rate.
    exclusive: bool,
}

pub(super) struct Shared {
    choice: Mutex<OutputChoice>,
    ring: Ring,
    /// Where each track begins in what has been written to the ring. Never touched by the
    /// device's callback.
    segments: Mutex<VecDeque<Segment>>,
    /// Raised while a command is waiting, so the engine's thread stops waiting on a download.
    interrupt: Arc<Interrupt>,
    playing: AtomicBool,
    volume: AtomicU32,
    out_rate: AtomicU32,
    /// Token of the track the decoder is currently reading; 0 when idle.
    decoding: AtomicU64,
    /// The same, when that track can be sought in without asking for it again: it's a file
    /// on disk, or the server sent it as it is and takes byte ranges.
    in_place: AtomicU64,
    /// Set by the device error callback when the stream must be rebuilt.
    pub(super) rebuild: AtomicBool,
    /// Seconds of overlap between one track and the next; 0 for none.
    crossfade: AtomicU32,
    effects: Controls,
}

impl Shared {
    fn new() -> Shared {
        Shared {
            choice: Mutex::new(OutputChoice::default()),
            ring: Ring::new(),
            segments: Mutex::default(),
            interrupt: Arc::default(),
            playing: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            out_rate: AtomicU32::new(48_000),
            decoding: AtomicU64::new(0),
            in_place: AtomicU64::new(0),
            rebuild: AtomicBool::new(false),
            crossfade: AtomicU32::new(0f32.to_bits()),
            effects: Controls::new(),
        }
    }

    /// The audible track and the position within it, in seconds.
    fn position(&self) -> Option<(u64, f64)> {
        let rate = self.out_rate.load(Ordering::Relaxed).max(1) as f64;
        let played = self.ring.played_frames();
        let mut segments = self.segments.lock().unwrap();
        audible(&mut segments, played).map(|s| (s.token, s.offset + (played - s.start_frame) as f64 / rate))
    }

    /// Throws away what's waiting to be played and starts counting a track from here.
    fn restart_at(&self, token: u64, offset: f64) {
        self.ring.clear();
        let mut segments = self.segments.lock().unwrap();
        segments.clear();
        segments.push_back(Segment { token, start_frame: self.ring.written_frames(), offset });
    }
}

enum Slot {
    Current,
    Next,
}

enum Cmd {
    Load(Client, TrackRequest),
    SetNext(Option<(Client, TrackRequest)>),
    Opened { token: u64, slot: Slot, result: Result<Box<Deck>, OpenError> },
    /// Where to go in the track with this token; ignored if another is playing by then.
    Seek(u64, f64),
    Stop,
    Wake,
}

struct OpenError {
    message: String,
    unsupported: bool,
    network: bool,
    shape: FaultShape,
}

/// An opened track: its download, demuxer and decoder.
struct Deck {
    token: u64,
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    gain: f32,
    /// Track time the first decoded packet corresponds to.
    start: f64,
    base: f64,
    duration: f64,
    /// Track time decoded so far.
    time: f64,
    /// The file's own sample rate and bit depth; 0 when it doesn't say.
    rate: u32,
    bits: u32,
    /// See `TrackRequest::continues`.
    continues: bool,
    // Declared last so the reader inside `format` is dropped before the file is deleted.
    // `None` for a downloaded copy, which is read straight from disk.
    download: Option<Download>,
}

impl Deck {
    /// See `Shared::in_place`.
    fn in_place(&self) -> bool {
        self.download.as_ref().is_none_or(Download::ranged)
    }
}

pub struct Engine {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
}

impl Engine {
    pub fn new(buffer_dir: PathBuf, on_event: impl Fn(Event) + Send + 'static) -> Engine {
        // With no sound card, NEXUS_AUDIO_DUMP=<file> records what would have been played.
        let record_to = std::env::var_os("NEXUS_AUDIO_DUMP").map(PathBuf::from);
        Engine::recording_to(buffer_dir, record_to, on_event)
    }

    /// An engine that, if there is no sound device to be had, plays into the file named:
    /// raw 32-bit float stereo, at the rate the engine works at. That is how its output is
    /// checked without listening to it.
    pub fn recording_to(buffer_dir: PathBuf, record_to: Option<PathBuf>, on_event: impl Fn(Event) + Send + 'static) -> Engine {
        let shared = Arc::new(Shared::new());
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            rx,
            tx: tx.clone(),
            shared: shared.clone(),
            buffer_dir,
            record_to,
            on_event: Box::new(on_event),
            output: None,
            current: None,
            next: None,
            ended: false,
            failed: false,
            load_token: 0,
            next_token: 0,
            next_continues: false,
            announced: 0,
            pending: VecDeque::new(),
            resampler: None,
            samples: Vec::new(),
            stereo: Vec::new(),
            paused_since: None,
            suspended: false,
            last_device_retry: Instant::now(),
            no_device: false,
            load_started: None,
            tail: Vec::new(),
            capturing_tail: false,
            fade: None,
            out: Vec::new(),
            opened_for: 0,
            bits_asked: 0,
            bits_given: 0,
            rate_switch: false,
            stream_title: String::new(),
            watched: Instant::now(),
        };
        std::thread::Builder::new()
            .name("audio-engine".into())
            .spawn(move || worker.run())
            .expect("failed to start the audio thread");
        Engine { tx, shared }
    }

    /// Replaces whatever is playing with this track.
    pub fn load(&self, client: Client, request: TrackRequest) {
        self.urgent(Cmd::Load(client, request));
    }

    /// Sends a command that replaces what the engine is doing. If its thread is waiting on a
    /// download that isn't arriving, it stops waiting and takes the command at once.
    fn urgent(&self, cmd: Cmd) {
        self.shared.interrupt.raise();
        if self.tx.send(cmd).is_err() {
            self.shared.interrupt.lower();
        }
    }

    /// Names the track to continue with, gaplessly, when the current one ends.
    pub fn set_next(&self, next: Option<(Client, TrackRequest)>) {
        let _ = self.tx.send(Cmd::SetNext(next));
    }

    /// Goes to `secs` into the track with this token, if it's still the one playing when the
    /// engine gets to it, not the one after it.
    pub fn seek(&self, token: u64, secs: f64) {
        self.urgent(Cmd::Seek(token, secs));
    }

    pub fn stop(&self) {
        self.urgent(Cmd::Stop);
    }

    pub fn set_playing(&self, playing: bool) {
        self.shared.playing.store(playing, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Wake);
    }

    /// Chooses the output device (a cpal device id, or empty for the system default) and
    /// whether to take it for exclusive use.
    pub fn set_output(&self, device: &str, exclusive: bool) {
        let choice = OutputChoice { device: device.to_string(), exclusive };
        let mut current = self.shared.choice.lock().unwrap();
        if *current != choice {
            *current = choice;
            drop(current);
            self.shared.rebuild.store(true, Ordering::Relaxed);
            let _ = self.tx.send(Cmd::Wake);
        }
    }

    /// Linear output gain, 0.0 to 1.0.
    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Seconds of overlap when one track runs into the next. 0 turns crossfading off.
    pub fn set_crossfade(&self, seconds: f32) {
        self.shared.crossfade.store(seconds.clamp(0.0, 12.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_mono(&self, mono: bool) {
        self.shared.effects.mono.store(mono, Ordering::Relaxed);
    }

    /// Gains in dB for the bands in `EQ_BANDS`.
    pub fn set_equalizer(&self, on: bool, gains: [f32; EQ_BANDS.len()], preamp: f32) {
        self.shared.effects.set_equalizer(on, gains, preamp);
    }

    /// The audible track and the position within it, in seconds.
    pub fn position(&self) -> Option<(u64, f64)> {
        self.shared.position()
    }

    /// Token of the track the decoder has reached, which runs ahead of the audible one near
    /// the end of a track.
    pub fn decoding_token(&self) -> u64 {
        self.shared.decoding.load(Ordering::Relaxed)
    }

    /// Whether the track with this token is the one being decoded and can be sought in
    /// without being asked for again. (A conversion can't, unless the server sent the file
    /// as it is after all.)
    pub fn seeks_in_place(&self, token: u64) -> bool {
        self.shared.decoding.load(Ordering::Relaxed) == token && self.shared.in_place.load(Ordering::Relaxed) == token
    }
}

/// The output devices on this PC: (id, name).
pub fn output_devices() -> Vec<(String, String)> {
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else { return Vec::new() };
    devices
        .filter_map(|device| {
            let id = device.id().ok()?.to_string();
            let name = device.description().ok()?.name().to_string();
            Some((id, name))
        })
        .collect()
}

enum Output {
    Device(cpal::Stream),
    /// The device is ours alone, running at the song's sample rate.
    #[cfg(windows)]
    Exclusive(#[allow(dead_code)] super::exclusive::ExclusiveStream),
    /// No usable device: a thread consumes audio at real-time speed so playback still advances.
    Null(Arc<AtomicBool>),
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Output::Null(stop) = self {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

struct ResamplerState {
    inner: Fft<f32>,
    from_rate: u32,
    to_rate: u32,
    input: Vec<f32>,
    output: Vec<f32>,
    /// Output frames still to drop to hide the filter's start-up delay.
    skip: usize,
    /// Frames taken in and frames handed on so far, to end on exactly the right one.
    fed: u64,
    given: u64,
}

struct Worker {
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    buffer_dir: PathBuf,
    /// Where to play to when there's no sound device (see `Engine::recording_to`).
    record_to: Option<PathBuf>,
    on_event: Box<dyn Fn(Event) + Send>,
    output: Option<Output>,
    current: Option<Box<Deck>>,
    next: Option<Box<Deck>>,
    /// The current deck has been decoded to its end.
    ended: bool,
    /// The current deck stopped because it failed. The player has been told and decides what
    /// happens next, so its running out isn't also reported as the queue finishing.
    failed: bool,
    load_token: u64,
    next_token: u64,
    /// The track named to come next carries on the album that's playing (no crossfade).
    next_continues: bool,
    announced: u64,
    /// Stereo samples at the device rate that didn't fit in the ring yet.
    pending: VecDeque<f32>,
    resampler: Option<ResamplerState>,
    samples: Vec<f32>,
    stereo: Vec<f32>,
    paused_since: Option<Instant>,
    suspended: bool,
    last_device_retry: Instant,
    /// The last attempt to open a device found none.
    no_device: bool,
    /// When the current load was requested, until its first audio is queued. For timing.
    load_started: Option<Instant>,
    /// The closing seconds of the current track, held back to blend into the next one.
    tail: Vec<f32>,
    capturing_tail: bool,
    /// A crossfade in progress: the previous track's tail and how far through it we are.
    fade: Option<Fade>,
    /// Scratch space for audio on its way out of the decoder.
    out: Vec<f32>,
    /// The song sample rate the output was last opened for (exclusive mode follows the song),
    /// the bit depth that was asked for then, and the one the device gave.
    opened_for: u32,
    bits_asked: u32,
    bits_given: u32,
    /// The next track needs the device at a different rate: wait for the current one to
    /// play out, then reopen.
    rate_switch: bool,
    /// What the radio station last said it was playing.
    stream_title: String,
    /// When the downloads were last checked for having gone quiet.
    watched: Instant,
}

struct Fade {
    tail: Vec<f32>,
    /// Frames of `tail` already blended.
    done: usize,
}

/// Prints start-up timings to stderr when NEXUS_TRACE is set. A diagnostic, off by default.
fn trace(what: &str, since: Instant) {
    if std::env::var_os("NEXUS_TRACE").is_some() {
        eprintln!("[trace] {what}: {} ms", since.elapsed().as_millis());
    }
}

impl Worker {
    fn run(mut self) {
        self.shared.interrupt.claim();
        // The device is opened when the first track loads, by which time the settings have
        // said which device to use and how.
        loop {
            let busy = self.current.is_some() && (!self.ended || !self.pending.is_empty());
            let can_decode = busy && self.flush_pending();
            let wait = if can_decode {
                Duration::ZERO
            } else if self.current.is_some() && self.shared.playing.load(Ordering::Relaxed) {
                // The ring is full: come back as the device drains it.
                Duration::from_millis(15)
            } else {
                // Paused or idle. Commands and play/pause wake the thread at once.
                Duration::from_millis(250)
            };
            let cmd = if wait.is_zero() {
                match self.rx.try_recv() {
                    Ok(cmd) => Some(cmd),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            } else {
                match self.rx.recv_timeout(wait) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            if let Some(cmd) = cmd {
                self.handle(cmd);
                continue;
            }
            self.housekeeping();
            if can_decode && !self.ended {
                self.decode_step();
            }
        }
    }

    fn emit(&self, event: Event) {
        (self.on_event)(event);
    }

    // ---- commands ---------------------------------------------------------------------------

    fn handle(&mut self, cmd: Cmd) {
        if matches!(cmd, Cmd::Load(..) | Cmd::Seek(..) | Cmd::Stop) {
            self.shared.interrupt.lower();
        }
        match cmd {
            Cmd::Load(client, request) => {
                self.reset();
                self.load_token = request.token;
                self.load_started = Some(Instant::now());
                let retry = matches!(self.output, Some(Output::Null(_)))
                    && self.last_device_retry.elapsed() > Duration::from_secs(3);
                // In exclusive mode the device is opened once the track's format is known.
                if (self.output.is_none() || retry) && !self.exclusive_wanted() {
                    self.open_output();
                }
                self.spawn_open(client, request, Slot::Current);
            }
            Cmd::SetNext(next) => {
                self.next = None;
                self.next_token = 0;
                self.next_continues = false;
                if let Some((client, request)) = next {
                    self.next_token = request.token;
                    self.next_continues = request.continues;
                    self.spawn_open(client, request, Slot::Next);
                }
            }
            Cmd::Opened { token, slot: Slot::Current, result } => {
                if token != self.load_token || self.current.is_some() {
                    return;
                }
                match result {
                    Ok(deck) => {
                        if let Some(since) = self.load_started {
                            trace("track opened", since);
                        }
                        if self.exclusive_wanted() && (self.output.is_none() || self.wrong_rate_for(&deck)) {
                            self.open_output_for(Some((deck.rate, deck.bits)));
                        }
                        self.shared.restart_at(token, deck.base + deck.start);
                        self.shared.in_place.store(if deck.in_place() { token } else { 0 }, Ordering::Relaxed);
                        self.shared.decoding.store(token, Ordering::Relaxed);
                        self.current = Some(deck);
                        self.ended = false;
                    }
                    Err(e) => self.emit(Event::Failed {
                        token,
                        message: e.message,
                        unsupported: e.unsupported,
                        network: e.network,
                        shape: e.shape,
                    }),
                }
            }
            Cmd::Opened { token, slot: Slot::Next, result } => {
                if token != self.next_token {
                    return;
                }
                match result {
                    Ok(deck) => {
                        let switch = self.exclusive_wanted() && self.wrong_rate_for(&deck);
                        self.next = Some(deck);
                        if self.ended && self.current.is_some() {
                            if switch {
                                // Can't run straight on at a different rate: let the ending
                                // play out first (see `housekeeping`).
                                self.drain_resampler();
                                self.release_tail();
                                self.rate_switch = true;
                            } else {
                                self.promote_next();
                            }
                        }
                    }
                    Err(_) => {
                        // Forget it; the player retries this track as a normal load when the
                        // current one finishes, and reports the failure then.
                        self.next_token = 0;
                        if self.ended {
                            self.drain_resampler();
                            self.release_tail();
                        }
                    }
                }
            }
            Cmd::Seek(token, secs) => self.seek(token, secs),
            Cmd::Stop => self.reset(),
            Cmd::Wake => {}
        }
    }

    fn reset(&mut self) {
        self.current = None;
        self.next = None;
        self.ended = false;
        self.failed = false;
        self.load_token = 0;
        self.next_token = 0;
        self.next_continues = false;
        self.announced = 0;
        self.pending.clear();
        self.resampler = None;
        self.rate_switch = false;
        self.clear_fade();
        self.shared.decoding.store(0, Ordering::Relaxed);
        self.shared.ring.clear();
        self.shared.segments.lock().unwrap().clear();
    }

    fn spawn_open(&self, client: Client, request: TrackRequest, slot: Slot) {
        let tx = self.tx.clone();
        let dir = self.buffer_dir.clone();
        let token = request.token;
        let interrupt = self.shared.interrupt.clone();
        let spawned = std::thread::Builder::new().name("track-open".into()).spawn(move || {
            let result = open_deck(client, request, &dir, interrupt).map(Box::new);
            let _ = tx.send(Cmd::Opened { token, slot, result });
        });
        if spawned.is_err() {
            self.emit(Event::Failed {
                token,
                message: "Couldn't start a thread to open the track.".into(),
                unsupported: false,
                network: false,
                shape: FaultShape::Unavailable,
            });
        }
    }

    fn seek(&mut self, token: u64, secs: f64) {
        let Some(deck) = self.current.as_mut().filter(|deck| deck.token == token) else { return };
        let target = (secs - deck.base).max(0.0);
        let time = Time::try_from_secs_f64(target).unwrap_or(Time::ZERO);
        let landed = match deck
            .format
            .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(deck.track_id) })
        {
            Ok(to) => {
                deck.decoder.reset();
                deck.time_base
                    .and_then(|tb| tb.calc_time(to.actual_ts))
                    .map(|t| t.as_secs_f64())
                    .unwrap_or(target)
            }
            // Seeking past the end or on an unseekable stream: carry on from where we are.
            Err(_) => return,
        };
        deck.time = deck.base + landed;
        let (token, offset) = (deck.token, deck.base + landed);
        self.ended = false;
        self.pending.clear();
        self.resampler = None;
        self.clear_fade();
        self.shared.restart_at(token, offset);
    }

    // ---- periodic work ----------------------------------------------------------------------

    fn housekeeping(&mut self) {
        if self.shared.rebuild.swap(false, Ordering::Relaxed) {
            let position = self.audible_position();
            let format = self.current.as_ref().map(|deck| (deck.rate, deck.bits));
            if self.current.is_none() && self.exclusive_wanted() {
                // Nothing to play: leave the device free for other apps until there is.
                self.output = None;
                self.emit(Event::Output {
                    status: "Exclusive mode is on. The device is taken while a song plays.".into(),
                    exclusive: false,
                    kind: SoundOutput::WaitingForExclusive,
                });
            } else {
                self.open_output_for(format);
            }
            // Buffered audio was produced for the old device's sample rate.
            if let Some((token, secs)) = position {
                self.seek(token, secs);
            }
        }

        // Suspend the device after a long pause; resume on play.
        let playing = self.shared.playing.load(Ordering::Relaxed);
        if playing {
            self.paused_since = None;
            if self.suspended {
                if let Some(Output::Device(stream)) = &self.output {
                    let _ = stream.play();
                }
                self.suspended = false;
            }
            if self.output.is_none() && (!self.no_device || self.last_device_retry.elapsed() > Duration::from_secs(1)) {
                // The device was handed back while paused, or there was none to be had: take
                // it now, at the same rate so the audio already buffered still fits.
                let format = self.current.as_ref().map(|deck| (deck.rate, deck.bits));
                self.open_output_for(format);
            }
        } else {
            let since = *self.paused_since.get_or_insert_with(Instant::now);
            #[cfg(windows)]
            if matches!(self.output, Some(Output::Exclusive(_))) && since.elapsed() > EXCLUSIVE_RELEASE_AFTER {
                // Don't keep other apps silent while nothing is playing.
                self.output = None;
            }
            if !self.suspended && since.elapsed() > SUSPEND_AFTER {
                if let Some(Output::Device(stream)) = &self.output {
                    let _ = stream.pause();
                }
                self.suspended = true;
            }
        }

        // See that what's being fetched is still arriving, while there's audio in hand.
        if self.watched.elapsed() > Duration::from_millis(500) {
            self.watched = Instant::now();
            for deck in self.current.iter().chain(self.next.iter()) {
                if let Some(download) = &deck.download {
                    download.watch();
                }
            }
        }

        // Pass on what a radio station says it's playing.
        if let Some(deck) = self.current.as_ref() {
            if let Some(title) = deck.download.as_ref().map(|d| d.title()).filter(|t| *t != self.stream_title) {
                self.stream_title = title.clone();
                self.emit(Event::StreamTitle { token: deck.token, title });
            }
        }

        // Announce track changes as playback crosses them, and the end of the queue.
        let drained = self.shared.ring.waiting_frames() == 0;
        let audible = self.shared.position().map(|(token, _)| token);
        if let Some(token) = audible {
            if token != self.announced {
                self.announced = token;
                self.emit(Event::Started(token));
            }
        }
        if self.ended && self.pending.is_empty() && drained && self.current.is_some() {
            if self.rate_switch && self.next.is_some() {
                // The old track has played out; reopen the device at the new track's rate.
                self.rate_switch = false;
                let format = self.next.as_ref().map(|deck| (deck.rate, deck.bits));
                self.open_output_for(format);
                self.resampler = None;
                self.clear_fade();
                self.shared.ring.clear();
                self.promote_next();
                return;
            }
            if self.next_token != 0 {
                // The next track is still opening; it starts as soon as it arrives.
                return;
            }
            self.current = None;
            self.ended = false;
            self.shared.decoding.store(0, Ordering::Relaxed);
            if !std::mem::take(&mut self.failed) {
                self.emit(Event::Finished);
            }
        }
    }

    fn audible_position(&self) -> Option<(u64, f64)> {
        self.shared.position()
    }

    /// Moves pending samples into the ring. Returns true when everything fit.
    fn flush_pending(&mut self) -> bool {
        if self.pending.is_empty() {
            return true;
        }
        let frames = self.shared.ring.free_frames().min(self.pending.len() / 2);
        self.shared.ring.push(self.pending.drain(..frames * 2));
        if frames > 0 {
            if let Some(since) = self.load_started.take() {
                trace("first audio queued", since);
            }
        }
        self.pending.is_empty()
    }

    // ---- decoding ---------------------------------------------------------------------------

    fn decode_step(&mut self) {
        let Some(deck) = self.current.as_mut() else { return };
        let packet = match deck.format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) | Err(SymError::ResetRequired) => return self.end_of_deck(),
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return self.end_of_deck();
            }
            // The wait for more of the file was cut short because a command is waiting. Whatever
            // it is (another track, a seek, stop) replaces this read.
            Err(_) if self.shared.interrupt.raised() => return,
            Err(e) => {
                let token = deck.token;
                let network = deck.download.as_ref().is_some_and(|download| download.troubled());
                // Whatever was decoded so far plays out; the player decides what follows.
                self.emit(Event::Failed {
                    token,
                    message: if network { format!("{e}") } else { format!("Playback stopped: {e}") },
                    unsupported: false,
                    network,
                    shape: if network { FaultShape::Connection } else { FaultShape::Read },
                });
                self.failed = true;
                if network {
                    // The song isn't over, its stream is: don't run on into the next one.
                    // What was decoded plays out while the player sees about getting it back.
                    self.drain_resampler();
                    self.release_tail();
                    self.ended = true;
                    return;
                }
                return self.end_of_deck();
            }
        };
        if packet.track_id != deck.track_id {
            return;
        }
        let mut samples = std::mem::take(&mut self.samples);
        let (rate, channels) = match deck.decoder.decode(&packet) {
            Ok(decoded) => {
                decoded.copy_to_vec_interleaved(&mut samples);
                (decoded.spec().rate(), decoded.spec().channels().count())
            }
            // A damaged packet: skip it and keep going.
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => {
                self.samples = samples;
                return;
            }
            Err(e) => {
                self.samples = samples;
                let token = deck.token;
                self.emit(Event::Failed {
                    token,
                    message: format!("Playback stopped: {e}"),
                    unsupported: false,
                    network: false,
                    shape: FaultShape::Decode,
                });
                self.failed = true;
                return self.end_of_deck();
            }
        };
        let gain = deck.gain;
        if channels > 0 && rate > 0 {
            // Encoder delay and padding, so albums play through without gaps.
            let frames = samples.len() / channels;
            let (head, tail) = trims(frames, packet.dur.get(), packet.trim_start.get(), packet.trim_end.get());
            let body = &samples[head * channels..(frames - tail) * channels];
            deck.time += (frames - head - tail) as f64 / rate as f64;
            // Within a crossfade's length of the end, audio is set aside for blending
            // instead of being queued.
            let overlap = crossfade_for(&self.shared, deck.duration);
            if overlap > 0.0 && deck.time >= deck.duration - overlap && !self.next_continues {
                self.capturing_tail = true;
            }
            self.feed(body, rate, channels, gain);
        }
        self.samples = samples;
    }

    /// Converts decoded samples to stereo at the device rate and queues them.
    fn feed(&mut self, samples: &[f32], rate: u32, channels: usize, gain: f32) {
        let mut stereo = std::mem::take(&mut self.stereo);
        stereo.clear();
        match channels {
            // Audio is only rounded off at the limiter when it has been turned up; untouched
            // or turned down, samples pass through exactly as decoded.
            1 => {
                for &s in samples {
                    let v = if gain > 1.0 { soft_clip(s * gain) } else { s * gain };
                    stereo.extend_from_slice(&[v, v]);
                }
            }
            2 if gain > 1.0 => stereo.extend(samples.iter().map(|&s| soft_clip(s * gain))),
            2 if gain == 1.0 => stereo.extend_from_slice(samples),
            2 => stereo.extend(samples.iter().map(|&s| s * gain)),
            _ => {
                // Fold surround down: fronts plus a share of everything else.
                for frame in samples.chunks_exact(channels) {
                    let mut l = frame[0];
                    let mut r = frame[1];
                    for (i, &s) in frame.iter().enumerate().skip(2) {
                        match i {
                            2 => {
                                l += s * 0.707;
                                r += s * 0.707;
                            }
                            3 => {}
                            _ if i % 2 == 0 => l += s * 0.5,
                            _ => r += s * 0.5,
                        }
                    }
                    stereo.push(soft_clip(l * 0.6 * gain));
                    stereo.push(soft_clip(r * 0.6 * gain));
                }
            }
        }

        let out_rate = self.shared.out_rate.load(Ordering::Relaxed);
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        if rate == out_rate {
            self.drain_resampler();
            out.extend_from_slice(&stereo);
        } else {
            if self.resampler.as_ref().is_some_and(|r| r.from_rate != rate) {
                self.drain_resampler();
            }
            if self.resampler.is_none() {
                self.resampler = make_resampler(rate, out_rate);
            }
            match self.resampler.as_mut() {
                Some(rs) => {
                    rs.input.extend_from_slice(&stereo);
                    rs.fed += stereo.len() as u64 / 2;
                    run_resampler(rs, &mut out);
                }
                // Couldn't build one (absurd rates): play at the wrong speed rather than not at all.
                None => out.extend_from_slice(&stereo),
            }
        }
        self.route(&mut out);
        self.out = out;
        self.stereo = stereo;
    }

    /// Sends finished audio on its way: blended with the previous track's tail while a
    /// crossfade is running, then either queued for the device or held back as this track's
    /// own tail.
    fn route(&mut self, out: &mut [f32]) {
        if let Some(fade) = self.fade.as_mut() {
            let total = fade.tail.len() / 2;
            for frame in out.chunks_exact_mut(2) {
                if fade.done >= total {
                    break;
                }
                // Equal-power curves: the combined loudness stays level through the blend.
                let t = fade.done as f32 / total as f32 * std::f32::consts::FRAC_PI_2;
                let (rise, fall) = t.sin_cos();
                frame[0] = soft_clip(frame[0] * rise + fade.tail[fade.done * 2] * fall);
                frame[1] = soft_clip(frame[1] * rise + fade.tail[fade.done * 2 + 1] * fall);
                fade.done += 1;
            }
            if fade.done >= total {
                self.fade = None;
            }
        }
        if self.capturing_tail {
            self.tail.extend_from_slice(out);
        } else {
            self.pending.extend(out.iter().copied());
        }
    }

    fn clear_fade(&mut self) {
        self.tail.clear();
        self.capturing_tail = false;
        self.fade = None;
    }

    /// Pushes the resampler's remaining audio out and retires it.
    fn drain_resampler(&mut self) {
        let Some(rs) = self.resampler.take() else { return };
        let mut out = finish_resampler(rs);
        self.route(&mut out);
    }

    fn end_of_deck(&mut self) {
        let switch = self.exclusive_wanted() && self.next.as_ref().is_some_and(|deck| self.wrong_rate_for(deck));
        if switch {
            self.drain_resampler();
            self.release_tail();
            self.ended = true;
            self.rate_switch = true;
        } else if self.next.is_some() {
            self.promote_next();
        } else {
            if self.next_token == 0 {
                self.drain_resampler();
                // Nothing to blend into: the held-back ending plays as recorded.
                self.release_tail();
            }
            self.ended = true;
        }
    }

    /// Queues the held-back tail unchanged.
    fn release_tail(&mut self) {
        self.capturing_tail = false;
        let tail = std::mem::take(&mut self.tail);
        self.pending.extend(tail);
    }

    /// Continues straight into the already-opened next track: gapless, or blended over the
    /// held-back tail when crossfading.
    fn promote_next(&mut self) {
        let Some(deck) = self.next.take() else { return };
        self.next_token = 0;
        self.next_continues = false;
        if deck.continues {
            // The album carries on: whatever was held back plays as recorded, and this
            // track follows it without a blend.
            self.drain_resampler();
            self.release_tail();
        }
        if !self.tail.is_empty() {
            // The tail must be complete, resampler remainder included, before blending.
            self.drain_resampler();
            self.capturing_tail = false;
            let mut tail = std::mem::take(&mut self.tail);
            let rate = self.shared.out_rate.load(Ordering::Relaxed) as f32;
            let wanted = (crossfade_for(&self.shared, deck.duration.max(1.0)) as f32 * rate) as usize;
            let frames = tail.len() / 2;
            if frames > wanted {
                // More was held back than the fade needs: the surplus plays first, as is.
                let surplus = (frames - wanted) * 2;
                self.pending.extend(tail.drain(..surplus));
            }
            if !tail.is_empty() {
                self.fade = Some(Fade { tail, done: 0 });
            }
        }
        let start_frame = self.shared.ring.written_frames() + (self.pending.len() / 2) as u64;
        self.shared.segments.lock().unwrap().push_back(Segment {
            token: deck.token,
            start_frame,
            offset: deck.base + deck.start,
        });
        self.shared.in_place.store(if deck.in_place() { deck.token } else { 0 }, Ordering::Relaxed);
        self.shared.decoding.store(deck.token, Ordering::Relaxed);
        self.current = Some(deck);
        self.ended = false;
        self.failed = false;
    }

    // ---- output device ----------------------------------------------------------------------

    fn open_output(&mut self) {
        self.open_output_for(None);
    }

    fn exclusive_wanted(&self) -> bool {
        cfg!(windows) && self.shared.choice.lock().unwrap().exclusive
    }

    /// Whether the output, as opened, doesn't suit this track: it's at another sample rate,
    /// or it's carrying fewer bits than the track has and more haven't been asked for yet.
    /// (A track with fewer bits than the output loses nothing, so that isn't a reason.)
    fn wrong_rate_for(&self, deck: &Deck) -> bool {
        let rate = deck.rate != 0 && deck.rate != self.opened_for;
        let bits = deck.bits > self.bits_given && deck.bits > self.bits_asked && self.bits_given != 0;
        rate || bits
    }

    /// Opens the chosen device. `format` is the (sample rate, bit depth) of the song about to
    /// play, which exclusive mode matches.
    fn open_output_for(&mut self, format: Option<(u32, u32)>) {
        self.output = None;
        self.no_device = false;
        self.suspended = false;
        self.last_device_retry = Instant::now();
        let choice = self.shared.choice.lock().unwrap().clone();
        let (rate, bits) = format.filter(|f| f.0 != 0).unwrap_or((44_100, 16));
        self.opened_for = rate;
        self.bits_asked = bits;
        self.bits_given = 0;
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut note = String::new();
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut exclusive_refused = false;
        #[cfg(windows)]
        if choice.exclusive {
            let device = choice.device.strip_prefix("wasapi:").unwrap_or("").to_string();
            match super::exclusive::open(self.shared.clone(), device, rate, bits) {
                Ok(stream) => {
                    self.bits_given = stream.bits;
                    self.set_rate(stream.rate);
                    let status = format!("Exclusive, {} at {}-bit", khz(stream.rate), stream.bits);
                    if std::env::var_os("NEXUS_TRACE").is_some() {
                        eprintln!("[trace] output: {status} (asked for {rate} Hz, {bits}-bit)");
                    }
                    let kind = SoundOutput::Exclusive { rate_hz: stream.rate, bits: stream.bits };
                    self.output = Some(Output::Exclusive(stream));
                    self.emit(Event::Output { status, exclusive: true, kind });
                    return;
                }
                Err(reason) => {
                    if std::env::var_os("NEXUS_TRACE").is_some() {
                        eprintln!("[trace] exclusive output at {rate} Hz, {bits}-bit: {reason}");
                    }
                    exclusive_refused = true;
                    note = format!(" Exclusive mode wasn't available ({reason}).");
                }
            }
        }
        let _ = bits;
        let (output, status, kind) = match open_device(&self.shared, &choice.device) {
            Ok((stream, rate)) => {
                self.set_rate(rate);
                let kind = if exclusive_refused {
                    SoundOutput::ExclusiveRefused { rate_hz: rate }
                } else {
                    SoundOutput::Shared { rate_hz: rate }
                };
                (Output::Device(stream), format!("Shared with other apps, {}.{note}", khz(rate)), kind)
            }
            Err(_) => {
                self.set_rate(48_000);
                let Some(record_to) = self.record_to.clone() else {
                    // Nothing to play through. Playing on in silence would move through the
                    // queue and count songs nobody heard, so say so and let the player pause;
                    // the device is looked for again when play is pressed.
                    self.no_device = true;
                    self.emit(Event::Output { status: "No sound device found.".into(), exclusive: false, kind: SoundOutput::NoDevice });
                    self.emit(Event::NoDevice);
                    return;
                };
                // For checking output without a sound card: play to the file named instead.
                (Output::Null(spawn_null_output(self.shared.clone(), record_to)), "No sound device found.".to_string(), SoundOutput::NoDevice)
            }
        };
        self.output = Some(output);
        self.emit(Event::Output { status, exclusive: false, kind });
    }

    fn set_rate(&self, rate: u32) {
        self.shared.out_rate.store(rate, Ordering::Relaxed);
        self.shared.ring.set_capacity((rate as f64 * RING_SECONDS) as usize);
    }
}

fn khz(rate: u32) -> String {
    let khz = rate as f32 / 1000.0;
    if khz.fract() == 0.0 { format!("{khz:.0} kHz") } else { format!("{khz:.1} kHz") }
}

fn make_resampler(from_rate: u32, to_rate: u32) -> Option<ResamplerState> {
    let inner =
        Fft::<f32>::new(from_rate as usize, to_rate as usize, 1024, 2, FixedSync::Input).ok()?;
    let skip = inner.output_delay();
    let output = vec![0.0; inner.output_frames_max() * 2];
    Some(ResamplerState { inner, from_rate, to_rate, input: Vec::new(), output, skip, fed: 0, given: 0 })
}

/// The last of a resampler's audio. Getting it out means pushing silence through behind it;
/// what comes back is cut off at the frame the track really ends on, so none of that silence
/// is played and one track still runs straight into the next.
fn finish_resampler(mut rs: ResamplerState) -> Vec<f32> {
    let chunk = rs.inner.input_frames_next() * 2;
    // Enough silence to flush both the partial chunk and the filter's delay line.
    let padded = (rs.input.len() / chunk + 2) * chunk;
    rs.input.resize(padded, 0.0);
    let mut out = Vec::new();
    run_resampler(&mut rs, &mut out);
    let due = (rs.fed as u128 * rs.to_rate as u128).div_ceil(rs.from_rate.max(1) as u128) as u64;
    let surplus = rs.given.saturating_sub(due) as usize * 2;
    out.truncate(out.len().saturating_sub(surplus));
    out
}

/// The crossfade length to use for a track, in seconds: the setting, but never more than a
/// third of the track so short tracks keep a middle.
fn crossfade_for(shared: &Shared, duration: f64) -> f64 {
    let setting = f32::from_bits(shared.crossfade.load(Ordering::Relaxed)) as f64;
    if setting <= 0.0 || duration <= 0.0 { 0.0 } else { setting.min(duration / 3.0) }
}

fn run_resampler(rs: &mut ResamplerState, out: &mut Vec<f32>) {
    let mut consumed = 0;
    loop {
        let need = rs.inner.input_frames_next();
        if rs.input.len() - consumed < need * 2 {
            break;
        }
        let capacity = rs.output.len() / 2;
        let Ok(input) = InterleavedSlice::new(&rs.input[consumed..consumed + need * 2], 2, need)
        else {
            break;
        };
        let Ok(mut output) = InterleavedSlice::new_mut(&mut rs.output, 2, capacity) else { break };
        let Ok((used, produced)) = rs.inner.process_into_buffer(&input, &mut output, None) else {
            break;
        };
        consumed += used.max(1) * 2;
        let skip = rs.skip.min(produced);
        rs.skip -= skip;
        rs.given += (produced - skip) as u64;
        out.extend_from_slice(&rs.output[skip * 2..produced * 2]);
    }
    rs.input.drain(..consumed.min(rs.input.len()));
}

fn open_deck(client: Client, mut request: TrackRequest, dir: &std::path::Path, interrupt: Arc<Interrupt>) -> Result<Deck, OpenError> {
    let fail = |message: String, unsupported: bool, shape| OpenError { message, unsupported, network: false, shape };
    let (source, download): (Box<dyn symphonia::core::io::MediaSource>, Option<Download>) = match &request.local {
        Some(path) => {
            let file = std::fs::File::open(path)
                .map_err(|e| fail(format!("The downloaded copy couldn't be opened: {e}"), false, FaultShape::Read))?;
            (Box::new(file), None)
        }
        None => {
            let download =
                Download::start(client, request.url.clone(), request.ranged, request.size, request.live, dir, request.keep.clone(), interrupt);
            // These two formats decide whether they can seek only when asked to, so it's safe
            // to skip the tag check at the end of the file. Others (MP4, Ogg) need the end
            // while opening.
            if matches!(request.suffix.as_str(), "flac" | "mp3") {
                download.hold_seeking();
            }
            // The download never got going: the server couldn't be reached, or said no.
            let reader = download.reader().map_err(|e| OpenError { message: e.to_string(), unsupported: false, network: true, shape: FaultShape::Connection })?;
            (Box::new(reader), Some(download))
        }
    };
    if !request.ranged && download.as_ref().is_some_and(Download::ranged) {
        // A conversion was asked for, starting `base_secs` in, and the server sent the whole
        // file as it is, from its start: so the seek to there is made in the file.
        request.start_secs += request.base_secs;
        request.base_secs = 0.0;
    }
    let stream = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if !request.suffix.is_empty() {
        hint.with_extension(&request.suffix);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| {
            // A file that stopped arriving while it was being opened is the connection's
            // doing, and it isn't a format to ask the server to convert.
            let network = download.as_ref().is_some_and(|download| download.troubled());
            let unsupported = matches!(e, SymError::Unsupported(_)) && !network;
            let shape = if network { FaultShape::Connection } else { FaultShape::Decode };
            OpenError { message: format!("This file couldn't be read: {e}"), unsupported, network, shape }
        })?;
    if let Some(download) = &download {
        download.allow_seeking();
    }
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| fail("This file has no audio track.".into(), true, FaultShape::Decode))?;
    let track_id = track.id;
    let time_base = track.time_base;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| fail("This file's audio format isn't recognised.".into(), true, FaultShape::Decode))?;
    let rate = params.sample_rate.unwrap_or(0);
    let bits = params.bits_per_sample.unwrap_or(0);
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| fail(format!("This audio format isn't supported: {e}"), true, FaultShape::Decode))?;

    let mut start = 0.0;
    if request.start_secs > 0.5 {
        let time = Time::try_from_secs_f64(request.start_secs).unwrap_or(Time::ZERO);
        if let Ok(to) =
            format.seek(SeekMode::Accurate, SeekTo::Time { time, track_id: Some(track_id) })
        {
            start = time_base
                .and_then(|tb| tb.calc_time(to.actual_ts))
                .map(|t| t.as_secs_f64())
                .unwrap_or(request.start_secs);
        }
    }
    Ok(Deck {
        token: request.token,
        format,
        decoder,
        track_id,
        time_base,
        gain: request.gain,
        start,
        base: request.base_secs,
        duration: request.duration_secs,
        time: request.base_secs + start,
        rate,
        bits,
        continues: request.continues,
        download,
    })
}

// ---- device callback ----------------------------------------------------------------------------

pub(super) struct CallbackState {
    pub(super) effects: Effects,
    pub(super) scratch: Vec<f32>,
    /// Play/pause envelope, 0.0 to 1.0, so transport changes don't click.
    pub(super) fade: f32,
    pub(super) volume: f32,
    /// Running-dry envelope, 0.0 to 1.0: eases down as the last of the buffered audio goes
    /// out, and back up when there is more.
    pub(super) level: f32,
}

fn open_device(shared: &Arc<Shared>, device_id: &str) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    // A chosen device that has since been unplugged falls back to the system default.
    let chosen = device_id.parse::<cpal::DeviceId>().ok().and_then(|id| host.device_by_id(&id));
    let device = chosen.or_else(|| host.default_output_device()).ok_or("no output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let rate = config.sample_rate;
    let stream = match format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, config, shared),
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, config, shared),
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, config, shared),
        cpal::SampleFormat::I32 => build_stream::<i32>(&device, config, shared),
        cpal::SampleFormat::I8 => build_stream::<i8>(&device, config, shared),
        cpal::SampleFormat::U8 => build_stream::<u8>(&device, config, shared),
        cpal::SampleFormat::F64 => build_stream::<f64>(&device, config, shared),
        other => Err(format!("unsupported device sample format {other}")),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, rate))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let fade_step = 1.0 / (config.sample_rate as f32 * 0.012);
    let data_shared = shared.clone();
    let error_shared = shared.clone();
    let mut state = CallbackState {
        effects: Effects::new(config.sample_rate),
        scratch: Vec::with_capacity(16_384),
        fade: 0.0,
        volume: 1.0,
        level: 0.0,
    };
    device
        .build_output_stream(
            config,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                fill(out, channels, fade_step, &data_shared, &mut state);
            },
            move |err: cpal::Error| match err.kind() {
                // The stream followed a default-device change by itself, or glitched once.
                cpal::ErrorKind::DeviceChanged
                | cpal::ErrorKind::Xrun
                | cpal::ErrorKind::RealtimeDenied => {}
                _ => error_shared.rebuild.store(true, Ordering::Relaxed),
            },
            None,
        )
        .map_err(|e| e.to_string())
}

pub(super) fn fill<T>(out: &mut [T], channels: usize, fade_step: f32, shared: &Shared, st: &mut CallbackState)
where
    T: SizedSample + FromSample<f32>,
{
    let frames = out.len() / channels.max(1);
    let playing = shared.playing.load(Ordering::Relaxed);
    let target_volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));

    st.scratch.clear();
    // How many frames are still waiting behind the ones taken now.
    let mut behind = 0;
    if playing || st.fade > 0.0 {
        behind = shared.ring.pop(&mut st.scratch, frames);
    }
    let got = st.scratch.len() / 2;
    st.effects.process(&shared.effects, &mut st.scratch);
    let volume_step = (target_volume - st.volume) / frames.max(1) as f32;

    for (i, frame) in out.chunks_mut(channels.max(1)).enumerate() {
        st.fade = if playing { (st.fade + fade_step).min(1.0) } else { (st.fade - fade_step).max(0.0) };
        st.volume += volume_step;
        // When the audio in hand is about to run out (the download fell behind), the last few
        // milliseconds are eased down to nothing instead of stopping dead with a click.
        let ahead = if i < got { (got - i + behind) as f32 * fade_step } else { 0.0 };
        st.level = if ahead < st.level { ahead } else { (st.level + fade_step).min(ahead).min(1.0) };
        let gain = st.fade * st.volume * st.level;
        let (l, r) = if i < got { (st.scratch[i * 2] * gain, st.scratch[i * 2 + 1] * gain) } else { (0.0, 0.0) };
        match frame.len() {
            1 => frame[0] = T::from_sample((l + r) * 0.5),
            _ => {
                frame[0] = T::from_sample(l);
                frame[1] = T::from_sample(r);
                for extra in &mut frame[2..] {
                    *extra = T::from_sample(0.0);
                }
            }
        }
    }
    st.volume = target_volume;
}

fn spawn_null_output(shared: Arc<Shared>, record_to: PathBuf) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    // What would have been played goes to the file, as raw 32-bit float stereo, so output
    // can be checked without listening to it.
    let mut dump = std::fs::OpenOptions::new().create(true).append(true).open(record_to).ok();
    let _ = std::thread::Builder::new().name("null-output".into()).spawn(move || {
        use std::io::Write;
        let tick = Duration::from_millis(20);
        let mut taken: Vec<f32> = Vec::new();
        let mut effects = Effects::new(shared.out_rate.load(Ordering::Relaxed));
        while !flag.load(Ordering::Relaxed) {
            std::thread::sleep(tick);
            if !shared.playing.load(Ordering::Relaxed) {
                continue;
            }
            let rate = shared.out_rate.load(Ordering::Relaxed) as usize;
            taken.clear();
            shared.ring.pop(&mut taken, rate / 50);
            effects.process(&shared.effects, &mut taken);
            if let Some(file) = dump.as_mut() {
                let bytes: Vec<u8> = taken.iter().flat_map(|s| s.to_le_bytes()).collect();
                let _ = file.write_all(&bytes);
            }
        }
    });
    stop
}

/// How many frames to drop from the start and end of a decoded packet so that only real audio
/// is left. `valid` is how many of the packet's frames are real audio; `head` and `tail` are
/// the encoder's delay and padding in it. The MP3 and Vorbis decoders have already dropped
/// those, and then `frames` is no more than `valid` and nothing more may go.
fn trims(frames: usize, valid: u64, head: u64, tail: u64) -> (usize, usize) {
    let spare = frames.saturating_sub(usize::try_from(valid).unwrap_or(usize::MAX));
    let head = usize::try_from(head).unwrap_or(usize::MAX).min(spare);
    let tail = usize::try_from(tail).unwrap_or(usize::MAX).min(spare - head);
    (head, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_alone_what_the_decoder_already_trimmed() {
        // A decoder that trims for itself hands back only the real audio.
        assert_eq!(trims(47, 47, 1105, 0), (0, 0));
        // One that doesn't hands back the lot, and the delay and padding go here.
        assert_eq!(trims(1152, 47, 1105, 0), (1105, 0));
        assert_eq!(trims(1024, 500, 0, 524), (0, 524));
        // A packet that doesn't say how long it is: trim as asked, within what there is.
        assert_eq!(trims(100, 0, 30, 200), (30, 70));
    }

    #[test]
    fn the_ring_hands_over_what_was_written_in_order() {
        let ring = Ring::new();
        ring.set_capacity(1000);
        ring.push((0..200).map(|n| n as f32));
        assert_eq!((ring.waiting_frames(), ring.free_frames()), (100, 900));
        let mut out = Vec::new();
        assert_eq!(ring.pop(&mut out, 30), 70);
        assert_eq!(out, (0..60).map(|n| n as f32).collect::<Vec<_>>());
        assert_eq!((ring.played_frames(), ring.written_frames()), (30, 100));
        // Asking for more than there is hands over what there is.
        out.clear();
        assert_eq!(ring.pop(&mut out, 500), 0);
        assert_eq!(out.len(), 140);

        // Round the end of its storage many times over, nothing is lost or repeated.
        ring.set_capacity(RING_SLOTS / 2);
        let mut next = 0u32;
        for _ in 0..7 {
            ring.push((0..400_000).map(|i| (next + i) as f32));
            out.clear();
            assert_eq!(ring.pop(&mut out, 200_000), 0);
            assert!(out.iter().enumerate().all(|(i, sample)| *sample == (next + i as u32) as f32));
            next += 400_000;
        }
    }

    #[test]
    fn clearing_the_ring_skips_what_was_waiting() {
        let ring = Ring::new();
        ring.push((0..200).map(|n| n as f32));
        let mut out = Vec::new();
        ring.pop(&mut out, 10);
        ring.clear();
        assert_eq!((ring.waiting_frames(), ring.played_frames()), (0, 100));
        ring.push([7.0, 8.0].into_iter());
        out.clear();
        assert_eq!(ring.pop(&mut out, 10), 0);
        assert_eq!(out, [7.0, 8.0]);

        // A track starts counting from where the ring had got to.
        let shared = Shared::new();
        shared.ring.push((0..2000).map(|_| 0.0));
        shared.restart_at(5, 42.0);
        assert_eq!(shared.position(), Some((5, 42.0)));
        shared.ring.push((0..96_000).map(|_| 0.0));
        shared.ring.pop(&mut out, 24_000);
        assert_eq!(shared.position(), Some((5, 42.5)));
    }

    #[test]
    fn running_dry_fades_out_and_coming_back_fades_in() {
        let shared = Shared::new();
        shared.playing.store(true, Ordering::Relaxed);
        let step = 1.0 / (48_000.0 * 0.012);
        let mut state =
            CallbackState { effects: Effects::new(48_000), scratch: Vec::new(), fade: 1.0, volume: 1.0, level: 1.0 };
        // A steady signal, with 600 frames left of it: less than two callbacks' worth.
        shared.ring.push((0..1200).map(|_| 0.5));
        let mut heard = Vec::new();
        for _ in 0..2 {
            let mut out = [0f32; 1024];
            fill(&mut out, 2, step, &shared, &mut state);
            heard.extend(out.iter().step_by(2).copied());
        }
        assert_eq!(heard[0], 0.5);
        assert_eq!(*heard.last().unwrap(), 0.0);
        let biggest_drop = heard.windows(2).map(|pair| (pair[0] - pair[1]).abs()).fold(0.0, f32::max);
        assert!(biggest_drop <= step, "a click of {biggest_drop} where the audio ran out");

        // More arrives: it comes up from nothing instead of starting at full level.
        shared.ring.push((0..8000).map(|_| 0.5));
        let mut out = [0f32; 4096];
        fill(&mut out, 2, step, &shared, &mut state);
        let back: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!(back[0] <= step && back[2000] == 0.5, "{} then {}", back[0], back[2000]);
        assert!(back.windows(2).all(|pair| pair[1] >= pair[0] && pair[1] - pair[0] <= step));
    }

    #[test]
    fn a_resampled_track_ends_on_its_last_frame() {
        // Just over a second of a steady tone at 44.1 kHz, played on a 48 kHz device.
        for frames in [44_100usize, 50_000, 44_101, 1_000] {
            let mut rs = make_resampler(44_100, 48_000).unwrap();
            let mut out = Vec::new();
            // Fed in the uneven pieces a decoder hands over.
            let tone: Vec<f32> = (0..frames * 2).map(|i| 0.5 * ((i / 2) as f32 * 0.05).sin()).collect();
            for piece in tone.chunks(2 * 1152) {
                rs.input.extend_from_slice(piece);
                rs.fed += piece.len() as u64 / 2;
                run_resampler(&mut rs, &mut out);
            }
            out.extend(finish_resampler(rs));
            let due = (frames * 48_000).div_ceil(44_100);
            assert_eq!(out.len() / 2, due, "{frames} frames in");
            // And what's at the very end is still the tone, not silence tacked on.
            let last: f32 = out[out.len() - 200..].iter().map(|sample| sample.abs()).fold(0.0, f32::max);
            assert!(last > 0.2, "the last frames are {last}");
        }
    }

    /// Decodes a whole file the way the engine does and counts the frames it would play.
    fn played_frames(bytes: &'static [u8], suffix: &str) -> usize {
        let stream = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
        let mut hint = Hint::new();
        hint.with_extension(suffix);
        let mut format = symphonia::default::get_probe()
            .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
            .unwrap();
        let track = format.default_track(TrackType::Audio).unwrap();
        let track_id = track.id;
        let params = track.codec_params.as_ref().and_then(|p| p.audio()).unwrap();
        let mut decoder =
            symphonia::default::get_codecs().make_audio_decoder(params, &AudioDecoderOptions::default()).unwrap();
        let mut samples: Vec<f32> = Vec::new();
        let mut total = 0;
        while let Ok(Some(packet)) = format.next_packet() {
            if packet.track_id != track_id {
                continue;
            }
            let decoded = decoder.decode(&packet).unwrap();
            decoded.copy_to_vec_interleaved(&mut samples);
            let frames = samples.len() / decoded.spec().channels().count();
            let (head, tail) = trims(frames, packet.dur.get(), packet.trim_start.get(), packet.trim_end.get());
            total += frames - head - tail;
        }
        total
    }

    // Both files are half a second of a tone at 44.1 kHz: 22,050 frames went into the encoder,
    // which added its own delay at the start and padding at the end.
    #[test]
    fn plays_every_frame_of_an_mp3_and_no_more() {
        assert_eq!(played_frames(include_bytes!("../../tests/fixtures/half-second.mp3"), "mp3"), 22_050);
    }

    #[test]
    fn plays_every_frame_of_an_ogg_vorbis_file_and_no_more() {
        assert_eq!(played_frames(include_bytes!("../../tests/fixtures/half-second.ogg"), "ogg"), 22_050);
    }

    // ---- the engine as a whole ------------------------------------------------------------

    /// Plays tracks through a whole engine that has a file to play into instead of a sound
    /// device, and returns what came out (stereo frames at 48 kHz) once the queue has
    /// finished. `first` is loaded; `set_up` can name what follows, seek, and so on.
    fn played(name: &str, first: TrackRequest, set_up: impl FnOnce(&Engine)) -> Vec<[f32; 2]> {
        played_from(name, Client::new("http://127.0.0.1:9", "someone", "secret"), first, set_up)
    }

    /// The same, with `first` streamed from `client`'s server.
    fn played_from(name: &str, client: Client, first: TrackRequest, set_up: impl FnOnce(&Engine)) -> Vec<[f32; 2]> {
        let dir = std::env::temp_dir().join(format!("nexus-engine-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let recording = dir.join("out.raw");
        let (events, heard) = mpsc::channel();
        let engine = Engine::recording_to(dir.clone(), Some(recording.clone()), move |event| {
            let _ = events.send(event);
        });
        engine.load(client, first);
        set_up(&engine);
        engine.set_playing(true);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match heard.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(Event::Finished) => break,
                Ok(Event::Failed { message, .. }) => panic!("the engine gave up: {message}"),
                Ok(_) => {}
                Err(_) => panic!("the engine never finished"),
            }
        }
        // The last of it is still on its way to the file.
        std::thread::sleep(Duration::from_millis(80));
        drop(engine);
        let bytes = std::fs::read(&recording).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        bytes.chunks_exact(8).map(|frame| [0, 4].map(|at| f32::from_le_bytes(frame[at..at + 4].try_into().unwrap()))).collect()
    }

    /// One of the half-second test files, written out to be played from disk.
    fn track(token: u64, name: &str, bytes: &[u8]) -> TrackRequest {
        let path = std::env::temp_dir().join(format!("nexus-engine-fixture-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        TrackRequest {
            token,
            local: Some(path),
            keep: None,
            live: false,
            url: String::new(),
            ranged: true,
            size: 0,
            suffix: name.rsplit('.').next().unwrap().to_string(),
            gain: 1.0,
            start_secs: 0.0,
            base_secs: 0.0,
            duration_secs: 0.5,
            continues: false,
        }
    }

    const MP3: &[u8] = include_bytes!("../../tests/fixtures/half-second.mp3");
    const OGG: &[u8] = include_bytes!("../../tests/fixtures/half-second.ogg");

    /// The quietest stretch of ten milliseconds in what was played, leaving out the very
    /// start and end: a gap between two tracks shows up as one that's near silent.
    fn quietest(frames: &[[f32; 2]]) -> f32 {
        frames[2400..frames.len() - 2400]
            .chunks(480)
            .map(|window| window.iter().map(|frame| frame[0].abs()).fold(0.0, f32::max))
            .fold(f32::MAX, f32::min)
    }

    #[test]
    fn one_track_runs_into_the_next_without_a_gap() {
        // Half a second of a tone as MP3, then the same as Ogg Vorbis: 22,050 frames each at
        // 44.1 kHz, which is 24,000 each at the 48 kHz they're played at.
        let client = Client::new("http://127.0.0.1:9", "someone", "secret");
        let frames = played("gapless", track(1, "a.mp3", MP3), |engine| engine.set_next(Some((client, track(2, "b.ogg", OGG)))));
        assert!(frames.len().abs_diff(48_000) <= 2, "{} frames came out", frames.len());
        assert!(quietest(&frames) > 0.03, "a quiet patch of {}", quietest(&frames));
    }

    #[test]
    fn a_crossfade_overlaps_two_tracks_unless_they_are_one_album() {
        let next = |continues: bool| {
            let client = Client::new("http://127.0.0.1:9", "someone", "secret");
            move |engine: &Engine| {
                // A fade longer than a third of the track is cut to a third: 8,000 frames here,
                // and up to a couple of packets more, since the blend starts on a packet.
                engine.set_crossfade(0.4);
                engine.set_next(Some((client, TrackRequest { continues, ..track(2, "d.ogg", OGG) })));
            }
        };
        // Blended: the second starts under the end of the first, so less comes out in all.
        let blended = played("crossfade", track(1, "c.mp3", MP3), next(false));
        // (Both files are the same tone, which can cancel itself out where they overlap, so
        // how loud the blend is says nothing here.)
        assert!((37_500..=40_000).contains(&blended.len()), "{} frames came out", blended.len());
        // The next track of the same album: every frame of both, end to end.
        let album = played("album", track(1, "e.mp3", MP3), next(true));
        assert!(album.len().abs_diff(48_000) <= 2, "{} frames came out", album.len());
    }

    #[test]
    fn seeking_plays_on_from_where_it_was_asked() {
        // Once the track is open, jump half-way: a quarter of a second is left, and a little
        // more (see `assert_plays_on_from`).
        let mut landed = 0.0;
        let frames = played("seek", track(1, "f.mp3", MP3), |engine| {
            heard_from(engine);
            landed = sought(engine, 0.25);
        });
        assert_plays_on_from(landed, frames.len());
    }

    #[test]
    fn a_seek_meant_for_another_track_is_ignored() {
        // Sent for the track before, as one can be at the moment one hands over to the next:
        // this one plays on from where it is, all of it.
        let frames = played("seek-other", track(2, "i.mp3", MP3), |engine| {
            heard_from(engine);
            engine.seek(1, 0.25);
        });
        assert!(frames.len().abs_diff(24_000) <= 2, "{} frames came out", frames.len());
    }

    /// About two seconds of MP3: the half-second one's audio four times over, without its
    /// first frame (182 bytes), which is the encoder's note of how long the file is.
    fn longer_mp3() -> &'static [u8] {
        Box::leak(MP3[182..].repeat(4).into_boxed_slice())
    }

    /// A server that answers every request for a song with `song`: as the file itself (its
    /// length, byte ranges taken), or as a conversion (neither).
    fn mp3_server(as_itself: bool, song: &'static [u8]) -> Client {
        let address = crate::subsonic::fake::server(move |_, from, stream| {
            use std::io::Write;
            let _ = match (as_itself, from) {
                (true, Some(from)) => {
                    let (from, last) = (from as usize, song.len() - 1);
                    write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {from}-{last}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", song.len(), song.len() - from)
                        .and_then(|_| stream.write_all(&song[from..]))
                }
                (true, None) => write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", song.len())
                    .and_then(|_| stream.write_all(song)),
                (false, _) => write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n").and_then(|_| stream.write_all(song)),
            };
        });
        Client::new(&address, "someone", "secret")
    }

    /// A conversion asked for, as a streaming quality below lossless does, starting `from`
    /// seconds in, of a song whose file on the server is `song`.
    fn conversion(client: &Client, from: f64, song: &[u8]) -> TrackRequest {
        let url = client.stream_url("song-1", 320, from as u32);
        TrackRequest { local: None, url, ranged: false, size: song.len() as u64, suffix: "mp3".into(), base_secs: from, duration_secs: 2.0, ..track(1, "g.mp3", MP3) }
    }

    /// Waits for the track to be heard, and gives where it was then.
    fn heard_from(engine: &Engine) -> f64 {
        let opened = Instant::now();
        while engine.position().is_none() && opened.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        engine.position().expect("the track was heard").1
    }

    /// Seeks in track 1, which is open and not yet playing, and waits until the engine has
    /// done it: gives where the seek landed.
    ///
    /// Playing must not start before that. A seek is carried out by the engine's own thread
    /// a moment after it is asked for, and until then the beginning of the track is what is
    /// waiting to be played: if playing starts first, the stand-in output takes a tick of it
    /// (960 frames every 20 ms) for each tick that passes before the seek is done. These
    /// cases used to seek and start playing in one breath, and count everything that came
    /// out: 13,918 frames where the engine's thread was quick, and one tick more (14,878,
    /// outside what they allowed) on a machine where the output's tick came first.
    fn sought(engine: &Engine, secs: f64) -> f64 {
        let before = engine.position().expect("the track is open").1;
        engine.seek(1, secs);
        let asked = Instant::now();
        while engine.position().is_some_and(|(_, at)| at == before) && asked.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(1));
        }
        engine.position().expect("the track is open").1
    }

    /// A seek to half-way through the half-second MP3 landed at `landed`, and `frames` were
    /// heard from then on. A seek in an MP3 lands on a packet, and a packet or two early at
    /// that (each needs the one before it to decode): so up to 50 ms before the quarter
    /// second. What is heard is the rest of the file from where it landed, and nothing
    /// else: nothing from before the seek, and no part of it twice.
    fn assert_plays_on_from(landed: f64, frames: usize) {
        assert!((0.2..=0.25).contains(&landed), "the seek landed at {landed}");
        let rest = ((0.5 - landed) * 48_000.0).round() as usize;
        assert!(frames.abs_diff(rest) <= 240, "{frames} frames came out after a seek that landed at {landed}: {rest} are left from there");
    }

    #[test]
    fn a_conversion_the_server_sent_as_the_file_itself_is_sought_in_where_it_is() {
        // Seeking: in the file, by the engine, as for a file asked for as itself.
        let client = mp3_server(true, MP3);
        let mut landed = 0.0;
        let frames = played_from("as-itself", client.clone(), conversion(&client, 0.0, MP3), |engine| {
            heard_from(engine);
            assert!(engine.seeks_in_place(1));
            landed = sought(engine, 0.25);
        });
        assert_plays_on_from(landed, frames.len());
        // Asked to start a second in, the server sent it from the start, taking no notice:
        // the app goes there in the file itself (a packet or two early, as a seek in an MP3
        // does), and plays the rest of the song's 2.19 seconds.
        let song = longer_mp3();
        let client = mp3_server(true, song);
        let frames = played_from("as-itself-later", client.clone(), conversion(&client, 1.0, song), |engine| {
            let at = heard_from(engine);
            assert!((0.9..1.1).contains(&at), "heard from {at}");
        });
        assert!((55_000..=62_000).contains(&frames.len()), "{} frames came out", frames.len());
    }

    #[test]
    fn a_real_conversion_is_asked_for_again_to_seek() {
        let client = mp3_server(false, MP3);
        played_from("converted", client.clone(), conversion(&client, 0.0, MP3), |engine| {
            heard_from(engine);
            assert!(!engine.seeks_in_place(1), "a conversion has to be asked for again from where a seek lands");
        });
        // Asked for from a second in, what comes is taken to start there, and all of it plays.
        let song = longer_mp3();
        let client = mp3_server(false, song);
        let frames = played_from("converted-later", client.clone(), conversion(&client, 1.0, song), |engine| {
            let at = heard_from(engine);
            assert!((1.0..1.1).contains(&at), "heard from {at}");
        });
        assert!(frames.len() > 100_000, "{} frames came out", frames.len());
    }

    #[test]
    fn a_song_kept_by_0_40_1_plays_and_seeks_with_the_server_out_of_reach() {
        // The song cache as 0.40.1 left it: an MP3 within a limit of 320 kbps, asked for as a
        // conversion at that limit and kept under that name, which is the name still used.
        let dir = std::env::temp_dir().join(format!("nexus-engine-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("song-7.320.mp3"), MP3).unwrap();
        let cache = crate::songcache::SongCache::new(dir.clone());
        let kept = cache.file("song-7", 320, "mp3");
        assert!(crate::songcache::ready(&kept), "{kept:?}");
        // Played from disk as a kept copy is (the client's server has nothing listening).
        let request = TrackRequest { local: Some(kept), ..track(1, "h.mp3", MP3) };
        let mut landed = 0.0;
        let frames = played("kept-0401", request, |engine| {
            heard_from(engine);
            assert!(engine.seeks_in_place(1));
            landed = sought(engine, 0.25);
        });
        assert_plays_on_from(landed, frames.len());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
