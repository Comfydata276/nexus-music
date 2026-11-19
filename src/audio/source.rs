//! A seekable byte source for the decoder, backed by an HTTP download.
//!
//! The track downloads sequentially into a temporary file while the decoder reads from the
//! same file. Reads that land far ahead of the download (the tail of the file while probing,
//! or a user seek) are answered with small HTTP range requests instead of waiting for
//! everything in between.
//!
//! Connections go quiet without closing (a laptop changing networks, a router dropping a
//! long transfer), so nothing here waits on one for ever. A download that stops arriving is
//! started again from where it had got to, and whoever is waiting for bytes waits in short
//! steps, so the audio engine can be told to do something else in the meantime.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

use crate::subsonic::Client;

/// Reads further ahead of the download than this are served by a range request.
const SIDE_THRESHOLD: u64 = 768 * 1024;
const SIDE_CHUNK_MIN: u64 = 64 * 1024;
const SIDE_CHUNK_MAX: u64 = 4 * 1024 * 1024;
/// A download that has brought nothing for this long is taken to be stuck and is asked for
/// again from where it stopped, this many times before giving up.
const STALL_RESTART: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 10 });
const STALL_RESTARTS: u32 = 3;
/// How long a stream that can't be picked up part-way (a conversion, a radio station, a small
/// out-of-order fetch) may bring nothing before it's given up on.
const STALL_GIVE_UP: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 20 });
/// How often something waiting for bytes looks up to see whether it should stop waiting.
const WAIT_STEP: Duration = Duration::from_millis(50);
/// How much of a live stream is held ahead of the decoder before the download waits, and how
/// much already-played audio is kept behind it.
const LIVE_AHEAD: usize = 2 * 1024 * 1024;
const LIVE_BEHIND: u64 = 256 * 1024;

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

/// A way to tell the audio engine's thread to stop waiting for a download. It waits for bytes
/// inside the decoder, which can't be paused half-way through a read; raising this makes the
/// read fail instead, so the thread gets back to the command that's waiting for it.
#[derive(Default)]
pub struct Interrupt {
    /// Commands sent to the engine that it hasn't picked up yet.
    waiting: AtomicUsize,
    engine: OnceLock<ThreadId>,
}

impl Interrupt {
    /// Called once, on the thread whose waits may be cut short.
    pub fn claim(&self) {
        let _ = self.engine.set(std::thread::current().id());
    }

    pub fn raise(&self) {
        self.waiting.fetch_add(1, Ordering::Relaxed);
    }

    pub fn lower(&self) {
        let _ = self.waiting.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }

    pub fn raised(&self) -> bool {
        self.waiting.load(Ordering::Relaxed) > 0
    }

    /// Whether the thread asking is the engine's and has a command waiting. Tracks are opened
    /// on other threads, and those are left to finish.
    fn stops_this_thread(&self) -> bool {
        self.raised() && self.engine.get() == Some(&std::thread::current().id())
    }
}

fn interrupted() -> io::Error {
    io::Error::other("interrupted by a new command")
}

struct State {
    /// Set once the response headers have arrived (or the request failed).
    opened: bool,
    downloaded: u64,
    total: Option<u64>,
    done: bool,
    error: Option<String>,
    /// Which attempt at the download is the live one. An earlier attempt that wakes up after
    /// being given up on sees a different number and leaves everything alone.
    attempt: u64,
    /// When bytes last arrived, and how many times in a row the download has been started
    /// again without any arriving.
    progressed: Instant,
    restarts: u32,
    /// For a live stream: the stretch of it currently held in memory, and where in the
    /// stream that stretch begins. (A live stream never ends, so it can't go in a file.)
    held: VecDeque<u8>,
    held_from: u64,
}

