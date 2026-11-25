//! Asking for music that isn't in the library, as each page and each action does it: what
//! is asked of Lidarr, MusicBrainz and ListenBrainz in what order, and what is made of the
//! answers. The questions themselves and the reading of each answer are in `lidarr`,
//! `musicbrainz` and `listenbrainz`; how what's found is drawn is each app's own.
//!
//! What these need of the app comes in as arguments: the services (`Services`), its copy of
//! the library (`Library`), what has been offered so far (`lidarr::Offers`), what has been
//! asked for, the "singles and EPs" setting, the time. What they find comes back as plain
//! data, with the sentences to show.
//!
//! Any of them may be stopped by the app (its `Http` answers `Failure::Stopped`): then it
//! comes back `Err(Stopped)`, never as though nothing were found, and hands nothing on to be
//! shown after the stop.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::asked::Asked;
use crate::http::STOPPED;
use crate::lidarr::{ArtistOffer, Lidarr, Offers, Pending};
use crate::listenbrainz::{ListenBrainz, TopSong};
use crate::musicbrainz::{MusicBrainz, SongOffer, TrackName};
use crate::offer::Offer;
use crate::text::{base, fold, same_artist, same_words, words};

/// The services a page or an action asks. Each app has one MusicBrainz (see
/// `MusicBrainz::over`) and makes the others as it likes.
#[derive(Clone, Copy)]
pub struct Services<'a> {
    pub lidarr: &'a Lidarr,
    pub musicbrainz: &'a MusicBrainz,
    pub listenbrainz: &'a ListenBrainz,
}

/// The app stopped what was being done (see `http::Failure::Stopped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stopped;

/// A service's answer, unless the app stopped it: then the whole operation stops.
pub(crate) fn unless_stopped<T>(answer: Result<T, String>) -> Result<Result<T, String>, Stopped> {
    match answer {
        Err(reason) if reason == STOPPED => Err(Stopped),
        answer => Ok(answer),
    }
}

/// Text as it's compared: folded, with runs of spaces closed up.
pub fn plain(text: &str) -> String {
    fold(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---- the library ------------------------------------------------------------------------------------

/// One of the library's albums, as the library has it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AlbumRef<'a> {
    /// The music server's id for it.
    pub id: &'a str,
    pub title: &'a str,
    /// Its artist, as the line beneath its title.
    pub artist: &'a str,
    pub cover: &'a str,
}

/// The same, kept.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryAlbum {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub cover: String,
}

impl From<AlbumRef<'_>> for LibraryAlbum {
    fn from(album: AlbumRef<'_>) -> LibraryAlbum {
        LibraryAlbum { id: album.id.into(), title: album.title.into(), artist: album.artist.into(), cover: album.cover.into() }
    }
}

/// One of the library's songs: its title and who it's credited to, as the app shows that.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SongRef<'a> {
    pub title: &'a str,
    pub artist: &'a str,
}

/// The app's copy of the library, as these ask it. An app answers the three lists; the
/// questions below are answered from them here, so that every app compares names the same
/// way. They're asked once per page or action, as each was before it moved here.
pub trait Library: Sync {
    fn albums(&self) -> Box<dyn Iterator<Item = AlbumRef<'_>> + '_>;
    fn songs(&self) -> Box<dyn Iterator<Item = SongRef<'_>> + '_>;
    /// The names of the artists it has.
    fn artists(&self) -> Box<dyn Iterator<Item = &str> + '_>;

    /// Every album it has, by title and artist, each `plain`: what a search leaves out.
    fn album_names(&self) -> HashSet<(String, String)> {
        self.albums().map(|album| (plain(album.title), plain(album.artist))).collect()
    }

    /// Every song it has, by title and artist, each `plain`: what a search leaves out.
    fn song_names(&self) -> HashSet<(String, String)> {
        self.songs().map(|song| (plain(song.title), plain(song.artist))).collect()
    }

    /// The titles of the songs it has by this artist (credited with their words in it),
    /// each `plain` and without what's tacked on: an artist's most played that it has.
    fn songs_by(&self, artist: &str) -> HashSet<String> {
        let name = words(artist).join(" ");
        self.songs().filter(|song| words(song.artist).join(" ").contains(&name)).map(|song| plain(base(song.title))).collect()
    }

    /// The titles of its albums by this artist, each `plain`.
    fn album_titles_by(&self, artist: &str) -> HashSet<String> {
        let artist = plain(artist);
        self.albums().filter(|album| plain(album.artist) == artist).map(|album| plain(album.title)).collect()
    }

    /// Its albums by this artist, by their titles `plain`: shown on the artist's page as the
    /// library's own. Of two of the same name, the later.
    fn albums_by(&self, artist: &str) -> HashMap<String, LibraryAlbum> {
        let artist = plain(artist);
        self.albums().filter(|album| plain(album.artist) == artist).map(|album| (plain(album.title), album.into())).collect()
    }

    /// The album an arrival is, if it has turned up: the same words in its title, by the
    /// same artist. Its id and cover.
    fn same_album(&self, title: &str, artist: &str) -> Option<(String, String)> {
        let album = self.albums().find(|album| same_words(album.title, title) && same_artist(album.artist, artist))?;
        Some((album.id.to_string(), album.cover.to_string()))
    }

    /// The names of the artists it has, each folded: who isn't news.
    fn artist_names(&self) -> HashSet<String> {
        self.artists().map(fold).collect()
    }
}

/// A copy of the library as plain lists: what an app hands across once whenever its own copy
/// changes, kept, and asked as often as it likes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub albums: Vec<LibraryAlbum>,
    /// Each song's title and who it's credited to.
    pub songs: Vec<(String, String)>,
    pub artists: Vec<String>,
}

impl Library for Snapshot {
    fn albums(&self) -> Box<dyn Iterator<Item = AlbumRef<'_>> + '_> {
        Box::new(self.albums.iter().map(|album| AlbumRef { id: &album.id, title: &album.title, artist: &album.artist, cover: &album.cover }))
    }

    fn songs(&self) -> Box<dyn Iterator<Item = SongRef<'_>> + '_> {
        Box::new(self.songs.iter().map(|(title, artist)| SongRef { title, artist }))
    }

    fn artists(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        Box::new(self.artists.iter().map(String::as_str))
    }
}

// ---- an artist's most played songs ----------------------------------------------------------------------

/// How many of an artist's most played songs are listed.
const POPULAR: usize = 8;

/// What an artist is most played for. ListenBrainz says outright for the best known; for
/// the rest it's worked out from their releases, which takes a couple of seconds longer.
pub fn top_of(musicbrainz: &MusicBrainz, listenbrainz: &ListenBrainz, artist: &str, releases: &[Offer]) -> Result<Vec<TopSong>, Stopped> {
    match unless_stopped(listenbrainz.top_songs(artist))? {
        Ok(top) if !top.is_empty() => Ok(top),
        _ => Ok(unless_stopped(musicbrainz.played_most(artist, releases))?.unwrap_or_default()),
    }
}

