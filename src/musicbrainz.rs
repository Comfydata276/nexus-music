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
const VARIOUS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";

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

fn get(path: &str) -> Result<Value, String> {
    let agent = AGENT.get_or_init(crate::subsonic::make_agent);
    for attempt in 0..3 {
        // One caller at a time, each waiting out the gap since the one before.
        {
            let mut next = NEXT.lock().unwrap_or_else(|stuck| stuck.into_inner());
            if let Some(wait) = next.and_then(|at| at.checked_duration_since(Instant::now())) {
                std::thread::sleep(wait);
            }
            *next = Some(Instant::now() + GAP);
        }
        let response = agent
            .get(&format!("{BASE}/{path}"))
            .header("Accept", "application/json")
            .call()
            .map_err(|e| format!("Couldn't reach MusicBrainz. {e}"))?;
        let status = response.status().as_u16();
        // Busy, or asked too fast: it says to come back shortly.
        if status == 503 && attempt < 2 {
            std::thread::sleep(Duration::from_millis(1500));
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

/// Whether this is a plain studio album: the kind requests deal in.
fn is_album(group: &Value) -> bool {
    text(group, "primary-type") == "Album" && list(group, "secondary-types").is_empty()
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
    Ok(songs_from(&found))
}

/// An artist's songs, the best known first as far as can be told, each with the studio
/// album it's on.
pub fn songs_by(artist: &str) -> Result<Vec<SongOffer>, String> {
    let found = get(&format!("recording?query=arid:{artist}&limit=100&fmt=json"))?;
    Ok(songs_from(&found))
}

/// A title without what's in brackets after it: "Lonely No More (Acoustic)" is, for finding
/// the song, "Lonely No More".
fn base(title: &str) -> &str {
    let cut = title.find(" (").or_else(|| title.find(" [")).unwrap_or(title.len());
    if cut == 0 { title } else { &title[..cut] }
}

/// MusicBrainz lists every recording of a song separately: the single, the album cut, each
/// compilation it was licensed to, each remix and live take. This folds them into one entry
/// per song and artist, named as plainly as any of them is, and keeps the earliest studio
/// album among them. A song found on more releases goes first: that's the nearest thing
/// here to how well known it is.
fn songs_from(found: &Value) -> Vec<SongOffer> {
    struct Group {
        offer: SongOffer,
        /// The date of the album chosen so far; empty if it has none.
        date: String,
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
                score: 0,
                releases: 0,
            });
            groups.len() - 1
        });
        let group = &mut groups[place];
        if title.len() < group.offer.title.len() {
            group.offer.title = title.to_string();
        }
        let releases = list(recording, "releases");
        group.score = group.score.max(recording.get("score").and_then(Value::as_u64).unwrap_or(0));
        group.releases += releases.len();
        for release in releases {
            let Some(album) = release.get("release-group").filter(|album| is_album(album)) else { continue };
            // A release credited to someone else is a compilation, whatever it's filed as.
            let (_, by, _) = credit(release);
            if matches!(text(release, "status"), "Bootleg" | "Pseudo-Release") || by == VARIOUS {
                continue;
            }
            let date = text(release, "date");
            let earlier = group.offer.album.is_empty() || (!date.is_empty() && (group.date.is_empty() || date < group.date.as_str()));
            if earlier {
                group.date = date.to_string();
                group.offer.album = text(album, "id").to_string();
                group.offer.album_title = text(album, "title").to_string();
                group.offer.year = year(date);
                group.offer.seconds = (recording.get("length").and_then(Value::as_u64).unwrap_or(0) / 1000) as u32;
            }
        }
    }
    groups.retain(|group| !group.offer.album.is_empty());
    // Well below the best match is noise.
    let best = groups.iter().map(|group| group.score).max().unwrap_or(0);
    groups.retain(|group| group.score + 30 >= best);
    groups.sort_by(|a, b| b.score.cmp(&a.score).then(b.releases.cmp(&a.releases)));
    groups.into_iter().map(|group| group.offer).collect()
}

/// An artist's studio albums, newest first, and what the artist is called.
pub fn artist_albums(artist: &str) -> Result<(String, Vec<Offer>), String> {
    let mut name = String::new();
    let mut albums = Vec::new();
    // A hundred at a time; live albums and compilations are among them and are left out.
    for page in 0..4 {
        let found = get(&format!("release-group?artist={artist}&type=album&limit=100&offset={}&inc=artist-credits&fmt=json", page * 100))?;
        let groups = list(&found, "release-groups");
        for group in groups {
            let credits = list(group, "artist-credit");
            if name.is_empty() {
                let this = credits.iter().filter_map(|c| c.get("artist")).find(|a| text(a, "id") == artist);
                name = this.map(|a| text(a, "name").to_string()).unwrap_or_default();
            }
            if is_album(group) {
                let (by, _, _) = credit(group);
                albums.push(Offer {
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
        let songs = songs_from(&found);
        assert_eq!(songs.len(), 2);
        assert_eq!(
            songs[0],
            SongOffer {
                title: "Lonely No More".into(),
                artist: "Rob Thomas".into(),
                artist_id: "rt".into(),
                artist_name: "Rob Thomas".into(),
                album: "stb".into(),
                album_title: "…Something to Be".into(),
                year: "2005".into(),
                seconds: 227,
            }
        );
        assert_eq!((songs[1].artist.as_str(), songs[1].album.as_str(), songs[1].seconds), ("W.C. Clark", "lp", 0));
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
}
