//! Asking for albums that aren't in the library.
//!
//! Optional, and off unless switched on in Settings. Lidarr is a separate program some people
//! run beside their music server to keep track of the albums they want. This looks albums up
//! through it, adds one to its list when asked, and reports how the ones on that list are
//! getting on. Where Lidarr gets albums from is its own business and set up there.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

/// How long to wait for Lidarr to learn an artist's albums after adding the artist.
const LEARN_ARTIST: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub struct Lidarr {
    base: String,
    key: String,
    /// For a sign-in that sits in front of Lidarr: the value of an `Authorization` header.
    proxy: Option<String>,
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

/// Standard base64, for the sign-in header.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
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
    /// `address` is where Lidarr is, e.g. "https://lidarr.example.org". `sign_in` is a name
    /// and password for a login that sits in front of it, if there is one.
    pub fn new(address: &str, key: &str, sign_in: Option<(&str, &str)>) -> Lidarr {
        let address = address.trim().trim_end_matches('/');
        let base = if address.contains("://") { address.to_string() } else { format!("http://{address}") };
        Lidarr {
            base,
            key: key.trim().to_string(),
            proxy: sign_in.map(|(name, password)| format!("Basic {}", base64(format!("{name}:{password}").as_bytes()))),
            agent: crate::subsonic::make_agent(),
        }
    }

    fn send<T: DeserializeOwned>(&self, method: &str, path: &str, body: Option<serde_json::Value>) -> Result<T, String> {
        let url = format!("{}/api/v1/{path}", self.base);
        let unreachable = |e: ureq::Error| format!("Couldn't reach Lidarr. {e}");
        let response = match (method, body) {
            ("GET", _) => {
                let mut request = self.agent.get(&url).header("X-Api-Key", &self.key);
                if let Some(proxy) = &self.proxy {
                    request = request.header("Authorization", proxy);
                }
                request.call().map_err(unreachable)?
            }
            (method, body) => {
                let request = if method == "PUT" { self.agent.put(&url) } else { self.agent.post(&url) };
                let mut request = request.header("X-Api-Key", &self.key).header("Content-Type", "application/json");
                if let Some(proxy) = &self.proxy {
                    request = request.header("Authorization", proxy);
                }
                request.send(body.unwrap_or(serde_json::Value::Null).to_string()).map_err(unreachable)?
            }
        };
        let status = response.status().as_u16();
        let text = response.into_body().read_to_string().unwrap_or_default();
        match status {
            200..=299 => serde_json::from_str(if text.trim().is_empty() { "null" } else { &text }).map_err(|_| {
                // A sign-in page, most likely: something in front of Lidarr wants a login.
                "That address answered, but not as Lidarr. If Lidarr sits behind a sign-in, fill in the name and password for it.".to_string()
            }),
            401 | 403 => Err("Lidarr turned the request away. Check the API key (and the sign-in, if there's one in front of it).".into()),
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
    fn encodes_the_sign_in() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"name:pass word"), "bmFtZTpwYXNzIHdvcmQ=");
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