/// An artist's most played songs that the library doesn't have, each on the release of
/// theirs it can be asked for with. `top` is ListenBrainz's list of what they're played
/// for and `releases` everything they've put out. Also says
/// whether any were left out for being in the library already.
pub fn popular_songs(top: &[TopSong], library: &dyn Library, artist: &str, name: &str, releases: &[Offer]) -> (Vec<SongOffer>, bool) {
    let have: HashSet<String> = library.songs_by(name);
    // A song on an album that's here is here too, whatever the file calls it.
    let have_albums: HashSet<String> = library.album_titles_by(name);
    let all = crate::musicbrainz::popular(top, releases, artist, name);
    let mut left_out = false;
    let mut songs: Vec<SongOffer> = Vec::new();
    for (place, song) in all.into_iter().enumerate() {
        if have.contains(&plain(base(&song.title))) || have_albums.contains(&plain(&song.album_title)) {
            // Only what would have been listed counts as left out.
            left_out |= place < POPULAR;
        } else if songs.len() < POPULAR {
            songs.push(song);
        }
    }
    (songs, left_out)
}

// ---- the "Not in your library" search ------------------------------------------------------------------

/// How many plays make an artist one of some note, and how many put it beyond doubt that
/// their name, typed, means them and not a song called the same.
const NOTED: u64 = 10_000;
const UNMISTAKABLE: u64 = 5_000_000;

/// What a search has found beyond the library so far.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Outside {
    /// What to say above it ("Looking for songs beyond your library…", "Nothing else was
    /// found beyond your library.", …).
    pub note: String,
    /// Songs that could be asked for, each with whether its album has been asked for.
    pub songs: Vec<(SongOffer, bool)>,
    pub artists: Vec<ArtistOffer>,
    pub albums: Vec<Offer>,
}

/// Songs, artists and albums matching a search that aren't in the library. Songs come from
/// MusicBrainz; artists and albums from Lidarr, which answers sooner. `publish` is handed
/// what's been found each time more of it is known, so what's found shows as it arrives;
/// all of it is returned.
///
/// `singles` is the "singles and EPs" setting, read when it's needed (it may change while a
/// service is waited for), and `asked` what has been asked for, read for each stage.
pub fn search(
    services: Services,
    library: &dyn Library,
    offers: &Offers,
    singles: &(dyn Fn() -> bool + Sync),
    asked: &dyn Fn() -> Vec<Asked>,
    query: &str,
    publish: &mut dyn FnMut(Outside),
) -> Result<Outside, Stopped> {
    const ASK: &str = "Ask Lidarr for any of these. A song comes with the album it's on, or as its single if it isn't on one.";
    let lidarr = services.lidarr;
    // What's been found so far, with whether each song's album has been asked for.
    let outside = |note: &str, songs: &[SongOffer], artists: &[ArtistOffer], albums: &[Offer]| -> Outside {
        // What this app has asked for and is still waiting on.
        let awaited: HashSet<String> = asked().into_iter().filter(|asked| asked.arrived == 0 && !asked.album.is_empty()).map(|asked| asked.album).collect();
        Outside {
            note: note.into(),
            songs: songs
                .iter()
                .map(|song| {
                    let requested = awaited.contains(&song.album)
                        || albums.iter().any(|album| album.album == song.album && album.requested)
                        || offers.offered(&song.album).is_some_and(|offer| offer.requested);
                    (song.clone(), requested)
                })
                .collect(),
            artists: artists.to_vec(),
            albums: albums.to_vec(),
        }
    };
    // Leave out what the library already has.
    let have_albums: HashSet<(String, String)> = library.album_names();
    let have_songs: HashSet<(String, String)> = library.song_names();
    let typed = plain(query);
    let words = typed.split(' ').count();

    std::thread::scope(|scope| {
        let releases = scope.spawn(|| unless_stopped(lidarr.find_releases(query)));
        // Artists named in what was typed. A name that's only a small part of it ("Lonely",
        // when a song's whole title was typed) is a coincidence and is left out.
        let (exact, named): (Vec<ArtistOffer>, Vec<ArtistOffer>) = unless_stopped(lidarr.find_artists(query))?
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
        let (albums, singles_found) = releases.join().unwrap_or(Ok(Ok(Default::default())))?.unwrap_or_default();
        let mut albums: Vec<Offer> = albums.into_iter().filter(|offer| !have_albums.contains(&(plain(&offer.title), plain(&offer.artist)))).take(24).collect();
        // What's been offered before is remembered as it was: asked for, and with its cover.
        for album in &mut albums {
            if let Some(known) = offers.offered(&album.album) {
                *album = known;
            }
        }
        offers.remember(&albums);
        // Of those named outright, the ones of some note (Lidarr has their picture) go first.
        let (noted, obscure): (Vec<_>, Vec<_>) = exact.iter().cloned().partition(|artist| !artist.cover.is_empty());
        let noted_count = noted.len();
        let mut artists: Vec<ArtistOffer> = noted.into_iter().chain(obscure).chain(named.iter().cloned()).collect();
        offers.remember_artists(&artists);

        // A well-known song typed by its title alone. Lidarr's list of singles says whose it
        // is: the one of that name that people have rated.
        let famous = singles_found
            .iter()
            .filter(|single| single.votes > 0 && !single.artist_id.is_empty() && same_words(&single.title, query))
            .max_by_key(|single| single.votes);
        let own = |songs: Vec<SongOffer>| -> Vec<SongOffer> { songs.into_iter().filter(|song| !have_songs.contains(&(plain(&song.title), plain(&song.artist)))).collect() };
        // The songs matching what was typed. Of those that answer it equally well, the most
        // played go first.
        let look = || -> Result<Vec<SongOffer>, Stopped> {
            let mut found = own(unless_stopped(services.musicbrainz.find_songs(query, singles))?.unwrap_or_default());
            let recordings: Vec<String> = found.iter().flat_map(|song| song.recordings.iter().cloned()).collect();
            if let Ok(counts) = unless_stopped(services.listenbrainz.listens(&recordings))? {
                crate::musicbrainz::by_listens(&mut found, &counts);
            }
            Ok(found)
        };
        // An artist's name was typed: what's wanted is what they've made, not songs that
        // happen to have the name in their title. But there's an obscure band named after
        // most famous songs, so of those by that name it's the one played most, and one of
        // some note: Lidarr has their picture, or they've been played a fair bit.
        let plays = if famous.is_some() || exact.is_empty() {
            Default::default()
        } else {
            unless_stopped(services.listenbrainz.artist_listens(&exact.iter().map(|artist| artist.id.clone()).collect::<Vec<_>>()))?.unwrap_or_default()
        };
        let played = |artist: &ArtistOffer| plays.get(&artist.id).copied().unwrap_or(0);
        let mut meant = if famous.is_some() {
            None
        } else {
            // The first of the most played; with nothing known of plays, the first pictured.
            exact.iter().rev().filter(|artist| !artist.cover.is_empty() || played(artist) >= NOTED).max_by_key(|artist| played(artist))
        };
        // A name that's as likely a song's ("Believer", "Home"): unless the artist is one of
        // the most played there are, the songs of that name are looked up too, and if one is
        // played more than everything the artist has made, the song is what's meant.
        let mut looked: Option<Vec<SongOffer>> = None;
        if let Some(artist) = meant.filter(|artist| played(artist) < UNMISTAKABLE) {
            publish(outside("Looking for songs and albums…", &[], &artists, &albums));
            let found = look()?;
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
            publish(outside("Looking for their songs and albums…", &[], &artists, &albums));
            let mut songs = Vec::new();
            if let Ok((_, releases)) = unless_stopped(services.musicbrainz.artist_releases(&artist.id, singles()))? {
                let theirs: Vec<Offer> = releases
                    .iter()
                    .filter(|offer| offer.kind() == "Album" && !have_albums.contains(&(plain(&offer.title), plain(&artist.name))))
                    .map(|offer| offers.offered(&offer.album).unwrap_or_else(|| offer.clone()))
                    .collect();
                offers.remember(&theirs);
                albums.retain(|album| !theirs.iter().any(|their| their.album == album.album));
                albums.splice(0..0, theirs);
                albums.truncate(24);
                publish(outside("Looking for their most played songs…", &[], &artists, &albums));
                songs = popular_songs(&top_of(services.musicbrainz, services.listenbrainz, &artist.id, &releases)?, library, &artist.id, &artist.name, &releases).0;
            }
            let note = if songs.is_empty() {
                "Ask Lidarr for any of these albums, or open the artist for everything they've made."
            } else {
                "Their most played songs come first. Ask Lidarr for any of these, or open the artist for everything they've made."
            };
            return Ok(outside(note, &songs, &artists, &albums));
        }
        publish(outside("Looking for songs beyond your library…", &[], &artists, &albums));

        // For a well-known song, MusicBrainz would hand back a hundred versions in no order,
        // so the one that's meant is asked for by name and goes first.
        let mut songs: Vec<SongOffer> = Vec::new();
        if let Some(single) = famous {
            songs = own(unless_stopped(services.musicbrainz.song_by(&single.artist_id, &single.title))?.unwrap_or_default());
            songs.truncate(1);
            if !songs.is_empty() {
                publish(outside("Looking for more songs…", &songs, &artists, &albums));
            }
        }
        let found = match looked {
            Some(found) => found,
            None => look()?,
        };
        for song in found {
            let listed = songs.iter().any(|have| same_words(&have.title, &song.title) && same_words(&have.artist, &song.artist));
            if !listed && songs.len() < 20 {
                songs.push(song);
            }
        }
        // Whoever the songs found are by joins the artists: typing a song's name should lead
        // to its artist too. They go after a noted artist who was named outright, before the
        // rest (an obscure band that shares the song's name, say).
        let mut by: Vec<ArtistOffer> = Vec::new();
        for song in songs.iter().take(3) {
            let listed = artists.iter().chain(&by).any(|artist| artist.id == song.artist_id);
            if !song.artist_id.is_empty() && !listed {
                by.push(offers.offered_artist(&song.artist_id).unwrap_or_else(|| ArtistOffer { id: song.artist_id.clone(), name: song.artist_name.clone(), ..Default::default() }));
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
                let sketch = Offer {
                    kind: String::new(),
                    album: song.album.clone(),
                    artist_id: song.artist_id.clone(),
                    title: song.album_title.clone(),
                    artist: song.artist_name.clone(),
                    year: song.year.clone(),
                    cover: crate::musicbrainz::cover(&song.album),
                    requested: false,
                    soundtrack: false,
                };
                albums.insert(front, offers.offered(&song.album).unwrap_or(sketch));
                front += 1;
            }
        }
        albums.truncate(12);
        let nothing = songs.is_empty() && artists.is_empty() && albums.is_empty();
        albums.truncate(24);
        let note = if nothing { "Nothing else was found beyond your library." } else { ASK };
        let found = outside(note, &songs, &artists, &albums);

        // Last, pictures for the artists that came from songs. Lidarr has those.
        let bare: Vec<&ArtistOffer> = artists.iter().filter(|artist| artist.cover.is_empty() && artist.about.is_empty()).collect();
        if bare.is_empty() {
            return Ok(found);
        }
        publish(found);
        let asked: Vec<_> = bare
            .iter()
            .map(|artist| {
                let id = artist.id.clone();
                scope.spawn(move || unless_stopped(lidarr.find_artists(&format!("lidarr:{id}"))).map(|found| found.ok().and_then(|found| found.into_iter().find(|artist| artist.id == id))))
            })
            .collect();
        let mut pictured: Vec<ArtistOffer> = Vec::new();
        for asked in asked {
            pictured.extend(asked.join().unwrap_or(Ok(None))?);
        }
        offers.remember_artists(&pictured);
        for artist in &mut artists {
            if let Some(better) = pictured.iter().find(|better| better.id == artist.id) {
                *artist = better.clone();
            }
        }
        Ok(outside(note, &songs, &artists, &albums))
    })
}

