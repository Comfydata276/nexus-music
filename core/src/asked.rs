//! What's been asked for through this app, kept so it can be followed up.
//!
//! Lidarr fetches an album in its own time, minutes or days later. Without a note of what
//! was asked, the album would just turn up in the library one day. With one, the app can say
//! it's arrived, name the song that was wanted from it, and put the songs a playlist was
//! missing into that playlist.
//!
//! The list and its rules are here. Keeping it in a file, and the library it's looked for
//! in, are the app's: each rule that changes the list says whether it did, for the app to
//! write it down.

use serde::{Deserialize, Serialize};

use crate::text::{same_artist, same_words};

/// How long an arrival stays listed on the Requests page.
pub const SHOWN_FOR: i64 = 14 * 24 * 3600;
/// How long to go on watching for something that never arrives.
const GIVEN_UP_AFTER: i64 = 180 * 24 * 3600;
/// How long after an album arrives to go on trying to get its songs into the playlists that
/// were waiting for them. An album can show up in the library before all its songs have,
/// and a playlist can fail to save.
pub const FILLED_WITHIN: i64 = 3 * 24 * 3600;

/// A song a playlist asks for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Wanted {
    pub title: String,
    pub artists: Vec<String>,
    pub album: String,
    /// Its length in seconds; 0 if the file doesn't say.
    pub seconds: u32,
    /// When it was added to the playlist, in seconds since 1970; 0 if the file doesn't say.
    pub added: i64,
}

/// A song a playlist is missing, to be put in once its album arrives.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fill {
    /// The playlist it belongs in: its id if it exists, and its name either way.
    pub playlist: String,
    pub playlist_name: String,
    pub title: String,
    pub artists: Vec<String>,
    pub seconds: u32,
    /// When it was added to the playlist it came from, in seconds since 1970; 0 if unknown.
    pub added: i64,
}

/// One album that was asked for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Asked {
    /// Lidarr's name for the album; may be empty.
    pub album: String,
    pub title: String,
    pub artist: String,
    /// When it was asked for.
    pub at: i64,
    /// The song that led to it, if it was asked for from a song.
    pub song: String,
    pub fills: Vec<Fill>,
    /// When it turned up in the library (0 until then), and what the library calls it.
    pub arrived: i64,
    pub library_album: String,
    pub cover: String,
    /// It has arrived, but not all the songs playlists were waiting for are in them yet.
    pub unfilled: bool,
}

/// An album that has turned up, with what's to be done about it.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Arrival {
    /// Its place among what's been asked for.
    pub place: usize,
    pub library_album: String,
    pub cover: String,
    /// Songs to put into playlists: the playlist (id, name), the song's id, and when it was
    /// added where it came from.
    pub fills: Vec<(String, String, String, i64)>,
    /// How many songs its playlists are waiting for that aren't in the library yet.
    pub missing: usize,
    /// When it was asked for and by what title: what tells it apart once places have moved.
    pub at: i64,
    pub title: String,
}

/// Notes something asked for. Asking again for the same album adds to the note that's
/// there: more songs for playlists, or the song that's wanted from it.
pub fn put(items: &mut Vec<Asked>, asked: Asked) {
    let same = |other: &Asked| {
        other.arrived == 0 && ((!asked.album.is_empty() && other.album == asked.album) || (same_words(&other.title, &asked.title) && same_artist(&other.artist, &asked.artist)))
    };
    match items.iter_mut().find(|other| same(other)) {
        Some(known) => {
            if known.song.is_empty() {
                known.song = asked.song;
            }
            if known.album.is_empty() {
                known.album = asked.album;
            }
            for fill in asked.fills {
                if !known.fills.contains(&fill) {
                    known.fills.push(fill);
                }
            }
        }
        None => items.push(asked),
    }
}

/// Stops watching for an album that's been taken off Lidarr's list: the one with its
/// MusicBrainz id (`album`), or, where either side has none (a note written by an older
/// version), the one with its title and artist both. Never by its title alone: two albums of
/// one name are two albums. Says whether the list changed.
pub fn forget(items: &mut Vec<Asked>, album: &str, title: &str, artist: &str) -> bool {
    let before = items.len();
    items.retain(|asked| {
        let same = if !album.is_empty() && !asked.album.is_empty() {
            asked.album == album
        } else {
            !artist.is_empty() && same_words(&asked.title, title) && same_artist(&asked.artist, artist)
        };
        asked.arrived != 0 || !same
    });
    items.len() != before
}

/// Whether anything asked for is still on its way, or has songs still to go into
/// playlists.
pub fn waiting(items: &[Asked]) -> bool {
    items.iter().any(|asked| asked.arrived == 0 || asked.unfilled)
}

/// Notes how getting an arrival's songs into its playlists went: done unless some of them
/// still aren't in the library. Says whether the list changed.
pub fn filled(items: &mut [Asked], at: i64, title: &str, missing: usize) -> bool {
    if let Some(asked) = items.iter_mut().find(|asked| asked.at == at && asked.title == title && asked.arrived != 0) {
        asked.unfilled = missing > 0;
        return true;
    }
    false
}