struct Shared {
    client: Client,
    url: String,
    /// Raw files can be range-requested; transcoded streams can't.
    ranged: bool,
    /// An internet radio stream: endless, kept in memory, and it may announce song titles.
    live: bool,
    title: Arc<Mutex<String>>,
    path: PathBuf,
    /// Where to keep a copy once the whole track has arrived, if it's to be kept, and the
    /// fewest bytes the whole track can be (for a conversion, whose length isn't announced).
    keep: Option<(PathBuf, u64)>,
    state: Mutex<State>,
    progress: Condvar,
    cancel: AtomicBool,
    /// Something went wrong fetching this track, as against decoding it.
    trouble: AtomicBool,
    interrupt: Arc<Interrupt>,
    /// While set, the source claims it can't seek. See `Download::hold_seeking`.
    seek_held: AtomicBool,
}

impl Shared {
    /// Looks at whether the download has gone quiet, and does something about it: a file
    /// that can be asked for from the middle is asked for again from where it stopped, and
    /// anything else is given up on so that whoever plays it can start it over.
    fn watch(self: &Arc<Self>, state: &mut State) {
        let quiet = state.progressed.elapsed();
        if !state.opened || state.done || state.error.is_some() || self.live || quiet < STALL_RESTART {
            return;
        }
        if self.ranged && state.restarts < STALL_RESTARTS {
            state.attempt += 1;
            state.restarts += 1;
            state.progressed = Instant::now();
            let (worker, attempt) = (self.clone(), state.attempt);
            let spawned = std::thread::Builder::new().name("download".into()).spawn(move || download_loop(&worker, attempt));
            if spawned.is_ok() {
                return;
            }
        }
        if self.ranged || quiet >= STALL_GIVE_UP {
            state.error = Some("The connection to the server went quiet.".into());
            self.trouble.store(true, Ordering::Relaxed);
            self.progress.notify_all();
        }
    }
}

/// Owns one track's download. Dropping it cancels the transfer and deletes the file.
pub struct Download {
    shared: Arc<Shared>,
}

impl Download {
    pub fn start(
        client: Client,
        url: String,
        ranged: bool,
        live: bool,
        dir: &std::path::Path,
        keep: Option<(PathBuf, u64)>,
        interrupt: Arc<Interrupt>,
    ) -> Download {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("stream-{}-{id}.part", std::process::id()));
        let shared = Arc::new(Shared {
            client,
            url,
            ranged,
            live,
            title: Arc::new(Mutex::new(String::new())),
            path,
            keep,
            state: Mutex::new(State {
                opened: false,
                downloaded: 0,
                total: None,
                done: false,
                error: None,
                attempt: 0,
                progressed: Instant::now(),
                restarts: 0,
                held: VecDeque::new(),
                held_from: 0,
            }),
            progress: Condvar::new(),
            cancel: AtomicBool::new(false),
            trouble: AtomicBool::new(false),
            interrupt,
            seek_held: AtomicBool::new(false),
        });
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("download".into())
            .spawn(move || if worker.live { live_loop(&worker) } else { download_loop(&worker, 0) })
            .expect("failed to start a download thread");
        Download { shared }
    }

    /// Checks that the download is still arriving, starting it again if it has gone quiet.
    /// The engine calls this now and then, so a stuck download is noticed while there is
    /// still audio in hand, not when it runs out.
    pub fn watch(&self) {
        let mut state = self.shared.state.lock().unwrap();
        self.shared.watch(&mut state);
    }

    /// Whether what went wrong with this track was fetching it.
    pub fn troubled(&self) -> bool {
        self.shared.trouble.load(Ordering::Relaxed)
    }

    /// Makes the source report itself unseekable until `allow_seeking` is called.
    ///
    /// When a file is first opened the decoder library looks for tags at its very end, if it
    /// can seek there. Over HTTP that is a second request before any audio arrives, for tags
    /// nobody needs: the server already told us the song's details.
    pub fn hold_seeking(&self) {
        self.shared.seek_held.store(true, Ordering::Relaxed);
    }

    pub fn allow_seeking(&self) {
        self.shared.seek_held.store(false, Ordering::Relaxed);
    }

    /// What a radio station says it's playing; empty if it hasn't said.
    pub fn title(&self) -> String {
        self.shared.title.lock().unwrap().clone()
    }

    pub fn reader(&self) -> io::Result<SourceReader> {
        // The file is created by the download thread; wait for that before opening it.
        let mut state = self.shared.state.lock().unwrap();
        while !state.opened {
            state = self.shared.progress.wait(state).unwrap();
        }
        if let Some(err) = &state.error {
            return Err(io::Error::other(err.clone()));
        }
        drop(state);
        Ok(SourceReader {
            file: if self.shared.live { None } else { Some(File::open(&self.shared.path)?) },
            shared: self.shared.clone(),
            pos: 0,
            file_pos: 0,
            side: None,
            side_chunk: SIDE_CHUNK_MIN,
        })
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.progress.notify_all();
        // The download thread also removes the file on exit; whichever runs last wins.
        let _ = fs::remove_file(&self.shared.path);
    }
}

