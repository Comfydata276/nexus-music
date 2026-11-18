//! Turns server responses into the content of each page. Runs on worker threads.
//!
//! A page loads in two steps. `load` fetches only what the page can't be drawn without, in as
//! few round trips as possible. `enrich` then adds the extras (other albums by the artist,
//! biography, popular songs), which can be slow because the server may ask Last.fm for them.

use serde::{Deserialize, Serialize};

use crate::controller::{Route, count, length};
use crate::subsonic::{Album, AlbumList, ApiResult, Artist, Client, Playlist, Song};
use crate::ui;

/// Albums fetched per request in the library grid.
pub const GRID_PAGE: u32 = 60;

/// Sort orders offered on the Albums page, in display order.
pub const ALBUM_SORTS: [(&str, AlbumList); 7] = [
    ("Recently added", AlbumList::Newest),
    ("A to Z", AlbumList::AlphabeticalByName),
    ("By artist", AlbumList::AlphabeticalByArtist),
    ("Recently played", AlbumList::Recent),
    ("Most played", AlbumList::Frequent),
    ("Newest releases", AlbumList::ByYearDescending),
    ("Liked", AlbumList::Starred),
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum Kind {
    #[default]
    Album,
    Artist,
    Playlist,
    Genre,
    /// A list of songs the app puts together from what's been played.
    Mix,
    /// An album that isn't in the library, which could be asked for.
    Request,
    /// An artist as the wider world knows them, whether or not the library has their music.
    OutsideArtist,
}

impl Kind {
    pub fn from_ui(kind: ui::CardKind) -> Kind {
        match kind {
            ui::CardKind::Album => Kind::Album,
            ui::CardKind::Artist => Kind::Artist,
            ui::CardKind::Playlist => Kind::Playlist,
            ui::CardKind::Genre => Kind::Genre,
            ui::CardKind::Mix => Kind::Mix,
            ui::CardKind::Request => Kind::Request,
            ui::CardKind::OutsideArtist => Kind::OutsideArtist,
        }
    }

    fn to_ui(self) -> ui::CardKind {
        match self {
            Kind::Album => ui::CardKind::Album,
            Kind::Artist => ui::CardKind::Artist,
            Kind::Playlist => ui::CardKind::Playlist,
            Kind::Genre => ui::CardKind::Genre,
            Kind::Mix => ui::CardKind::Mix,
            Kind::Request => ui::CardKind::Request,
            Kind::OutsideArtist => ui::CardKind::OutsideArtist,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CardData {
    pub kind: Kind,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub cover: String,
}

impl CardData {
    pub fn album(album: &Album) -> CardData {
        CardData {
            kind: Kind::Album,
            id: album.id.clone(),
            title: album.name.clone(),
            subtitle: album.artist_label().to_string(),
            cover: album.cover_art.clone(),
        }
    }

    /// An album on its artist's page, where the year says more than repeating the name.
    fn album_by_year(album: &Album) -> CardData {
        let kind = release_kind(album);
        let subtitle =
            if album.year > 0 { format!("{}, {kind}", album.year) } else { kind.to_string() };
        CardData { subtitle, ..CardData::album(album) }
    }

    pub fn artist(artist: &Artist) -> CardData {
        CardData {
            kind: Kind::Artist,
            id: artist.id.clone(),
            title: artist.name.clone(),
            subtitle: "Artist".into(),
            cover: artist.cover_art.clone(),
        }
    }

    pub fn to_ui(&self) -> ui::Card {
        ui::Card {
            kind: self.kind.to_ui(),
            id: self.id.as_str().into(),
            title: self.title.as_str().into(),
            subtitle: self.subtitle.as_str().into(),
            cover: self.cover.as_str().into(),
        }
    }
}

/// A song that isn't in the library, which could be had by asking for the album it's on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OfferData {
    /// The album it's on: Lidarr's name for it, and its title.
    pub album: String,
    pub album_title: String,
    pub title: String,
    pub artist: String,
    /// The first artist credited: Lidarr's name for them, and what they're called.
    pub artist_id: String,
    pub artist_name: String,
    /// E.g. "…Something to Be • 2005".
    pub about: String,
    pub cover: String,
    pub seconds: u32,
    pub requested: bool,
    /// "EP" or "Single" when that's what it's on; empty for an album.
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShelfData {
    pub title: String,
    pub route: String,
    pub wrap: bool,
    pub cards: Vec<CardData>,
}

/// What the header's play button starts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum PlaySource {
    /// The songs listed on the page.
    #[default]
    Songs,
    /// Every song on these albums, fetched when asked.
    Albums(Vec<String>),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PageData {
    pub label: String,
    pub title: String,
    pub subtitle: String,
    pub subtitle_route: String,
    pub meta: String,
    pub about: String,
    pub cover: String,
    pub cover_round: bool,
    pub liked_songs: bool,
    pub can_play: bool,
    pub has_menu: bool,
    /// The API parameter and id used to like this page's subject, e.g. `("albumId", id)`.
    pub star: Option<(String, String)>,
    pub starred: bool,
    pub songs: Vec<Song>,
    pub tracks_title: String,
    pub show_album: bool,
    pub show_cover: bool,
    pub show_number: bool,
    /// Number rows by the files' track numbers instead of list position.
    pub album_numbers: bool,
    pub disc_sections: bool,
    pub shelves: Vec<ShelfData>,
    /// Shelves that go above the songs instead of below them.
    pub lead: Vec<ShelfData>,
    /// The label of a button beside the title, if the page has something to offer.
    pub action: String,
    /// On the Requests page: Lidarr's number for the album on each row.
    pub request_ids: Vec<i64>,
    /// Music that isn't in the library: the section's heading, songs that could be asked
    /// for, and the shelves beneath them.
    pub offers_title: String,
    pub offers_note: String,
    /// Where "Show all" beside the songs leads, for a list that's been cut short; and the
    /// same for the songs that aren't in the library.
    pub tracks_route: String,
    pub offers_route: String,
    pub offers: Vec<OfferData>,
    pub tail: Vec<ShelfData>,
    pub tiles: Vec<CardData>,
    pub grid: Vec<CardData>,
    /// The list to keep fetching from as the grid scrolls; `None` once it's all loaded.
    pub more: Option<AlbumList>,
    pub sorts: Vec<String>,
    pub sort: usize,
    pub empty: String,
    /// Name shown in the queue for what's playing from this page.
    pub context: String,
    pub play: PlaySource,
    /// Set on playlist pages, so rows can be removed.
    pub playlist: Option<Playlist>,
    /// The subject's artist, for the extras fetched by `enrich`.
    pub artist_id: String,
    /// Only the header is known so far; the rest is still on its way.
    #[serde(skip)]
    pub pending: bool,
}

/// A page to draw the instant something is clicked, from what the clicked item already says.
pub fn hint(kind: Kind, title: &str, subtitle: &str, cover: &str) -> Option<PageData> {
    if title.is_empty() {
        return None;
    }
    let (label, round) = match kind {
        Kind::Album | Kind::Request => ("Album", false),
        Kind::Artist | Kind::OutsideArtist => ("Artist", true),
        Kind::Playlist => ("Playlist", false),
        Kind::Genre | Kind::Mix => return None,
    };
    Some(PageData {
        label: label.into(),
        title: title.into(),
        subtitle: subtitle.into(),
        cover: cover.into(),
        cover_round: round,
        show_album: !matches!(kind, Kind::Album | Kind::Request),
        show_cover: !matches!(kind, Kind::Album | Kind::Request),
        show_number: true,
        pending: true,
        ..Default::default()
    })
}

pub fn load(client: &Client, route: &Route) -> ApiResult<PageData> {
    match route {
        Route::Home => home(client),
        Route::Search(query) => search(client, query),
        Route::Album(id) => album(client, id),
        Route::Artist(id) => artist(client, id),
        Route::Playlist(id) => playlist(client, id),
        Route::Liked => liked(client),
        Route::Radio => radio(client),
        Route::Mix(id) => Ok(mix(id)),
        Route::Requests => Ok(requests()),
        Route::OutsideArtist(id) => Ok(outside_artist(id)),
        Route::OutsideAlbum(id) => Ok(outside_album(id)),
        Route::Albums(sort) => albums(client, *sort),
        Route::Artists => artists(client),
        Route::Genres => genres(client),
        Route::Genre(name) => genre(client, name),
        Route::Settings | Route::Lyrics | Route::NowPlaying | Route::Downloads | Route::Import => {
            Ok(PageData::default())
        }
    }
}

/// Fetches a page's extras. Returns `None` when the route has none.
/// `publish` is handed the page whenever more of it is known before the whole is.
pub fn enrich(client: &Client, route: &Route, page: &PageData, publish: &mut dyn FnMut(PageData)) -> Option<PageData> {
    match route {
        Route::Search(query) if !query.is_empty() => search_extras(query, page, publish),
        Route::Album(id) => album_extras(client, id, page),
        Route::Artist(id) => artist_extras(client, id, page, publish),
        _ => None,
    }
}

fn greeting() -> &'static str {
    use chrono::Timelike;
    match chrono::Local::now().hour() {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

fn shelf(title: &str, route: &str, albums: &[Album]) -> Option<ShelfData> {
    (!albums.is_empty()).then(|| ShelfData {
        title: title.into(),
        route: route.into(),
        wrap: false,
        cards: albums.iter().map(CardData::album).collect(),
    })
}

/// Text as it's compared: folded, with runs of spaces closed up.
fn plain(text: &str) -> String {
    crate::search::fold(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The kinds of thing a search can be narrowed to, as its chips name them.
pub const SEARCH_FILTERS: [&str; 5] = ["All", "Songs", "Artists", "Albums", "Playlists"];
/// How many songs each list shows before "Show all", and how many cards make a shelf worth
/// a "Show all" of its own.
const TOP_SONGS: usize = 4;
const SHELF_ROW: usize = 5;

/// A search page as it's shown with one of its chips chosen. `whole` holds everything that
/// was found; "All" shows the top few of each kind, the others one kind in full.
pub fn search_view(whole: &PageData, filter: usize) -> PageData {
    let mut page = whole.clone();
    let looking = whole.offers_note.starts_with("Looking");
    if filter == 0 || filter >= SEARCH_FILTERS.len() {
        if page.songs.len() > TOP_SONGS {
            page.songs.truncate(TOP_SONGS);
            page.tracks_route = "filter:1".into();
        }
        if page.offers.len() > TOP_SONGS {
            page.offers.truncate(TOP_SONGS);
            page.offers_route = "filter:1".into();
        }
        for shelf in page.lead.iter_mut().chain(&mut page.shelves).chain(&mut page.tail) {
            if shelf.cards.len() > SHELF_ROW {
                if let Some(kind) = SEARCH_FILTERS.iter().position(|name| *name == shelf.title) {
                    shelf.route = format!("filter:{kind}");
                }
            }
        }
        return page;
    }
    // Every card of the kind chosen, on as many rows as they need.
    let of = |shelves: &[ShelfData], title: &str| -> Vec<ShelfData> {
        shelves.iter().filter(|shelf| shelf.title == title).cloned().map(|shelf| ShelfData { wrap: true, route: String::new(), ..shelf }).collect()
    };
    page.lead.clear();
    if filter == 1 {
        page.shelves.clear();
        page.tail.clear();
    } else {
        let title = SEARCH_FILTERS[filter];
        page.shelves = of(&whole.lead, title).into_iter().chain(of(&whole.shelves, title)).collect();
        page.tail = of(&whole.tail, title);
        page.songs.clear();
        page.offers.clear();
    }
    let outside = !page.offers.is_empty() || !page.tail.is_empty();
    if !outside && !looking {
        page.offers_title.clear();
        page.offers_note.clear();
    }
    let nothing = page.songs.is_empty() && page.shelves.is_empty() && !outside;
    page.empty = if nothing && !looking {
        format!("No {} found for {}.", SEARCH_FILTERS[filter].to_lowercase(), whole.context)
    } else {
        String::new()
    };
    page
}

/// What's been found beyond the library so far, put onto a copy of the search page.
fn with_outside(
    page: &PageData,
    note: &str,
    songs: &[crate::musicbrainz::SongOffer],
    artists: &[crate::lidarr::ArtistOffer],
    albums: &[crate::lidarr::Offer],
) -> PageData {
    let mut page = page.clone();
    if !(songs.is_empty() && artists.is_empty() && albums.is_empty()) {
        page.empty.clear();
    }
    page.offers_title = "Not in your library".into();
    page.offers_note = note.into();
    // What this app has asked for and is still waiting on.
    let awaited: std::collections::HashSet<String> =
        crate::asked::shared().into_iter().filter(|asked| asked.arrived == 0 && !asked.album.is_empty()).map(|asked| asked.album).collect();
    page.offers = songs
        .iter()
        .map(|song| OfferData {
            album: song.album.clone(),
            album_title: song.album_title.clone(),
            title: song.title.clone(),
            artist: song.artist.clone(),
            artist_id: song.artist_id.clone(),
            artist_name: song.artist_name.clone(),
            about: {
                // An album or EP by its name; a single is named after the song, so it's
                // just called what it is.
                let on = match song.kind.as_str() {
                    "Single" => "Single".to_string(),
                    "EP" => format!("{} (EP)", song.album_title),
                    _ => song.album_title.clone(),
                };
                if song.year.is_empty() { on } else { format!("{on} • {}", song.year) }
            },
            kind: song.kind.clone(),
            cover: crate::musicbrainz::cover(&song.album),
            seconds: song.seconds,
            requested: awaited.contains(&song.album)
                || albums.iter().any(|album| album.album == song.album && album.requested)
                || crate::lidarr::offered(&song.album).is_some_and(|offer| offer.requested),
        })
        .collect();
    page.tail.clear();
    if !artists.is_empty() {
        page.tail.push(ShelfData { title: "Artists".into(), route: String::new(), wrap: false, cards: artists.iter().map(outside_artist_card).collect() });
    }
    if !albums.is_empty() {
        page.tail.push(ShelfData { title: "Albums".into(), route: String::new(), wrap: false, cards: albums.iter().map(request_card).collect() });
    }
    page
}

/// Songs, artists and albums matching a search that aren't in the library, if requests are
/// switched on. Songs come from MusicBrainz; artists and albums from Lidarr, which answers
/// sooner. `publish` is handed the page each time more of it is known, so what's found
/// shows as it arrives; the finished page is returned.
fn search_extras(query: &str, page: &PageData, publish: &mut dyn FnMut(PageData)) -> Option<PageData> {
    const ASK: &str = "Ask Lidarr for any of these. A song comes with the album it's on, or as its single if it isn't on one.";
    let lidarr = crate::lidarr::current()?;
    // Leave out what the library already has.
    let index = crate::search::current();
    let have_albums: std::collections::HashSet<(String, String)> = index
        .as_ref()
        .map(|index| index.library.albums.iter().map(|album| (plain(&album.title), plain(&album.subtitle))).collect())
        .unwrap_or_default();
    let have_songs: std::collections::HashSet<(String, String)> = index
        .as_ref()
        .map(|index| index.library.songs.iter().map(|song| (plain(&song.title), plain(song.artist_label()))).collect())
        .unwrap_or_default();
    let typed = plain(query);
    let words = typed.split(' ').count();

    std::thread::scope(|scope| {
        let releases = scope.spawn(|| lidarr.find_releases(query).unwrap_or_default());
        // Artists named in what was typed. A name that's only a small part of it ("Lonely",
        // when a song's whole title was typed) is a coincidence and is left out.
        let (exact, named): (Vec<crate::lidarr::ArtistOffer>, Vec<crate::lidarr::ArtistOffer>) = lidarr
            .find_artists(query)
            .unwrap_or_default()
            .into_iter()
            .filter(|artist| {
                let name = plain(&artist.name);
                let within = format!(" {typed} ").contains(&format!(" {name} ")) && name.split(' ').count() * 2 >= words;
                !name.is_empty() && (name.contains(&typed) || within)
            })
            .take(10)
            .partition(|artist| plain(&artist.name) == typed);

        // Lidarr's part is in: show it while more is being looked for.
        let (albums, singles) = releases.join().unwrap_or_default();
        let mut albums: Vec<crate::lidarr::Offer> =
            albums.into_iter().filter(|offer| !have_albums.contains(&(plain(&offer.title), plain(&offer.artist)))).take(24).collect();
        // What's been offered before is remembered as it was: asked for, and with its cover.
        for album in &mut albums {
            if let Some(known) = crate::lidarr::offered(&album.album) {
                *album = known;
            }
        }
        crate::lidarr::remember(&albums);
        // Of those named outright, the ones of some note (Lidarr has their picture) go first.
        let (noted, obscure): (Vec<_>, Vec<_>) = exact.iter().cloned().partition(|artist| !artist.cover.is_empty());
        let noted_count = noted.len();
        let mut artists: Vec<crate::lidarr::ArtistOffer> = noted.into_iter().chain(obscure).chain(named.iter().cloned()).collect();
        crate::lidarr::remember_artists(&artists);

        // A well-known song typed by its title alone. Lidarr's list of singles says whose it
        // is: the one of that name that people have rated.
        let famous = singles
            .iter()
            .filter(|single| single.votes > 0 && !single.artist_id.is_empty() && crate::musicbrainz::same_words(&single.title, query))
            .max_by_key(|single| single.votes);
        // An artist's name was typed: what's wanted is what they've made, not songs that
        // happen to have the name in their title. Only an artist of some note counts, one
        // Lidarr has a picture of: there's an obscure band named after most famous songs.
        let meant = if famous.is_some() { None } else { exact.iter().find(|artist| !artist.cover.is_empty()) };
        if let Some(artist) = meant {
            publish(with_outside(page, "Looking for their albums…", &[], &artists, &albums));
            if let Ok((_, theirs)) = crate::musicbrainz::artist_albums(&artist.id) {
                let theirs: Vec<crate::lidarr::Offer> = theirs
                    .into_iter()
                    .filter(|offer| !have_albums.contains(&(plain(&offer.title), plain(&artist.name))))
                    .map(|offer| crate::lidarr::offered(&offer.album).unwrap_or(offer))
                    .collect();
                crate::lidarr::remember(&theirs);
                albums.retain(|album| !theirs.iter().any(|their| their.album == album.album));
                albums.splice(0..0, theirs);
                albums.truncate(24);
            }
            return Some(with_outside(page, "Ask Lidarr for any of these albums, or open the artist for everything they've made.", &[], &artists, &albums));
        }
        publish(with_outside(page, "Looking for songs beyond your library…", &[], &artists, &albums));

        let own = |songs: Vec<crate::musicbrainz::SongOffer>| -> Vec<crate::musicbrainz::SongOffer> {
            songs.into_iter().filter(|song| !have_songs.contains(&(plain(&song.title), plain(&song.artist)))).collect()
        };
        // For a well-known song, MusicBrainz would hand back a hundred versions in no order,
        // so the one that's meant is asked for by name and goes first.
        let mut songs: Vec<crate::musicbrainz::SongOffer> = Vec::new();
        if let Some(single) = famous {
            songs = own(crate::musicbrainz::song_by(&single.artist_id, &single.title).unwrap_or_default());
            songs.truncate(1);
            if !songs.is_empty() {
                publish(with_outside(page, "Looking for more songs…", &songs, &artists, &albums));
            }
        }
        for song in own(crate::musicbrainz::find_songs(query).unwrap_or_default()) {
            let listed = songs.iter().any(|have| {
                crate::musicbrainz::same_words(&have.title, &song.title) && crate::musicbrainz::same_words(&have.artist, &song.artist)
            });
            if !listed && songs.len() < 20 {
                songs.push(song);
            }
        }
        // Whoever the songs found are by joins the artists: typing a song's name should lead
        // to its artist too. They go after a noted artist who was named outright, before the
        // rest (an obscure band that shares the song's name, say).
        let mut by: Vec<crate::lidarr::ArtistOffer> = Vec::new();
        for song in songs.iter().take(3) {
            let listed = artists.iter().chain(&by).any(|artist| artist.id == song.artist_id);
            if !song.artist_id.is_empty() && !listed {
                by.push(
                    crate::lidarr::offered_artist(&song.artist_id)
                        .unwrap_or_else(|| crate::lidarr::ArtistOffer { id: song.artist_id.clone(), name: song.artist_name.clone(), ..Default::default() }),
                );
            }
        }
        artists.splice(noted_count..noted_count, by);
        artists.truncate(12);
        // The albums those songs are on go first among the albums. They're made up from what's
        // known of the song, which is less than Lidarr would say, so they aren't kept to act
        // on later: Lidarr is asked when one is opened or requested.
        let mut front = 0;
        for song in songs.iter().filter(|song| song.kind.is_empty()).take(3) {
            if !albums.iter().any(|album| album.album == song.album) {
                let sketch = crate::lidarr::Offer {
                    kind: String::new(),
                    album: song.album.clone(),
                    artist_id: song.artist_id.clone(),
                    title: song.album_title.clone(),
                    artist: song.artist_name.clone(),
                    year: song.year.clone(),
                    cover: crate::musicbrainz::cover(&song.album),
                    requested: false,
                };
                albums.insert(front, crate::lidarr::offered(&song.album).unwrap_or(sketch));
                front += 1;
            }
        }
        albums.truncate(12);
        let nothing = songs.is_empty() && artists.is_empty() && albums.is_empty();
        albums.truncate(24);
        let note = if nothing { "Nothing else was found beyond your library." } else { ASK };
        let found = with_outside(page, note, &songs, &artists, &albums);

        // Last, pictures for the artists that came from songs. Lidarr has those.
        let bare: Vec<&crate::lidarr::ArtistOffer> = artists.iter().filter(|artist| artist.cover.is_empty() && artist.about.is_empty()).collect();
        if bare.is_empty() {
            return Some(found);
        }
        publish(found);
        let lidarr = &lidarr;
        let asked: Vec<_> = bare
            .iter()
            .map(|artist| {
                let id = artist.id.clone();
                scope.spawn(move || lidarr.find_artists(&format!("lidarr:{id}")).ok().and_then(|found| found.into_iter().find(|artist| artist.id == id)))
            })
            .collect();
        let pictured: Vec<crate::lidarr::ArtistOffer> = asked.into_iter().filter_map(|asked| asked.join().ok().flatten()).collect();
        crate::lidarr::remember_artists(&pictured);
        for artist in &mut artists {
            if let Some(better) = pictured.iter().find(|better| better.id == artist.id) {
                *artist = better.clone();
            }
        }
        Some(with_outside(page, note, &songs, &artists, &albums))
    })
}

fn outside_artist_card(artist: &crate::lidarr::ArtistOffer) -> CardData {
    CardData {
        kind: Kind::OutsideArtist,
        id: artist.id.clone(),
        title: artist.name.clone(),
        subtitle: if artist.about.is_empty() { "Artist".into() } else { artist.about.clone() },
        cover: artist.cover.clone(),
    }
}

fn requests_off(title: &str) -> PageData {
    PageData { title: title.into(), empty: "Requests are switched off. Turn them on in Settings.".into(), ..Default::default() }
}

/// An artist as the wider world knows them: every studio album they've made, with the ones
/// in the library opening as usual and the rest there to be asked for.
fn outside_artist(id: &str) -> PageData {
    let Some(lidarr) = crate::lidarr::current() else { return requests_off("Artist") };
    let remembered = crate::lidarr::offered_artist(id);
    // If Lidarr keeps track of the artist, its list says what's wanted and what's arrived.
    let known = lidarr.known_artist(id).ok().flatten();
    let singles = crate::lidarr::singles();
    let found = match &known {
        Some((number, name)) => lidarr.releases_of(*number, name).map(|mut releases| {
            // Lidarr only lists the kinds of release the artist's profile covers, which may
            // not include singles and EPs: those come from MusicBrainz.
            if singles {
                for release in crate::musicbrainz::artist_releases(id, true).map(|(_, releases)| releases).unwrap_or_default() {
                    if release.kind() != "Album" && !releases.iter().any(|(known, _)| known.album == release.album) {
                        releases.push((release, false));
                    }
                }
            }
            (name.clone(), releases)
        }),
        None => crate::musicbrainz::artist_releases(id, singles).map(|(name, releases)| (name, releases.into_iter().map(|release| (release, false)).collect())),
    };
    let (name, albums) = match found {
        Ok(found) => found,
        Err(reason) => return PageData { label: "Artist".into(), title: remembered.map(|a| a.name).unwrap_or_default(), cover_round: true, empty: reason, ..Default::default() },
    };
    let name = if name.is_empty() { remembered.as_ref().map(|artist| artist.name.clone()).unwrap_or_default() } else { name };
    let offers: Vec<crate::lidarr::Offer> = albums.iter().map(|(offer, _)| offer.clone()).collect();
    crate::lidarr::remember(&offers);
    // Albums the library has are shown as its own, so they open and play.
    let library: std::collections::HashMap<String, CardData> = crate::search::current()
        .map(|index| {
            index
                .library
                .albums
                .iter()
                .filter(|album| plain(&album.subtitle) == plain(&name))
                .map(|album| (plain(&album.title), album.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut owned = 0;
    let mut open = 0;
    let mut card = |offer: &crate::lidarr::Offer, have: bool, counted: bool| -> CardData {
        // The year, what kind of thing it is if it isn't an album, and how it stands.
        let caption = |state: &str| {
            let kind = if offer.kind() == "Album" { "" } else { offer.kind() };
            [offer.year.as_str(), kind, state].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" • ")
        };
        if let Some(mine) = library.get(&plain(&offer.title)) {
            owned += counted as usize;
            return CardData { subtitle: caption("In your library"), ..mine.clone() };
        }
        if counted && !have && !offer.requested {
            open += 1;
        }
        CardData { subtitle: caption(if have { "Arrived" } else if offer.requested { "Requested" } else { "" }), ..request_card(offer) }
    };
    let cards: Vec<CardData> = albums.iter().filter(|(offer, _)| offer.kind() == "Album").map(|(offer, have)| card(offer, *have, true)).collect();
    let smaller: Vec<CardData> = albums.iter().filter(|(offer, _)| offer.kind() != "Album").map(|(offer, have)| card(offer, *have, false)).collect();
    let meta = match (cards.len(), owned) {
        (0, _) => String::new(),
        (all, 0) => count(all as u32, "album"),
        (all, owned) => format!("{} • {owned} in your library", count(all as u32, "album")),
    };
    PageData {
        label: "Artist".into(),
        title: name,
        meta,
        cover: remembered.map(|artist| artist.cover).unwrap_or_default(),
        cover_round: true,
        action: if open > 0 { "Add artist".into() } else { String::new() },
        empty: if cards.is_empty() && smaller.is_empty() { "Nothing is listed for this artist.".into() } else { String::new() },
        shelves: [("Albums", cards), ("Singles and EPs", smaller)]
            .into_iter()
            .filter(|(_, cards)| !cards.is_empty())
            .map(|(title, cards)| ShelfData { title: title.into(), route: String::new(), wrap: true, cards })
            .collect(),
        artist_id: id.to_string(),
        ..Default::default()
    }
}

/// An album that isn't in the library: what's on it, and a button to ask for it.
fn outside_album(id: &str) -> PageData {
    let Some(lidarr) = crate::lidarr::current() else { return requests_off("Album") };
    let offer = crate::lidarr::offered(id)
        .map(|offer| Ok(Some(offer)))
        .unwrap_or_else(|| lidarr.find_any(&format!("lidarr:{id}")).map(|found| found.into_iter().find(|offer| offer.album == id)));
    let offer = match offer {
        Ok(Some(offer)) => offer,
        Ok(None) => return PageData { label: "Album".into(), empty: "Lidarr doesn't list that one, so it can't be asked for.".into(), ..Default::default() },
        Err(reason) => return PageData { label: "Album".into(), empty: reason, ..Default::default() },
    };
    crate::lidarr::remember(std::slice::from_ref(&offer));
    let tracks = crate::musicbrainz::album_tracks(id);
    let (songs, empty) = match tracks {
        Ok(tracks) => (tracks, String::new()),
        Err(reason) => (Vec::new(), format!("The list of songs couldn't be fetched. {reason}")),
    };
    let discs = songs.iter().map(|track| track.disc).max().unwrap_or(1);
    let mut meta: Vec<String> = Vec::new();
    if !offer.year.is_empty() {
        meta.push(offer.year.clone());
    }
    if !songs.is_empty() {
        meta.push(count(songs.len() as u32, "song"));
    }
    meta.push(if offer.requested { "Requested".into() } else { "Not in your library".into() });
    PageData {
        label: offer.kind().into(),
        title: offer.title.clone(),
        subtitle: offer.artist.clone(),
        subtitle_route: if offer.artist_id.is_empty() { String::new() } else { format!("outside-artist:{}", offer.artist_id) },
        meta: meta.join(" • "),
        cover: if offer.cover.is_empty() { crate::musicbrainz::cover(id) } else { offer.cover.clone() },
        action: if offer.requested { "Requested".into() } else { format!("Request {}", offer.kind_word()) },
        songs: songs
            .into_iter()
            .map(|track| Song {
                title: track.title,
                artist: offer.artist.clone(),
                track: track.number,
                disc_number: track.disc,
                duration: track.seconds,
                ..Default::default()
            })
            .collect(),
        show_number: true,
        album_numbers: true,
        disc_sections: discs > 1,
        empty,
        artist_id: offer.artist_id,
        ..Default::default()
    }
}

pub fn request_card(offer: &crate::lidarr::Offer) -> CardData {
    CardData {
        kind: Kind::Request,
        id: offer.album.clone(),
        title: offer.title.clone(),
        subtitle: match (offer.requested, offer.year.is_empty()) {
            (true, _) => format!("{} • Requested", offer.artist),
            (false, true) => offer.artist.clone(),
            (false, false) => format!("{} • {}", offer.artist, offer.year),
        },
        cover: offer.cover.clone(),
    }
}

/// The albums that have been asked for: those that have arrived lately, then those still on
/// their way.
fn requests() -> PageData {
    let title = "Requests".to_string();
    let Some(lidarr) = crate::lidarr::current() else {
        return PageData { title, empty: "Requests are switched off. Turn them on in Settings.".into(), ..Default::default() };
    };
    // What this app asked for that has since turned up, newest first.
    let mut arrived: Vec<crate::asked::Asked> = crate::asked::shared().into_iter().filter(|asked| asked.arrived != 0 && !asked.library_album.is_empty()).collect();
    arrived.sort_by_key(|asked| std::cmp::Reverse(asked.arrived));
    let lead: Vec<ShelfData> = if arrived.is_empty() {
        Vec::new()
    } else {
        vec![ShelfData {
            title: "Arrived".into(),
            route: String::new(),
            wrap: true,
            cards: arrived
                .iter()
                .map(|asked| CardData {
                    kind: Kind::Album,
                    id: asked.library_album.clone(),
                    title: asked.title.clone(),
                    subtitle: if asked.song.is_empty() { asked.artist.clone() } else { format!("{} • for “{}”", asked.artist, asked.song) },
                    cover: asked.cover.clone(),
                })
                .collect(),
        }]
    };
    match lidarr.pending() {
        Err(reason) => PageData { title, lead, empty: reason, ..Default::default() },
        Ok(pending) if pending.is_empty() => PageData {
            title,
            empty: if lead.is_empty() {
                "Nothing is on its way. Search for a song, artist or album and pick it under “Not in your library”.".into()
            } else {
                "Nothing else is on its way.".into()
            },
            lead,
            ..Default::default()
        },
        Ok(pending) => PageData {
            title,
            lead,
            tracks_title: format!("On their way ({})", pending.len()),
            request_ids: pending.iter().map(|album| album.id).collect(),
            // Rows that name an album, not songs that can be played.
            songs: pending
                .into_iter()
                .map(|album| Song {
                    title: album.title,
                    artist: match album.progress {
                        Some(done) => format!("{} • Downloading, {done}%", album.artist),
                        None => format!("{} • Being looked for", album.artist),
                    },
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        },
    }
}

/// The page for one of the "Made for you" mixes.
fn mix(id: &str) -> PageData {
    let made = crate::search::current().and_then(|index| crate::mixes::one(&index, id));
    let Some(mut made) = made else {
        return PageData {
            label: "Made for you".into(),
            title: "Mix".into(),
            show_number: true,
            empty: "There isn't enough listening to make this mix from yet.".into(),
            ..Default::default()
        };
    };
    for song in &mut made.songs {
        // Whether it's liked may have changed since the copy of the library was made.
        song.starred = crate::search::is_liked(song).then(|| song.starred.clone().unwrap_or_default());
    }
    PageData {
        label: "Made for you".into(),
        title: made.title.clone(),
        subtitle: made.about,
        meta: format!(
            "{}, {}",
            count(made.songs.len() as u32, "song"),
            length(made.songs.iter().map(|s| s.duration).sum())
        ),
        cover: made.songs.first().map(|song| song.cover_art.clone()).unwrap_or_default(),
        can_play: true,
        show_album: true,
        show_cover: true,
        show_number: true,
        context: made.title,
        songs: made.songs,
        ..Default::default()
    }
}

fn home(client: &Client) -> ApiResult<PageData> {
    // Four independent lists: ask for them all at once.
    let (recent, newest, frequent, random) = std::thread::scope(|scope| {
        let list = |kind: AlbumList, size: u32| scope.spawn(move || client.album_list(&kind, size, 0));
        let (a, b, c, d) = (
            list(AlbumList::Recent, 12),
            list(AlbumList::Newest, 12),
            // More of these than are shown, to find old favourites among.
            list(AlbumList::Frequent, 60),
            list(AlbumList::Random, 12),
        );
        let join = |h: std::thread::ScopedJoinHandle<ApiResult<Vec<Album>>>| {
            h.join().unwrap_or_else(|_| Ok(Vec::new()))
        };
        (join(a), join(b), join(c), join(d))
    });
    let recent = recent?;
    // The remaining rows are extras: one failing shouldn't blank the page.
    let (newest, frequent, random) =
        (newest.unwrap_or_default(), frequent.unwrap_or_default(), random.unwrap_or_default());

    // Quick picks: what you played lately, topped up with the newest arrivals.
    let mut tiles: Vec<CardData> = Vec::new();
    for album in recent.iter().chain(newest.iter()) {
        if tiles.len() < 8 && !tiles.iter().any(|t| t.id == album.id) {
            tiles.push(CardData::album(album));
        }
    }
    // Lists put together from what's been played.
    let mixes: Vec<CardData> = crate::search::current()
        .map(|index| crate::mixes::all(&index))
        .unwrap_or_default()
        .into_iter()
        .map(|mix| CardData {
            kind: Kind::Mix,
            id: mix.id,
            title: mix.title,
            subtitle: mix.about,
            cover: mix.songs.iter().map(|song| song.cover_art.clone()).find(|cover| !cover.is_empty()).unwrap_or_default(),
        })
        .collect();
    let made = (!mixes.is_empty()).then(|| ShelfData { title: "Made for you".into(), route: String::new(), wrap: false, cards: mixes });
    // Albums played a lot, but not for a while.
    let now = chrono::Utc::now().timestamp();
    let neglected: Vec<Album> = frequent
        .iter()
        .filter(|album| {
            let last = crate::added::parse_time(&album.played);
            last > 0 && now - last > 45 * 24 * 3600
        })
        .take(12)
        .cloned()
        .collect();
    let frequent = &frequent[..frequent.len().min(12)];
    let shelves: Vec<ShelfData> = [
        made,
        shelf("Recently added", "albums:0", &newest),
        shelf("Recently played", "albums:3", &recent),
        shelf("Most played", "albums:4", frequent),
        (neglected.len() >= 4).then(|| shelf("It's been a while", "", &neglected)).flatten(),
        shelf("Something different", "", &random),
    ]
    .into_iter()
    .flatten()
    .collect();
    let empty = if shelves.is_empty() {
        "Your library is empty. Add music to Navidrome and it will show up here.".into()
    } else {
        String::new()
    };
    Ok(PageData { title: greeting().into(), tiles, shelves, empty, ..Default::default() })
}

fn search(client: &Client, query: &str) -> ApiResult<PageData> {
    if query.is_empty() {
        let mut genres = client.genres()?;
        genres.retain(|g| !g.name.is_empty());
        genres.sort_by(|a, b| b.album_count.cmp(&a.album_count).then(a.name.cmp(&b.name)));
        let cards = genres.iter().map(genre_card).collect::<Vec<_>>();
        let shelves = if cards.is_empty() {
            Vec::new()
        } else {
            vec![ShelfData { title: "Browse by genre".into(), route: String::new(), wrap: true, cards }]
        };
        return Ok(PageData { title: "Search".into(), shelves, ..Default::default() });
    }
    // The app's own copy of the library forgives typing mistakes; the server doesn't, and
    // is only asked while that copy hasn't arrived yet.
    let found = match crate::search::current() {
        Some(index) => {
            let mut found = index.search(query, &crate::search::is_liked, 24, 36, 60);
            for song in &mut found.songs {
                // Whether it's liked may have changed since the copy was made.
                song.starred = crate::search::is_liked(song).then(|| song.starred.clone().unwrap_or_default());
            }
            found
        }
        None => {
            let found = client.search(query, 24, 36, 60)?;
            crate::search::Results {
                artists: found.artists.iter().map(CardData::artist).collect(),
                albums: found.albums.iter().map(CardData::album).collect(),
                songs: found.songs,
                artist_first: false,
            }
        }
    };
    let row = |title: &str, cards: Vec<CardData>| {
        (!cards.is_empty()).then(|| ShelfData { title: title.into(), route: String::new(), wrap: false, cards })
    };
    let artists = row("Artists", found.artists);
    let albums = row("Albums", found.albums);
    let playlists = row("Playlists", crate::search::playlists_matching(query));
    // An artist who matches what was typed goes above the songs.
    let (lead, shelves): (Vec<ShelfData>, Vec<ShelfData>) = if found.artist_first {
        (artists.into_iter().collect(), albums.into_iter().chain(playlists).collect())
    } else {
        (Vec::new(), artists.into_iter().chain(albums).chain(playlists).collect())
    };
    let empty = if found.songs.is_empty() && shelves.is_empty() && lead.is_empty() {
        format!("Nothing in your library matches “{query}”. Check the spelling or try fewer words.")
    } else {
        String::new()
    };
    // With requests switched on, more is on its way from beyond the library.
    let (offers_title, offers_note) = match crate::lidarr::current() {
        Some(_) => ("Not in your library".to_string(), "Looking for songs, artists and albums beyond your library…".to_string()),
        None => Default::default(),
    };
    Ok(PageData {
        tracks_title: "Songs".into(),
        songs: found.songs,
        show_album: true,
        show_cover: true,
        show_number: false,
        shelves,
        lead,
        offers_title,
        offers_note,
        empty,
        context: format!("“{query}”"),
        ..Default::default()
    })
}

fn release_kind(album: &Album) -> &'static str {
    let has = |name: &str| album.release_types.iter().any(|t| t.eq_ignore_ascii_case(name));
    if has("single") {
        "Single"
    } else if has("ep") {
        "EP"
    } else if has("compilation") {
        "Compilation"
    } else {
        "Album"
    }
}

fn album(client: &Client, id: &str) -> ApiResult<PageData> {
    let album = client.album(id)?;
    let mut parts = Vec::new();
    if album.year > 0 {
        parts.push(album.year.to_string());
    }
    parts.push(count(album.songs.len() as u32, "song"));
    parts.push(length(album.songs.iter().map(|s| s.duration).sum()));

    Ok(PageData {
        label: release_kind(&album).into(),
        title: album.name.clone(),
        subtitle: album.artist_label().to_string(),
        subtitle_route: if album.artist_id.is_empty() {
            String::new()
        } else {
            format!("artist:{}", album.artist_id)
        },
        meta: parts.join(", "),
        cover: album.cover_art.clone(),
        can_play: !album.songs.is_empty(),
        has_menu: true,
        star: Some(("albumId".into(), album.id.clone())),
        starred: album.starred.is_some(),
        show_number: true,
        album_numbers: true,
        disc_sections: true,
        empty: "This album has no songs.".into(),
        context: album.name.clone(),
        artist_id: album.artist_id.clone(),
        songs: album.songs,
        ..Default::default()
    })
}

/// Other releases by the album's artist.
fn album_extras(client: &Client, id: &str, page: &PageData) -> Option<PageData> {
    if page.artist_id.is_empty() {
        return None;
    }
    let artist = client.artist(&page.artist_id).ok()?;
    let others: Vec<CardData> =
        artist.albums.iter().filter(|a| a.id != id).map(CardData::album_by_year).collect();
    if others.is_empty() {
        return None;
    }
    let mut page = page.clone();
    page.shelves = vec![ShelfData {
        title: format!("More by {}", page.subtitle),
        route: format!("artist:{}", page.artist_id),
        wrap: false,
        cards: others,
    }];
    Some(page)
}

fn playlist(client: &Client, id: &str) -> ApiResult<PageData> {
    let mut playlist = client.playlist(id)?;
    let songs = std::mem::take(&mut playlist.songs);
    let mut parts = vec![count(songs.len() as u32, "song")];
    if !songs.is_empty() {
        parts.push(length(songs.iter().map(|s| s.duration).sum()));
    }
    Ok(PageData {
        label: "Playlist".into(),
        title: playlist.name.clone(),
        subtitle: playlist.owner.clone(),
        meta: parts.join(", "),
        cover: playlist.cover_art.clone(),
        can_play: !songs.is_empty(),
        has_menu: true,
        show_album: true,
        show_cover: true,
        show_number: true,
        empty: "This playlist is empty. Right-click any song and choose Add to playlist.".into(),
        context: playlist.name.clone(),
        songs,
        playlist: Some(playlist),
        ..Default::default()
    })
}

fn liked(client: &Client) -> ApiResult<PageData> {
    let starred = client.starred()?;
    let songs = starred.songs;
    Ok(PageData {
        label: "Playlist".into(),
        title: "Liked songs".into(),
        meta: count(songs.len() as u32, "song"),
        liked_songs: true,
        can_play: !songs.is_empty(),
        show_album: true,
        show_cover: true,
        show_number: true,
        empty: "Songs you like show up here. Click the heart next to any song to save it.".into(),
        context: "Liked songs".into(),
        songs,
        ..Default::default()
    })
}

fn radio(client: &Client) -> ApiResult<PageData> {
    let songs = client.radio_stations()?;
    Ok(PageData {
        label: "Live streams".into(),
        title: "Radio".into(),
        meta: count(songs.len() as u32, "station"),
        can_play: !songs.is_empty(),
        show_number: true,
        empty: "No stations yet. Add them in Navidrome's web interface, under Radios, and they'll appear here.".into(),
        context: "Radio".into(),
        songs,
        ..Default::default()
    })
}

/// Last.fm biographies arrive with a trailing HTML link; show the prose only.
fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out.replace("&amp;", "&").replace("&quot;", "\"").replace("&#39;", "'");
    out.trim().trim_end_matches("Read more on Last.fm").trim().to_string()
}

fn artist(client: &Client, id: &str) -> ApiResult<PageData> {
    let artist = client.artist(id)?;
    let mut albums: Vec<&Album> = artist.albums.iter().collect();
    albums.sort_by(|a, b| b.year.cmp(&a.year).then(a.name.cmp(&b.name)));
    let (singles, full): (Vec<&Album>, Vec<&Album>) =
        albums.iter().partition(|a| matches!(release_kind(a), "Single" | "EP"));

    let mut shelves = Vec::new();
    for (title, group) in [("Albums", &full), ("Singles and EPs", &singles)] {
        if !group.is_empty() {
            shelves.push(ShelfData {
                title: title.into(),
                route: String::new(),
                wrap: true,
                cards: group.iter().map(|a| CardData::album_by_year(a)).collect(),
            });
        }
    }

    Ok(PageData {
        label: "Artist".into(),
        title: artist.name.clone(),
        meta: count(artist.albums.len() as u32, "album"),
        cover: artist.cover_art.clone(),
        cover_round: true,
        can_play: !artist.albums.is_empty(),
        tracks_title: "Popular".into(),
        show_album: true,
        show_cover: true,
        show_number: true,
        shelves,
        context: artist.name.clone(),
        play: PlaySource::Albums(albums.iter().map(|a| a.id.clone()).collect()),
        ..Default::default()
    })
}

/// Popular songs, biography and similar artists. The server gets these from Last.fm, which
/// can take seconds, so the artist's own albums are on screen long before.
/// The studio albums by an artist that the library doesn't have, as a shelf to ask for them
/// from. `mbid` is MusicBrainz's name for the artist if the server knows it; otherwise Lidarr
/// is asked who goes by `name`.
fn missing_albums(name: &str, mbid: &str, page: &PageData) -> Option<ShelfData> {
    let lidarr = crate::lidarr::current()?;
    let artist = if mbid.is_empty() {
        let found = lidarr.find_artists(name).ok()?;
        let mut same = found.into_iter().filter(|artist| crate::musicbrainz::same_words(&artist.name, name));
        // Of several by that name, the one of some note.
        let first = same.next()?;
        if first.cover.is_empty() { same.find(|artist| !artist.cover.is_empty()).unwrap_or(first) } else { first }
    } else {
        crate::lidarr::ArtistOffer { id: mbid.to_string(), name: name.to_string(), ..Default::default() }
    };
    crate::lidarr::remember_artists(std::slice::from_ref(&artist));
    let albums: Vec<(crate::lidarr::Offer, bool)> = match lidarr.known_artist(&artist.id).ok().flatten() {
        Some((number, known)) => lidarr.albums_of(number, &known).ok()?,
        None => crate::musicbrainz::artist_albums(&artist.id).ok()?.1.into_iter().map(|album| (album, false)).collect(),
    };
    // What the page already lists is what the library has.
    let have: Vec<&str> = page.shelves.iter().flat_map(|shelf| &shelf.cards).filter(|card| card.kind == Kind::Album).map(|card| card.title.as_str()).collect();
    let missing: Vec<crate::lidarr::Offer> = albums
        .into_iter()
        .filter(|(offer, arrived)| !arrived && !have.iter().any(|title| crate::musicbrainz::same_words(title, &offer.title)))
        .map(|(offer, _)| offer)
        .collect();
    if missing.is_empty() {
        return None;
    }
    crate::lidarr::remember(&missing);
    Some(ShelfData {
        title: "Not in your library".into(),
        route: format!("outside-artist:{}", artist.id),
        wrap: false,
        cards: missing
            .iter()
            .map(|offer| CardData {
                subtitle: match (offer.year.is_empty(), offer.requested) {
                    (true, true) => "Requested".into(),
                    (true, false) => "Album".into(),
                    (false, true) => format!("{} • Requested", offer.year),
                    (false, false) => offer.year.clone(),
                },
                ..request_card(offer)
            })
            .collect(),
    })
}

fn artist_extras(client: &Client, id: &str, page: &PageData, publish: &mut dyn FnMut(PageData)) -> Option<PageData> {
    let name = page.title.clone();
    let (info, top) = std::thread::scope(|scope| {
        let info = scope.spawn(|| client.artist_info(id).unwrap_or_default());
        let top = scope.spawn(|| client.top_songs(&name, 5).unwrap_or_default());
        (info.join().unwrap_or_default(), top.join().unwrap_or_default())
    });
    let similar: Vec<CardData> =
        info.similar.iter().filter(|a| !a.id.is_empty()).map(CardData::artist).collect();
    let about = plain_text(&info.biography);
    let asking = crate::lidarr::current().is_some();
    if top.is_empty() && similar.is_empty() && about.is_empty() && !asking {
        return None;
    }
    let mut page = page.clone();
    page.songs = top;
    page.about = about;
    page.shelves.retain(|s| s.title != "Fans also like" && s.title != "Not in your library");
    let fans = (!similar.is_empty()).then(|| ShelfData { title: "Fans also like".into(), route: String::new(), wrap: false, cards: similar });
    if !asking {
        page.shelves.extend(fans);
        return Some(page);
    }
    // With requests switched on: what they've made that isn't here, which takes a moment
    // longer to find, so the rest is shown meanwhile.
    let mut shown = page.clone();
    shown.shelves.extend(fans.clone());
    publish(shown);
    page.shelves.extend(missing_albums(&name, &info.music_brainz_id, &page));
    page.shelves.extend(fans);
    Some(page)
}

fn albums(client: &Client, sort: usize) -> ApiResult<PageData> {
    let sort = sort.min(ALBUM_SORTS.len() - 1);
    let list = ALBUM_SORTS[sort].1.clone();
    let albums = client.album_list(&list, GRID_PAGE, 0)?;
    let more = (albums.len() as u32 == GRID_PAGE).then(|| list.clone());
    Ok(PageData {
        title: "Albums".into(),
        empty: match (albums.is_empty(), &list) {
            (false, _) => String::new(),
            (true, AlbumList::Starred) => {
                "Albums you like show up here. Open an album and click its heart.".into()
            }
            (true, _) => "No albums in your library yet.".into(),
        },
        grid: albums.iter().map(CardData::album).collect(),
        more,
        sorts: ALBUM_SORTS.iter().map(|s| s.0.to_string()).collect(),
        sort,
        ..Default::default()
    })
}

fn artists(client: &Client) -> ApiResult<PageData> {
    let artists = client.artists()?;
    Ok(PageData {
        title: "Artists".into(),
        empty: if artists.is_empty() { "No artists in your library yet.".into() } else { String::new() },
        grid: artists
            .iter()
            .map(|a| CardData { subtitle: count(a.album_count, "album"), ..CardData::artist(a) })
            .collect(),
        ..Default::default()
    })
}

fn genre_card(genre: &crate::subsonic::Genre) -> CardData {
    CardData {
        kind: Kind::Genre,
        id: genre.name.clone(),
        title: genre.name.clone(),
        subtitle: format!("{}, {}", count(genre.album_count, "album"), count(genre.song_count, "song")),
        cover: String::new(),
    }
}

fn genres(client: &Client) -> ApiResult<PageData> {
    let mut genres = client.genres()?;
    genres.retain(|g| !g.name.is_empty());
    genres.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(PageData {
        title: "Genres".into(),
        empty: if genres.is_empty() { "None of your music has a genre tag.".into() } else { String::new() },
        grid: genres.iter().map(genre_card).collect(),
        ..Default::default()
    })
}

fn genre(client: &Client, name: &str) -> ApiResult<PageData> {
    let list = AlbumList::ByGenre(name.to_string());
    let albums = client.album_list(&list, GRID_PAGE, 0)?;
    let more = (albums.len() as u32 == GRID_PAGE).then_some(list);
    Ok(PageData {
        title: name.to_string(),
        empty: if albums.is_empty() { "No albums are tagged with this genre.".into() } else { String::new() },
        grid: albums.iter().map(CardData::album).collect(),
        more,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markup_from_biographies() {
        let bio = "A trio from Leeds &amp; beyond. <a target='_blank' href=\"https://last.fm/x\">Read more on Last.fm</a>";
        assert_eq!(plain_text(bio), "A trio from Leeds & beyond.");
    }

    #[test]
    fn narrows_a_search_to_one_kind() {
        let cards = |count: usize, kind: Kind| -> Vec<CardData> {
            (0..count).map(|n| CardData { kind, id: n.to_string(), title: format!("Card {n}"), ..Default::default() }).collect()
        };
        let shelf = |title: &str, count: usize, kind: Kind| ShelfData { title: title.into(), route: String::new(), wrap: false, cards: cards(count, kind) };
        let whole = PageData {
            songs: (0..9).map(|n| Song { id: n.to_string(), title: format!("Song {n}"), ..Default::default() }).collect(),
            lead: vec![shelf("Artists", 2, Kind::Artist)],
            shelves: vec![shelf("Albums", 8, Kind::Album)],
            offers_title: "Not in your library".into(),
            offers_note: "Ask Lidarr for any of these.".into(),
            offers: (0..6).map(|n| OfferData { title: format!("Offer {n}"), ..Default::default() }).collect(),
            tail: vec![shelf("Artists", 7, Kind::OutsideArtist)],
            context: "“words”".into(),
            ..Default::default()
        };
        // All: the top few songs of each list, with a way to the rest.
        let all = search_view(&whole, 0);
        assert_eq!((all.songs.len(), all.offers.len()), (4, 4));
        assert_eq!((all.tracks_route.as_str(), all.offers_route.as_str()), ("filter:1", "filter:1"));
        assert_eq!(all.lead[0].route, "");
        assert_eq!((all.shelves[0].route.as_str(), all.tail[0].route.as_str()), ("filter:3", "filter:2"));
        // Songs: every song, in the library and out of it, and nothing else.
        let songs = search_view(&whole, 1);
        assert_eq!((songs.songs.len(), songs.offers.len()), (9, 6));
        assert!(songs.lead.is_empty() && songs.shelves.is_empty() && songs.tail.is_empty() && songs.tracks_route.is_empty());
        // Artists: the library's and the others', on as many rows as they need.
        let artists = search_view(&whole, 2);
        assert!(artists.songs.is_empty() && artists.offers.is_empty() && artists.lead.is_empty());
        assert_eq!((artists.shelves.len(), artists.shelves[0].cards.len(), artists.shelves[0].wrap), (1, 2, true));
        assert_eq!((artists.tail[0].cards.len(), artists.tail[0].wrap, artists.offers_title.as_str()), (7, true, "Not in your library"));
        // Albums: there are none beyond the library, so that heading goes.
        let albums = search_view(&whole, 3);
        assert_eq!((albums.shelves[0].cards.len(), albums.tail.len(), albums.offers_title.as_str()), (8, 0, ""));
        // A kind with nothing says so, unless more is still being looked for.
        let bare = PageData { songs: whole.songs.clone(), context: "“words”".into(), ..Default::default() };
        assert_eq!(search_view(&bare, 3).empty, "No albums found for “words”.");
        let looking = PageData { offers_title: "Not in your library".into(), offers_note: "Looking for songs…".into(), ..bare.clone() };
        assert_eq!(search_view(&looking, 3).empty, "");
        assert_eq!(search_view(&looking, 3).offers_note, "Looking for songs…");
    }

    #[test]
    fn cached_pages_survive_a_round_trip() {
        let page = PageData {
            title: "Glass Meridian".into(),
            star: Some(("albumId".into(), "al-1".into())),
            songs: vec![Song { id: "s1".into(), title: "Aperture".into(), duration: 45, ..Default::default() }],
            more: Some(AlbumList::ByGenre("Jazz".into())),
            play: PlaySource::Albums(vec!["al-1".into()]),
            ..Default::default()
        };
        let json = serde_json::to_string(&page).unwrap();
        assert_eq!(serde_json::from_str::<PageData>(&json).unwrap(), page);
    }
}
