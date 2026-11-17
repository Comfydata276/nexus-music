//! When songs were added to playlists.
//!
//! The server doesn't record this, so the app keeps its own note. The first time it sees a
//! playlist, the songs already in it have no known date. From then on, a song that turns up
//! in the playlist is dated to when the app added it, or first saw it there. Dated notes are
//! shared with the app on other devices (see `sync`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::subsonic::Song;

/// What's known about one song in one playlist.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Note {
    /// One time per copy of the song in the playlist, oldest first, in seconds since 1970.
    /// 0 means "before the app was keeping track". Empty once the song has been removed.
    pub times: Vec<i64>,
    /// When this note last changed. 0 for a note that only says "it was already there",
    /// which isn't worth sharing.
    pub at: i64,
}

pub type Notes = HashMap<String, HashMap<String, Note>>;

/// A removed song's note is kept this long, so other devices hear that it went.
const KEEP_REMOVED: i64 = 90 * 24 * 3600;

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    lists: Notes,
    seen: HashSet<String>,
}

#[derive(Default)]
pub struct Added {
    path: PathBuf,
    /// Playlist id, then song id.
    lists: Notes,
    /// Playlists this device has looked at. Songs found in one on the first look can't be
    /// dated; songs that appear in it later can.
    seen: HashSet<String>,
}

impl Added {
    pub fn load(path: PathBuf) -> Added {
        let bytes = fs::read(&path).unwrap_or_default();
        let stored: Stored = match serde_json::from_slice::<Stored>(&bytes) {
            Ok(stored) if !stored.lists.is_empty() || !stored.seen.is_empty() => stored,
            // The file as written before notes were shared: just the times.
            _ => match serde_json::from_slice::<HashMap<String, HashMap<String, Vec<i64>>>>(&bytes) {
                Ok(old) => Stored {
                    seen: old.keys().cloned().collect(),
                    lists: old
                        .into_iter()
                        .map(|(playlist, songs)| {
                            let notes = songs
                                .into_iter()
                                .map(|(id, times)| {
                                    let at = times.iter().copied().max().unwrap_or(0);
                                    (id, Note { times, at })
                                })
                                .collect();
                            (playlist, notes)
                        })
                        .collect(),
                },
                Err(_) => Stored::default(),
            },
        };
        Added { path, lists: stored.lists, seen: stored.seen }
    }

    fn save(&self) {
        // Borrowed fields can't go straight into `Stored`, so write the same shape by hand.
        let stored = serde_json::json!({ "lists": self.lists, "seen": self.seen });
        if let Ok(json) = serde_json::to_vec(&stored) {
            let temp = self.path.with_extension("tmp");
            if fs::write(&temp, json).is_ok() {
                let _ = fs::rename(&temp, &self.path);
            }
        }
    }

    /// Brings the notes for a playlist in step with the songs now in it, as just read from
    /// the server. Songs it didn't have before are dated `now`, unless this is the first
    /// look at the playlist. Returns whether anything worth sharing changed.
    pub fn observe(&mut self, playlist: &str, songs: &[Song], now: i64) -> bool {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for song in songs {
            *counts.entry(song.id.as_str()).or_default() += 1;
        }
        let first_look = self.seen.insert(playlist.to_string());
        let notes = self.lists.entry(playlist.to_string()).or_default();
        let mut changed = first_look;
        let mut shared = false;
        // Songs that have gone: a dated note stays, emptied, so other devices hear of it.
        notes.retain(|id, note| {
            if counts.contains_key(id.as_str()) {
                return true;
            }
            changed = true;
            if note.at == 0 {
                return false;
            }
            if !note.times.is_empty() {
                note.times.clear();
                note.at = now;
                shared = true;
            }
            now - note.at < KEEP_REMOVED
        });
        for (id, count) in counts {
            let known = notes.contains_key(id);
            let note = notes.entry(id.to_string()).or_default();
            if note.times.len() == count {
                continue;
            }
            changed = true;
            if first_look {
                // Copies nothing is known about go in front, as the oldest.
                while note.times.len() < count {
                    note.times.insert(0, 0);
                }
                note.times.truncate(count);
            } else {
                note.times.resize(count, now);
            }
            if known || !first_look {
                note.at = now;
                shared = true;
            }
        }
        if changed {
            self.save();
        }
        shared
    }

