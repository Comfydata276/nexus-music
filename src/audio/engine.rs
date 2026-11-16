//! The playback engine: one thread that owns the output device, decodes tracks and feeds a
//! short ring buffer the device callback drains.
//!
//! Gapless playback falls out of the design: when a track ends and the next one is already
//! open, its samples go straight into the same ring with no flush in between.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
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
use super::source::Download;
use crate::subsonic::Client;

/// How much decoded audio is kept ahead of the device, in seconds.
const RING_SECONDS: f64 = 1.0;
/// Seconds of pause after which the device stream is suspended so Windows can sleep.
const SUSPEND_AFTER: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct TrackRequest {
    /// Identifies this load in events and position reports. Never 0.
    pub token: u64,
    pub url: String,
    /// True when the server sends the original file, which supports byte ranges.
    pub ranged: bool,
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
}

#[derive(Debug, Clone)]
pub enum Event {
    /// This track is now the one coming out of the speakers.
    Started(u64),
    /// Everything queued in the engine has finished playing.
    Finished,
    Failed { token: u64, message: String, unsupported: bool },
}

struct Segment {
    token: u64,
    /// Device frame, counted from the last flush, at which this track begins.
    start_frame: u64,
    /// Track time at `start_frame`.
    offset: f64,
}

struct Ring {
    /// Interleaved stereo at the device sample rate.
    buf: VecDeque<f32>,
    capacity_frames: usize,
    written: u64,
    played: u64,
    segments: VecDeque<Segment>,
}

impl Ring {
    fn clear(&mut self) {
        self.buf.clear();
        self.written = 0;
        self.played = 0;
        self.segments.clear();
    }

    fn free_frames(&self) -> usize {
        self.capacity_frames.saturating_sub(self.buf.len() / 2)
    }

    /// The segment currently audible, discarding the ones already left behind.
    fn audible(&mut self) -> Option<&Segment> {
        while self.segments.len() > 1 && self.segments[1].start_frame <= self.played {
            self.segments.pop_front();
        }
        self.segments.front().filter(|s| s.start_frame <= self.played)
    }
}

struct Shared {
    ring: Mutex<Ring>,
    playing: AtomicBool,
    volume: AtomicU32,
    out_rate: AtomicU32,
    /// Token of the track the decoder is currently reading; 0 when idle.
    decoding: AtomicU64,
    /// Set by the device error callback when the stream must be rebuilt.
    rebuild: AtomicBool,
    /// Seconds of overlap between one track and the next; 0 for none.
    crossfade: AtomicU32,
    effects: Controls,
}

enum Slot {
    Current,
    Next,
}

enum Cmd {
    Load(Client, TrackRequest),
    SetNext(Option<(Client, TrackRequest)>),
    Opened { token: u64, slot: Slot, result: Result<Box<Deck>, OpenError> },
    Seek(f64),
    Stop,
    Wake,
}

struct OpenError {
    message: String,
    unsupported: bool,
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
    // Declared last so the reader inside `format` is dropped before the file is deleted.
    _download: Download,
}

pub struct Engine {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
}

