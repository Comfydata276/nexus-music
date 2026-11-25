//! An album that could be asked for, as Lidarr and MusicBrainz both describe one.

use serde::Deserialize;

/// An album Lidarr knows of that could be asked for.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, Deserialize)]
#[serde(default)]
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
    /// "EP" or "Single" for one of those; empty (or "Album") for an album.
    pub kind: String,
    /// It's filed as a soundtrack: music made for a film, a show or a game.
    pub soundtrack: bool,
}

impl Offer {
    /// What kind of release it is: "Album", "EP" or "Single".
    pub fn kind(&self) -> &str {
        if self.kind.is_empty() { "Album" } else { &self.kind }
    }

    /// The kind as it's said in a sentence: "album", "EP" or "single".
    pub fn kind_word(&self) -> &'static str {
        match self.kind() {
            "EP" => "EP",
            "Single" => "single",
            _ => "album",
        }
    }
}

/// MusicBrainz's name for "Various Artists", which most film soundtracks are credited to.
pub const VARIOUS: &str = "89ad4ac3-39f7-470e-963a-56509c546377";