    /// Notes songs the app has just added to a playlist.
    pub fn record(&mut self, playlist: &str, ids: &[String], now: i64) {
        let notes = self.lists.entry(playlist.to_string()).or_default();
        for id in ids {
            let note = notes.entry(id.clone()).or_default();
            note.times.push(now);
            note.at = now;
        }
        self.save();
    }

    /// Notes songs added to a playlist at known times, as when a playlist is brought over
    /// from somewhere that kept them. `fresh` says the app has just made the playlist, with
    /// nothing else in it.
    pub fn record_at(&mut self, playlist: &str, songs: &[(String, i64)], fresh: bool, now: i64) {
        if fresh {
            self.lists.remove(playlist);
            self.seen.insert(playlist.to_string());
        }
        let notes = self.lists.entry(playlist.to_string()).or_default();
        for (id, at) in songs {
            let note = notes.entry(id.clone()).or_default();
            note.times.push(if *at > 0 { *at } else { now });
            note.times.sort_unstable();
            note.at = now;
        }
        self.save();
    }

    /// Notes a playlist the app has just created with these songs in it.
    pub fn create(&mut self, playlist: &str, ids: &[String], now: i64) {
        self.lists.remove(playlist);
        self.seen.insert(playlist.to_string());
        self.record(playlist, ids, now);
    }

    pub fn forget(&mut self, playlist: &str) {
        let known = self.seen.remove(playlist);
        if self.lists.remove(playlist).is_some() || known {
            self.save();
        }
    }

    /// The time each of these songs was added to the playlist, in the same order; 0 where
    /// it isn't known.
    pub fn dates(&self, playlist: &str, songs: &[Song]) -> Vec<i64> {
        let notes = self.lists.get(playlist);
        let mut seen: HashMap<&str, usize> = HashMap::new();
        songs
            .iter()
            .map(|song| {
                let copy = seen.entry(song.id.as_str()).or_default();
                *copy += 1;
                notes
                    .and_then(|n| n.get(&song.id))
                    .and_then(|note| note.times.get(*copy - 1))
                    .copied()
                    .unwrap_or(0)
            })
            .collect()
    }

    /// The notes worth sharing with other devices: the dated ones, for playlists that still
    /// exist (when `playlists` lists them).
    pub fn shared(&self, playlists: Option<&HashSet<&str>>) -> Notes {
        self.lists
            .iter()
            .filter(|(playlist, _)| playlists.is_none_or(|known| known.contains(playlist.as_str())))
            .map(|(playlist, notes)| {
                let dated = notes.iter().filter(|(_, note)| note.at > 0).map(|(id, note)| (id.clone(), note.clone()));
                (playlist.clone(), dated.collect::<HashMap<_, _>>())
            })
            .filter(|(_, notes)| !notes.is_empty())
            .collect()
    }

    /// Takes in notes from other devices: for each song, the newer note wins. Returns
    /// whether anything changed here.
    pub fn merge(&mut self, theirs: &Notes) -> bool {
        let mut changed = false;
        for (playlist, notes) in theirs {
            let mine = self.lists.entry(playlist.clone()).or_default();
            for (id, note) in notes {
                if mine.get(id).is_none_or(|have| note.at > have.at) {
                    mine.insert(id.clone(), note.clone());
                    changed = true;
                }
            }
        }
        if changed {
            self.save();
        }
        changed
    }
}

