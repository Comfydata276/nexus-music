//! Asking for albums that aren't in the library.
//!
//! Optional, and off unless switched on in Settings. Lidarr is a separate program some people
//! run beside their music server to keep track of the albums they want. This looks albums up
//! through it, adds one to its list when asked, and reports how the ones on that list are
//! getting on. Where Lidarr gets albums from is its own business and set up there.
//!
//! The rules are here; the connection is the app's (see `http`), and so is what it keeps of
//! what was offered and which Lidarr is in use.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::http::{Failure, Http, Method, Request, STOPPED};

/// How long to wait for Lidarr to learn an artist's albums after adding the artist.
const LEARN_ARTIST: Duration = Duration::from_secs(90);

#[derive(Clone)]
pub struct Lidarr {
    base: String,
    key: String,
    /// The sign-in that sits in front of Lidarr, if there is one and it's been signed in to.
    gate: Option<Arc<dyn SignIn>>,
    http: Arc<dyn Http>,
}

/// The sign-in that can sit in front of Lidarr: it hands out passes to send with requests.
pub trait SignIn: Send + Sync {
    /// A pass to send along with a request to Lidarr, fetched anew if the last has run out.
    fn pass(&self) -> Result<String, String>;
    /// The pass was turned away: don't hand it out again.
    fn forget_pass(&self);
}

/// What has been offered while a connection to Lidarr lasts: albums and artists by Lidarr's
/// names for them, so that one clicked on can be acted on, and whether each album has been
/// asked for. An app keeps one for as long as it's connected, and hands it to what offers
/// and asks.
pub struct Offers {
    albums: std::sync::RwLock<Option<HashMap<String, Offer>>>,
    artists: std::sync::RwLock<Option<HashMap<String, ArtistOffer>>>,
}

impl Default for Offers {
    fn default() -> Offers {
        Offers::new()
    }
}

impl Offers {
    pub const fn new() -> Offers {
        Offers { albums: std::sync::RwLock::new(None), artists: std::sync::RwLock::new(None) }
    }

    pub fn remember_artists(&self, artists: &[ArtistOffer]) {
        if let Ok(mut all) = self.artists.write() {
            let all = all.get_or_insert_with(HashMap::new);
            for artist in artists {
                // A later mention without a picture doesn't replace one with.
                if !artist.cover.is_empty() || !all.contains_key(&artist.id) {
                    all.insert(artist.id.clone(), artist.clone());
                }
            }
        }
    }

    pub fn offered_artist(&self, artist: &str) -> Option<ArtistOffer> {
        self.artists.read().ok()?.as_ref()?.get(artist).cloned()
    }

    pub fn remember(&self, offers: &[Offer]) {
        if let Ok(mut all) = self.albums.write() {
            let all = all.get_or_insert_with(HashMap::new);
            for offer in offers {
                all.insert(offer.album.clone(), offer.clone());
            }
        }
    }

    pub fn offered(&self, album: &str) -> Option<Offer> {
        self.albums.read().ok()?.as_ref()?.get(album).cloned()
    }

    /// Notes that an album has been asked for (or no longer is), so it's shown that way.
    pub fn mark(&self, album: &str, requested: bool) {
        if let Ok(mut all) = self.albums.write() {
            if let Some(offer) = all.as_mut().and_then(|all| all.get_mut(album)) {
                offer.requested = requested;
            }
        }
    }

    /// Forgets everything offered.
    pub fn forget(&self) {
        if let Ok(mut all) = self.albums.write() {
            *all = None;
        }
        if let Ok(mut all) = self.artists.write() {
            *all = None;
        }
    }
}

/// Said once Lidarr has answered, with its version.
pub fn connected(version: &str) -> String {
    format!("Connected to Lidarr {version}.")
}

/// Said while there's no Lidarr to ask.
pub const NOT_CONNECTED: &str = "Not connected yet.";
/// Said while Lidarr is being asked whether it's there.
pub const CHECKING: &str = "Checking…";
/// Said when the address has moved to another host, so the key kept for the last is gone.
pub const KEY_FORGOTTEN: &str = "That's a different address, so the saved key was forgotten. Enter the key for it.";

/// The app's `Http`, for one request that's to be seen through once it has begun changing
/// Lidarr (see `Lidarr::request`). Everything is passed on as it is.
struct SeeingThrough {
    http: Arc<dyn Http>,
    said: std::sync::atomic::AtomicBool,
}

impl Http for SeeingThrough {
    fn send(&self, request: &Request) -> Result<crate::http::Response, Failure> {
        if !matches!(request.method, Method::Get) && !self.said.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.http.keep_going();
        }
        self.http.send(request)
    }

    fn pause(&self, wait: Duration) -> Result<(), Failure> {
        self.http.pause(wait)
    }

    fn keep_going(&self) {
        self.http.keep_going()
    }
}

/// Said when Lidarr can't be reached without signing in.
pub const NEEDED: &str = "Lidarr is behind a sign-in. In Settings, under Requests, sign in with your browser.";

use crate::offer::{Offer, VARIOUS};

/// Where the way back from a change to Lidarr is written down **before the change is made**,
/// and crossed off once it has been put back: an artist's "new releases" setting, turned off
/// while their singles are read in. The batch of albums keeps it in its file
/// (`bulk::Job`), so that a run stopped before putting it back has it put back by the next.
pub trait WayBack {
    /// One step at a time, for everyone: reading an artist's setting, writing the way back
    /// and changing the setting are one step, and so are putting it back and crossing it off.
    /// The step is held while what this returns is. A request asked for by itself holds it
    /// for its whole sequence; the batch, for each step of its own. The window's thread never
    /// takes it.
    fn turn(&self) -> Box<dyn Turn + '_>;
    /// Writes down that `artist`'s setting on the Lidarr at `lidarr` (its address, as
    /// `Lidarr::address` has it) is to go back to `before`, and says whether it is on the disk:
    /// `Ok(true)`, written by this call; `Ok(false)`, a way back for the artist on that Lidarr
    /// was on the disk already, from before this step, and is that one's to cross off. If it
    /// can't be written, the answer is why, and Lidarr isn't changed. A way back is only ever
    /// acted on at the Lidarr it was written for: an artist's number on another means another
    /// artist.
    fn keep(&self, lidarr: &str, artist: i64, before: &str) -> Result<bool, String>;
    /// `artist`'s setting on the Lidarr at `lidarr` has gone back to `before`: the way back
    /// this step wrote is crossed off.
    fn put_back(&self, lidarr: &str, artist: i64, before: &str);
}

/// Held while a step of the way back is under way (`WayBack::turn`).
pub trait Turn {}

impl<T> Turn for T {}

/// The name of the profile this app makes in Lidarr for artists a single or an EP is asked
/// of. Lidarr only knows the kinds of release an artist's profile lists.
const SINGLES_PROFILE: &str = "Albums, EPs and singles";
/// And the one for artists a soundtrack is asked of, which lists those as well.
const SOUNDTRACKS_PROFILE: &str = "Albums, EPs, singles and soundtracks";

/// An artist Lidarr knows of.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ArtistOffer {
    /// Lidarr's name for the artist (a MusicBrainz id).
    pub id: String,
    pub name: String,
    /// A few words that tell artists of the same name apart; may be empty.
    pub about: String,
    /// Where a picture of them is, as a web address; may be empty.
    pub cover: String,
}

/// A song that was put out on its own, as Lidarr knows it. Not something that can be asked
/// for, but it says whose a well-known song is.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Single {
    pub title: String,
    pub artist_id: String,
    /// How many people have rated it at MusicBrainz: few for most, some for the famous.
    pub votes: i64,
}

/// An album on Lidarr's list that hasn't fully arrived.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Pending {
    /// Lidarr's own number for the album.
    pub id: i64,
    /// Its MusicBrainz id (Lidarr's `foreignAlbumId`): what tells it apart from another
    /// album of the same name.
    pub album: String,
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
    ratings: RatingsRes,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RatingsRes {
    votes: i64,
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
    total_records: usize,
}

/// How many albums (or downloads) Lidarr is asked for at a time when a list is read.
const PAGE: usize = 500;
/// The most pages of a list that are read: a stop for a server that never says it's done.
const PAGES: usize = 100;

