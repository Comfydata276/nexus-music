//! What the pages and sentences of requests come to, pinned against stand-ins for Lidarr,
//! MusicBrainz and ListenBrainz: written against the app as it was before the deciding moved
//! to the library (`nexus_core::requests`), so that the move can be seen to change nothing.
//!
//! Each scene sets up what the services answer, what the library holds, what's been asked
//! for and offered, and the "singles and EPs" setting; goes through the page as the window
//! does (`pages::load` and `pages::enrich`, by route); and compares every stage of the page
//! it produces, and every request made, with what the old code produced, kept in
//! `tests/fixtures/requests/`. `NEXUS_RECORD=1` writes those files afresh: only ever on the
//! old code.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nexus_core::http::{Failure, Http, Request, Response};
use serde_json::{Value, json};

use crate::pages::{CardData, Kind, PageData};
use crate::controller::Route;
use crate::search::{Index, Library};
use crate::subsonic::Song;

/// The one stand-in, answering for Lidarr, MusicBrainz and ListenBrainz alike. The app's
/// MusicBrainz and ListenBrainz are made over it in tests (see their `service`).
pub struct World;

/// What the stand-in answers: the first route whose method and address part match.
static ROUTES: Mutex<Vec<(String, u16, String)>> = Mutex::new(Vec::new());
/// Every request it has been sent, as "METHOD address body", with the thread it came on.
static ASKED: Mutex<Vec<(std::thread::ThreadId, String)>> = Mutex::new(Vec::new());
/// What to change while a request is held, before it's answered: the first request that fits
/// each key sets it off, once.
static HOOKS: Mutex<Vec<(String, Box<dyn FnOnce() + Send>)>> = Mutex::new(Vec::new());
/// One scene at a time: they share the app's globals.
static SERIAL: Mutex<()> = Mutex::new(());

/// Whether the stand-in is to answer "stopped" from now on.
static STOPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether a scene has been entered in this run: then the app's MusicBrainz and ListenBrainz
/// are made over the stand-in. (The live tests, run by hand on their own, get the real ones.)
static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn entered() -> bool {
    ENTERED.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn world() -> Arc<dyn Http> {
    Arc::new(World)
}

impl Http for World {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let (method, body) = match request.method {
            nexus_core::http::Method::Get => ("GET", ""),
            nexus_core::http::Method::Post(body) => ("POST", body),
            nexus_core::http::Method::Put(body) => ("PUT", body),
        };
        let body: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
        ASKED.lock().unwrap().push((std::thread::current().id(), format!("{method} {} {body}", request.url)));
        if STOPPED.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Failure::Stopped);
        }
        // A key is a method and parts of the address, " && " between them, all to be there.
        let fits = |key: &str| {
            let (want, parts) = key.split_once(' ').unwrap();
            want == method && parts.split(" && ").all(|part| request.url.contains(part))
        };
        let hook = {
            let mut hooks = HOOKS.lock().unwrap();
            hooks.iter().position(|(key, _)| fits(key)).map(|at| hooks.remove(at).1)
        };
        if let Some(hook) = hook {
            hook();
        }
        let routes = ROUTES.lock().unwrap();
        let found = routes.iter().find(|(key, _, _)| fits(key));
        let (status, text) = found.map(|(_, status, text)| (*status, text.clone())).unwrap_or((404, "{}".into()));
        drop(routes);
        Ok(Response::new(status, Vec::new(), move || Ok(text)))
    }

    fn pause(&self, _: Duration) -> Result<(), Failure> {
        if STOPPED.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Failure::Stopped);
        }
        Ok(())
    }
}

/// A scene: what each service answers, by "METHOD part-of-address" (or several parts, joined
/// by " && "), the first that fits. An answer starting "@500 " is sent with that status.
pub struct Scene {
    routes: Vec<(String, u16, String)>,
    hooks: Vec<(String, Box<dyn FnOnce() + Send>)>,
    library: Library,
    asked: Vec<crate::asked::Asked>,
    singles: bool,
    lidarr: bool,
}

impl Scene {
    pub fn new() -> Scene {
        Scene { routes: Vec::new(), hooks: Vec::new(), library: Library::default(), asked: Vec::new(), singles: true, lidarr: true }
    }

    pub fn answer(mut self, key: &str, answer: impl Into<String>) -> Scene {
        let answer = answer.into();
        let (status, text) = match answer.strip_prefix('@').and_then(|rest| rest.split_once(' ')) {
            Some((status, text)) => (status.parse().unwrap(), text.to_string()),
            None => (200, answer),
        };
        self.routes.push((key.to_string(), status, text));
        self
    }

    /// Does `change` while the first request that fits `key` is held, before it's answered.
    pub fn meanwhile(mut self, key: &str, change: impl FnOnce() + Send + 'static) -> Scene {
        self.hooks.push((key.to_string(), Box::new(change)));
        self
    }

    pub fn singles(mut self, on: bool) -> Scene {
        self.singles = on;
        self
    }

    pub fn without_lidarr(mut self) -> Scene {
        self.lidarr = false;
        self
    }

    pub fn album(mut self, id: &str, title: &str, artist: &str) -> Scene {
        self.library.albums.push(CardData { kind: Kind::Album, id: id.into(), title: title.into(), subtitle: artist.into(), cover: format!("al-{id}") });
        self
    }

    pub fn song(mut self, id: &str, title: &str, artist: &str, artist_id: &str, plays: u32) -> Scene {
        self.library.songs.push(Song { id: id.into(), title: title.into(), artist: artist.into(), artist_id: artist_id.into(), play_count: plays, ..Default::default() });
        self
    }

    pub fn artist(mut self, id: &str, name: &str) -> Scene {
        self.library.artists.push(CardData { kind: Kind::Artist, id: id.into(), title: name.into(), subtitle: String::new(), cover: String::new() });
        self
    }

    pub fn asked(mut self, asked: crate::asked::Asked) -> Scene {
        self.asked.push(asked);
        self
    }

    /// Sets the app's globals to the scene, under the lock that keeps scenes apart.
    pub fn enter(self) -> std::sync::MutexGuard<'static, ()> {
        let guard = SERIAL.lock().unwrap_or_else(|stuck| stuck.into_inner());
        ENTERED.store(true, std::sync::atomic::Ordering::SeqCst);
        STOPPED.store(false, std::sync::atomic::Ordering::SeqCst);
        *ROUTES.lock().unwrap() = self.routes;
        *HOOKS.lock().unwrap() = self.hooks;
        ASKED.lock().unwrap().clear();
        crate::lidarr::forget_offers();
        crate::lidarr::set_singles(self.singles);
        crate::lidarr::set_current(self.lidarr.then(|| crate::lidarr::Lidarr::new("http://lidarr.test", "key", None, world())));
        crate::search::set_current(Some(Index::new(self.library)));
        let path = std::env::temp_dir().join(format!("nexus-pins-asked-{}.json", std::process::id()));
        let _ = std::fs::write(&path, serde_json::to_vec(&self.asked).unwrap());
        let _ = crate::asked::AskedList::load(&path);
        let _ = std::fs::remove_file(&path);
        guard
    }
}

