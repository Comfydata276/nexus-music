//! Changing a playlist that something else may have changed too.
//!
//! A page shows a copy of a playlist as it was when the page loaded. Another device, or
//! another app, can add or remove songs after that. So a change made on the page is never
//! described as "song number 7" or as the page's whole list: it's described by the songs
//! themselves, and worked out on the playlist as the server has it at that moment. This is
//! the describing and the working out; reading the playlist and sending the change is each
//! app's own.
//!
//! `apply` and `positions` work on whatever list they are given. They do not check that the
//! list is all the server holds: before sending a removal by position, or a whole new list,
//! an app has to ask `hides_songs` of the same read of the playlist, and send nothing if it
//! says yes (the Windows app's `playlists::carry_out` is that).

use crate::model::Playlist;

/// Whether the server holds songs in this playlist that its answer leaves out. Navidrome keeps
/// an entry whose song has gone missing from the library (its file gone, or a disk offline
/// during a scan) and still counts it when told to remove the song at a position, but leaves
/// it out of the playlist's own answer, and out of that answer's count. The list of playlists
/// still counts it. So a playlist whose count in that list is more than the songs its own
/// answer lists is hiding some; so is one the list doesn't hold at all.
pub fn hides_songs(playlist: &Playlist, all: &[Playlist]) -> bool {
    all.iter().find(|other| other.id == playlist.id).is_none_or(|listed| listed.song_count as usize > playlist.songs.len())
}

/// One song's place in a list where the same song can appear more than once: the song, and
/// which of its appearances this is (0 for the first).
#[derive(Debug, Clone, PartialEq)]
pub struct Spot {
    pub id: String,
    pub nth: usize,
}

/// A change to a playlist, described so that it still means the same thing if the playlist
/// has changed in the meantime.
#[derive(Debug, Clone, PartialEq)]
pub enum Edit {
    /// Take these out.
    Remove(Vec<Spot>),
    /// Put one song straight after another, or first.
    Move { song: Spot, after: Option<Spot> },
    /// Put songs in, each straight after another (or first), in the order given.
    Insert(Vec<(String, Option<Spot>)>),
}

/// The spot of the song at `index`.
pub fn spot(list: &[String], index: usize) -> Spot {
    let id = list[index].clone();
    let nth = list[..index].iter().filter(|other| **other == id).count();
    Spot { id, nth }
}

/// Where a spot is in a list, if that appearance of the song is there.
fn place(list: &[String], spot: &Spot) -> Option<usize> {
    list.iter().enumerate().filter(|(_, id)| **id == spot.id).nth(spot.nth).map(|(index, _)| index)
}

/// Where to put something that goes after `after`: straight after it, first if there's
/// nothing it goes after, and last if what it went after has gone.
fn following(list: &[String], after: &Option<Spot>) -> usize {
    match after {
        None => 0,
        Some(spot) => place(list, spot).map_or(list.len(), |index| index + 1),
    }
}