// ---- an artist beyond the library ------------------------------------------------------------------

/// One of an artist's releases, as their page shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Release {
    pub offer: Offer,
    /// The line under it: the year, what kind of release it is if it isn't an album, and how
    /// it stands ("2019 • EP • Requested", "2016 • In your library").
    pub caption: String,
    /// The library's own copy, if it has it: then it's shown, and opened, as that.
    pub library: Option<LibraryAlbum>,
    /// Lidarr has it.
    pub arrived: bool,
}

/// An artist as the wider world knows them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutsideArtist {
    pub name: String,
    pub cover: String,
    /// What's said of them under the name ("3 albums • 1 in your library").
    pub about: String,
    pub albums: Vec<Release>,
    /// Singles and EPs, and soundtracks other than albums.
    pub smaller: Vec<Release>,
    /// How many albums could be asked for: none means there's nothing for "Add artist" to do.
    pub open: usize,
    /// What to say instead, when there's nothing to show, or why it couldn't be had.
    pub empty: String,
    /// It couldn't be had: `empty` says why, and `name` is what they were called when offered.
    pub failed: bool,
}

/// An artist as the wider world knows them: every studio album they've made, with the ones
/// in the library opening as usual and the rest there to be asked for. `id` is
/// MusicBrainz's name for them; `singles` the "singles and EPs" setting, read once Lidarr has
/// said whether it keeps track of them (it may be switched while Lidarr is waited for).
pub fn artist(services: Services, library: &dyn Library, offers: &Offers, singles: &(dyn Fn() -> bool + Sync), id: &str) -> Result<OutsideArtist, Stopped> {
    let lidarr = services.lidarr;
    let remembered = offers.offered_artist(id);
    // If Lidarr keeps track of the artist, its list says what's wanted and what's arrived.
    let known = unless_stopped(lidarr.known_artist(id))?.ok().flatten();
    let singles = singles();
    let found = match &known {
        Some((number, name)) => match unless_stopped(lidarr.releases_of(*number, name))? {
            Ok(mut releases) => {
                // Lidarr only lists the kinds of release the artist's profile covers, which may
                // not include singles and EPs: those come from MusicBrainz.
                if singles {
                    for release in unless_stopped(services.musicbrainz.artist_releases(id, true))?.map(|(_, releases)| releases).unwrap_or_default() {
                        if release.kind() != "Album" && !releases.iter().any(|(known, _)| known.album == release.album) {
                            releases.push((release, false));
                        }
                    }
                }
                Ok((name.clone(), releases))
            }
            Err(reason) => Err(reason),
        },
        None => unless_stopped(services.musicbrainz.artist_releases(id, singles))?.map(|(name, releases)| (name, releases.into_iter().map(|release| (release, false)).collect())),
    };
    let (name, albums): (String, Vec<(Offer, bool)>) = match found {
        Ok(found) => found,
        Err(reason) => return Ok(OutsideArtist { name: remembered.map(|a| a.name).unwrap_or_default(), empty: reason, failed: true, ..Default::default() }),
    };
    let name = if name.is_empty() { remembered.as_ref().map(|artist| artist.name.clone()).unwrap_or_default() } else { name };
    let offered: Vec<Offer> = albums.iter().map(|(offer, _)| offer.clone()).collect();
    offers.remember(&offered);
    // Albums the library has are shown as its own, so they open and play.
    let library: HashMap<String, LibraryAlbum> = library.albums_by(&name);
    let mut owned = 0;
    let mut open = 0;
    let mut card = |offer: &Offer, have: bool, counted: bool| -> Release {
        // The year, what kind of thing it is if it isn't an album, and how it stands.
        let caption = |state: &str| {
            let kind = if offer.kind() == "Album" { "" } else { offer.kind() };
            [offer.year.as_str(), kind, state].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(" • ")
        };
        if let Some(mine) = library.get(&plain(&offer.title)) {
            owned += counted as usize;
            return Release { offer: offer.clone(), caption: caption("In your library"), library: Some(mine.clone()), arrived: have };
        }
        if counted && !have && !offer.requested {
            open += 1;
        }
        Release { offer: offer.clone(), caption: caption(if have { "Arrived" } else if offer.requested { "Requested" } else { "" }), library: None, arrived: have }
    };
    let cards: Vec<Release> = albums.iter().filter(|(offer, _)| offer.kind() == "Album").map(|(offer, have)| card(offer, *have, true)).collect();
    let smaller: Vec<Release> = albums.iter().filter(|(offer, _)| offer.kind() != "Album").map(|(offer, have)| card(offer, *have, false)).collect();
    let about = match (cards.len(), owned) {
        (0, _) => String::new(),
        (all, 0) => count(all, "album"),
        (all, owned) => format!("{} • {owned} in your library", count(all, "album")),
    };
    Ok(OutsideArtist {
        empty: if cards.is_empty() && smaller.is_empty() { "Nothing is listed for this artist.".into() } else { String::new() },
        name,
        cover: remembered.map(|artist| artist.cover).unwrap_or_default(),
        about,
        albums: cards,
        smaller,
        open,
        failed: false,
    })
}