/// From now on, every request and wait says "stopped".
#[allow(dead_code)]
pub fn stop() {
    STOPPED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The requests made so far, in the order made.
pub fn asked() -> Vec<String> {
    ASKED.lock().unwrap().iter().map(|(_, asked)| asked.clone()).collect()
}

/// The same, by the thread each was made on: each thread's in the order it made them.
pub fn asked_by_thread() -> Vec<Vec<String>> {
    let mut threads: Vec<(std::thread::ThreadId, Vec<String>)> = Vec::new();
    for (thread, asked) in ASKED.lock().unwrap().iter() {
        match threads.iter_mut().find(|(known, _)| known == thread) {
            Some((_, list)) => list.push(asked.clone()),
            None => threads.push((*thread, vec![asked.clone()])),
        }
    }
    let mut threads: Vec<Vec<String>> = threads.into_iter().map(|(_, list)| list).collect();
    threads.sort();
    threads
}

/// A library to put in place of the scene's while it's being looked at, as the window's
/// worker does when the server's library has changed.
pub fn replace_library(albums: &[(&str, &str, &str)], songs: &[(&str, &str, &str)]) -> impl FnOnce() + Send + 'static {
    let albums: Vec<CardData> = albums.iter().map(|(id, title, artist)| CardData { kind: Kind::Album, id: (*id).into(), title: (*title).into(), subtitle: (*artist).into(), cover: format!("al-{id}") }).collect();
    let songs: Vec<Song> = songs.iter().map(|(id, title, artist)| Song { id: (*id).into(), title: (*title).into(), artist: (*artist).into(), ..Default::default() }).collect();
    move || crate::search::set_current(Some(Index::new(Library { albums, songs, ..Default::default() })))
}

/// A dummy client: none of these pages asks the music server.
pub fn client() -> crate::subsonic::Client {
    crate::subsonic::Client::new("http://127.0.0.1:9", "nobody", "nothing")
}

/// Loads a page as the window does, then its extras, and gives every stage.
pub fn walk(route: Route, base: Option<PageData>) -> Vec<PageData> {
    let client = client();
    let mut stages = Vec::new();
    let page = match base {
        Some(page) => page,
        None => {
            let page = crate::pages::load(&client, &route).unwrap();
            stages.push(page.clone());
            page
        }
    };
    let mut published = Vec::new();
    if let Some(done) = crate::pages::enrich(&client, &route, &page, &mut |stage| published.push(stage)) {
        stages.extend(published);
        stages.push(done);
    }
    stages
}

/// Compares what a scene came to with what the old code made of it; with NEXUS_RECORD set,
/// writes that instead. `sorted` for requests made on several threads at once.
pub fn pinned(name: &str, stages: &[PageData], sorted: bool) {
    if sorted {
        pinned_threads(name, asked_by_thread());
    }
    pinned_with(name, stages, asked(), sorted)
}

/// Where requests were made on several threads, each thread's in its order, compared with
/// what the old code made (`<name>.threads.json`).
pub fn pinned_threads(name: &str, threads: Vec<Vec<String>>) {
    let found = json!(threads);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/requests").join(format!("{name}.threads.json"));
    if std::env::var_os("NEXUS_RECORD").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&found).unwrap() + "\n").unwrap();
        return;
    }
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("no fixture {name}.threads"))).unwrap();
    assert_eq!(kept, found, "{name}: the requests on each thread differ from what the old code made");
}

/// The same, with the requests given.
pub fn pinned_with(name: &str, stages: &[PageData], mut asked: Vec<String>, sorted: bool) {
    if sorted {
        asked.sort();
    }
    let found = json!({ "stages": stages, "asked": asked });
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/requests").join(format!("{name}.json"));
    if std::env::var_os("NEXUS_RECORD").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&found).unwrap() + "\n").unwrap();
        return;
    }
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("no fixture {name}"))).unwrap();
    if kept != found {
        let (a, b) = (serde_json::to_string_pretty(&kept).unwrap(), serde_json::to_string_pretty(&found).unwrap());
        let first = a.lines().zip(b.lines()).position(|(x, y)| x != y).unwrap_or(0);
        let show = |text: &str| text.lines().skip(first.saturating_sub(4)).take(14).collect::<Vec<_>>().join("\n");
        panic!("{name} differs from what the old code made, from line {first}:\n--- then\n{}\n--- now\n{}", show(&a), show(&b));
    }
}

// ---- what the services answer --------------------------------------------------------------------

pub fn lidarr_artist(id: &str, name: &str, pictured: bool, about: &str) -> Value {
    let images = if pictured { json!([{ "coverType": "poster", "remoteUrl": format!("https://pictures.test/{id}.jpg") }]) } else { json!([]) };
    json!({ "artistName": name, "foreignArtistId": id, "disambiguation": about, "images": images })
}

pub fn lidarr_album(id: &str, title: &str, kind: &str, secondary: &[&str], date: &str, artist: (&str, &str), votes: i64) -> Value {
    json!({ "title": title, "foreignAlbumId": id, "albumType": kind, "secondaryTypes": secondary, "releaseDate": date,
            "remoteCover": format!("https://covers.test/{id}.jpg"), "monitored": false, "ratings": { "votes": votes },
            "artist": { "artistName": artist.1, "foreignArtistId": artist.0 } })
}

/// A release group as MusicBrainz lists an artist's.
pub fn group(id: &str, title: &str, kind: &str, secondary: &[&str], date: &str, artist: (&str, &str)) -> Value {
    json!({ "id": id, "title": title, "primary-type": kind, "secondary-types": secondary, "first-release-date": date,
            "artist-credit": [{ "name": artist.1, "joinphrase": "", "artist": { "id": artist.0, "name": artist.1 } }] })
}

pub fn groups(groups: &[Value]) -> String {
    json!({ "release-group-count": groups.len(), "release-groups": groups }).to_string()
}

