//! Keeping devices in step: what is shared, and how two devices' copies are combined.
//!
//! Pins, each list's shuffle and sort choice, the theme, when songs were added to playlists,
//! Recently played and the latest plays live as text in the comment of one playlist the app
//! keeps for the purpose. Each item carries the time it last changed, and the newer one wins.
//! This is the layout of that text and the merge. Every app on the account has to do both
//! identically, or they undo each other's changes.
//!
//! `merge` combines whatever two records it is given and stamps the result with this
//! layout. It does not look at the layout number of either, and it is not the whole of an
//! exchange: what the comment is read as is `record`'s, and what a device may then keep,
//! take in and write is `step::settle`'s, which is what an app calls. Talking to the server,
//! and applying what comes back, is each app's own.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::notes::{Notes, merge_notes};

/// The playlist whose comment holds the shared data.
pub const PLAYLIST_NAME: &str = "Nexus Music sync";
/// The layout of the shared data. A copy written by a later layout is left alone.
pub const VERSION: u32 = 1;
/// Songs remembered for "Recently played".
pub const HISTORY_LENGTH: usize = 100;
/// How many of the latest plays are passed between devices for "Your stats". Each device
/// keeps every play it hears of; this is only how far back one that's been away can catch up.
pub const PLAYS_SHARED: usize = 500;
/// Stands in for "some time ago" on choices made before sharing existed, so they're passed
/// on but lose to anything chosen since.
pub const LONG_AGO: i64 = 1;

/// What's remembered about one list of songs (a playlist, an album, Liked songs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListPrefs {
    /// Play it shuffled.
    pub shuffle: bool,
    /// The order it's shown in (an index into `TRACK_SORTS`), and whether that's reversed.
    pub sort: usize,
    pub descending: bool,
    /// The order was chosen by the user. Until it is, the list opens in its usual order:
    /// for a playlist, the most recently added songs first.
    pub sort_chosen: bool,
    /// When these choices last changed, in seconds since 1970: between devices, the newer
    /// set wins.
    pub at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shared {
    /// Marks the text as this app's and says which layout it's in.
    pub nexus_music_sync: u32,
    pub pins: Vec<String>,
    pub pins_at: i64,
    pub theme: String,
    pub theme_at: i64,
    /// By route key, e.g. "playlist:abc".
    pub lists: HashMap<String, ListPrefs>,
    pub added: Notes,
    /// Song id and when it was played, newest first.
    pub history: Vec<(String, i64)>,
    /// The latest plays that counted, from every device, newest first: what "Your stats"
    /// for the last weeks and months is worked out from. (Unlike `history`, a song played
    /// twice running is here twice.)
    pub plays: Vec<(String, i64)>,
    /// For those of `plays` that were made from a playlist: the play, and that playlist's
    /// route key. (Apart from `plays` so that versions before 0.40 can still read that.)
    pub play_sources: Vec<(String, i64, String)>,
}

pub fn list_rank(prefs: &ListPrefs) -> (i64, bool, usize, bool, bool) {
    (prefs.at, prefs.shuffle, prefs.sort, prefs.descending, prefs.sort_chosen)
}

/// Puts plays from two devices into one list, newest first.
pub fn merge_history(a: &[(String, i64)], b: &[(String, i64)]) -> Vec<(String, i64)> {
    let mut all: Vec<(String, i64)> = a.iter().chain(b).cloned().collect();
    all.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    all.dedup();
    // The same song twice running is one stretch of listening.
    all.dedup_by(|later, earlier| later.0 == earlier.0);
    all.truncate(HISTORY_LENGTH);
    all
}

/// Puts the plays two devices know of into one list, newest first, each play once.
pub fn merge_plays(a: &[(String, i64)], b: &[(String, i64)]) -> Vec<(String, i64)> {
    let mut all: Vec<(String, i64)> = a.iter().chain(b).cloned().collect();
    all.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    all.dedup();
    all.truncate(PLAYS_SHARED);
    all
}

/// The plays a device offers from its log: the newest [`PLAYS_SHARED`] **by when they were
/// played**, newest first, each once. (Not the last so many lines of the log: plays taken in
/// from another device are written after ones made here that are newer, and a log read by
/// its end would offer old plays of theirs in place of newer ones of its own.)
pub fn newest_plays<'a>(log: impl Iterator<Item = &'a (String, i64)>) -> Vec<(String, i64)> {
    let mut all: Vec<&(String, i64)> = log.collect();
    all.sort_unstable_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    all.dedup();
    all.into_iter().take(PLAYS_SHARED).cloned().collect()
}

/// Where the plays being passed on were made from, as far as either device knows.
fn merge_sources(a: &Shared, b: &Shared, plays: &[(String, i64)]) -> Vec<(String, i64, String)> {
    let kept: std::collections::HashSet<(&str, i64)> = plays.iter().map(|(song, at)| (song.as_str(), *at)).collect();
    let mut all: Vec<(String, i64, String)> =
        a.play_sources.iter().chain(&b.play_sources).filter(|(song, at, _)| kept.contains(&(song.as_str(), *at))).cloned().collect();
    all.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)).then_with(|| x.2.cmp(&y.2)));
    all.dedup_by(|later, earlier| later.0 == earlier.0 && later.1 == earlier.1);
    all
}