/// The songs an artist is most played for, for the top of their page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Popular {
    /// "Popular", or "Popular, and not in your library" when some were left out for being
    /// in the library already.
    pub heading: String,
    /// Each with whether its album has been asked for.
    pub songs: Vec<(SongOffer, bool)>,
}

/// The songs an artist is most played for that the library doesn't have, for the top of
/// their page; `None` if there are none. Found after the page itself, since it takes a few
/// more questions. `name` is what the page calls them; `singles` and `asked` are read when
/// they're needed.
pub fn artist_popular(
    services: Services,
    library: &dyn Library,
    offers: &Offers,
    singles: &(dyn Fn() -> bool + Sync),
    asked: &dyn Fn() -> Vec<Asked>,
    id: &str,
    name: &str,
) -> Result<Option<Popular>, Stopped> {
    let Ok((_, releases)) = unless_stopped(services.musicbrainz.artist_releases(id, singles()))? else { return Ok(None) };
    let (popular, left_out) = popular_songs(&top_of(services.musicbrainz, services.listenbrainz, id, &releases)?, library, id, name, &releases);
    if popular.is_empty() {
        return Ok(None);
    }
    let awaited: HashSet<String> = asked().into_iter().filter(|asked| asked.arrived == 0 && !asked.album.is_empty()).map(|asked| asked.album).collect();
    Ok(Some(Popular {
        heading: if left_out { "Popular, and not in your library".into() } else { "Popular".into() },
        songs: popular
            .into_iter()
            .map(|song| {
                let requested = awaited.contains(&song.album) || offers.offered(&song.album).is_some_and(|offer| offer.requested);
                (song, requested)
            })
            .collect(),
    }))
}

// ---- an album beyond the library -------------------------------------------------------------------

/// Said of an album Lidarr has no record of.
pub const NOT_LISTED: &str = "Lidarr doesn't list that one, so it can't be asked for.";

/// An album that isn't in the library, as its page shows it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutsideAlbum {
    pub offer: Offer,
    pub cover: String,
    pub tracks: Vec<TrackName>,
    /// Why the songs couldn't be listed, if they couldn't.
    pub tracks_failed: String,
    /// What's said of it under its name ("2019 • 12 songs • Not in your library").
    pub about: String,
    /// "Request album" (or EP, single), or "Requested".
    pub action: String,
}

/// What an album's page comes to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlbumPage {
    Found(OutsideAlbum),
    /// It can't be shown, and this says why (`NOT_LISTED`, or why Lidarr couldn't be asked).
    Unavailable(String),
}

/// An album that isn't in the library: what's on it, and whether it can be asked for. `id`
/// is Lidarr's (and MusicBrainz's) name for it.
pub fn album(services: Services, offers: &Offers, id: &str) -> Result<AlbumPage, Stopped> {
    let offer = match offers.offered(id) {
        Some(offer) => Ok(Some(offer)),
        None => unless_stopped(services.lidarr.find_any(&format!("lidarr:{id}")))?.map(|found| found.into_iter().find(|offer| offer.album == id)),
    };
    let offer = match offer {
        Ok(Some(offer)) => offer,
        Ok(None) => return Ok(AlbumPage::Unavailable(NOT_LISTED.into())),
        Err(reason) => return Ok(AlbumPage::Unavailable(reason)),
    };
    offers.remember(std::slice::from_ref(&offer));
    let tracks = unless_stopped(services.musicbrainz.album_tracks(id))?;
    let (songs, empty) = match tracks {
        Ok(tracks) => (tracks, String::new()),
        Err(reason) => (Vec::new(), format!("The list of songs couldn't be fetched. {reason}")),
    };
    let mut meta: Vec<String> = Vec::new();
    if !offer.year.is_empty() {
        meta.push(offer.year.clone());
    }
    if !songs.is_empty() {
        meta.push(count(songs.len(), "song"));
    }
    meta.push(if offer.requested { "Requested".into() } else { "Not in your library".into() });
    Ok(AlbumPage::Found(OutsideAlbum {
        about: meta.join(" • "),
        cover: if offer.cover.is_empty() { crate::musicbrainz::cover(id) } else { offer.cover.clone() },
        action: if offer.requested { "Requested".into() } else { format!("Request {}", offer.kind_word()) },
        tracks: songs,
        tracks_failed: empty,
        offer,
    }))
}

