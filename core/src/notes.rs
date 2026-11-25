//! When songs were added to playlists, as devices tell each other (see `sync`).
//!
//! The server doesn't record this, so each device keeps notes and shares the dated ones.
//! This is the shape of a note and the rule for combining two devices' notes; keeping them
//! on a device is each app's own business.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::playlists::{Edit, apply, positions};

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
        notes.retain(|_, note| !note.times.is_empty() || now.saturating_sub(note.at) < KEEP_REMOVED);
    }
    out.retain(|_, notes| !notes.is_empty());
    out
}

/// Takes in notes from other devices: for each song, the newer note wins. Returns whether
/// anything changed. (The same rule as [`merge_notes`], applied to what a device keeps; a
/// note the device only made for its own display, with no time on it, gives way to any
/// dated one.)
pub fn take(mine: &mut Notes, theirs: &Notes) -> bool {
    let mut changed = false;
    for (playlist, notes) in theirs {
        let here = mine.entry(playlist.clone()).or_default();
        for (id, note) in notes {
            if here.get(id).is_none_or(|have| note.at > have.at) {
                here.insert(id.clone(), note.clone());
                changed = true;
            }
        }
    }
    changed
}

/// What a device keeps about when songs were added to playlists.
///
/// Two kinds of note live here, told apart by the time on them. A **dated** note (`at` above
/// 0) says the person added or removed that song, on this device or another, and when: it
/// is shared, and between devices the newer wins. An **undated** one (`at` 0) is only what
/// this device has noticed in a playlist, kept so its own "Date added" column has something
/// to show: it is never shared, and gives way to any dated note.
///
/// **A date is put only on the device's own confirmed change** ([`Kept::added`],
/// [`Kept::changed`], [`Kept::create`]). What a read of the playlist happens to show
/// ([`Kept::notice`]) never puts a time on a note that is shared, and never alters one: a
/// device that looked late would otherwise put today on songs added long ago, or on what
/// another device added a moment before, and that newer note would replace the real date on
/// every device. Nor does a date this device only noticed ever go into a shared note, when
/// the person later adds another copy of the same song.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Kept {
    /// Playlist id, then song id.
    pub lists: Notes,
    /// Playlists this device has looked at. Songs found in one on the first look were there
    /// before it was keeping track; songs that appear in it later were not.
    pub seen: std::collections::HashSet<String>,
}

fn counts<'a>(ids: impl IntoIterator<Item = &'a str>) -> HashMap<&'a str, usize> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for id in ids {
        *counts.entry(id).or_default() += 1;
    }
    counts
}

/// A dated note's times made to stand for `copies` copies of the song: copies nothing is
/// known about go in front, as the oldest, with no time.
fn fitted(times: &[i64], copies: usize) -> Vec<i64> {
    let mut out = vec![0; copies.saturating_sub(times.len())];
    out.extend(times.iter().take(copies));
    out
}

impl Kept {
    /// Notes what a playlist holds, as just read from the server, **for this device's own
    /// display only**. Songs it didn't have before are remembered as first seen `now`
    /// (unless this is the first look at the playlist), in a note with no time on it. A
    /// dated note is never touched, except that word of a removal which has had long enough
    /// to get round is let go. Returns whether anything changed (to be saved); nothing it
    /// changes is ever shared.
    pub fn notice(&mut self, playlist: &str, ids: &[&str], now: i64) -> bool {
        let counts = counts(ids.iter().copied());
        let first_look = self.seen.insert(playlist.to_string());
        let notes = self.lists.entry(playlist.to_string()).or_default();
        let mut changed = first_look;
        notes.retain(|id, note| {
            let keep = if note.at == 0 { counts.contains_key(id.as_str()) } else { !note.times.is_empty() || now.saturating_sub(note.at) < KEEP_REMOVED };
            changed |= !keep;
            keep
        });
        for (id, count) in counts {
            let note = notes.entry(id.to_string()).or_default();
            if note.at != 0 || note.times.len() == count {
                continue;
            }
            changed = true;
            if first_look {
                while note.times.len() < count {
                    note.times.insert(0, 0);
                }
                note.times.truncate(count);
            } else {
                note.times.resize(count, now);
            }
        }
        if notes.is_empty() {
            self.lists.remove(playlist);
        }
        changed
    }

