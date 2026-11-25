//! How much a song is played, from ListenBrainz.
//!
//! MusicBrainz knows what exists and not what people listen to, so by itself it can't say
//! which of an artist's songs are the well-known ones, or which of forty songs called "Home"
//! is the one most likely meant. ListenBrainz, its sister project, counts plays of the same
//! recordings under the same names. It's free, and what's asked of it here needs no account.
//! It's told which artists, releases or recordings are being asked about, nothing else.
//! Nothing here is used unless requests are switched on.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::http::{Failure, Http, Request, Response, STOPPED};

const BASE: &str = "https://api.listenbrainz.org/1";
/// Long enough for a slow answer, short enough that a page isn't held up by it for long.
const PATIENCE: Duration = Duration::from_secs(5);

/// ListenBrainz, asked through the app's own way of making requests.
#[derive(Clone)]
pub struct ListenBrainz {
    http: Arc<dyn Http>,
}

/// One of an artist's most played songs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TopSong {
    pub title: String,
    /// The release it's best known from, by title.
    pub album_title: String,
    pub seconds: u32,
    pub listens: u64,
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn unreachable(failure: Failure) -> String {
    match failure {
        Failure::Reach(e) => format!("Couldn't reach ListenBrainz. {e}"),
        Failure::Stopped => STOPPED.to_string(),
    }
}

fn read(response: Response) -> Result<Value, String> {
    let status = response.status;
    if let Some(e) = response.cut_short {
        return Err(format!("Couldn't read ListenBrainz's answer. {e}"));
    }
    let text = response.body;
    if status != 200 {
        return Err(format!("ListenBrainz answered with an error ({status})."));
    }
    serde_json::from_str(&text).map_err(|_| "ListenBrainz's answer made no sense.".to_string())
}

impl ListenBrainz {
    pub fn new(http: Arc<dyn Http>) -> ListenBrainz {
        ListenBrainz { http }
    }

    /// Sends a question as JSON, set out as the app has always sent it.
    fn post(&self, url: &str, body: Value) -> Result<Response, String> {
        let body = serde_json::to_string_pretty(&body).unwrap_or_default();
        let request = Request { method: "POST", url, headers: &[("Content-Type", "application/json; charset=utf-8")], body: Some(&body), patience: Some(PATIENCE) };
        self.http.send(&request).map_err(unreachable)
    }

    /// An artist's songs, most played first. `artist` is MusicBrainz's name for them.
    ///
    /// ListenBrainz only answers this without an account for the artists it's asked about
    /// most; for the rest it's worked out from the questions it does answer (see
    /// `musicbrainz::played_most`).
    pub fn top_songs(&self, artist: &str) -> Result<Vec<TopSong>, String> {
        let url = format!("{BASE}/popularity/top-recordings-for-artist/{artist}");
        let response = self.http.send(&Request { patience: Some(PATIENCE), ..Request::get(&url) }).map_err(unreachable)?;
        Ok(top_from(&read(response)?))
    }

    /// How many times each of these recordings has been played, by MusicBrainz's names for
    /// them. Ones nobody has played are left out.
    pub fn listens(&self, recordings: &[String]) -> Result<HashMap<String, u64>, String> {
        if recordings.is_empty() {
            return Ok(HashMap::new());
        }
        let response = self.post(&format!("{BASE}/popularity/recording"), serde_json::json!({ "recording_mbids": recordings }))?;
        Ok(listens_from(&read(response)?, "recording_mbid"))
    }

    /// How many times each of these artists has been played, by MusicBrainz's names for them.
    pub fn artist_listens(&self, artists: &[String]) -> Result<HashMap<String, u64>, String> {
        if artists.is_empty() {
            return Ok(HashMap::new());
        }
        let response = self.post(&format!("{BASE}/popularity/artist"), serde_json::json!({ "artist_mbids": artists }))?;
        Ok(listens_from(&read(response)?, "artist_mbid"))
    }

    /// How many times each of these albums, EPs and singles has been played, by
    /// MusicBrainz's names for them.
    pub fn release_listens(&self, releases: &[String]) -> Result<HashMap<String, u64>, String> {
        if releases.is_empty() {
            return Ok(HashMap::new());
        }
        let response = self.post(&format!("{BASE}/popularity/release-group"), serde_json::json!({ "release_group_mbids": releases }))?;
        Ok(listens_from(&read(response)?, "release_group_mbid"))
    }
}

fn top_from(found: &Value) -> Vec<TopSong> {
    found
        .as_array()
        .map(|songs| {
            songs
                .iter()
                .map(|song| TopSong {
                    title: text(song, "recording_name").to_string(),
                    album_title: text(song, "release_name").to_string(),
                    seconds: (song.get("length").and_then(Value::as_u64).unwrap_or(0) / 1000) as u32,
                    listens: song.get("total_listen_count").and_then(Value::as_u64).unwrap_or(0),
                })
                .filter(|song| !song.title.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn listens_from(found: &Value, named: &str) -> HashMap<String, u64> {
    found
        .as_array()
        .map(|counts| {
            counts
                .iter()
                .filter_map(|count| Some((text(count, named).to_string(), count.get("total_listen_count").and_then(Value::as_u64)?)))
                .filter(|(recording, listens)| !recording.is_empty() && *listens > 0)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_listenbrainz_says() {
        let top = serde_json::json!([
            { "recording_name": "Yellow", "release_name": "Parachutes", "length": 269110, "total_listen_count": 3030199 },
            { "recording_name": "", "release_name": "Nothing", "total_listen_count": 5 },
            { "recording_name": "Trouble", "release_name": "Parachutes", "length": null, "total_listen_count": null }
        ]);
        let songs = top_from(&top);
        assert_eq!(songs.len(), 2);
        assert_eq!((songs[0].title.as_str(), songs[0].album_title.as_str(), songs[0].seconds, songs[0].listens), ("Yellow", "Parachutes", 269, 3030199));
        assert_eq!((songs[1].seconds, songs[1].listens), (0, 0));
        assert!(top_from(&serde_json::json!({ "error": "no" })).is_empty());

        let counts = serde_json::json!([
            { "recording_mbid": "a", "total_listen_count": 40 },
            { "recording_mbid": "b", "total_listen_count": null },
            { "recording_mbid": "c", "total_listen_count": 0 }
        ]);
        assert_eq!(listens_from(&counts, "recording_mbid"), HashMap::from([("a".to_string(), 40)]));
    }
}
