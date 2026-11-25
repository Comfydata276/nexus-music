//! Changing a playlist that something else may have changed too.
//!
//! The page shows a copy of a playlist as it was when the page loaded. Another device, or
//! another app, can add or remove songs after that. So a change made on the page is never
//! sent as "song number 7" or as the page's whole list: it's described by the songs
//! themselves, and carried out on the playlist as the server has it at that moment.

// How a change is described, and what it makes of a playlist as it is now, are the library's.
pub use nexus_core::playlists::{Edit, apply, hides_songs, moving, positions, restoring, spot};

use crate::subsonic::{ApiResult, Client};

/// What the person is told when a change to a playlist can't be made by position.
pub const HIDES_SONGS: &str = "Some songs in this playlist are missing from your library, so it can't be changed from here.";

/// How a change to a playlist went on the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Carried out (or there was nothing to do). `elsewhere` is whether the playlist had
    /// changed since the page took its copy; `before` is the playlist as the change was
    /// worked out from, and `after` what the change made of it.
    Done { elsewhere: bool, before: Vec<String>, after: Vec<String> },
    /// Nothing was sent: the playlist holds songs the server doesn't show, so a position or
    /// a whole new list would be taken to mean something other than what the page meant.
    HidesSongs,
}

/// Carries out an edit on the playlist as the server has it now. `expected` is what the page
/// took it to hold before the change. Removing goes by positions and moving or putting back
/// replaces the whole list, so neither is sent for a playlist that hides songs; that is
/// decided on the same read the change is worked out from.
pub fn carry_out(client: &Client, id: &str, expected: &[String], edit: &Edit) -> ApiResult<Outcome> {
    let playlist = client.playlist(id)?;
    if hides_songs(&playlist, &client.playlists()?) {
        return Ok(Outcome::HidesSongs);
    }
    let now: Vec<String> = playlist.songs.into_iter().map(|song| song.id).collect();
    // What the change makes of the playlist as it is now: worked out, not read back
    // afterwards (a later read can show another device's change as well).
    let next = apply(&now, edit);
    match edit {
        Edit::Remove(spots) => {
            let places = positions(&now, spots);
            if !places.is_empty() {
                client.remove_many_from_playlist(id, &places)?;
            }
        }
        _ => {
            if next != now {
                client.replace_playlist(id, &next)?;
            }
        }
    }
    Ok(Outcome::Done { elsewhere: now != expected, before: now, after: next })
}

/// A playlist as a spreadsheet file: one line per song, with the column names Spotify
/// exports use, so it can be read back by this app's import or by other tools.
pub fn csv(songs: &[crate::subsonic::Song], added: &[i64]) -> String {
    let cell = |text: &str| format!("\"{}\"", text.replace('"', "\"\""));
    let mut out = String::from("Track Name,Artist Name(s),Album Name,Duration (ms),Added At\r\n");
    for (index, song) in songs.iter().enumerate() {
        let when = added
            .get(index)
            .filter(|at| **at > 0)
            .and_then(|at| chrono::DateTime::from_timestamp(*at, 0))
            .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_default();
        out.push_str(&format!(
            "{},{},{},{},{}\r\n",
            cell(&song.title),
            cell(&song.artist_label()),
            cell(&song.album),
            song.duration as u64 * 1000,
            when
        ));
    }
    out
}

