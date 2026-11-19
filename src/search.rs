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

    pub fn fetch(client: &Client) -> ApiResult<Library> {
        let status = client.scan_status()?;
        let mut library = Library {
            count: status.count,
            last_scan: status.last_scan,
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

/// Lower-case, without accents or punctuation, so "Beyoncé" and "beyonce" are the same.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'ç' | 'ć' | 'č' => out.push('c'),
            'ď' | 'đ' => out.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => out.push('e'),
            'ğ' => out.push('g'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => out.push('i'),
            'ł' => out.push('l'),
            'ñ' | 'ń' | 'ň' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => out.push('o'),
            'ř' => out.push('r'),
            'ś' | 'š' | 'ş' => out.push('s'),
            'ť' | 'ţ' => out.push('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'ź' | 'ż' | 'ž' => out.push('z'),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ß' => out.push_str("ss"),
            // "don't" and "dont" are the same word.
            '\'' | '’' | '‘' | '`' => {}
            c if c.is_alphanumeric() => out.push(c),
            _ => out.push(' '),
        }
    }
    out
}

/// How many single-letter slips (a letter wrong, missing, extra, or two swapped) separate
/// two words; anything over `limit` comes back as `limit + 1`.
pub fn slips(a: &[char], b: &[char], limit: usize) -> usize {
    if a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }
    let mut before: Vec<usize> = vec![0; b.len() + 1];
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        let mut least = current[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (previous[j] + 1).min(current[j - 1] + 1).min(previous[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(before[j - 2] + 1);
            }
            current[j] = best;
            least = least.min(best);
        }
        if least > limit {
            return limit + 1;
        }
        std::mem::swap(&mut before, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()].min(limit + 1)
}

/// How well a typed word matches a word from the library, from 0 (not at all) to 1 (the
/// same word). The start of a word counts nearly as much, since the rest may yet be typed;
/// a slip or two is forgiven in words long enough for that not to match everything.
fn likeness(typed: &[char], word: &[char]) -> f32 {
    if typed == word {
        return 1.0;
    }
    if word.starts_with(typed) {
        return 0.85 + 0.1 * typed.len() as f32 / word.len() as f32;
    }
    let limit = match typed.len() {
        0..=3 => return 0.0,
        4..=7 => 1,
        _ => 2,
    };
    // A short word is only taken for a mistyping of one that starts the same way. Without
    // that, four or five letters are near enough to a good many words that have nothing to
    // do with them: "home" would find "Someday".
    if typed.len() <= 5 && typed.first() != word.first() {
        return 0.0;
    }
    let whole = slips(typed, word, limit);
    if whole <= limit {
        return if whole == 1 { 0.75 } else { 0.6 };
    }
    // A mistyped start of a longer word: "masi" for "maisie".
    let mut best = limit + 1;
    for length in typed.len().saturating_sub(1)..=(typed.len() + 1).min(word.len().saturating_sub(1)) {
        best = best.min(slips(typed, &word[..length], limit));
    }
    // Worth much less than a whole word that's nearly right: it's a guess on a guess.
    match best {
        1 => 0.4,
        2 if limit == 2 => 0.3,
        _ => 0.0,
    }
}

/// One thing's match against what was typed.
#[derive(Default, Clone, Copy)]
struct Match {
    /// Overall, counting every field.
    score: f32,
    /// Counting the name (or title) alone; 0 if some typed word isn't in it.
    name: f32,
}

/// Scores something whose name is `name` and which also answers to `others` (an artist's or
/// album's words, each with how much they count), against the typed words. `scores[i][w]`
/// is how well typed word `i` matches library word `w`.
fn rate(scores: &[Vec<f32>], name: &[u32], others: &[(&[u32], f32)]) -> Option<Match> {
    let mut total = 0.0;
    let mut name_total = 0.0;
    let mut in_name = true;
    // Which of the name's words were what a typed word matched best.
    let mut used = 0u64;
    for typed in scores {
        let (mut best_name, mut at) = (0.0f32, usize::MAX);
        for (position, &word) in name.iter().enumerate() {
            if typed[word as usize] > best_name {
                best_name = typed[word as usize];
                at = position;
            }
        }
        let mut best = best_name;
        for (words, weight) in others {
            for &word in *words {
                best = best.max(typed[word as usize] * weight);
            }
        }
        if best <= 0.0 {
            return None;
        }
        total += best;
        if best_name > 0.0 {
            name_total += best_name;
            if best_name >= best && at < 64 {
                used |= 1 << at;
            }
        } else {
            in_name = false;
        }
    }
    let typed = scores.len() as f32;
    // Bonuses for the typed words making up more of the name: all of it, or its start.
    let covered = if name.is_empty() { 0.0 } else { used.count_ones() as f32 / name.len() as f32 };
    let in_order = name.len() >= scores.len() && scores.iter().zip(name).all(|(typed, &word)| typed[word as usize] >= 0.85);
    let exact = in_order && name.len() == scores.len() && scores.iter().zip(name).all(|(typed, &word)| typed[word as usize] >= 1.0);
    let bonus = 0.3 * covered + if exact { 0.5 } else { 0.0 } + if in_order { 0.2 } else { 0.0 };
    // ...which counts for less the shakier the match with the name is.
    let bonus = bonus * name_total / typed;
    Some(Match {
        score: total / typed + bonus,
        name: if in_name { name_total / typed + bonus } else { 0.0 },
    })
}

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
    fn folds_accents_and_punctuation() {
        assert_eq!(fold("Beyoncé — Déjà Vu!"), "beyonce   deja vu ");
        assert_eq!(fold("Don’t Stop"), "dont stop");
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
    fn counts_slips() {
        let chars = |text: &str| text.chars().collect::<Vec<char>>();
        assert_eq!(slips(&chars("masie"), &chars("maisie"), 2), 1);
        assert_eq!(slips(&chars("pyscho"), &chars("psycho"), 2), 1);
        assert_eq!(slips(&chars("queen"), &chars("queen"), 1), 0);
        assert_eq!(slips(&chars("abc"), &chars("xyz"), 1), 2);
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