/// A recording as MusicBrainz's search finds one: on these releases (id, title, kind, date).
pub fn recording(id: &str, title: &str, artist: (&str, &str), seconds: u64, score: u64, releases: &[(&str, &str, &str, &str)]) -> Value {
    let releases: Vec<Value> = releases
        .iter()
        .map(|(group, title, kind, date)| {
            json!({ "status": "Official", "date": date, "release-group": { "id": group, "title": title, "primary-type": kind, "secondary-types": [] },
                    "artist-credit": [{ "name": artist.1, "artist": { "id": artist.0, "name": artist.1 } }] })
        })
        .collect();
    json!({ "id": id, "title": title, "length": seconds * 1000, "score": score,
            "artist-credit": [{ "name": artist.1, "joinphrase": "", "artist": { "id": artist.0, "name": artist.1 } }], "releases": releases })
}

pub fn recordings(found: &[Value]) -> String {
    json!({ "count": found.len(), "recordings": found }).to_string()
}

pub fn listens(key: &str, counts: &[(&str, u64)]) -> String {
    Value::Array(counts.iter().map(|(id, count)| json!({ key: id, "total_listen_count": count })).collect()).to_string()
}

pub fn top(songs: &[(&str, &str, u64)]) -> String {
    Value::Array(songs.iter().map(|(title, album, count)| json!({ "recording_name": title, "release_name": album, "length": 200000, "total_listen_count": count })).collect()).to_string()
}

pub fn list(values: &[Value]) -> String {
    Value::Array(values.to_vec()).to_string()
}

pub fn asked_for(album: &str, title: &str, artist: &str, at: i64, song: &str, arrived: i64, library_album: &str) -> crate::asked::Asked {
    crate::asked::Asked {
        album: album.into(),
        title: title.into(),
        artist: artist.into(),
        at,
        song: song.into(),
        arrived,
        library_album: library_album.into(),
        cover: if library_album.is_empty() { String::new() } else { format!("al-{library_album}") },
        ..Default::default()
    }
}

fn search_page(query: &str) -> PageData {
    PageData { title: format!("Results for “{query}”"), empty: format!("Nothing in your library matches “{query}”."), ..Default::default() }
}

fn searched(query: &str) -> Vec<PageData> {
    walk(Route::Search(query.into()), Some(search_page(query)))
}

// ---- the scenes ------------------------------------------------------------------------------------

#[test]
fn search_for_an_artist_beyond_doubt() {
    // An artist's name typed: one of the most played there are, so their own releases and
    // most played songs are what's offered; Lidarr's albums of theirs join MusicBrainz's.
    let qv = ("qv1", "Quillon Vane");
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Quillon%20Vane", list(&[
            lidarr_artist("qv1", "Quillon Vane", true, "the band"),
            lidarr_artist("qvx", "The Quillon Vane Experience", false, ""),
            lidarr_artist("qx", "Vane", false, ""),
            lidarr_artist("qs", "Quill", true, ""),
        ]))
        .answer("GET lidarr.test/api/v1/album/lookup?term=Quillon%20Vane", list(&[
            lidarr_album("rg-go", "Glass Orchard", "Album", &[], "2019-04-12T00:00:00Z", qv, 3),
            lidarr_album("rg-live", "At the Pier", "Album", &["Live"], "2020-01-01T00:00:00Z", qv, 1),
            lidarr_album("rg-old", "An Old One", "Album", &[], "2001-01-01T00:00:00Z", ("other", "Somebody"), 0),
            lidarr_album("rg-qvs", "Quillon Vane", "Single", &[], "2015-01-01T00:00:00Z", ("other", "Somebody"), 0),
        ]))
        .answer("POST api.listenbrainz.org/1/popularity/artist", listens("artist_mbid", &[("qv1", 6_000_000)]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=qv1&type=album%7Cep%7Csingle", groups(&[
            group("rg-go", "Glass Orchard", "Album", &[], "2019-04-12", qv),
            group("rg-pt", "Paper Tides", "Album", &[], "2022-09-30", qv),
            group("rg-lh", "Lantern Hymn", "Single", &[], "2021-02-01", qv),
            group("rg-nf", "Night Ferry", "EP", &[], "2020-06-05", qv),
            group("rg-comp", "Best Of", "Album", &["Compilation"], "2023-01-01", qv),
        ]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/qv1", top(&[
            ("Lantern Hymn", "Lantern Hymn", 900),
            ("Harbour", "Paper Tides", 800),
            ("Orchard Light", "Glass Orchard", 700),
            ("Tidewater", "Paper Tides", 600),
            ("Ferry Song", "Night Ferry", 500),
            ("Nowhere Song", "Not One of Theirs", 400),
        ]))
        .album("lib-pt", "Paper Tides", "Quillon Vane")
        .song("s1", "Harbour", "Quillon Vane", "lib-qv", 3)
        .enter();
    pinned("search-artist-beyond-doubt", &searched("Quillon Vane"), true);
}

#[test]
fn search_for_a_famous_single() {
    // A well-known song typed by its title: Lidarr's singles say whose it is, and that
    // artist's song goes first; then the other songs of that name; the albums they're on
    // join the albums, and their artists the artists, pictured afterwards.
    let qv = ("qv2", "Quillon Vane");
    let other = ("ob2", "Other Band");
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Lantern%20Hymn", "[]")
        .answer("GET lidarr.test/api/v1/album/lookup?term=Lantern%20Hymn", list(&[
            lidarr_album("rg-lh2", "Lantern Hymn", "Single", &[], "2021-02-01T00:00:00Z", qv, 12),
            lidarr_album("rg-lh3", "Lantern Hymn", "Single", &[], "2011-02-01T00:00:00Z", other, 2),
        ]))
        .answer("GET musicbrainz.org/ws/2/recording?query=arid%3Aqv2", recordings(&[
            recording("rec-1", "Lantern Hymn", qv, 201, 100, &[("rg-go2", "Glass Orchard", "Album", "2019-04-12"), ("rg-lh2", "Lantern Hymn", "Single", "2021-02-01")]),
        ]))
        .answer("GET musicbrainz.org/ws/2/recording?query=Lantern%20Hymn&dismax=true", recordings(&[
            recording("rec-1", "Lantern Hymn", qv, 201, 100, &[("rg-go2", "Glass Orchard", "Album", "2019-04-12")]),
            recording("rec-2", "Lantern Hymn", other, 180, 95, &[("rg-ob", "Lights", "Album", "2012-05-05")]),
            recording("rec-3", "Lantern Hymn (live)", other, 190, 90, &[("rg-ob-live", "Live", "Album", "2013-05-05")]),
            recording("rec-4", "Lanterns", ("x3", "Someone"), 100, 20, &[("rg-x", "Far Off", "Album", "2000-01-01")]),
        ]))
        .answer("POST api.listenbrainz.org/1/popularity/recording", listens("recording_mbid", &[("rec-1", 5000), ("rec-2", 300)]))
        .answer("GET lidarr.test/api/v1/artist/lookup?term=lidarr%3Aqv2", list(&[lidarr_artist("qv2", "Quillon Vane", true, "the band")]))
        .answer("GET lidarr.test/api/v1/artist/lookup?term=lidarr%3Aob2", list(&[lidarr_artist("ob2", "Other Band", false, "from somewhere")]))
        .enter();
    pinned("search-famous-single", &searched("Lantern Hymn"), true);
}

