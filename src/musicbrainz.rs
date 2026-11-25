//! Looking music up in MusicBrainz, the open database that Lidarr is built on.
//!
//! The questions, and the rules for what to make of the answers, are in the library
//! (`nexus_core::musicbrainz`), for the Android app to use too. This is the Windows app's
//! end: the connection they're asked over, and the settings they're asked with. Nothing
//! here is used unless requests are switched on.

use std::sync::{Arc, OnceLock};

use nexus_core::musicbrainz::{MusicBrainz, THEIR_IDLE};
pub use nexus_core::musicbrainz::{SongOffer, TrackName, base, by_listens, cover, popular, same_words, words};

use crate::lidarr::Offer;

fn service() -> &'static MusicBrainz {
    static SERVICE: OnceLock<MusicBrainz> = OnceLock::new();
    SERVICE.get_or_init(|| {
        let agent = crate::subsonic::make_agent_idle(THEIR_IDLE);
        MusicBrainz::new(Arc::new(crate::corehttp::Agent(agent)), crate::listenbrainz::service().clone())
    })
}

/// Opens a connection ahead of a search, so the search doesn't wait for one. Called while
/// something is being typed into the search box; does nothing if a connection is likely open.
pub fn warm() {
    if service().needs_warming() {
        std::thread::spawn(|| service().warm());
    }
}

/// Looks for songs matching what was typed, each with the studio album it's on.
pub fn find_songs(term: &str) -> Result<Vec<SongOffer>, String> {
    service().find_songs(term, crate::lidarr::singles())
}

pub fn song_by(artist: &str, title: &str) -> Result<Vec<SongOffer>, String> {
    service().song_by(artist, title)
}

pub fn release_with(artist: &str, title: &str, album: &str, singles: bool) -> Result<Option<String>, String> {
    service().release_with(artist, title, album, singles)
}

/// An artist's studio albums, newest first, and what the artist is called.
pub fn artist_albums(artist: &str) -> Result<(String, Vec<Offer>), String> {
    // Asked for as their page asks, so the one answer serves both.
    service().artist_albums(artist, crate::lidarr::singles())
}

pub fn artist_releases(artist: &str, singles: bool) -> Result<(String, Vec<Offer>), String> {
    service().artist_releases(artist, singles)
}

pub fn played_most(artist: &str, releases: &[Offer]) -> Result<Vec<crate::listenbrainz::TopSong>, String> {
    service().played_most(artist, releases)
}

/// The songs on an album.
pub fn album_tracks(album: &str) -> Result<Vec<TrackName>, String> {
    service().album_tracks(album)
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