/// Combines two sets of shared notes: for each song, the newer one.
pub fn merge_notes(a: &Notes, b: &Notes, now: i64) -> Notes {
    let mut out = a.clone();
    for (playlist, notes) in b {
        let mine = out.entry(playlist.clone()).or_default();
        for (id, note) in notes {
            if mine.get(id).is_none_or(|have| note.at > have.at) {
                mine.insert(id.clone(), note.clone());
            }
        }
    }
    // Word that a song was removed has had long enough to get round.
    for notes in out.values_mut() {
        notes.retain(|_, note| !note.times.is_empty() || now - note.at < KEEP_REMOVED);
    }
    out.retain(|_, notes| !notes.is_empty());
    out
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-added-{name}-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir.join("added.json")
    }

    #[test]
    fn dates_songs_added_after_the_first_look() {
        let path = scratch("dates");
        let mut added = Added::load(path.clone());
        // Songs already there on the first look have no date, and nothing to share.
        assert!(!added.observe("p", &songs(&["a", "b"]), 100));
        assert_eq!(added.dates("p", &songs(&["a", "b"])), [0, 0]);
        assert!(added.shared(None).is_empty());
        // One that turns up later is dated to when it was seen; a second copy gets its own.
        assert!(added.observe("p", &songs(&["a", "b", "c", "a"]), 200));
        assert_eq!(added.dates("p", &songs(&["a", "b", "c", "a"])), [0, 0, 200, 200]);
        // Songs the app adds itself are dated at once, and looking again changes nothing.
        added.record("p", &["d".to_string()], 300);
        assert!(!added.observe("p", &songs(&["a", "b", "c", "a", "d"]), 400));
        assert_eq!(added.dates("p", &songs(&["d", "c", "a", "b", "a"])), [300, 200, 0, 0, 200]);
        // Removed songs are forgotten, so adding one again dates it afresh.
        added.observe("p", &songs(&["a", "b"]), 500);
        added.observe("p", &songs(&["a", "b", "c"]), 600);
        assert_eq!(added.dates("p", &songs(&["a", "b", "c"])), [0, 0, 600]);
        // It survives a restart.
        let again = Added::load(path.clone());
        assert_eq!(again.dates("p", &songs(&["a", "b", "c"])), [0, 0, 600]);
        // A song added to a playlist not yet looked at keeps its date through the first look.
        added.record("q", &["a".to_string()], 700);
        added.observe("q", &songs(&["x", "a", "a"]), 800);
        assert_eq!(added.dates("q", &songs(&["x", "a", "a"])), [0, 0, 700]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn notes_travel_between_devices() {
        let (path_a, path_b) = (scratch("a"), scratch("b"));
        let (mut a, mut b) = (Added::load(path_a.clone()), Added::load(path_b.clone()));
        a.observe("p", &songs(&["x", "y"]), 100);
        a.record("p", &["z".to_string()], 200);
        // The other device hears of the dated song before it ever opens the playlist...
        assert!(b.merge(&a.shared(None)));
        assert!(!b.merge(&a.shared(None)));
        // ...so its first look keeps that date, and has nothing new to say.
        assert!(!b.observe("p", &songs(&["x", "y", "z"]), 300));
        assert_eq!(b.dates("p", &songs(&["x", "y", "z"])), [0, 0, 200]);
        // A removal seen on one device reaches the other, and a later re-add is dated anew.
        assert!(b.observe("p", &songs(&["x", "y"]), 400));
        assert!(a.merge(&b.shared(None)));
        assert!(a.observe("p", &songs(&["x", "y", "z"]), 500));
        assert_eq!(a.dates("p", &songs(&["x", "y", "z"])), [0, 0, 500]);
        // Notes for playlists that no longer exist aren't passed on.
        let known: HashSet<&str> = ["other"].into();
        assert!(a.shared(Some(&known)).is_empty());
        // Combining is the same whichever way round, and old removals drop out in time.
        let (notes_a, notes_b) = (a.shared(None), b.shared(None));
        assert_eq!(merge_notes(&notes_a, &notes_b, 600), merge_notes(&notes_b, &notes_a, 600));
        assert!(merge_notes(&b.shared(None), &Notes::new(), 400 + KEEP_REMOVED + 1).is_empty());
        for path in [path_a, path_b] {
            let _ = fs::remove_dir_all(path.parent().unwrap());
        }
    }

    #[test]
    fn reads_the_file_from_before_notes_were_shared() {
        let path = scratch("old");
        fs::write(&path, r#"{"p":{"a":[0],"b":[0,250]}}"#).unwrap();
        let mut added = Added::load(path.clone());
        assert_eq!(added.dates("p", &songs(&["a", "b", "b"])), [0, 0, 250]);
        // It counts as looked at, so a new song is dated.
        added.observe("p", &songs(&["a", "b", "b", "c"]), 300);
        assert_eq!(added.dates("p", &songs(&["c"])), [300]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reads_server_times() {
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), 86400);
        assert_eq!(parse_time("1970-01-02T00:00:00.123456789Z"), 86400);
        assert_eq!(parse_time(""), 0);
        assert_eq!(date_label(0), "");
    }
}