/// A release Lidarr lists for an artist it keeps track of.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Listed {
    /// Its MusicBrainz id, and Lidarr's own number for it.
    pub album: String,
    pub id: i64,
    /// It's on Lidarr's list of things to get.
    pub wanted: bool,
    /// Lidarr has files for it.
    pub have: bool,
}

/// What a new artist is added with: where Lidarr keeps music, and the profiles it uses there.
pub struct Setup {
    folder: String,
    quality: i64,
    /// The profile for an artist only albums are wanted of, and the one that lists singles
    /// and EPs too, once that's been looked up (or made).
    albums: i64,
    singles: Option<i64>,
    /// And the one that lists soundtracks as well.
    soundtracks: Option<i64>,
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

/// Where an address leads: the scheme, the host and the port the app connects to. A saved
/// key (and a sign-in) belongs to one, and to no other.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Origin {
    /// "http" or "https".
    pub scheme: String,
    /// As written, in lower case (a name, an IPv4 address, or an IPv6 one in brackets).
    pub host: String,
    pub port: u16,
}

/// Said of an address with a name and password in it, which isn't kept.
pub const ADDRESS_WITH_NAME: &str = "That address has a name and password in it. Enter the address alone: Lidarr is reached with its API key.";
/// Said of an address the app can't be sure of reaching as written.
pub const ADDRESS_UNUSABLE: &str = "That isn't an address the app can use. Enter it as, say, https://lidarr.example.org or 192.168.1.5:8686.";

/// Where an address leads (see `Origin`), read as the app's HTTP client reads it, or why it
/// can't be used. Only the origin is judged: the scheme, the host and the port (by its number,
/// leading zeros and all). Anything there the client might read differently from this is
/// refused rather than guessed at: a backslash, a space, letters beyond plain ASCII, a port
/// that isn't a number. A name and password in the address are refused too. What comes after
/// the host and port (a path) is the address's own, as it always was.
pub fn origin(address: &str) -> Result<Origin, String> {
    let address = tidy(address);
    let (scheme, rest) = address.split_once("://").ok_or(ADDRESS_UNUSABLE)?;
    let judged = &address[..scheme.len() + 3 + rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    if !judged.is_ascii() || judged.chars().any(|c| c.is_ascii_control() || c.is_ascii_whitespace() || c == '\\') {
        return Err(ADDRESS_UNUSABLE.into());
    }
    let scheme = scheme.to_ascii_lowercase();
    let default = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return Err(ADDRESS_UNUSABLE.into()),
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(ADDRESS_WITH_NAME.into());
    }
    let (host, port) = if let Some(inside) = authority.strip_prefix('[') {
        // An IPv6 address, in brackets.
        let (inside, after) = inside.split_once(']').ok_or(ADDRESS_UNUSABLE)?;
        if inside.is_empty() || !inside.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.') {
            return Err(ADDRESS_UNUSABLE.into());
        }
        (format!("[{}]", inside.to_ascii_lowercase()), after)
    } else {
        let at = authority.find(':').unwrap_or(authority.len());
        let host = &authority[..at];
        if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '~')) {
            return Err(ADDRESS_UNUSABLE.into());
        }
        (host.to_ascii_lowercase(), &authority[at..])
    };
    let port = match port.strip_prefix(':') {
        None if port.is_empty() => default,
        Some(digits) if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) => match digits.trim_start_matches('0').parse::<u16>() {
            Ok(port) if port > 0 => port,
            _ => return Err(ADDRESS_UNUSABLE.into()),
        },
        _ => return Err(ADDRESS_UNUSABLE.into()),
    };
    Ok(Origin { scheme, host, port })
}

/// Where an address already saved leads, read as 0.40.10 used it: as `origin`, but a name and
/// password in it (a reverse proxy's HTTP Basic) are passed over, and an empty port (`host:`)
/// is the scheme's own. For an address loaded from what was kept, never one being typed: what
/// the person types or changes is held to `origin`. (The phone keeps `origin` always.)
pub fn origin_saved(address: &str) -> Result<Origin, String> {
    let address = tidy(address);
    let Some((scheme, rest)) = address.split_once("://") else { return origin(&address) };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, after) = rest.split_at(end);
    let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    let authority = authority.strip_suffix(':').unwrap_or(authority);
    origin(&format!("{scheme}://{authority}{after}"))
}

/// Where an address submitted in Settings leads: if it's the address already `saved`, as it
/// was saved (`origin_saved`: only a new key was typed); if it differs, by the rule for what's
/// typed (`origin`).
pub fn origin_entered(entered: &str, saved: &str) -> Result<Origin, String> {
    if entered.trim() == saved.trim() { origin_saved(entered) } else { origin(entered) }
}

/// Whether two addresses lead to the same place (see `origin`). One that can't be used is the
/// same as nothing.
pub fn same_origin(a: &str, b: &str) -> bool {
    matches!((origin(a), origin(b)), (Ok(a), Ok(b)) if a == b)
}