    /// The person added these songs to a playlist on this device, and the server has taken
    /// them: each new copy is dated `now`. Copies the device already knew of keep the times
    /// a dated note had for them; ones it had only noticed are given none (a time this
    /// device merely noticed is never shared).
    pub fn added(&mut self, playlist: &str, ids: &[String], now: i64) {
        let notes = self.lists.entry(playlist.to_string()).or_default();
        for id in ids {
            let note = notes.entry(id.clone()).or_default();
            if note.at == 0 {
                note.times = vec![0; note.times.len()];
            }
            note.times.push(now);
            note.at = now;
        }
    }

    /// The person changed a playlist on this device, and the server has taken the change:
    /// `before` is the playlist as the change was worked out from, `after` what that change
    /// made of it (worked out, not read back: a later read can show another device's change
    /// too, which is not this device's to date). Copies added are dated `now`; a song that
    /// lost copies, or went altogether, has its dated note say so, as of `now`. Returns
    /// whether anything worth sharing changed.
    pub fn changed(&mut self, playlist: &str, before: &[String], after: &[String], now: i64) -> bool {
        let (was, is) = (counts(before.iter().map(String::as_str)), counts(after.iter().map(String::as_str)));
        let notes = self.lists.entry(playlist.to_string()).or_default();
        let mut shared = false;
        for (id, &count) in &is {
            let had = was.get(id).copied().unwrap_or(0);
            if count > had {
                let note = notes.entry(id.to_string()).or_default();
                let known = if note.at == 0 { Vec::new() } else { std::mem::take(&mut note.times) };
                note.times = fitted(&known, had);
                note.times.resize(count, now);
                note.at = now;
                shared = true;
            }
        }
        for (id, &had) in &was {
            let count = is.get(id).copied().unwrap_or(0);
            if count >= had {
                continue;
            }
            match notes.get_mut(*id) {
                // Word of it is only worth passing on where there was a date to take back.
                Some(note) if note.at != 0 => {
                    note.times = fitted(&note.times, had);
                    note.times.truncate(count);
                    note.at = now;
                    shared = true;
                }
                Some(note) if count > 0 => note.times.truncate(count),
                Some(_) => {
                    notes.remove(*id);
                }
                None => {}
            }
        }
        if notes.is_empty() {
            self.lists.remove(playlist);
        }
        shared
    }

    /// The person changed a playlist on this device with `edit`, worked out on the playlist
    /// as it was `before`, and the server has taken it. As [`Kept::changed`], and where the
    /// change took copies out it is **those copies' times that go**: the first of two copies
    /// removed leaves the second's time, not the first's. (A note's times stand for the
    /// song's copies in the order the playlist has them, which is how [`Kept::dates`] shows
    /// them.) Returns whether anything worth sharing changed.
    pub fn edited(&mut self, playlist: &str, before: &[String], edit: &Edit, now: i64) -> bool {
        let Edit::Remove(spots) = edit else { return self.changed(playlist, before, &apply(before, edit), now) };
        let was = counts(before.iter().map(String::as_str));
        // Which copies of each song went: the song, and its copies' numbers, lowest first.
        let mut gone: HashMap<&str, Vec<usize>> = HashMap::new();
        for index in positions(before, spots) {
            let id = before[index].as_str();
            gone.entry(id).or_default().push(before[..index].iter().filter(|other| *other == id).count());
        }
        let notes = self.lists.entry(playlist.to_string()).or_default();
        let mut shared = false;
        for (id, copies) in gone {
            let Some(note) = notes.get_mut(id) else { continue };
            let mut times = fitted(&note.times, was[id]);
            for copy in copies.into_iter().rev() {
                times.remove(copy);
            }
            if note.at != 0 {
                // Word of it is only worth passing on where there was a date to take back.
                note.times = times;
                note.at = now;
                shared = true;
            } else if times.is_empty() {
                notes.remove(id);
            } else {
                note.times = times;
            }
        }
        if notes.is_empty() {
            self.lists.remove(playlist);
        }
        shared
    }

