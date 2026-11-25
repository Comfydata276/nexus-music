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
        let mut log = PlayLog { path, lines: 0 };
        if let Ok(mut sources) = SOURCES.write() {
            *sources = None;
        }
        let plays: Vec<(String, i64)> = match std::fs::read_to_string(&log.path) {
            // A line cut short by the app being killed is passed over.
            Ok(text) => text
                .lines()
                .filter_map(parse)
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

    /// Takes in plays made on other devices (see `sync`), leaving out the ones already here,
    /// and **makes sure they are in the file before saying so**. `sources` says which
    /// playlists some of them were made from. Returns how many were new; or, if they could
    /// not be written (the disk is full, the file can't be opened), why, and then none of
    /// them is counted here either.
    ///
    /// The record devices share holds only the latest plays. One that is about to fall out
    /// of it, and that this PC has not yet got, has to be safe here before this PC writes
    /// the record back without it: so nothing is written back on the strength of plays this
    /// PC could not keep.
    pub fn keep(&mut self, plays: &[(String, i64)], sources: &[(String, i64, String)]) -> std::io::Result<usize> {
        let known: std::collections::HashSet<(String, i64)> = match SHARED.read() {
            // Anything another device has to tell is recent, so the newest part is enough to
            // compare with.
            Ok(shared) => shared.iter().rev().take(5000).cloned().collect(),
            Err(_) => return Err(std::io::Error::other("the log of plays is in use")),
        };
        let mut new: Vec<&(String, i64)> = plays.iter().filter(|play| !known.contains(*play)).collect();
        new.sort_by_key(|(_, at)| *at);
        new.dedup();
        if new.is_empty() {
            return Ok(0);
        }
        let from = |song: &str, at: i64| sources.iter().find(|(other, when, _)| other == song && *when == at).map(|(_, _, from)| from.as_str()).unwrap_or("");
        let text: String = new.iter().map(|(song, at)| line(song, *at, from(song, *at))).collect();
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        file.write_all(text.as_bytes())?;
        file.sync_data()?;
        for (song, at) in &new {
            note_source(song, *at, from(song, *at));
        }
        if let Ok(mut shared) = SHARED.write() {
            shared.extend(new.iter().map(|play| (*play).clone()));
        }
        self.lines += new.len();
        self.trim();
        Ok(new.len())
    }

    /// Notes a play, and the playlist it was made from (its route key) if it was from one.
    pub fn add(&mut self, song: &str, at: i64, from: &str) {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = file.write_all(line(song, at, from).as_bytes());
        }
        note_source(song, at, from);
        if let Ok(mut shared) = SHARED.write() {
            shared.push((song.to_string(), at));
        }
        self.lines += 1;
        self.trim();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_plays_between_runs() {
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
        let blocked = PlayLog { path: std::env::temp_dir(), lines: before.len() };
        let mut blocked = blocked;
        assert!(blocked.keep(&[("never".to_string(), 500)], &[]).is_err());
        assert_eq!(shared(), before);
        // With nothing new there is nothing to write, and nothing to fail.
        assert_eq!(blocked.keep(&before, &[]).unwrap(), 0);

        // No log at all: it starts from what Recently played has, which is newest first.
        PlayLog::load(&old, &[("y".to_string(), 20), ("x".to_string(), 10)]);
        assert_eq!(shared(), [("x".to_string(), 10), ("y".to_string(), 20)]);
        let _ = std::fs::remove_file(&path);
    }
}