impl Engine {
    pub fn new(buffer_dir: PathBuf, on_event: impl Fn(Event) + Send + 'static) -> Engine {
        let shared = Arc::new(Shared {
            ring: Mutex::new(Ring {
                buf: VecDeque::new(),
                capacity_frames: 48_000,
                written: 0,
                played: 0,
                segments: VecDeque::new(),
            }),
            playing: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            out_rate: AtomicU32::new(48_000),
            decoding: AtomicU64::new(0),
            rebuild: AtomicBool::new(false),
            crossfade: AtomicU32::new(0f32.to_bits()),
            effects: Controls::new(),
        });
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            rx,
            tx: tx.clone(),
            shared: shared.clone(),
            buffer_dir,
            on_event: Box::new(on_event),
            output: None,
            current: None,
            next: None,
            ended: false,
            load_token: 0,
            next_token: 0,
            announced: 0,
            pending: VecDeque::new(),
            resampler: None,
            samples: Vec::new(),
            stereo: Vec::new(),
            paused_since: None,
            suspended: false,
            last_device_retry: Instant::now(),
            load_started: None,
            tail: Vec::new(),
            capturing_tail: false,
            fade: None,
            out: Vec::new(),
        };
        std::thread::Builder::new()
            .name("audio-engine".into())
            .spawn(move || worker.run())
            .expect("failed to start the audio thread");
        Engine { tx, shared }
    }

    /// Replaces whatever is playing with this track.
    pub fn load(&self, client: Client, request: TrackRequest) {
        let _ = self.tx.send(Cmd::Load(client, request));
    }

    /// Names the track to continue with, gaplessly, when the current one ends.
    pub fn set_next(&self, next: Option<(Client, TrackRequest)>) {
        let _ = self.tx.send(Cmd::SetNext(next));
    }

    pub fn seek(&self, secs: f64) {
        let _ = self.tx.send(Cmd::Seek(secs));
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn set_playing(&self, playing: bool) {
        self.shared.playing.store(playing, Ordering::Relaxed);
        let _ = self.tx.send(Cmd::Wake);
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
    pub fn set_equalizer(&self, on: bool, gains: [f32; EQ_BANDS.len()]) {
        self.shared.effects.set_equalizer(on, gains);
    }

    /// The audible track and the position within it, in seconds.
    pub fn position(&self) -> Option<(u64, f64)> {
        let rate = self.shared.out_rate.load(Ordering::Relaxed).max(1) as f64;
        let mut ring = self.shared.ring.lock().unwrap();
        let played = ring.played;
        ring.audible().map(|s| (s.token, s.offset + (played - s.start_frame) as f64 / rate))
    }

    /// Token of the track the decoder has reached, which runs ahead of the audible one near
    /// the end of a track.
    pub fn decoding_token(&self) -> u64 {
        self.shared.decoding.load(Ordering::Relaxed)
    }
}

enum Output {
    Device(cpal::Stream),
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
    input: Vec<f32>,
    output: Vec<f32>,
    /// Output frames still to drop to hide the filter's start-up delay.
    skip: usize,
}

struct Worker {
    rx: Receiver<Cmd>,
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    buffer_dir: PathBuf,
    on_event: Box<dyn Fn(Event) + Send>,
    output: Option<Output>,
    current: Option<Box<Deck>>,
    next: Option<Box<Deck>>,
    /// The current deck has been decoded to its end.
    ended: bool,
    load_token: u64,
    next_token: u64,
    announced: u64,
    /// Stereo samples at the device rate that didn't fit in the ring yet.
    pending: VecDeque<f32>,
    resampler: Option<ResamplerState>,
    samples: Vec<f32>,
    stereo: Vec<f32>,
    paused_since: Option<Instant>,
    suspended: bool,
    last_device_retry: Instant,
    /// When the current load was requested, until its first audio is queued. For timing.
    load_started: Option<Instant>,
    /// The closing seconds of the current track, held back to blend into the next one.
    tail: Vec<f32>,
    capturing_tail: bool,
    /// A crossfade in progress: the previous track's tail and how far through it we are.
    fade: Option<Fade>,
    /// Scratch space for audio on its way out of the decoder.
    out: Vec<f32>,
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
        self.open_output();
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
        match cmd {
            Cmd::Load(client, request) => {
                self.reset();
                self.load_token = request.token;
                self.load_started = Some(Instant::now());
                if matches!(self.output, Some(Output::Null(_)))
                    && self.last_device_retry.elapsed() > Duration::from_secs(3)
                {
                    self.open_output();
                }
                self.spawn_open(client, request, Slot::Current);
            }
            Cmd::SetNext(next) => {
                self.next = None;
                self.next_token = 0;
                if let Some((client, request)) = next {
                    self.next_token = request.token;
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
                        let mut ring = self.shared.ring.lock().unwrap();
                        ring.clear();
                        ring.segments.push_back(Segment {
                            token,
                            start_frame: 0,
                            offset: deck.base + deck.start,
                        });
                        drop(ring);
                        self.shared.decoding.store(token, Ordering::Relaxed);
                        self.current = Some(deck);
                        self.ended = false;
                    }
                    Err(e) => self.emit(Event::Failed {
                        token,
                        message: e.message,
                        unsupported: e.unsupported,
                    }),
                }
            }
            Cmd::Opened { token, slot: Slot::Next, result } => {
                if token != self.next_token {
                    return;
                }
                match result {
                    Ok(deck) => {
                        self.next = Some(deck);
                        if self.ended && self.current.is_some() {
                            self.promote_next();
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
            Cmd::Seek(secs) => self.seek(secs),
            Cmd::Stop => self.reset(),
            Cmd::Wake => {}
        }
    }

    fn reset(&mut self) {
        self.current = None;
        self.next = None;
        self.ended = false;
        self.load_token = 0;
        self.next_token = 0;
        self.announced = 0;
        self.pending.clear();
        self.resampler = None;
        self.clear_fade();
        self.shared.decoding.store(0, Ordering::Relaxed);
        self.shared.ring.lock().unwrap().clear();
    }

    fn spawn_open(&self, client: Client, request: TrackRequest, slot: Slot) {
        let tx = self.tx.clone();
        let dir = self.buffer_dir.clone();
        let token = request.token;
        let spawned = std::thread::Builder::new().name("track-open".into()).spawn(move || {
            let result = open_deck(client, request, &dir).map(Box::new);
            let _ = tx.send(Cmd::Opened { token, slot, result });
        });
        if spawned.is_err() {
            self.emit(Event::Failed {
                token,
                message: "Couldn't start a thread to open the track.".into(),
                unsupported: false,
            });
        }
    }

    fn seek(&mut self, secs: f64) {
        let Some(deck) = self.current.as_mut() else { return };
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
        let mut ring = self.shared.ring.lock().unwrap();
        ring.clear();
        ring.segments.push_back(Segment { token, start_frame: 0, offset });
    }

    // ---- periodic work ----------------------------------------------------------------------

    fn housekeeping(&mut self) {
        if self.shared.rebuild.swap(false, Ordering::Relaxed) {
            let position = self.audible_position();
            self.open_output();
            // Buffered audio was produced for the old device's sample rate.
            if let Some((token, secs)) = position {
                if self.current.as_ref().is_some_and(|d| d.token == token) {
                    self.seek(secs);
                }
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
        } else {
            let since = *self.paused_since.get_or_insert_with(Instant::now);
            if !self.suspended && since.elapsed() > SUSPEND_AFTER {
                if let Some(Output::Device(stream)) = &self.output {
                    let _ = stream.pause();
                }
                self.suspended = true;
            }
        }

        // Announce track changes as playback crosses them, and the end of the queue.
        let mut ring = self.shared.ring.lock().unwrap();
        let drained = ring.played >= ring.written;
        let audible = ring.audible().map(|s| s.token);
        drop(ring);
        if let Some(token) = audible {
            if token != self.announced {
                self.announced = token;
                self.emit(Event::Started(token));
            }
        }
        if self.ended && self.pending.is_empty() && drained && self.current.is_some() {
            if self.next_token != 0 {
                // The next track is still opening; it starts as soon as it arrives.
                return;
            }
            self.current = None;
            self.ended = false;
            self.shared.decoding.store(0, Ordering::Relaxed);
            self.emit(Event::Finished);
        }
    }

    fn audible_position(&self) -> Option<(u64, f64)> {
        let rate = self.shared.out_rate.load(Ordering::Relaxed).max(1) as f64;
        let mut ring = self.shared.ring.lock().unwrap();
        let played = ring.played;
        ring.audible().map(|s| (s.token, s.offset + (played - s.start_frame) as f64 / rate))
    }

    /// Moves pending samples into the ring. Returns true when everything fit.
    fn flush_pending(&mut self) -> bool {
        if self.pending.is_empty() {
            return true;
        }
        let mut ring = self.shared.ring.lock().unwrap();
        let frames = ring.free_frames().min(self.pending.len() / 2);
        ring.buf.extend(self.pending.drain(..frames * 2));
        ring.written += frames as u64;
        drop(ring);
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
            Err(e) => {
                let token = deck.token;
                // Whatever was decoded so far plays out; the player decides what follows.
                self.emit(Event::Failed {
                    token,
                    message: format!("Playback stopped: {e}"),
                    unsupported: false,
                });
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
                });
                return self.end_of_deck();
            }
        };
        let gain = deck.gain;
        if channels > 0 && rate > 0 {
            // Encoder delay and padding, so albums play through without gaps.
            let frames = samples.len() / channels;
            let head = (packet.trim_start.get() as usize).min(frames);
            let tail = (packet.trim_end.get() as usize).min(frames - head);
            let body = &samples[head * channels..(frames - tail) * channels];
            deck.time += (frames - head - tail) as f64 / rate as f64;
            // Within a crossfade's length of the end, audio is set aside for blending
            // instead of being queued.
            let overlap = crossfade_for(&self.shared, deck.duration);
            if overlap > 0.0 && deck.time >= deck.duration - overlap {
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
            1 => {
                for &s in samples {
                    let v = soft_clip(s * gain);
                    stereo.extend_from_slice(&[v, v]);
                }
            }
            2 => stereo.extend(samples.iter().map(|&s| soft_clip(s * gain))),
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
        let Some(mut rs) = self.resampler.take() else { return };
        let chunk = rs.inner.input_frames_next() * 2;
        // Enough silence to flush both the partial chunk and the filter's delay line.
        let padded = (rs.input.len() / chunk + 2) * chunk;
        rs.input.resize(padded, 0.0);
        let mut out = Vec::new();
        run_resampler(&mut rs, &mut out);
        self.route(&mut out);
    }

    fn end_of_deck(&mut self) {
        if self.next.is_some() {
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
        let mut ring = self.shared.ring.lock().unwrap();
        let start_frame = ring.written + (self.pending.len() / 2) as u64;
        ring.segments.push_back(Segment {
            token: deck.token,
            start_frame,
            offset: deck.base + deck.start,
        });
        drop(ring);
        self.shared.decoding.store(deck.token, Ordering::Relaxed);
        self.current = Some(deck);
        self.ended = false;
    }

    // ---- output device ----------------------------------------------------------------------

    fn open_output(&mut self) {
        self.output = None;
        self.suspended = false;
        self.last_device_retry = Instant::now();
        let output = match open_device(&self.shared) {
            Ok((stream, rate)) => {
                self.set_rate(rate);
                Output::Device(stream)
            }
            Err(_) => {
                self.set_rate(48_000);
                Output::Null(spawn_null_output(self.shared.clone()))
            }
        };
        self.output = Some(output);
    }

    fn set_rate(&self, rate: u32) {
        self.shared.out_rate.store(rate, Ordering::Relaxed);
        let mut ring = self.shared.ring.lock().unwrap();
        ring.capacity_frames = (rate as f64 * RING_SECONDS) as usize;
    }
}

fn make_resampler(from_rate: u32, to_rate: u32) -> Option<ResamplerState> {
    let inner =
        Fft::<f32>::new(from_rate as usize, to_rate as usize, 1024, 2, FixedSync::Input).ok()?;
    let skip = inner.output_delay();
    let output = vec![0.0; inner.output_frames_max() * 2];
    Some(ResamplerState { inner, from_rate, input: Vec::new(), output, skip })
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
        out.extend_from_slice(&rs.output[skip * 2..produced * 2]);
    }
    rs.input.drain(..consumed.min(rs.input.len()));
}

fn open_deck(client: Client, request: TrackRequest, dir: &std::path::Path) -> Result<Deck, OpenError> {
    let fail = |message: String, unsupported: bool| OpenError { message, unsupported };
    let download = Download::start(client, request.url.clone(), request.ranged, dir);
    // These two formats decide whether they can seek only when asked to, so it's safe to skip
    // the tag check at the end of the file. Others (MP4, Ogg) need the end while opening.
    if matches!(request.suffix.as_str(), "flac" | "mp3") {
        download.hold_seeking();
    }
    let reader = download.reader().map_err(|e| fail(e.to_string(), false))?;
    let stream = MediaSourceStream::new(Box::new(reader), Default::default());
    let mut hint = Hint::new();
    if !request.suffix.is_empty() {
        hint.with_extension(&request.suffix);
    }
    let mut format = symphonia::default::get_probe()
        .probe(&hint, stream, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| {
            let unsupported = matches!(e, SymError::Unsupported(_));
            fail(format!("This file couldn't be read: {e}"), unsupported)
        })?;
    download.allow_seeking();
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| fail("This file has no audio track.".into(), true))?;
    let track_id = track.id;
    let time_base = track.time_base;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| fail("This file's audio format isn't recognised.".into(), true))?;
    let decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| fail(format!("This audio format isn't supported: {e}"), true))?;

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
        _download: download,
    })
}