/// Waits a little before trying again, unless the track is dropped in the meantime.
fn pause_before_retry(shared: &Shared, failures: u64) {
    let until = Instant::now() + Duration::from_millis(600 * failures);
    while Instant::now() < until && !shared.cancel.load(Ordering::Relaxed) {
        std::thread::sleep(WAIT_STEP);
    }
}

/// Brings the file down, from where the last attempt got to. `attempt` is which attempt this
/// is: once the download has been started again by `watch`, an earlier attempt stops.
fn download_loop(shared: &Arc<Shared>, attempt: u64) {
    let opened = if attempt == 0 {
        OpenOptions::new().create(true).write(true).truncate(true).open(&shared.path)
    } else {
        OpenOptions::new().write(true).open(&shared.path)
    };
    let mut file = match opened {
        Ok(f) => f,
        Err(e) => return finish(shared, attempt, Some(format!("Couldn't create a buffer file: {e}"))),
    };
    let mut failures = 0;
    let mut buf = vec![0u8; 64 * 1024];
    'request: loop {
        let offset = {
            let state = shared.state.lock().unwrap();
            if state.attempt != attempt {
                return;
            }
            state.downloaded
        };
        if shared.cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_file(&shared.path);
            return;
        }
        if let Err(e) = file.seek(SeekFrom::Start(offset)) {
            return finish(shared, attempt, Some(format!("Couldn't buffer the track: {e}")));
        }
        let range = (offset > 0).then_some((offset, None));
        let (mut body, length) = match shared.client.open_stream(&shared.url, range) {
            Ok(v) => v,
            Err(e) => {
                failures += 1;
                // Before anything has come at all there's no point lingering: whoever asked
                // for the track decides whether to ask again, and tells the listener.
                if failures > 4 || offset == 0 || !shared.ranged {
                    return finish(shared, attempt, Some(e.to_string()));
                }
                pause_before_retry(shared, failures);
                continue 'request;
            }
        };
        {
            let mut state = shared.state.lock().unwrap();
            if state.attempt != attempt {
                return;
            }
            if !state.opened {
                state.opened = true;
                state.total = if shared.ranged { length } else { None };
            }
            state.progressed = Instant::now();
            shared.progress.notify_all();
        }
        loop {
            if shared.cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = fs::remove_file(&shared.path);
                return;
            }
            match body.read(&mut buf) {
                Ok(0) => return finish(shared, attempt, None),
                Ok(n) => {
                    // Written with the state held, so an attempt that has been given up on
                    // can never add to the file behind the one that replaced it.
                    let mut state = shared.state.lock().unwrap();
                    if state.attempt != attempt {
                        return;
                    }
                    if let Err(e) = file.write_all(&buf[..n]) {
                        drop(state);
                        return finish(shared, attempt, Some(format!("Couldn't buffer the track: {e}")));
                    }
                    state.downloaded += n as u64;
                    state.progressed = Instant::now();
                    state.restarts = 0;
                    failures = 0;
                    shared.progress.notify_all();
                }
                Err(e) => {
                    failures += 1;
                    if failures > 4 || !shared.ranged {
                        return finish(shared, attempt, Some(format!("The connection dropped: {e}")));
                    }
                    pause_before_retry(shared, failures);
                    continue 'request;
                }
            }
        }
    }
}