    /// The person made a playlist on this device with these songs in it.
    pub fn create(&mut self, playlist: &str, ids: &[String], now: i64) {
        self.lists.remove(playlist);
        self.seen.insert(playlist.to_string());
        self.added(playlist, ids, now);
    }

    /// Songs added to a playlist at known times, as when a playlist is brought over from
    /// somewhere that kept them. `fresh` says the app has just made the playlist, with
    /// nothing else in it.
    pub fn added_at(&mut self, playlist: &str, songs: &[(String, i64)], fresh: bool, now: i64) {
        if fresh {
            self.lists.remove(playlist);
            self.seen.insert(playlist.to_string());
        }
        let notes = self.lists.entry(playlist.to_string()).or_default();
        for (id, at) in songs {
            let note = notes.entry(id.clone()).or_default();
            if note.at == 0 {
                note.times = vec![0; note.times.len()];
            }
            note.times.push(if *at > 0 { *at } else { now });
            note.times.sort_unstable();
            note.at = now;
        }
    }

    /// The playlist has gone. Returns whether anything was known of it.
    pub fn forget(&mut self, playlist: &str) -> bool {
        let known = self.seen.remove(playlist);
        self.lists.remove(playlist).is_some() || known
    }

    /// The time each of these songs was added to the playlist, in the same order; 0 where it
    /// isn't known. Where a note has fewer times than the playlist has copies of the song,
    /// they are the newest copies': the earlier ones have no known time.
    pub fn dates(&self, playlist: &str, ids: &[&str]) -> Vec<i64> {
        let Some(notes) = self.lists.get(playlist) else { return vec![0; ids.len()] };
        let counts = counts(ids.iter().copied());
        let mut met: HashMap<&str, usize> = HashMap::new();
        ids.iter()
            .map(|id| {
                let copy = met.entry(id).or_default();
                *copy += 1;
                let Some(note) = notes.get(*id) else { return 0 };
                let unknown = counts[id].saturating_sub(note.times.len());
                if *copy <= unknown { 0 } else { note.times.get(*copy - 1 - unknown).copied().unwrap_or(0) }
            })
            .collect()
    }

