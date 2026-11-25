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
    /// The file is there and could not be read: it is not written over (see `read_back`).
    held: bool,
    /// What the person is told on the Requests page about the file, if anything.
    trouble: String,
}

/// A copy for the threads that look for arrivals and draw the Requests page.
static SHARED: RwLock<Vec<Asked>> = RwLock::new(Vec::new());
/// What the Requests page says about the file the list is kept in, if anything.
static TROUBLE: RwLock<String> = RwLock::new(String::new());

pub fn shared() -> Vec<Asked> {
    SHARED.read().map(|items| items.clone()).unwrap_or_default()
}

/// For the Requests page: the list's file was found damaged or could not be read.
pub fn trouble() -> String {
    TROUBLE.read().map(|trouble| trouble.clone()).unwrap_or_default()
}

const FOUND_EMPTY: &str = "The list of albums asked for through this app was found empty: the app was stopped while it was saving it. Albums asked for before then won't be announced when they arrive, or put into their playlists. Lidarr still fetches them, and what it's fetching is shown here.";
const FOUND_DAMAGED: &str = "The list of albums asked for through this app couldn't be read, so albums asked for before now won't be announced when they arrive, or put into their playlists. Lidarr still fetches them, and what it's fetching is shown here.";
const NOT_READ: &str = "The list of albums asked for through this app couldn't be read just now, so it has been left as it is. It is read again when something is next asked for, and when the app next starts.";

/// What the file holds, read back. **A file that could not be read is never taken for an
/// empty list**: one that is there and can't be read just now is left as it is, and not
/// written over (`held`); one that reads and isn't a list (cut short by 0.40.10 or earlier,
/// which wrote it in place) is moved aside under a name of its own (`config::set_aside`),
/// and only then written afresh; if it can't be moved, it is held as one that can't be read.
/// An empty file is what such a cut left: there is nothing to keep of it.
fn read_back(path: &Path) -> (Vec<Asked>, bool, String) {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(items) => (items, false, String::new()),
            Err(_) if bytes.iter().all(u8::is_ascii_whitespace) => (Vec::new(), false, FOUND_EMPTY.into()),
            Err(_) => match crate::config::set_aside(path) {
                Some(aside) => (Vec::new(), false, format!("{FOUND_DAMAGED} The file has been kept as {}.", aside.file_name().unwrap_or_default().to_string_lossy())),
                None => (Vec::new(), true, NOT_READ.into()),
            },
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false, String::new()),
        Err(_) => (Vec::new(), true, NOT_READ.into()),
    }
}

impl AskedList {
    pub fn load(path: &Path) -> AskedList {
        let (items, held, trouble) = read_back(path);
        let list = AskedList { path: path.to_path_buf(), items, held, trouble };
        list.share();
        list
    }

    fn share(&self) {
        if let Ok(mut shared) = SHARED.write() {
            *shared = self.items.clone();
        }
        if let Ok(mut shared) = TROUBLE.write() {
            shared.clone_from(&self.trouble);
        }
    }