/// Combines what two devices know. For each item the newer wins; equally new, the choice is
/// arbitrary but the same whichever way round they're given, so devices end up agreeing.
pub fn merge(a: &Shared, b: &Shared, now: i64) -> Shared {
    let pins_from_a = (a.pins_at, &a.pins) >= (b.pins_at, &b.pins);
    let theme_from_a = (a.theme_at, &a.theme) >= (b.theme_at, &b.theme);
    let mut lists = a.lists.clone();
    for (key, theirs) in &b.lists {
        if lists.get(key).is_none_or(|mine| list_rank(theirs) > list_rank(mine)) {
            lists.insert(key.clone(), *theirs);
        }
    }
    let plays = merge_plays(&a.plays, &b.plays);
    Shared {
        nexus_music_sync: VERSION,
        pins: if pins_from_a { a.pins.clone() } else { b.pins.clone() },
        pins_at: a.pins_at.max(b.pins_at),
        theme: if theme_from_a { a.theme.clone() } else { b.theme.clone() },
        theme_at: a.theme_at.max(b.theme_at),
        lists,
        added: merge_notes(&a.added, &b.added, now),
        history: merge_history(&a.history, &b.history),
        play_sources: merge_sources(a, b, &plays),
        plays,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::Note;

    fn history(entries: &[(&str, i64)]) -> Vec<(String, i64)> {
        entries.iter().map(|(id, at)| (id.to_string(), *at)).collect()
    }

    #[test]
    fn every_device_s_plays_are_counted_once() {
        let here = history(&[("a", 30), ("a", 20), ("b", 10)]);
        let there = history(&[("c", 40), ("a", 20)]);
        // Unlike Recently played, a song played twice is two plays; the same play known to
        // both devices is one.
        let merged = merge_plays(&here, &there);
        assert_eq!(merged, history(&[("c", 40), ("a", 30), ("a", 20), ("b", 10)]));
        assert_eq!(merge_plays(&there, &here), merged);
        // Only so many are passed around.
        let many: Vec<(String, i64)> = (0..PLAYS_SHARED as i64 + 50).map(|at| ("s".to_string(), at)).collect();
        let kept = merge_plays(&many, &[]);
        assert_eq!((kept.len(), kept[0].1), (PLAYS_SHARED, PLAYS_SHARED as i64 + 49));
    }

    #[test]
    fn the_plays_offered_are_the_newest_by_time_wherever_they_are_in_the_log() {
        // A long log of this device's own plays, and after them, as they were taken in, some
        // older ones from another device.
        let mut log: Vec<(String, i64)> = (0..PLAYS_SHARED as i64 + 100).map(|n| ("mine".to_string(), 10_000 + n)).collect();
        log.extend((0..50).map(|n| ("theirs".to_string(), 500 + n)));
        log.push(("mine".to_string(), 10_000 + PLAYS_SHARED as i64 + 99));
        let offered = newest_plays(log.iter());
        assert_eq!(offered.len(), PLAYS_SHARED);
        assert_eq!((offered[0].1, offered[PLAYS_SHARED - 1].1), (10_000 + PLAYS_SHARED as i64 + 99, 10_100));
        assert!(offered.iter().all(|(song, _)| song == "mine"));
        assert_eq!(newest_plays([].iter()), []);
    }

    #[test]
    fn plays_from_two_devices_interleave() {
        let here = history(&[("c", 30), ("a", 10)]);
        let there = history(&[("d", 40), ("b", 20), ("a", 10)]);
        let merged = merge_history(&here, &there);
        assert_eq!(merged, history(&[("d", 40), ("c", 30), ("b", 20), ("a", 10)]));
        assert_eq!(merge_history(&there, &here), merged);
        // The same song on both devices back to back is one entry.
        assert_eq!(merge_history(&history(&[("a", 12)]), &history(&[("a", 10)])), history(&[("a", 12)]));
    }

    #[test]
    fn the_newer_choice_wins_whichever_way_round() {
        let mut here = Shared { pins: vec!["p1".into()], pins_at: 100, theme: "Ember".into(), theme_at: 50, ..Default::default() };
        here.lists.insert("playlist:x".into(), ListPrefs { shuffle: true, at: 70, ..Default::default() });
        here.lists.insert("album:y".into(), ListPrefs { sort: 1, at: 10, ..Default::default() });
        here.added.entry("x".into()).or_default().insert("s".into(), Note { times: vec![5], at: 5 });
        let mut there = Shared { pins: vec!["p2".into(), "p3".into()], pins_at: 90, theme: "Ocean".into(), theme_at: 60, ..Default::default() };
        there.lists.insert("playlist:x".into(), ListPrefs { shuffle: false, at: 80, ..Default::default() });
        there.added.entry("x".into()).or_default().insert("s".into(), Note { times: vec![], at: 9 });

        let merged = merge(&here, &there, 100);
        assert_eq!(merged, merge(&there, &here, 100));
        assert_eq!(merged.pins, ["p1"]);
        assert_eq!(merged.theme, "Ocean");
        assert!(!merged.lists["playlist:x"].shuffle);
        assert_eq!(merged.lists["album:y"].sort, 1);
        assert!(merged.added["x"]["s"].times.is_empty());
        // Combining again with either side changes nothing: the devices have settled.
        assert_eq!(merge(&merged, &here, 100), merged);
        assert_eq!(merge(&merged, &there, 100), merged);
        // It survives being written out and read back.
        let text = serde_json::to_string(&merged).unwrap();
        assert_eq!(serde_json::from_str::<Shared>(&text).unwrap(), merged);
        // Text that isn't this app's reads as nothing shared yet.
        assert_eq!(serde_json::from_str::<Shared>("hello").unwrap_or_default(), Shared::default());
    }

    #[test]
    fn choices_as_old_as_each_other_still_settle() {
        let here = Shared { pins: vec!["a".into()], pins_at: LONG_AGO, ..Default::default() };
        let there = Shared { pins: vec!["b".into()], pins_at: LONG_AGO, ..Default::default() };
        assert_eq!(merge(&here, &there, 0).pins, merge(&there, &here, 0).pins);
    }
}
