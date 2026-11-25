//! Every play, with when it happened, kept on this PC.
//!
//! The server counts how often each song has been played but not when, and Recently played
//! only goes back a hundred songs. Neither can say what was played most this month. This
//! log can, from the day it starts: one line per play that counted (the same rule as for
//! reporting plays: half the song, or four minutes).
//!
//! It is a file that is only ever added to, a line at a time, so noting a play costs the
//! same however long the log has grown.
//!
//! A play made from a playlist says which (from version 0.40), so the playlists played most
//! can be told too.

use std::collections::HashMap;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// More than anyone plays in ten years; the oldest go first after that.
const LIMIT: usize = 200_000;
/// How far past the limit the file may grow before the oldest lines are taken out of it.
const SLACK: usize = 5_000;

pub struct PlayLog {
    /// The log: a song id and when it was played (seconds since 1970) on each line, oldest
    /// first; and after those, for a play made from a playlist, that page's route key.
    path: PathBuf,
    lines: usize,
    /// Plays made here that could not be written to the file when they were made (the disk
    /// was full): they are counted for this run, and written with the next thing that is.
    /// Until they are, nothing is said to be kept (see `keep`).
    unwritten: Vec<(String, i64, String)>,
    /// The log is not to be written to in this run: it could not be read when the app
    /// started (see `load`). What would be added to it is added to a file beside it
    /// instead (`side`), and joined to it the next time it reads.
    aside: bool,
    /// Every line of the file was read. Only then is the file ever written again whole
    /// (from the plays in memory): never over lines that this run could not read.
    whole: bool,
    /// The log is there and could not be read (`note`).
    unread: bool,
    /// Plays put back into a log that an earlier version was stopped while starting
    /// (`note`).
    restored: usize,
}

/// The log is there and could not be read when the app started: what is counted is only
/// what has been played since, and nothing says the rest is gone.
static UNREAD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn unread() -> bool {
    UNREAD.load(std::sync::atomic::Ordering::SeqCst)
}

/// The plays in a log's text, each with where it was played from if that was noted; and
/// whether every line of it read. The lines that read, read; the last record ends where
/// `ending` says.
fn read(bytes: &[u8]) -> (Vec<(String, i64, String)>, bool) {
    let lines: Vec<&[u8]> = bytes[..ending(bytes).0].split(|byte| *byte == b'\n').filter(|line| !line.iter().all(u8::is_ascii_whitespace)).collect();
    let plays: Vec<(String, i64, String)> = lines.iter().filter_map(|line| read_line(line)).collect();
    let whole = plays.len() == lines.len();
    (plays, whole)
}

fn text(plays: &[(String, i64, String)]) -> String {
    plays.iter().map(|(song, at, from)| line(song, *at, from)).collect()
}

/// A line of the log as it lies in the file.
fn read_line(bytes: &[u8]) -> Option<(String, i64, String)> {
    std::str::from_utf8(bytes).ok().and_then(parse)
}

/// Where a log's last record ends: **the one rule, for reading the log and for adding to
/// it.** The log is whole lines. What follows the last line end is a play if it reads as
/// one (the app was stopped after writing it and before writing its line end): it is then
/// part of the log, with its line end still to be put after it. Anything else there is the
/// beginning of a line that was never finished, and is no part of the log. Returns how many
/// of the bytes are the log, and whether a line end is wanting after them.
fn ending(bytes: &[u8]) -> (usize, bool) {
    let start = bytes.iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
    match &bytes[start..] {
        [] => (bytes.len(), false),
        last if read_line(last).is_some() => (bytes.len(), true),
        _ => (start, false),
    }
}

/// Adds lines to the end of a log, and says whether they are there. They go where the log's
/// last record ends (`ending`): after a line end put in if the last play had none, and in
/// place of the beginning of a line a failed write left (a line written straight after
/// that would be joined to it and read as neither). If these lines can't all be written,
/// the file is put back as it was before them, so that writing them again later doesn't
/// make some of them twice.
fn append(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(path)?;
    let length = file.metadata()?.len();
    // The end of the file is enough to find its last line, unless that line is longer.
    let (mut from, mut tail) = (length.saturating_sub(64 * 1024), Vec::new());
    loop {
        file.seek(SeekFrom::Start(from))?;
        tail.clear();
        file.read_to_end(&mut tail)?;
        if from == 0 || tail.contains(&b'\n') {
            break;
        }
        from = 0;
    }
    let (kept, wanting) = ending(&tail);
    let whole = from + kept as u64;
    if whole < length {
        file.set_len(whole)?;
    }
    file.seek(SeekFrom::Start(whole))?;
    let end = if wanting { "\n" } else { "" };
    let written = file.write_all(end.as_bytes()).and_then(|()| file.write_all(text.as_bytes())).and_then(|()| file.sync_data());
    if written.is_err() {
        let _ = file.set_len(whole);
    }
    written
}