/// A name that's safe to give a file.
pub fn file_name(name: &str) -> String {
    let safe: String = name.chars().map(|c| if c.is_control() || "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    let safe = safe.trim().trim_end_matches('.').to_string();
    if safe.is_empty() { "Playlist".into() } else { safe }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    /// A pretend Navidrome holding one playlist, "p1": its songs as stored, each with whether
    /// it's missing from the library, and the changes it was sent.
    struct Stored {
        songs: Vec<(String, bool)>,
        writes: Vec<String>,
    }

    fn navidrome(songs: &[(&str, bool)]) -> (Client, std::sync::Arc<std::sync::Mutex<Stored>>) {
        use crate::subsonic::fake;
        let stored = std::sync::Arc::new(std::sync::Mutex::new(Stored {
            songs: songs.iter().map(|(id, missing)| (id.to_string(), *missing)).collect(),
            writes: Vec::new(),
        }));
        let held = stored.clone();
        let address = fake::server(move |target, _, stream| {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let pairs: Vec<(&str, &str)> = query.split('&').filter_map(|pair| pair.split_once('=')).collect();
            let all = |name: &str| pairs.iter().filter(|(key, _)| *key == name).map(|(_, value)| value.to_string()).collect::<Vec<_>>();
            let mut stored = held.lock().unwrap();
            let entries = |stored: &Stored| {
                let shown: Vec<String> =
                    stored.songs.iter().filter(|(_, missing)| !missing).map(|(id, _)| format!(r#"{{"id":"{id}","title":"Song {id}"}}"#)).collect();
                format!(r#""songCount":{},"entry":[{}]"#, shown.len(), shown.join(","))
            };
            match path {
                "/rest/getPlaylist" => fake::ok(stream, &format!(r#""playlist":{{"id":"p1","name":"Mine",{}}}"#, entries(&stored))),
                "/rest/getPlaylists" => {
                    fake::ok(stream, &format!(r#""playlists":{{"playlist":[{{"id":"p1","name":"Mine","songCount":{}}}]}}"#, stored.songs.len()))
                }
                "/rest/updatePlaylist" | "/rest/createPlaylist" => {
                    stored.writes.push(path.trim_start_matches("/rest/").to_string());
                    // Positions count every stored song, the missing ones too.
                    let mut gone: Vec<usize> = all("songIndexToRemove").iter().map(|at| at.parse().unwrap()).collect();
                    gone.sort_unstable();
                    for at in gone.into_iter().rev() {
                        stored.songs.remove(at);
                    }
                    stored.songs.extend(all("songIdToAdd").into_iter().map(|id| (id, false)));
                    if path == "/rest/createPlaylist" {
                        stored.songs = all("songId").into_iter().map(|id| (id, false)).collect();
                    }
                    fake::ok(stream, "");
                }
                _ => fake::ok(stream, ""),
            }
        });
        (Client::new(&address, "someone", "secret"), stored)
    }

    #[test]
    fn a_playlist_hiding_a_missing_song_is_not_changed_by_position() {
        let (client, stored) = navidrome(&[("x", true), ("a", false), ("b", false), ("c", false)]);
        let held = |stored: &std::sync::Mutex<Stored>| stored.lock().unwrap().songs.iter().map(|(id, _)| id.clone()).collect::<Vec<_>>();
        let shown = list("a b c");
        // Taking out "b" as shown would have taken out "a"; moving "c" to the top, or putting
        // back a removed song, would have thrown "x" away. None of them is sent.
        for edit in [Edit::Remove(vec![spot(&shown, 1)]), moving(&shown, 2, 0), restoring(&list("a b c d"), &[3])] {
            assert_eq!(carry_out(&client, "p1", &shown, &edit).unwrap(), Outcome::HidesSongs, "{edit:?}");
        }
        assert!(stored.lock().unwrap().writes.is_empty());
        assert_eq!(held(&stored), list("x a b c"));
        // Adding is by the song and goes at the end, so it still goes.
        client.add_to_playlist("p1", &["d".to_string()]).unwrap();
        assert_eq!(held(&stored), list("x a b c d"));

        // With the song back in the library, the same removal takes out "b".
        stored.lock().unwrap().songs[0].1 = false;
        let shown = list("x a b c d");
        let outcome = carry_out(&client, "p1", &shown, &Edit::Remove(vec![spot(&shown, 2)])).unwrap();
        assert_eq!(outcome, Outcome::Done { elsewhere: false, before: list("x a b c d"), after: list("x a c d") });
        assert_eq!(held(&stored), list("x a c d"));
    }

    #[test]
    fn writes_a_playlist_as_a_spreadsheet() {
        use crate::subsonic::Song;
        let song = Song { title: "Say \"Hello\", Wave".into(), artist: "Them".into(), album: "One, Two".into(), duration: 185, ..Default::default() };
        let text = csv(&[song], &[1_700_000_000]);
        assert_eq!(
            text,
            "Track Name,Artist Name(s),Album Name,Duration (ms),Added At\r\n\"Say \"\"Hello\"\", Wave\",\"Them\",\"One, Two\",185000,2023-11-14T22:13:20Z\r\n"
        );
        assert_eq!(file_name("AC/DC: Best?"), "AC_DC_ Best_");
        assert_eq!(file_name(" .. "), "Playlist");
    }
}