// ---- asking for an album, a single, an EP, a soundtrack, or a song's album ------------------------------

/// Whether to ask, and how the question is put.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Asking {
    /// It has been asked for already: this says so. (Each app adds where to see it.)
    Already(String),
    Ask { heading: String, text: String },
}

/// Asks whether to request an album that isn't in the library: Lidarr's name for it, its
/// title, whose it is (may be empty), the song that led here if it was one (the album is
/// what gets fetched, and that's said), and what kind of release it is if the caller knows.
pub fn asking(offers: &Offers, album: &str, title: &str, by: &str, song: &str, kind: &str) -> Asking {
    let offered = offers.offered(album);
    if offered.as_ref().is_some_and(|offer| offer.requested) {
        return Asking::Already("That one has already been asked for.".into());
    }
    // What kind of release it is, if the caller didn't say: Lidarr may have.
    let kind = if kind.is_empty() { offered.as_ref().map(|offer| offer.kind().to_string()).unwrap_or_default() } else { kind.to_string() };
    let whose = if by.is_empty() { "the artist".to_string() } else { by.to_string() };
    let by = if by.is_empty() { String::new() } else { format!(" by {by}") };
    let text = match (kind.as_str(), song.is_empty()) {
        ("Single", _) => format!(
            "“{title}”{by} came out as a single. It will go on Lidarr's list, and Lidarr will start looking for it. If Lidarr only lists {whose}'s albums so far, it will be set to list their singles and EPs too. None but this one is fetched, though ones they release from now on are treated as their new albums are."
        ),
        ("EP", true) => format!(
            "“{title}”{by} is an EP. It will go on Lidarr's list, and Lidarr will start looking for it. If Lidarr only lists {whose}'s albums so far, it will be set to list their singles and EPs too. None but this one is fetched, though ones they release from now on are treated as their new albums are."
        ),
        ("EP", false) => format!(
            "“{song}” is on the EP “{title}”{by}. That EP will go on Lidarr's list, and Lidarr will start looking for it. If Lidarr only lists {whose}'s albums so far, it will be set to list their singles and EPs too. None but this one is fetched, though ones they release from now on are treated as their new albums are."
        ),
        (_, true) => format!("“{title}”{by} will go on Lidarr's list, and Lidarr will start looking for it. The whole album is fetched."),
        (_, false) => format!("“{song}” is on “{title}”{by}. That album will go on Lidarr's list, and Lidarr will start looking for it. The whole album is fetched."),
    };
    // A soundtrack is another kind of release Lidarr has to be set to list.
    let soundtrack = offered.as_ref().is_some_and(|offer| offer.soundtrack);
    let text = if !soundtrack {
        text
    } else if offered.as_ref().is_some_and(|offer| offer.artist_id == crate::offer::VARIOUS) {
        format!("{text} It's a soundtrack credited to various artists, so it goes on Lidarr's list under “Various Artists”.")
    } else if kind == "Album" || kind.is_empty() {
        format!("{text} It's a soundtrack. If Lidarr doesn't list {whose}'s soundtracks so far, it will be set to. None but this one is fetched.")
    } else {
        text.replace("their singles and EPs too", "their singles, EPs and soundtracks too")
    };
    let heading = match kind.as_str() {
        "Single" => "Ask for this single?",
        "EP" => "Ask for this EP?",
        _ => "Ask for this album?",
    };
    Asking::Ask { heading: heading.into(), text }
}

/// What's said while an album is being asked for.
pub fn asking_for(title: &str) -> String {
    format!("Asking for “{title}”…")
}

/// Said of an album Lidarr had a record of and no longer has.
pub const NO_LONGER_LISTED: &str = "Lidarr no longer lists that one.";

/// Asks Lidarr for an album that's been offered: as remembered, or looked up afresh (from a
/// page drawn in an earlier session). Gives who it's by, or why it couldn't be asked for.
/// Once Lidarr has begun changing anything it runs to its end, or puts back what it changed
/// (see `Lidarr::request`); a stop before then gives `Err(Stopped)`.
pub fn request(lidarr: &Lidarr, offers: &Offers, album: &str) -> Result<Result<String, String>, Stopped> {
    // Offered in this session, or (from a page drawn in an earlier one) looked up afresh.
    let offer = match offers.offered(album) {
        Some(offer) => Ok(offer),
        None => match unless_stopped(lidarr.find_any(&format!("lidarr:{album}")))? {
            Ok(found) => found.into_iter().find(|o| o.album == album).ok_or_else(|| NO_LONGER_LISTED.to_string()),
            Err(reason) => Err(reason),
        },
    };
    let Ok(offer) = offer else { return Ok(offer.map(|offer| offer.artist)) };
    offers.remember(std::slice::from_ref(&offer));
    Ok(unless_stopped(lidarr.request(&offer))?.map(|()| offer.artist))
}

/// Once Lidarr has an album on its list: notes it as asked for, to be followed up when it
/// arrives (for the app to `asked::put` and keep), and marks it so among what's offered.
pub fn asked_for(offers: &Offers, album: &str, title: &str, artist: &str, song: &str, now: i64) -> Asked {
    offers.mark(album, true);
    Asked { album: album.to_string(), title: title.to_string(), artist: artist.to_string(), at: now, song: song.to_string(), ..Default::default() }
}

/// What's said once it has been asked for.
pub fn asked_sentence(title: &str) -> String {
    format!("Asked for “{title}”. It'll be in your library once it arrives.")
}

// ---- "Add artist" ------------------------------------------------------------------------------

/// The question before Lidarr is set to keep track of an artist: its heading and text.
/// `open` is how many of their albums could be asked for (see `OutsideArtist::open`).
pub fn adding(name: &str, open: usize) -> (String, String) {
    (
        format!("Add {name}?"),
        format!(
            "Lidarr will look for {} by {name} that you don't have, and for new ones as they come out. That can be a lot to fetch.",
            if open == 1 { "the 1 album".to_string() } else { format!("all {open} albums") }
        ),
    )
}

/// What's said while an artist is being added.
pub fn adding_now(name: &str) -> String {
    format!("Adding {name}…")
}

/// What's said once they have been. It ends before where to see them, which each app adds
/// (the Windows app: "; see Requests in the sidebar.").
pub fn added(name: &str) -> String {
    format!("Added {name}. Their albums are on Lidarr's list")
}

// ---- what is on its way --------------------------------------------------------------------------