#[test]
fn search_for_a_name_that_is_a_song_more_than_an_artist() {
    // A name that's as likely a song's: the artist of that name is played less than the
    // song is, so the songs are what's meant, and the artist stays among the rest.
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Home", list(&[lidarr_artist("hm", "Home", true, "a band"), lidarr_artist("hm2", "Home", false, "another")]))
        .answer("GET lidarr.test/api/v1/album/lookup?term=Home", list(&[lidarr_album("rg-home", "Home", "Album", &[], "2010-01-01T00:00:00Z", ("hm", "Home"), 0)]))
        .answer("POST api.listenbrainz.org/1/popularity/artist", listens("artist_mbid", &[("hm", 20_000), ("hm2", 50)]))
        .answer("GET musicbrainz.org/ws/2/recording?query=Home&dismax=true", recordings(&[
            recording("rec-h1", "Home", ("ps", "Phillip Someone"), 210, 100, &[("rg-ps", "Homeward", "Album", "2012-03-03")]),
            recording("rec-h2", "Home", ("mb", "Michael Else"), 220, 100, &[("rg-mb", "Songs", "Album", "2009-03-03")]),
            recording("rec-h3", "Home", ("hm", "Home"), 200, 100, &[("rg-home", "Home", "Album", "2010-01-01")]),
        ]))
        .answer("POST api.listenbrainz.org/1/popularity/recording", listens("recording_mbid", &[("rec-h1", 900_000), ("rec-h2", 40_000), ("rec-h3", 10)]))
        .song("s9", "Home", "Michael Else", "lib-mb", 1)
        .enter();
    pinned("search-name-that-is-a-song", &searched("Home"), true);
}

