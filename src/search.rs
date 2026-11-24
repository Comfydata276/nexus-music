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
/// How many times one look at the library fetches it, while the library keeps changing.
const FETCH_TRIES: usize = 3;
/// After a copy that couldn't be trusted, how long until the next try (seconds); it doubles
/// with each one after, up to the most.
const RETRY_AFTER: i64 = 5 * 60;
const RETRY_AFTER_MOST: i64 = 3600;
/// More pages than this of one kind (two million songs) is a server that never says it's done.
const MOST_PAGES: u32 = 4000;

fn trusted() -> bool {
    true
}

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
    /// The copy was checked against the server once it had arrived, and held together. One
    /// that wasn't is used only when there's no other, and replaced by one that is as soon as
    /// the server allows. (Copies kept before this was noted were trusted.)
    #[serde(default = "trusted")]
    pub verified: bool,
    /// The server kept sending pages, the same one over and over or more than any library
    /// has, and they weren't all read.
    #[serde(skip)]
    pub cut_short: bool,
}

impl Library {
    /// Whether the server's library has changed since this copy was fetched, or this copy
    /// couldn't be trusted and the server may now give one that can.
    pub fn is_stale(&self, status: &ScanStatus, now: i64) -> bool {
        if status.scanning {
            // Wait for it to finish; half a library is no use.
            return false;
        }
        !self.verified
            // A server that gives no count can't be checked by it.
            || status.count.is_some_and(|count| count != self.count)
            || (status.last_scan != self.last_scan && now - self.fetched > RESCAN_AFTER)
            // How often songs have been played moves on without any scan.
            || now - self.fetched > REFRESH_AFTER
    }

    /// Fetches the whole library afresh. The server hands it over a page at a time, by
    /// position, so a song added or removed while that's under way shifts the rest along: one
    /// can then fall between two pages, or come twice. What the server says about its library
    /// is asked again at the end, and a copy taken while it changed isn't trusted; it's
    /// fetched again, a few times at most. A scan under way means waiting for it to finish:
    /// half a library is no use.
    ///
    /// `any` says there's no copy in use at all. Then one that couldn't be trusted is better
    /// than none: it's fetched even during a scan, and given back marked as not verified.
    /// Otherwise only a verified copy is given back, and the one in use stays.
    pub fn fetch(client: &Client, any: bool) -> ApiResult<Fetched> {
        // Whether the server's song count is the number of songs, to check the copy against.
        let counts_songs = client.ping().is_ok_and(|info| info.counts_songs());
        let mut status = client.scan_status()?;
        let mut untrusted = None;
        for _ in 0..FETCH_TRIES {
            if status.scanning && !any {
                break;
            }
            let mut library = Self::fetch_pages(client, &status)?;
            let after = client.scan_status()?;
            let unchanged = !status.scanning
                && !after.scanning
                && after.count == status.count
                && after.last_scan == status.last_scan;
            // No count means no check by count.
            let count = after.count.filter(|_| counts_songs);
            if unchanged && library.is_whole(count) {
                library.verified = true;
                return Ok(Fetched { library: Some(library), last_scan: after.last_scan });
            }
            let scanning = after.scanning;
            status = after;
            untrusted = Some(library);
            // A scan under way won't be over by the next try.
            if scanning {
                break;
            }
        }
        let library = untrusted.filter(|_| any).map(|mut library| {
            library.verified = false;
            library.without_repeats();
            library
        });
        Ok(Fetched { library, last_scan: status.last_scan })
    }