/// The newest plays, for sharing with other devices (see `sync`), and where those made from
/// a playlist were made from. The log itself is not copied.
pub fn newest() -> (Vec<(String, i64)>, Vec<(String, i64, String)>) {
    let plays = SHARED.read().map(|plays| nexus_core::sync::newest_plays(plays.iter())).unwrap_or_default();
    let from = match SOURCES.read() {
        Ok(sources) => plays.iter().filter_map(|play| sources.as_ref()?.get(play).map(|from| (play.0.clone(), play.1, from.clone()))).collect(),
        Err(_) => Vec::new(),
    };
    (plays, from)
}

/// The plays, for the threads that build pages.
static SHARED: RwLock<Vec<(String, i64)>> = RwLock::new(Vec::new());
/// Where the plays that were made from a playlist were made from: its route key
/// ("playlist:abc"), by the play.
static SOURCES: RwLock<Option<HashMap<(String, i64), String>>> = RwLock::new(None);

pub fn shared() -> Vec<(String, i64)> {
    SHARED.read().map(|plays| plays.clone()).unwrap_or_default()
}

/// The plays that were made from a playlist, each with that playlist's route key.
pub fn sources() -> Vec<(String, i64, String)> {
    let Ok(sources) = SOURCES.read() else { return Vec::new() };
    sources.iter().flatten().map(|((song, at), from)| (song.clone(), *at, from.clone())).collect()
}

fn source_of(song: &str, at: i64) -> String {
    let Ok(sources) = SOURCES.read() else { return String::new() };
    sources.as_ref().and_then(|sources| sources.get(&(song.to_string(), at)).cloned()).unwrap_or_default()
}

fn note_source(song: &str, at: i64, from: &str) {
    if from.is_empty() {
        return;
    }
    if let Ok(mut sources) = SOURCES.write() {
        sources.get_or_insert_with(HashMap::new).insert((song.to_string(), at), from.to_string());
    }
}

fn line(song: &str, at: i64, from: &str) -> String {
    let text = if from.is_empty() { serde_json::to_string(&(song, at)) } else { serde_json::to_string(&(song, at, from)) };
    format!("{}\n", text.unwrap_or_default())
}

/// A line of the log: the song, when, and where it was played from if that was noted.
fn parse(line: &str) -> Option<(String, i64, String)> {
    serde_json::from_str::<(String, i64, String)>(line)
        .ok()
        .or_else(|| serde_json::from_str::<(String, i64)>(line).ok().map(|(song, at)| (song, at, String::new())))
}

impl PlayLog {
    /// Reads the log.
    ///
    /// **A log that could not be read is never taken for a log that is not there.** Only
    /// "there is no such file" means there is none: then one is started (`begin`). Any
    /// other failure to read it (something else has the file open, the disk gave an error)
    /// leaves it exactly as it is on the disk: nothing is written to it in this run, the
    /// app says so (`note`), and what is played meanwhile is kept in the file beside it and
    /// joined to it the next time it reads. A log that reads but has damaged lines in it
    /// is read for the lines that read, and added to as usual; it is not written again
    /// whole (see `whole`).
    pub fn load(old: &Path, recent: &[(String, i64)]) -> PlayLog {
        PlayLog::load_by(old, recent, &crate::config::ON_DISK)
    }

