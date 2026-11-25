//! What's been asked for through this app, kept so it can be followed up.
//!
//! Lidarr fetches an album in its own time, minutes or days later. Without a note of what
//! was asked, the album would just turn up in the library one day. With one, the app can say
//! it's arrived, name the song that was wanted from it, and put the songs a playlist was
//! missing into that playlist.
//!
//! The list and its rules are in the library (`nexus_core::asked`), for the Android app to
//! use too. This is the Windows app's end: the file the list is kept in, the copy the other
//! threads read, and the library copy arrivals are looked for in.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::import::Matcher;
use crate::search::Index;

pub use nexus_core::asked::{Arrival, Asked, Fill};

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
        nexus_core::asked::put(&mut self.items, asked);
        self.save();
    }

    /// Notes several things asked for at once.
    pub fn add_all(&mut self, asked: Vec<Asked>) {
        if asked.is_empty() {
            return;
        }
        for one in asked {
            nexus_core::asked::put(&mut self.items, one);
        }
        self.save();
    }

    /// Stops watching for an album that's been taken off Lidarr's list.
    pub fn forget(&mut self, album: &str, title: &str, artist: &str) {
        if nexus_core::asked::forget(&mut self.items, album, title, artist) {
            self.save();
        }
    }

    /// Whether anything asked for is still on its way, or has songs still to go into
    /// playlists.
    pub fn waiting(&self) -> bool {
        nexus_core::asked::waiting(&self.items)
    }

    /// Notes how getting an arrival's songs into its playlists went: done unless some of them
    /// still aren't in the library.
    pub fn filled(&mut self, at: i64, title: &str, missing: usize) {
        if nexus_core::asked::filled(&mut self.items, at, title, missing) {
            self.save();
        }
    }

    /// Drops what's been listed long enough, and what's been waited for too long.
    pub fn tidy(&mut self, now: i64) {
        if nexus_core::asked::tidy(&mut self.items, now) {
            self.save();
        }
    }

    /// Marks arrivals as such.
    pub fn mark(&mut self, arrivals: &[Arrival], now: i64) {
        nexus_core::asked::mark(&mut self.items, arrivals, now);
        self.save();
    }
}

/// Looks through the library for the albums in `asked` that are still awaited, and for the
/// ones that arrived lately with songs still to go into playlists. For each that's there
/// now, finds the songs its playlists were missing.
pub fn arrivals(index: &Index, asked: &[Asked], now: i64) -> Vec<Arrival> {
    if !asked.iter().any(|asked| nexus_core::asked::due(asked, now)) {
        return Vec::new();
    }
    let matcher = Matcher::new(index);
    nexus_core::asked::arrivals(
        asked,
        now,
        // The album an arrival is, by the library's rule (the same words, the same artist).
        |title, artist| nexus_core::requests::Library::same_album(index, title, artist),
        |wanted| matcher.find(wanted).map(|song| song.id.clone()),
    )
}

#[cfg(test)]
mod tests {
    use nexus_core::asked::{FILLED_WITHIN, SHOWN_FOR};

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
        list.forget("", "Cradlesong", "Rob Thomas");
        assert_eq!(list.items.len(), 1);
        list.tidy(1000 + SHOWN_FOR + 1);
        assert!(list.items.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
