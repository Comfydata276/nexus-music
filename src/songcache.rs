//! Songs kept on this PC so they can play without the server.
//!
//! Two things fill it. A song that's streamed is kept once it has all arrived, at no extra
//! cost. And the next few songs in the queue are fetched ahead of time, so that if the
//! connection drops the music carries on for a while. Unlike downloads, nothing here is
//! chosen by hand or promised to stay: when the space allowed runs out, whatever was played
//! longest ago goes first.

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::subsonic::Client;

/// A file that's still arriving ends in one of these.
const ARRIVING: [&str; 2] = ["fetch", "adopt"];

#[derive(Clone)]
pub struct SongCache {
    dir: PathBuf,
}

impl SongCache {
    pub fn new(dir: PathBuf) -> SongCache {
        // Whatever was half-fetched when the app last closed is no use.
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if is_arriving(&entry.path()) {
                let _ = fs::remove_file(entry.path());
            }
        }
        SongCache { dir }
    }

    /// Where a song is (or would be) kept. `bitrate` is 0 for the file as the server has it,
    /// otherwise the rate it was converted to: the two are different files.
    pub fn file(&self, song: &str, bitrate: u32, suffix: &str) -> PathBuf {
        let safe: String = song.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let form = if bitrate == 0 { "raw".to_string() } else { bitrate.to_string() };
        self.dir.join(format!("{safe}.{form}.{}", if suffix.is_empty() { "bin" } else { suffix }))
    }

    /// Bytes kept.
    pub fn size(&self) -> u64 {
        crate::config::dir_size(&self.dir)
    }

    pub fn clear(&self) {
        for entry in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let _ = fs::remove_file(entry.path());
        }
    }

    /// Makes room: removes the songs played longest ago until what's kept fits in `limit`
    /// bytes. Those in `keep` (playing now, or about to) are left alone.
    pub fn trim(&self, limit: u64, keep: &HashSet<PathBuf>) {
        let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let meta = entry.metadata().ok()?;
                (meta.is_file() && !is_arriving(&entry.path())).then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len(), entry.path()))
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
        files.sort();
        for (_, size, path) in files {
            if total <= limit {
                break;
            }
            if !keep.contains(&path) && fs::remove_file(&path).is_ok() {
                total -= size;
            }
        }
    }
}

fn is_arriving(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ARRIVING.iter().any(|arriving| ext == *arriving))
}

/// Whether a kept song is there to be played.
pub fn ready(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() > 0)
}