    /// `load`, starting a log by the steps given (a test's, to stop the app at one or have
    /// one fail).
    fn load_by(old: &Path, recent: &[(String, i64)], steps: &crate::config::Steps) -> PlayLog {
        let path = old.with_extension("log");
        let mut log = PlayLog { path, lines: 0, unwritten: Vec::new(), aside: false, whole: true, unread: false, restored: 0 };
        if let Ok(mut sources) = SOURCES.write() {
            *sources = None;
        }
        // (Reading it is the one way of asking whether it is there: there is no check
        // first for the answer to change after.)
        let mut plays = match std::fs::read(&log.path) {
            Ok(bytes) => {
                let (plays, whole) = read(&bytes);
                log.whole = whole;
                plays
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match log.begin(old, recent, steps) {
                Ok(plays) => plays.into_iter().map(|(song, at)| (song, at, String::new())).collect(),
                Err(_) => {
                    log.aside = true;
                    Vec::new()
                }
            },
            Err(_) => {
                log.aside = true;
                log.unread = true;
                Vec::new()
            }
        };
        if !log.aside {
            log.put_back(old, &mut plays);
        }
        UNREAD.store(log.unread, std::sync::atomic::Ordering::SeqCst);
        // What was kept beside the log while it could not be read: joined to it now that
        // it can, each play once; or, if it still can't, counted for this run as it is.
        if let Ok(bytes) = std::fs::read(log.side()) {
            let mut have: std::collections::HashSet<(String, i64)> = plays.iter().map(|(song, at, _)| (song.clone(), *at)).collect();
            let beside: Vec<(String, i64, String)> = read(&bytes).0.into_iter().filter(|(song, at, _)| have.insert((song.clone(), *at))).collect();
            if !log.aside {
                match append(&log.path, &text(&beside)) {
                    Ok(()) => drop(std::fs::remove_file(log.side())),
                    Err(_) => log.aside = true,
                }
            }
            plays.extend(beside);
        }
        for (song, at, from) in &plays {
            note_source(song, *at, from);
        }
        log.lines = plays.len();
        if let Ok(mut shared) = SHARED.write() {
            *shared = plays.into_iter().map(|(song, at, _)| (song, at)).collect();
        }
        log.trim();
        log
    }

    /// What to tell the person, once, when the log was there and could not be read, or
    /// had plays put back into it (`put_back`).
    pub fn note(&self) -> Option<&'static str> {
        if self.unread {
            return Some(
                "The plays kept on this PC couldn't be read when the app started, so they have been left exactly as they are. “Your stats” counts only what's played from now until the app next starts; those plays are kept, and added to the rest then.",
            );
        }
        (self.restored > 0).then_some(
            "Some of the plays kept on this PC had gone missing: the app had been stopped while it was first setting them up. They were still in the older file they came from, and have been put back, so “Your stats” has them again.",
        )
    }

    /// The list from before 0.26 goes only once the log has all of it (`begin`), so a list
    /// still there beside a log that was read is one whose carrying over was cut short: by
    /// 0.40.10 or earlier, stopped after making the log at its name and before writing all
    /// of it in (an empty or half-written log, read as one with fewer plays). Its plays
    /// that the log lacks are put in, in order, and then it goes. (An empty log with no
    /// list beside it is left as it is: it can't be told from one started with nothing.)
    fn put_back(&mut self, old: &Path, plays: &mut Vec<(String, i64, String)>) {
        let Some(list) = std::fs::read(old).ok().and_then(|bytes| serde_json::from_slice::<Vec<(String, i64)>>(&bytes).ok()) else { return };
        let have: std::collections::HashSet<(String, i64)> = plays.iter().map(|(song, at, _)| (song.clone(), *at)).collect();
        let mut missing: Vec<(String, i64, String)> = list.into_iter().filter(|play| !have.contains(play)).map(|(song, at)| (song, at, String::new())).collect();
        missing.sort_by_key(|(_, at, _)| *at);
        missing.dedup();
        if !missing.is_empty() {
            let mut all = plays.clone();
            all.extend(missing.iter().cloned());
            all.sort_by_key(|(_, at, _)| *at);
            // A log read whole is written again whole, in order; one with lines that did not
            // read is only added to (`whole`).
            let written = if self.whole { crate::config::write_surely(&self.path, text(&all).as_bytes()) } else { append(&self.path, &text(&missing)) };
            if written.and_then(|()| (crate::config::ON_DISK.sync_folder)(&self.path)).is_err() {
                return;
            }
            self.restored = missing.len();
            *plays = all;
        }
        let _ = std::fs::remove_file(old);
    }

    /// Where plays are kept while the log itself is not to be written to (see `aside`).
    fn side(&self) -> PathBuf {
        self.path.with_extension("log.new")
    }

    /// The file plays are added to in this run.
    fn target(&self) -> PathBuf {
        if self.aside { self.side() } else { self.path.clone() }
    }

