//! Looking music up in MusicBrainz, the open database that Lidarr is built on.
//!
//! Lidarr can only be searched by album title. To find the album a song is on, to list what
//! an artist has made, or to show what's on an album, the app asks MusicBrainz itself. Its
//! names for albums and artists are the ones Lidarr uses, so whatever is found here can be
//! asked for through Lidarr. Nothing here is used unless requests are switched on.
//!
//! MusicBrainz is free and asks callers to make no more than one request a second.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::lidarr::Offer;
use crate::search::fold;

const BASE: &str = "https://musicbrainz.org/ws/2";
/// The gap kept between requests.
const GAP: Duration = Duration::from_millis(1100);
/// MusicBrainz's own name for "Various Artists".
const VARIOUS: &str = crate::lidarr::VARIOUS;

/// When the next request may be made.
static NEXT: Mutex<Option<Instant>> = Mutex::new(None);
static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// A song that isn't in the library, and the album it could be had on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SongOffer {
    pub title: String,
    pub artist: String,
    /// MusicBrainz's name for the first artist credited, and what they're called.
    pub artist_id: String,
    pub artist_name: String,
    /// The album it's on: MusicBrainz's name for it (which is Lidarr's too), and its title.
    pub album: String,
    pub album_title: String,
    pub year: String,
    pub seconds: u32,
    /// "EP" or "Single" when that's what it's on; empty for an album.
    pub kind: String,
    /// MusicBrainz's names for the recordings of it that were found.
    pub recordings: Vec<String>,
    /// How well it answers what was typed, 0 being best (see `closeness`).
    pub close: u8,
    /// How many times it's been played, where ListenBrainz has been asked.
    pub listens: u64,
}

/// A song on an album, by name only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackName {
    pub title: String,
    pub seconds: u32,
    pub disc: u32,
    pub number: u32,
}

/// Where the cover of an album is kept, as a web address. Not every album has one.
pub fn cover(album: &str) -> String {
    format!("https://coverartarchive.org/release-group/{album}/front-250")
}

/// MusicBrainz closes a connection that's been idle for 15 seconds. Setting one up takes
/// about a second from far away, most of what a search would otherwise take.
const THEIR_IDLE: Duration = Duration::from_secs(12);
/// When MusicBrainz was last talked to.
static TOUCHED: Mutex<Option<Instant>> = Mutex::new(None);

fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| crate::subsonic::make_agent_idle(THEIR_IDLE))
}

/// Notes that MusicBrainz is being talked to now; says whether it had been lately.
fn touch() -> bool {
    let mut touched = TOUCHED.lock().unwrap_or_else(|stuck| stuck.into_inner());
    let lately = touched.is_some_and(|at| at.elapsed() < THEIR_IDLE - Duration::from_secs(2));
    *touched = Some(Instant::now());
    lately
}

/// Opens a connection ahead of a search, so the search doesn't wait for one. Called while
/// something is being typed into the search box; does nothing if a connection is likely open.
pub fn warm() {
    if touch() {
        return;
    }
    std::thread::spawn(|| {
        // The smallest thing there is to ask for, and not one of the lookups they ration.
        if let Ok(response) = agent().get("https://musicbrainz.org/robots.txt").call() {
            let _ = response.into_body().read_to_string();
        }
    });
}

fn get(path: &str) -> Result<Value, String> {
    for attempt in 0..4u64 {
        // One caller at a time, each waiting out the gap since the one before.
        {
            let mut next = NEXT.lock().unwrap_or_else(|stuck| stuck.into_inner());
            if let Some(wait) = next.and_then(|at| at.checked_duration_since(Instant::now())) {
                std::thread::sleep(wait);
            }
            *next = Some(Instant::now() + GAP);
        }
        touch();
        let response = agent()
            .get(&format!("{BASE}/{path}"))
            .header("Accept", "application/json")
            .call()
            .map_err(|e| format!("Couldn't reach MusicBrainz. {e}"))?;
        let status = response.status().as_u16();
        // Its search is shared by everyone and turns callers away when it's busy, saying how
        // soon to come back (a second, as a rule). That's waited out by the gap above.
        if status == 503 && attempt < 3 {
            let wait = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(1).clamp(1, 3);
            let _ = response.into_body().read_to_string();
            *NEXT.lock().unwrap_or_else(|stuck| stuck.into_inner()) = Some(Instant::now() + Duration::from_secs(wait));
            continue;
        }
        if status != 200 {
            return Err(format!("MusicBrainz answered with an error ({status})."));
        }
        let text = response.into_body().read_to_string().map_err(|e| format!("Couldn't read MusicBrainz's answer. {e}"))?;
        return serde_json::from_str(&text).map_err(|_| "MusicBrainz's answer made no sense.".to_string());
    }
    Err("MusicBrainz is busy. Try again in a moment.".into())
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get(key).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// Whether a release is filed as a soundtrack and nothing else besides.
fn is_soundtrack(group: &Value) -> bool {
    matches!(list(group, "secondary-types"), [only] if only.as_str() == Some("Soundtrack"))
}

/// The kind of release this is, if it's one that can be asked for: an album, an EP or a
/// single, made in a studio or for a soundtrack, and not a live one, a compilation, a remix
/// and so on.
fn kind_of(group: &Value) -> Option<&'static str> {
    if !list(group, "secondary-types").is_empty() && !is_soundtrack(group) {
        return None;
    }
    match text(group, "primary-type") {
        "Album" => Some("Album"),
        "EP" => Some("EP"),
        "Single" => Some("Single"),
        _ => None,
    }
}

/// The artists credited, as written ("A feat. B"), and the first one's id and name.
fn credit(value: &Value) -> (String, String, String) {
    let credits = list(value, "artist-credit");
    let written: String = credits.iter().map(|c| format!("{}{}", text(c, "name"), text(c, "joinphrase"))).collect();
    let first = credits.first().and_then(|c| c.get("artist"));
    (written, first.map(|a| text(a, "id").to_string()).unwrap_or_default(), first.map(|a| text(a, "name").to_string()).unwrap_or_default())
}