/// Feeds a live stream into memory, pausing whenever the decoder falls far enough behind.
fn live_loop(shared: &Shared) {
    let (mut body, _) = match shared.client.open_live(&shared.url, &shared.title) {
        Ok(v) => v,
        Err(e) => return finish(shared, 0, Some(e.to_string())),
    };
    {
        let mut state = shared.state.lock().unwrap();
        state.opened = true;
        state.progressed = Instant::now();
        shared.progress.notify_all();
    }
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match body.read(&mut buf) {
            // A station doesn't end. This one went away, and is rejoined by whoever plays it.
            Ok(0) => return finish(shared, 0, Some("The station's stream ended.".into())),
            Ok(n) => {
                let mut state = shared.state.lock().unwrap();
                while state.held.len() > LIVE_AHEAD && !shared.cancel.load(Ordering::Relaxed) {
                    let (next, _) = shared.progress.wait_timeout(state, Duration::from_millis(250)).unwrap();
                    state = next;
                }
                if shared.cancel.load(Ordering::Relaxed) {
                    return;
                }
                state.held.extend(&buf[..n]);
                state.downloaded += n as u64;
                shared.progress.notify_all();
            }
            Err(e) => return finish(shared, 0, Some(format!("The station's stream dropped: {e}"))),
        }
    }
}

fn finish(shared: &Shared, attempt: u64, error: Option<String>) {
    let mut state = shared.state.lock().unwrap();
    if state.attempt != attempt {
        return;
    }
    state.opened = true;
    match error {
        // A short read on a known-length file is an error; treat clean EOF as completion.
        None => {
            // All of it: not a file that ended short of the length it was said to have, and
            // not a conversion (which has no announced length) that stopped well before the
            // song could have been over.
            let least = shared.keep.as_ref().map_or(0, |(_, least)| *least);
            let whole = state.downloaded > 0
                && state.total.is_none_or(|total| total == state.downloaded)
                && (shared.ranged || state.downloaded >= least);
            state.done = true;
            state.total = Some(state.downloaded);
            if let (true, false, Some((keep, _))) = (whole, shared.live, &shared.keep) {
                crate::songcache::adopt(&shared.path, keep);
            }
        }
        Some(e) => {
            state.error = Some(e);
            shared.trouble.store(true, Ordering::Relaxed);
        }
    }
    shared.progress.notify_all();
}

/// A small stretch of the file fetched out of order, arriving on a thread of its own.
struct Side {
    start: u64,
    len: u64,
    data: Mutex<SideData>,
    arrived: Condvar,
    /// Nobody wants it any more.
    dropped: AtomicBool,
}

#[derive(Default)]
struct SideData {
    bytes: Vec<u8>,
    error: Option<String>,
}

fn fetch_side(shared: &Shared, side: &Side) {
    let end = side.start + side.len - 1;
    let result = (|| -> Result<(), String> {
        let (mut body, _) = shared.client.open_stream(&shared.url, Some((side.start, Some(end)))).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            if side.dropped.load(Ordering::Relaxed) || shared.cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            match body.read(&mut buf).map_err(|e| format!("The connection dropped: {e}"))? {
                0 => break,
                n => {
                    let mut data = side.data.lock().unwrap();
                    let room = (side.len as usize).saturating_sub(data.bytes.len());
                    data.bytes.extend_from_slice(&buf[..n.min(room)]);
                    side.arrived.notify_all();
                    if data.bytes.len() as u64 >= side.len {
                        return Ok(());
                    }
                }
            }
        }
        Err("The server sent less of the file than was asked for.".into())
    })();
    if let Err(reason) = result {
        side.data.lock().unwrap().error = Some(reason);
        side.arrived.notify_all();
    }
}

pub struct SourceReader {
    shared: Arc<Shared>,
    /// `None` for a live stream, which is read from memory.
    file: Option<File>,
    pos: u64,
    /// Where the file handle's cursor currently is, to skip redundant seeks.
    file_pos: u64,
    /// A chunk fetched out of band.
    side: Option<Arc<Side>>,
    side_chunk: u64,
}

