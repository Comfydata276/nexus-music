//! What's been asked for through this app, kept so it can be followed up.
//!
//! Lidarr fetches an album in its own time, minutes or days later. Without a note of what
//! was asked, the album would just turn up in the library one day. With one, the app can say
//! it's arrived, name the song that was wanted from it, and put the songs a playlist was
//! missing into that playlist.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::import::{Matcher, Wanted};
use crate::musicbrainz::{same_words, words};
use crate::search::Index;

/// How long an arrival stays listed on the Requests page.
const SHOWN_FOR: i64 = 14 * 24 * 3600;
/// How long to go on watching for something that never arrives.
const GIVEN_UP_AFTER: i64 = 180 * 24 * 3600;
/// How long after an album arrives to go on trying to get its songs into the playlists that
/// were waiting for them. An album can show up in the library before all its songs have,
/// and a playlist can fail to save.
const FILLED_WITHIN: i64 = 3 * 24 * 3600;

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
#[derive(Debug, Clone, Default, PartialEq)]
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

pub struct AskedList {
    path: PathBuf,
    pub items: Vec<Asked>,
}

/// A copy for the threads that look for arrivals and draw the Requests page.
static SHARED: RwLock<Vec<Asked>> = RwLock::new(Vec::new());

pub fn shared() -> Vec<Asked> {
    SHARED.read().map(|items| items.clone()).unwrap_or_default()
}

impl AskedList {
    pub fn load(path: &Path) -> AskedList {
        let items: Vec<Asked> = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let list = AskedList { path: path.to_path_buf(), items };
        list.share();
        list
    }

    fn share(&self) {
        if let Ok(mut shared) = SHARED.write() {
            *shared = self.items.clone();
        }
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_vec_pretty(&self.items) {
            let _ = std::fs::write(&self.path, json);
        }
        self.share();
    }

    /// Notes something asked for. Asking again for the same album adds to the note that's
    /// there: more songs for playlists, or the song that's wanted from it.
    pub fn add(&mut self, asked: Asked) {
        self.put(asked);
        self.save();
    }

    /// Notes several things asked for at once.
    pub fn add_all(&mut self, asked: Vec<Asked>) {
        if asked.is_empty() {
            return;
        }
        for one in asked {
            self.put(one);
        }
        self.save();
    }

    fn put(&mut self, asked: Asked) {
        let same = |other: &Asked| {
            other.arrived == 0 && ((!asked.album.is_empty() && other.album == asked.album) || (same_words(&other.title, &asked.title) && same_artist(&other.artist, &asked.artist)))
        };
        match self.items.iter_mut().find(|other| same(other)) {
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
            None => self.items.push(asked),
        }
    }

    /// Stops watching for an album that's been taken off Lidarr's list.
    pub fn forget(&mut self, title: &str) {
        let before = self.items.len();
        self.items.retain(|asked| asked.arrived != 0 || !same_words(&asked.title, title));
        if self.items.len() != before {
            self.save();
        }
    }

    /// Whether anything asked for is still on its way, or has songs still to go into
    /// playlists.
    pub fn waiting(&self) -> bool {
        self.items.iter().any(|asked| asked.arrived == 0 || asked.unfilled)
    }

    /// Notes how getting an arrival's songs into its playlists went: done unless some of them
    /// still aren't in the library.
    pub fn filled(&mut self, at: i64, title: &str, missing: usize) {
        if let Some(asked) = self.items.iter_mut().find(|asked| asked.at == at && asked.title == title && asked.arrived != 0) {
            asked.unfilled = missing > 0;
            self.save();
        }
    }

    /// Drops what's been listed long enough, and what's been waited for too long.
    pub fn tidy(&mut self, now: i64) {
        let before = self.items.len();
        self.items.retain(|asked| if asked.arrived != 0 { now - asked.arrived < SHOWN_FOR } else { now - asked.at < GIVEN_UP_AFTER });
        if self.items.len() != before {
            self.save();
        }
    }

    /// Marks arrivals as such.
    pub fn mark(&mut self, arrivals: &[Arrival], now: i64) {
        for arrival in arrivals {
            let Some(asked) = self.items.get_mut(arrival.place) else { continue };
            if asked.arrived == 0 {
                asked.arrived = now;
                asked.library_album = arrival.library_album.clone();
                asked.cover = arrival.cover.clone();
            }
            // Until its songs are known to be in their playlists (see `filled`).
            asked.unfilled = !arrival.fills.is_empty() || arrival.missing > 0;
        }
        self.save();
    }
}

/// Whether two artist credits name the same artist, or one is among the other's.
pub(crate) fn same_artist(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    !a.is_empty() && !b.is_empty() && (a.iter().all(|word| b.contains(word)) || b.iter().all(|word| a.contains(word)))
}

