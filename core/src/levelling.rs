//! Evening out volume: the parts every app has to decide alike.
//!
//! So far, when a song counts as playing as part of its album, which is when "Auto" levels
//! by the album's value and not the song's own. The gain itself is worked out from the
//! app's settings and stays with each app for now.

use crate::model::Song;

/// Whether a song is playing as part of its album, which is when Auto levels by album: shuffle
/// is off and the song before or after it is the album's previous or next track. A playlist
/// or a shuffle uses each song's own value instead, except for a run of consecutive tracks
/// from one album, which keeps their levels relative to each other.
/// Without track numbers to go by, any neighbour from the same album counts.
pub fn in_album_order(song: &Song, before: Option<&Song>, after: Option<&Song>, shuffled: bool) -> bool {
    if shuffled || song.album_id.is_empty() {
        return false;
    }
    let key = |s: &Song| (s.disc_number.max(1), s.track);
    // Whether `second` follows `first` directly in the album: the next track on the same
    // disc, or the first track of the next disc.
    let follows = |first: &Song, second: &Song| {
        let ((d1, t1), (d2, t2)) = (key(first), key(second));
        (d1 == d2 && t1 + 1 == t2) || (d1 + 1 == d2 && t2 == 1)
    };
    [(before, true), (after, false)].into_iter().any(|(other, is_before)| {
        other.is_some_and(|other| {
            other.album_id == song.album_id
                && if song.track == 0 || other.track == 0 {
                    true
                } else if is_before {
                    follows(other, song)
                } else {
                    follows(song, other)
                }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn album_order_decides_auto_levelling() {
        let track = |album: &str, disc: u32, track: u32| Song { album_id: album.into(), disc_number: disc, track, ..Default::default() };
        let (one, two, three) = (track("a", 1, 1), track("a", 1, 2), track("a", 1, 3));
        // Playing an album through, in order.
        assert!(in_album_order(&two, Some(&one), Some(&three), false));
        assert!(in_album_order(&one, None, Some(&two), false));
        assert!(in_album_order(&three, Some(&two), None, false));
        // Across a disc change, with or without disc numbers on the first disc.
        assert!(in_album_order(&track("a", 2, 1), Some(&track("a", 1, 12)), None, false));
        assert!(in_album_order(&track("a", 2, 1), Some(&track("a", 0, 12)), None, false));
        // Shuffled, even if two happen to land in order.
        assert!(!in_album_order(&two, Some(&one), Some(&three), true));
        // A playlist: neighbours from other albums, or from the same album out of order.
        assert!(!in_album_order(&two, Some(&track("b", 1, 1)), Some(&track("c", 1, 3)), false));
        assert!(!in_album_order(&two, Some(&track("a", 1, 7)), Some(&track("a", 1, 9)), false));
        assert!(!in_album_order(&two, Some(&three), Some(&one), false));
        // Two consecutive tracks of an album within a playlist keep their balance.
        assert!(in_album_order(&two, Some(&track("b", 1, 1)), Some(&three), false));
        // No track numbers: any neighbour from the album counts; no album never does.
        assert!(in_album_order(&track("a", 0, 0), Some(&track("a", 0, 0)), None, false));
        assert!(!in_album_order(&track("", 1, 2), Some(&track("", 1, 1)), None, false));
        // A lone song.
        assert!(!in_album_order(&one, None, None, false));
    }
}