impl SourceReader {
    fn read_side(&mut self, buf: &mut [u8], total: u64) -> io::Result<usize> {
        let hit = self.side.as_ref().is_some_and(|side| self.pos >= side.start && self.pos < side.start + side.len);
        if !hit {
            if let Some(old) = self.side.take() {
                old.dropped.store(true, Ordering::Relaxed);
            }
            let len = self.side_chunk.min(total - self.pos);
            let side = Arc::new(Side {
                start: self.pos,
                len,
                data: Mutex::default(),
                arrived: Condvar::new(),
                dropped: AtomicBool::new(false),
            });
            let (shared, fetching) = (self.shared.clone(), side.clone());
            std::thread::Builder::new().name("download-part".into()).spawn(move || fetch_side(&shared, &fetching))?;
            self.side = Some(side);
            // Sequential reads through side chunks mean a seek is playing out: fetch more per trip.
            self.side_chunk = (self.side_chunk * 2).min(SIDE_CHUNK_MAX);
        }
        let Some(side) = self.side.clone() else { return Err(io::Error::other("no fetch under way")) };
        let from = (self.pos - side.start) as usize;
        let mut data = side.data.lock().unwrap();
        let (mut had, mut since) = (data.bytes.len(), Instant::now());
        // What went wrong, in a form that makes the next read ask again from scratch.
        let give_up = |reader: &mut SourceReader, reason: String| {
            side.dropped.store(true, Ordering::Relaxed);
            reader.side = None;
            reader.shared.trouble.store(true, Ordering::Relaxed);
            Err(io::Error::other(reason))
        };
        loop {
            if data.bytes.len() > from {
                let n = buf.len().min(data.bytes.len() - from);
                buf[..n].copy_from_slice(&data.bytes[from..from + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if let Some(reason) = data.error.clone() {
                drop(data);
                return give_up(self, reason);
            }
            if self.shared.interrupt.stops_this_thread() {
                return Err(interrupted());
            }
            if self.shared.cancel.load(Ordering::Relaxed) {
                return Err(io::Error::other("cancelled"));
            }
            data = side.arrived.wait_timeout(data, WAIT_STEP).unwrap().0;
            if data.bytes.len() != had {
                (had, since) = (data.bytes.len(), Instant::now());
            } else if since.elapsed() > STALL_GIVE_UP {
                drop(data);
                return give_up(self, "The connection to the server went quiet.".into());
            }
        }
    }
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut waited_since: Option<Instant> = None;
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if self.shared.live && self.pos < state.downloaded {
                if self.pos < state.held_from {
                    return Err(io::Error::other("that part of the live stream is gone"));
                }
                let skip = (self.pos - state.held_from) as usize;
                let n = buf.len().min(state.held.len() - skip);
                for (slot, byte) in buf[..n].iter_mut().zip(state.held.iter().skip(skip)) {
                    *slot = *byte;
                }
                self.pos += n as u64;
                // Let go of what has been played, and wake the download if it was waiting.
                let stale = (self.pos - state.held_from).saturating_sub(LIVE_BEHIND) as usize;
                if stale > 0 {
                    state.held.drain(..stale);
                    state.held_from += stale as u64;
                    self.shared.progress.notify_all();
                }
                return Ok(n);
            }
            if self.pos < state.downloaded {
                let Some(file) = self.file.as_mut() else {
                    return Err(io::Error::other("no buffer file"));
                };
                let available = (state.downloaded - self.pos) as usize;
                drop(state);
                if self.file_pos != self.pos {
                    file.seek(SeekFrom::Start(self.pos))?;
                }
                let want = buf.len().min(available);
                let n = file.read(&mut buf[..want])?;
                self.pos += n as u64;
                self.file_pos = self.pos;
                return Ok(n);
            }
            if state.done {
                return Ok(0);
            }
            if let Some(err) = &state.error {
                return Err(io::Error::other(err.clone()));
            }
            if self.shared.cancel.load(Ordering::Relaxed) {
                return Err(io::Error::other("cancelled"));
            }
            if self.shared.interrupt.stops_this_thread() {
                return Err(interrupted());
            }
            if let Some(total) = state.total {
                if self.pos >= total {
                    return Ok(0);
                }
                if self.shared.ranged && self.pos - state.downloaded > SIDE_THRESHOLD {
                    drop(state);
                    return self.read_side(buf, total);
                }
            }
            let before = state.downloaded;
            state = self.shared.progress.wait_timeout(state, WAIT_STEP).unwrap().0;
            // Out of bytes to hand over: see that the download hasn't gone quiet.
            self.shared.watch(&mut state);
            if state.downloaded != before {
                waited_since = None;
            } else if self.shared.live {
                let since = *waited_since.get_or_insert_with(Instant::now);
                if since.elapsed() > STALL_GIVE_UP {
                    self.shared.trouble.store(true, Ordering::Relaxed);
                    return Err(io::Error::other("The station's stream went quiet."));
                }
            }
        }
    }
}

impl Seek for SourceReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => self.pos as i128 + n as i128,
            SeekFrom::End(n) => {
                let total = self.byte_len().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::Unsupported, "stream length unknown")
                })?;
                total as i128 + n as i128
            }
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl MediaSource for SourceReader {
    fn is_seekable(&self) -> bool {
        self.shared.ranged && !self.shared.seek_held.load(Ordering::Relaxed)
    }

    fn byte_len(&self) -> Option<u64> {
        let state = self.shared.state.lock().unwrap();
        if self.shared.ranged || state.done { state.total } else { None }
    }
}

