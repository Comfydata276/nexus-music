//! Asking for albums that aren't in the library.
//!
//! Optional, and off unless switched on in Settings. Lidarr is a separate program some people
//! run beside their music server to keep track of the albums they want. This looks albums up
//! through it, adds one to its list when asked, and reports how the ones on that list are
//! getting on. Where Lidarr gets albums from is its own business and set up there.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

/// How long to wait for Lidarr to learn an artist's albums after adding the artist.
const LEARN_ARTIST: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub struct Lidarr {
    base: String,
    key: String,
    /// The sign-in that sits in front of Lidarr, if there is one and it's been signed in to.
    gate: Option<Arc<crate::gate::Session>>,
    agent: ureq::Agent,
}

/// An album Lidarr knows of that could be asked for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Offer {
    /// Lidarr's name for the album (a MusicBrainz release-group id) and for its artist.
    pub album: String,
    pub artist_id: String,
    pub title: String,
    pub artist: String,
    pub year: String,
    /// Where its cover picture is, as a web address; may be empty.
    pub cover: String,
    /// It's already on Lidarr's list of albums to get.
    pub requested: bool,
}

/// An artist Lidarr knows of.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArtistOffer {
    /// Lidarr's name for the artist (a MusicBrainz id).
    pub id: String,
    pub name: String,
    /// A few words that tell artists of the same name apart; may be empty.
    pub about: String,
    /// Where a picture of them is, as a web address; may be empty.
    pub cover: String,
}