/// Notes that a kept song has just been played, so it's among the last to make room.
pub fn touch(path: &Path) {
    if let Ok(file) = fs::File::options().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// Keeps a copy of a file that has finished streaming. A second name for the same data where
/// the disk allows it, which costs nothing; a copy otherwise.
pub fn adopt(streamed: &Path, keep: &Path) {
    if ready(keep) {
        return;
    }
    let arriving = keep.with_extension("adopt");
    let _ = fs::remove_file(&arriving);
    if fs::hard_link(streamed, &arriving).is_err() && fs::copy(streamed, &arriving).is_err() {
        let _ = fs::remove_file(&arriving);
        return;
    }
    if fs::rename(&arriving, keep).is_err() {
        let _ = fs::remove_file(&arriving);
    }
}

// ---- fetching ahead ---------------------------------------------------------------------------------

/// A song to fetch: where from, and where to keep it.
#[derive(Clone)]
pub struct Wanted {
    pub client: Client,
    pub url: String,
    pub path: PathBuf,
    /// The fewest bytes the whole song can come to, for a conversion (whose length the
    /// server doesn't announce); 0 for a file as it is.
    pub least: u64,
}

struct Plan {
    wanted: VecDeque<Wanted>,
    /// The one being fetched now.
    busy: Option<PathBuf>,
}

/// Fetches songs ahead of time, one at a time, in the background.
pub struct Ahead {
    plan: Arc<(Mutex<Plan>, Condvar)>,
    /// Set to abandon the fetch under way.
    stop: Arc<AtomicBool>,
}

impl Ahead {
    pub fn new() -> Ahead {
        let plan = Arc::new((Mutex::new(Plan { wanted: VecDeque::new(), busy: None }), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, halt) = (plan.clone(), stop.clone());
        let _ = std::thread::Builder::new().name("fetch-ahead".into()).spawn(move || {
            let (lock, wake) = &*shared;
            loop {
                let next = {
                    let mut plan = lock.lock().unwrap_or_else(|stuck| stuck.into_inner());
                    plan.busy = None;
                    loop {
                        if let Some(next) = plan.wanted.pop_front() {
                            plan.busy = Some(next.path.clone());
                            break next;
                        }
                        plan = wake.wait(plan).unwrap_or_else(|stuck| stuck.into_inner());
                    }
                };
                halt.store(false, Ordering::Relaxed);
                if !ready(&next.path) {
                    fetch(&next, &halt);
                }
            }
        });
        Ahead { plan, stop }
    }

    /// Says which songs should be fetched, soonest needed first. Replaces what was asked
    /// for before; a fetch already under way carries on if it's still wanted.
    pub fn want(&self, wanted: Vec<Wanted>) {
        let (lock, wake) = &*self.plan;
        let mut plan = lock.lock().unwrap_or_else(|stuck| stuck.into_inner());
        if plan.busy.as_ref().is_some_and(|busy| !wanted.iter().any(|want| &want.path == busy)) {
            self.stop.store(true, Ordering::Relaxed);
        }
        let busy = plan.busy.clone();
        plan.wanted = wanted.into_iter().filter(|want| Some(&want.path) != busy.as_ref() && !ready(&want.path)).collect();
        wake.notify_one();
    }
}

/// How a fetch is getting on, shared between the thread doing it and the one watching it.
#[derive(Default)]
struct Transfer {
    arrived: AtomicU64,
    over: AtomicBool,
    /// The watcher has walked away: the file is no longer this transfer's to touch.
    dropped: AtomicBool,
}

/// How long a fetch may bring nothing before it's walked away from.
const QUIET: Duration = Duration::from_secs(30);

/// Downloads one song to where it's kept. Left unfinished if told to stop, the connection
/// fails or it goes quiet; it'll be asked for again if it's still wanted.
///
/// The transfer runs on a thread of its own, because a read from a connection that has died
/// without closing never returns. This thread only watches, and so can always move on.
fn fetch(want: &Wanted, stop: &AtomicBool) {
    let transfer = Arc::new(Transfer::default());
    let (job, theirs) = (want.clone(), transfer.clone());
    let spawned = std::thread::Builder::new().name("fetch-ahead-transfer".into()).spawn(move || {
        bring(&job, &theirs);
        theirs.over.store(true, Ordering::Relaxed);
    });
    if spawned.is_err() {
        return;
    }
    let (mut had, mut since) = (0, Instant::now());
    while !transfer.over.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        let now = transfer.arrived.load(Ordering::Relaxed);
        if now != had {
            (had, since) = (now, Instant::now());
        }
        if stop.load(Ordering::Relaxed) || since.elapsed() > QUIET {
            transfer.dropped.store(true, Ordering::Relaxed);
            let _ = fs::remove_file(want.path.with_extension("fetch"));
            return;
        }
    }
}

fn bring(want: &Wanted, transfer: &Transfer) {
    let arriving = want.path.with_extension("fetch");
    let dropped = || transfer.dropped.load(Ordering::Relaxed);
    let done = (|| -> Result<(), String> {
        let (mut body, length) = want.client.open_stream(&want.url, None).map_err(|e| e.to_string())?;
        let mut file = fs::File::create(&arriving).map_err(|e| e.to_string())?;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut got: u64 = 0;
        loop {
            match body.read(&mut buffer).map_err(|e| e.to_string())? {
                _ if dropped() => return Err("stopped".into()),
                0 => break,
                count => {
                    file.write_all(&buffer[..count]).map_err(|e| e.to_string())?;
                    got += count as u64;
                    transfer.arrived.store(got, Ordering::Relaxed);
                }
            }
        }
        // Short of what was promised, or of what the whole song must come to: the
        // connection closed early.
        if got == 0 || got < want.least || length.is_some_and(|length| got < length) {
            return Err("incomplete".into());
        }
        file.flush().map_err(|e| e.to_string())
    })();
    if dropped() {
        return;
    }
    if done.is_err() || fs::rename(&arriving, &want.path).is_err() {
        let _ = fs::remove_file(&arriving);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_room_oldest_first() {
        let dir = std::env::temp_dir().join(format!("nexus-songcache-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("half.fetch"), b"xx").unwrap();
        let cache = SongCache::new(dir.clone());
        assert!(!dir.join("half.fetch").exists());

        let (a, b, c) = (cache.file("song/a", 0, "flac"), cache.file("song-b", 320, "mp3"), cache.file("c", 0, ""));
        assert_eq!(a.file_name().unwrap(), "song_a.raw.flac");
        assert_eq!(b.file_name().unwrap(), "song-b.320.mp3");
        assert_eq!(c.file_name().unwrap(), "c.raw.bin");
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for (path, age) in [(&a, 3), (&b, 2), (&c, 1)] {
            fs::write(path, vec![0u8; 1000]).unwrap();
            fs::File::options().write(true).open(path).unwrap().set_modified(old + std::time::Duration::from_secs(10 - age)).unwrap();
        }
        assert_eq!(cache.size(), 3000);
        assert!(ready(&a) && !ready(&dir.join("missing")));

        // Room for two: the oldest goes, unless it's being kept for playing.
        cache.trim(2000, &HashSet::from([a.clone()]));
        assert!(a.exists() && !b.exists() && c.exists());
        // Played just now, it outlasts the other.
        touch(&a);
        cache.trim(1000, &HashSet::new());
        assert!(a.exists() && !c.exists());

        // A finished stream is kept under the song's name.
        let streamed = dir.join("stream-1.part");
        fs::write(&streamed, b"audio").unwrap();
        adopt(&streamed, &b);
        assert_eq!(fs::read(&b).unwrap(), b"audio");
        assert!(!dir.join("song-b.320.adopt").exists());
        cache.clear();
        assert_eq!(cache.size(), 0);
        let _ = fs::remove_dir_all(&dir);
    }
}
