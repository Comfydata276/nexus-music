//! When songs were added to playlists, as devices tell each other (see `sync`).
//!
//! The server doesn't record this, so each device keeps notes and shares the dated ones.
//! This is the shape of a note and the rule for combining two devices' notes; keeping them
//! on a device is each app's own business.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
pub const KEEP_REMOVED: i64 = 90 * 24 * 3600;

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