/// The host (and port) an address points at, as written (see `origin` for what a key belongs to).
pub fn host(address: &str) -> String {
    let address = tidy(address);
    let rest = address.split_once("://").map_or(address.as_str(), |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority.rsplit_once('@').map_or(authority, |(_, host)| host).to_lowercase()
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

/// Whether this is a plain studio album: the kind this feature deals in first.
fn is_album(album: &AlbumRes) -> bool {
    kind_of(album) == Some("Album")
}

/// Whether a release is filed as a soundtrack and nothing else besides (not a live one, or a
/// compilation of them).
fn is_soundtrack(album: &AlbumRes) -> bool {
    matches!(album.secondary_types.as_slice(), [only] if only.eq_ignore_ascii_case("soundtrack"))
}

/// The kind of release this is, if it's one that can be asked for: a studio release or a
/// soundtrack. No live albums, compilations, remixes and the like.
fn kind_of(album: &AlbumRes) -> Option<&'static str> {
    if !album.secondary_types.is_empty() && !is_soundtrack(album) {
        return None;
    }
    match album.album_type.to_lowercase().as_str() {
        "album" => Some("Album"),
        "ep" => Some("EP"),
        "single" => Some("Single"),
        _ => None,
    }
}

/// Whether a Lidarr profile lists releases of a kind ("Album", "EP", "Single").
fn allows(profile: &serde_json::Value, kind: &str) -> bool {
    listed(profile, "primaryAlbumTypes", "albumType").iter().any(|name| name == kind)
}

/// Whether a Lidarr profile lists releases like this one: its kind, and soundtracks if it's one.
fn suits(profile: &serde_json::Value, wanted: &Offer) -> bool {
    allows(profile, wanted.kind()) && (!wanted.soundtrack || listed(profile, "secondaryAlbumTypes", "albumType").iter().any(|name| name == "Soundtrack"))
}

/// The names switched on in one of a profile's three lists.
fn listed(profile: &serde_json::Value, list: &str, item: &str) -> Vec<String> {
    profile
        .get(list)
        .and_then(|entries| entries.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| entry.get("allowed").and_then(|allowed| allowed.as_bool()) == Some(true))
                .filter_map(|entry| entry.get(item)?.get("name")?.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

const PROFILE_LISTS: [(&str, &str); 3] = [("primaryAlbumTypes", "albumType"), ("secondaryAlbumTypes", "albumType"), ("releaseStatuses", "releaseStatus")];

/// Whether everything `small` lists, `big` lists too: moving an artist from one to the
/// other loses nothing.
fn covers(big: &serde_json::Value, small: &serde_json::Value) -> bool {
    PROFILE_LISTS.iter().all(|(list, item)| {
        let has = listed(big, list, item);
        listed(small, list, item).iter().all(|name| has.contains(name))
    })
}

/// A profile like `template` that lists studio albums, EPs and singles (and soundtracks, if
/// asked), official releases only, ready to be handed to Lidarr.
fn made_profile(template: &serde_json::Value, soundtracks: bool) -> serde_json::Value {
    let mut profile = template.clone();
    if let Some(fields) = profile.as_object_mut() {
        fields.remove("id");
        fields.insert("name".into(), if soundtracks { SOUNDTRACKS_PROFILE } else { SINGLES_PROFILE }.into());
    }
    let wanted: [&[&str]; 3] = [&["Album", "EP", "Single"], if soundtracks { &["Studio", "Soundtrack"] } else { &["Studio"] }, &["Official"]];
    for ((list, item), names) in PROFILE_LISTS.iter().zip(wanted) {
        for entry in profile.get_mut(*list).and_then(|entries| entries.as_array_mut()).into_iter().flatten() {
            let name = entry.get(*item).and_then(|kind| kind.get("name")).and_then(|name| name.as_str()).unwrap_or_default().to_string();
            entry["allowed"] = names.contains(&name.as_str()).into();
        }
    }
    profile
}

/// Which of these artists Lidarr's list of jobs has something queued or under way for. A
/// job that reads every artist in keeps them all busy.
fn busy_among(commands: &[serde_json::Value], artists: &std::collections::HashSet<i64>) -> std::collections::HashSet<i64> {
    let mut busy = std::collections::HashSet::new();
    for command in commands {
        let status = command.get("status").and_then(|status| status.as_str()).unwrap_or_default().to_lowercase();
        if status != "queued" && status != "started" {
            continue;
        }
        let name = command.get("name").and_then(|name| name.as_str()).unwrap_or_default();
        let body = command.get("body");
        let mut named: Vec<i64> =
            body.and_then(|body| body.get("artistIds")).and_then(|ids| ids.as_array()).into_iter().flatten().filter_map(|id| id.as_i64()).collect();
        named.extend(body.and_then(|body| body.get("artistId")).and_then(|id| id.as_i64()));
        if named.is_empty() && name == "RefreshArtist" {
            return artists.clone();
        }
        busy.extend(named.into_iter().filter(|id| artists.contains(id)));
    }
    busy
}

/// A number that stands for the jobs Lidarr has queued or under way, and how far each has got.
fn jobs_stamp(commands: &[serde_json::Value]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut jobs: Vec<(i64, String)> = commands
        .iter()
        .map(|command| (command.get("id").and_then(|id| id.as_i64()).unwrap_or_default(), command.get("status").and_then(|status| status.as_str()).unwrap_or_default().to_lowercase()))
        .filter(|(_, status)| status == "queued" || status == "started")
        .collect();
    jobs.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    jobs.hash(&mut hasher);
    hasher.finish()
}

fn offer(album: AlbumRes) -> Offer {
    Offer {
        soundtrack: is_soundtrack(&album),
        kind: kind_of(&album).filter(|kind| *kind != "Album").unwrap_or_default().to_string(),
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
    /// sign-in in front of it, if there is one. `http` must not follow redirects: Lidarr's
    /// own answers never are one, and a sign-in in front of it shows itself by sending one.
    pub fn new(address: &str, key: &str, gate: Option<Arc<dyn SignIn>>, http: Arc<dyn Http>) -> Lidarr {
        Lidarr { base: tidy(address), key: key.trim().to_string(), gate, http }
    }

    /// One request, as it went: the status and what came back. Every answer's body is read,
    /// whatever its status.
    fn once(&self, method: &str, url: &str, body: Option<&str>, pass: Option<&str>) -> Result<(u16, String), String> {
        let bearer = pass.map(|pass| format!("Bearer {pass}"));
        let mut headers = vec![("X-Api-Key", self.key.as_str())];
        let method = match body {
            None => Method::Get,
            Some(body) => {
                headers.push(("Content-Type", "application/json"));
                if method == "PUT" { Method::Put(body) } else { Method::Post(body) }
            }
        };
        if let Some(bearer) = &bearer {
            headers.push(("Authorization", bearer));
        }
        let response = self.http.send(&Request { method, url, headers: &headers, patience: None }).map_err(|failure| match failure {
            Failure::Reach(e) => format!("Couldn't reach Lidarr. {e}"),
            Failure::Stopped => STOPPED.to_string(),
        })?;
        let status = response.status;
        Ok((status, response.body().unwrap_or_default()))
    }

    /// The same Lidarr, asked through a view of the app's `Http` that tells it to see what's
    /// being done through just before the first request that changes anything is sent.
    fn seeing_through(&self) -> Lidarr {
        Lidarr { http: Arc::new(SeeingThrough { http: self.http.clone(), said: std::sync::atomic::AtomicBool::new(false) }), ..self.clone() }
    }

    /// Waits while Lidarr works, as the app waits.
    pub fn pause(&self, wait: Duration) -> Result<(), String> {
        self.http.pause(wait).map_err(|_| STOPPED.to_string())
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
            // The sign-in in front turned the pass away; it may only have run out early. A
            // write is sent again too, once, with a fresh pass: a 3xx comes from the sign-in in
            // front, which did not pass the request on; Lidarr's API does not redirect a write,
            // and a release or an artist added twice is refused by Lidarr as already there.
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
            300..=399 => Err(NEEDED.into()),
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

    /// Looks for albums matching what was typed, and the singles of that name too.
    pub fn find_releases(&self, term: &str) -> Result<(Vec<Offer>, Vec<Single>), String> {
        let found: Vec<AlbumRes> = self.get(&format!("album/lookup?term={}", crate::text::encode(term)))?;
        let singles = found
            .iter()
            .filter(|album| album.album_type.eq_ignore_ascii_case("single") && album.secondary_types.is_empty())
            .map(|album| Single { title: album.title.clone(), artist_id: album.artist.foreign_artist_id.clone(), votes: album.ratings.votes })
            .collect();
        Ok((found.into_iter().filter(is_album).map(offer).collect(), singles))
    }

    /// Looks for albums, EPs and singles matching what was typed. "lidarr:" followed by a
    /// release's name finds that one.
    pub fn find_any(&self, term: &str) -> Result<Vec<Offer>, String> {
        let found: Vec<AlbumRes> = self.get(&format!("album/lookup?term={}", crate::text::encode(term)))?;
        Ok(found.into_iter().filter(|album| kind_of(album).is_some()).map(offer).collect())
    }

    /// Looks for artists matching what was typed.
    pub fn find_artists(&self, term: &str) -> Result<Vec<ArtistOffer>, String> {
        let found: Vec<ArtistRes> = self.get(&format!("artist/lookup?term={}", crate::text::encode(term)))?;
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

    /// Every artist Lidarr keeps track of, by their MusicBrainz ids.
    pub fn tracked(&self) -> Result<std::collections::HashSet<String>, String> {
        let artists: Vec<ArtistRes> = self.get("artist")?;
        Ok(artists.into_iter().map(|known| known.foreign_artist_id).collect())
    }

    /// Lidarr's own number and name for an artist, if it keeps track of them.
    pub fn known_artist(&self, artist: &str) -> Result<Option<(i64, String)>, String> {
        let artists: Vec<ArtistRes> = self.get("artist")?;
        Ok(artists.into_iter().find(|known| known.foreign_artist_id == artist).map(|known| (known.id, known.artist_name)))
    }

    /// The studio albums of an artist Lidarr keeps track of, newest first, and whether each
    /// has arrived.
    pub fn albums_of(&self, artist: i64, name: &str) -> Result<Vec<(Offer, bool)>, String> {
        Ok(self.releases_of(artist, name)?.into_iter().filter(|(offer, _)| offer.kind() == "Album").collect())
    }

    /// The same, with whatever EPs and singles Lidarr lists for the artist too.
    pub fn releases_of(&self, artist: i64, name: &str) -> Result<Vec<(Offer, bool)>, String> {
        let albums: Vec<AlbumRes> = self.get(&format!("album?artistId={artist}"))?;
        let mut albums: Vec<(Offer, bool)> = albums
            .into_iter()
            .filter(|album| kind_of(album).is_some())
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
    /// ones will as they come out. Returns how many albums that put on the list. Seen through
    /// once it has changed anything, as `request` is, and one step at a time with every other
    /// change to an artist's record (`WayBack::turn`): it reads the record and writes it back.
    pub fn add_artist(&self, artist: &str, name: &str, way_back: &dyn WayBack) -> Result<usize, String> {
        self.seeing_through().adding_artist(artist, name, way_back)
    }

    fn adding_artist(&self, artist: &str, name: &str, way_back: &dyn WayBack) -> Result<usize, String> {
        let _turn = way_back.turn();
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

    /// Puts an album (or an EP, or a single) on Lidarr's list and has Lidarr start looking
    /// for it. Nothing else by the artist is asked for.
    ///
    /// It takes several steps, and once the first has changed anything on Lidarr (an artist
    /// added, a profile made, a setting turned off, an album marked) the rest are seen through,
    /// or what was changed is put back: an album marked and never looked for, or an artist
    /// left not taking new releases, would be worse than no request. So a stop is honoured only
    /// before the first change; just before it, the app is told to see this through
    /// (`Http::keep_going`).
    pub fn request(&self, wanted: &Offer, way_back: &dyn WayBack) -> Result<(), String> {
        self.seeing_through().requesting(wanted, way_back)
    }

    fn requesting(&self, wanted: &Offer, way_back: &dyn WayBack) -> Result<(), String> {
        // The whole of it, one at a time with every other change that has to be put back
        // (`WayBack::turn`): it is a few requests long. An album by Various Artists too: it
        // reads their record and writes it back.
        let _turn = way_back.turn();
        if wanted.artist_id == VARIOUS {
            return self.request_from_various(wanted);
        }
        let kind = wanted.kind();
        // An album by an artist Lidarr lists the albums of needs nothing special.
        let plain = kind == "Album" && !wanted.soundtrack;
        let artists: Vec<ArtistRes> = self.get("artist")?;
        let known = artists.into_iter().find(|artist| artist.foreign_artist_id == wanted.artist_id).map(|artist| artist.id);
        // What the artist's "new releases" setting was, if it had to be turned off meanwhile.
        let mut put_back: Option<(String, bool)> = None;
        let artist = match known {
            Some(artist) => {
                if !plain {
                    put_back = self.list_singles(artist, wanted, way_back)?;
                }
                artist
            }
            None => {
                // New to Lidarr. Added with none of their albums wanted, or Lidarr would
                // set about fetching everything they've made.
                let folders: Vec<RootFolder> = self.get("rootfolder")?;
                let folder = folders.first().ok_or("Lidarr has no music folder set up yet.")?;
                let profile = if plain {
                    folder.default_metadata_profile_id
                } else {
                    let profiles: Vec<serde_json::Value> = self.get("metadataprofile")?;
                    self.profile_for(&profiles, wanted.soundtrack)?.get("id").and_then(|id| id.as_i64()).ok_or("Lidarr's profile for singles has no number.")?
                };
                let added: ArtistRes = self.send(
                    "POST",
                    "artist",
                    Some(serde_json::json!({
                        "foreignArtistId": wanted.artist_id,
                        "artistName": wanted.artist,
                        "qualityProfileId": folder.default_quality_profile_id,
                        "metadataProfileId": profile,
                        "rootFolderPath": folder.path,
                        "monitored": true,
                        "monitorNewItems": "none",
                        "addOptions": { "monitor": "none", "searchForMissingAlbums": false },
                    })),
                )?;
                added.id
            }
        };
        let outcome = self.place(artist, wanted);
        match put_back {
            Some((before, wrote)) => {
                let restored = self.restore_new_releases(artist, &before);
                // Crossed off only if this request wrote it.
                if restored.is_ok() && wrote {
                    way_back.put_back(self.address(), artist, &before);
                }
                outcome.and(restored.map_err(|_| {
                    format!("Asked for “{}”, but couldn't turn new releases back on for {} in Lidarr. Check that artist's settings there.", wanted.title, wanted.artist)
                }))
            }
            None => outcome,
        }
    }

    /// The profile that lists EPs and singles as well as albums (and soundtracks, if asked),
    /// made if Lidarr hasn't one.
    fn profile_for(&self, profiles: &[serde_json::Value], soundtracks: bool) -> Result<serde_json::Value, String> {
        let name = if soundtracks { SOUNDTRACKS_PROFILE } else { SINGLES_PROFILE };
        if let Some(ours) = profiles.iter().find(|profile| profile.get("name").and_then(|known| known.as_str()) == Some(name)) {
            return Ok(ours.clone());
        }
        let template = profiles.first().ok_or("Lidarr has no profiles to copy from.")?;
        self.send("POST", "metadataprofile", Some(made_profile(template, soundtracks)))
    }

    /// Asks for an album credited to "Various Artists", as most film soundtracks are. That
    /// isn't an artist whose releases Lidarr can read in: it holds only the albums put
    /// there one at a time, so this one is put there, wanted.
    fn request_from_various(&self, wanted: &Offer) -> Result<(), String> {
        let known = self.artist_numbers()?.get(VARIOUS).copied();
        if let Some(artist) = known {
            if let Some(there) = self.listed(artist)?.into_iter().find(|there| there.album == wanted.album) {
                return if there.wanted || there.have { Ok(()) } else { self.want(artist, &[there.id]) };
            }
        }
        // Lidarr's own description of the album, which is what it takes back to add one.
        let found: Vec<serde_json::Value> = self.get(&format!("album/lookup?term={}", crate::text::encode(&format!("lidarr:{}", wanted.album))))?;
        let mut album = found
            .into_iter()
            .find(|album| album.get("foreignAlbumId").and_then(|id| id.as_str()) == Some(wanted.album.as_str()))
            .ok_or("Lidarr no longer lists that one.")?;
        let folders: Vec<RootFolder> = self.get("rootfolder")?;
        let folder = folders.first().ok_or("Lidarr has no music folder set up yet.")?;
        let profile = if wanted.kind() == "Album" && !wanted.soundtrack {
            folder.default_metadata_profile_id
        } else {
            let profiles: Vec<serde_json::Value> = self.get("metadataprofile")?;
            self.profile_for(&profiles, wanted.soundtrack)?.get("id").and_then(|id| id.as_i64()).ok_or("Lidarr's profile for soundtracks has no number.")?
        };
        album["monitored"] = true.into();
        album["addOptions"] = serde_json::json!({ "addType": "manual", "searchForNewAlbum": true });
        if !album.get("artist").is_some_and(|artist| artist.is_object()) {
            album["artist"] = serde_json::json!({ "foreignArtistId": VARIOUS, "artistName": wanted.artist });
        }
        let artist = &mut album["artist"];
        artist["qualityProfileId"] = folder.default_quality_profile_id.into();
        artist["metadataProfileId"] = profile.into();
        artist["rootFolderPath"] = folder.path.as_str().into();
        artist["monitored"] = true.into();
        artist["monitorNewItems"] = "none".into();
        artist["addOptions"] = serde_json::json!({ "monitor": "unknown", "albumsToMonitor": [wanted.album], "searchForMissingAlbums": false });
        let _: serde_json::Value = self.send("POST", "album", Some(album))?;
        // If "Various Artists" was there already, Lidarr must be watching it to look for this.
        if let Some(artist) = known {
            let mut record: serde_json::Value = self.get(&format!("artist/{artist}"))?;
            if record.get("monitored").and_then(|m| m.as_bool()) != Some(true) {
                record["monitored"] = true.into();
                let _: serde_json::Value = self.send("PUT", &format!("artist/{artist}"), Some(record))?;
            }
        }
        Ok(())
    }

    /// Has Lidarr list an artist's EPs and singles, if it doesn't already, so one of them can
    /// be asked for. Returns the artist's "new releases" setting if it was turned off for
    /// the change, to be put back afterwards.
    ///
    /// Left as it was, Lidarr would take every single and EP it now learns of to be a new
    /// release and fetch the lot. With the setting off while it reads them in, they're
    /// listed and nothing more; turning it back on afterwards doesn't change that.
    fn list_singles(&self, artist: i64, wanted: &Offer, way_back: &dyn WayBack) -> Result<Option<(String, bool)>, String> {
        let before = self.start_listing_singles_kept(artist, wanted, way_back)?;
        if before.is_some() {
            // Lidarr starts reading the artist's releases in again; give it a moment to begin.
            // A wait cut short ends here, but the setting is still handed back to be put back.
            let _ = self.pause(Duration::from_secs(3));
        }
        Ok(before)
    }

    /// The same, without waiting for Lidarr to begin, for a caller that watches for it to
    /// finish (see `busy_with`) and puts the setting back itself (see `set_new_releases`). The
    /// caller holds `way_back`'s turn. What the setting is to be put back to is written down
    /// (`WayBack::keep`) **before anything on Lidarr is changed**, the profile made for this
    /// included: if it can't be, nothing is. Returns it, and whether this wrote it. The
    /// requests sent to Lidarr, and their order, are 0.40.10's.
    pub fn start_listing_singles_kept(&self, artist: i64, wanted: &Offer, way_back: &dyn WayBack) -> Result<Option<(String, bool)>, String> {
        let profiles: Vec<serde_json::Value> = self.get("metadataprofile")?;
        let mut record: serde_json::Value = self.get(&format!("artist/{artist}"))?;
        let current = profiles.iter().find(|profile| profile.get("id") == record.get("metadataProfileId"));
        if current.is_some_and(|profile| suits(profile, wanted)) {
            return Ok(None);
        }
        let before = record.get("monitorNewItems").and_then(|setting| setting.as_str()).unwrap_or("all").to_string();
        let wrote = way_back.keep(self.address(), artist, &before)?;
        // Turned back before the setting is changed: nothing to put back, and what was
        // written is crossed off.
        let refused = |reason: String| {
            if wrote {
                way_back.put_back(self.address(), artist, &before);
            }
            reason
        };
        let ours = self.profile_for(&profiles, wanted.soundtrack).map_err(&refused)?;
        if let Some(current) = current.filter(|current| !covers(&ours, current)) {
            let name = current.get("name").and_then(|name| name.as_str()).unwrap_or("its own");
            return Err(refused(format!(
                "{} is on Lidarr's “{name}” profile, which doesn't list {}s. Add them to that profile in Lidarr, then ask again.",
                wanted.artist,
                if wanted.soundtrack && allows(current, wanted.kind()) { "soundtrack" } else { wanted.kind_word() }
            )));
        }
        record["metadataProfileId"] = ours.get("id").cloned().unwrap_or_default();
        record["monitorNewItems"] = "none".into();
        // (A change that fails here may have been made all the same: what was written stays.)
        let _: serde_json::Value = self.send("PUT", &format!("artist/{artist}"), Some(record))?;
        Ok(Some((before, wrote)))
    }

    /// Sets what Lidarr does about an artist's new releases ("all", "new" or "none").
    pub fn set_new_releases(&self, artist: i64, setting: &str) -> Result<(), String> {
        let mut record: serde_json::Value = self.get(&format!("artist/{artist}"))?;
        if record.get("monitorNewItems").and_then(|now| now.as_str()) == Some(setting) {
            return Ok(());
        }
        record["monitorNewItems"] = setting.into();
        self.send::<serde_json::Value>("PUT", &format!("artist/{artist}"), Some(record)).map(drop)
    }

    // ---- many at once (see `bulk`) -------------------------------------------------------------

    /// Where this Lidarr is: what tells one from another.
    pub fn address(&self) -> &str {
        &self.base
    }

    /// Lidarr's own numbers for the artists it keeps track of, by their MusicBrainz ids.
    pub fn artist_numbers(&self) -> Result<HashMap<String, i64>, String> {
        let artists: Vec<ArtistRes> = self.get("artist")?;
        Ok(artists.into_iter().map(|artist| (artist.foreign_artist_id, artist.id)).collect())
    }

    /// What's needed to add an artist.
    pub fn setup(&self) -> Result<Setup, String> {
        let folders: Vec<RootFolder> = self.get("rootfolder")?;
        let folder = folders.first().ok_or("Lidarr has no music folder set up yet.")?;
        Ok(Setup { folder: folder.path.clone(), quality: folder.default_quality_profile_id, albums: folder.default_metadata_profile_id, singles: None, soundtracks: None })
    }

    /// Adds an artist Lidarr doesn't have with these releases of theirs wanted and none of
    /// the rest, in one call. Lidarr reads the artist in, marks the releases named and
    /// starts looking for them by itself, in its own time: nothing has to wait for it, and
    /// nothing is lost if this app closes meanwhile. Returns Lidarr's number for the artist.
    pub fn add_wanting(&self, setup: &mut Setup, wanted: &[Offer]) -> Result<i64, String> {
        let first = wanted.first().ok_or("Nothing was asked for.")?;
        let soundtracks = wanted.iter().any(|offer| offer.soundtrack);
        let profile = if wanted.iter().all(|offer| offer.kind() == "Album" && !offer.soundtrack) {
            setup.albums
        } else {
            match if soundtracks { setup.soundtracks } else { setup.singles } {
                Some(id) => id,
                None => {
                    let profiles: Vec<serde_json::Value> = self.get("metadataprofile")?;
                    let id = self.profile_for(&profiles, soundtracks)?.get("id").and_then(|id| id.as_i64()).ok_or("Lidarr's profile for singles has no number.")?;
                    if soundtracks {
                        setup.soundtracks = Some(id);
                    } else {
                        setup.singles = Some(id);
                    }
                    id
                }
            }
        };
        let albums: Vec<&str> = wanted.iter().map(|offer| offer.album.as_str()).collect();
        let added: ArtistRes = self.send(
            "POST",
            "artist",
            Some(serde_json::json!({
                "foreignArtistId": first.artist_id,
                "artistName": first.artist,
                "qualityProfileId": setup.quality,
                "metadataProfileId": profile,
                "rootFolderPath": setup.folder,
                "monitored": true,
                "monitorNewItems": "none",
                // With releases named, Lidarr marks those and no others whatever "monitor"
                // says. "none" isn't said, because that also stops Lidarr watching the artist.
                "addOptions": { "monitor": "unknown", "albumsToMonitor": albums, "searchForMissingAlbums": true },
            })),
        )?;
        Ok(added.id)
    }

    /// What Lidarr lists for an artist it keeps track of.
    pub fn listed(&self, artist: i64) -> Result<Vec<Listed>, String> {
        let albums: Vec<AlbumRes> = self.get(&format!("album?artistId={artist}"))?;
        Ok(albums
            .into_iter()
            .map(|album| Listed { wanted: album.monitored, have: album.statistics.track_file_count > 0, id: album.id, album: album.foreign_album_id })
            .collect())
    }

    /// Marks releases of an artist Lidarr has as wanted, and starts the search for them.
    pub fn want(&self, artist: i64, albums: &[i64]) -> Result<(), String> {
        if albums.is_empty() {
            return Ok(());
        }
        // Lidarr only looks for albums of artists it's watching.
        let mut record: serde_json::Value = self.get(&format!("artist/{artist}"))?;
        if record.get("monitored").and_then(|m| m.as_bool()) != Some(true) {
            record["monitored"] = true.into();
            let _: serde_json::Value = self.send("PUT", &format!("artist/{artist}"), Some(record))?;
        }
        let _: serde_json::Value = self.send("PUT", "album/monitor", Some(serde_json::json!({ "albumIds": albums, "monitored": true })))?;
        self.send::<serde_json::Value>("POST", "command", Some(serde_json::json!({ "name": "AlbumSearch", "albumIds": albums }))).map(drop)
    }

    /// Takes releases off Lidarr's list.
    pub fn unwant(&self, albums: &[i64]) -> Result<(), String> {
        if albums.is_empty() {
            return Ok(());
        }
        self.send::<serde_json::Value>("PUT", "album/monitor", Some(serde_json::json!({ "albumIds": albums, "monitored": false }))).map(drop)
    }

    /// Which of these artists Lidarr is in the middle of something for: reading them in,
    /// looking through their folder, or waiting its turn to.
    pub fn busy_with(&self, artists: &std::collections::HashSet<i64>) -> Result<std::collections::HashSet<i64>, String> {
        self.busy(artists).map(|(busy, _)| busy)
    }

    /// The same, with a number that changes whenever Lidarr's list of jobs does: a way to
    /// tell a Lidarr that's working through a long list from one that's stuck.
    pub fn busy(&self, artists: &std::collections::HashSet<i64>) -> Result<(std::collections::HashSet<i64>, u64), String> {
        let commands: Vec<serde_json::Value> = self.get("command")?;
        Ok((busy_among(&commands, artists), jobs_stamp(&commands)))
    }

    /// Puts an artist's "new releases" setting back, once Lidarr has finished with them.
    fn restore_new_releases(&self, artist: i64, before: &str) -> Result<(), String> {
        if before == "none" {
            return Ok(());
        }
        let started = std::time::Instant::now();
        while self.busy_with_artists()? && started.elapsed() < LEARN_ARTIST {
            // A wait cut short ends the waiting, not the putting back.
            if self.pause(Duration::from_secs(2)).is_err() {
                break;
            }
        }
        let mut record: serde_json::Value = self.get(&format!("artist/{artist}"))?;
        record["monitorNewItems"] = before.into();
        self.send::<serde_json::Value>("PUT", &format!("artist/{artist}"), Some(record)).map(drop)
    }

    /// Marks a release of an artist Lidarr has as wanted, and starts the search for it.
    fn place(&self, artist: i64, wanted: &Offer) -> Result<(), String> {
        // Lidarr learns an artist's albums a little after the artist is added.
        let started = std::time::Instant::now();
        let album = loop {
            let albums: Vec<AlbumRes> = self.get(&format!("album?artistId={artist}"))?;
            if let Some(album) = albums.into_iter().find(|album| album.foreign_album_id == wanted.album) {
                break album.id;
            }
            if started.elapsed() > LEARN_ARTIST {
                return Err(format!("Lidarr has {} but doesn't list “{}” among their releases.", wanted.artist, wanted.title));
            }
            self.pause(Duration::from_secs(3))?;
        };
        // Lidarr goes on setting a new artist up for a few seconds after their albums appear,
        // and finishes by marking everything unwanted: wait for that, or the request is lost.
        while self.busy_with_artists()? && started.elapsed() < LEARN_ARTIST {
            self.pause(Duration::from_secs(2))?;
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
            self.pause(Duration::from_secs(3))?;
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

    /// Every record of one of Lidarr's lists that come a page at a time.
    fn every<T: DeserializeOwned + Default>(&self, path: &str) -> Result<Vec<T>, String> {
        let mut all: Vec<T> = Vec::new();
        for page in 1..=PAGES {
            let join = if path.contains('?') { '&' } else { '?' };
            let got: Page<T> = self.get(&format!("{path}{join}page={page}&pageSize={PAGE}"))?;
            let count = got.records.len();
            all.extend(got.records);
            if count < PAGE || (got.total_records > 0 && all.len() >= got.total_records) {
                break;
            }
        }
        Ok(all)
    }

    /// The albums on Lidarr's list that haven't fully arrived, those being downloaded first.
    pub fn pending(&self) -> Result<Vec<Pending>, String> {
        let wanted: Vec<AlbumRes> = self.every("wanted/missing?includeArtist=true")?;
        let queue: Vec<QueueItem> = self.every("queue")?;
        let mut progress: HashMap<i64, u8> = HashMap::new();
        for item in queue.iter().filter(|item| item.size > 0.0) {
            let done = (((item.size - item.sizeleft) / item.size) * 100.0).clamp(0.0, 100.0) as u8;
            let best = progress.entry(item.album_id).or_default();
            *best = (*best).max(done);
        }
        let mut pending: Vec<Pending> = wanted
            .into_iter()
            .map(|album| Pending {
                progress: progress.get(&album.id).copied(),
                id: album.id,
                album: album.foreign_album_id,
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn an_address_submitted_unchanged_is_judged_as_saved() {
        let saved = "http://me:secret@lidarr.example.org:8686";
        // Only a new key typed: the address as it was saved.
        assert_eq!(origin_entered(saved, saved), origin_saved(saved));
        assert_eq!(origin_entered(&format!(" {saved} "), saved), origin_saved(saved));
        // Changed: by the rule for what's typed.
        assert_eq!(origin_entered("http://me:other@lidarr.example.org:8686", saved).unwrap_err(), ADDRESS_WITH_NAME);
        assert_eq!(origin_entered("http://lidarr.example.org:", saved).unwrap_err(), ADDRESS_UNUSABLE);
    }

    #[test]
    fn a_saved_address_is_read_as_0_40_10_used_it_and_a_typed_one_held_to_the_rule() {
        let at = |scheme: &str, host: &str, port: u16| Ok(Origin { scheme: scheme.into(), host: host.into(), port });
        // A name and password (a reverse proxy's HTTP Basic), and an empty port: refused when
        // typed, read as 0.40.10 used them when saved.
        assert_eq!(origin("http://me:secret@lidarr.example.org:8686").unwrap_err(), ADDRESS_WITH_NAME);
        assert_eq!(origin_saved("http://me:secret@lidarr.example.org:8686"), at("http", "lidarr.example.org", 8686));
        assert_eq!(origin_saved("https://me:s%40cret@Lidarr.example.org/lidarr"), at("https", "lidarr.example.org", 443));
        assert_eq!(origin("http://lidarr.example.org:").unwrap_err(), ADDRESS_UNUSABLE);
        assert_eq!(origin_saved("http://lidarr.example.org:/lidarr"), at("http", "lidarr.example.org", 80));
        // Otherwise the same reading.
        assert_eq!(origin_saved("lidarr.example.org:08686"), origin("lidarr.example.org:08686"));
        assert!(origin_saved("http://lid\\arr.example.org").is_err() && origin_saved("ftp://lidarr.example.org").is_err());
    }

    #[test]
    fn tidies_addresses() {
        assert_eq!(tidy(" https://lidarr.example.org/ "), "https://lidarr.example.org");
        assert_eq!(tidy("192.168.1.5:8686"), "http://192.168.1.5:8686");
        assert_eq!(tidy(""), "");
    }

    #[test]
    fn finds_the_host_a_key_belongs_to() {
        assert_eq!(host("https://Lidarr.example.org/lidarr/"), "lidarr.example.org");
        assert_eq!(host("http://lidarr.example.org"), "lidarr.example.org");
        assert_eq!(host("192.168.1.5:8686"), "192.168.1.5:8686");
        assert_ne!(host("192.168.1.5:8686"), host("192.168.1.5:8687"));
        assert_ne!(host("https://lidarr.example.org"), host("https://lidarr.example.com"));
        assert_eq!(host(""), "");
    }

    #[test]
    fn masks_keys() {
        assert_eq!(mask("1565b829294744838c094584fd123456"), "••••••••••123456");
        assert_eq!(mask(" 1565b829294744838c094584fd123456\n"), "••••••••••123456");
        assert_eq!(mask("short-key"), "••••••••••••••••");
    }

    #[test]
    fn makes_a_profile_that_lists_singles() {
        let entry = |item: &str, name: &str, allowed: bool| serde_json::json!({ item: { "id": 1, "name": name }, "allowed": allowed });
        let standard = serde_json::json!({
            "id": 1, "name": "Standard",
            "primaryAlbumTypes": [entry("albumType", "Single", false), entry("albumType", "EP", false), entry("albumType", "Album", true), entry("albumType", "Other", false)],
            "secondaryAlbumTypes": [entry("albumType", "Studio", true), entry("albumType", "Live", false)],
            "releaseStatuses": [entry("releaseStatus", "Official", true), entry("releaseStatus", "Bootleg", false)],
        });
        assert!(allows(&standard, "Album") && !allows(&standard, "Single"));
        let ours = made_profile(&standard, false);
        assert_eq!(ours.get("id"), None);
        assert_eq!(ours["name"], SINGLES_PROFILE);
        assert!(allows(&ours, "Album") && allows(&ours, "EP") && allows(&ours, "Single") && !allows(&ours, "Other"));
        assert_eq!(listed(&ours, "secondaryAlbumTypes", "albumType"), ["Studio"]);
        assert_eq!(listed(&ours, "releaseStatuses", "releaseStatus"), ["Official"]);
        // Moving an artist from Standard to it loses nothing; from one that lists live albums, it would.
        assert!(covers(&ours, &standard));
        let mut live = standard.clone();
        live["secondaryAlbumTypes"][1]["allowed"] = true.into();
        assert!(!covers(&ours, &live));
    }

    #[test]
    fn makes_a_profile_that_lists_soundtracks_too() {
        let entry = |item: &str, name: &str, allowed: bool| serde_json::json!({ item: { "id": 1, "name": name }, "allowed": allowed });
        let standard = serde_json::json!({
            "id": 1, "name": "Standard",
            "primaryAlbumTypes": [entry("albumType", "Single", false), entry("albumType", "EP", false), entry("albumType", "Album", true)],
            "secondaryAlbumTypes": [entry("albumType", "Studio", true), entry("albumType", "Soundtrack", false), entry("albumType", "Live", false)],
            "releaseStatuses": [entry("releaseStatus", "Official", true)],
        });
        let singles = made_profile(&standard, false);
        let ours = made_profile(&standard, true);
        assert_eq!(ours["name"], SOUNDTRACKS_PROFILE);
        assert_eq!(listed(&ours, "secondaryAlbumTypes", "albumType"), ["Studio", "Soundtrack"]);
        // It lists everything the other two do, so an artist can be moved to it from either.
        assert!(covers(&ours, &standard) && covers(&ours, &singles) && !covers(&singles, &ours));
        let single = Offer { kind: "Single".into(), ..Default::default() };
        let from_a_film = Offer { kind: "Single".into(), soundtrack: true, ..Default::default() };
        let score = Offer { soundtrack: true, ..Default::default() };
        assert!(suits(&standard, &Offer::default()) && !suits(&standard, &single) && !suits(&standard, &score));
        assert!(suits(&singles, &single) && !suits(&singles, &from_a_film));
        assert!(suits(&ours, &single) && suits(&ours, &from_a_film) && suits(&ours, &score));
    }

    #[test]
    fn offers_soundtracks_but_not_live_ones() {
        let found: Vec<AlbumRes> = serde_json::from_str(
            r#"[
            {"title":"Still Falling for You","foreignAlbumId":"s1","albumType":"Single","secondaryTypes":["Soundtrack"],"artist":{"artistName":"Ellie Goulding","foreignArtistId":"e1"}},
            {"title":"Still Falling for You (live)","foreignAlbumId":"s2","albumType":"Single","secondaryTypes":["Live","Soundtrack"],"artist":{"artistName":"Ellie Goulding","foreignArtistId":"e1"}},
            {"title":"Hits from Films","foreignAlbumId":"s3","albumType":"Album","secondaryTypes":["Compilation","Soundtrack"],"artist":{"artistName":"Various Artists","foreignArtistId":"v"}},
            {"title":"Sing","foreignAlbumId":"s4","albumType":"Album","secondaryTypes":["Soundtrack"],"artist":{"artistName":"Various Artists","foreignArtistId":"v"}}
            ]"#,
        )
        .unwrap();
        let offers: Vec<Offer> = found.into_iter().filter(|album| kind_of(album).is_some()).map(offer).collect();
        assert_eq!(offers.iter().map(|offer| (offer.album.as_str(), offer.kind(), offer.soundtrack)).collect::<Vec<_>>(), [("s1", "Single", true), ("s4", "Album", true)]);
    }

    #[test]
    fn tells_which_artists_lidarr_is_busy_with() {
        let commands: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
            {"name":"RefreshArtist","status":"started","body":{"artistIds":[10],"isNewArtist":true}},
            {"name":"RefreshArtist","status":"queued","body":{"artistIds":[11,12]}},
            {"name":"RescanFolders","status":"queued","body":{"folders":["/music/x"],"artistIds":[13]}},
            {"name":"MissingAlbumSearch","status":"started","body":{"artistId":14}},
            {"name":"RefreshArtist","status":"completed","body":{"artistIds":[15]}},
            {"name":"RssSync","status":"started","body":{}}
            ]"#,
        )
        .unwrap();
        let asked: std::collections::HashSet<i64> = [10, 12, 13, 14, 15, 16].into();
        let mut busy: Vec<i64> = busy_among(&commands, &asked).into_iter().collect();
        busy.sort_unstable();
        assert_eq!(busy, [10, 12, 13, 14]);
        // Lidarr's own round of every artist keeps them all busy.
        let all: Vec<serde_json::Value> = serde_json::from_str(r#"[{"name":"RefreshArtist","status":"started","body":{"artistIds":[]}}]"#).unwrap();
        assert_eq!(busy_among(&all, &asked), asked);
        // A job finishing, or starting, shows.
        let step = |text: &str| jobs_stamp(&serde_json::from_str::<Vec<serde_json::Value>>(text).unwrap());
        let before = step(r#"[{"id":1,"status":"started"},{"id":2,"status":"queued"},{"id":3,"status":"queued"}]"#);
        assert_eq!(before, step(r#"[{"id":3,"status":"queued"},{"id":0,"status":"completed"},{"id":2,"status":"queued"},{"id":1,"status":"started"}]"#));
        assert_ne!(before, step(r#"[{"id":1,"status":"completed"},{"id":2,"status":"started"},{"id":3,"status":"queued"}]"#));
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
                kind: String::new(),
                soundtrack: false,
            }
        );
        assert!(offers[1].requested);
        assert_eq!(offers[1].year, "");
    }

    /// A Lidarr that keeps one artist, on the Standard profile with new releases wanted, and
    /// is forever busy reading artists in; asked over an app that has been told to stop, so
    /// that every wait is cut short. Every request is written down.
    pub(crate) struct Stopping {
        pub(crate) asked: std::sync::Mutex<Vec<String>>,
    }

    impl crate::http::Http for Stopping {
        fn send(&self, request: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
            use crate::http::Method;
            let (method, body) = match request.method {
                Method::Get => ("GET", ""),
                Method::Post(body) => ("POST", body),
                Method::Put(body) => ("PUT", body),
            };
            let path = request.url.trim_start_matches("http://lidarr.test/api/v1/");
            self.asked.lock().unwrap().push(format!("{method} {path} {body}"));
            let entry = |item: &str, name: &str, allowed: bool| serde_json::json!({ item: { "id": 1, "name": name }, "allowed": allowed });
            let profile = |id: i64, name: &str, singles: bool| {
                serde_json::json!({ "id": id, "name": name,
                    "primaryAlbumTypes": [entry("albumType", "Album", true), entry("albumType", "Single", singles), entry("albumType", "EP", singles)],
                    "secondaryAlbumTypes": [entry("albumType", "Studio", true)],
                    "releaseStatuses": [entry("releaseStatus", "Official", true)] })
            };
            let answer = match (method, path) {
                ("GET", "artist") => r#"[{"id":5,"artistName":"Someone","foreignArtistId":"ar1"}]"#.to_string(),
                ("GET", "metadataprofile") => serde_json::json!([profile(1, "Standard", false), profile(2, SINGLES_PROFILE, true)]).to_string(),
                ("GET", "artist/5") => r#"{"id":5,"metadataProfileId":1,"monitorNewItems":"all","monitored":true}"#.to_string(),
                ("GET", "album?artistId=5") => r#"[{"id":9,"title":"A Single","foreignAlbumId":"al1","albumType":"Single"}]"#.to_string(),
                ("GET", "command") => r#"[{"name":"RefreshArtist","status":"started"}]"#.to_string(),
                _ => "{}".to_string(),
            };
            Ok(crate::http::Response::new(200, Vec::new(), move || Ok(answer)))
        }

        fn pause(&self, _: Duration) -> Result<(), crate::http::Failure> {
            Err(crate::http::Failure::Stopped)
        }
    }

    #[test]
    fn a_setting_changed_for_a_while_is_put_back_however_the_wait_ends() {
        // Asking for a single of an artist whose profile lists albums only: their "new
        // releases" setting is turned off while Lidarr reads their singles in, and is put back
        // afterwards, even though every wait for Lidarr is cut short.
        let http = Arc::new(Stopping { asked: Default::default() });
        let lidarr = Lidarr::new("lidarr.test", "key", None, http.clone());
        let single = Offer { album: "al1".into(), artist_id: "ar1".into(), title: "A Single".into(), artist: "Someone".into(), kind: "Single".into(), ..Default::default() };
        let noted = Noted::default();
        assert_eq!(lidarr.request(&single, &noted), Err(STOPPED.to_string()));
        let puts: Vec<String> = http.asked.lock().unwrap().iter().filter(|asked| asked.starts_with("PUT artist/5")).cloned().collect();
        assert_eq!(puts.len(), 2, "{puts:?}");
        assert!(puts[0].contains(r#""monitorNewItems":"none""#) && puts[0].contains(r#""metadataProfileId":2"#), "{}", puts[0]);
        assert!(puts[1].contains(r#""monitorNewItems":"all""#), "{}", puts[1]);
        // Written down before the change, and crossed off once it was put back.
        assert_eq!((noted.kept(), noted.put_back.lock().unwrap().clone()), (vec![(5, "all".to_string())], vec![(5, "all".to_string())]));
    }

    /// Writes the way back down in memory; or, `refuse`, can't.
    #[derive(Default)]
    pub(crate) struct Noted {
        kept: std::sync::Mutex<Vec<(i64, String)>>,
        put_back: std::sync::Mutex<Vec<(i64, String)>>,
        refuse: bool,
    }

    impl Noted {
        pub(crate) fn kept(&self) -> Vec<(i64, String)> {
            self.kept.lock().unwrap().clone()
        }
    }

    impl WayBack for Noted {
        fn turn(&self) -> Box<dyn Turn + '_> {
            Box::new(())
        }

        fn keep(&self, _: &str, artist: i64, before: &str) -> Result<bool, String> {
            if self.refuse {
                return Err("couldn't be written down".into());
            }
            self.kept.lock().unwrap().push((artist, before.to_string()));
            Ok(true)
        }

        fn put_back(&self, _: &str, artist: i64, before: &str) {
            self.put_back.lock().unwrap().push((artist, before.to_string()));
        }
    }

    /// The same Lidarr, and the app killed the moment Lidarr has been changed in a way that
    /// has to be put back.
    pub(crate) struct KilledAfterChange(pub(crate) Stopping);

    impl crate::http::Http for KilledAfterChange {
        fn send(&self, request: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
            let answer = self.0.send(request);
            if self.0.asked.lock().unwrap().last().is_some_and(|asked| asked.starts_with("PUT artist/5") && asked.contains(r#""monitorNewItems":"none""#)) {
                panic!("killed");
            }
            answer
        }

        fn pause(&self, wait: Duration) -> Result<(), crate::http::Failure> {
            self.0.pause(wait)
        }
    }

    pub(crate) const SINGLES: &str = SINGLES_PROFILE;

    /// The same Lidarr without the profile for singles, which is made for the request; and
    /// the app killed the moment it is asked to make it.
    struct KilledMakingTheProfile(Stopping);

    impl crate::http::Http for KilledMakingTheProfile {
        fn send(&self, request: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
            if matches!(request.method, crate::http::Method::Post(_)) && request.url.ends_with("/metadataprofile") {
                self.0.asked.lock().unwrap().push("POST metadataprofile".into());
                panic!("killed");
            }
            let answer = self.0.send(request)?;
            if request.url.ends_with("/metadataprofile") {
                let only_standard = r#"[{"id":1,"name":"Standard","primaryAlbumTypes":[{"albumType":{"id":1,"name":"Album"},"allowed":true}],"secondaryAlbumTypes":[{"albumType":{"id":1,"name":"Studio"},"allowed":true}],"releaseStatuses":[{"releaseStatus":{"id":1,"name":"Official"},"allowed":true}]}]"#;
                return Ok(crate::http::Response::new(200, Vec::new(), move || Ok(only_standard.to_string())));
            }
            Ok(answer)
        }

        fn pause(&self, wait: Duration) -> Result<(), crate::http::Failure> {
            self.0.pause(wait)
        }
    }

    #[test]
    fn the_way_back_is_written_down_before_a_profile_is_made_and_lidarr_is_asked_as_before() {
        let http = Arc::new(KilledMakingTheProfile(Stopping { asked: Default::default() }));
        let lidarr = Lidarr::new("lidarr.test", "key", None, http.clone());
        let noted = Noted::default();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lidarr.request(&single(), &noted))).is_err());
        assert_eq!(noted.kept(), [(5, "all".to_string())]);
        // The requests, and their order, are 0.40.10's: the profiles, the artist, the profile made.
        let asked: Vec<String> = http.0.asked.lock().unwrap().iter().map(|asked| asked.split(' ').take(2).collect::<Vec<_>>().join(" ")).collect();
        assert_eq!(asked, ["GET artist", "GET metadataprofile", "GET artist/5", "POST metadataprofile"]);
    }

    pub(crate) fn single() -> Offer {
        Offer { album: "al1".into(), artist_id: "ar1".into(), title: "A Single".into(), artist: "Someone".into(), kind: "Single".into(), ..Default::default() }
    }

    #[test]
    fn a_change_to_lidarr_that_has_to_be_put_back_is_written_down_first() {
        // Killed the moment the setting was turned off: what it goes back to was written down
        // before, so the next run puts it back.
        let lidarr = Lidarr::new("lidarr.test", "key", None, Arc::new(KilledAfterChange(Stopping { asked: Default::default() })));
        let noted = Noted::default();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lidarr.request(&single(), &noted))).is_err());
        assert_eq!(noted.kept(), [(5, "all".to_string())]);
        // It can't be written down: Lidarr isn't changed, and the request says why.
        let http = Arc::new(Stopping { asked: Default::default() });
        let lidarr = Lidarr::new("lidarr.test", "key", None, http.clone());
        let refusing = Noted { refuse: true, ..Default::default() };
        let done = lidarr.request(&single(), &refusing);
        assert!(done.as_ref().is_err_and(|reason| reason.contains("couldn't be written down")), "{done:?}");
        assert!(!http.asked.lock().unwrap().iter().any(|asked| asked.starts_with("PUT artist/5")), "Lidarr was changed");
    }

    /// A Lidarr that keeps one artist, with the album wanted listed, and an app that stops
    /// what's being done once `stop_at` has been sent; it goes on answering a request the
    /// library has said must be seen through (`Http::keep_going`).
    struct Phone {
        stop_at: &'static str,
        stopped: std::sync::atomic::AtomicBool,
        seeing_through: std::sync::atomic::AtomicBool,
        asked: std::sync::Mutex<Vec<String>>,
    }

    impl Phone {
        fn new(stop_at: &'static str) -> Phone {
            Phone { stop_at, stopped: Default::default(), seeing_through: Default::default(), asked: Default::default() }
        }

        fn stops(&self) -> bool {
            self.stopped.load(std::sync::atomic::Ordering::SeqCst) && !self.seeing_through.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl crate::http::Http for Phone {
        fn send(&self, request: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
            use crate::http::Method;
            if self.stops() {
                return Err(crate::http::Failure::Stopped);
            }
            let method = match request.method {
                Method::Get => "GET",
                Method::Post(_) => "POST",
                Method::Put(_) => "PUT",
            };
            let path = request.url.trim_start_matches("http://lidarr.test/api/v1/").to_string();
            let line = format!("{method} {path}");
            self.asked.lock().unwrap().push(line.clone());
            let answer = match line.as_str() {
                "GET artist" => r#"[{"id":5,"artistName":"Someone","foreignArtistId":"ar1"}]"#,
                "GET album?artistId=5" => r#"[{"id":9,"title":"First","foreignAlbumId":"al1","albumType":"Album"}]"#,
                "GET command" => "[]",
                "GET artist/5" => r#"{"id":5,"monitored":true}"#,
                "GET album/9" => r#"{"id":9,"monitored":true}"#,
                _ => "{}",
            };
            // The stop comes once this has been answered.
            if line == self.stop_at {
                self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(crate::http::Response::new(200, Vec::new(), move || Ok(answer.to_string())))
        }

        fn pause(&self, _: Duration) -> Result<(), crate::http::Failure> {
            if self.stops() { Err(crate::http::Failure::Stopped) } else { Ok(()) }
        }

        fn keep_going(&self) {
            self.seeing_through.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn requested(stop_at: &'static str) -> (Result<(), String>, Vec<String>) {
        let http = Arc::new(Phone::new(stop_at));
        let lidarr = Lidarr::new("http://lidarr.test", "key", None, http.clone());
        let wanted = Offer { album: "al1".into(), artist_id: "ar1".into(), title: "First".into(), artist: "Someone".into(), ..Default::default() };
        let done = lidarr.request(&wanted, &Noted::default());
        let asked = http.asked.lock().unwrap().clone();
        (done, asked)
    }

    #[test]
    fn a_request_lidarr_has_been_changed_for_is_seen_through() {
        // Stopped once the album is marked: Lidarr is still told to look for it.
        let (done, asked) = requested("PUT album/monitor");
        assert_eq!(done, Ok(()));
        assert_eq!(asked.last().map(String::as_str), Some("POST command"));
        // Stopped before anything was changed: it stops, and nothing is changed.
        let (done, asked) = requested("GET album?artistId=5");
        assert_eq!(done, Err(STOPPED.to_string()));
        assert!(asked.iter().all(|line| line.starts_with("GET ")), "{asked:?}");
    }
}
