//! Search that forgives typing mistakes.
//!
//! The server only finds exact words, so the app keeps its own list of every song, album and
//! artist and searches that. It's fetched once, kept on disk, and fetched again when the
//! server says the library has changed. Until the first copy has arrived, searches go to
//! the server as before.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::pages::CardData;
use crate::subsonic::{ApiResult, Client, ScanStatus, Song};

/// How many of each kind to ask the server for at a time.
const PAGE: u32 = 500;
/// A scan that didn't change the number of songs may still have changed tags, but isn't
/// worth fetching everything again for more often than this (seconds).
const RESCAN_AFTER: i64 = 15 * 60;
/// The copy is fetched afresh this often regardless (seconds), to keep play counts current.
const REFRESH_AFTER: i64 = 24 * 3600;

/// The library as fetched from the server.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    /// The server's song count and last scan when this was fetched, and when that was.
    pub count: u64,
    pub last_scan: String,
    pub fetched: i64,
    pub songs: Vec<Song>,
    pub albums: Vec<CardData>,
    pub artists: Vec<CardData>,
}

impl Library {
    /// Whether the server's library has changed since this copy was fetched.
    pub fn is_stale(&self, status: &ScanStatus, now: i64) -> bool {
        if status.scanning {
            // Wait for it to finish; half a library is no use.
            return false;
        }
        status.count != self.count
            || (status.last_scan != self.last_scan && now - self.fetched > RESCAN_AFTER)
            // How often songs have been played moves on without any scan.
            || now - self.fetched > REFRESH_AFTER
    }

    /// Fetches the whole library, a page at a time. The pages go by position, so a song added
    /// or removed while they arrive shifts the rest along: one can fall between two pages, or
    /// come twice. So the server is asked about its library again at the end. The copy is
    /// used either way, but if the server says its library changed meanwhile, or it's
    /// scanning, the copy is stamped with a song count no server has, so `is_stale` sees it
    /// as out of date and it's fetched again at the next look. If the server can't say, the
    /// copy is stamped as it was found at the start.
    pub fn fetch(client: &Client) -> ApiResult<Library> {
        let status = client.scan_status()?;
        let mut library = Library {
            count: status.count,
            last_scan: status.last_scan.clone(),
            fetched: chrono::Utc::now().timestamp(),
            ..Default::default()
        };
        let (mut artists, mut albums, mut songs) = (true, true, true);
        let mut page = 0;
        while artists || albums || songs {
            let offset = page * PAGE;
            let found = client.library_page(
                if artists { PAGE } else { 0 },
                if albums { PAGE } else { 0 },
                if songs { PAGE } else { 0 },
                offset,
            )?;
            artists = artists && found.artists.len() as u32 == PAGE;
            albums = albums && found.albums.len() as u32 == PAGE;
            songs = songs && found.songs.len() as u32 == PAGE;
            library.artists.extend(found.artists.iter().map(CardData::artist));
            library.albums.extend(found.albums.iter().map(CardData::album));
            library.songs.extend(found.songs);
            page += 1;
        }
        // A slow answer doesn't hold up a copy that's ready.
        let changed = client
            .scan_status_within(std::time::Duration::from_secs(5))
            .is_ok_and(|after| after.scanning || after != status);
        if changed {
            library.count = u64::MAX;
        }
        Ok(library)
    }

    pub fn load(path: &Path) -> Option<Library> {
        serde_json::from_slice(&std::fs::read(path).ok()?).ok()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_vec(self) {
            let temp = path.with_extension("tmp");
            if std::fs::write(&temp, json).is_ok() {
                let _ = std::fs::rename(&temp, path);
            }
        }
    }
}

/// Looks at the server's library, and fetches a new copy if `current`, the one in use, is out
/// of date, or there is none. Not while the server is scanning: half a library is no use.
pub fn renew(client: &Client, current: Option<&Library>) -> Option<Library> {
    let status = client.scan_status().ok()?;
    let now = chrono::Utc::now().timestamp();
    let stale = match current {
        Some(library) => library.is_stale(&status, now),
        None => !status.scanning,
    };
    stale.then(|| Library::fetch(client).ok()).flatten()
}

/// The words of a song, as positions in the index's word list.
struct SongWords {
    title: Vec<u32>,
    artist: Vec<u32>,
    album: Vec<u32>,
}

/// A library made ready for searching.
pub struct Index {
    pub library: Library,
    /// Every distinct word in the library.
    words: Vec<Vec<char>>,
    songs: Vec<SongWords>,
    /// Each album's name and artist.
    albums: Vec<(Vec<u32>, Vec<u32>)>,
    artists: Vec<Vec<u32>>,
}

