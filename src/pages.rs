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
    /// Set when it's a song the library has, suggested for a playlist: the song's id. The
    /// album and artist ids are then the library's, and `requested` means it's been added.
    pub song: String,
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
    /// Figures across the top of the page: each a number and what it counts.
    pub stats: Vec<(String, String)>,
    pub about: String,
    pub cover: String,
    pub cover_round: bool,
    pub liked_songs: bool,
    pub can_play: bool,
    pub has_menu: bool,
    /// Made from the copy of the library on this PC, not asked of the server: there's no
    /// call to keep a copy of it on disk as well.
    #[serde(skip)]
    pub local: bool,
    /// Something that went wrong on this page, and the name of what can be done about it.
    #[serde(default)]
    pub notice: String,
    #[serde(default)]
    pub notice_action: String,
    /// A second thing that can be done about it.
    #[serde(default)]
    pub notice_other: String,
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
    /// What the songs among them are headed, if not "Songs".
    pub offers_heading: String,
    /// That section goes at the top of the page, not the foot.
    pub offers_first: bool,
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
        Route::Songs => Ok(all_songs()),
        Route::Radio => radio(client),
        Route::Mix(id) => Ok(mix(id)),
        Route::Requests => Ok(requests()),
        Route::Stats(period) => Ok(stats(*period)),
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
        Route::Home => home_extras(client, page),
        Route::Artist(id) => artist_extras(client, id, page, publish),
        Route::OutsideArtist(id) => outside_artist_extras(id, page),
        Route::Playlist(_) => playlist_extras(client, page),
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

/// A song that could be asked for, as a row on a page.
fn offer_data(song: &crate::musicbrainz::SongOffer, requested: bool) -> OfferData {
    // An album or EP by its name; a single is named after the song, so it's just called
    // what it is.
    let on = match song.kind.as_str() {
        "Single" => "Single".to_string(),
        "EP" => format!("{} (EP)", song.album_title),
        _ => song.album_title.clone(),
    };
    OfferData {
        album: song.album.clone(),
        album_title: song.album_title.clone(),
        title: song.title.clone(),
        artist: song.artist.clone(),
        artist_id: song.artist_id.clone(),
        artist_name: song.artist_name.clone(),
        about: if song.year.is_empty() { on } else { format!("{on} • {}", song.year) },
        kind: song.kind.clone(),
        cover: crate::musicbrainz::cover(&song.album),
        seconds: song.seconds,
        requested,
        song: String::new(),
    }
}

/// How many plays make an artist one of some note, and how many put it beyond doubt that
/// their name, typed, means them and not a song called the same.
const NOTED: u64 = 10_000;
const UNMISTAKABLE: u64 = 5_000_000;
/// How many of an artist's most played songs are listed.
const POPULAR: usize = 8;

/// What an artist is most played for. ListenBrainz says outright for the best known; for
/// the rest it's worked out from their releases, which takes a couple of seconds longer.
fn top_of(artist: &str, releases: &[crate::lidarr::Offer]) -> Vec<crate::listenbrainz::TopSong> {
    match crate::listenbrainz::top_songs(artist) {
        Ok(top) if !top.is_empty() => top,
        _ => crate::musicbrainz::played_most(artist, releases).unwrap_or_default(),
    }
}