/// Drops what's been listed long enough, and what's been waited for too long. Says whether
/// the list changed.
pub fn tidy(items: &mut Vec<Asked>, now: i64) -> bool {
    let before = items.len();
    items.retain(|asked| if asked.arrived != 0 { now - asked.arrived < SHOWN_FOR } else { now - asked.at < GIVEN_UP_AFTER });
    items.len() != before
}

/// Marks arrivals as such.
pub fn mark(items: &mut [Asked], arrivals: &[Arrival], now: i64) {
    for arrival in arrivals {
        let Some(asked) = items.get_mut(arrival.place) else { continue };
        if asked.arrived == 0 {
            asked.arrived = now;
            asked.library_album = arrival.library_album.clone();
            asked.cover = arrival.cover.clone();
        }
        // Until its songs are known to be in their playlists (see `filled`).
        asked.unfilled = !arrival.fills.is_empty() || arrival.missing > 0;
    }
}

/// Whether something asked for is still to be looked for in the library: it hasn't arrived,
/// or it arrived lately with songs still to go into playlists.
pub fn due(asked: &Asked, now: i64) -> bool {
    asked.arrived == 0 || (asked.unfilled && now - asked.arrived < FILLED_WITHIN)
}

/// Looks through the library for the albums in `asked` that are still awaited, and for the
/// ones that arrived lately with songs still to go into playlists. For each that's there
/// now, finds the songs its playlists were missing.
///
/// The library is the app's. `album` finds an album by its title and artist, as its id and
/// cover; `song` finds the library's copy of a song a playlist wanted, as its id.
pub fn arrivals(
    asked: &[Asked],
    now: i64,
    album: impl Fn(&str, &str) -> Option<(String, String)>,
    song: impl Fn(&Wanted) -> Option<String>,
) -> Vec<Arrival> {
    let awaited: Vec<(usize, &Asked)> = asked.iter().enumerate().filter(|(_, asked)| due(asked, now)).collect();
    if awaited.is_empty() {
        return Vec::new();
    }
    awaited
        .into_iter()
        .filter_map(|(place, asked)| {
            let (library_album, cover) = album(&asked.title, &asked.artist)?;
            let fills: Vec<(String, String, String, i64)> = asked
                .fills
                .iter()
                .filter_map(|fill| {
                    let wanted = Wanted { title: fill.title.clone(), artists: fill.artists.clone(), album: asked.title.clone(), seconds: fill.seconds, added: fill.added };
                    let song = song(&wanted)?;
                    Some((fill.playlist.clone(), fill.playlist_name.clone(), song, fill.added))
                })
                .collect();
            let missing = asked.fills.len() - fills.len();
            // One that arrived before and still has nothing to add is no news.
            if asked.arrived != 0 && fills.is_empty() {
                return None;
            }
            Some(Arrival { place, library_album, cover, fills, missing, at: asked.at, title: asked.title.clone() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library of two albums and two songs, as the app would look in its own.
    fn album(title: &str, artist: &str) -> Option<(String, String)> {
        [("a1", "…Something to Be", "Rob Thomas"), ("a2", "Parachutes", "Coldplay")]
            .into_iter()
            .find(|(_, known, by)| same_words(known, title) && same_artist(by, artist))
            .map(|(id, _, _)| (id.to_string(), format!("al-{id}")))
    }

    fn song(wanted: &Wanted) -> Option<String> {
        [("s1", "Lonely No More", "Rob Thomas"), ("s2", "Yellow", "Coldplay")]
            .into_iter()
            .find(|(_, title, artist)| same_words(title, &wanted.title) && wanted.artists.iter().any(|known| same_artist(known, artist)))
            .map(|(id, _, _)| id.to_string())
    }

    #[test]
    fn spots_what_has_arrived() {
        let fill = Fill { playlist: "p1".into(), playlist_name: "Road Trip".into(), title: "Lonely No More".into(), artists: vec!["Rob Thomas".into()], seconds: 200, added: 1_700_000_000 };
        let asked = vec![
            Asked { title: "...Something to Be".into(), artist: "Rob Thomas".into(), fills: vec![fill.clone(), Fill { title: "Not On It".into(), ..fill.clone() }], ..Default::default() },
            Asked { title: "Cradlesong".into(), artist: "Rob Thomas".into(), ..Default::default() },
            Asked { title: "Parachutes".into(), artist: "Coldplay".into(), arrived: 5, ..Default::default() },
            Asked { title: "Parachutes".into(), artist: "Someone Else".into(), ..Default::default() },
        ];
        let found = arrivals(&asked, 1000, album, song);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].place, found[0].library_album.as_str(), found[0].cover.as_str()), (0, "a1", "al-a1"));
        assert_eq!(found[0].fills, vec![("p1".to_string(), "Road Trip".to_string(), "s1".to_string(), 1_700_000_000)]);
        // One of the two songs its playlist wanted isn't in the library (yet).
        assert_eq!(found[0].missing, 1);
        // Nothing awaited: the library isn't looked in.
        let looked = std::cell::Cell::new(false);
        let arrived = vec![Asked { arrived: 5, ..Default::default() }];
        assert!(arrivals(&arrived, 1000, |_, _| { looked.set(true); None }, |_| None).is_empty());
        assert!(!looked.get());
    }

    #[test]
    fn goes_on_filling_playlists_after_an_album_arrives() {
        let fill = Fill { playlist: "p1".into(), playlist_name: "Road Trip".into(), title: "Lonely No More".into(), artists: vec!["Rob Thomas".into()], seconds: 200, added: 0 };
        let mut items = Vec::new();
        put(&mut items, Asked { title: "...Something to Be".into(), artist: "Rob Thomas".into(), at: 50, fills: vec![fill], ..Default::default() });
        let first = arrivals(&items, 1000, album, song);
        mark(&mut items, &first, 1000);
        assert!(items[0].arrived == 1000 && items[0].unfilled && waiting(&items));
        let again = arrivals(&items, 2000, album, song);
        assert_eq!((again.len(), again[0].fills.len()), (1, 1));
        mark(&mut items, &again, 2000);
        assert_eq!(items[0].arrived, 1000);
        assert!(filled(&mut items, 50, "...Something to Be", 0));
        assert!(!items[0].unfilled && !waiting(&items));
        assert!(arrivals(&items, 3000, album, song).is_empty());
        items[0].unfilled = true;
        assert!(arrivals(&items, 1000 + FILLED_WITHIN + 1, album, song).is_empty());
        // Only an arrival is filled.
        assert!(!filled(&mut items, 51, "...Something to Be", 0));
    }

    #[test]
    fn keeps_one_note_per_album() {
        let mut items = Vec::new();
        put(&mut items, Asked { album: "rg1".into(), title: "Cradlesong".into(), artist: "Rob Thomas".into(), at: 100, ..Default::default() });
        put(&mut items, Asked { album: "rg1".into(), title: "Cradlesong".into(), artist: "Rob Thomas".into(), at: 200, song: "Her Diamonds".into(), ..Default::default() });
        put(&mut items, Asked { title: "cradlesong".into(), artist: "rob thomas".into(), fills: vec![Fill { title: "Someday".into(), ..Default::default() }], ..Default::default() });
        assert_eq!(items.len(), 1);
        assert_eq!((items[0].song.as_str(), items[0].fills.len(), items[0].at), ("Her Diamonds", 1, 100));
        assert!(waiting(&items));

        mark(&mut items, &[Arrival { place: 0, library_album: "a9".into(), ..Default::default() }], 1000);
        assert!(!waiting(&items));
        assert_eq!(items[0].library_album, "a9");
        // Taking it off Lidarr's list doesn't forget one that's already here.
        assert!(!forget(&mut items, "", "Cradlesong", "Rob Thomas"));
        assert_eq!(items.len(), 1);
        assert!(!tidy(&mut items, 1000 + SHOWN_FOR - 1));
        assert!(tidy(&mut items, 1000 + SHOWN_FOR + 1));
        assert!(items.is_empty());
        // One still awaited is forgotten by its title, and given up on after half a year.
        put(&mut items, Asked { title: "Cradlesong".into(), artist: "Rob Thomas".into(), at: 100, ..Default::default() });
        put(&mut items, Asked { title: "Something to Be".into(), artist: "Rob Thomas".into(), at: 100, ..Default::default() });
        assert!(forget(&mut items, "", "cradlesong", "rob thomas") && items.len() == 1);
        assert!(!tidy(&mut items, 100 + GIVEN_UP_AFTER - 1) && tidy(&mut items, 100 + GIVEN_UP_AFTER));
    }

    #[test]
    fn what_was_asked_for_is_forgotten_by_what_it_is() {
        // Two albums of one name, by two artists, both asked for; and one noted by an older
        // version, with no id. Taking one off forgets that one alone.
        let mut items = vec![
            Asked { album: "rg-a".into(), title: "Greatest Hits".into(), artist: "Queen".into(), ..Default::default() },
            Asked { album: "rg-b".into(), title: "Greatest Hits".into(), artist: "ABBA".into(), ..Default::default() },
            Asked { title: "Greatest Hits".into(), artist: "Blondie".into(), ..Default::default() },
        ];
        assert!(forget(&mut items, "rg-b", "Greatest Hits", "ABBA"));
        assert_eq!(items.iter().map(|asked| asked.artist.as_str()).collect::<Vec<_>>(), ["Queen", "Blondie"]);
        // One with no id is found by its title and artist together, never its title alone.
        assert!(!forget(&mut items, "rg-z", "Greatest Hits", "Someone Else"));
        assert!(forget(&mut items, "rg-c", "greatest hits", "blondie"));
        assert_eq!(items.iter().map(|asked| asked.artist.as_str()).collect::<Vec<_>>(), ["Queen"]);
        // An id that doesn't match isn't overruled by a title and artist that do.
        assert!(!forget(&mut items, "rg-other", "Greatest Hits", "Queen"));
    }
}
