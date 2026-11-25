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
    /// Reads the log. `old` is where versions before 0.26 kept it, as one list rewritten on
    /// every play; that's carried over once. Failing that it's started off with `recent`:
    /// what Recently played remembers, so the log isn't empty on day one.
    pub fn load(old: &Path, recent: &[(String, i64)]) -> PlayLog {
        let path = old.with_extension("log");
        let mut log = PlayLog { path, lines: 0, unwritten: Vec::new() };
        if let Ok(mut sources) = SOURCES.write() {
            *sources = None;
        }
        let plays: Vec<(String, i64)> = match std::fs::read_to_string(&log.path) {
            // The lines that read, read; and the last record ends where `ending` says.
            Ok(text) => text.as_bytes()[..ending(text.as_bytes()).0]
                .split(|byte| *byte == b'\n')
                .filter_map(read_line)
                .map(|(song, at, from)| {
                    note_source(&song, at, &from);
                    (song, at)
                })
                .collect(),
            Err(_) => {
                let carried = std::fs::read(old).ok().and_then(|bytes| serde_json::from_slice::<Vec<(String, i64)>>(&bytes).ok());
                let mut plays = carried.unwrap_or_else(|| recent.to_vec());
                plays.sort_by_key(|(_, at)| *at);
                log.rewrite(&plays);
                let _ = std::fs::remove_file(old);
                plays
            }
        };
        log.lines = plays.len();
        if let Ok(mut shared) = SHARED.write() {
            *shared = plays;
        }
        log.trim();
        log
    }

    fn rewrite(&self, plays: &[(String, i64)]) {
        let text: String = plays.iter().map(|(song, at)| line(song, *at, &source_of(song, *at))).collect();
        crate::config::write_atomically(&self.path, text.as_bytes());
    }

    /// Takes the oldest plays out once the log is well past its limit.
    fn trim(&mut self) {
        if self.lines <= LIMIT + SLACK {
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
        let text: String = self.unwritten.iter().map(|(song, at, from)| line(song, *at, from)).collect();
        append(&self.path, &text)?;
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
            append(&self.path, &text)?;
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
        if !late.is_empty() {
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
        let mut blocked = PlayLog { path: std::env::temp_dir(), lines: before.len(), unwritten: Vec::new() };
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
}