    /// The notes worth sharing with other devices: the dated ones, for playlists that still
    /// exist (when `playlists` lists them).
    pub fn shared(&self, playlists: Option<&std::collections::HashSet<&str>>) -> Notes {
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

    /// Takes in notes from other devices: see [`take`].
    pub fn take(&mut self, theirs: &Notes) -> bool {
        take(&mut self.lists, theirs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn what_a_device_merely_notices_is_never_dated_for_the_others() {
        let mut kept = Kept::default();
        // Songs already there on the first look have no date, and nothing to share.
        kept.notice("p", &["a", "b"], 100);
        assert_eq!(kept.dates("p", &["a", "b"]), [0, 0]);
        // One that turns up later (another device added it, or another app) is shown as
        // first seen then, on this device; a second copy likewise. None of it is shared.
        assert!(kept.notice("p", &["a", "b", "c", "a"], 200));
        assert_eq!(kept.dates("p", &["a", "b", "c", "a"]), [0, 0, 200, 200]);
        assert!(kept.shared(None).is_empty());
        // Looking again changes nothing; a song noticed gone is forgotten.
        assert!(!kept.notice("p", &["a", "b", "c", "a"], 300));
        assert!(kept.notice("p", &["a", "b"], 400));
        assert_eq!(kept.dates("p", &["a", "b", "c"]), [0, 0, 0]);
        assert!(kept.shared(None).is_empty());
    }

    #[test]
    fn a_dated_note_is_not_altered_by_what_is_noticed() {
        let mut kept = Kept::default();
        // The real date of a song, as the device that added it passed it on.
        kept.take(&Notes::from([("p".to_string(), HashMap::from([("z".to_string(), Note { times: vec![150], at: 150 })]))]));
        let before = kept.shared(None);
        // A first look, a later look that finds a second copy of it, and one that finds it
        // gone: the note is as it came.
        kept.notice("p", &["x", "z"], 300);
        kept.notice("p", &["x", "z", "z"], 400);
        kept.notice("p", &["x"], 500);
        assert_eq!(kept.shared(None), before);
        // (With two copies and one time, the time is the newer copy's.)
        assert_eq!(kept.dates("p", &["z", "x", "z"]), [0, 0, 150]);
        // Word that a song went is let go once it has had long enough to get round.
        kept.take(&Notes::from([("p".to_string(), HashMap::from([("gone".to_string(), Note { times: vec![], at: 600 })]))]));
        kept.notice("p", &["x"], 600 + KEEP_REMOVED - 1);
        assert!(kept.lists["p"].contains_key("gone"));
        kept.notice("p", &["x"], 600 + KEEP_REMOVED);
        assert!(!kept.lists["p"].contains_key("gone"));
    }

    #[test]
    fn the_devices_own_additions_are_dated_and_only_those() {
        let mut kept = Kept::default();
        kept.notice("p", &["a", "b"], 100);
        // Added here, and taken by the server: dated at once.
        kept.added("p", &ids(&["d"]), 300);
        assert_eq!(kept.shared(None)["p"]["d"], Note { times: vec![300], at: 300 });
        // The read that follows shows it, and something another device added in between:
        // that one is noticed, for display, and not dated for anyone.
        kept.notice("p", &["a", "b", "y", "d"], 310);
        assert_eq!(kept.dates("p", &["a", "b", "y", "d"]), [0, 0, 310, 300]);
        assert_eq!(kept.shared(None)["p"].keys().collect::<Vec<_>>(), ["d"]);
        // A second copy of a song this device had only noticed: the new copy is dated, and
        // the time it noticed the first one at stays out of what is shared.
        kept.added("p", &ids(&["y"]), 400);
        assert_eq!(kept.shared(None)["p"]["y"], Note { times: vec![0, 400], at: 400 });
        // A second copy of one it had dated itself keeps that date.
        kept.added("p", &ids(&["d"]), 500);
        assert_eq!(kept.shared(None)["p"]["d"], Note { times: vec![300, 500], at: 500 });
        // A playlist made here: every song dated; whatever was known under that id is gone.
        kept.create("q", &ids(&["a", "a"]), 600);
        assert_eq!(kept.shared(None)["q"]["a"], Note { times: vec![600, 600], at: 600 });
        // A song added to a playlist not yet looked at keeps its date through the first look.
        kept.added("r", &ids(&["a"]), 700);
        kept.notice("r", &["x", "a", "a"], 800);
        assert_eq!(kept.dates("r", &["x", "a", "a"]), [0, 0, 700]);
    }

    #[test]
    fn a_change_made_here_dates_its_own_copies_and_nothing_the_playlist_gained_meanwhile() {
        let mut kept = Kept::default();
        kept.notice("p", &["a", "b"], 100);
        // The change was worked out from [a, b] and made it [a, x, b]: x is dated.
        assert!(kept.changed("p", &ids(&["a", "b"]), &ids(&["a", "x", "b"]), 200));
        assert_eq!(kept.shared(None)["p"], HashMap::from([("x".to_string(), Note { times: vec![200], at: 200 })]));
        // Moving songs about dates nothing.
        assert!(!kept.changed("p", &ids(&["a", "x", "b"]), &ids(&["b", "a", "x"]), 250));
        // Taking x out again says so, as of then; taking out one that never had a date says
        // nothing to anyone.
        assert!(kept.changed("p", &ids(&["b", "a", "x"]), &ids(&["b", "a"]), 300));
        assert_eq!(kept.shared(None)["p"]["x"], Note { times: vec![], at: 300 });
        assert!(!kept.changed("p", &ids(&["b", "a"]), &ids(&["b"]), 350));
        assert_eq!(kept.shared(None)["p"].len(), 1);
        // One of two dated copies taken out: the note keeps one time, and is newer.
        kept.added("p", &ids(&["d", "d"]), 400);
        assert!(kept.changed("p", &ids(&["b", "d", "d"]), &ids(&["b", "d"]), 450));
        assert_eq!(kept.shared(None)["p"]["d"], Note { times: vec![400], at: 450 });
    }

    #[test]
    fn taking_out_one_of_two_copies_drops_that_copys_date_not_the_others() {
        use crate::playlists::spot;
        let list = ids(&["x", "a", "x", "b"]);
        let two = || {
            let mut kept = Kept::default();
            // Two copies of x, added on different days; the playlist has them in that order.
            kept.take(&Notes::from([("p".to_string(), HashMap::from([("x".to_string(), Note { times: vec![100, 200], at: 200 })]))]));
            kept.notice("p", &["x", "a", "x", "b"], 250);
            kept
        };
        // The first copy taken out: the one that is left is the one added on the second day.
        let mut kept = two();
        assert!(kept.edited("p", &list, &Edit::Remove(vec![spot(&list, 0).unwrap()]), 300));
        assert_eq!(kept.shared(None)["p"]["x"], Note { times: vec![200], at: 300 });
        assert_eq!(kept.dates("p", &["a", "x", "b"]), [0, 200, 0]);
        // The second taken out: the first day's is left.
        let mut kept = two();
        assert!(kept.edited("p", &list, &Edit::Remove(vec![spot(&list, 2).unwrap()]), 300));
        assert_eq!(kept.shared(None)["p"]["x"], Note { times: vec![100], at: 300 });
        // Both, and one that was only ever noticed: x is told as gone; a says nothing.
        let mut kept = two();
        assert!(kept.edited("p", &list, &Edit::Remove(vec![spot(&list, 0).unwrap(), spot(&list, 1).unwrap(), spot(&list, 2).unwrap()]), 300));
        assert_eq!(kept.shared(None)["p"], HashMap::from([("x".to_string(), Note { times: vec![], at: 300 })]));
        assert_eq!(kept.dates("p", &["b"]), [0]);
        // A copy the note has no time for (it is one of three, and the note knows two):
        // the times that are left are still the later copies'.
        let three = ids(&["x", "x", "x"]);
        let mut kept = two();
        assert!(kept.edited("p", &three, &Edit::Remove(vec![spot(&three, 0).unwrap()]), 300));
        assert_eq!(kept.shared(None)["p"]["x"].times, [100, 200]);
        // Putting songs in and moving them go as before: what is put in is dated.
        let mut kept = two();
        assert!(kept.edited("p", &list, &Edit::Insert(vec![("new".to_string(), None)]), 400));
        assert_eq!(kept.shared(None)["p"]["new"], Note { times: vec![400], at: 400 });
        assert!(!kept.edited("p", &list, &crate::playlists::moving(&list, 0, 3), 500));
    }

    #[test]
    fn notes_travel_between_devices() {
        let (mut a, mut b) = (Kept::default(), Kept::default());
        a.notice("p", &["x", "y"], 100);
        a.added("p", &ids(&["z"]), 200);
        // The other device hears of the dated song before it ever opens the playlist...
        assert!(b.take(&a.shared(None)));
        assert!(!b.take(&a.shared(None)));
        // ...so its first look keeps that date, and has nothing new to say.
        b.notice("p", &["x", "y", "z"], 300);
        assert_eq!(b.dates("p", &["x", "y", "z"]), [0, 0, 200]);
        assert_eq!(b.shared(None), a.shared(None));
        // Taken out on one device, by the person: the other hears; put back, it is dated anew.
        assert!(b.changed("p", &ids(&["x", "y", "z"]), &ids(&["x", "y"]), 400));
        assert!(a.take(&b.shared(None)));
        assert!(a.changed("p", &ids(&["x", "y"]), &ids(&["x", "y", "z"]), 500));
        assert_eq!(a.dates("p", &["x", "y", "z"]), [0, 0, 500]);
        // Notes for playlists that no longer exist aren't passed on.
        let known: std::collections::HashSet<&str> = ["other"].into();
        assert!(a.shared(Some(&known)).is_empty());
        // Combining is the same whichever way round, and old removals drop out in time.
        let (notes_a, notes_b) = (a.shared(None), b.shared(None));
        assert_eq!(merge_notes(&notes_a, &notes_b, 600), merge_notes(&notes_b, &notes_a, 600));
        assert!(merge_notes(&b.shared(None), &Notes::new(), 400 + KEEP_REMOVED + 1).is_empty());
    }
}