// ---- device callback ----------------------------------------------------------------------------

struct CallbackState {
    effects: Effects,
    scratch: Vec<f32>,
    /// Play/pause envelope, 0.0 to 1.0, so transport changes don't click.
    fade: f32,
    volume: f32,
}

fn open_device(shared: &Arc<Shared>) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;
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

fn fill<T>(out: &mut [T], channels: usize, fade_step: f32, shared: &Shared, st: &mut CallbackState)
where
    T: SizedSample + FromSample<f32>,
{
    let frames = out.len() / channels.max(1);
    let playing = shared.playing.load(Ordering::Relaxed);
    let target_volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));

    st.scratch.clear();
    if playing || st.fade > 0.0 {
        // Never block the audio thread: on the rare contended lock, output a moment of silence.
        if let Ok(mut ring) = shared.ring.try_lock() {
            let take = frames.min(ring.buf.len() / 2);
            st.scratch.extend(ring.buf.drain(..take * 2));
            ring.played += take as u64;
        }
    }
    let got = st.scratch.len() / 2;
    st.effects.process(&shared.effects, &mut st.scratch);
    let volume_step = (target_volume - st.volume) / frames.max(1) as f32;

    for (i, frame) in out.chunks_mut(channels.max(1)).enumerate() {
        st.fade = if playing { (st.fade + fade_step).min(1.0) } else { (st.fade - fade_step).max(0.0) };
        st.volume += volume_step;
        let gain = st.fade * st.volume;
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

fn spawn_null_output(shared: Arc<Shared>) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    // Diagnostic: with no sound card, NEXUS_AUDIO_DUMP=<file> records what would have been
    // played, as raw 32-bit float stereo, so output can be checked without listening to it.
    let mut dump = std::env::var_os("NEXUS_AUDIO_DUMP")
        .and_then(|path| std::fs::OpenOptions::new().create(true).append(true).open(path).ok());
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
            let mut ring = shared.ring.lock().unwrap();
            let take = (rate / 50).min(ring.buf.len() / 2);
            taken.clear();
            taken.extend(ring.buf.drain(..take * 2));
            ring.played += take as u64;
            drop(ring);
            effects.process(&shared.effects, &mut taken);
            if let Some(file) = dump.as_mut() {
                let bytes: Vec<u8> = taken.iter().flat_map(|s| s.to_le_bytes()).collect();
                let _ = file.write_all(&bytes);
            }
        }
    });
    stop
}