/// An artist's most played songs that the library doesn't have, each on the release of
/// theirs it can be asked for with. `top` is ListenBrainz's list of what they're played
/// for and `releases` everything they've put out. Also says
/// whether any were left out for being in the library already.
fn popular_songs(top: &[crate::listenbrainz::TopSong], artist: &str, name: &str, releases: &[crate::lidarr::Offer]) -> (Vec<crate::musicbrainz::SongOffer>, bool) {
    let have: std::collections::HashSet<String> = crate::search::current()
        .map(|index| {
            index
                .library
                .songs
                .iter()
                .filter(|song| crate::musicbrainz::words(song.artist_label()).join(" ").contains(&crate::musicbrainz::words(name).join(" ")))
                .map(|song| plain(crate::musicbrainz::base(&song.title)))
                .collect()
        })
        .unwrap_or_default();
    // A song on an album that's here is here too, whatever the file calls it.
    let have_albums: std::collections::HashSet<String> = crate::search::current()
        .map(|index| index.library.albums.iter().filter(|album| plain(&album.subtitle) == plain(name)).map(|album| plain(&album.title)).collect())
        .unwrap_or_default();
    let all = crate::musicbrainz::popular(top, releases, artist, name);
    let mut left_out = false;
    let mut songs: Vec<crate::musicbrainz::SongOffer> = Vec::new();
    for (place, song) in all.into_iter().enumerate() {
        if have.contains(&plain(crate::musicbrainz::base(&song.title))) || have_albums.contains(&plain(&song.album_title)) {
            // Only what would have been listed counts as left out.
            left_out |= place < POPULAR;
        } else if songs.len() < POPULAR {
            songs.push(song);
        }
    }
    (songs, left_out)
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
        .map(|song| {
            let requested = awaited.contains(&song.album)
                || albums.iter().any(|album| album.album == song.album && album.requested)
                || crate::lidarr::offered(&song.album).is_some_and(|offer| offer.requested);
            offer_data(song, requested)
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
        let own = |songs: Vec<crate::musicbrainz::SongOffer>| -> Vec<crate::musicbrainz::SongOffer> {
            songs.into_iter().filter(|song| !have_songs.contains(&(plain(&song.title), plain(&song.artist)))).collect()
        };
        // The songs matching what was typed. Of those that answer it equally well, the most
        // played go first.
        let look = || -> Vec<crate::musicbrainz::SongOffer> {
            let mut found = own(crate::musicbrainz::find_songs(query).unwrap_or_default());
            let recordings: Vec<String> = found.iter().flat_map(|song| song.recordings.iter().cloned()).collect();
            if let Ok(counts) = crate::listenbrainz::listens(&recordings) {
                crate::musicbrainz::by_listens(&mut found, &counts);
            }
            found
        };
        // An artist's name was typed: what's wanted is what they've made, not songs that
        // happen to have the name in their title. But there's an obscure band named after
        // most famous songs, so of those by that name it's the one played most, and one of
        // some note: Lidarr has their picture, or they've been played a fair bit.
        let plays = if famous.is_some() || exact.is_empty() {
            Default::default()
        } else {
            crate::listenbrainz::artist_listens(&exact.iter().map(|artist| artist.id.clone()).collect::<Vec<_>>()).unwrap_or_default()
        };
        let played = |artist: &crate::lidarr::ArtistOffer| plays.get(&artist.id).copied().unwrap_or(0);
        let mut meant = if famous.is_some() {
            None
        } else {
            // The first of the most played; with nothing known of plays, the first pictured.
            exact.iter().rev().filter(|artist| !artist.cover.is_empty() || played(artist) >= NOTED).max_by_key(|artist| played(artist))
        };
        // A name that's as likely a song's ("Believer", "Home"): unless the artist is one of
        // the most played there are, the songs of that name are looked up too, and if one is
        // played more than everything the artist has made, the song is what's meant.
        let mut looked: Option<Vec<crate::musicbrainz::SongOffer>> = None;
        if let Some(artist) = meant.filter(|artist| played(artist) < UNMISTAKABLE) {
            publish(with_outside(page, "Looking for songs and albums…", &[], &artists, &albums));
            let found = look();
            let best = found.iter().filter(|song| song.close <= 1).map(|song| song.listens).max().unwrap_or(0);
            if best > played(artist) {
                meant = None;
            }
            looked = Some(found);
        }
        if let Some(artist) = meant {
            // They go first among the artists.
            if let Some(place) = artists.iter().position(|other| other.id == artist.id) {
                let first = artists.remove(place);
                artists.insert(0, first);
            }
        }
        if let Some(artist) = meant {
            publish(with_outside(page, "Looking for their songs and albums…", &[], &artists, &albums));
            let mut songs = Vec::new();
            if let Ok((_, releases)) = crate::musicbrainz::artist_releases(&artist.id, crate::lidarr::singles()) {
                let theirs: Vec<crate::lidarr::Offer> = releases
                    .iter()
                    .filter(|offer| offer.kind() == "Album" && !have_albums.contains(&(plain(&offer.title), plain(&artist.name))))
                    .map(|offer| crate::lidarr::offered(&offer.album).unwrap_or_else(|| offer.clone()))
                    .collect();
                crate::lidarr::remember(&theirs);
                albums.retain(|album| !theirs.iter().any(|their| their.album == album.album));
                albums.splice(0..0, theirs);
                albums.truncate(24);
                publish(with_outside(page, "Looking for their most played songs…", &[], &artists, &albums));
                songs = popular_songs(&top_of(&artist.id, &releases), &artist.id, &artist.name, &releases).0;
            }
            let note = if songs.is_empty() {
                "Ask Lidarr for any of these albums, or open the artist for everything they've made."
            } else {
                "Their most played songs come first. Ask Lidarr for any of these, or open the artist for everything they've made."
            };
            return Some(with_outside(page, note, &songs, &artists, &albums));
        }
        publish(with_outside(page, "Looking for songs beyond your library…", &[], &artists, &albums));

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
        let found = match looked {
            Some(found) => found,
            None => look(),
        };
        for song in found {
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
        // Beneath the songs they're most played for, which arrive after (see
        // `outside_artist_extras`); that's where the page puts `tail`.
        tail: [("Albums", cards), ("Singles and EPs", smaller)]
            .into_iter()
            .filter(|(_, cards)| !cards.is_empty())
            .map(|(title, cards)| ShelfData { title: title.into(), route: String::new(), wrap: true, cards })
            .collect(),
        artist_id: id.to_string(),
        ..Default::default()
    }
}

/// The songs an artist is most played for, for the top of their page. Found after the page
/// itself, since it takes a few more questions.
fn outside_artist_extras(id: &str, page: &PageData) -> Option<PageData> {
    crate::lidarr::current()?;
    if page.title.is_empty() || !page.offers.is_empty() {
        return None;
    }
    let (_, releases) = crate::musicbrainz::artist_releases(id, crate::lidarr::singles()).ok()?;
    let (popular, left_out) = popular_songs(&top_of(id, &releases), id, &page.title, &releases);
    if popular.is_empty() {
        return None;
    }
    let awaited: std::collections::HashSet<String> =
        crate::asked::shared().into_iter().filter(|asked| asked.arrived == 0 && !asked.album.is_empty()).map(|asked| asked.album).collect();
    let mut page = page.clone();
    page.offers_heading = if left_out { "Popular, and not in your library".into() } else { "Popular".into() };
    page.offers = popular
        .iter()
        .map(|song| offer_data(song, awaited.contains(&song.album) || crate::lidarr::offered(&song.album).is_some_and(|offer| offer.requested)))
        .collect();
    Some(page)
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
    let mut page = match lidarr.pending() {
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
    };
    // A batch of albums being asked for (see `bulk`): how it stands, and what it couldn't ask for.
    let (notice, action, other) = crate::bulk::summary().notice();
    page.notice = notice;
    page.notice_action = action.to_string();
    page.notice_other = other.to_string();
    let missed = crate::bulk::missed();
    if !missed.is_empty() {
        if !crate::bulk::running() && page.tracks_title.is_empty() {
            page.empty = String::new();
        }
        page.offers_first = true;
        page.offers_title = "Couldn't be asked for".into();
        page.offers_note = "Lidarr was asked for these by the names your playlists give them, and by their songs. Pick one to search for it yourself.".into();
        page.offers_heading = if missed.len() == 1 { "1 album".into() } else { format!("{} albums", missed.len()) };
        page.offers = missed
            .into_iter()
            .map(|want| {
                let song = want.songs.first().cloned().unwrap_or_default();
                OfferData {
                    title: if want.album.is_empty() { song.clone() } else { want.album.clone() },
                    artist: want.artist.clone(),
                    artist_name: want.artist,
                    about: if want.album.is_empty() || song.is_empty() { want.reason } else { format!("{}, for “{song}”", want.reason) },
                    ..Default::default()
                }
            })
            .collect();
    }
    page
}

/// The page for one of the "Made for you" mixes.
/// How far back "Your stats" can look.
pub const STATS_PERIODS: [&str; 3] = ["All time", "Last 6 months", "Last 4 weeks"];

/// A number with its thousands marked off: 5210 is "5,210".
fn grouped(number: u64) -> String {
    let digits = number.to_string();
    let mut out = String::new();
    for (place, digit) in digits.chars().enumerate() {
        if place > 0 && (digits.len() - place) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn plays(times: u64) -> String {
    format!("{} {}", grouped(times), if times == 1 { "play" } else { "plays" })
}

/// What's been played most: songs, artists, albums and kinds of music, with how much
/// listening it comes to. "All time" goes by the server's count of each song's plays, which
/// takes in every app and device. The shorter periods go by this app's own log of plays,
/// since the server doesn't say when anything was played.
fn stats(period: usize) -> PageData {
    let title = "Your stats".to_string();
    let period = period.min(STATS_PERIODS.len() - 1);
    let Some(index) = crate::search::current() else {
        return PageData { title, empty: "Your library is still being read. Try again in a moment.".into(), ..Default::default() };
    };
    let now = chrono::Utc::now().timestamp();
    let log = crate::plays::shared();
    // How many times each song was played in the period.
    let mut times: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    if period == 0 {
        for song in index.library.songs.iter().filter(|song| song.play_count > 0) {
            times.insert(song.id.as_str(), song.play_count as u64);
        }
    } else {
        let since = now - if period == 1 { 183 } else { 28 } * 24 * 3600;
        for (song, _) in log.iter().filter(|(_, at)| *at >= since) {
            *times.entry(song.as_str()).or_default() += 1;
        }
    }
    let mut songs: Vec<(&Song, u64)> = index.library.songs.iter().filter_map(|song| times.get(song.id.as_str()).map(|times| (song, *times))).collect();
    songs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.title.cmp(&b.0.title)));
    // The log only knows what's happened since it was started.
    let started = log.first().map(|(_, at)| *at).unwrap_or(now);
    let note = if period == 0 {
        String::new()
    } else {
        let day = chrono::DateTime::from_timestamp(started, 0).map(|at| at.with_timezone(&chrono::Local).format("%-d %B %Y").to_string()).unwrap_or_default();
        format!("Plays in this period are the ones this app has counted on this PC, starting {day}. “All time” uses your server's own count, which takes in every app.")
    };
    if songs.is_empty() {
        let empty = if period == 0 { "Nothing has been played yet." } else { "Nothing has been played in this period." };
        return PageData { title, empty: empty.into(), about: note, ..Default::default() };
    }

    // The same plays, added up by artist, by album and by kind of music.
    let tally = |key: &dyn Fn(&Song) -> &str| -> Vec<(String, u64)> {
        let mut sums: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
        for (song, times) in &songs {
            let key = key(song);
            if !key.is_empty() {
                *sums.entry(key).or_default() += times;
            }
        }
        let mut sums: Vec<(String, u64)> = sums.into_iter().map(|(key, times)| (key.to_string(), times)).collect();
        sums.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        sums
    };
    let artists = tally(&|song| song.artist_id.as_str());
    let albums = tally(&|song| song.album_id.as_str());
    let genres = tally(&|song| song.genre.as_str());
    let seconds: u64 = songs.iter().map(|(song, times)| song.duration as u64 * times).sum();
    let total: u64 = songs.iter().map(|(_, times)| times).sum();

    let listened = if seconds >= 3600 { (grouped(seconds / 3600), "hours listened") } else { (grouped(seconds / 60), "minutes listened") };
    let figures = vec![
        (listened.0, listened.1.to_string()),
        (grouped(total), if total == 1 { "play" } else { "plays" }.to_string()),
        (grouped(songs.len() as u64), if songs.len() == 1 { "song" } else { "songs" }.to_string()),
        (grouped(artists.len() as u64), if artists.len() == 1 { "artist" } else { "artists" }.to_string()),
    ];
    let cards = |sums: &[(String, u64)], known: &[CardData], limit: usize| -> Vec<CardData> {
        sums.iter()
            .filter_map(|(id, times)| known.iter().find(|card| &card.id == id).map(|card| (card, times)))
            .take(limit)
            .map(|(card, times)| CardData {
                // An album keeps its artist beside the count; an artist has only the count.
                subtitle: if card.kind == Kind::Album && !card.subtitle.is_empty() { format!("{} • {}", plays(*times), card.subtitle) } else { plays(*times) },
                ..card.clone()
            })
            .collect()
    };
    let top_artists = cards(&artists, &index.library.artists, 10);
    let top_albums = cards(&albums, &index.library.albums, 10);
    let kinds: Vec<CardData> =
        genres.iter().take(10).map(|(name, times)| CardData { kind: Kind::Genre, id: name.clone(), title: name.clone(), subtitle: plays(*times), cover: String::new() }).collect();
    let shelf = |title: &str, cards: Vec<CardData>| (!cards.is_empty()).then(|| ShelfData { title: title.into(), route: String::new(), wrap: false, cards });
    PageData {
        title,
        stats: figures,
        about: note,
        lead: shelf("Top artists", top_artists).into_iter().collect(),
        songs: songs.iter().take(25).map(|(song, _)| (*song).clone()).collect(),
        tracks_title: "Top songs".into(),
        show_album: true,
        show_cover: true,
        show_number: true,
        shelves: [shelf("Top albums", top_albums), shelf("Most played kinds of music", kinds)].into_iter().flatten().collect(),
        context: "Your top songs".into(),
        ..Default::default()
    }
}

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
        has_menu: true,
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
    // Artists that aren't here but might be wanted, if there's a way to ask for them.
    let discover = crate::lidarr::current().and(crate::search::current()).and_then(|index| discover_shelf(&crate::discover::shown(&index)));
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
        discover,
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

const DISCOVER: &str = "Artists you might like";

/// The row of artists that aren't in the library but are like the ones played most.
fn discover_shelf(artists: &[crate::discover::Suggestion]) -> Option<ShelfData> {
    if artists.is_empty() {
        return None;
    }
    // Their pages are opened by Lidarr's name for them, and want their name and picture.
    let offers: Vec<crate::lidarr::ArtistOffer> = artists
        .iter()
        .map(|artist| crate::lidarr::ArtistOffer { id: artist.id.clone(), name: artist.name.clone(), cover: artist.cover.clone(), ..Default::default() })
        .collect();
    crate::lidarr::remember_artists(&offers);
    Some(ShelfData {
        title: DISCOVER.into(),
        route: String::new(),
        wrap: false,
        cards: artists
            .iter()
            .map(|artist| CardData { kind: Kind::OutsideArtist, id: artist.id.clone(), title: artist.name.clone(), subtitle: format!("Like {}", artist.like), cover: artist.cover.clone() })
            .collect(),
    })
}

/// Looks for artists worth suggesting, when that's due. It takes a while, so Home is
/// on screen long before; the row joins it when it's ready.
fn home_extras(client: &Client, page: &PageData) -> Option<PageData> {
    let lidarr = crate::lidarr::current()?;
    let index = crate::search::current()?;
    let now = chrono::Utc::now().timestamp();
    if crate::discover::fresh(now) || index.library.songs.is_empty() {
        return None;
    }
    crate::discover::find(client, &index, &lidarr, now);
    let shelf = discover_shelf(&crate::discover::shown(&index))?;
    let mut page = page.clone();
    page.shelves.retain(|shelf| shelf.title != DISCOVER);
    // After what's been played lately, as on the page when it's loaded with them known.
    let place = page.shelves.iter().position(|shelf| shelf.title == "Recently played").map_or(page.shelves.len().min(1), |at| at + 1);
    page.shelves.insert(place, shelf);
    Some(page)
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

/// How many songs are suggested for a playlist.
const SUGGESTED: usize = 10;

/// Songs from the library that would suit a playlist: ones the server reckons similar to a
/// few of the songs in it, picked afresh each time, so asking again gives different ones.
/// If the server has nothing to go on, songs of the kind the playlist is mostly made of.
fn playlist_extras(client: &Client, page: &PageData) -> Option<PageData> {
    page.playlist.as_ref()?;
    if page.songs.is_empty() {
        return None;
    }
    let listed: std::collections::HashSet<&str> = page.songs.iter().map(|song| song.id.as_str()).collect();
    // The same song from another album is no suggestion either.
    let named: std::collections::HashSet<(String, String)> = page.songs.iter().map(|song| (plain(&song.title), plain(&song.artist))).collect();
    let mut seeds: Vec<&Song> = page.songs.iter().filter(|song| song.radio_url.is_empty()).collect();
    fastrand::shuffle(&mut seeds);
    seeds.truncate(3);
    let similar: Vec<Vec<Song>> = std::thread::scope(|scope| {
        let asked: Vec<_> = seeds.iter().map(|seed| scope.spawn(|| client.similar_songs(&seed.id, 30).unwrap_or_default())).collect();
        asked.into_iter().map(|asked| asked.join().unwrap_or_default()).collect()
    });
    let mut picks: Vec<Song> = Vec::new();
    let take = |song: &Song, picks: &mut Vec<Song>| {
        let known = listed.contains(song.id.as_str()) || named.contains(&(plain(&song.title), plain(&song.artist))) || picks.iter().any(|pick| pick.id == song.id);
        // No more than two by one artist, so one seed's artist doesn't fill the list.
        let by = picks.iter().filter(|pick| pick.artist_id == song.artist_id).count();
        if !known && by < 2 && picks.len() < SUGGESTED {
            picks.push(song.clone());
        }
    };
    // A song from each seed's list in turn, the most alike first.
    for place in 0..30 {
        for list in &similar {
            if let Some(song) = list.get(place) {
                take(song, &mut picks);
            }
        }
    }
    let mut note = "Based on what's in this playlist.";
    if picks.len() < SUGGESTED {
        // The kind of music most of it is.
        let mut kinds: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for song in page.songs.iter().filter(|song| !song.genre.is_empty()) {
            *kinds.entry(song.genre.as_str()).or_default() += 1;
        }
        if let Some((genre, _)) = kinds.into_iter().max_by_key(|(genre, count)| (*count, std::cmp::Reverse(*genre))) {
            if picks.is_empty() {
                note = "More of the kind of music this playlist is mostly made of.";
            }
            for song in client.random_songs(40, Some(genre)).unwrap_or_default() {
                take(&song, &mut picks);
            }
        }
    }
    if picks.is_empty() {
        return None;
    }
    let mut page = page.clone();
    page.offers_heading = "Recommended".into();
    page.offers_note = note.into();
    page.offers = picks
        .iter()
        .map(|song| OfferData {
            album: song.album_id.clone(),
            album_title: song.album.clone(),
            title: song.title.clone(),
            artist: song.artist_label().to_string(),
            artist_id: song.artist_id.clone(),
            artist_name: song.artist.clone(),
            about: song.album.clone(),
            cover: song.cover_art.clone(),
            seconds: song.duration,
            requested: false,
            kind: String::new(),
            song: song.id.clone(),
        })
        .collect();
    Some(page)
}

/// Every song in the library, the newest arrivals first. Made from the copy of the library
/// kept on this PC, so it's there at once and without the server.
fn all_songs() -> PageData {
    let mut songs = crate::search::current().map(|index| index.library.songs.clone()).unwrap_or_default();
    // The server's dates are text that sorts as time does; the title settles ties (a whole
    // album arrives at once).
    songs.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.album.cmp(&b.album)).then_with(|| a.track.cmp(&b.track)));
    for song in &mut songs {
        // Whether it's liked may have changed since the copy of the library was made.
        song.starred = crate::search::is_liked(song).then(|| song.starred.clone().unwrap_or_default());
    }
    PageData {
        label: "Library".into(),
        title: "Songs".into(),
        meta: format!("{}, newest first", count(songs.len() as u32, "song")),
        can_play: !songs.is_empty(),
        show_album: true,
        show_cover: true,
        show_number: true,
        local: true,
        empty: "Your library is still being fetched. It'll be here in a moment.".into(),
        context: "Songs".into(),
        songs,
        ..Default::default()
    }
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

/// The songs by an artist that the library has, most played in the world first.
fn library_top(name: &str, mbid: &str) -> Vec<Song> {
    let Some(index) = crate::search::current() else { return Vec::new() };
    let Ok((_, releases)) = crate::musicbrainz::artist_releases(mbid, crate::lidarr::singles()) else { return Vec::new() };
    let top = top_of(mbid, &releases);
    let theirs: Vec<&Song> = index.library.songs.iter().filter(|song| plain(song.artist_label()) == plain(name)).collect();
    let mut songs: Vec<Song> = Vec::new();
    for wanted in &top {
        let title = plain(crate::musicbrainz::base(&wanted.title));
        if songs.iter().any(|song| plain(crate::musicbrainz::base(&song.title)) == title) {
            continue;
        }
        if let Some(song) = theirs.iter().find(|song| plain(crate::musicbrainz::base(&song.title)) == title) {
            songs.push((*song).clone());
            if songs.len() == 5 {
                break;
            }
        }
    }
    songs
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
    // The server can only say what's popular if it has somewhere to ask. With requests
    // switched on there's ListenBrainz, whose list is matched to the songs that are here.
    let top = if top.is_empty() && asking && !info.music_brainz_id.is_empty() { library_top(&name, &info.music_brainz_id) } else { top };
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