    /// Every page of the library, stamped with `status`, read before the first.
    fn fetch_pages(client: &Client, status: &ScanStatus) -> ApiResult<Library> {
        let mut library = Library {
            count: status.count.unwrap_or(0),
            last_scan: status.last_scan.clone(),
            fetched: chrono::Utc::now().timestamp(),
            ..Default::default()
        };
        let (mut artists, mut albums, mut songs) = (true, true, true);
        // The first of each kind on the page before: a server that takes no notice of where
        // to start sends the same page again, for ever.
        let mut before: [Option<String>; 3] = Default::default();
        let mut page = 0;
        while artists || albums || songs {
            if page == MOST_PAGES {
                library.cut_short = true;
                break;
            }
            let offset = page * PAGE;
            let found = client.library_page(
                if artists { PAGE } else { 0 },
                if albums { PAGE } else { 0 },
                if songs { PAGE } else { 0 },
                offset,
            )?;
            let first = [
                found.artists.first().map(|a| a.id.clone()),
                found.albums.first().map(|a| a.id.clone()),
                found.songs.first().map(|s| s.id.clone()),
            ];
            if page > 0 && first.iter().zip(&before).any(|(now, then)| now.is_some() && now == then) {
                library.cut_short = true;
                break;
            }
            before = first;
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

    /// Whether a copy holds together: every page read, no song in it twice, which is what a
    /// song added while the pages were fetched looks like, and no more songs than `count`,
    /// the server's song count where it is one. Fewer is no sign of a fault: the count takes
    /// in every library on the server, and an account may be let into only some of them.
    fn is_whole(&self, count: Option<u64>) -> bool {
        let mut seen = HashSet::with_capacity(self.songs.len());
        !self.cut_short
            && self.songs.iter().all(|song| seen.insert(song.id.as_str()))
            && count.is_none_or(|count| self.songs.len() as u64 <= count)
    }

    /// Leaves out anything that came twice, as it can in a copy that wasn't trusted.
    fn without_repeats(&mut self) {
        let mut seen = HashSet::new();
        self.songs.retain(|song| seen.insert(song.id.clone()));
        for cards in [&mut self.albums, &mut self.artists] {
            let mut seen = HashSet::new();
            cards.retain(|card| seen.insert(card.id.clone()));
        }
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

/// What `Library::fetch` came back with.
pub struct Fetched {
    /// The copy to use, if there's one to use.
    pub library: Option<Library>,
    /// The server's scan stamp when it was last asked.
    pub last_scan: String,
}

/// When a copy may be fetched again after one that couldn't be trusted: not every few minutes,
/// which on a big library is a lot of fetching for nothing, but after a wait that grows each
/// time, unless the server has scanned its library since.
#[derive(Debug, Default)]
pub struct Retry {
    /// Seconds waited after the last refusal; 0 when there hasn't been one.
    wait: i64,
    until: i64,
    /// The server's scan stamp at that refusal.
    last_scan: String,
}

impl Retry {
    pub const fn new() -> Retry {
        Retry { wait: 0, until: 0, last_scan: String::new() }
    }

    pub fn allows(&self, status: &ScanStatus, now: i64) -> bool {
        self.wait == 0 || now >= self.until || status.last_scan != self.last_scan
    }

    fn refused(&mut self, last_scan: &str, now: i64) {
        self.wait = if self.wait == 0 { RETRY_AFTER } else { (self.wait * 2).min(RETRY_AFTER_MOST) };
        self.until = now + self.wait;
        self.last_scan = last_scan.to_string();
    }
}

/// Waits after copies that couldn't be trusted, for the server signed in to.
pub static RETRY: std::sync::Mutex<Retry> = std::sync::Mutex::new(Retry::new());

/// Looks at the server's library and fetches a new copy if `current`, the one in use, is out
/// of date (or there is none), as far as `retry` allows. Gives back the copy to use instead,
/// if there is one.
pub fn renew(client: &Client, current: Option<&Library>, retry: &mut Retry, now: i64) -> ApiResult<Option<Library>> {
    let status = client.scan_status()?;
    let stale = current.is_none_or(|library| library.is_stale(&status, now));
    if !stale || !retry.allows(&status, now) {
        return Ok(None);
    }
    let fetched = Library::fetch(client, current.is_none())?;
    match &fetched.library {
        Some(library) if library.verified => *retry = Retry::new(),
        _ => retry.refused(&fetched.last_scan, now),
    }
    Ok(fetched.library)
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

    /// A library on a pretend server, as `Library::fetch` sees it.
    struct Shelf {
        songs: Vec<String>,
        last_scan: String,
        scanning: bool,
        /// The server says how many songs it has.
        counts: bool,
        /// The server takes no notice of where a page is to start.
        ignores_offset: bool,
        /// Pages of songs asked for, by where each started.
        pages: Vec<u32>,
    }

    /// A pretend server holding `shelf`. `meanwhile` is called after each page of songs is
    /// sent, with where that page started, to change the library as a scan would.
    fn library_server(
        shelf: Arc<std::sync::Mutex<Shelf>>,
        meanwhile: impl Fn(&mut Shelf, u32) + Send + Sync + 'static,
    ) -> Client {
        use crate::subsonic::fake;
        let address = fake::server(move |target, stream| {
            let number = |name: &str| {
                let at = target.find(&format!("&{name}=")).map(|at| at + name.len() + 2);
                at.map_or(0, |at| target[at..].split('&').next().unwrap().parse::<u32>().unwrap())
            };
            let mut shelf = shelf.lock().unwrap();
            if target.starts_with("/rest/getScanStatus") {
                let count = if shelf.counts { format!(r#","count":{}"#, shelf.songs.len()) } else { String::new() };
                let fields = format!(r#""scanStatus":{{"scanning":{}{count},"lastScan":"{}"}}"#, shelf.scanning, shelf.last_scan);
                fake::ok(stream, &fields);
            } else if target.starts_with("/rest/search3") {
                let (offset, count) = (number("songOffset"), number("songCount"));
                let offset = if shelf.ignores_offset { 0 } else { offset };
                let page: Vec<String> = shelf
                    .songs
                    .iter()
                    .skip(offset as usize)
                    .take(count as usize)
                    .map(|id| format!(r#"{{"id":"{id}","title":"Song {id}"}}"#))
                    .collect();
                fake::ok(stream, &format!(r#""searchResult3":{{"song":[{}]}}"#, page.join(",")));
                if count > 0 {
                    shelf.pages.push(offset);
                    meanwhile(&mut shelf, offset);
                }
            } else {
                fake::ok(stream, "");
            }
        });
        Client::new(&address, "someone", "secret")
    }

    fn shelf_of(ids: &[&str]) -> Arc<std::sync::Mutex<Shelf>> {
        let shelf = shelf(0);
        shelf.lock().unwrap().songs = ids.iter().map(|id| id.to_string()).collect();
        shelf
    }

    fn shelf(songs: usize) -> Arc<std::sync::Mutex<Shelf>> {
        let songs = (0..songs).map(|n| format!("s{n}")).collect();
        Arc::new(std::sync::Mutex::new(Shelf { songs, last_scan: "1".into(), scanning: false, counts: true, ignores_offset: false, pages: Vec::new() }))
    }

    fn ids(library: &Library) -> Vec<String> {
        library.songs.iter().map(|song| song.id.clone()).collect()
    }

    #[test]
    fn a_song_removed_while_the_pages_arrive_is_not_lost_from_the_copy() {
        let shelf = shelf(1200);
        // After the first page, a scan finds an early song gone: everything after it moves
        // up one, and the song that was first on the second page would be skipped.
        let client = library_server(shelf.clone(), |shelf, offset| {
            if offset == 0 && shelf.last_scan == "1" {
                shelf.songs.remove(3);
                shelf.last_scan = "2".into();
            }
        });
        let library = Library::fetch(&client, false).unwrap().library.expect("fetched again, and trusted");
        let shelf = shelf.lock().unwrap();
        assert_eq!(ids(&library), shelf.songs);
        assert!(library.songs.iter().any(|song| song.id == "s500"));
        assert_eq!((library.count, library.last_scan.as_str()), (1199, "2"));
        // The first go, and a whole second one.
        assert_eq!(shelf.pages, [0, 500, 1000, 0, 500, 1000]);
    }

    #[test]
    fn a_song_that_comes_twice_is_caught_even_if_the_server_says_nothing_changed() {
        let shelf = shelf(1200);
        // One added at the start and one taken from the end: the same count, and no scan
        // to say so, but the last song of the first page comes again on the second.
        let client = library_server(shelf.clone(), |shelf, offset| {
            if offset == 0 && shelf.pages.len() == 1 {
                shelf.songs.insert(0, "new".into());
                shelf.songs.pop();
            }
        });
        let library = Library::fetch(&client, false).unwrap().library.expect("fetched again, and trusted");
        assert_eq!(ids(&library), shelf.lock().unwrap().songs);
        assert_eq!(shelf.lock().unwrap().pages.len(), 6);
    }

    #[test]
    fn a_library_that_keeps_changing_leaves_the_copy_in_use_alone() {
        let shelf = shelf(700);
        let client = library_server(shelf.clone(), |shelf, offset| {
            if offset == 0 {
                shelf.songs.push(format!("added{}", shelf.pages.len()));
                shelf.last_scan = format!("{}", shelf.pages.len());
            }
        });
        assert!(Library::fetch(&client, false).unwrap().library.is_none());
        // A few goes, then it waits for the next look at the library.
        assert_eq!(shelf.lock().unwrap().pages.iter().filter(|&&offset| offset == 0).count(), FETCH_TRIES);
    }

    #[test]
    fn waits_for_a_scan_to_finish() {
        let shelf = shelf(10);
        shelf.lock().unwrap().scanning = true;
        let client = library_server(shelf.clone(), |_, _| {});
        assert!(Library::fetch(&client, false).unwrap().library.is_none());
        assert!(shelf.lock().unwrap().pages.is_empty());
        // Once it has, the copy comes.
        shelf.lock().unwrap().scanning = false;
        let library = Library::fetch(&client, false).unwrap().library.unwrap();
        assert_eq!(library.songs.len(), 10);
        assert!(library.verified);
    }

    /// A copy in use, of a library that has since changed.
    fn old_copy() -> Library {
        Library { count: 1, last_scan: "0".into(), verified: true, ..Default::default() }
    }

    #[test]
    fn a_server_that_gives_no_count_is_not_checked_by_one() {
        let shelf = shelf(10);
        shelf.lock().unwrap().counts = false;
        let client = library_server(shelf.clone(), |_, _| {});
        let library = renew(&client, None, &mut Retry::new(), 0).unwrap().expect("a copy");
        assert!(library.verified);
        assert_eq!(library.songs.len(), 10);
        // And no count isn't taken for a change in it.
        assert!(renew(&client, Some(&library), &mut Retry::new(), 60).unwrap().is_none());
    }

    #[test]
    fn after_a_copy_that_couldnt_be_trusted_it_waits_longer_each_time() {
        let shelf = shelf(20);
        // A song comes in at the front during every fetch, with no new scan stamp.
        let client = library_server(shelf.clone(), |shelf, offset| {
            if offset == 0 {
                shelf.songs.insert(0, format!("new{}", shelf.pages.len()));
            }
        });
        let fetches = || shelf.lock().unwrap().pages.len();
        let (mut retry, current) = (Retry::new(), old_copy());
        assert!(renew(&client, Some(&current), &mut retry, 0).unwrap().is_none());
        assert_eq!(fetches(), FETCH_TRIES);
        // Each look at the library in the meantime fetches nothing: five minutes, then ten,
        // twenty, forty, and an hour at most from then on.
        let mut now = 0;
        for wait in [300, 600, 1200, 2400, 3600, 3600] {
            let before = fetches();
            for _ in 0..4 {
                assert!(renew(&client, Some(&current), &mut retry, now + wait - 60).unwrap().is_none());
            }
            assert_eq!(fetches(), before, "within {wait} seconds");
            now += wait;
            assert!(renew(&client, Some(&current), &mut retry, now).unwrap().is_none());
            assert_eq!(fetches(), before + FETCH_TRIES, "after {wait} seconds");
        }
        // A new scan of the library is worth a try straight away; once it's trusted, the
        // copy in use is replaced and the waits start again.
        let mut calm = shelf.lock().unwrap();
        calm.last_scan = "9".into();
        drop(calm);
        let client = library_server(shelf.clone(), |_, _| {});
        let library = renew(&client, Some(&current), &mut retry, now + 1).unwrap().expect("a trusted copy");
        assert!(library.verified);
        assert_eq!(retry.wait, 0);
    }

    #[test]
    fn with_no_copy_at_all_one_that_couldnt_be_checked_is_used_until_one_can_be() {
        // A server that's always scanning.
        let shelf = shelf(12);
        shelf.lock().unwrap().scanning = true;
        let client = library_server(shelf.clone(), |_, _| {});
        let mut retry = Retry::new();
        let first = renew(&client, None, &mut retry, 0).unwrap().expect("better than none");
        assert!(!first.verified);
        assert_eq!(first.songs.len(), 12);
        // Fetched once, not again and again while the scan goes on.
        assert_eq!(shelf.lock().unwrap().pages, [0]);
        // While there is a copy, one that can't be trusted doesn't replace it.
        assert!(renew(&client, Some(&first), &mut retry, 400).unwrap().is_none());
        shelf.lock().unwrap().scanning = false;
        assert!(renew(&client, Some(&first), &mut retry, 100).unwrap().is_none(), "still waiting after the first");
        let second = renew(&client, Some(&first), &mut retry, 400).unwrap().expect("a trusted copy, later");
        assert!(second.verified);
        // A library that changes all the time: with nothing in use, a copy without repeats.
        let shelf = shelf_of(&["a", "b", "c"]);
        let client = library_server(shelf.clone(), |shelf, _| {
            let n = shelf.pages.len();
            shelf.songs.insert(0, format!("n{n}"));
            shelf.last_scan = n.to_string();
        });
        let copy = renew(&client, None, &mut Retry::new(), 0).unwrap().expect("better than none");
        assert!(!copy.verified);
        let ids = ids(&copy);
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), ids.len());
        // A copy that's kept on disk says whether it was checked; ones from before, which
        // didn't say, were.
        let saved: Library = serde_json::from_str(&serde_json::to_string(&copy).unwrap()).unwrap();
        assert!(!saved.verified);
        let older: Library = serde_json::from_str(r#"{"count":3,"last_scan":"1","fetched":0,"songs":[]}"#).unwrap();
        assert!(older.verified);
    }

    #[test]
    fn a_server_that_sends_the_same_page_for_ever_is_not_read_for_ever() {
        let shelf = shelf(1200);
        shelf.lock().unwrap().ignores_offset = true;
        let client = library_server(shelf.clone(), |_, _| {});
        let fetched = Library::fetch(&client, true).unwrap();
        let library = fetched.library.expect("better than none");
        assert!(!library.verified);
        assert_eq!(library.songs.len(), 500);
        // Two pages a go: the first, and the same one again.
        assert_eq!(shelf.lock().unwrap().pages.len(), 2 * FETCH_TRIES);
    }

    #[test]
    fn a_copy_holds_no_song_twice_and_no_more_than_the_server_counts() {
        let copy = |ids: &[&str]| Library { songs: ids.iter().map(|id| song(id, "", "", "")).collect(), ..Default::default() };
        assert!(copy(&["a", "b", "c"]).is_whole(Some(3)));
        // Fewer can be an account let into only some of the server's libraries.
        assert!(copy(&["a", "b"]).is_whole(Some(3)));
        assert!(!copy(&["a", "b", "c", "d"]).is_whole(Some(3)));
        assert!(!copy(&["a", "b", "a"]).is_whole(None));
        // A count that isn't the song count isn't gone by.
        assert!(copy(&["a", "b", "c", "d"]).is_whole(None));
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