/// Looks through the library for the albums in `asked` that are still awaited, and for the
/// ones that arrived lately with songs still to go into playlists. For each that's there
/// now, finds the songs its playlists were missing.
pub fn arrivals(index: &Index, asked: &[Asked], now: i64) -> Vec<Arrival> {
    let due = |asked: &Asked| asked.arrived == 0 || (asked.unfilled && now - asked.arrived < FILLED_WITHIN);
    let awaited: Vec<(usize, &Asked)> = asked.iter().enumerate().filter(|(_, asked)| due(asked)).collect();
    if awaited.is_empty() {
        return Vec::new();
    }
    let matcher = Matcher::new(index);
    awaited
        .into_iter()
        .filter_map(|(place, asked)| {
            // The library's albums carry their artist as the line beneath the title.
            let album = index.library.albums.iter().find(|album| same_words(&album.title, &asked.title) && same_artist(&album.subtitle, &asked.artist))?;
            let fills: Vec<(String, String, String, i64)> = asked
                .fills
                .iter()
                .filter_map(|fill| {
                    let wanted = Wanted { title: fill.title.clone(), artists: fill.artists.clone(), album: asked.title.clone(), seconds: fill.seconds, added: fill.added };
                    let song = matcher.find(&wanted)?;
                    Some((fill.playlist.clone(), fill.playlist_name.clone(), song.id.clone(), fill.added))
                })
                .collect();
            let missing = asked.fills.len() - fills.len();
            // One that arrived before and still has nothing to add is no news.
            if asked.arrived != 0 && fills.is_empty() {
                return None;
            }
            Some(Arrival { place, library_album: album.id.clone(), cover: album.cover.clone(), fills, missing, at: asked.at, title: asked.title.clone() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::{CardData, Kind};
    use crate::search::Library;
    use crate::subsonic::Song;

    fn index() -> Index {
        let album = |id: &str, title: &str, artist: &str| CardData { kind: Kind::Album, id: id.into(), title: title.into(), subtitle: artist.into(), cover: format!("al-{id}") };
        let song = |id: &str, title: &str, artist: &str, album: &str| Song { id: id.into(), title: title.into(), artist: artist.into(), album: album.into(), duration: 200, ..Default::default() };
        Index::new(Library {
            albums: vec![album("a1", "…Something to Be", "Rob Thomas"), album("a2", "Parachutes", "Coldplay")],
            songs: vec![song("s1", "Lonely No More", "Rob Thomas", "…Something to Be"), song("s2", "Yellow", "Coldplay", "Parachutes")],
            ..Default::default()
        })
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
        let found = arrivals(&index(), &asked, 1000);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].place, found[0].library_album.as_str(), found[0].cover.as_str()), (0, "a1", "al-a1"));
        assert_eq!(found[0].fills, vec![("p1".to_string(), "Road Trip".to_string(), "s1".to_string(), 1_700_000_000)]);
        // One of the two songs its playlist wanted isn't in the library (yet).
        assert_eq!(found[0].missing, 1);
    }

    #[test]
    fn goes_on_filling_playlists_after_an_album_arrives() {
        let path = std::env::temp_dir().join(format!("nexus-asked-fill-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let fill = Fill { playlist: "p1".into(), playlist_name: "Road Trip".into(), title: "Lonely No More".into(), artists: vec!["Rob Thomas".into()], seconds: 200, added: 0 };
        let mut list = AskedList::load(&path);
        list.add(Asked { title: "...Something to Be".into(), artist: "Rob Thomas".into(), at: 50, fills: vec![fill], ..Default::default() });
        let first = arrivals(&index(), &list.items, 1000);
        list.mark(&first, 1000);
        // It has arrived, and its song still has to go into the playlist: if that fails (or
        // the app closes first) the next look at the library offers it again.
        assert!(list.items[0].arrived == 1000 && list.items[0].unfilled && list.waiting());
        let again = arrivals(&index(), &list.items, 2000);
        assert_eq!((again.len(), again[0].fills.len()), (1, 1));
        list.mark(&again, 2000);
        assert_eq!(list.items[0].arrived, 1000);
        // Once the playlist has it, there's nothing more to do.
        list.filled(50, "...Something to Be", 0);
        assert!(!list.items[0].unfilled && !list.waiting());
        assert!(arrivals(&index(), &list.items, 3000).is_empty());
        // And it isn't tried for ever.
        list.items[0].unfilled = true;
        assert!(arrivals(&index(), &list.items, 1000 + FILLED_WITHIN + 1).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keeps_one_note_per_album() {
        let path = std::env::temp_dir().join(format!("nexus-asked-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut list = AskedList::load(&path);
        list.add(Asked { album: "rg1".into(), title: "Cradlesong".into(), artist: "Rob Thomas".into(), at: 100, ..Default::default() });
        list.add(Asked { album: "rg1".into(), title: "Cradlesong".into(), artist: "Rob Thomas".into(), at: 200, song: "Her Diamonds".into(), ..Default::default() });
        list.add(Asked { title: "cradlesong".into(), artist: "rob thomas".into(), fills: vec![Fill { title: "Someday".into(), ..Default::default() }], ..Default::default() });
        assert_eq!(list.items.len(), 1);
        assert_eq!((list.items[0].song.as_str(), list.items[0].fills.len(), list.items[0].at), ("Her Diamonds", 1, 100));
        assert!(list.waiting());

        list.mark(&[Arrival { place: 0, library_album: "a9".into(), ..Default::default() }], 1000);
        assert!(!list.waiting());
        assert_eq!(AskedList::load(&path).items[0].library_album, "a9");
        // Taking it off Lidarr's list doesn't forget one that's already here.
        list.forget("Cradlesong");
        assert_eq!(list.items.len(), 1);
        list.tidy(1000 + SHOWN_FOR + 1);
        assert!(list.items.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