    /// Starts a log where there is none. `old` is where versions before 0.26 kept the plays,
    /// as one list rewritten on every play: that is carried over, once, and goes when the
    /// log has it. If it is there and can't be read just now, no log is made without it
    /// (an error); if it reads and isn't such a list, it is left where it is. Failing a
    /// list, the log starts with `recent`: what Recently played remembers, so that it
    /// isn't empty on day one. The file comes to its name whole or not at all
    /// (`config::write_once`), only if nothing has the name: a log is never written over by
    /// this, and what a stopped start left beside it is never the log. The list goes only
    /// once the log is on the disk.
    fn begin(&self, old: &Path, recent: &[(String, i64)], steps: &crate::config::Steps) -> std::io::Result<Vec<(String, i64)>> {
        let carried = match std::fs::read(old) {
            Ok(bytes) => serde_json::from_slice::<Vec<(String, i64)>>(&bytes).ok(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let from_old = carried.is_some();
        let mut plays = carried.unwrap_or_else(|| recent.to_vec());
        plays.sort_by_key(|(_, at)| *at);
        let lines: String = plays.iter().map(|(song, at)| line(song, *at, "")).collect();
        crate::config::write_once(&self.path, lines.as_bytes(), steps)?;
        if from_old {
            let _ = std::fs::remove_file(old);
        }
        Ok(plays)
    }

    fn rewrite(&self, plays: &[(String, i64)]) {
        let text: String = plays.iter().map(|(song, at)| line(song, *at, &source_of(song, *at))).collect();
        crate::config::write_atomically(&self.path, text.as_bytes());
    }

    /// Takes the oldest plays out once the log is well past its limit.
    fn trim(&mut self) {
        if self.lines <= LIMIT + SLACK || !self.whole || self.aside {
            return;
        }
        let Ok(mut shared) = SHARED.write() else { return };
        let extra = shared.len().saturating_sub(LIMIT);
        let gone: Vec<(String, i64)> = shared.drain(..extra).collect();
        if let Ok(mut sources) = SOURCES.write() {
            if let Some(sources) = sources.as_mut() {
                for play in &gone {
                    sources.remove(play);
                }
            }
        }
        self.rewrite(&shared);
        self.lines = shared.len();
    }

    /// Writes what is still waiting to be written (see `unwritten`).
    fn flush(&mut self) -> std::io::Result<()> {
        if self.unwritten.is_empty() {
            return Ok(());
        }
        append(&self.target(), &text(&self.unwritten))?;
        self.unwritten.clear();
        Ok(())
    }

    /// Takes in plays made on other devices (see `sync`), leaving out the ones already here,
    /// and **makes sure they are in the file before saying so**: and with them every play
    /// this PC has counted. `sources` says which playlists some of them were made from; a
    /// play already here whose source is only told now is given it. Returns how many were
    /// new; or, if the log could not be written (the disk is full, the file can't be
    /// opened), why, and then none of them is counted here.
    ///
    /// The record devices share holds only the latest plays. One that is about to fall out
    /// of it, and that this PC has not yet got, has to be safe here before this PC writes
    /// the record back without it: so nothing is written back on the strength of plays this
    /// PC could not keep, or of a log that could not be read back.
    pub fn keep(&mut self, plays: &[(String, i64)], sources: &[(String, i64, String)]) -> std::io::Result<usize> {
        // A play made here that never reached the file is in memory all the same: it must
        // not pass for kept.
        self.flush()?;
        let known: std::collections::HashSet<(String, i64)> = match SHARED.read() {
            // Anything another device has to tell is recent, so the newest part is enough to
            // compare with.
            Ok(shared) => shared.iter().rev().take(5000).cloned().collect(),
            Err(_) => return Err(std::io::Error::other("the log of plays is in use")),
        };
        let from = |song: &str, at: i64| sources.iter().find(|(other, when, _)| other == song && *when == at).map(|(_, _, from)| from.as_str()).unwrap_or("");
        let mut new: Vec<&(String, i64)> = plays.iter().filter(|play| !known.contains(*play)).collect();
        new.sort_by_key(|(_, at)| *at);
        new.dedup();
        if !new.is_empty() {
            let text: String = new.iter().map(|(song, at)| line(song, *at, from(song, *at))).collect();
            append(&self.target(), &text)?;
            for (song, at) in &new {
                note_source(song, *at, from(song, *at));
            }
            if let Ok(mut shared) = SHARED.write() {
                shared.extend(new.iter().map(|play| (*play).clone()));
            }
            self.lines += new.len();
        }
        // A play this PC already had, whose source another device knew: its line in the
        // file has none, so the file is written again, whole. The source is known here only
        // once the file has it: if the file can't be written, it is asked about again.
        let late: HashMap<(String, i64), &str> = sources
            .iter()
            .filter(|(song, at, from)| !from.is_empty() && known.contains(&(song.clone(), *at)) && source_of(song, *at).is_empty())
            .map(|(song, at, from)| ((song.clone(), *at), from.as_str()))
            .collect();
        // (Only a file that was read whole, and is being written to, is written again.)
        if !late.is_empty() && self.whole && !self.aside {
            let all = SHARED.read().map(|shared| shared.clone()).unwrap_or_default();
            let text: String = all.iter().map(|play| line(&play.0, play.1, late.get(play).map(|from| from.to_string()).unwrap_or_else(|| source_of(&play.0, play.1)).as_str())).collect();
            crate::config::write_surely(&self.path, text.as_bytes())?;
            for ((song, at), from) in &late {
                note_source(song, *at, from);
            }
        }
        self.trim();
        Ok(new.len())
    }

    /// Notes a play, and the playlist it was made from (its route key) if it was from one.
    /// It is counted at once; if it can't be written to the file now it is written with
    /// the next play that can be.
    pub fn add(&mut self, song: &str, at: i64, from: &str) {
        note_source(song, at, from);
        if let Ok(mut shared) = SHARED.write() {
            shared.push((song.to_string(), at));
        }
        self.unwritten.push((song.to_string(), at, from.to_string()));
        self.lines += 1;
        if self.flush().is_ok() {
            self.trim();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log's plays are held once for the whole app, so the cases take turns.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn turn() -> std::sync::MutexGuard<'static, ()> {
        ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A folder of its own for a case: where the list from before 0.26 would be, and the log.
    fn fresh(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("nexus-plays-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.join("plays.json"), dir.join("plays.log"))
    }

    fn play(song: &str, at: i64) -> (String, i64) {
        (song.to_string(), at)
    }

    /// Keeps a file from being read, as a virus scanner holding it does, until dropped.
    struct Held(PathBuf, #[allow(dead_code)] Option<std::fs::File>);

    fn hold(path: &Path) -> Held {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            // Open, and sharing it with nobody: any other open of it fails.
            Held(path.to_path_buf(), Some(std::fs::OpenOptions::new().read(true).share_mode(0).open(path).unwrap()))
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
            Held(path.to_path_buf(), None)
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644));
            }
        }
    }

    #[test]
    fn a_log_that_could_not_be_read_is_never_taken_for_a_log_that_is_not_there() {
        let _turn = turn();
        let (old, path) = fresh("unread");
        let side = path.with_extension("log.new");
        let years = "[\"a\",100]\n[\"b\",200]\n";
        std::fs::write(&path, years).unwrap();
        // What Recently played has: what a log would be started from if there were none.
        let recent = [play("z", 50)];
        {
            let held = hold(&path);
            if std::fs::read(&path).is_ok() {
                // (Whoever runs this may read anything: nothing here can be made unreadable.)
                return;
            }
            let mut log = PlayLog::load(&old, &recent);
            // It says so, and counts nothing it could not read.
            assert!(log.note().is_some() && unread());
            assert!(shared().is_empty());
            // A play made meanwhile, and plays other devices tell of (one of them a play the
            // log has, for all this run knows), are kept: beside the log, not in it.
            log.add("c", 300, "playlist:q");
            assert_eq!(log.keep(&[play("a", 100), play("d", 400)], &[]).unwrap(), 2);
            assert_eq!(shared(), [play("c", 300), play("a", 100), play("d", 400)]);
            drop(held);
        }
        // The log is exactly as it was.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), years);
        assert_eq!(std::fs::read_to_string(&side).unwrap(), "[\"c\",300,\"playlist:q\"]\n[\"a\",100]\n[\"d\",400]\n");
        // The next start reads it, and what was kept beside it is joined to it: each play once.
        let log = PlayLog::load(&old, &recent);
        assert!(log.note().is_none() && !unread());
        assert_eq!(shared(), [play("a", 100), play("b", 200), play("c", 300), play("d", 400)]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{years}[\"c\",300,\"playlist:q\"]\n[\"d\",400]\n"));
        assert!(!side.exists());
        assert_eq!(sources(), [("c".to_string(), 300, "playlist:q".to_string())]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_log_with_a_damaged_part_is_read_for_what_reads_and_never_written_again_whole() {
        let _turn = turn();
        let (old, path) = fresh("damaged");
        // Not valid text part-way, and a line that is no play.
        let damaged = b"[\"a\",100]\n\xff\xfe[\"b\",2\nnot a play\n[\"c\",300]\n".to_vec();
        std::fs::write(&path, &damaged).unwrap();
        let mut log = PlayLog::load(&old, &[play("z", 50)]);
        assert!(log.note().is_none());
        assert_eq!(shared(), [play("a", 100), play("c", 300)]);
        // A play is added after it; the rest of the file is as it was.
        log.add("d", 400, "");
        let after = [damaged.as_slice(), b"[\"d\",400]\n"].concat();
        assert_eq!(std::fs::read(&path).unwrap(), after);
        // A source told later would have the file written again, whole, from the plays that
        // were read: that is not done to a file with lines that were not.
        assert_eq!(log.keep(&[play("c", 300)], &[("c".to_string(), 300, "playlist:p".to_string())]).unwrap(), 0);
        assert_eq!(std::fs::read(&path).unwrap(), after);
        assert!(sources().is_empty());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_log_is_started_only_where_there_is_none_and_from_a_list_that_could_be_read() {
        let _turn = turn();
        let (old, path) = fresh("begin");
        // Whether there is a log is told by reading it, in one step: there is no moment
        // between a check and a read for it to change in. And if one has appeared by the
        // time the new one is made, it is not written over.
        std::fs::write(&path, "[\"a\",100]\n").unwrap();
        let log = PlayLog { path: path.clone(), lines: 0, unwritten: Vec::new(), aside: false, whole: true, unread: false, restored: 0 };
        assert!(log.begin(&old, &[play("z", 50)], &crate::config::ON_DISK).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"a\",100]\n");
        std::fs::remove_file(&path).unwrap();
        // The list from before 0.26 is there and is damaged: the log starts from Recently
        // played, and the list is left where it is.
        std::fs::write(&old, "[[\"a\",1").unwrap();
        PlayLog::load(&old, &[play("z", 50)]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"z\",50]\n");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "[[\"a\",1");
        std::fs::remove_file(&path).unwrap();
        // It is there and can't be read just now: no log is made without it.
        std::fs::write(&old, "[[\"a\",100]]").unwrap();
        {
            let _held = hold(&old);
            if std::fs::read(&old).is_err() {
                let mut log = PlayLog::load(&old, &[play("z", 50)]);
                log.add("c", 300, "");
                assert!(!path.exists());
            }
        }
        // And when it can: carried over, with what was played meanwhile.
        PlayLog::load(&old, &[play("z", 50)]);
        assert!(!old.exists());
        assert_eq!(shared().first(), Some(&play("a", 100)));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_last_record_ends_in_the_same_place_for_reading_and_for_writing() {
        let _turn = turn();
        let (old, path) = fresh("ending");
        // The app was killed after a play's closing bracket and before its line end. It is a
        // play to the reader, so it is one to the writer: the line end is put in, and the
        // next play goes after it.
        std::fs::write(&path, "[\"a\",100]\n[\"b\",200]").unwrap();
        let mut log = PlayLog::load(&old, &[]);
        assert_eq!(shared(), [play("a", 100), play("b", 200)]);
        assert_eq!(log.keep(&[play("b", 200), play("c", 300)], &[]).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"a\",100]\n[\"b\",200]\n[\"c\",300]\n");
        PlayLog::load(&old, &[]);
        assert_eq!(shared(), [play("a", 100), play("b", 200), play("c", 300)]);
        // A last line that does not read as a play is one to neither: passed over, and cut
        // away before anything is added.
        std::fs::write(&path, "[\"a\",100]\n[\"b\",2").unwrap();
        let mut log = PlayLog::load(&old, &[]);
        assert_eq!(shared(), [play("a", 100)]);
        log.add("c", 300, "");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"a\",100]\n[\"c\",300]\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_source_told_later_is_known_here_only_once_the_file_has_it() {
        let _turn = turn();
        let (old, path) = fresh("late");
        std::fs::write(&path, "[\"a\",100]\n[\"b\",200]\n").unwrap();
        let mut log = PlayLog::load(&old, &[]);
        let told = [("b".to_string(), 200, "playlist:p".to_string())];
        // The file can't be written again (a folder stands where its new text is put first).
        let way = path.with_extension("tmp");
        std::fs::create_dir_all(&way).unwrap();
        assert!(log.keep(&[play("b", 200)], &told).is_err());
        assert!(sources().is_empty(), "the source is not known here while the file does not have it");
        // It can now: the source is asked about again, and this time it is in the file.
        std::fs::remove_dir_all(&way).unwrap();
        assert_eq!(log.keep(&[play("b", 200)], &told).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"a\",100]\n[\"b\",200,\"playlist:p\"]\n");
        assert_eq!(sources(), told);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn keeps_plays_between_runs() {
        let _turn = turn();
        let old = std::env::temp_dir().join(format!("nexus-plays-test-{}.json", std::process::id()));
        let path = old.with_extension("log");
        let _ = std::fs::remove_file(&path);
        // A log from before 0.26: one list in one file. It's carried over, and the old file goes.
        std::fs::write(&old, r#"[["a",100],["b",200]]"#).unwrap();
        let mut log = PlayLog::load(&old, &[]);
        assert!(!old.exists());
        log.add("c", 300, "");
        // Adding a play adds a line and leaves the rest of the file as it was.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"a\",100]\n[\"b\",200]\n[\"c\",300]\n");
        // A line cut short (the app was killed part-way through writing it) is passed over.
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"[\"d\",4").unwrap();
        PlayLog::load(&old, &[]);
        assert_eq!(shared(), [("a".to_string(), 100), ("b".to_string(), 200), ("c".to_string(), 300)]);
        let _ = std::fs::remove_file(&path);

        // Plays from another device are taken in once each, oldest first.
        let mut log = PlayLog::load(&old, &[("c".to_string(), 300)]);
        let elsewhere = [("z".to_string(), 350), ("c".to_string(), 300), ("y".to_string(), 320), ("z".to_string(), 350)];
        let from = [("y".to_string(), 320, "playlist:p".to_string())];
        assert_eq!(log.keep(&elsewhere, &from).unwrap(), 2);
        assert_eq!(log.keep(&elsewhere, &from).unwrap(), 0);
        assert_eq!(shared(), [("c".to_string(), 300), ("y".to_string(), 320), ("z".to_string(), 350)]);
        // A play made from a playlist says so, here and after the log is read again.
        log.add("w", 400, "playlist:q");
        let mut noted = sources();
        noted.sort();
        assert_eq!(noted, [("w".to_string(), 400, "playlist:q".to_string()), ("y".to_string(), 320, "playlist:p".to_string())]);
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("[\"y\",320,\"playlist:p\"]\n[\"z\",350]\n[\"w\",400,\"playlist:q\"]\n"));
        PlayLog::load(&old, &[]);
        assert_eq!(sources().len(), 2);
        assert_eq!(shared().len(), 4);
        let _ = std::fs::remove_file(&path);

        // Plays that can't be written to the file are not counted as kept: the log is as it
        // was, here and on the disk. (A folder stands where the file should be.)
        let before = shared();
        let mut blocked = PlayLog { path: std::env::temp_dir(), lines: before.len(), unwritten: Vec::new(), aside: false, whole: true, unread: false, restored: 0 };
        assert!(blocked.keep(&[("never".to_string(), 500)], &[]).is_err());
        assert_eq!(shared(), before);
        // With nothing new there is nothing to write, and nothing to fail.
        assert_eq!(blocked.keep(&before, &[]).unwrap(), 0);

        // A write that failed part-way left the beginning of a line (the disk filled). The
        // next write does not go straight after it: joined to it, the play would be counted
        // as kept and be in a line nothing can read.
        let _ = std::fs::remove_file(&path);
        let mut log = PlayLog::load(&old, &[("a".to_string(), 100)]);
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"[\"cut-sh").unwrap();
        assert_eq!(log.keep(&[("b".to_string(), 200)], &[]).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"a\",100]\n[\"b\",200]\n");
        PlayLog::load(&old, &[]);
        assert_eq!(shared(), [("a".to_string(), 100), ("b".to_string(), 200)]);
        // A play made here while the file can't be written is counted, but nothing is said
        // to be kept until it has been: then it is in the file, once, with the others.
        let mut log = PlayLog::load(&old, &[]);
        let file = std::mem::replace(&mut log.path, std::env::temp_dir());
        log.add("c", 300, "playlist:q");
        assert_eq!(shared().len(), 3);
        assert!(log.keep(&[], &[]).is_err() && log.keep(&[("d".to_string(), 400)], &[]).is_err());
        log.path = file;
        assert_eq!(log.keep(&[("d".to_string(), 400)], &[]).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"a\",100]\n[\"b\",200]\n[\"c\",300,\"playlist:q\"]\n[\"d\",400]\n");
        // A play already in the log is given a source that another device tells of later,
        // in the file too; it is not counted again.
        assert_eq!(log.keep(&[("b".to_string(), 200)], &[("b".to_string(), 200, "playlist:p".to_string())]).unwrap(), 0);
        assert!(std::fs::read_to_string(&path).unwrap().contains("[\"b\",200,\"playlist:p\"]\n"));
        PlayLog::load(&old, &[]);
        assert_eq!((shared().len(), sources().len()), (4, 2));
        // The newest are offered by when they were played, with their sources.
        let (offered, from) = newest();
        assert_eq!((offered[0].clone(), offered.len(), from.len()), (("d".to_string(), 400), 4, 2));
        let _ = std::fs::remove_file(&path);

        // No log at all: it starts from what Recently played has, which is newest first.
        PlayLog::load(&old, &[("y".to_string(), 20), ("x".to_string(), 10)]);
        assert_eq!(shared(), [("x".to_string(), 10), ("y".to_string(), 20)]);
        let _ = std::fs::remove_file(&path);
    }

    /// Runs `load_by` and has the app "killed" where `steps` panic: what was done up to
    /// there stays on the disk, and nothing after it is done. Says whether it was killed.
    fn killed(old: &Path, steps: crate::config::Steps) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| PlayLog::load_by(old, &[play("z", 50)], &steps))).is_err()
    }

    const LIST: &str = r#"[["a",100],["b",200],["c",300]]"#;
    const LOGGED: &str = "[\"a\",100]\n[\"b\",200]\n[\"c\",300]\n";

    #[test]
    fn a_log_cut_short_by_a_kill_while_it_is_started_is_never_at_its_name() {
        use crate::config::{ON_DISK, Steps};
        let _turn = turn();
        let (old, path) = fresh("kill");
        let beside = crate::config::beside(&path);
        std::fs::write(&old, LIST).unwrap();
        // Killed the moment the file was made, empty, and again when it was written and
        // made sure of and not yet named: there is no log, and the list it is started from
        // is still there.
        let made_then_killed = Steps {
            create: |path| {
                let _empty = (ON_DISK.create)(path)?;
                panic!("killed")
            },
            ..ON_DISK
        };
        assert!(killed(&old, made_then_killed));
        assert!(!path.exists(), "an empty log at its name");
        let written_then_killed = Steps { place: |_, _| panic!("killed"), ..ON_DISK };
        assert!(killed(&old, written_then_killed));
        assert!(!path.exists(), "a log at its name before it was given it");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), LIST);
        // What a kill left beside it, half written, is not the log: the next start makes
        // the log whole, from the list, and clears it.
        std::fs::write(&beside, "[\"a\",100]\n[\"b\",2").unwrap();
        let log = PlayLog::load(&old, &[play("z", 50)]);
        assert_eq!(shared(), [play("a", 100), play("b", 200), play("c", 300)]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), LOGGED);
        assert!(!old.exists() && !beside.exists() && log.note().is_none());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_log_that_cannot_be_made_sure_of_or_named_is_not_started_and_nothing_is_lost() {
        use crate::config::{ON_DISK, Steps};
        let _turn = turn();
        let (old, path) = fresh("fail");
        fn fails() -> std::io::Result<()> {
            Err(std::io::Error::other("the disk said no"))
        }
        std::fs::write(&old, LIST).unwrap();
        for steps in [Steps { flush: |_| fails(), ..ON_DISK }, Steps { place: |_, _| fails(), ..ON_DISK }] {
            let mut log = PlayLog::load_by(&old, &[play("z", 50)], &steps);
            assert!(!path.exists() && !crate::config::beside(&path).exists());
            // What is played meanwhile is kept beside it, as for a log that can't be read.
            log.add("d", 400, "");
            assert_eq!(std::fs::read_to_string(&old).unwrap(), LIST);
        }
        // The next start that can make it starts it from the list, and joins what was
        // played meanwhile (once).
        PlayLog::load(&old, &[play("z", 50)]);
        assert_eq!(shared(), [play("a", 100), play("b", 200), play("c", 300), play("d", 400)]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{LOGGED}[\"d\",400]\n"));
        assert!(!old.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_log_that_0_40_10_left_empty_or_cut_short_gets_back_what_it_was_started_from() {
        let _turn = turn();
        let (old, path) = fresh("left");
        // Killed while it started the log from the list kept before 0.26: the log is empty,
        // or has the first of its plays, and the list is still there (it goes only once
        // the log has it). And played since, with the log as it was left.
        for left in ["", "[\"a\",100]\n[\"b\",2", "[\"a\",100]\n[\"d\",400]\n"] {
            std::fs::write(&old, LIST).unwrap();
            std::fs::write(&path, left).unwrap();
            let log = PlayLog::load(&old, &[play("z", 50)]);
            let mut want = vec![play("a", 100), play("b", 200), play("c", 300)];
            if left.contains("400") {
                want.push(play("d", 400));
            }
            assert_eq!(shared(), want, "from {left:?}");
            // In the file, once each, and in order; the list has gone; and the person is told.
            let again = PlayLog::load(&old, &[play("z", 50)]);
            assert_eq!(shared(), want);
            assert!(!old.exists());
            assert!(log.note().is_some_and(|note| note.contains("put back")) && again.note().is_none());
        }
        // An empty log with no list beside it can't be told from one started with nothing
        // to start it from: it is left as it is, and nothing is said.
        std::fs::write(&path, "").unwrap();
        let log = PlayLog::load(&old, &[play("z", 50)]);
        assert!(shared().is_empty() && log.note().is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
