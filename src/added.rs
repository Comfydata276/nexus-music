//! When songs were added to playlists.
//!
//! The server doesn't record this, so the app keeps its own note. The first time it sees a
//! playlist, the songs already in it have no known date. From then on, a song the person
//! adds here is dated to when it was added, and the dated notes are shared with the app on
//! other devices (see `sync`). A song that only turns up in the playlist (added on another
//! device, or in another app) is shown as first seen then, on this PC alone: what is merely
//! noticed is never dated for the other devices, who may know the real date.
//!
//! What a note means, and when one gets a date, is the library's (`nexus_core::notes`), the
//! same for every app. This is the file it is kept in.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use crate::subsonic::Song;

pub use nexus_core::notes::Notes;
use nexus_core::notes::{Kept, Note};
pub use nexus_core::time::parse_time;

#[derive(Default)]
pub struct Added {
    path: PathBuf,
    kept: Kept,
}

fn ids(songs: &[Song]) -> Vec<&str> {
    songs.iter().map(|song| song.id.as_str()).collect()
}

impl Added {
    pub fn load(path: PathBuf) -> Added {
        let bytes = fs::read(&path).unwrap_or_default();
        let kept: Kept = match serde_json::from_slice::<Kept>(&bytes) {
            Ok(kept) if !kept.lists.is_empty() || !kept.seen.is_empty() => kept,
            // The file as written before notes were shared: just the times, which were when
            // this PC first saw each song. They are kept for its own display, with no time
            // on the note: never shared.
            _ => match serde_json::from_slice::<HashMap<String, HashMap<String, Vec<i64>>>>(&bytes) {
                Ok(old) => Kept {
                    seen: old.keys().cloned().collect(),
                    lists: old
                        .into_iter()
                        .map(|(playlist, songs)| (playlist, songs.into_iter().map(|(id, times)| (id, Note { times, at: 0 })).collect()))
                        .collect(),
                },
                Err(_) => Kept::default(),
            },
        };
        Added { path, kept }
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_vec(&self.kept) {
            crate::config::write_atomically(&self.path, &json);
        }
    }

    /// Notes what a playlist holds, as just read from the server, for this PC's own "Date
    /// added". Nothing it notes is shared.
    pub fn notice(&mut self, playlist: &str, songs: &[Song], now: i64) {
        if self.kept.notice(playlist, &ids(songs), now) {
            self.save();
        }
    }

    /// Songs the person has just added to a playlist here, and the server has taken.
    pub fn record(&mut self, playlist: &str, ids: &[String], now: i64) {
        self.kept.added(playlist, ids, now);
        self.save();
    }

    /// A change the person has just made to a playlist here, and the server has taken:
    /// `before` is the playlist as the change was worked out from. Returns whether there is
    /// anything to tell the other devices.
    pub fn edited(&mut self, playlist: &str, before: &[String], edit: &crate::playlists::Edit, now: i64) -> bool {
        let shared = self.kept.edited(playlist, before, edit, now);
        self.save();
        shared
    }

    /// Notes songs added to a playlist at known times, as when a playlist is brought over
    /// from somewhere that kept them. `fresh` says the app has just made the playlist, with
    /// nothing else in it.
    pub fn record_at(&mut self, playlist: &str, songs: &[(String, i64)], fresh: bool, now: i64) {
        self.kept.added_at(playlist, songs, fresh, now);
        self.save();
    }

    /// Notes a playlist the app has just created with these songs in it.
    pub fn create(&mut self, playlist: &str, ids: &[String], now: i64) {
        self.kept.create(playlist, ids, now);
        self.save();
    }

    pub fn forget(&mut self, playlist: &str) {
        if self.kept.forget(playlist) {
            self.save();
        }
    }

    /// The time each of these songs was added to the playlist, in the same order; 0 where
    /// it isn't known.
    pub fn dates(&self, playlist: &str, songs: &[Song]) -> Vec<i64> {
        self.kept.dates(playlist, &ids(songs))
    }

