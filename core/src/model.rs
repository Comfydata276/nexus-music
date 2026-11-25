//! What a Subsonic server's answers are read into: a song, an album, a playlist and the rest.
//!
//! How an answer is read is part of how the app behaves: what a missing field is taken for,
//! a number sent where text was expected. Every rule in this library that is about a song or
//! a playlist is written against these, so that each app gets the same answer from it.
//! Asking the server is each app's own.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReplayGain {
    pub track_gain: Option<f32>,
    pub album_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_peak: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub album: String,
    pub artist: String,
    pub album_id: String,
    pub artist_id: String,
    pub display_artist: String,
    /// Everyone credited on the song, when the server says (it may name only the first
    /// otherwise, in `artist`).
    pub artists: Vec<Credit>,
    pub track: u32,
    pub disc_number: u32,
    pub year: u32,
    pub cover_art: String,
    pub duration: u32,
    pub suffix: String,
    /// Size of the file on the server, in bytes.
    pub size: u64,
    pub bit_rate: u32,
    pub sampling_rate: u32,
    pub bit_depth: u32,
    pub genre: String,
    pub starred: Option<String>,
    /// When the song was added to the library.
    pub created: String,
    /// How many times it's been played, and when last.
    pub play_count: u32,
    pub played: String,
    /// The user's star rating, 1 to 5; 0 when unrated.
    pub user_rating: u8,
    pub replay_gain: Option<ReplayGain>,
    /// Set (by this app, not the server) on entries that stand for an internet radio station.
    pub radio_url: String,
    /// Set (by this app) on a queue entry that was put there by hand and hasn't played yet:
    /// it's in "Next in queue", ahead of the rest of whatever list is playing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub queued: bool,
}

/// An artist named on a song.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Credit {
    pub id: String,
    pub name: String,
}

impl Song {
    pub fn artist_label(&self) -> &str {
        if self.display_artist.is_empty() { &self.artist } else { &self.display_artist }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub artist_id: String,
    pub display_artist: String,
    pub cover_art: String,
    pub song_count: u32,
    pub duration: u32,
    pub year: u32,
    pub genre: String,
    pub starred: Option<String>,
    /// When it was last played.
    pub played: String,
    pub release_types: Vec<String>,
    #[serde(rename = "song")]
    pub songs: Vec<Song>,
}

impl Album {
    pub fn artist_label(&self) -> &str {
        if self.display_artist.is_empty() { &self.artist } else { &self.display_artist }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub cover_art: String,
    pub album_count: u32,
    pub starred: Option<String>,
    #[serde(rename = "album")]
    pub albums: Vec<Album>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ArtistInfo {
    pub biography: String,
    /// MusicBrainz's name for the artist, where the server knows it.
    pub music_brainz_id: String,
    #[serde(rename = "similarArtist")]
    pub similar: Vec<Artist>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub comment: String,
    pub owner: String,
    pub song_count: u32,
    pub duration: u32,
    pub cover_art: String,
    #[serde(rename = "entry")]
    pub songs: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanStatus {
    pub scanning: bool,
    pub count: u64,
    pub last_scan: String,
}

/// A play queue saved on the server.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlayQueue {
    #[serde(rename = "entry")]
    pub songs: Vec<Song>,
    /// The id of the song that was playing. (Some servers send a number.)
    #[serde(deserialize_with = "text_or_number")]
    pub current: String,
    /// How far into that song, in milliseconds.
    pub position: u64,
    /// When it was saved, and by which app.
    pub changed: String,
    pub changed_by: String,
}

fn text_or_number<'de, D: serde::Deserializer<'de>>(from: D) -> Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(from)? {
        serde_json::Value::String(text) => text,
        serde_json::Value::Number(number) => number.to_string(),
        _ => String::new(),
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Genre {
    #[serde(rename = "value")]
    pub name: String,
    #[serde(rename = "albumCount")]
    pub album_count: u32,
    #[serde(rename = "songCount")]
    pub song_count: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Found {
    #[serde(rename = "artist")]
    pub artists: Vec<Artist>,
    #[serde(rename = "album")]
    pub albums: Vec<Album>,
    #[serde(rename = "song")]
    pub songs: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct LyricLine {
    /// Milliseconds from the start of the track; absent for unsynced lyrics.
    pub start: Option<i64>,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Lyrics {
    pub synced: bool,
    pub line: Vec<LyricLine>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub version: String,
}

/// Which list `getAlbumList2` should return.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub enum AlbumList {
    Newest,
    Recent,
    Frequent,
    Random,
    Starred,
    AlphabeticalByName,
    AlphabeticalByArtist,
    ByYearDescending,
    ByGenre(String),
}

/// Lyric lines with the file's own offset taken off their times, so everything after this
/// can take the times as they are.
pub fn shifted(mut lines: Vec<LyricLine>, offset: i64) -> Vec<LyricLine> {
    if offset != 0 {
        for line in &mut lines {
            line.start = line.start.map(|at| (at - offset).max(0));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_a_lyrics_file_s_offset_off_its_times() {
        let line = |start: Option<i64>| LyricLine { start, value: "la".into() };
        // 500 ms sooner; a line can't start before the song does.
        let sooner = shifted(vec![line(Some(200)), line(Some(1500)), line(None)], 500);
        assert_eq!(sooner.iter().map(|line| line.start).collect::<Vec<_>>(), [Some(0), Some(1000), None]);
        let later = shifted(vec![line(Some(200))], -300);
        assert_eq!(later[0].start, Some(500));
    }
}