/// An album on Lidarr's list that hasn't fully arrived.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pending {
    /// Lidarr's own number for the album.
    pub id: i64,
    pub title: String,
    pub artist: String,
    /// How far along, from 0 to 100, if it's being downloaded; `None` while it's still
    /// being looked for.
    pub progress: Option<u8>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct ArtistRes {
    id: i64,
    artist_name: String,
    foreign_artist_id: String,
    disambiguation: String,
    images: Vec<ImageRes>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct ImageRes {
    cover_type: String,
    remote_url: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct StatsRes {
    track_file_count: i64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct AlbumRes {
    id: i64,
    title: String,
    foreign_album_id: String,
    album_type: String,
    secondary_types: Vec<String>,
    release_date: String,
    remote_cover: String,
    monitored: bool,
    artist_id: i64,
    artist: ArtistRes,
    images: Vec<ImageRes>,
    statistics: StatsRes,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RootFolder {
    path: String,
    default_quality_profile_id: i64,
    default_metadata_profile_id: i64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Page<T> {
    records: Vec<T>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct QueueItem {
    album_id: i64,
    size: f64,
    sizeleft: f64,
}

/// An address as typed, made into one that can be called.
pub fn tidy(address: &str) -> String {
    let address = address.trim().trim_end_matches('/');
    if address.is_empty() || address.contains("://") { address.to_string() } else { format!("http://{address}") }
}

/// A key for showing: dots, then its last six characters, enough to tell which key it is.
/// A key too short for that to be safe is all dots.
pub fn mask(key: &str) -> String {
    let letters: Vec<char> = key.trim().chars().collect();
    let shown = if letters.len() >= 16 { letters[letters.len() - 6..].iter().collect() } else { String::new() };
    format!("{}{shown}", "•".repeat(if shown.is_empty() { 16 } else { 10 }))
}

/// The first picture of one of the kinds wanted, as a web address; empty if there's none.
fn picture(images: &[ImageRes], kinds: &[&str]) -> String {
    kinds
        .iter()
        .find_map(|kind| images.iter().find(|image| image.cover_type.eq_ignore_ascii_case(kind) && !image.remote_url.is_empty()))
        .map(|image| image.remote_url.clone())
        .unwrap_or_default()
}

/// Whether this is a plain studio album: the kind this feature deals in.
fn is_album(album: &AlbumRes) -> bool {
    album.album_type.eq_ignore_ascii_case("album") && album.secondary_types.is_empty()
}

fn offer(album: AlbumRes) -> Offer {
    Offer {
        requested: album.id > 0 && album.monitored,
        year: album.release_date.get(..4).filter(|year| *year != "0001").unwrap_or_default().to_string(),
        album: album.foreign_album_id,
        artist_id: album.artist.foreign_artist_id,
        title: album.title,
        artist: album.artist.artist_name,
        cover: album.remote_cover,
    }
}

impl Lidarr {
    /// `address` is where Lidarr is, e.g. "https://lidarr.example.org". `gate` is the
    /// sign-in in front of it, if there is one.
    pub fn new(address: &str, key: &str, gate: Option<Arc<crate::gate::Session>>) -> Lidarr {
        // Redirects aren't followed: Lidarr's own answers never are one, and a sign-in in
        // front of it shows itself by sending one.
        Lidarr { base: tidy(address), key: key.trim().to_string(), gate, agent: crate::subsonic::make_agent_without_redirects() }
    }

    /// One request, as it went: the status and what came back.
    fn once(&self, method: &str, url: &str, body: Option<&str>, pass: Option<&str>) -> Result<(u16, String), String> {
        let unreachable = |e: ureq::Error| format!("Couldn't reach Lidarr. {e}");
        let response = match body {
            None => {
                let mut request = self.agent.get(url).header("X-Api-Key", &self.key);
                if let Some(pass) = pass {
                    request = request.header("Authorization", &format!("Bearer {pass}"));
                }
                request.call().map_err(unreachable)?
            }
            Some(body) => {
                let request = if method == "PUT" { self.agent.put(url) } else { self.agent.post(url) };
                let mut request = request.header("X-Api-Key", &self.key).header("Content-Type", "application/json");
                if let Some(pass) = pass {
                    request = request.header("Authorization", &format!("Bearer {pass}"));
                }
                request.send(body).map_err(unreachable)?
            }
        };
        let status = response.status().as_u16();
        Ok((status, response.into_body().read_to_string().unwrap_or_default()))
    }

    fn send<T: DeserializeOwned>(&self, method: &str, path: &str, body: Option<serde_json::Value>) -> Result<T, String> {
        let url = format!("{}/api/v1/{path}", self.base);
        let body = match (method, body) {
            ("GET", _) => None,
            (_, body) => Some(body.unwrap_or(serde_json::Value::Null).to_string()),
        };
        let pass = self.gate.as_ref().map(|gate| gate.pass()).transpose()?;
        let (mut status, mut text) = self.once(method, &url, body.as_deref(), pass.as_deref())?;
        if (300..400).contains(&status) {
            // The sign-in in front turned the pass away; it may only have run out early.
            if let Some(gate) = &self.gate {
                gate.forget_pass();
                let pass = gate.pass()?;
                (status, text) = self.once(method, &url, body.as_deref(), Some(&pass))?;
                if (300..400).contains(&status) {
                    return Err("You're signed in, but the sign-in in front of Lidarr still turned the app away. \
                                The proxy in front of Lidarr may not be passing the sign-in on."
                        .into());
                }
            }
        }
        match status {
            200..=299 => serde_json::from_str(if text.trim().is_empty() { "null" } else { &text })
                .map_err(|_| "That address answered, but not as Lidarr. Check the address.".to_string()),
            300..=399 => Err(crate::gate::NEEDED.into()),
            401 | 403 => Err("Lidarr turned the request away. Check the API key.".into()),
            _ => Err(format!("Lidarr answered with an error ({status}).")),
        }
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        self.send("GET", path, None)
    }

    /// Checks the address and key by asking Lidarr which version it is.
    pub fn version(&self) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Status {
            version: String,
        }
        self.get::<Status>("system/status").map(|status| status.version)
    }

    /// Looks for albums matching what was typed.
    pub fn find_albums(&self, term: &str) -> Result<Vec<Offer>, String> {
        let found: Vec<AlbumRes> = self.get(&format!("album/lookup?term={}", crate::subsonic::encode(term)))?;
        Ok(found.into_iter().filter(is_album).map(offer).collect())
    }

    /// Looks for artists matching what was typed.
    pub fn find_artists(&self, term: &str) -> Result<Vec<ArtistOffer>, String> {
        let found: Vec<ArtistRes> = self.get(&format!("artist/lookup?term={}", crate::subsonic::encode(term)))?;
        Ok(found
            .into_iter()
            .filter(|artist| !artist.foreign_artist_id.is_empty())
            .map(|artist| ArtistOffer {
                cover: picture(&artist.images, &["poster", "fanart"]),
                id: artist.foreign_artist_id,
                name: artist.artist_name,
                about: artist.disambiguation,
            })
            .collect())
    }

    /// Lidarr's own number and name for an artist, if it keeps track of them.
    pub fn known_artist(&self, artist: &str) -> Result<Option<(i64, String)>, String> {
        let artists: Vec<ArtistRes> = self.get("artist")?;
        Ok(artists.into_iter().find(|known| known.foreign_artist_id == artist).map(|known| (known.id, known.artist_name)))
    }

    /// The studio albums of an artist Lidarr keeps track of, newest first, and whether each
    /// has arrived.
    pub fn albums_of(&self, artist: i64, name: &str) -> Result<Vec<(Offer, bool)>, String> {
        let albums: Vec<AlbumRes> = self.get(&format!("album?artistId={artist}"))?;
        let mut albums: Vec<(Offer, bool)> = albums
            .into_iter()
            .filter(is_album)
            .map(|album| {
                let have = album.statistics.track_file_count > 0;
                let cover = picture(&album.images, &["cover", "disc"]);
                let mut offer = offer(album);
                offer.requested &= !have;
                offer.artist = name.to_string();
                offer.cover = if cover.is_empty() { crate::musicbrainz::cover(&offer.album) } else { cover };
                (offer, have)
            })
            .collect();
        albums.sort_by(|a, b| b.0.year.cmp(&a.0.year).then(a.0.title.cmp(&b.0.title)));
        Ok(albums)
    }

    /// Has Lidarr keep track of an artist: every album of theirs goes on its list, and new
    /// ones will as they come out. Returns how many albums that put on the list.
    pub fn add_artist(&self, artist: &str, name: &str) -> Result<usize, String> {
        let Some((known, _)) = self.known_artist(artist)? else {
            let folders: Vec<RootFolder> = self.get("rootfolder")?;
            let folder = folders.first().ok_or("Lidarr has no music folder set up yet.")?;
            let _: ArtistRes = self.send(
                "POST",
                "artist",
                Some(serde_json::json!({
                    "foreignArtistId": artist,
                    "artistName": name,
                    "qualityProfileId": folder.default_quality_profile_id,
                    "metadataProfileId": folder.default_metadata_profile_id,
                    "rootFolderPath": folder.path,
                    "monitored": true,
                    "monitorNewItems": "all",
                    "addOptions": { "monitor": "all", "searchForMissingAlbums": true },
                })),
            )?;
            // Lidarr learns their albums in its own time; how many isn't known yet.
            return Ok(0);
        };
        let mut record: serde_json::Value = self.get(&format!("artist/{known}"))?;
        record["monitored"] = true.into();
        record["monitorNewItems"] = "all".into();
        let _: serde_json::Value = self.send("PUT", &format!("artist/{known}"), Some(record))?;
        let albums: Vec<AlbumRes> = self.get(&format!("album?artistId={known}"))?;
        let wanted: Vec<i64> =
            albums.iter().filter(|album| !album.monitored && album.statistics.track_file_count == 0).map(|album| album.id).collect();
        if !wanted.is_empty() {
            let _: serde_json::Value =
                self.send("PUT", "album/monitor", Some(serde_json::json!({ "albumIds": wanted, "monitored": true })))?;
        }
        let _: serde_json::Value = self.send("POST", "command", Some(serde_json::json!({ "name": "ArtistSearch", "artistId": known })))?;
        Ok(wanted.len())
    }

    /// Puts an album on Lidarr's list and has Lidarr start looking for it. Nothing else by
    /// the artist is asked for.
    pub fn request(&self, wanted: &Offer) -> Result<(), String> {
        let artists: Vec<ArtistRes> = self.get("artist")?;
        let artist = match artists.into_iter().find(|artist| artist.foreign_artist_id == wanted.artist_id) {
            Some(artist) => artist.id,
            None => {
                // New to Lidarr. Added with none of their albums wanted, or Lidarr would
                // set about fetching everything they've made.
                let folders: Vec<RootFolder> = self.get("rootfolder")?;
                let folder = folders.first().ok_or("Lidarr has no music folder set up yet.")?;
                let added: ArtistRes = self.send(
                    "POST",
                    "artist",
                    Some(serde_json::json!({
                        "foreignArtistId": wanted.artist_id,
                        "artistName": wanted.artist,
                        "qualityProfileId": folder.default_quality_profile_id,
                        "metadataProfileId": folder.default_metadata_profile_id,
                        "rootFolderPath": folder.path,
                        "monitored": true,
                        "monitorNewItems": "none",
                        "addOptions": { "monitor": "none", "searchForMissingAlbums": false },
                    })),
                )?;
                added.id
            }
        };
        // Lidarr learns an artist's albums a little after the artist is added.
        let started = std::time::Instant::now();
        let album = loop {
            let albums: Vec<AlbumRes> = self.get(&format!("album?artistId={artist}"))?;
            if let Some(album) = albums.into_iter().find(|album| album.foreign_album_id == wanted.album) {
                break album.id;
            }
            if started.elapsed() > LEARN_ARTIST {
                return Err(format!("Lidarr added {} but doesn't list “{}” among their albums.", wanted.artist, wanted.title));
            }
            std::thread::sleep(Duration::from_secs(3));
        };
        // Lidarr goes on setting a new artist up for a few seconds after their albums appear,
        // and finishes by marking everything unwanted: wait for that, or the request is lost.
        while self.busy_with_artists()? && started.elapsed() < LEARN_ARTIST {
            std::thread::sleep(Duration::from_secs(2));
        }
        // Mark the album wanted, then make sure it stayed that way.
        for attempt in 0..4 {
            // Lidarr only looks for albums of artists it's watching.
            let mut record: serde_json::Value = self.get(&format!("artist/{artist}"))?;
            if record.get("monitored").and_then(|m| m.as_bool()) != Some(true) {
                record["monitored"] = true.into();
                let _: serde_json::Value = self.send("PUT", &format!("artist/{artist}"), Some(record))?;
            }
            let _: serde_json::Value =
                self.send("PUT", "album/monitor", Some(serde_json::json!({ "albumIds": [album], "monitored": true })))?;
            std::thread::sleep(Duration::from_secs(3));
            let now: AlbumRes = self.get(&format!("album/{album}"))?;
            let watching: serde_json::Value = self.get(&format!("artist/{artist}"))?;
            if now.monitored && watching.get("monitored").and_then(|m| m.as_bool()) == Some(true) {
                let _: serde_json::Value =
                    self.send("POST", "command", Some(serde_json::json!({ "name": "AlbumSearch", "albumIds": [album] })))?;
                return Ok(());
            }
            if attempt == 3 {
                break;
            }
        }
        Err(format!("Lidarr wouldn't keep “{}” on its list. Try again in a minute.", wanted.title))
    }

    /// Whether Lidarr is in the middle of setting artists up.
    fn busy_with_artists(&self) -> Result<bool, String> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Command {
            name: String,
            status: String,
        }
        let commands: Vec<Command> = self.get("command")?;
        Ok(commands.iter().any(|command| {
            command.name.contains("Artist") && matches!(command.status.to_lowercase().as_str(), "queued" | "started")
        }))
    }

    /// The albums on Lidarr's list that haven't fully arrived, those being downloaded first.
    pub fn pending(&self) -> Result<Vec<Pending>, String> {
        let wanted: Page<AlbumRes> = self.get("wanted/missing?pageSize=200&includeArtist=true")?;
        let queue: Page<QueueItem> = self.get("queue?pageSize=200")?;
        let mut progress: HashMap<i64, u8> = HashMap::new();
        for item in queue.records.iter().filter(|item| item.size > 0.0) {
            let done = (((item.size - item.sizeleft) / item.size) * 100.0).clamp(0.0, 100.0) as u8;
            let best = progress.entry(item.album_id).or_default();
            *best = (*best).max(done);
        }
        let mut pending: Vec<Pending> = wanted
            .records
            .into_iter()
            .map(|album| Pending {
                progress: progress.get(&album.id).copied(),
                id: album.id,
                title: album.title,
                artist: album.artist.artist_name,
            })
            .collect();
        pending.sort_by_key(|album| (album.progress.is_none(), album.artist.to_lowercase(), album.title.to_lowercase()));
        Ok(pending)
    }

    /// Takes an album off Lidarr's list.
    pub fn cancel(&self, id: i64) -> Result<(), String> {
        self.send::<serde_json::Value>("PUT", "album/monitor", Some(serde_json::json!({ "albumIds": [id], "monitored": false })))
            .map(drop)
    }
}

// ---- the connection in use, and what it has offered -----------------------------------------------

static CURRENT: RwLock<Option<Lidarr>> = RwLock::new(None);
/// Albums that have been offered, by Lidarr's name for them, so a click on one can be acted on.
static OFFERS: RwLock<Option<HashMap<String, Offer>>> = RwLock::new(None);

pub fn current() -> Option<Lidarr> {
    CURRENT.read().ok()?.clone()
}

pub fn set_current(lidarr: Option<Lidarr>) {
    if let Ok(mut current) = CURRENT.write() {
        *current = lidarr;
    }
}

/// Artists that have been offered, by Lidarr's name for them.
static ARTISTS: RwLock<Option<HashMap<String, ArtistOffer>>> = RwLock::new(None);

pub fn remember_artists(artists: &[ArtistOffer]) {
    if let Ok(mut all) = ARTISTS.write() {
        let all = all.get_or_insert_with(HashMap::new);
        for artist in artists {
            // A later mention without a picture doesn't replace one with.
            if !artist.cover.is_empty() || !all.contains_key(&artist.id) {
                all.insert(artist.id.clone(), artist.clone());
            }
        }
    }
}

pub fn offered_artist(artist: &str) -> Option<ArtistOffer> {
    ARTISTS.read().ok()?.as_ref()?.get(artist).cloned()
}

pub fn remember(offers: &[Offer]) {
    if let Ok(mut all) = OFFERS.write() {
        let all = all.get_or_insert_with(HashMap::new);
        for offer in offers {
            all.insert(offer.album.clone(), offer.clone());
        }
    }
}

pub fn offered(album: &str) -> Option<Offer> {
    OFFERS.read().ok()?.as_ref()?.get(album).cloned()
}

/// Notes that an album has been asked for (or no longer is), so it's shown that way.
pub fn mark(album: &str, requested: bool) {
    if let Ok(mut all) = OFFERS.write() {
        if let Some(offer) = all.as_mut().and_then(|all| all.get_mut(album)) {
            offer.requested = requested;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tidies_addresses() {
        assert_eq!(tidy(" https://lidarr.example.org/ "), "https://lidarr.example.org");
        assert_eq!(tidy("192.168.1.5:8686"), "http://192.168.1.5:8686");
        assert_eq!(tidy(""), "");
    }

    #[test]
    fn masks_keys() {
        assert_eq!(mask("1565b829294744838c094584fd123456"), "••••••••••123456");
        assert_eq!(mask(" 1565b829294744838c094584fd123456\n"), "••••••••••123456");
        assert_eq!(mask("short-key"), "••••••••••••••••");
    }

    #[test]
    fn offers_plain_albums_only() {
        let found: Vec<AlbumRes> = serde_json::from_str(
            r#"[
            {"title":"The Good Witch","foreignAlbumId":"a1","albumType":"Album","secondaryTypes":[],"releaseDate":"2023-06-23T00:00:00Z",
             "remoteCover":"https://example.org/c.jpg","monitored":false,"artist":{"artistName":"Maisie Peters","foreignArtistId":"m1"}},
            {"title":"Birthday","foreignAlbumId":"a2","albumType":"Single","secondaryTypes":[],"artist":{"artistName":"Maisie Peters","foreignArtistId":"m1"}},
            {"title":"Live Somewhere","foreignAlbumId":"a3","albumType":"Album","secondaryTypes":["Live"],"artist":{"artistName":"Someone","foreignArtistId":"s1"}},
            {"id":7,"title":"Already Wanted","foreignAlbumId":"a4","albumType":"Album","secondaryTypes":[],"releaseDate":"0001-01-01T00:00:00Z","monitored":true,
             "artist":{"artistName":"Someone","foreignArtistId":"s1"}}
            ]"#,
        )
        .unwrap();
        let offers: Vec<Offer> = found.into_iter().filter(is_album).map(offer).collect();
        assert_eq!(offers.len(), 2);
        assert_eq!(
            offers[0],
            Offer {
                album: "a1".into(),
                artist_id: "m1".into(),
                title: "The Good Witch".into(),
                artist: "Maisie Peters".into(),
                year: "2023".into(),
                cover: "https://example.org/c.jpg".into(),
                requested: false,
            }
        );
        assert!(offers[1].requested);
        assert_eq!(offers[1].year, "");
    }
}