    /// The notes worth sharing with other devices: the dated ones, for playlists that still
    /// exist (when `playlists` lists them).
    pub fn shared(&self, playlists: Option<&HashSet<&str>>) -> Notes {
        self.kept.shared(playlists)
    }

    /// Takes in notes from other devices: for each song, the newer note wins. Returns
    /// whether anything changed here.
    pub fn merge(&mut self, theirs: &Notes) -> bool {
        let changed = self.kept.take(theirs);
        if changed {
            self.save();
        }
        changed
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
    fn keeps_its_notes_between_runs() {
        let path = scratch("dates");
        let mut added = Added::load(path.clone());
        // Songs already there on the first look have no date, and nothing to share.
        added.notice("p", &songs(&["a", "b"]), 100);
        assert_eq!(added.dates("p", &songs(&["a", "b"])), [0, 0]);
        // One that turns up later is shown as first seen then, here; it is not shared.
        added.notice("p", &songs(&["a", "b", "c"]), 200);
        assert_eq!(added.dates("p", &songs(&["a", "b", "c"])), [0, 0, 200]);
        assert!(added.shared(None).is_empty());
        // Songs the person adds here are dated at once, and shared; so is one taken out.
        added.record("p", &["d".to_string()], 300);
        let list: Vec<String> = ["a", "b", "c", "d"].iter().map(|id| id.to_string()).collect();
        assert!(added.edited("p", &list, &crate::playlists::Edit::Remove(vec![crate::playlists::spot(&list, 3).unwrap()]), 400));
        assert_eq!(added.shared(None)["p"]["d"], Note { times: vec![], at: 400 });
        // It survives a restart.
        let mut again = Added::load(path.clone());
        assert_eq!(again.dates("p", &songs(&["a", "b", "c"])), [0, 0, 200]);
        assert_eq!(again.shared(None), added.shared(None));
        // Notes from another device are taken in, and kept.
        let theirs = Notes::from([("p".to_string(), HashMap::from([("c".to_string(), Note { times: vec![150], at: 150 })]))]);
        assert!(again.merge(&theirs) && !again.merge(&theirs));
        assert_eq!(Added::load(path.clone()).dates("p", &songs(&["a", "b", "c"])), [0, 0, 150]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reads_the_file_from_before_notes_were_shared() {
        let path = scratch("old");
        fs::write(&path, r#"{"p":{"a":[0],"b":[0,250]}}"#).unwrap();
        let mut added = Added::load(path.clone());
        assert_eq!(added.dates("p", &songs(&["a", "b", "b"])), [0, 0, 250]);
        // What that version dated was what it had noticed: nothing of it is shared.
        assert!(added.shared(None).is_empty());
        // It counts as looked at, so a new song is shown as seen now.
        added.notice("p", &songs(&["a", "b", "b", "c"]), 300);
        assert_eq!(added.dates("p", &songs(&["c"])), [300]);
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reads_the_file_a_version_that_dated_what_it_noticed_wrote() {
        // 0.40.4's file: a dated note (already shared with every device, whatever it was
        // dated by) and one from a first look. Neither is lost, and neither given a new time.
        let path = scratch("stamped");
        fs::write(&path, r#"{"lists":{"p":{"a":{"times":[0],"at":0},"b":{"times":[250],"at":250}}},"seen":["p"]}"#).unwrap();
        let mut added = Added::load(path.clone());
        assert_eq!(added.dates("p", &songs(&["a", "b"])), [0, 250]);
        assert_eq!(added.shared(None)["p"], HashMap::from([("b".to_string(), Note { times: vec![250], at: 250 })]));
        added.notice("p", &songs(&["a", "b"]), 900);
        assert_eq!(added.shared(None)["p"]["b"], Note { times: vec![250], at: 250 });
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_time_nobody_knows_has_no_label() {
        assert_eq!(date_label(0), "");
    }
}
