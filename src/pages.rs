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
}

impl Kind {
    pub fn from_ui(kind: ui::CardKind) -> Kind {
        match kind {
            ui::CardKind::Album => Kind::Album,
            ui::CardKind::Artist => Kind::Artist,
            ui::CardKind::Playlist => Kind::Playlist,
            ui::CardKind::Genre => Kind::Genre,
            ui::CardKind::Mix => Kind::Mix,
        }
    }

    fn to_ui(self) -> ui::CardKind {
        match self {
            Kind::Album => ui::CardKind::Album,
            Kind::Artist => ui::CardKind::Artist,
            Kind::Playlist => ui::CardKind::Playlist,
            Kind::Genre => ui::CardKind::Genre,
            Kind::Mix => ui::CardKind::Mix,
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
        Kind::Album => ("Album", false),
        Kind::Artist => ("Artist", true),
        Kind::Playlist => ("Playlist", false),
        Kind::Genre | Kind::Mix => return None,
    };
    Some(PageData {
        label: label.into(),
        title: title.into(),
        subtitle: subtitle.into(),
        cover: cover.into(),
        cover_round: round,
        show_album: kind != Kind::Album,
        show_cover: kind != Kind::Album,
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
pub fn enrich(client: &Client, route: &Route, page: &PageData) -> Option<PageData> {
    match route {
        Route::Album(id) => album_extras(client, id, page),
        Route::Artist(id) => artist_extras(client, id, page),
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
            let mut found = index.search(query, &crate::search::is_liked, 8, 12, 30);
            for song in &mut found.songs {
                // Whether it's liked may have changed since the copy was made.
                song.starred = crate::search::is_liked(song).then(|| song.starred.clone().unwrap_or_default());
            }
            found
        }
        None => {
            let found = client.search(query, 8, 12, 20)?;
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
    // An artist who matches what was typed goes above the songs.
    let (lead, shelves): (Vec<ShelfData>, Vec<ShelfData>) = if found.artist_first {
        (artists.into_iter().collect(), albums.into_iter().collect())
    } else {
        (Vec::new(), artists.into_iter().chain(albums).collect())
    };
    let empty = if found.songs.is_empty() && shelves.is_empty() && lead.is_empty() {
        format!("Nothing in your library matches “{query}”. Check the spelling or try fewer words.")
    } else {
        String::new()
    };
    Ok(PageData {
        tracks_title: "Songs".into(),
        songs: found.songs,
        show_album: true,
        show_cover: true,
        show_number: false,
        shelves,
        lead,
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
fn artist_extras(client: &Client, id: &str, page: &PageData) -> Option<PageData> {
    let name = page.title.clone();
    let (info, top) = std::thread::scope(|scope| {
        let info = scope.spawn(|| client.artist_info(id).unwrap_or_default());
        let top = scope.spawn(|| client.top_songs(&name, 5).unwrap_or_default());
        (info.join().unwrap_or_default(), top.join().unwrap_or_default())
    });
    let similar: Vec<CardData> =
        info.similar.iter().filter(|a| !a.id.is_empty()).map(CardData::artist).collect();
    let about = plain_text(&info.biography);
    if top.is_empty() && similar.is_empty() && about.is_empty() {
        return None;
    }
    let mut page = page.clone();
    page.songs = top;
    page.about = about;
    page.shelves.retain(|s| s.title != "Fans also like");
    if !similar.is_empty() {
        page.shelves.push(ShelfData {
            title: "Fans also like".into(),
            route: String::new(),
            wrap: false,
            cards: similar,
        });
    }
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