fn year(date: &str) -> String {
    date.get(..4).filter(|year| year.chars().all(|c| c.is_ascii_digit())).unwrap_or_default().to_string()
}

/// Looks for songs matching what was typed, each with the studio album it's on.
pub fn find_songs(term: &str) -> Result<Vec<SongOffer>, String> {
    let found = get(&format!("recording?query={}&dismax=true&limit=100&fmt=json", crate::subsonic::encode(term)))?;
    Ok(songs_from(&found, term, crate::lidarr::singles()))
}

/// One artist's song of a given name, with the studio album it's on. For when it's known
/// whose song is meant: a search by title alone can bring back a hundred other people's
/// versions of a well-known song and not the original.
pub fn song_by(artist: &str, title: &str) -> Result<Vec<SongOffer>, String> {
    let phrase: String = title.chars().filter(|c| !matches!(c, '"' | '\\')).collect();
    let ask = |only: &str, limit: u32| -> Result<Vec<SongOffer>, String> {
        let query = format!("arid:{artist} AND recording:\"{phrase}\" AND primarytype:album AND status:official AND {only}");
        let found = get(&format!("recording?query={}&limit={limit}&fmt=json", crate::subsonic::encode(&query)))?;
        Ok(songs_from(&found, title, false).into_iter().filter(|song| same_words(base(&song.title), base(title))).collect())
    };
    // Theirs goes before one where they're a guest (a remix by someone else, "ft." them),
    // and theirs alone before one they share the credit for (a mash-up, a duet).
    let order = |songs: &mut Vec<SongOffer>| songs.sort_by_key(|song| (song.artist_id != artist, std::cmp::Reverse(song.listens), song.artist != song.artist_name));
    // Asked for among recordings that are only on studio albums: a famous song has hundreds
    // of live and compilation recordings, and the album cut needn't be among the first
    // handed back.
    let mut songs = ask("NOT secondarytype:*", 50)?;
    order(&mut songs);
    if songs.first().is_none_or(|song| song.artist_id != artist) {
        // But the album cut of a hit is on compilations as well, which rules it out above.
        // So it's asked for again with only the live recordings left out, and the one
        // that's played most is taken to be it.
        let mut wider = ask("NOT secondarytype:live", 100)?;
        let recordings: Vec<String> = wider.iter().flat_map(|song| song.recordings.iter().cloned()).collect();
        if let Ok(counts) = crate::listenbrainz::listens(&recordings) {
            by_listens(&mut wider, &counts);
        }
        order(&mut wider);
        if wider.first().is_some_and(|song| song.artist_id == artist) {
            songs = wider;
        }
    }
    Ok(songs)
}

/// The release a song came out on, for when only names are known: an artist's, a song's and
/// (perhaps) the album a playlist says it's on. The answer is MusicBrainz's name for the
/// release, which is Lidarr's too. Singles and EPs only count if `singles` says so.
pub fn release_with(artist: &str, title: &str, album: &str, singles: bool) -> Result<Option<String>, String> {
    let quoted = |text: &str| text.chars().filter(|c| !matches!(c, '"' | '\\')).collect::<String>();
    let song = crate::import::undecorated(title);
    let mut query = format!("artist:\"{}\" AND recording:\"{}\"", quoted(artist), quoted(&song));
    let ask = |query: &str, limit: u32| get(&format!("recording?query={}&limit={limit}&fmt=json", crate::subsonic::encode(query)));
    // A well-known song has hundreds of recordings (every live take, every compilation),
    // and the one wanted needn't be among the first handed back. So the release the
    // playlist names is asked for first, by name.
    let named = crate::import::undecorated(album);
    if !named.is_empty() {
        let found = ask(&format!("{query} AND release:\"{}\"", quoted(&named)), 50)?;
        if let Some(release) = release_among(&found, artist, title, album, singles) {
            return Ok(Some(release));
        }
    }
    query.push_str(" AND NOT secondarytype:live");
    Ok(release_among(&ask(&query, 100)?, artist, title, album, singles))
}

/// Of the releases these recordings are on, the one to ask for: the one the playlist named
/// if it's among them; then a studio release before a soundtrack, the artist's own before
/// one credited to various artists, an album before a single before an EP, and the earliest.
fn release_among(found: &Value, artist: &str, title: &str, album: &str, singles: bool) -> Option<String> {
    let song = crate::import::undecorated(title);
    // A live take or a remix isn't the song, unless that's what was asked for.
    let kind = crate::import::version(title);
    // Playlists name soundtracks loosely ("Sing (Original Motion Picture Soundtrack Deluxe)"
    // for "Sing: Original Motion Picture Soundtrack"): one's words within the other's will do.
    let (bare, full) = (words(&crate::import::undecorated(album)), words(album));
    let named = |title: &str| {
        let theirs = words(title);
        !bare.is_empty() && (same_words(&crate::import::undecorated(title), &crate::import::undecorated(album)) || (theirs.len() > 1 && theirs.iter().all(|word| full.contains(word))))
    };
    let mut best: Option<((bool, bool, bool, u8, String), String)> = None;
    for recording in list(found, "recordings") {
        let theirs = text(recording, "title");
        if recording.get("video").and_then(Value::as_bool) == Some(true) || !same_words(&crate::import::undecorated(theirs), &song) || crate::import::version(theirs) != kind {
            continue;
        }
        let (written, _, first) = credit(recording);
        if !crate::asked::same_artist(&first, artist) && !crate::asked::same_artist(&written, artist) {
            continue;
        }
        for release in list(recording, "releases") {
            let Some(group) = release.get("release-group") else { continue };
            let rank = match kind_of(group) {
                Some("Album") => 0,
                Some("Single") if singles => 1,
                Some("EP") if singles => 2,
                _ => continue,
            };
            let (_, by, _) = credit(release);
            let soundtrack = is_soundtrack(group);
            // A release credited to various artists is a compilation, whatever it's filed
            // as, unless it's a film's soundtrack.
            let various = by == VARIOUS;
            if matches!(text(release, "status"), "Bootleg" | "Pseudo-Release") || (various && !soundtrack) || text(group, "id").is_empty() {
                continue;
            }
            // One with no date goes after those with.
            let date = if text(release, "date").is_empty() { "9999" } else { text(release, "date") };
            let key = (!named(text(group, "title")), soundtrack, various, rank, date.to_string());
            if best.as_ref().is_none_or(|(top, _)| key < *top) {
                best = Some((key, text(group, "id").to_string()));
            }
        }
    }
    best.map(|(_, id)| id)
}

