//! Changing a playlist that something else may have changed too.
//!
//! The page shows a copy of a playlist as it was when the page loaded. Another device, or
//! another app, can add or remove songs after that. So a change made on the page is never
//! sent as "song number 7" or as the page's whole list: it's described by the songs
//! themselves, and carried out on the playlist as the server has it at that moment.

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

/// A playlist as a spreadsheet file: one line per song, with the column names Spotify
/// exports use, so it can be read back by this app's import or by other tools.
pub fn csv(songs: &[crate::subsonic::Song], added: &[i64]) -> String {
    let cell = |text: &str| format!("\"{}\"", text.replace('"', "\"\""));
    let mut out = String::from("Track Name,Artist Name(s),Album Name,Duration (ms),Added At\r\n");
    for (index, song) in songs.iter().enumerate() {
        let when = added
            .get(index)
            .filter(|at| **at > 0)
            .and_then(|at| chrono::DateTime::from_timestamp(*at, 0))
            .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "{},{},{},{},{}\r\n",
            cell(&song.title),
            cell(&song.artist_label()),
            cell(&song.album),
            song.duration as u64 * 1000,
            when
        ));
    }
    out
}

/// A name that's safe to give a file.
pub fn file_name(name: &str) -> String {
    let safe: String = name.chars().map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let safe = safe.trim().trim_end_matches('.').to_string();
    if safe.is_empty() { "Playlist".into() } else { safe }
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
    fn writes_a_playlist_as_a_spreadsheet() {
        use crate::subsonic::Song;
        let song = Song { title: "Say \"Hello\", Wave".into(), artist: "Them".into(), album: "One, Two".into(), duration: 185, ..Default::default() };
        let text = csv(&[song], &[1_700_000_000]);
        assert_eq!(
            text,
            "Track Name,Artist Name(s),Album Name,Duration (ms),Added At\r\n\"Say \"\"Hello\"\", Wave\",\"Them\",\"One, Two\",185000,2023-11-14T22:13:20Z\r\n"
        );
        assert_eq!(file_name("AC/DC: Best?"), "AC_DC_ Best_");
        assert_eq!(file_name(" .. "), "Playlist");
    }
}