#[test]
fn search_for_an_artist_meant_after_all() {
    // The same, but the artist is played more than any song of the name: their releases.
    let hm = ("hm4", "Homeland");
    let _scene = Scene::new()
        .singles(false)
        .answer("GET lidarr.test/api/v1/artist/lookup?term=homeland", list(&[lidarr_artist("hm4", "Homeland", true, "")]))
        .answer("GET lidarr.test/api/v1/album/lookup?term=homeland", "[]")
        .answer("POST api.listenbrainz.org/1/popularity/artist", listens("artist_mbid", &[("hm4", 90_000)]))
        .answer("GET musicbrainz.org/ws/2/recording?query=homeland&dismax=true", recordings(&[
            recording("rec-hl", "Homeland", ("zz", "Zed"), 210, 100, &[("rg-zz", "Zed's", "Album", "2012-03-03")]),
        ]))
        .answer("POST api.listenbrainz.org/1/popularity/recording", listens("recording_mbid", &[("rec-hl", 10_000)]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=hm4&type=album&", groups(&[group("rg-hl1", "First", "Album", &[], "2015-01-01", hm)]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/hm4", "[]")
        .answer("POST api.listenbrainz.org/1/popularity/release-group", "[]")
        .enter();
    pinned("search-artist-meant-after-all", &searched("homeland"), true);
}

#[test]
fn search_that_finds_nothing() {
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup", "[]")
        .answer("GET lidarr.test/api/v1/album/lookup", "[]")
        .answer("GET musicbrainz.org/ws/2/recording", recordings(&[]))
        .enter();
    pinned("search-nothing", &searched("zzqx vvb"), true);
}

#[test]
fn search_when_the_services_fail() {
    // Lidarr's artists can't be had, MusicBrainz answers with an error: what can be had is.
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup", "@500 {}")
        .answer("GET lidarr.test/api/v1/album/lookup", list(&[lidarr_album("rg-sh", "Saltwater Hours", "Album", &[], "2016-03-11T00:00:00Z", ("ml", "Marrow Lake"), 0)]))
        .answer("GET musicbrainz.org/ws/2/recording", "@500 {}")
        .enter();
    pinned("search-services-fail", &searched("saltwater"), true);
}

#[test]
fn search_with_what_has_been_asked_for() {
    // Songs whose albums are asked for already show as asked for: noted as asked, offered
    // and marked so, or an album among the results that's asked for; the library's own
    // albums and songs are left out.
    let ml = ("ml5", "Marrow Lake");
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup", "[]")
        .answer("GET lidarr.test/api/v1/album/lookup?term=saltwater", list(&[
            json!({ "id": 4, "monitored": true, "title": "Saltwater Hours", "foreignAlbumId": "rg-sh5", "albumType": "Album", "secondaryTypes": [], "releaseDate": "2016-03-11T00:00:00Z", "artist": { "artistName": "Marrow Lake", "foreignArtistId": "ml5" } }),
            lidarr_album("rg-in", "Saltwater Live", "Album", &[], "2018-01-01T00:00:00Z", ml, 0),
            lidarr_album("rg-lib", "Saltwater Days", "Album", &[], "2014-01-01T00:00:00Z", ml, 0),
        ]))
        .answer("GET musicbrainz.org/ws/2/recording?query=saltwater", recordings(&[
            recording("rec-s1", "Saltwater", ml, 200, 100, &[("rg-sh5", "Saltwater Hours", "Album", "2016-03-11")]),
            recording("rec-s2", "Saltwater Song", ml, 200, 100, &[("rg-asked", "Asked One", "Album", "2017-03-11")]),
            recording("rec-s3", "Saltwater Blues", ml, 200, 100, &[("rg-marked", "Marked One", "Album", "2019-03-11")]),
            recording("rec-s4", "Saltwater Heart", ml, 200, 100, &[("rg-free", "Free One", "Album", "2020-03-11")]),
            recording("rec-s5", "Saltwater Owned", ml, 200, 100, &[("rg-own", "Owned One", "Album", "2021-03-11")]),
        ]))
        .answer("POST api.listenbrainz.org/1/popularity/recording", "[]")
        .album("lib-sd", "Saltwater Days", "Marrow Lake")
        .song("s5", "Saltwater Owned", "Marrow Lake", "lib-ml", 1)
        .asked(asked_for("rg-asked", "Asked One", "Marrow Lake", 100, "", 0, ""))
        .asked(asked_for("rg-free", "Free One", "Marrow Lake", 100, "", 50, "lib-free"))
        .enter();
    crate::lidarr::remember(&[nexus_core::offer::Offer { album: "rg-marked".into(), title: "Marked One".into(), artist: "Marrow Lake".into(), artist_id: "ml5".into(), ..Default::default() }]);
    crate::lidarr::mark("rg-marked", true);
    pinned("search-asked-for", &searched("saltwater"), true);
}

#[test]
fn search_with_requests_off() {
    let _scene = Scene::new().without_lidarr().enter();
    pinned("search-requests-off", &searched("anything"), false);
}

#[test]
fn an_artist_lidarr_does_not_keep() {
    // Their page from MusicBrainz, singles and EPs included: one album in the library, one
    // asked for, the rest open; then their most played songs, worked out from their releases
    // since ListenBrainz has no list for them.
    let ml = ("ml6", "Marrow Lake");
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist", "[]")
        .answer("GET musicbrainz.org/ws/2/release-group?artist=ml6&type=album%7Cep%7Csingle", groups(&[
            group("rg-ik", "Iron Kite", "Album", &[], "2018-10-19", ml),
            group("rg-sh6", "Saltwater Hours", "Album", &[], "2016-03-11", ml),
            group("rg-new", "Newest", "Album", &[], "2024-01-01", ml),
            group("rg-ost", "A Film", "Album", &["Soundtrack"], "2023-01-01", ml),
            group("rg-ep", "Small Hours", "EP", &[], "2017-01-01", ml),
            group("rg-s", "Kite", "Single", &[], "", ml),
        ]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/ml6", "[]")
        .answer("POST api.listenbrainz.org/1/popularity/release-group", listens("release_group_mbid", &[("rg-ik", 900), ("rg-sh6", 500), ("rg-s", 50)]))
        .answer("GET musicbrainz.org/ws/2/recording?query=rgid", recordings(&[
            recording("rec-k1", "Kite", ml, 200, 100, &[("rg-ik", "Iron Kite", "Album", "2018-10-19"), ("rg-s", "Kite", "Single", "2018-01-01")]),
            recording("rec-k2", "Iron", ml, 200, 100, &[("rg-ik", "Iron Kite", "Album", "2018-10-19")]),
            recording("rec-k3", "Salt", ml, 200, 100, &[("rg-sh6", "Saltwater Hours", "Album", "2016-03-11")]),
            recording("rec-k4", "Sea Wall", ml, 200, 100, &[("rg-sh6", "Saltwater Hours", "Album", "2016-03-11")]),
            recording("rec-k5", "Low Tide", ml, 200, 100, &[("rg-sh6", "Saltwater Hours", "Album", "2016-03-11")]),
        ]))
        .answer("POST api.listenbrainz.org/1/popularity/recording", listens("recording_mbid", &[("rec-k1", 700), ("rec-k2", 300), ("rec-k3", 100), ("rec-k4", 90), ("rec-k5", 80)]))
        .album("lib-ik", "Iron Kite", "Marrow Lake")
        .song("s7", "Salt", "Marrow Lake", "lib-ml", 2)
        .asked(asked_for("rg-new", "Newest", "Marrow Lake", 100, "", 0, ""))
        .enter();
    crate::lidarr::remember_artists(&[crate::lidarr::ArtistOffer { id: "ml6".into(), name: "Marrow Lake".into(), about: "a band".into(), cover: "https://pictures.test/ml6.jpg".into() }]);
    crate::lidarr::remember(&[nexus_core::offer::Offer { album: "rg-new".into(), title: "Newest".into(), artist: "Marrow Lake".into(), artist_id: "ml6".into(), requested: true, ..Default::default() }]);
    pinned("artist-not-kept", &walk(Route::OutsideArtist("ml6".into()), None), false);
}

#[test]
fn an_artist_lidarr_keeps() {
    // Their page from Lidarr's list (singles switched off): arrived, asked for, open; then
    // their most played, some left out for being in the library.
    let _scene = Scene::new()
        .singles(false)
        .answer("GET lidarr.test/api/v1/artist", r#"[{"id":7,"artistName":"Saltmarsh","foreignArtistId":"sm7"}]"#)
        .answer("GET lidarr.test/api/v1/album?artistId=7", list(&[
            json!({ "id": 71, "title": "Arrived One", "foreignAlbumId": "rg-a1", "albumType": "Album", "secondaryTypes": [], "releaseDate": "2010-01-01T00:00:00Z", "monitored": true, "statistics": { "trackFileCount": 10 }, "images": [{ "coverType": "cover", "remoteUrl": "https://covers.test/a1.jpg" }], "artist": { "artistName": "Saltmarsh", "foreignArtistId": "sm7" } }),
            json!({ "id": 72, "title": "Wanted One", "foreignAlbumId": "rg-a2", "albumType": "Album", "secondaryTypes": [], "releaseDate": "2012-01-01T00:00:00Z", "monitored": true, "statistics": { "trackFileCount": 0 }, "artist": { "artistName": "Saltmarsh", "foreignArtistId": "sm7" } }),
            json!({ "id": 73, "title": "Open One", "foreignAlbumId": "rg-a3", "albumType": "Album", "secondaryTypes": [], "releaseDate": "0001-01-01T00:00:00Z", "monitored": false, "statistics": { "trackFileCount": 0 }, "artist": { "artistName": "Saltmarsh", "foreignArtistId": "sm7" } }),
            json!({ "id": 74, "title": "An EP", "foreignAlbumId": "rg-a4", "albumType": "EP", "secondaryTypes": [], "releaseDate": "2013-01-01T00:00:00Z", "monitored": false, "artist": { "artistName": "Saltmarsh", "foreignArtistId": "sm7" } }),
        ]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=sm7&type=album&", groups(&[
            group("rg-a1", "Arrived One", "Album", &[], "2010-01-01", ("sm7", "Saltmarsh")),
            group("rg-a3", "Open One", "Album", &[], "2014-01-01", ("sm7", "Saltmarsh")),
        ]))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/sm7", top(&[
            ("Owned Song", "Open One", 900),
            ("First Song", "Open One", 800),
            ("Second Song", "Arrived One", 700),
        ]))
        .song("s8", "Owned Song", "Saltmarsh", "lib-sm", 2)
        .enter();
    pinned("artist-kept", &walk(Route::OutsideArtist("sm7".into()), None), false);
}

