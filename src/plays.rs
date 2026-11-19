//! Every play, with when it happened, kept on this PC.
//!
//! The server counts how often each song has been played but not when, and Recently played
//! only goes back a hundred songs. Neither can say what was played most this month. This
//! log can, from the day it starts: one line per play that counted (the same rule as for
//! reporting plays: half the song, or four minutes).
//!
//! It is a file that is only ever added to, a line at a time, so noting a play costs the
//! same however long the log has grown.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// More than anyone plays in ten years; the oldest go first after that.
const LIMIT: usize = 200_000;
/// How far past the limit the file may grow before the oldest lines are taken out of it.
const SLACK: usize = 5_000;

pub struct PlayLog {
    /// The log: a song id and when it was played (seconds since 1970) on each line, oldest
    /// first.
    path: PathBuf,
    lines: usize,
}

/// The plays, for the threads that build pages.
static SHARED: RwLock<Vec<(String, i64)>> = RwLock::new(Vec::new());

pub fn shared() -> Vec<(String, i64)> {
    SHARED.read().map(|plays| plays.clone()).unwrap_or_default()
}

fn line(song: &str, at: i64) -> String {
    format!("{}\n", serde_json::to_string(&(song, at)).unwrap_or_default())
}

impl PlayLog {
    /// Reads the log. `old` is where versions before 0.26 kept it, as one list rewritten on
    /// every play; that's carried over once. Failing that it's started off with `recent`:
    /// what Recently played remembers, so the log isn't empty on day one.
    pub fn load(old: &Path, recent: &[(String, i64)]) -> PlayLog {
        let path = old.with_extension("log");
        let mut log = PlayLog { path, lines: 0 };
        let plays: Vec<(String, i64)> = match std::fs::read_to_string(&log.path) {
            // A line cut short by the app being killed is passed over.
            Ok(text) => text.lines().filter_map(|line| serde_json::from_str(line).ok()).collect(),
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
        let text: String = plays.iter().map(|(song, at)| line(song, *at)).collect();
        crate::config::write_atomically(&self.path, text.as_bytes());
    }

    /// Takes the oldest plays out once the log is well past its limit.
    fn trim(&mut self) {
        if self.lines <= LIMIT + SLACK {
            return;
        }
        let Ok(mut shared) = SHARED.write() else { return };
        let extra = shared.len().saturating_sub(LIMIT);
        shared.drain(..extra);
        self.rewrite(&shared);
        self.lines = shared.len();
    }

    /// Notes a play.
    pub fn add(&mut self, song: &str, at: i64) {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = file.write_all(line(song, at).as_bytes());
        }
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
        log.add("c", 300);
        // Adding a play adds a line and leaves the rest of the file as it was.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[\"a\",100]\n[\"b\",200]\n[\"c\",300]\n");
        // A line cut short (the app was killed part-way through writing it) is passed over.
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"[\"d\",4").unwrap();
        PlayLog::load(&old, &[]);
        assert_eq!(shared(), [("a".to_string(), 100), ("b".to_string(), 200), ("c".to_string(), 300)]);
        let _ = std::fs::remove_file(&path);

        // No log at all: it starts from what Recently played has, which is newest first.
        PlayLog::load(&old, &[("y".to_string(), 20), ("x".to_string(), 10)]);
        assert_eq!(shared(), [("x".to_string(), 10), ("y".to_string(), 20)]);
        let _ = std::fs::remove_file(&path);
    }
}