/// The positions of these songs in the list as it is now, lowest first. A song that's no
/// longer there (or whose second copy is no longer there) is left out.
pub fn positions(now: &[String], spots: &[Spot]) -> Vec<usize> {
    let mut found: Vec<usize> = spots.iter().filter_map(|spot| place(now, spot)).collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// Carries out an edit on the list as it is now.
pub fn apply(now: &[String], edit: &Edit) -> Vec<String> {
    let mut list = now.to_vec();
    match edit {
        Edit::Remove(spots) => {
            for index in positions(now, spots).into_iter().rev() {
                list.remove(index);
            }
        }
        Edit::Move { song, after } => {
            // A song that has been taken out elsewhere isn't put back by moving it.
            if let Some(from) = place(&list, song) {
                let id = list.remove(from);
                let to = following(&list, after);
                list.insert(to, id);
            }
        }
        Edit::Insert(songs) => {
            for (id, after) in songs {
                let to = following(&list, after);
                list.insert(to, id.clone());
            }
        }
    }
    list
}

/// The edit that moves the song at `from` to `to` in `list`.
pub fn moving(list: &[String], from: usize, to: usize) -> Edit {
    let song = spot(list, from);
    let mut without = list.to_vec();
    without.remove(from);
    let after = (to > 0).then(|| spot(&without, to - 1));
    Edit::Move { song, after }
}

/// The edit that puts back the songs that were at these places in `before` (lowest first).
pub fn restoring(before: &[String], places: &[usize]) -> Edit {
    Edit::Insert(places.iter().map(|&index| (before[index].clone(), (index > 0).then(|| spot(before, index - 1)))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn removes_the_songs_meant_from_a_playlist_that_changed() {
        let page = list("a b c d e");
        let remove = Edit::Remove(vec![spot(&page, 1), spot(&page, 3)]);
        // Nothing else has touched it.
        assert_eq!(apply(&page, &remove), list("a c e"));
        // Another device reordered it and added a song: the same two songs go, not whatever
        // now sits at positions 1 and 3.
        assert_eq!(apply(&list("e d c b a x"), &remove), list("e c a x"));
        // One of them was already taken out elsewhere.
        assert_eq!(apply(&list("a c d e"), &remove), list("a c e"));
        assert_eq!(positions(&list("x a b c d e"), &[spot(&page, 1), spot(&page, 3)]), [2, 4]);
    }

    #[test]
    fn tells_copies_of_the_same_song_apart() {
        let page = list("a b a c a");
        // The second "a".
        let remove = Edit::Remove(vec![spot(&page, 2)]);
        assert_eq!(apply(&page, &remove), list("a b c a"));
        // If only one copy is left on the server there is no second copy to take out.
        assert_eq!(apply(&list("b a c"), &remove), list("b a c"));
    }

    #[test]
    fn moves_a_song_to_follow_the_same_neighbour() {
        let page = list("a b c d e");
        // "d" dragged up to sit after "a".
        let edit = moving(&page, 3, 1);
        assert_eq!(apply(&page, &edit), list("a d b c e"));
        // Meanwhile another device added "x" and "y": they're kept, and "d" still follows "a".
        assert_eq!(apply(&list("x a b c d e y"), &edit), list("x a d b c e y"));
        // Moved to the top.
        assert_eq!(apply(&list("x a b c d e"), &moving(&page, 4, 0)), list("e x a b c d"));
        // Moved down.
        assert_eq!(apply(&page, &moving(&page, 0, 3)), list("b c d a e"));
        // The song was taken out elsewhere: moving it doesn't bring it back.
        assert_eq!(apply(&list("a b c e"), &edit), list("a b c e"));
        // The neighbour was taken out elsewhere: it goes to the end.
        assert_eq!(apply(&list("b c d e"), &edit), list("b c e d"));
    }

    #[test]
    fn puts_removed_songs_back_where_they_were() {
        let before = list("a b c d e");
        let removed = [1, 2, 4];
        let after = apply(&before, &Edit::Remove(removed.iter().map(|&at| spot(&before, at)).collect()));
        assert_eq!(after, list("a d"));
        let undo = restoring(&before, &removed);
        assert_eq!(apply(&after, &undo), before);
        // Something was added elsewhere before the undo: it stays, and the rest goes back
        // beside its old neighbours.
        assert_eq!(apply(&list("a x d"), &undo), list("a b c x d e"));
        // The first song of the list, put back first.
        assert_eq!(apply(&list("b"), &restoring(&list("a b"), &[0])), list("a b"));
    }

    #[test]
    fn a_playlist_hides_songs_when_the_list_of_playlists_counts_more_than_it_shows() {
        // As Navidrome 0.64.2 answered for the stored [X, A, B, C] with X's file gone: the
        // playlist's own answer lists three and counts three, the list of playlists counts four.
        let shown: Playlist = serde_json::from_str(
            r#"{"id":"p1","name":"exp1 read","songCount":3,"duration":155,"owner":"tester","readonly":false,
                "entry":[{"id":"a","title":"Side B"},{"id":"b","title":"Rust Thunder"},{"id":"c","title":"Under Engine"}]}"#,
        )
        .unwrap();
        let listed: Vec<Playlist> = serde_json::from_str(
            r#"[{"id":"p0","name":"Other","songCount":1},{"id":"p1","name":"exp1 read","songCount":4,"duration":215,"owner":"tester"}]"#,
        )
        .unwrap();
        assert!(hides_songs(&shown, &listed));
        // Once X is back, or an administrator has removed it, the two agree.
        let mut agreed = listed.clone();
        agreed[1].song_count = 3;
        assert!(!hides_songs(&shown, &agreed));
        // A song taken out elsewhere between the two reads makes the list count fewer: that's
        // not hiding, and the change goes ahead as any other would.
        agreed[1].song_count = 2;
        assert!(!hides_songs(&shown, &agreed));
        // Not in the list at all: nothing to go by, so it's taken to hide songs.
        assert!(hides_songs(&shown, &listed[..1]));
        assert!(hides_songs(&shown, &[]));
    }
}