#[test]
fn an_artist_that_cannot_be_looked_up() {
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist", "[]")
        .answer("GET musicbrainz.org/ws/2/release-group?artist=gone", "@500 {}")
        .enter();
    crate::lidarr::remember_artists(&[crate::lidarr::ArtistOffer { id: "gone".into(), name: "Gone Band".into(), ..Default::default() }]);
    let mut stages = walk(Route::OutsideArtist("gone".into()), None);
    // With requests switched off.
    let _ = crate::lidarr::set_current(None);
    stages.extend(walk(Route::OutsideArtist("gone".into()), None));
    pinned("artist-cannot-be-looked-up", &stages, false);
}

#[test]
fn an_album_beyond_the_library() {
    // One offered before, its songs on two discs; one Lidarr is asked about afresh and
    // doesn't list; one asked for whose songs can't be had; one Lidarr can't be asked about.
    let _scene = Scene::new()
        .answer("GET musicbrainz.org/ws/2/release?release-group=rg-b1", json!({ "releases": [{ "media": [
            { "position": 1, "tracks": [{ "title": "One", "length": 200000, "position": 1 }, { "title": "Two", "length": 100000, "position": 2 }] },
            { "position": 2, "tracks": [{ "title": "Three", "length": 300000, "position": 1 }] }
        ] }] }).to_string())
        .answer("GET lidarr.test/api/v1/album/lookup?term=lidarr%3Arg-b2", "[]")
        .answer("GET musicbrainz.org/ws/2/release?release-group=rg-b3", "@500 {}")
        .answer("GET lidarr.test/api/v1/album/lookup?term=lidarr%3Arg-b4", "@500 {}")
        .answer("GET lidarr.test/api/v1/album/lookup?term=lidarr%3Arg-b5", list(&[lidarr_album("rg-b5", "Found Afresh", "EP", &[], "", ("ab5", "Afresh"), 0)]))
        .answer("GET musicbrainz.org/ws/2/release?release-group=rg-b5", json!({ "releases": [] }).to_string())
        .enter();
    crate::lidarr::remember(&[
        nexus_core::offer::Offer { album: "rg-b1".into(), title: "Two Discs".into(), artist: "Quillon Vane".into(), artist_id: "qv".into(), year: "2019".into(), cover: "https://covers.test/b1.jpg".into(), ..Default::default() },
        nexus_core::offer::Offer { album: "rg-b3".into(), title: "Asked".into(), artist: "Nobody".into(), kind: "Single".into(), requested: true, ..Default::default() },
    ]);
    let mut stages = Vec::new();
    for id in ["rg-b1", "rg-b2", "rg-b3", "rg-b4", "rg-b5"] {
        stages.extend(walk(Route::OutsideAlbum(id.into()), None));
    }
    let _ = crate::lidarr::set_current(None);
    stages.extend(walk(Route::OutsideAlbum("rg-b1".into()), None));
    pinned("album", &stages, false);
}

#[test]
fn the_requests_page() {
    // Nothing; arrived and on their way; arrived only; Lidarr failing; switched off.
    let pending = |records: Value| json!({ "page": 1, "pageSize": 500, "totalRecords": records.as_array().unwrap().len(), "records": records }).to_string();
    let (mut stages, mut log) = (Vec::new(), Vec::new());
    {
        let _scene = Scene::new()
            .answer("GET lidarr.test/api/v1/wanted/missing", pending(json!([])))
            .answer("GET lidarr.test/api/v1/queue", pending(json!([])))
            .enter();
        stages.extend(walk(Route::Requests, None));
        log.extend(asked());
    }
    {
        let _scene = Scene::new()
            .answer("GET lidarr.test/api/v1/wanted/missing", pending(json!([
                { "id": 1, "title": "Slow One", "artist": { "artistName": "Zed" } },
                { "id": 2, "title": "Coming One", "artist": { "artistName": "Abe" } },
                { "id": 3, "title": "Another", "artist": { "artistName": "abe" } },
            ])))
            .answer("GET lidarr.test/api/v1/queue", pending(json!([{ "albumId": 2, "size": 100.0, "sizeleft": 60.0 }, { "albumId": 2, "size": 100.0, "sizeleft": 20.0 }])))
            .asked(asked_for("rg-r1", "Old Arrival", "Zed", 10, "", 100, "lib-r1"))
            .asked(asked_for("rg-r2", "New Arrival", "Abe", 20, "A Song", 200, "lib-r2"))
            .asked(asked_for("rg-r3", "Still Coming", "Abe", 30, "", 0, ""))
            .asked(asked_for("rg-r4", "Arrived Without Its Id", "Abe", 30, "", 300, ""))
            .enter();
        stages.extend(walk(Route::Requests, None));
        log.extend(asked());
    }
    {
        let _scene = Scene::new()
            .answer("GET lidarr.test/api/v1/wanted/missing", pending(json!([])))
            .answer("GET lidarr.test/api/v1/queue", pending(json!([])))
            .asked(asked_for("rg-r1", "Old Arrival", "Zed", 10, "", 100, "lib-r1"))
            .enter();
        stages.extend(walk(Route::Requests, None));
        log.extend(asked());
    }
    {
        let _scene = Scene::new()
            .answer("GET lidarr.test/api/v1/wanted/missing", "@500 {}")
            .asked(asked_for("rg-r1", "Old Arrival", "Zed", 10, "", 100, "lib-r1"))
            .enter();
        stages.extend(walk(Route::Requests, None));
        log.extend(asked());
    }
    let _scene = Scene::new().without_lidarr().enter();
    stages.extend(walk(Route::Requests, None));
    pinned_with("requests", &stages, log, false);
}

/// What the stand-in music server was asked, in "Artists you might like".
static SERVER: Mutex<Vec<String>> = Mutex::new(Vec::new());

