//! Every play, with when it happened, kept on this PC.
//!
//! The server counts how often each song has been played but not when, and Recently played
//! only goes back a hundred songs. Neither can say what was played most this month. This
//! log can, from the day it starts: one line per play that counted (the same rule as for
//! reporting plays: half the song, or four minutes).

use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// More than anyone plays in ten years; the oldest go first after that.
const LIMIT: usize = 200_000;

pub struct PlayLog {
    path: PathBuf,
    /// Song ids and when each was played (seconds since 1970), oldest first.
    plays: Vec<(String, i64)>,
}

/// A copy for the threads that build pages.
static SHARED: RwLock<Vec<(String, i64)>> = RwLock::new(Vec::new());

pub fn shared() -> Vec<(String, i64)> {
    SHARED.read().map(|plays| plays.clone()).unwrap_or_default()
}

impl PlayLog {
    /// Reads the log. The first time, it's started off with `recent`: what Recently played
    /// remembers, so the log isn't empty on day one.
    pub fn load(path: &Path, recent: &[(String, i64)]) -> PlayLog {
        let plays = match std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<Vec<(String, i64)>>(&bytes).ok()) {
            Some(plays) => plays,
            None => {
                let mut plays = recent.to_vec();
                plays.sort_by_key(|(_, at)| *at);
                plays
            }
        };
        let log = PlayLog { path: path.to_path_buf(), plays };
        log.share();
        log
    }

    fn share(&self) {
        if let Ok(mut shared) = SHARED.write() {
            *shared = self.plays.clone();
        }
    }

    /// Notes a play.
    pub fn add(&mut self, song: &str, at: i64) {
        self.plays.push((song.to_string(), at));
        if self.plays.len() > LIMIT {
            let extra = self.plays.len() - LIMIT;
            self.plays.drain(..extra);
        }
        if let Ok(json) = serde_json::to_vec(&self.plays) {
            let temp = self.path.with_extension("tmp");
            if std::fs::write(&temp, json).is_ok() {
                let _ = std::fs::rename(&temp, &self.path);
            }
        }
        self.share();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_plays_between_runs() {
        let path = std::env::temp_dir().join(format!("nexus-plays-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        // Started from what Recently played has, which is newest first.
        let mut log = PlayLog::load(&path, &[("b".to_string(), 200), ("a".to_string(), 100)]);
        log.add("c", 300);
        let again = PlayLog::load(&path, &[]);
        assert_eq!(again.plays, [("a".to_string(), 100), ("b".to_string(), 200), ("c".to_string(), 300)]);
        assert_eq!(shared().len(), 3);
        let _ = std::fs::remove_file(&path);
    }
}