#[derive(Default)]
pub struct Results {
    pub artists: Vec<CardData>,
    pub albums: Vec<CardData>,
    pub songs: Vec<Song>,
    /// An artist matches what was typed at least as well as any song's title does.
    pub artist_first: bool,
}

// How names are folded and how a match is scored are the library's; going through this
// PC's copy of the library with them is below.
pub use nexus_core::search::slips;
use nexus_core::search::{likeness, rate};
pub use nexus_core::text::fold;

impl Index {
    pub fn new(library: Library) -> Index {
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut words: Vec<Vec<char>> = Vec::new();
        let mut split = |text: &str| -> Vec<u32> {
            fold(text)
                .split_whitespace()
                .map(|word| {
                    *ids.entry(word.to_string()).or_insert_with(|| {
                        words.push(word.chars().collect());
                        (words.len() - 1) as u32
                    })
                })
                .collect()
        };
        let songs = library
            .songs
            .iter()
            .map(|song| SongWords { title: split(&song.title), artist: split(song.artist_label()), album: split(&song.album) })
            .collect();
        let albums = library.albums.iter().map(|album| (split(&album.title), split(&album.subtitle))).collect();
        let artists = library.artists.iter().map(|artist| split(&artist.title)).collect();
        Index { library, words, songs, albums, artists }
    }

    /// Finds what matches `query`, best first. `liked` says whether a song is liked: those
    /// come ahead of others that match as well.
    pub fn search(&self, query: &str, liked: &dyn Fn(&Song) -> bool, artists: usize, albums: usize, songs: usize) -> Results {
        let typed: Vec<Vec<char>> = fold(query).split_whitespace().map(|word| word.chars().collect()).collect();
        if typed.is_empty() {
            return Results::default();
        }
        // Compare each typed word with each distinct word once, however many songs use it.
        let scores: Vec<Vec<f32>> =
            typed.iter().map(|typed| self.words.iter().map(|word| likeness(typed, word)).collect()).collect();

        fn best<T>(mut found: Vec<(f32, usize)>, keep: usize, pick: impl Fn(usize) -> T) -> Vec<T> {
            // Equal matches stay in the library's own order.
            found.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            found.into_iter().take(keep).map(|(_, index)| pick(index)).collect()
        }

        let mut top_title = 0.0f32;
        let mut found = Vec::new();
        for (index, words) in self.songs.iter().enumerate() {
            let Some(rated) = rate(&scores, &words.title, &[(&words.artist, 0.9), (&words.album, 0.8)]) else { continue };
            top_title = top_title.max(rated.name);
            let liked = if liked(&self.library.songs[index]) { 0.25 } else { 0.0 };
            found.push((rated.score + liked, index));
        }
        let song_results = best(found, songs, |index| self.library.songs[index].clone());

        let mut top_artist = 0.0f32;
        let mut found = Vec::new();
        for (index, words) in self.artists.iter().enumerate() {
            let Some(rated) = rate(&scores, words, &[]) else { continue };
            top_artist = top_artist.max(rated.score);
            found.push((rated.score, index));
        }
        let artist_results = best(found, artists, |index| self.library.artists[index].clone());

        let mut found = Vec::new();
        for (index, (name, artist)) in self.albums.iter().enumerate() {
            let Some(rated) = rate(&scores, name, &[(artist, 0.85)]) else { continue };
            found.push((rated.score, index));
        }
        let album_results = best(found, albums, |index| self.library.albums[index].clone());

        Results {
            artist_first: top_artist > 0.0 && top_artist >= top_title,
            artists: artist_results,
            albums: album_results,
            songs: song_results,
        }
    }
}

// ---- the copy in use -----------------------------------------------------------------------------

static CURRENT: RwLock<Option<Arc<Index>>> = RwLock::new(None);
/// Songs that are liked: the library's own word on it goes out of date as hearts are clicked.
static LIKED: RwLock<Option<HashSet<String>>> = RwLock::new(None);

/// The library as the requests pages ask it (see `nexus_core::requests::Library`), over the
/// index as it is: nothing is copied or rebuilt to answer.
impl nexus_core::requests::Library for Index {
    fn albums(&self) -> Box<dyn Iterator<Item = nexus_core::requests::AlbumRef<'_>> + '_> {
        Box::new(self.library.albums.iter().map(|album| nexus_core::requests::AlbumRef { id: &album.id, title: &album.title, artist: &album.subtitle, cover: &album.cover }))
    }