/// Deletes buffer files left behind by a previous run that didn't exit cleanly.
pub fn clean_stale(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().ends_with(".part") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};

    type Range = Option<(usize, Option<usize>)>;

    /// A web server for one test. `handle` is given each connection in turn: which one it is
    /// (0 for the first), the byte range asked for, and the connection to answer on.
    fn server(handle: impl Fn(usize, Range, &mut TcpStream) + Send + Sync + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/song", listener.local_addr().unwrap());
        let handle = Arc::new(handle);
        std::thread::spawn(move || {
            for (index, stream) in listener.incoming().flatten().enumerate() {
                let handle = handle.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut lines = BufReader::new(stream.try_clone().unwrap());
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if lines.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.to_lowercase().strip_prefix("range: bytes=") {
                            let (from, to) = value.trim().split_once('-').unwrap();
                            range = Some((from.parse().unwrap(), to.parse().ok()));
                        }
                    }
                    handle(index, range, &mut stream);
                });
            }
        });
        address
    }

    fn head(stream: &mut TcpStream, status: &str, length: Option<usize>) {
        let length = length.map(|n| format!("Content-Length: {n}\r\n")).unwrap_or_default();
        let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: audio/flac\r\n{length}Connection: close\r\n\r\n");
    }

    /// Keeps a connection open and silent, the way a dead one looks from this end.
    fn go_quiet(stream: &mut TcpStream) {
        let _ = stream.flush();
        std::thread::sleep(Duration::from_secs(30));
    }

    fn song(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn start(url: String, ranged: bool, keep: Option<(PathBuf, u64)>, interrupt: Arc<Interrupt>) -> (Download, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nexus-source-test-{}-{}", std::process::id(), NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&dir).unwrap();
        let client = Client::new("http://127.0.0.1:9", "someone", "secret");
        (Download::start(client, url, ranged, false, &dir, keep, interrupt), dir)
    }

    #[test]
    fn picks_up_a_download_that_went_quiet() {
        let content = song(300_000);
        let served = content.clone();
        let url = server(move |index, range, stream| match (index, range) {
            // The first connection brings a third of the file, then nothing more.
            (0, None) => {
                head(stream, "200 OK", Some(served.len()));
                let _ = stream.write_all(&served[..100_000]);
                go_quiet(stream);
            }
            // Asked again from where that stopped, the server sends the rest.
            (_, Some((from, None))) => {
                head(stream, "206 Partial Content", Some(served.len() - from));
                let _ = stream.write_all(&served[from..]);
            }
            _ => go_quiet(stream),
        });
        let (download, dir) = start(url, true, None, Arc::default());
        let began = Instant::now();
        let mut got = Vec::new();
        download.reader().unwrap().read_to_end(&mut got).unwrap();
        assert!(got == content, "the file arrived whole and in order");
        assert!(began.elapsed() < Duration::from_secs(8), "took {:?}", began.elapsed());
        assert!(!download.troubled());
        drop(download);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn gives_up_on_a_download_that_stays_quiet() {
        let content = song(50_000);
        let url = server(move |index, _, stream| {
            if index == 0 {
                head(stream, "200 OK", Some(content.len()));
                let _ = stream.write_all(&content[..10_000]);
            }
            go_quiet(stream);
        });
        let (download, dir) = start(url, true, None, Arc::default());
        let mut got = Vec::new();
        let error = download.reader().unwrap().read_to_end(&mut got).unwrap_err();
        assert_eq!(got.len(), 10_000);
        assert!(error.to_string().contains("went quiet"), "{error}");
        assert!(download.troubled());
        drop(download);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stops_waiting_when_the_engine_has_a_command() {
        let content = song(50_000);
        let url = server(move |index, _, stream| {
            if index == 0 {
                head(stream, "200 OK", Some(content.len()));
                let _ = stream.write_all(&content[..10_000]);
            }
            go_quiet(stream);
        });
        let interrupt: Arc<Interrupt> = Arc::default();
        // This thread stands in for the engine's.
        interrupt.claim();
        let (download, dir) = start(url, true, None, interrupt.clone());
        let mut reader = download.reader().unwrap();
        let mut got = vec![0u8; 10_000];
        reader.read_exact(&mut got).unwrap();
        let raiser = interrupt.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            raiser.raise();
        });
        let began = Instant::now();
        let error = reader.read(&mut got).unwrap_err();
        assert!(error.to_string().contains("interrupted"), "{error}");
        assert!(began.elapsed() < Duration::from_millis(900), "took {:?}", began.elapsed());
        // The command has been taken: reads wait again.
        interrupt.lower();
        assert!(!interrupt.raised());
        drop(download);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn fetches_a_far_off_part_without_waiting_for_what_comes_before() {
        let content = song(2_000_000);
        let served = content.clone();
        let url = server(move |_, range, stream| match range {
            // The download from the start never gets anywhere.
            None | Some((_, None)) => {
                head(stream, "200 OK", Some(served.len()));
                go_quiet(stream);
            }
            Some((from, Some(to))) => {
                head(stream, "206 Partial Content", Some(to + 1 - from));
                let _ = stream.write_all(&served[from..=to]);
            }
        });
        let (download, dir) = start(url, true, None, Arc::default());
        let mut reader = download.reader().unwrap();
        reader.seek(SeekFrom::Start(1_500_000)).unwrap();
        let mut got = vec![0u8; 100_000];
        reader.read_exact(&mut got).unwrap();
        assert!(got == content[1_500_000..1_600_000]);
        drop(download);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn keeps_a_conversion_only_when_all_of_it_came() {
        // A conversion comes with no length; the connection closing is all that ends it.
        for (sent, kept) in [(20_000, false), (60_000, true)] {
            let content = song(sent);
            let served = content.clone();
            let url = server(move |_, _, stream| {
                head(stream, "200 OK", None);
                let _ = stream.write_all(&served);
            });
            let dir = std::env::temp_dir().join(format!("nexus-source-keep-{}-{sent}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            let keep = dir.join("kept.320.mp3");
            let client = Client::new("http://127.0.0.1:9", "someone", "secret");
            // The whole song would come to at least 50,000 bytes.
            let download = Download::start(client, url, false, false, &dir, Some((keep.clone(), 50_000)), Arc::default());
            let mut got = Vec::new();
            download.reader().unwrap().read_to_end(&mut got).unwrap();
            assert!(got == content);
            assert_eq!(keep.exists(), kept, "{sent} bytes of a song that is at least 50,000");
            drop(download);
            let _ = fs::remove_dir_all(dir);
        }
    }
}
