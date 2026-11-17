//! A seekable byte source for the decoder, backed by an HTTP download.
//!
//! The track downloads sequentially into a temporary file while the decoder reads from the
//! same file. Reads that land far ahead of the download (the tail of the file while probing,
//! or a user seek) are answered with small HTTP range requests instead of waiting for
//! everything in between.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

use crate::subsonic::Client;

/// Reads further ahead of the download than this are served by a range request.
const SIDE_THRESHOLD: u64 = 768 * 1024;
const SIDE_CHUNK_MIN: u64 = 64 * 1024;
const SIDE_CHUNK_MAX: u64 = 4 * 1024 * 1024;
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How much of a live stream is held ahead of the decoder before the download waits, and how
/// much already-played audio is kept behind it.
const LIVE_AHEAD: usize = 2 * 1024 * 1024;
const LIVE_BEHIND: u64 = 256 * 1024;

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct State {
    /// Set once the response headers have arrived (or the request failed).
    opened: bool,
    downloaded: u64,
    total: Option<u64>,
    done: bool,
    error: Option<String>,
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
    state: Mutex<State>,
    progress: Condvar,
    cancel: AtomicBool,
    /// While set, the source claims it can't seek. See `Download::hold_seeking`.
    seek_held: AtomicBool,
}

impl Shared {
    fn fail(&self, message: String) {
        let mut state = self.state.lock().unwrap();
        state.opened = true;
        state.error = Some(message);
        self.progress.notify_all();
    }
}

/// Owns one track's download. Dropping it cancels the transfer and deletes the file.
pub struct Download {
    shared: Arc<Shared>,
}

impl Download {
    pub fn start(client: Client, url: String, ranged: bool, live: bool, dir: &std::path::Path) -> Download {
        let id = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("stream-{}-{id}.part", std::process::id()));
        let shared = Arc::new(Shared {
            client,
            url,
            ranged,
            live,
            title: Arc::new(Mutex::new(String::new())),
            path,
            state: Mutex::new(State::default()),
            progress: Condvar::new(),
            cancel: AtomicBool::new(false),
            seek_held: AtomicBool::new(false),
        });
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("download".into())
            .spawn(move || if worker.live { live_loop(&worker) } else { download_loop(&worker) })
            .expect("failed to start a download thread");
        Download { shared }
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

fn download_loop(shared: &Shared) {
    let mut file = match OpenOptions::new().create(true).write(true).truncate(true).open(&shared.path)
    {
        Ok(f) => f,
        Err(e) => return shared.fail(format!("Couldn't create a buffer file: {e}")),
    };
    let mut attempts = 0;
    let mut buf = vec![0u8; 64 * 1024];
    'request: loop {
        let offset = shared.state.lock().unwrap().downloaded;
        let range = (offset > 0).then_some((offset, None));
        let (mut body, length) = match shared.client.open_stream(&shared.url, range) {
            Ok(v) => v,
            Err(e) => {
                attempts += 1;
                if attempts > 3 || (offset > 0 && !shared.ranged) {
                    return finish(shared, Some(e.to_string()));
                }
                std::thread::sleep(Duration::from_millis(600 * attempts));
                continue 'request;
            }
        };
        {
            let mut state = shared.state.lock().unwrap();
            if !state.opened {
                state.opened = true;
                state.total = if shared.ranged { length } else { None };
            }
            shared.progress.notify_all();
        }
        loop {
            if shared.cancel.load(Ordering::Relaxed) {
                drop(file);
                let _ = fs::remove_file(&shared.path);
                return;
            }
            match body.read(&mut buf) {
                Ok(0) => return finish(shared, None),
                Ok(n) => {
                    if let Err(e) = file.write_all(&buf[..n]) {
                        return finish(shared, Some(format!("Couldn't buffer the track: {e}")));
                    }
                    let mut state = shared.state.lock().unwrap();
                    state.downloaded += n as u64;
                    shared.progress.notify_all();
                }
                Err(e) => {
                    attempts += 1;
                    if attempts > 3 || !shared.ranged {
                        return finish(shared, Some(format!("The connection dropped: {e}")));
                    }
                    std::thread::sleep(Duration::from_millis(600 * attempts));
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
        Err(e) => return finish(shared, Some(e.to_string())),
    };
    {
        let mut state = shared.state.lock().unwrap();
        state.opened = true;
        shared.progress.notify_all();
    }
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match body.read(&mut buf) {
            Ok(0) => return finish(shared, None),
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
            Err(e) => return finish(shared, Some(format!("The station's stream dropped: {e}"))),
        }
    }
}

fn finish(shared: &Shared, error: Option<String>) {
    let mut state = shared.state.lock().unwrap();
    state.opened = true;
    match error {
        // A short read on a known-length file is an error; treat clean EOF as completion.
        None => {
            state.done = true;
            state.total = Some(state.downloaded);
        }
        Some(e) => state.error = Some(e),
    }
    shared.progress.notify_all();
}

pub struct SourceReader {
    shared: Arc<Shared>,
    /// `None` for a live stream, which is read from memory.
    file: Option<File>,
    pos: u64,
    /// Where the file handle's cursor currently is, to skip redundant seeks.
    file_pos: u64,
    /// A chunk fetched out of band: its start offset and bytes.
    side: Option<(u64, Vec<u8>)>,
    side_chunk: u64,
}

impl SourceReader {
    fn read_side(&mut self, buf: &mut [u8], total: u64) -> io::Result<usize> {
        let hit = matches!(&self.side, Some((start, data))
            if self.pos >= *start && self.pos < *start + data.len() as u64);
        if !hit {
            let end = (self.pos + self.side_chunk).min(total) - 1;
            let (mut body, _) = self
                .shared
                .client
                .open_stream(&self.shared.url, Some((self.pos, Some(end))))
                .map_err(|e| io::Error::other(e.to_string()))?;
            let mut data = Vec::with_capacity((end - self.pos + 1) as usize);
            body.read_to_end(&mut data)?;
            if data.is_empty() {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty range response"));
            }
            self.side = Some((self.pos, data));
            // Sequential reads through side chunks mean a seek is playing out: fetch more per trip.
            self.side_chunk = (self.side_chunk * 2).min(SIDE_CHUNK_MAX);
        }
        let (start, data) = self.side.as_ref().unwrap();
        let from = (self.pos - start) as usize;
        let n = buf.len().min(data.len() - from);
        buf[..n].copy_from_slice(&data[from..from + n]);
        self.pos += n as u64;
        Ok(n)
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
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
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
            let (next, _) =
                self.shared.progress.wait_timeout(state, Duration::from_millis(250)).unwrap();
            state = next;
            if state.downloaded == before {
                let since = *waited_since.get_or_insert_with(Instant::now);
                if since.elapsed() > STALL_TIMEOUT {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "the download stalled"));
                }
            } else {
                waited_since = None;
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