    fn songs(&self) -> Box<dyn Iterator<Item = nexus_core::requests::SongRef<'_>> + '_> {
        Box::new(self.library.songs.iter().map(|song| nexus_core::requests::SongRef { title: &song.title, artist: song.artist_label() }))
    }

    fn artists(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        Box::new(self.library.artists.iter().map(|artist| artist.title.as_str()))
    }
}

pub fn current() -> Option<Arc<Index>> {
    CURRENT.read().ok()?.clone()
}

pub fn set_current(index: Option<Index>) {
    if let Ok(mut current) = CURRENT.write() {
        if index.is_none() {
            if let Ok(mut liked) = LIKED.write() {
                *liked = None;
            }
        }
        *current = index.map(Arc::new);
    }
}

pub fn set_liked(ids: HashSet<String>) {
    if let Ok(mut liked) = LIKED.write() {
        *liked = Some(ids);
    }
}

/// Notes a song being liked or unliked here.
pub fn note_liked(id: &str, liked: bool) {
    if let Ok(mut all) = LIKED.write() {
        if let Some(all) = all.as_mut() {
            if liked {
                all.insert(id.to_string());
            } else {
                all.remove(id);
            }
        }
    }
}

pub fn is_liked(song: &Song) -> bool {
    match LIKED.read().ok().as_ref().and_then(|liked| liked.as_ref()) {
        Some(liked) => liked.contains(&song.id),
        None => song.starred.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::Kind;

    /// A pretend server's library: its songs, scan stamp and whether it's scanning; whether
    /// it fails to say how its library is; and where each page of songs asked for started.
    struct Shelf {
        songs: Vec<String>,
        last_scan: String,
        scanning: bool,
        silent: bool,
        pages: Vec<u32>,
    }

    /// A server holding `songs` songs. `meanwhile` is called after each page of songs is sent,
    /// with where that page started, to change the library as a scan would.
    fn library_server(songs: usize, meanwhile: impl Fn(&mut Shelf, u32) + Send + Sync + 'static) -> (Client, Arc<std::sync::Mutex<Shelf>>) {
        use crate::subsonic::fake;
        let songs = (0..songs).map(|n| format!("s{n}")).collect();
        let shelf = Arc::new(std::sync::Mutex::new(Shelf { songs, last_scan: "1".into(), scanning: false, silent: false, pages: Vec::new() }));
        let served = shelf.clone();
        let address = fake::server(move |target, _, stream| {
            let number = |name: &str| {
                let at = target.find(&format!("&{name}=")).map(|at| at + name.len() + 2);
                at.map_or(0, |at| target[at..].split('&').next().unwrap().parse::<usize>().unwrap())
            };
            let mut shelf = served.lock().unwrap();
            if target.starts_with("/rest/getScanStatus") && shelf.silent {
                let _ = std::io::Write::write_all(stream, b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            } else if target.starts_with("/rest/getScanStatus") {
                let status = format!(r#""scanStatus":{{"scanning":{},"count":{},"lastScan":"{}"}}"#, shelf.scanning, shelf.songs.len(), shelf.last_scan);
                fake::ok(stream, &status);
            } else if target.starts_with("/rest/search3") {
                let (offset, count) = (number("songOffset"), number("songCount"));
                let page: Vec<String> = shelf.songs.iter().skip(offset).take(count).map(|id| format!(r#"{{"id":"{id}","title":"Song {id}"}}"#)).collect();
                fake::ok(stream, &format!(r#""searchResult3":{{"song":[{}]}}"#, page.join(",")));
                if count > 0 {
                    shelf.pages.push(offset as u32);
                    meanwhile(&mut shelf, offset as u32);
                }
            } else {
                fake::ok(stream, "");
            }
        });
        (Client::new(&address, "someone", "secret"), shelf)
    }

    fn ids(library: &Library) -> Vec<String> {
        library.songs.iter().map(|song| song.id.clone()).collect()
    }

    #[test]
    fn a_copy_taken_while_the_library_changed_is_used_and_fetched_again_at_the_next_look() {
        // After the first page, a scan adds a song at the start and finds one at the end gone:
        // the same number of songs, but everything moves along one, so the last song of the
        // first page comes again on the second and the new one is missed.
        let (client, shelf) = library_server(700, |shelf, offset| {
            if offset == 0 && shelf.last_scan == "1" {
                shelf.songs.insert(0, "new".into());
                shelf.songs.pop();
                shelf.last_scan = "2".into();
            }
        });
        let now = chrono::Utc::now().timestamp();
        let first = renew(&client, None).expect("the copy is used");
        assert_eq!(first.songs.len(), 700);
        assert!(first.is_stale(&client.scan_status().unwrap(), now), "marked as out of date");
        // The next look fetches it again, and that copy holds.
        let second = renew(&client, Some(&first)).expect("fetched again");
        assert_eq!(ids(&second), shelf.lock().unwrap().songs);
        assert_eq!((second.count, second.last_scan.as_str()), (700, "2"));
        assert!(renew(&client, Some(&second)).is_none());
        assert_eq!(shelf.lock().unwrap().pages, [0, 500, 0, 500]);
    }

    #[test]
    fn a_copy_the_server_cant_say_about_afterwards_is_stamped_as_usual() {
        // Once the songs are sent, the server stops answering how its library is.
        let (client, shelf) = library_server(700, |shelf, offset| shelf.silent = offset == 500);
        let library = renew(&client, None).expect("the copy is used");
        assert_eq!(ids(&library), shelf.lock().unwrap().songs);
        assert_eq!((library.count, library.last_scan.as_str()), (700, "1"));
        // It answers again, and the copy isn't fetched again.
        shelf.lock().unwrap().silent = false;
        assert!(renew(&client, Some(&library)).is_none());
        assert_eq!(shelf.lock().unwrap().pages, [0, 500]);
    }

    #[test]
    fn a_quiet_library_is_fetched_once() {
        let (client, shelf) = library_server(700, |_, _| {});
        let library = renew(&client, None).expect("fetched");
        assert_eq!(ids(&library), shelf.lock().unwrap().songs);
        assert_eq!((library.count, library.last_scan.as_str()), (700, "1"));
        assert!(renew(&client, Some(&library)).is_none());
        assert_eq!(shelf.lock().unwrap().pages, [0, 500]);
    }

    #[test]
    fn nothing_is_fetched_during_a_scan() {
        let (client, shelf) = library_server(10, |_, _| {});
        shelf.lock().unwrap().scanning = true;
        let out_of_date = Library { count: u64::MAX, ..Default::default() };
        assert!(renew(&client, None).is_none());
        assert!(renew(&client, Some(&out_of_date)).is_none());
        assert!(shelf.lock().unwrap().pages.is_empty());
        // Once it's over, the copy comes.
        shelf.lock().unwrap().scanning = false;
        assert_eq!(renew(&client, Some(&out_of_date)).expect("fetched").songs.len(), 10);
    }

    fn song(id: &str, title: &str, artist: &str, album: &str) -> Song {
        Song { id: id.into(), title: title.into(), artist: artist.into(), album: album.into(), ..Default::default() }
    }

    fn card(kind: Kind, title: &str, subtitle: &str) -> CardData {
        CardData { kind, id: title.into(), title: title.into(), subtitle: subtitle.into(), cover: String::new() }
    }

    fn library() -> Index {
        Index::new(Library {
            songs: vec![
                song("1", "Psycho", "Maisie Peters", "You Signed Up for This"),
                song("2", "Sweet but Psycho", "Ava Max", "Heaven & Hell"),
                song("3", "Psychosocial", "Slipknot", "All Hope Is Gone"),
                song("4", "Psycho", "Taylor Acorn", "Psycho"),
                song("5", "Lost the Breakup", "Maisie Peters", "The Good Witch"),
                song("6", "Déjà Vu", "Beyoncé", "B'Day"),
                song("7", "Don't Stop Me Now", "Queen", "Jazz"),
                song("8", "Masterpiece", "Madonna", "MDNA"),
            ],
            albums: vec![
                card(Kind::Album, "The Good Witch", "Maisie Peters"),
                card(Kind::Album, "Psycho", "Taylor Acorn"),
                card(Kind::Album, "Jazz", "Queen"),
            ],
            artists: vec![
                card(Kind::Artist, "Maisie Peters", ""),
                card(Kind::Artist, "Ava Max", ""),
                card(Kind::Artist, "Beyoncé", ""),
                card(Kind::Artist, "Queen", ""),
            ],
            ..Default::default()
        })
    }

    fn titles(songs: &[Song]) -> Vec<String> {
        songs.iter().map(|song| format!("{} / {}", song.title, song.artist)).collect()
    }

    #[test]
    fn short_words_only_match_words_that_start_the_same_way() {
        let chars = |text: &str| text.chars().collect::<Vec<char>>();
        let like = |typed: &str, word: &str| likeness(&chars(typed), &chars(word));
        // One letter out, and it's the first: a different word altogether.
        assert_eq!(like("home", "someday"), 0.0);
        assert_eq!(like("home", "come"), 0.0);
        assert_eq!(like("lost", "most"), 0.0);
        // A slip later in the word is still a slip.
        assert!(like("masi", "maisie") > 0.0);
        assert!(like("quen", "queen") > 0.0);
        assert!(like("jaz", "jazz") > 0.8);
        // Longer words are forgiven a wrong first letter, as before.
        assert!(like("saisie", "maisie") > 0.0);
        // And through the whole search: "tazz" isn't taken for "Jazz", "masi" still finds Maisie.
        let found = library().search("tazz", &|_| false, 5, 5, 5);
        assert!(found.songs.is_empty() && found.albums.is_empty(), "{:?}", titles(&found.songs));
        let found = library().search("masi", &|_| false, 5, 5, 5);
        assert_eq!(found.artists.first().map(|artist| artist.title.as_str()), Some("Maisie Peters"));
    }

    #[test]
    fn forgives_a_typo_and_puts_the_artist_first() {
        let index = library();
        let none = |_: &Song| false;
        // The misspelling from the bug report.
        let found = index.search("masie", &none, 5, 5, 10);
        assert_eq!(found.artists[0].title, "Maisie Peters");
        assert!(found.artist_first);
        // Her songs come before one whose title merely starts something like it.
        assert_eq!(
            titles(&found.songs),
            ["Psycho / Maisie Peters", "Lost the Breakup / Maisie Peters", "Masterpiece / Madonna"]
        );
        assert_eq!(found.albums[0].title, "The Good Witch");
        // Still being typed, and already wrong.
        assert_eq!(index.search("masi", &none, 5, 5, 10).artists[0].title, "Maisie Peters");
        // Accents and apostrophes needn't be typed.
        assert_eq!(index.search("beyonce", &none, 5, 5, 10).artists[0].title, "Beyoncé");
        assert_eq!(index.search("dont stop", &none, 5, 5, 10).songs[0].title, "Don't Stop Me Now");
        // Short words have to be right: "mas" is the start of "Masterpiece", not a typo for "Max".
        assert_eq!(titles(&index.search("mas", &none, 5, 5, 10).songs), ["Masterpiece / Madonna"]);
        assert!(index.search("zzzz", &none, 5, 5, 10).songs.is_empty());
    }

    #[test]
    fn ranks_closer_titles_and_liked_songs_first() {
        let index = library();
        let found = index.search("psycho", &|_: &Song| false, 5, 5, 10);
        // A song called exactly that beats ones that merely contain or start with it.
        assert_eq!(
            titles(&found.songs),
            ["Psycho / Maisie Peters", "Psycho / Taylor Acorn", "Psychosocial / Slipknot", "Sweet but Psycho / Ava Max"]
        );
        // No artist is called anything like it, so songs lead.
        assert!(!found.artist_first);
        // Of equal matches, the liked one comes first.
        let found = index.search("psycho", &|song: &Song| song.id == "4", 5, 5, 10);
        assert_eq!(found.songs[0].artist, "Taylor Acorn");
        // Words can come from the title and the artist together.
        let found = index.search("psycho maisie", &|_: &Song| false, 5, 5, 10);
        assert_eq!(titles(&found.songs), ["Psycho / Maisie Peters"]);
    }
}

// ---- playlists ------------------------------------------------------------------------------------

/// The playlists, as cards, for a search to look through. Kept up to date by whatever lists
/// them in the sidebar.
static PLAYLISTS: std::sync::RwLock<Vec<CardData>> = std::sync::RwLock::new(Vec::new());

pub fn set_playlists(playlists: Vec<CardData>) {
    if let Ok(mut all) = PLAYLISTS.write() {
        *all = playlists;
    }
}

/// The playlists whose name has every word that was typed, or the start of it.
pub fn playlists_matching(query: &str) -> Vec<CardData> {
    let typed = crate::musicbrainz::words(query);
    if typed.is_empty() {
        return Vec::new();
    }
    let Ok(all) = PLAYLISTS.read() else { return Vec::new() };
    all.iter()
        .filter(|playlist| {
            let name = crate::musicbrainz::words(&playlist.title);
            typed.iter().all(|word| name.iter().any(|part| part.starts_with(word.as_str())))
        })
        .cloned()
        .collect()
}