    /// Written whole, or not at all: a kill or a power cut part-way leaves the list as it
    /// was (`config::write_surely`). A file that could not be read when the app started is
    /// tried again first, and what it holds is joined to what has been asked since.
    pub fn save(&mut self) {
        if self.held {
            let (items, held, trouble) = read_back(&self.path);
            if !held {
                let mut joined = items;
                for asked in self.items.drain(..) {
                    nexus_core::asked::put(&mut joined, asked);
                }
                self.items = joined;
                self.held = false;
                self.trouble = trouble;
            }
        }
        if !self.held {
            if let Ok(json) = serde_json::to_vec_pretty(&self.items) {
                crate::saver::saver().write(self.path.clone(), move || Some(json));
            }
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
    pub fn forget(&mut self, title: &str) {
        if nexus_core::asked::forget(&mut self.items, title) {
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
        crate::saver::saver().flush();
        let first = arrivals(&index(), &list.items, 1000);
        list.mark(&first, 1000);
        crate::saver::saver().flush();
        // It has arrived, and its song still has to go into the playlist: if that fails (or
        // the app closes first) the next look at the library offers it again.
        assert!(list.items[0].arrived == 1000 && list.items[0].unfilled && list.waiting());
        let again = arrivals(&index(), &list.items, 2000);
        assert_eq!((again.len(), again[0].fills.len()), (1, 1));
        list.mark(&again, 2000);
        crate::saver::saver().flush();
        assert_eq!(list.items[0].arrived, 1000);
        // Once the playlist has it, there's nothing more to do.
        list.filled(50, "...Something to Be", 0);
        crate::saver::saver().flush();
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
        crate::saver::saver().flush();
        list.add(Asked { album: "rg1".into(), title: "Cradlesong".into(), artist: "Rob Thomas".into(), at: 200, song: "Her Diamonds".into(), ..Default::default() });
        crate::saver::saver().flush();
        list.add(Asked { title: "cradlesong".into(), artist: "rob thomas".into(), fills: vec![Fill { title: "Someday".into(), ..Default::default() }], ..Default::default() });
        crate::saver::saver().flush();
        assert_eq!(list.items.len(), 1);
        assert_eq!((list.items[0].song.as_str(), list.items[0].fills.len(), list.items[0].at), ("Her Diamonds", 1, 100));
        assert!(list.waiting());

        list.mark(&[Arrival { place: 0, library_album: "a9".into(), ..Default::default() }], 1000);
        crate::saver::saver().flush();
        assert!(!list.waiting());
        assert_eq!(AskedList::load(&path).items[0].library_album, "a9");
        // Taking it off Lidarr's list doesn't forget one that's already here.
        list.forget("Cradlesong");
        crate::saver::saver().flush();
        assert_eq!(list.items.len(), 1);
        list.tidy(1000 + SHOWN_FOR + 1);
        crate::saver::saver().flush();
        assert!(list.items.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_list_found_empty_or_damaged_is_said_and_kept_and_never_taken_for_nothing() {
        let dir = std::env::temp_dir().join(format!("nexus-asked-found-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("asked.json");
        let cradlesong = || Asked { title: "Cradlesong".into(), artist: "Rob Thomas".into(), at: 100, ..Default::default() };
        // What 0.40.10 left, killed while it saved the list in place: an empty file. Nothing
        // can be had back from it, and the person is told why arrivals of what they asked
        // for before won't be announced.
        std::fs::write(&path, "").unwrap();
        let mut list = AskedList::load(&path);
        assert!(list.items.is_empty());
        assert_eq!(list.trouble, FOUND_EMPTY);
        list.add(cradlesong());
        crate::saver::saver().flush();
        assert_eq!(AskedList::load(&path).items.len(), 1);
        // Cut short part-way: it is kept aside, as it was, before anything is written.
        let whole = std::fs::read(&path).unwrap();
        std::fs::write(&path, &whole[..whole.len() / 2]).unwrap();
        let mut list = AskedList::load(&path);
        // (Kept under a name of its own, which the page names: a second never replaces it.)
        let aside = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().path()).find(|path| path.file_name().unwrap().to_string_lossy().starts_with("asked.damaged-")).unwrap();
        assert!(list.items.is_empty() && list.trouble.starts_with(FOUND_DAMAGED));
        assert!(list.trouble.contains(&*aside.file_name().unwrap().to_string_lossy()));
        assert_eq!(std::fs::read(&aside).unwrap(), &whole[..whole.len() / 2]);
        list.add(Asked { title: "Parachutes".into(), artist: "Coldplay".into(), ..Default::default() });
        crate::saver::saver().flush();
        assert_eq!(std::fs::read(&aside).unwrap(), &whole[..whole.len() / 2]);
        assert_eq!(AskedList::load(&path).items[0].title, "Parachutes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_list_that_could_not_be_read_is_not_written_over_and_is_joined_once_it_can_be() {
        let dir = std::env::temp_dir().join(format!("nexus-asked-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("asked.json");
        let mut list = AskedList::load(&path);
        list.add(Asked { title: "Cradlesong".into(), artist: "Rob Thomas".into(), at: 100, ..Default::default() });
        crate::saver::saver().flush();
        let before = std::fs::read(&path).unwrap();
        // (A folder where the file is read is a file that can't be read, on every system.)
        let away = dir.join("away.json");
        std::fs::rename(&path, &away).unwrap();
        std::fs::create_dir(&path).unwrap();
        let mut list = AskedList::load(&path);
        assert_eq!((list.items.len(), list.trouble.as_str()), (0, NOT_READ));
        list.add(Asked { title: "Parachutes".into(), artist: "Coldplay".into(), at: 200, ..Default::default() });
        crate::saver::saver().flush();
        assert!(path.is_dir(), "written over");
        // It can be read again: what it holds and what was asked meanwhile, both.
        std::fs::remove_dir(&path).unwrap();
        std::fs::rename(&away, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        list.add(Asked { title: "Yellow".into(), artist: "Coldplay".into(), at: 300, ..Default::default() });
        crate::saver::saver().flush();
        let titles: Vec<String> = AskedList::load(&path).items.into_iter().map(|asked| asked.title).collect();
        assert_eq!(titles, ["Cradlesong", "Parachutes", "Yellow"]);
        assert_eq!(list.trouble, "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