#[test]
fn artists_you_might_like() {
    // The music server says who's like the artists played most; Lidarr says who each of
    // those is. Anyone it keeps track of already, or the library has, is left out.
    use std::io::Write;
    SERVER.lock().unwrap().clear();
    let server = crate::subsonic::fake::server(|path, _, stream| {
        // Without what changes from run to run: the salt and the token made from it.
        let shown: Vec<&str> = path.split('&').filter(|part| !part.starts_with("t=") && !part.starts_with("s=")).collect();
        SERVER.lock().unwrap().push(shown.join("&"));
        let similar = if path.contains("id=lib-a") {
            r#"[{"name":"Shore Lights","id":"-1"},{"name":"Kept Band","id":""},{"name":"Owned Band","id":"lib-o"},{"name":"Nobody Knows","id":"-1"}]"#
        } else {
            r#"[{"name":"Shore Lights","id":"-1"},{"name":"Far Hills","id":"-1"},{"name":"In Library","id":""}]"#
        };
        crate::subsonic::fake::ok(stream, &format!(r#""artistInfo2":{{"similarArtist":{similar}}}"#));
        let _ = stream.flush();
    });
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Shore%20Lights", list(&[lidarr_artist("sl", "Shore Lights", false, ""), lidarr_artist("sl2", "Shore Lights", true, "the other")]))
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Far%20Hills", list(&[lidarr_artist("fh", "Far Hills", true, "")]))
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Kept%20Band", list(&[lidarr_artist("kb", "Kept Band", true, "")]))
        .answer("GET lidarr.test/api/v1/artist/lookup?term=In%20Library", list(&[lidarr_artist("il", "In Library", true, "")]))
        .answer("GET lidarr.test/api/v1/artist/lookup", "[]")
        .answer("GET lidarr.test/api/v1/artist", list(&[lidarr_artist("kb", "Kept Band", true, "")]))
        .song("h1", "One", "Played A Lot", "lib-a", 40)
        .song("h2", "Two", "Played Less", "lib-b", 3)
        .artist("lib-il", "In Library")
        .enter();
    let client = crate::subsonic::Client::new(&server, "tester", "secret");
    let home = PageData { title: "Home".into(), shelves: vec![crate::pages::ShelfData { title: "Recently played".into(), ..Default::default() }], ..Default::default() };
    let mut stages = Vec::new();
    stages.extend(crate::pages::enrich(&client, &Route::Home, &home, &mut |_| {}));
    pinned_threads("artists-you-might-like", asked_by_thread());
    let mut log = asked();
    log.sort();
    pinned_with("artists-you-might-like", &stages, log, false);
    // And what the music server was asked (four at a time, so in no set order).
    let mut server = SERVER.lock().unwrap().clone();
    server.sort();
    pinned_threads("artists-you-might-like.server", vec![server]);
}

// ---- what no scene above reaches -------------------------------------------------------------------

fn named(id: &str, name: &str) -> Value {
    lidarr_artist(id, name, false, "")
}

#[test]
fn search_at_its_limits() {
    // Eleven artists named in what was typed (ten are kept); twenty-six of Lidarr's albums
    // (twenty-four are); twenty-two songs by four artists (twenty are kept, and the first three
    // songs' artists join the artists, which then stop at twelve); the albums the first three
    // songs are on go first, and the albums stop at twelve.
    let artists: Vec<Value> = (1..=11).map(|n| named(&format!("lim-a{n}"), &format!("Lim Artist {n}"))).collect();
    let albums: Vec<Value> = (1..=26).map(|n| lidarr_album(&format!("rg-lim{n}"), &format!("Lim Album {n}"), "Album", &[], "2010-01-01T00:00:00Z", (&format!("lim-x{n}"), &format!("Lim Maker {n}")), 0)).collect();
    let by = |n: usize| (["sa0", "sa1", "sa2", "sa3"][n % 4], ["Song Artist Zero", "Song Artist One", "Song Artist Two", "Song Artist Three"][n % 4]);
    let songs: Vec<Value> = (0..22)
        .map(|n| {
            let (id, name) = by(n);
            recording(&format!("rec-lim{n}"), &format!("Lim Song {n}"), (id, name), 200, 100, &[(&format!("rg-ls{n}"), &format!("Lim Song Album {n}"), "Album", "2001-01-01")])
        })
        .collect();
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=lim&", list(&artists))
        .answer("GET lidarr.test/api/v1/artist/lookup?term=lim", list(&artists))
        .answer("GET lidarr.test/api/v1/artist/lookup?term=lidarr", "[]")
        .answer("GET lidarr.test/api/v1/album/lookup?term=lim", list(&albums))
        .answer("GET musicbrainz.org/ws/2/recording?query=lim&dismax=true", recordings(&songs))
        .answer("POST api.listenbrainz.org/1/popularity/recording", "[]")
        .enter();
    pinned("search-at-its-limits", &searched("lim"), true);
}

#[test]
fn search_for_an_artist_on_plays_alone() {
    // Three artists called exactly what was typed: one pictured and played a little, two not
    // pictured and played as much as each other, more. Plays alone make an artist one of
    // note; of two equal, the first is meant, and goes ahead of the pictured one. Twenty-six
    // of their albums are listed by MusicBrainz: twenty-four are offered.
    let vale = ("va-a", "Vale");
    let groups_of: Vec<Value> = (1..=26).map(|n| group(&format!("rg-va{n}"), &format!("Vale Album {n}"), "Album", &[], &format!("19{:02}-01-01", 70 + n), vale)).collect();
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Vale", list(&[
            lidarr_artist("va-n", "Vale", true, "pictured"),
            named("va-a", "Vale"),
            named("va-b", "Vale"),
            named("va-c", "Vale Choir"),
        ]))
        .answer("GET lidarr.test/api/v1/album/lookup?term=Vale", list(&[lidarr_album("rg-vl", "From Lidarr", "Album", &[], "2020-01-01T00:00:00Z", ("va-n", "Vale"), 0)]))
        .answer("POST api.listenbrainz.org/1/popularity/artist", listens("artist_mbid", &[("va-n", 15_000), ("va-a", 20_000), ("va-b", 20_000)]))
        .answer("GET musicbrainz.org/ws/2/recording?query=Vale&dismax=true", recordings(&[recording("rec-v", "Vale", ("zz9", "Zed"), 200, 100, &[("rg-zz9", "Zed's", "Album", "2012-01-01")])]))
        .answer("POST api.listenbrainz.org/1/popularity/recording", listens("recording_mbid", &[("rec-v", 100)]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=va-a&type=album%7Cep%7Csingle", groups(&groups_of))
        .answer("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/va-a", "[]")
        .answer("POST api.listenbrainz.org/1/popularity/release-group", "[]")
        .enter();
    pinned("search-artist-on-plays-alone", &searched("Vale"), true);
}

#[test]
fn search_for_an_artist_whose_releases_cannot_be_had() {
    // The artist is meant beyond doubt, but Lidarr's search for releases fails, and so does
    // MusicBrainz's list of theirs.
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Fail%20Band", list(&[lidarr_artist("fb", "Fail Band", true, "")]))
        .answer("GET lidarr.test/api/v1/album/lookup?term=Fail%20Band", "@500 {}")
        .answer("POST api.listenbrainz.org/1/popularity/artist", listens("artist_mbid", &[("fb", 6_000_000)]))
        .answer("GET musicbrainz.org/ws/2/release-group?artist=fb", "@500 {}")
        .enter();
    pinned("search-artist-releases-fail", &searched("Fail Band"), true);
}

#[test]
fn search_for_a_famous_single_found_only_the_wider_way() {
    // Of the artist's song among studio albums only, MusicBrainz knows a guest's version
    // first; asked again with only live recordings left out, theirs is there and played most.
    let os = ("os1", "Old Singer");
    let guest = ("gx1", "Guest Band");
    let _scene = Scene::new()
        .answer("GET lidarr.test/api/v1/artist/lookup?term=Old%20Song", "[]")
        .answer("GET lidarr.test/api/v1/album/lookup?term=Old%20Song", list(&[lidarr_album("rg-os", "Old Song", "Single", &[], "1990-01-01T00:00:00Z", os, 9)]))
        .answer("GET musicbrainz.org/ws/2/recording?query=arid%3Aos1 && limit=50", recordings(&[
            recording("rec-g", "Old Song", guest, 200, 100, &[("rg-g", "Guest Album", "Album", "2015-01-01")]),
        ]))
        .answer("GET musicbrainz.org/ws/2/recording?query=arid%3Aos1 && limit=100", recordings(&[
            recording("rec-g", "Old Song", guest, 200, 100, &[("rg-g", "Guest Album", "Album", "2015-01-01")]),
            recording("rec-o", "Old Song", os, 210, 100, &[("rg-oa", "Old Album", "Album", "1990-05-01"), ("rg-hits", "Hits", "Album", "2000-05-01")]),
        ]))
        .answer("POST api.listenbrainz.org/1/popularity/recording", listens("recording_mbid", &[("rec-o", 9000), ("rec-g", 50)]))
        .answer("GET musicbrainz.org/ws/2/recording?query=Old%20Song&dismax=true", recordings(&[
            recording("rec-o", "Old Song", os, 210, 100, &[("rg-oa", "Old Album", "Album", "1990-05-01")]),
            recording("rec-c", "Old Song", ("cv", "Cover Band"), 200, 90, &[("rg-cv", "Covers", "Album", "2010-05-01")]),
        ]))
        .answer("GET lidarr.test/api/v1/artist/lookup?term=lidarr", "[]")
        .enter();
    pinned("search-famous-single-wider", &searched("Old Song"), true);
}

/// Ten songs an artist is most played for, all on their one album.
fn ten_most_played(id: &str, name: &str) -> (String, String) {
    let songs: Vec<(String, &str, u64)> = (1..=10).map(|n| (format!("Song {n}"), "The Album", 1000 - n as u64)).collect();
    let top_of: Vec<(&str, &str, u64)> = songs.iter().map(|(title, album, count)| (title.as_str(), *album, *count)).collect();
    (groups(&[group(&format!("rg-{id}"), "The Album", "Album", &[], "2015-01-01", (id, name))]), top(&top_of))
}

#[test]
fn popular_at_its_edges() {
    // Eight of ten listed; one the library has at the eighth place leaves a ninth in its
    // place and says some were left out; one it has only at the ninth says nothing of it.
    let mut stages = Vec::new();
    let mut log = Vec::new();
    for (id, owned) in [("p8", "Song 8"), ("p9", "Song 9")] {
        let (releases, most) = ten_most_played(id, "Eight Band");
        let _scene = Scene::new()
            .answer("GET lidarr.test/api/v1/artist", "[]")
            .answer(&format!("GET musicbrainz.org/ws/2/release-group?artist={id}&"), releases)
            .answer(&format!("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/{id}"), most)
            .song("s-own", owned, "Eight Band", "lib-eb", 1)
            .enter();
        stages.extend(walk(Route::OutsideArtist(id.into()), None));
        log.extend(asked());
    }
    pinned_with("popular-at-its-edges", &stages, log, false);
}

#[test]
fn an_artist_lidarr_keeps_with_singles_from_musicbrainz() {
    // Singles and EPs on: Lidarr's list, and MusicBrainz's singles and EPs it hasn't got (one
    // it has, by the same name for it, isn't listed twice; an album it hasn't isn't added).
    // Then the same artist when MusicBrainz can't be had: Lidarr's list alone.
    let mut stages = Vec::new();
    let mut log = Vec::new();
    for (id, fails) in [("tm", false), ("tf", true)] {
        let lidarr_list = list(&[
            json!({ "id": 91, "title": "Their Album", "foreignAlbumId": format!("rg-{id}-a"), "albumType": "Album", "secondaryTypes": [], "releaseDate": "2015-01-01T00:00:00Z", "monitored": false, "statistics": { "trackFileCount": 0 }, "artist": { "artistName": "Tracked Band", "foreignArtistId": id } }),
            json!({ "id": 92, "title": "Their EP", "foreignAlbumId": format!("rg-{id}-e"), "albumType": "EP", "secondaryTypes": [], "releaseDate": "2016-01-01T00:00:00Z", "monitored": true, "statistics": { "trackFileCount": 0 }, "artist": { "artistName": "Tracked Band", "foreignArtistId": id } }),
        ]);
        let brainz = if fails {
            "@500 {}".to_string()
        } else {
            groups(&[
                group(&format!("rg-{id}-a"), "Their Album", "Album", &[], "2015-01-01", (id, "Tracked Band")),
                group(&format!("rg-{id}-e"), "Their EP", "EP", &[], "2016-01-01", (id, "Tracked Band")),
                group(&format!("rg-{id}-s"), "Their Single", "Single", &[], "2017-01-01", (id, "Tracked Band")),
                group(&format!("rg-{id}-a2"), "Another Album", "Album", &[], "2018-01-01", (id, "Tracked Band")),
            ])
        };
        let _scene = Scene::new()
            .answer("GET lidarr.test/api/v1/artist", &format!(r#"[{{"id":9,"artistName":"Tracked Band","foreignArtistId":"{id}"}}]"#))
            .answer("GET lidarr.test/api/v1/album?artistId=9", lidarr_list)
            .answer(&format!("GET musicbrainz.org/ws/2/release-group?artist={id}&type=album%7Cep%7Csingle"), brainz)
            .answer(&format!("GET api.listenbrainz.org/1/popularity/top-recordings-for-artist/{id}"), "[]")
            .answer("POST api.listenbrainz.org/1/popularity/release-group", "[]")
            .enter();
        stages.extend(walk(Route::OutsideArtist(id.into()), None));
        log.extend(asked());
    }
    pinned_with("artist-kept-with-singles", &stages, log, false);
}

mod meanwhile;