pub use nexus_core::text::{base, same_words, words};

/// How well a song answers what was typed, 0 being best:
/// 0, its title and artist are exactly what was typed ("yellow coldplay");
/// 1, its title is ("yellow");
/// 2, everything typed is in its title and artist, among other words;
/// 3, anything else.
fn closeness(typed: &[String], song: &SongOffer) -> u8 {
    let title = words(base(&song.title));
    let artist = words(&song.artist);
    let all_typed = typed.iter().all(|word| title.contains(word) || artist.contains(word));
    let title_typed = !title.is_empty() && title.iter().all(|word| typed.contains(word));
    let artist_typed = artist.iter().all(|word| typed.contains(word));
    if all_typed && title_typed && artist_typed {
        0
    } else if title_typed && typed.iter().all(|word| title.contains(word)) {
        1
    } else if all_typed {
        2
    } else {
        3
    }
}

/// MusicBrainz lists every recording of a song separately: the single, the album cut, each
/// compilation it was licensed to, each remix and live take. This folds them into one entry
/// per song and artist, named as plainly as any of them is, and keeps the earliest studio
/// album among them. They're put in order of how well they answer what was typed; among
/// equals, a song found on more releases goes first, which is the nearest thing here to how
/// well known it is.
///
/// A song that isn't on any album is offered on the single or EP it came out on, if
/// `singles` says those are wanted.
fn songs_from(found: &Value, typed: &str, singles: bool) -> Vec<SongOffer> {
    struct Group {
        offer: SongOffer,
        /// The date of the album chosen so far; empty if it has none.
        date: String,
        /// How good a home that is for the song: 0 an album, 1 a single, 2 an EP.
        rank: u8,
        score: u64,
        releases: usize,
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut places: HashMap<(String, String), usize> = HashMap::new();
    for recording in list(found, "recordings") {
        if recording.get("video").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let title = text(recording, "title");
        let (artist, artist_id, artist_name) = credit(recording);
        if title.is_empty() || artist.is_empty() {
            continue;
        }
        let key = (fold(base(title)), fold(&artist));
        let place = *places.entry(key).or_insert_with(|| {
            groups.push(Group {
                offer: SongOffer { title: title.into(), artist, artist_id, artist_name, ..Default::default() },
                date: String::new(),
                rank: 0,
                score: 0,
                releases: 0,
            });
            groups.len() - 1
        });
        let group = &mut groups[place];
        if !text(recording, "id").is_empty() {
            group.offer.recordings.push(text(recording, "id").to_string());
        }
        if title.len() < group.offer.title.len() {
            group.offer.title = title.to_string();
        }
        let releases = list(recording, "releases");
        group.score = group.score.max(recording.get("score").and_then(Value::as_u64).unwrap_or(0));
        group.releases += releases.len();
        for release in releases {
            let Some(album) = release.get("release-group") else { continue };
            let kind = match kind_of(album) {
                Some("Album") => 0,
                Some("Single") if singles => 1,
                Some("EP") if singles => 2,
                _ => continue,
            };
            // A release credited to various artists is a compilation, whatever it's filed
            // as, unless it's a film's soundtrack.
            let (_, by, _) = credit(release);
            let soundtrack = is_soundtrack(album);
            if matches!(text(release, "status"), "Bootleg" | "Pseudo-Release") || (by == VARIOUS && !soundtrack) {
                continue;
            }
            // A soundtrack is the song's home only if it has no other: after every studio
            // release, and a film's own soundtrack album after the artist's.
            let rank = kind + if !soundtrack { 0 } else if by == VARIOUS { 6 } else { 3 };
            let date = text(release, "date");
            // An album first. Failing that the single, which is the song as it was put out,
            // before an EP, which is as likely a session or a live set. Of a kind, the earliest.
            let earlier = !date.is_empty() && (group.date.is_empty() || date < group.date.as_str());
            let better = group.offer.album.is_empty() || rank < group.rank || (rank == group.rank && earlier);
            if better {
                group.rank = rank;
                group.offer.kind = ["", "Single", "EP"][kind as usize].to_string();
                group.date = date.to_string();
                group.offer.album = text(album, "id").to_string();
                group.offer.album_title = text(album, "title").to_string();
                group.offer.year = year(date);
                group.offer.seconds = (recording.get("length").and_then(Value::as_u64).unwrap_or(0) / 1000) as u32;
            }
        }
    }
    groups.retain(|group| !group.offer.album.is_empty());
    let typed = words(typed);
    let mut ranked: Vec<(u8, Group)> = groups.into_iter().map(|group| (closeness(&typed, &group.offer), group)).collect();
    // Of the loose matches, those MusicBrainz itself scored well below its best are noise.
    let best = ranked.iter().map(|(_, group)| group.score).max().unwrap_or(0);
    ranked.retain(|(close, group)| *close < 3 || group.score + 30 >= best);
    ranked.sort_by(|(close_a, a), (close_b, b)| {
        close_a.cmp(close_b).then(if *close_a == 3 { b.score.cmp(&a.score) } else { std::cmp::Ordering::Equal }).then(b.releases.cmp(&a.releases))
    });
    ranked.into_iter().map(|(close, group)| SongOffer { close, ..group.offer }).collect()
}

/// Puts songs that answer what was typed equally well in order of how much they're played:
/// of forty songs called "Home", the one most people mean goes first. `counts` is what
/// ListenBrainz says of their recordings. Loose matches keep the order they were found in.
pub fn by_listens(songs: &mut [SongOffer], counts: &HashMap<String, u64>) {
    for song in songs.iter_mut() {
        song.listens = song.recordings.iter().filter_map(|recording| counts.get(recording)).copied().max().unwrap_or(0);
    }
    songs.sort_by(|a, b| a.close.cmp(&b.close).then(if a.close < 3 { b.listens.cmp(&a.listens) } else { std::cmp::Ordering::Equal }));
}

/// An artist's most played songs, each on the release of theirs it can be asked for with.
/// `top` is ListenBrainz's list, most played first; `releases` is what the artist has put
/// out. A song is placed on the album ListenBrainz knows it from, or failing that on the
/// single of its own name; one that fits neither is left out.
pub fn popular(top: &[crate::listenbrainz::TopSong], releases: &[Offer], artist: &str, artist_name: &str) -> Vec<SongOffer> {
    let mut songs: Vec<SongOffer> = Vec::new();
    for song in top {
        let title = base(&song.title);
        if songs.iter().any(|have| same_words(base(&have.title), title)) {
            continue;
        }
        let on = |name: &str, kind: &str| releases.iter().find(|release| release.kind() == kind && (same_words(&release.title, name) || same_words(base(&release.title), base(name))));
        let Some(release) = on(&song.album_title, "Album").or_else(|| on(title, "Single")).or_else(|| on(&song.album_title, "EP")).or_else(|| on(&song.album_title, "Single")) else {
            continue;
        };
        songs.push(SongOffer {
            title: song.title.clone(),
            artist: artist_name.to_string(),
            artist_id: artist.to_string(),
            artist_name: artist_name.to_string(),
            album: release.album.clone(),
            album_title: release.title.clone(),
            year: release.year.clone(),
            seconds: song.seconds,
            kind: release.kind.clone(),
            listens: song.listens,
            ..Default::default()
        });
    }
    songs
}

/// An artist's studio albums, newest first, and what the artist is called.
pub fn artist_albums(artist: &str) -> Result<(String, Vec<Offer>), String> {
    // Asked for as their page asks, so the one answer serves both.
    artist_releases(artist, crate::lidarr::singles()).map(|(name, releases)| (name, releases.into_iter().filter(|release| release.kind() == "Album").collect()))
}

/// An artist's studio albums and, if `singles` says so, their EPs and singles too, newest
/// first; and what the artist is called.
pub fn artist_releases(artist: &str, singles: bool) -> Result<(String, Vec<Offer>), String> {
    // Asked for more than once in a row: by a search that names the artist, then by their
    // page, then for what's on the page. Kept a while so only the first has to wait.
    let key = (artist.to_string(), singles);
    {
        let kept = RELEASES.lock().unwrap_or_else(|stuck| stuck.into_inner());
        if let Some((_, _, found)) = kept.iter().find(|(known, at, _)| *known == key && at.elapsed() < RELEASES_KEPT) {
            return Ok(found.clone());
        }
    }
    let found = fetch_artist_releases(artist, singles)?;
    let mut kept = RELEASES.lock().unwrap_or_else(|stuck| stuck.into_inner());
    kept.retain(|(known, at, _)| *known != key && at.elapsed() < RELEASES_KEPT);
    if kept.len() >= 40 {
        kept.remove(0);
    }
    kept.push((key, Instant::now(), found.clone()));
    Ok(found)
}

/// How long an artist's list of releases is kept.
const RELEASES_KEPT: Duration = Duration::from_secs(600);
type Releases = (String, Vec<Offer>);
static RELEASES: Mutex<Vec<((String, bool), Instant, Releases)>> = Mutex::new(Vec::new());

/// An artist's songs, most played first, worked out from what ListenBrainz will say of
/// anyone: which of their releases are played most, and then which songs on those.
/// `releases` is what they've put out.
pub fn played_most(artist: &str, releases: &[Offer]) -> Result<Vec<crate::listenbrainz::TopSong>, String> {
    /// How many of their releases are looked into.
    const DEEP: usize = 4;
    let ids: Vec<String> = releases.iter().take(300).map(|release| release.album.clone()).collect();
    let plays = crate::listenbrainz::release_listens(&ids)?;
    let mut most: Vec<&Offer> = releases.iter().filter(|release| plays.contains_key(&release.album)).collect();
    most.sort_by_key(|release| std::cmp::Reverse(plays[&release.album]));
    most.truncate(DEEP);
    if most.is_empty() {
        return Ok(Vec::new());
    }
    let groups: Vec<&str> = most.iter().map(|release| release.album.as_str()).collect();
    let query = format!("rgid:({}) AND arid:{artist}", groups.join(" OR "));
    // Two hundred at most: deluxe editions can double what an album holds.
    let mut recordings: Vec<Value> = Vec::new();
    for page in 0..2 {
        let found = get(&format!("recording?query={}&limit=100&offset={}&fmt=json", crate::subsonic::encode(&query), page * 100))?;
        let got = list(&found, "recordings");
        recordings.extend(got.iter().cloned());
        if got.len() < 100 || found.get("count").and_then(Value::as_u64).unwrap_or(0) <= 100 {
            break;
        }
    }
    let counts = crate::listenbrainz::listens(&recordings.iter().map(|recording| text(recording, "id").to_string()).collect::<Vec<_>>())?;
    Ok(most_played(&recordings, &counts, &most))
}

/// One entry per song among `recordings`, most played first, each on the best of `most`
/// (the artist's most played releases, in that order) that it's on.
fn most_played(recordings: &[Value], counts: &HashMap<String, u64>, most: &[&Offer]) -> Vec<crate::listenbrainz::TopSong> {
    let mut songs: Vec<crate::listenbrainz::TopSong> = Vec::new();
    let mut places: HashMap<String, usize> = HashMap::new();
    for recording in recordings {
        let title = text(recording, "title");
        let listens = counts.get(text(recording, "id")).copied().unwrap_or(0);
        if title.is_empty() || listens == 0 {
            continue;
        }
        let on: Vec<&str> = list(recording, "releases").iter().filter_map(|release| release.get("release-group")).map(|group| text(group, "id")).collect();
        // An album before a single or EP; of a kind, the more played.
        let Some(home) = most.iter().filter(|release| on.contains(&release.album.as_str())).min_by_key(|release| release.kind() != "Album") else { continue };
        let place = *places.entry(fold(base(title))).or_insert_with(|| {
            songs.push(crate::listenbrainz::TopSong { title: title.to_string(), album_title: home.title.clone(), ..Default::default() });
            songs.len() - 1
        });
        let song = &mut songs[place];
        if title.len() < song.title.len() {
            song.title = title.to_string();
        }
        // The same song recorded twice (the album cut and its video, say) counts once, as
        // whichever is played more.
        if listens > song.listens {
            song.listens = listens;
            song.seconds = (recording.get("length").and_then(Value::as_u64).unwrap_or(0) / 1000) as u32;
            if home.kind() == "Album" || song.album_title.is_empty() {
                song.album_title = home.title.clone();
            }
        }
    }
    songs.sort_by_key(|song| std::cmp::Reverse(song.listens));
    songs
}

fn fetch_artist_releases(artist: &str, singles: bool) -> Result<(String, Vec<Offer>), String> {
    let mut name = String::new();
    let mut albums = Vec::new();
    let kinds = if singles { "album%7Cep%7Csingle" } else { "album" };
    // A hundred at a time; live albums and compilations are among them and are left out.
    for page in 0..6 {
        let found = get(&format!("release-group?artist={artist}&type={kinds}&limit=100&offset={}&inc=artist-credits&fmt=json", page * 100))?;
        let groups = list(&found, "release-groups");
        for group in groups {
            let credits = list(group, "artist-credit");
            if name.is_empty() {
                let this = credits.iter().filter_map(|c| c.get("artist")).find(|a| text(a, "id") == artist);
                name = this.map(|a| text(a, "name").to_string()).unwrap_or_default();
            }
            if let Some(kind) = kind_of(group) {
                let (by, _, _) = credit(group);
                albums.push(Offer {
                    soundtrack: is_soundtrack(group),
                    kind: if kind == "Album" { String::new() } else { kind.to_string() },
                    album: text(group, "id").to_string(),
                    artist_id: artist.to_string(),
                    title: text(group, "title").to_string(),
                    artist: by,
                    year: year(text(group, "first-release-date")),
                    cover: cover(text(group, "id")),
                    requested: false,
                });
            }
        }
        let total = found.get("release-group-count").and_then(Value::as_u64).unwrap_or(0);
        if groups.is_empty() || (page + 1) * 100 >= total {
            break;
        }
    }
    albums.sort_by(|a, b| b.year.cmp(&a.year).then(a.title.cmp(&b.title)));
    Ok((name, albums))
}

/// The songs on an album.
pub fn album_tracks(album: &str) -> Result<Vec<TrackName>, String> {
    let found = get(&format!("release?release-group={album}&inc=recordings+media&limit=50&fmt=json"))?;
    Ok(tracks_from(&found))
}

/// An album comes out many times over: with bonus songs, with a film on a second disc, on
/// other continents. This picks the edition with the usual number of songs, the earliest
/// such one, and leaves out discs of video.
fn tracks_from(found: &Value) -> Vec<TrackName> {
    let audio = |medium: &&Value| {
        let format = text(medium, "format");
        !["DVD", "Blu-ray", "VHS", "LaserDisc", "VCD"].iter().any(|video| format.contains(video))
    };
    let count = |release: &Value| -> usize { list(release, "media").iter().filter(audio).map(|medium| list(medium, "tracks").len()).sum() };
    let releases: Vec<&Value> = list(found, "releases").iter().filter(|release| count(release) > 0).collect();
    let official: Vec<&Value> = releases.iter().copied().filter(|release| text(release, "status") == "Official").collect();
    let pool = if official.is_empty() { releases } else { official };
    let mut tally: HashMap<usize, usize> = HashMap::new();
    for release in &pool {
        *tally.entry(count(release)).or_default() += 1;
    }
    // The commonest length; of two equally common, the shorter.
    let Some(usual) = tally.into_iter().max_by_key(|&(length, times)| (times, std::cmp::Reverse(length))).map(|(length, _)| length) else {
        return Vec::new();
    };
    let dated = |release: &&Value| {
        let date = text(release, "date");
        (date.is_empty(), date.to_string())
    };
    let Some(chosen) = pool.into_iter().filter(|release| count(release) == usual).min_by_key(dated) else {
        return Vec::new();
    };
    let mut tracks = Vec::new();
    for (disc, medium) in list(chosen, "media").iter().filter(audio).enumerate() {
        for (at, track) in list(medium, "tracks").iter().enumerate() {
            tracks.push(TrackName {
                title: text(track, "title").to_string(),
                seconds: (track.get("length").and_then(Value::as_u64).unwrap_or(0) / 1000) as u32,
                disc: disc as u32 + 1,
                number: at as u32 + 1,
            });
        }
    }
    tracks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_release_a_named_song_came_out_on() {
        let release = |id: &str, title: &str, kind: &str, secondary: &str, date: &str| {
            format!(r#"{{"status":"Official","date":"{date}","release-group":{{"id":"{id}","title":"{title}","primary-type":"{kind}","secondary-types":[{secondary}]}}}}"#)
        };
        let recording = |title: &str, artist: &str, releases: &[String]| {
            format!(r#"{{"title":"{title}","artist-credit":[{{"name":"{artist}","artist":{{"id":"a1","name":"{artist}"}}}}],"releases":[{}]}}"#, releases.join(","))
        };
        let found: Value = serde_json::from_str(&format!(
            r#"{{"recordings":[{},{},{}]}}"#,
            recording("All In", "Lifehouse", &[release("ep", "All In", "EP", "", ""), release("best", "Greatest Hits", "Album", r#""Compilation""#, "2015"), release("album", "Smoke & Mirrors", "Album", "", "2010")]),
            recording("All In (live)", "Lifehouse", &[release("live", "Live Somewhere", "Album", "", "2001")]),
            recording("All In", "Someone Else", &[release("other", "Theirs", "Album", "", "1999")]),
        ))
        .unwrap();
        // The studio album, whatever else the song is on.
        assert_eq!(release_among(&found, "Lifehouse", "All In", "", true).as_deref(), Some("album"));
        // Unless the playlist names something else it's on.
        assert_eq!(release_among(&found, "Lifehouse", "All In", "All In - EP", true).as_deref(), Some("ep"));
        assert_eq!(release_among(&found, "Lifehouse", "All In", "Smoke & Mirrors (International Version)", true).as_deref(), Some("album"));
        // With singles and EPs switched off, only an album will do.
        assert_eq!(release_among(&found, "Lifehouse", "All In", "All In", false).as_deref(), Some("album"));
        assert_eq!(release_among(&found, "Nobody", "All In", "", true), None);
        assert_eq!(release_among(&found, "Lifehouse", "Another Song", "", true), None);
        // A live take is what's found only when a live take is what's wanted.
        assert_eq!(release_among(&found, "Lifehouse", "All In - Live", "", true).as_deref(), Some("live"));

        // A song made for a film: its own single is filed as a soundtrack, and so is the
        // film's album, which is credited to various artists.
        let by_various = |id: &str, title: &str, secondary: &str| {
            format!(
                r#"{{"status":"Official","date":"2016","artist-credit":[{{"name":"Various Artists","artist":{{"id":"{VARIOUS}","name":"Various Artists"}}}}],"release-group":{{"id":"{id}","title":"{title}","primary-type":"Album","secondary-types":[{secondary}]}}}}"#
            )
        };
        let found: Value = serde_json::from_str(&format!(
            r#"{{"recordings":[{}]}}"#,
            recording("Still Falling for You", "Ellie Goulding", &[by_various("hits", "Hits 2016", r#""Compilation""#), by_various("film", "Bridget Jones’s Baby: Original Motion Picture Soundtrack", r#""Soundtrack""#), release("single", "Still Falling for You", "Single", r#""Soundtrack""#, "2016-08-19")]),
        ))
        .unwrap();
        let wanted = "Still Falling For You - From \"Bridget Jones's Baby\"";
        assert_eq!(release_among(&found, "Ellie Goulding", wanted, "Still Falling For You (From \"Bridget Jones's Baby\" Original Motion Picture Soundtrack)", true).as_deref(), Some("single"));
        // Her own release before the film's, when the playlist doesn't say; the film's when it does, or when singles are off.
        assert_eq!(release_among(&found, "Ellie Goulding", wanted, "", true).as_deref(), Some("single"));
        assert_eq!(release_among(&found, "Ellie Goulding", wanted, "Bridget Jones's Baby (Original Motion Picture Soundtrack Deluxe)", true).as_deref(), Some("film"));
        assert_eq!(release_among(&found, "Ellie Goulding", wanted, "", false).as_deref(), Some("film"));
    }

    #[test]
    fn folds_recordings_into_songs() {
        let found: Value = serde_json::from_str(
            r#"{"recordings":[
            {"score":100,"title":"Lonely No More","length":227333,"artist-credit":[{"name":"Rob Thomas","artist":{"id":"rt","name":"Rob Thomas"}}],
             "releases":[
               {"title":"Now 60","status":"Official","date":"2005-03-01","artist-credit":[{"name":"Various Artists","artist":{"id":"89ad4ac3-39f7-470e-963a-56509c546377"}}],
                "release-group":{"id":"now","title":"Now 60","primary-type":"Album","secondary-types":["Compilation"]}},
               {"title":"Lonely No More","status":"Official","date":"2005-02-14","release-group":{"id":"single","title":"Lonely No More","primary-type":"Single"}},
               {"title":"…Something to Be","status":"Official","date":"2009-05-05","release-group":{"id":"stb","title":"…Something to Be","primary-type":"Album","secondary-types":[]}},
               {"title":"…Something to Be","status":"Official","date":"2005-04-05","release-group":{"id":"stb","title":"…Something to Be","primary-type":"Album"}}]},
            {"score":100,"title":"Lonely No More","length":225000,"artist-credit":[{"name":"Rob Thomas","artist":{"id":"rt","name":"Rob Thomas"}}],
             "releases":[{"title":"Hits","status":"Official","release-group":{"id":"hits","title":"Hits","primary-type":"Album","secondary-types":["Compilation"]}}]},
            {"score":100,"title":"Lonely No More","video":true,"artist-credit":[{"name":"Someone","artist":{"id":"s","name":"Someone"}}],
             "releases":[{"status":"Official","date":"1990","release-group":{"id":"x","title":"X","primary-type":"Album"}}]},
            {"score":100,"title":"Lonely No More (Acoustic)","length":245000,"artist-credit":[{"name":"Rob Thomas","artist":{"id":"rt","name":"Rob Thomas"}}],
             "releases":[{"title":"…Something to Be","status":"Official","date":"2005-04-19","release-group":{"id":"stb","title":"…Something to Be","primary-type":"Album"}}]},
            {"score":98,"title":"Lonely No More","length":null,"artist-credit":[{"name":"W.C. Clark","artist":{"id":"wc","name":"W.C. Clark"}}],
             "releases":[{"title":"Lover's Plea","status":"Official","date":"1998-05-05","release-group":{"id":"lp","title":"Lover's Plea","primary-type":"Album"}}]},
            {"score":98,"title":"Only On A Compilation","artist-credit":[{"name":"Nobody","artist":{"id":"n","name":"Nobody"}}],
             "releases":[{"status":"Official","release-group":{"id":"c","title":"C","primary-type":"Album","secondary-types":["Compilation"]}}]},
            {"score":40,"title":"More Lonely","artist-credit":[{"name":"Far Off","artist":{"id":"f","name":"Far Off"}}],
             "releases":[{"status":"Official","date":"2001","release-group":{"id":"f1","title":"F","primary-type":"Album"}}]}
            ]}"#,
        )
        .unwrap();
        // With singles wanted, a song still goes on its album where it has one.
        assert_eq!(songs_from(&found, "lonely no more", true)[0].album, "stb");
        let songs = songs_from(&found, "lonely no more", false);
        assert_eq!(songs.len(), 2);
        assert_eq!(
            SongOffer { recordings: Vec::new(), ..songs[0].clone() },
            SongOffer {
                title: "Lonely No More".into(),
                artist: "Rob Thomas".into(),
                artist_id: "rt".into(),
                artist_name: "Rob Thomas".into(),
                album: "stb".into(),
                album_title: "…Something to Be".into(),
                year: "2005".into(),
                seconds: 227,
                kind: String::new(),
                // The title is what was typed; the artist wasn't.
                close: 1,
                ..Default::default()
            }
        );
        assert_eq!((songs[1].artist.as_str(), songs[1].album.as_str(), songs[1].seconds), ("W.C. Clark", "lp", 0));
    }

    #[test]
    fn puts_the_most_played_first() {
        let song = |title: &str, artist: &str, close: u8, recordings: &[&str]| SongOffer {
            title: title.into(),
            artist: artist.into(),
            close,
            recordings: recordings.iter().map(|id| id.to_string()).collect(),
            ..Default::default()
        };
        let mut songs = vec![song("Home", "Nobody", 1, &["a"]), song("Home", "Somebody", 1, &["b", "c"]), song("Homeward", "Loose", 3, &["d"]), song("Home", "Typed", 0, &[])];
        let counts = HashMap::from([("a".to_string(), 10), ("b".to_string(), 3), ("c".to_string(), 900), ("d".to_string(), 5000)]);
        by_listens(&mut songs, &counts);
        // What was typed outright stays first whatever it's played; a loose match stays last.
        assert_eq!(songs.iter().map(|song| song.artist.as_str()).collect::<Vec<_>>(), ["Typed", "Somebody", "Nobody", "Loose"]);
        assert_eq!(songs[1].listens, 900);
    }

    #[test]
    fn places_popular_songs_on_releases() {
        let release = |id: &str, title: &str, kind: &str| Offer { album: id.into(), title: title.into(), kind: kind.into(), year: "2005".into(), ..Default::default() };
        let releases = [release("stb", "…Something to Be", ""), release("lnm", "Lonely No More", "Single"), release("pie", "Pieces", "Single"), release("ep", "Sessions", "EP")];
        let top = |title: &str, album: &str, listens: u64| crate::listenbrainz::TopSong { title: title.into(), album_title: album.into(), seconds: 200, listens };
        let songs = popular(
            &[
                top("Lonely No More", "...Something to Be", 90),
                top("Lonely No More (Acoustic)", "Sessions", 80),
                top("Pieces", "Greatest Hits", 70),
                top("Nowhere", "A Film Soundtrack", 60),
                top("Live Cut", "Sessions", 50),
            ],
            &releases,
            "rt",
            "Rob Thomas",
        );
        let placed: Vec<(&str, &str, &str)> = songs.iter().map(|song| (song.title.as_str(), song.album.as_str(), song.kind.as_str())).collect();
        // On its album where it has one; on its own single failing that; left out otherwise.
        assert_eq!(placed, [("Lonely No More", "stb", ""), ("Pieces", "pie", "Single"), ("Live Cut", "ep", "EP")]);
        assert_eq!((songs[0].artist_id.as_str(), songs[0].listens, songs[0].year.as_str()), ("rt", 90, "2005"));
    }

    #[test]
    fn works_out_the_most_played() {
        let recordings: Value = serde_json::from_str(
            r#"[
            {"id":"r1","title":"Yellow","length":269000,"releases":[{"release-group":{"id":"para"}},{"release-group":{"id":"best"}}]},
            {"id":"r2","title":"Yellow (video)","length":270000,"releases":[{"release-group":{"id":"yel"}}]},
            {"id":"r3","title":"Trouble","length":270000,"releases":[{"release-group":{"id":"para"}}]},
            {"id":"r4","title":"Unplayed","releases":[{"release-group":{"id":"para"}}]},
            {"id":"r5","title":"Elsewhere","releases":[{"release-group":{"id":"other"}}]}
            ]"#,
        )
        .unwrap();
        let (single, album) = (Offer { album: "yel".into(), title: "Yellow".into(), kind: "Single".into(), ..Default::default() }, Offer { album: "para".into(), title: "Parachutes".into(), ..Default::default() });
        let counts = HashMap::from([("r1".to_string(), 50), ("r2".to_string(), 700), ("r3".to_string(), 90), ("r5".to_string(), 999)]);
        let songs = most_played(recordings.as_array().unwrap(), &counts, &[&single, &album]);
        let listed: Vec<(&str, &str, u64, u32)> = songs.iter().map(|song| (song.title.as_str(), song.album_title.as_str(), song.listens, song.seconds)).collect();
        // The two recordings of Yellow count once, at the more played, and it's on the album.
        assert_eq!(listed, [("Yellow", "Parachutes", 700, 270), ("Trouble", "Parachutes", 90, 270)]);
    }

    #[test]
    fn picks_the_usual_edition() {
        let found: Value = serde_json::from_str(
            r#"{"releases":[
            {"status":"Official","date":"2005-04-19","media":[{"format":"CD","tracks":[{"title":"A","length":230960},{"title":"B","length":null}]},{"format":"DVD-Video","tracks":[{"title":"Film"}]}]},
            {"status":"Official","date":"2009","media":[{"format":"Digital Media","tracks":[{"title":"A"},{"title":"B"},{"title":"Bonus"}]}]},
            {"status":"Official","date":"2005-04-05","media":[{"format":"CD","tracks":[{"title":"A (first)","length":1000},{"title":"B (first)"}]}]},
            {"status":"Bootleg","date":"2001","media":[{"format":"CD","tracks":[{"title":"A"},{"title":"B"}]}]}
            ]}"#,
        )
        .unwrap();
        let tracks = tracks_from(&found);
        assert_eq!(tracks.iter().map(|track| track.title.as_str()).collect::<Vec<_>>(), ["A (first)", "B (first)"]);
        assert_eq!((tracks[0].seconds, tracks[1].number, tracks[1].disc), (1, 2, 1));
        assert!(tracks_from(&serde_json::json!({ "releases": [] })).is_empty());
        assert_eq!(base("Lonely No More (Acoustic)"), "Lonely No More");
        assert_eq!(base("(What's the Story) Morning Glory?"), "(What's the Story) Morning Glory?");
        assert_eq!(base("Plain"), "Plain");
    }

    #[test]
    fn puts_what_was_meant_first() {
        let song = |title: &str, artist: &str| SongOffer { title: title.into(), artist: artist.into(), ..Default::default() };
        let typed = words("Yellow coldplay");
        assert_eq!(closeness(&typed, &song("Yellow", "Coldplay")), 0);
        assert_eq!(closeness(&typed, &song("Yellow (Coldplay)", "Calum Graham")), 3);
        assert_eq!(closeness(&typed, &song("Yellow Submarine", "Coldplay")), 2);
        assert_eq!(closeness(&words("mr brightside"), &song("Mr. Brightside", "The Killers")), 1);
        assert_eq!(closeness(&words("mr brightside"), &song("Mr. Brightside (Jacques Lu Cont remix)", "The Killers")), 1);
        assert_eq!(closeness(&words("don't stop"), &song("Dont Stop", "Someone")), 1);
        assert!(same_words("Mr. Brightside", "mr brightside"));
        assert!(!same_words("Mr. Brightside", "brightside"));
        // A cover scored higher by MusicBrainz still goes after the song that was asked for.
        let found: Value = serde_json::from_str(
            r#"{"recordings":[
            {"score":100,"title":"Yellow (Coldplay)","artist-credit":[{"name":"Calum Graham","artist":{"id":"cg","name":"Calum Graham"}}],
             "releases":[{"status":"Official","date":"2015","release-group":{"id":"t","title":"Thread","primary-type":"Album"}}]},
            {"score":41,"title":"Yellow","artist-credit":[{"name":"Coldplay","artist":{"id":"cp","name":"Coldplay"}}],
             "releases":[{"status":"Official","date":"2000-07-10","release-group":{"id":"p","title":"Parachutes","primary-type":"Album"}}]}
            ]}"#,
        )
        .unwrap();
        let songs = songs_from(&found, "yellow coldplay", false);
        assert_eq!(songs.iter().map(|song| song.artist.as_str()).collect::<Vec<_>>(), ["Coldplay", "Calum Graham"]);
    }

    #[test]
    fn offers_a_song_on_the_single_it_came_out_on() {
        let found: Value = serde_json::from_str(
            r#"{"recordings":[
            {"score":100,"title":"Only A Single","length":180000,"artist-credit":[{"name":"Someone","artist":{"id":"s","name":"Someone"}}],
             "releases":[
               {"status":"Official","date":"2024-03-01","release-group":{"id":"single","title":"Only A Single","primary-type":"Single"}},
               {"status":"Official","date":"2024-01-01","release-group":{"id":"remix","title":"Only A Single (Remixes)","primary-type":"Single","secondary-types":["Remix"]}},
               {"status":"Official","date":"2024-06-01","release-group":{"id":"ep","title":"Short Player","primary-type":"EP"}}]}
            ]}"#,
        )
        .unwrap();
        // Albums only: it isn't on one, so it isn't offered.
        assert!(songs_from(&found, "only a single", false).is_empty());
        // Otherwise it's offered on its single: not the remix single, and not the later EP.
        let songs = songs_from(&found, "only a single", true);
        assert_eq!((songs[0].album.as_str(), songs[0].kind.as_str(), songs[0].year.as_str()), ("single", "Single", "2024"));
    }
}