/// What has been asked for: what has arrived lately, and what Lidarr has still to get.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Waiting {
    /// What this app asked for that has since turned up, newest first, each with the line
    /// under it ("Coldplay • for “Yellow”").
    pub arrived: Vec<(Asked, String)>,
    /// "On their way (3)"; empty when nothing is.
    pub heading: String,
    /// Each album Lidarr has still to get, with the line under it ("Coldplay • Downloading,
    /// 40%", "Coldplay • Being looked for").
    pub pending: Vec<(Pending, String)>,
    /// What to say when nothing is on its way, or why Lidarr couldn't say.
    pub empty: String,
    /// Lidarr couldn't say: `empty` is why.
    pub failed: bool,
}

/// The albums that have been asked for: those that have arrived lately, then those still on
/// their way. `asked` is the app's list of what it asked for.
pub fn on_their_way(lidarr: &Lidarr, asked: &[Asked]) -> Result<Waiting, Stopped> {
    // What this app asked for that has since turned up, newest first.
    let mut arrived: Vec<Asked> = asked.iter().filter(|asked| asked.arrived != 0 && !asked.library_album.is_empty()).cloned().collect();
    arrived.sort_by_key(|asked| std::cmp::Reverse(asked.arrived));
    let arrived: Vec<(Asked, String)> = arrived
        .into_iter()
        .map(|asked| {
            let line = if asked.song.is_empty() { asked.artist.clone() } else { format!("{} • for “{}”", asked.artist, asked.song) };
            (asked, line)
        })
        .collect();
    Ok(match unless_stopped(lidarr.pending())? {
        Err(reason) => Waiting { arrived, empty: reason, failed: true, ..Default::default() },
        Ok(pending) if pending.is_empty() => Waiting {
            empty: if arrived.is_empty() {
                "Nothing is on its way. Search for a song, artist or album and pick it under “Not in your library”.".into()
            } else {
                "Nothing else is on its way.".into()
            },
            arrived,
            ..Default::default()
        },
        Ok(pending) => Waiting {
            arrived,
            heading: format!("On their way ({})", pending.len()),
            pending: pending
                .into_iter()
                .map(|album| {
                    let line = match album.progress {
                        Some(done) => format!("{} • Downloading, {done}%", album.artist),
                        None => format!("{} • Being looked for", album.artist),
                    };
                    (album, line)
                })
                .collect(),
            ..Default::default()
        },
    })
}

/// What's said once an album has been taken off Lidarr's list (see `Lidarr::cancel`).
pub fn off_the_list(title: &str) -> String {
    format!("“{title}” is off the list")
}

// ---- an arrival ------------------------------------------------------------------------------------

/// What's said when albums that were asked for have turned up: `told` are the ones that are
/// news (see `asked::arrivals`). Empty when none is.
pub fn arrived_news(told: &[Asked]) -> String {
    match told {
        [] => String::new(),
        [one] if !one.song.is_empty() => format!("“{}” has arrived, on “{}”.", one.song, one.title),
        [one] => format!("“{}” by {} has arrived.", one.title, one.artist),
        many => format!("{} albums you asked for have arrived.", many.len()),
    }
}

