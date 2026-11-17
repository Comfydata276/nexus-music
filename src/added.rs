//! When songs were added to playlists.
//!
//! The server doesn't record this, so the app keeps its own note. The first time it sees a
//! playlist, the songs already in it have no known date. From then on, a song that turns up
//! in the playlist is dated to when the app added it, or first saw it there.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use crate::subsonic::Song;

#[derive(Default)]
pub struct Added {
    path: PathBuf,
    /// Playlist id, then song id, then one time per copy of the song in the playlist, oldest
    /// first. Times are seconds since 1970; 0 means "before the app was keeping track".
    lists: HashMap<String, HashMap<String, Vec<i64>>>,
}

impl Added {
    pub fn load(path: PathBuf) -> Added {
        let lists = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        Added { path, lists }
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_vec(&self.lists) {
            let temp = self.path.with_extension("tmp");
            if fs::write(&temp, json).is_ok() {
                let _ = fs::rename(&temp, &self.path);
            }
        }
    }

    /// Brings the note for a playlist in step with the songs now in it. Songs it didn't have
    /// before are dated `now`, unless this is the first look at the playlist.
    pub fn observe(&mut self, playlist: &str, songs: &[Song], now: i64) {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for song in songs {
            *counts.entry(song.id.as_str()).or_default() += 1;
        }
        let first_look = !self.lists.contains_key(playlist);
        let noted = self.lists.entry(playlist.to_string()).or_default();
        let mut changed = first_look;
        let before = noted.len();
        noted.retain(|id, _| counts.contains_key(id.as_str()));
        changed |= noted.len() != before;
        for (id, count) in counts {
            let times = noted.entry(id.to_string()).or_default();
            if times.len() != count {
                changed = true;
                times.resize(count, if first_look { 0 } else { now });
            }
        }
        if changed {
            self.save();
        }
    }

    /// Notes songs the app has just added to a playlist. Does nothing for a playlist that
    /// hasn't been looked at yet: there'd be no telling its older songs from these.
    pub fn record(&mut self, playlist: &str, ids: &[String], now: i64) {
        let Some(noted) = self.lists.get_mut(playlist) else { return };
        for id in ids {
            noted.entry(id.clone()).or_default().push(now);
        }
        self.save();
    }

    /// Notes a playlist the app has just created with these songs in it.
    pub fn create(&mut self, playlist: &str, ids: &[String], now: i64) {
        let mut noted: HashMap<String, Vec<i64>> = HashMap::new();
        for id in ids {
            noted.entry(id.clone()).or_default().push(now);
        }
        self.lists.insert(playlist.to_string(), noted);
        self.save();
    }

    pub fn forget(&mut self, playlist: &str) {
        if self.lists.remove(playlist).is_some() {
            self.save();
        }
    }

    /// The time each of these songs was added to the playlist, in the same order; 0 where
    /// it isn't known.
    pub fn dates(&self, playlist: &str, songs: &[Song]) -> Vec<i64> {
        let noted = self.lists.get(playlist);
        let mut seen: HashMap<&str, usize> = HashMap::new();
        songs
            .iter()
            .map(|song| {
                let copy = seen.entry(song.id.as_str()).or_default();
                *copy += 1;
                noted.and_then(|n| n.get(&song.id)).and_then(|times| times.get(*copy - 1)).copied().unwrap_or(0)
            })
            .collect()
    }
}

/// A date for the "Date added" column.
pub fn date_label(at: i64) -> String {
    use chrono::TimeZone;
    if at <= 0 {
        return String::new();
    }
    let Some(then) = chrono::Local.timestamp_opt(at, 0).single() else { return String::new() };
    match (chrono::Local::now().date_naive() - then.date_naive()).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => then.format("%-d %b %Y").to_string(),
    }
}

/// The start of the day a time falls on, here. Used to treat songs that joined the library
/// on the same day as equally old, so they keep the order the playlist has them in.
pub fn day_start(at: i64) -> i64 {
    use chrono::TimeZone;
    if at <= 0 {
        return 0;
    }
    chrono::Local
        .timestamp_opt(at, 0)
        .single()
        .and_then(|then| then.date_naive().and_hms_opt(0, 0, 0))
        .and_then(|midnight| chrono::Local.from_local_datetime(&midnight).earliest())
        .map(|midnight| midnight.timestamp())
        .unwrap_or(at)
}

/// Reads a time as the server writes it, e.g. "2024-03-09T10:15:00Z"; 0 if it isn't one.
pub fn parse_time(text: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(text).map(|time| time.timestamp()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn songs(ids: &[&str]) -> Vec<Song> {
        ids.iter().map(|id| Song { id: id.to_string(), ..Default::default() }).collect()
    }

    #[test]
    fn dates_songs_added_after_the_first_look() {
        let dir = std::env::temp_dir().join(format!("nexus-added-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let mut added = Added::load(dir.join("added.json"));
        // Songs already there on the first look have no date.
        added.observe("p", &songs(&["a", "b"]), 100);
        assert_eq!(added.dates("p", &songs(&["a", "b"])), [0, 0]);
        // One that turns up later is dated to when it was seen; a second copy gets its own.
        added.observe("p", &songs(&["a", "b", "c", "a"]), 200);
        assert_eq!(added.dates("p", &songs(&["a", "b", "c", "a"])), [0, 0, 200, 200]);
        // Songs the app adds itself are dated at once, and looking again changes nothing.
        added.record("p", &["d".to_string()], 300);
        added.observe("p", &songs(&["a", "b", "c", "a", "d"]), 400);
        assert_eq!(added.dates("p", &songs(&["d", "c", "a", "b", "a"])), [300, 200, 0, 0, 200]);
        // Removed songs are forgotten, so adding one again dates it afresh.
        added.observe("p", &songs(&["a", "b"]), 500);
        added.observe("p", &songs(&["a", "b", "c"]), 600);
        assert_eq!(added.dates("p", &songs(&["a", "b", "c"])), [0, 0, 600]);
        // It survives a restart.
        let again = Added::load(dir.join("added.json"));
        assert_eq!(again.dates("p", &songs(&["a", "b", "c"])), [0, 0, 600]);
        // A playlist never looked at isn't guessed about.
        added.record("q", &["a".to_string()], 700);
        assert_eq!(added.dates("q", &songs(&["a"])), [0]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reads_server_times() {
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), 86400);
        assert_eq!(parse_time("1970-01-02T00:00:00.123456789Z"), 86400);
        assert_eq!(parse_time(""), 0);
        assert_eq!(date_label(0), "");
    }
}