#[cfg(test)]
mod live {
    /// Not run as a rule: it asks the real services. `cargo test live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn song_search() {
        let term = std::env::var("TERM_TYPED").unwrap_or_else(|_| "somebody that i used to know".into());
        let mut found = super::find_songs(&term).unwrap();
        let recordings: Vec<String> = found.iter().flat_map(|song| song.recordings.iter().cloned()).collect();
        let counts = crate::listenbrainz::listens(&recordings);
        println!("{} songs, {} recordings, listens {:?}", found.len(), recordings.len(), counts.as_ref().map(|c| c.len()));
        if let Ok(counts) = counts {
            super::by_listens(&mut found, &counts);
        }
        for song in found.iter().take(12) {
            println!("{} {} | {} | {} | {} {} {}", song.close, song.listens, song.title, song.artist, song.album_title, song.year, song.kind);
        }
    }

    #[test]
    #[ignore]
    fn most_played_by_artist() {
        let artist = std::env::var("ARTIST_ID").unwrap_or_else(|_| "e42680f7-e439-4666-9c2c-536c2b3c6db3".into());
        let (name, releases) = super::artist_releases(&artist, true).unwrap();
        let top = super::played_most(&artist, &releases).unwrap();
        println!("{name}: {} releases, {} songs", releases.len(), top.len());
        for song in top.iter().take(12) {
            println!("{} | {} | {}", song.listens, song.title, song.album_title);
        }
        for song in super::popular(&top, &releases, &artist, &name).iter().take(12) {
            println!("  -> {} | {} {} {}", song.title, song.album_title, song.year, song.kind);
        }
    }

    #[test]
    #[ignore]
    fn song_by_artist() {
        let artist = std::env::var("ARTIST_ID").unwrap_or_else(|_| "6f6fd596-76e0-4b82-aa37-f558ac2d337b".into());
        let title = std::env::var("TITLE_TYPED").unwrap_or_else(|_| "Somebody That I Used to Know".into());
        for song in super::song_by(&artist, &title).unwrap().iter().take(5) {
            println!("{} | {} | {} | {} {} {}", song.listens, song.title, song.artist, song.album_title, song.year, song.kind);
        }
    }
}