/// "1 album", "3 albums": a number of things.
pub fn count(n: usize, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn snapshot() -> Snapshot {
        let album = |id: &str, title: &str, artist: &str| LibraryAlbum { id: id.into(), title: title.into(), artist: artist.into(), cover: format!("al-{id}") };
        Snapshot {
            albums: vec![album("a1", "…Something to Be", "Rob Thomas"), album("a2", "Parachutes", "Coldplay"), album("a3", "Parachutes ", "coldplay"), album("a4", "X&Y", "Coldplay")],
            songs: vec![("Yellow (Live)".into(), "Coldplay".into()), ("Lonely No More".into(), "Rob Thomas".into()), ("Fix You".into(), "Coldplay & Friends".into())],
            artists: vec!["Coldplay".into(), "Rob Thomas".into(), "Beyoncé".into()],
        }
    }

    #[test]
    fn answers_the_questions_from_its_lists() {
        let library = snapshot();
        assert!(library.album_names().contains(&(plain("Parachutes"), plain("COLDPLAY"))));
        assert!(library.song_names().contains(&("yellow live".into(), "coldplay".into())));
        assert_eq!(library.songs_by("Coldplay"), HashSet::from(["yellow".to_string(), "fix you".to_string()]));
        assert_eq!(library.album_titles_by("coldplay"), HashSet::from(["parachutes".to_string(), "x y".to_string()]));
        let by = library.albums_by("Coldplay");
        assert_eq!((by.len(), by["parachutes"].id.as_str()), (2, "a3"));
        assert_eq!(library.same_album("...Something To Be", "rob thomas"), Some(("a1".into(), "al-a1".into())));
        assert_eq!(library.same_album("Parachutes", "Someone Else"), None);
        assert_eq!(library.artist_names(), HashSet::from(["coldplay".to_string(), "rob thomas".to_string(), "beyonce".to_string()]));
    }

    /// A stand-in for every service: answers by part of the address, the first that fits,
    /// and from the `stop_after`th request on says "stopped", as an app does once what was
    /// being done is no longer wanted.
    pub(crate) struct Scripted {
        pub routes: Vec<(&'static str, String)>,
        pub stop_after: usize,
        pub asked: std::sync::Mutex<Vec<String>>,
    }

    impl crate::http::Http for Scripted {
        fn send(&self, request: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
            let mut asked = self.asked.lock().unwrap();
            asked.push(request.url.to_string());
            if asked.len() > self.stop_after {
                return Err(crate::http::Failure::Stopped);
            }
            let answer = self.routes.iter().find(|(part, _)| request.url.contains(part)).map(|(_, answer)| answer.clone()).unwrap_or_else(|| "[]".into());
            Ok(crate::http::Response::new(200, Vec::new(), move || Ok(answer)))
        }

        fn pause(&self, _: std::time::Duration) -> Result<(), crate::http::Failure> {
            if self.asked.lock().unwrap().len() >= self.stop_after { Err(crate::http::Failure::Stopped) } else { Ok(()) }
        }
    }

    /// Runs a search against the stand-in, giving what came of it and every stage published.
    pub(crate) fn searching(routes: Vec<(&'static str, String)>, stop_after: usize, query: &str) -> (Result<Outside, Stopped>, Vec<Outside>, Vec<String>) {
        let http = std::sync::Arc::new(Scripted { routes, stop_after, asked: Default::default() });
        let lidarr = Lidarr::new("http://lidarr.test", "key", None, http.clone());
        let listenbrainz = ListenBrainz::new(http.clone());
        let musicbrainz = MusicBrainz::new(http.clone(), listenbrainz.clone());
        let services = Services { lidarr: &lidarr, musicbrainz: &musicbrainz, listenbrainz: &listenbrainz };
        let mut published = Vec::new();
        let found = search(services, &snapshot(), &Offers::new(), &|| true, &Vec::new, query, &mut |stage| {
            // Nothing is to be published once a stop has been met.
            assert!(http.asked.lock().unwrap().len() <= http.stop_after, "published after the stop: {stage:?}");
            published.push(stage)
        });
        let asked = http.asked.lock().unwrap().clone();
        (found, published, asked)
    }

    fn quiet_band() -> Vec<(&'static str, String)> {
        vec![
            ("artist/lookup", r#"[{"artistName":"Quiet Band","foreignArtistId":"qb"}]"#.into()),
            ("album/lookup", r#"[{"title":"Soft","foreignAlbumId":"rg-s","albumType":"Album","artist":{"artistName":"Quiet Band","foreignArtistId":"qb"}}]"#.into()),
            ("recording?query", r#"{"recordings":[{"id":"r1","title":"Soft Song","score":100,"artist-credit":[{"name":"Quiet Band","artist":{"id":"qb","name":"Quiet Band"}}],"releases":[{"status":"Official","date":"2001","release-group":{"id":"rg-s","title":"Soft","primary-type":"Album","secondary-types":[]}}]}]}"#.into()),
        ]
    }

    #[test]
    fn a_search_that_runs_its_course_finds_what_there_is() {
        let (found, published, _) = searching(quiet_band(), usize::MAX, "quiet band");
        let found = found.unwrap();
        assert_eq!(found.note, "Ask Lidarr for any of these. A song comes with the album it's on, or as its single if it isn't on one.");
        assert_eq!((found.songs.len(), found.artists.len(), found.albums.len()), (1, 1, 1));
        // The artist found has no picture yet: what's found is shown while one is looked for.
        assert_eq!(published.iter().map(|stage| stage.note.as_str()).collect::<Vec<_>>(), ["Looking for songs beyond your library…", found.note.as_str()]);
    }

    #[test]
    fn a_stopped_search_says_so_and_shows_nothing_after() {
        // Stopped at every point it could be: before anything, between Lidarr's answers,
        // while MusicBrainz is asked, while ListenBrainz is. Never "nothing was found", and
        // nothing published once the stop has been met.
        let (_, _, all) = searching(quiet_band(), usize::MAX, "quiet band");
        assert!(all.len() >= 5, "{all:?}");
        for stop_after in 0..all.len() {
            let (found, _, _) = searching(quiet_band(), stop_after, "quiet band");
            assert_eq!(found, Err(Stopped), "stopped after {stop_after}");
        }
    }

    /// Asks `ask` of the stand-in, stopped from its `stop_after`th request on.
    fn served<T>(routes: &[(&'static str, &str)], stop_after: usize, ask: impl Fn(Services) -> T) -> (T, usize) {
        let routes = routes.iter().map(|(part, answer)| (*part, answer.to_string())).collect();
        let http = std::sync::Arc::new(Scripted { routes, stop_after, asked: Default::default() });
        let lidarr = Lidarr::new("http://lidarr.test", "key", None, http.clone());
        let listenbrainz = ListenBrainz::new(http.clone());
        let musicbrainz = MusicBrainz::new(http.clone(), listenbrainz.clone());
        let found = ask(Services { lidarr: &lidarr, musicbrainz: &musicbrainz, listenbrainz: &listenbrainz });
        let asked = http.asked.lock().unwrap().len();
        (found, asked)
    }

    const MARROW_LAKE: &[(&str, &str)] = &[
        ("api/v1/artist", "[]"),
        ("release-group?artist=ml", r#"{"release-group-count":2,"release-groups":[
            {"id":"rg-1","title":"Iron Kite","primary-type":"Album","secondary-types":[],"first-release-date":"2018-10-19","artist-credit":[{"name":"Marrow Lake","artist":{"id":"ml","name":"Marrow Lake"}}]},
            {"id":"rg-2","title":"Small Hours","primary-type":"EP","secondary-types":[],"first-release-date":"2017","artist-credit":[{"name":"Marrow Lake","artist":{"id":"ml","name":"Marrow Lake"}}]}]}"#),
        ("top-recordings-for-artist/ml", r#"[{"recording_name":"Kite","release_name":"Iron Kite","total_listen_count":9}]"#),
    ];

    #[test]
    fn an_artists_page_says_what_they_have_made() {
        let (found, _) = served(MARROW_LAKE, usize::MAX, |services| artist(services, &snapshot(), &Offers::new(), &|| true, "ml"));
        let found = found.unwrap();
        assert_eq!((found.name.as_str(), found.about.as_str(), found.open), ("Marrow Lake", "1 album", 1));
        assert_eq!(found.albums.iter().chain(&found.smaller).map(|release| release.caption.as_str()).collect::<Vec<_>>(), ["2018", "2017 • EP"]);
        let (popular, _) = served(MARROW_LAKE, usize::MAX, |services| artist_popular(services, &snapshot(), &Offers::new(), &|| true, &Vec::new, "ml", "Marrow Lake"));
        let popular = popular.unwrap().unwrap();
        assert_eq!((popular.heading.as_str(), popular.songs.len()), ("Popular", 1));
    }

    #[test]
    fn a_stopped_artists_page_says_so() {
        fn page(services: Services) -> Result<OutsideArtist, Stopped> {
            artist(services, &snapshot(), &Offers::new(), &|| true, "ml")
        }
        fn popular(services: Services) -> Result<Option<Popular>, Stopped> {
            artist_popular(services, &snapshot(), &Offers::new(), &|| true, &Vec::new, "ml", "Marrow Lake")
        }
        let (_, all) = served(MARROW_LAKE, usize::MAX, page);
        for stop_after in 0..all {
            assert_eq!(served(MARROW_LAKE, stop_after, page).0, Err(Stopped), "stopped after {stop_after} of {all}");
        }
        let (_, all) = served(MARROW_LAKE, usize::MAX, popular);
        assert!(all >= 2);
        for stop_after in 0..all {
            assert_eq!(served(MARROW_LAKE, stop_after, popular).0, Err(Stopped), "popular, stopped after {stop_after} of {all}");
        }
    }

    #[test]
    fn an_albums_page_and_when_it_is_stopped() {
        fn page(services: Services) -> Result<AlbumPage, Stopped> {
            album(services, &Offers::new(), "rg-9")
        }
        const ROUTES: &[(&str, &str)] = &[
            ("album/lookup?term=lidarr%3Arg-9", r#"[{"title":"Nine","foreignAlbumId":"rg-9","albumType":"EP","releaseDate":"2019-01-01","artist":{"artistName":"Someone","foreignArtistId":"so"}}]"#),
            ("release?release-group=rg-9", r#"{"releases":[{"media":[{"position":1,"tracks":[{"title":"One","length":100000,"position":1}]}]}]}"#),
        ];
        let (found, all) = served(ROUTES, usize::MAX, page);
        let Ok(AlbumPage::Found(found)) = found else { panic!("{found:?}") };
        assert_eq!((found.about.as_str(), found.action.as_str(), found.tracks.len()), ("2019 • 1 song • Not in your library", "Request EP", 1));
        assert_eq!(all, 2);
        for stop_after in 0..all {
            assert_eq!(served(ROUTES, stop_after, page).0, Err(Stopped), "stopped after {stop_after}");
        }
        assert_eq!(served(&[], usize::MAX, page).0, Ok(AlbumPage::Unavailable(NOT_LISTED.into())));
    }

    #[test]
    fn asks_as_it_always_has() {
        // Every question, as the Windows app put it before the words moved here.
        let offers = Offers::new();
        let offer = |album: &str, kind: &str, soundtrack: bool, artist_id: &str| Offer { album: album.into(), kind: kind.into(), soundtrack, artist_id: artist_id.into(), ..Default::default() };
        offers.remember(&[
            offer("asked", "", false, "a"),
            offer("ost", "", true, "a"),
            offer("ost-various", "", true, crate::offer::VARIOUS),
            offer("ost-ep", "EP", true, "a"),
            offer("lidarr-ep", "EP", false, "a"),
        ]);
        offers.mark("asked", true);
        let ask = |album: &str, by: &str, song: &str, kind: &str| match asking(&offers, album, "Title", by, song, kind) {
            Asking::Ask { heading, text } => format!("{heading} | {text}"),
            Asking::Already(said) => said,
        };
        let more = "If Lidarr only lists Them's albums so far, it will be set to list their singles and EPs too. None but this one is fetched, though ones they release from now on are treated as their new albums are.";
        assert_eq!(ask("asked", "Them", "", ""), "That one has already been asked for.");
        assert_eq!(ask("x", "Them", "", "Single"), format!("Ask for this single? | “Title” by Them came out as a single. It will go on Lidarr's list, and Lidarr will start looking for it. {more}"));
        assert_eq!(ask("x", "Them", "A Song", "Single"), format!("Ask for this single? | “Title” by Them came out as a single. It will go on Lidarr's list, and Lidarr will start looking for it. {more}"));
        assert_eq!(ask("x", "Them", "", "EP"), format!("Ask for this EP? | “Title” by Them is an EP. It will go on Lidarr's list, and Lidarr will start looking for it. {more}"));
        assert_eq!(ask("x", "Them", "A Song", "EP"), format!("Ask for this EP? | “A Song” is on the EP “Title” by Them. That EP will go on Lidarr's list, and Lidarr will start looking for it. {more}"));
        assert_eq!(ask("x", "Them", "", ""), "Ask for this album? | “Title” by Them will go on Lidarr's list, and Lidarr will start looking for it. The whole album is fetched.");
        assert_eq!(ask("x", "", "A Song", "Album"), "Ask for this album? | “A Song” is on “Title”. That album will go on Lidarr's list, and Lidarr will start looking for it. The whole album is fetched.");
        // What kind it is, when the caller doesn't say: what Lidarr said.
        assert_eq!(ask("lidarr-ep", "", "", ""), "Ask for this EP? | “Title” is an EP. It will go on Lidarr's list, and Lidarr will start looking for it. If Lidarr only lists the artist's albums so far, it will be set to list their singles and EPs too. None but this one is fetched, though ones they release from now on are treated as their new albums are.");
        // Soundtracks.
        assert_eq!(ask("ost", "Them", "", ""), "Ask for this album? | “Title” by Them will go on Lidarr's list, and Lidarr will start looking for it. The whole album is fetched. It's a soundtrack. If Lidarr doesn't list Them's soundtracks so far, it will be set to. None but this one is fetched.");
        assert_eq!(ask("ost-various", "Various Artists", "", ""), "Ask for this album? | “Title” by Various Artists will go on Lidarr's list, and Lidarr will start looking for it. The whole album is fetched. It's a soundtrack credited to various artists, so it goes on Lidarr's list under “Various Artists”.");
        assert_eq!(ask("ost-ep", "Them", "", ""), "Ask for this EP? | “Title” by Them is an EP. It will go on Lidarr's list, and Lidarr will start looking for it. If Lidarr only lists Them's albums so far, it will be set to list their singles, EPs and soundtracks too. None but this one is fetched, though ones they release from now on are treated as their new albums are.");
        assert_eq!(asking_for("Title"), "Asking for “Title”…");
        assert_eq!(asked_sentence("Title"), "Asked for “Title”. It'll be in your library once it arrives.");
        let note = asked_for(&offers, "lidarr-ep", "Title", "Them", "A Song", 50);
        assert_eq!((note.album.as_str(), note.title.as_str(), note.artist.as_str(), note.song.as_str(), note.at, note.arrived), ("lidarr-ep", "Title", "Them", "A Song", 50, 0));
        assert!(offers.offered("lidarr-ep").unwrap().requested);
    }

    #[test]
    fn asks_lidarr_for_what_was_offered_or_looks_it_up() {
        fn ask(services: Services) -> Result<Result<String, String>, Stopped> {
            request(services.lidarr, &Offers::new(), "rg-9")
        }
        // Not offered, and Lidarr no longer has it.
        assert_eq!(served(&[("album/lookup", "[]")], usize::MAX, ask).0, Ok(Err(NO_LONGER_LISTED.to_string())));
        // Stopped while it's looked up.
        assert_eq!(served(&[("album/lookup", "[]")], 0, ask).0, Err(Stopped));
    }

    #[test]
    fn adds_an_artist_in_the_words_it_always_has() {
        assert_eq!(adding("Marrow Lake", 1), ("Add Marrow Lake?".to_string(), "Lidarr will look for the 1 album by Marrow Lake that you don't have, and for new ones as they come out. That can be a lot to fetch.".to_string()));
        assert_eq!(adding("Marrow Lake", 3).1, "Lidarr will look for all 3 albums by Marrow Lake that you don't have, and for new ones as they come out. That can be a lot to fetch.");
        assert_eq!(adding_now("Marrow Lake"), "Adding Marrow Lake…");
        assert_eq!(format!("{}; see Requests in the sidebar.", added("Marrow Lake")), "Added Marrow Lake. Their albums are on Lidarr's list; see Requests in the sidebar.");
    }

    #[test]
    fn a_stopped_look_at_what_is_on_its_way_says_so() {
        fn look(services: Services) -> Result<Waiting, Stopped> {
            on_their_way(services.lidarr, &[])
        }
        let none = r#"{"page":1,"pageSize":500,"totalRecords":0,"records":[]}"#;
        let (found, all) = served(&[("wanted/missing", none), ("queue", none)], usize::MAX, look);
        assert_eq!(found.unwrap().empty, "Nothing is on its way. Search for a song, artist or album and pick it under “Not in your library”.");
        for stop_after in 0..all {
            assert_eq!(served(&[("wanted/missing", none), ("queue", none)], stop_after, look).0, Err(Stopped));
        }
        assert_eq!(off_the_list("Title"), "“Title” is off the list");
    }

    #[test]
    fn says_what_has_arrived() {
        let one = Asked { title: "Parachutes".into(), artist: "Coldplay".into(), ..Default::default() };
        assert_eq!(arrived_news(&[]), "");
        assert_eq!(arrived_news(std::slice::from_ref(&one)), "“Parachutes” by Coldplay has arrived.");
        assert_eq!(arrived_news(&[Asked { song: "Yellow".into(), ..one.clone() }]), "“Yellow” has arrived, on “Parachutes”.");
        assert_eq!(arrived_news(&[one.clone(), one]), "2 albums you asked for have arrived.");
    }
}
