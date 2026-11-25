//! Bringing playlists over from Spotify.
//!
//! Spotify won't hand playlists to another app directly, but they can be exported as files:
//! one CSV per playlist from a tool such as Exportify, or the JSON files in the "download
//! your data" archive Spotify sends on request. This reads those, finds each song in the
//! library by title and artist, and says which ones it couldn't find.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::search::{Index, fold, slips};
use crate::subsonic::Song;

pub use nexus_core::asked::Wanted;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct WantedList {
    pub name: String,
    pub tracks: Vec<Wanted>,
}

// ---- reading the files ---------------------------------------------------------------------------

/// Splits CSV text into rows of fields. Fields in quotes may hold commas, line breaks and
/// (doubled) quotes.
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                chars.next();
                field.push('"');
            }
            (true, '"') => quoted = false,
            (true, c) => field.push(c),
            (false, '"') => quoted = true,
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {}
            (false, '\n') => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, c) => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|row| row.iter().any(|field| !field.trim().is_empty()));
    rows
}

/// A date as these files write it: with a time ("2023-12-20T10:15:30Z") or without.
fn when(text: &str) -> i64 {
    let text = text.trim();
    let exact = crate::added::parse_time(text);
    if exact > 0 {
        return exact;
    }
    chrono::NaiveDate::parse_from_str(text.get(..10).unwrap_or(text), "%Y-%m-%d")
        .ok()
        .and_then(|day| day.and_hms_opt(12, 0, 0))
        .map(|noon| noon.and_utc().timestamp())
        .unwrap_or(0)
}

/// Reads a playlist exported as CSV. The tools that make these don't agree on the columns,
/// so they're found by what their headings say. `None` if it isn't a list of songs.
fn read_csv(text: &str, name: &str) -> Option<WantedList> {
    let rows = csv_rows(text);
    let header: Vec<String> = rows.first()?.iter().map(|h| h.trim().to_lowercase()).collect();
    let column = |wanted: &[&str], unwanted: &[&str]| {
        wanted.iter().find_map(|want| {
            header.iter().position(|h| h.contains(want) && !unwanted.iter().any(|no| h.contains(no)))
        })
    };
    let ids = ["uri", " id", "url", "isrc"];
    let title = column(&["track name", "song name", "title"], &ids)
        .or_else(|| header.iter().position(|h| h == "name" || h == "track" || h == "song"))?;
    let artist = column(&["artist name", "artist"], &[ids.as_slice(), &["genre"]].concat())?;
    let album = column(&["album name", "album"], &[ids.as_slice(), &["artist", "date", "upc", "type"]].concat());
    let added = column(&["added at", "added_at", "date added", "added on"], &[]);
    let length = column(&["duration", "length"], &[]);
    let in_ms = length.is_some_and(|at| header[at].contains("ms"));
    let cell = |row: &Vec<String>, at: Option<usize>| at.and_then(|at| row.get(at)).map(|text| text.trim().to_string()).unwrap_or_default();
    let tracks = rows[1..]
        .iter()
        .filter_map(|row| {
            let title = cell(row, Some(title));
            if title.is_empty() {
                return None;
            }
            let length: f64 = cell(row, length).parse().unwrap_or(0.0);
            Some(Wanted {
                title,
                artists: cell(row, Some(artist)).split([',', ';']).map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect(),
                album: cell(row, album),
                seconds: if in_ms || length > 20_000.0 { (length / 1000.0) as u32 } else { length as u32 },
                added: when(&cell(row, added)),
            })
        })
        .collect();
    Some(WantedList { name: name.to_string(), tracks })
}

/// Reads the JSON from Spotify's own data download: `Playlist1.json` (every playlist) or
/// `YourLibrary.json` (liked songs).
fn read_json(text: &str) -> Vec<WantedList> {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text.trim_start_matches('\u{feff}')) else {
        return Vec::new();
    };
    let text_at = |value: &serde_json::Value, keys: &[&str]| {
        keys.iter().find_map(|key| value.get(key).and_then(|v| v.as_str())).unwrap_or_default().to_string()
    };
    let song = |value: &serde_json::Value, added: i64| {
        let title = text_at(value, &["trackName", "track", "name"]);
        (!title.is_empty()).then(|| Wanted {
            title,
            artists: vec![text_at(value, &["artistName", "artist"])].into_iter().filter(|a| !a.is_empty()).collect(),
            album: text_at(value, &["albumName", "album"]),
            seconds: 0,
            added,
        })
    };
    let mut lists = Vec::new();
    for playlist in root.get("playlists").and_then(|p| p.as_array()).into_iter().flatten() {
        let tracks: Vec<Wanted> = playlist
            .get("items")
            .and_then(|items| items.as_array())
            .into_iter()
            .flatten()
            .filter_map(|item| {
                // Podcast episodes and files from the user's own PC have no "track".
                let track = item.get("track").filter(|track| track.is_object())?;
                song(track, when(&text_at(item, &["addedDate", "addedAt"])))
            })
            .collect();
        let name = text_at(playlist, &["name"]);
        if !name.is_empty() && !tracks.is_empty() {
            lists.push(WantedList { name, tracks });
        }
    }
    if let Some(liked) = root.get("tracks").and_then(|t| t.as_array()) {
        let tracks: Vec<Wanted> = liked.iter().filter_map(|track| song(track, 0)).collect();
        if !tracks.is_empty() {
            lists.push(WantedList { name: "Liked Songs (Spotify)".into(), tracks });
        }
    }
    lists
}

/// Reads every playlist in these files. A folder stands for the files in it.
pub fn read(paths: &[PathBuf]) -> Vec<WantedList> {
    let mut files: Vec<PathBuf> = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut inside: Vec<PathBuf> =
                std::fs::read_dir(path).into_iter().flatten().flatten().map(|entry| entry.path()).collect();
            inside.sort();
            files.extend(inside);
        } else {
            files.push(path.clone());
        }
    }
    let mut lists = Vec::new();
    for file in files {
        let kind = file.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        if kind != "csv" && kind != "json" {
            continue;
        }
        let Ok(bytes) = std::fs::read(&file) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        if kind == "json" {
            lists.extend(read_json(&text));
        } else {
            // Exportify names the file after the playlist, with underscores for spaces.
            let name = file.file_stem().map(|s| s.to_string_lossy().replace('_', " ")).unwrap_or_default();
            let name = if fold(&name).split_whitespace().eq(["liked", "songs"]) { "Liked Songs (Spotify)".to_string() } else { name };
            lists.extend(read_csv(&text, name.trim()).filter(|list| !list.tracks.is_empty()));
        }
    }
    lists
}

// ---- doing it ------------------------------------------------------------------------------------

/// How one playlist fared.
pub struct ListReport {
    pub name: String,
    /// The playlist it went into; empty if none of its songs were found.
    pub playlist: String,
    pub total: usize,
    pub found: usize,
    /// How many songs were put in. Less than `found` when the playlist already existed and
    /// had some of them.
    pub added: usize,
    /// The playlist was already there and has been topped up.
    pub existed: bool,
    pub missing: Vec<Wanted>,
    /// The songs put in, with when each was added to the playlist on Spotify (0 if unknown).
    pub dated: Vec<(String, i64)>,
}

/// Reads the exported playlists, finds their songs in the library and makes the playlists
/// on the server. A playlist that already exists by that name gains the songs it's missing,
/// so the same files can be brought in again once more music has been added to the library.
pub fn run(client: &crate::subsonic::Client, index: &Index, paths: &[PathBuf]) -> Result<Vec<ListReport>, String> {
    let lists = read(paths);
    if lists.is_empty() {
        return Err("No playlists found there. Choose the CSV files from Exportify, or Playlist1.json from Spotify's data download.".into());
    }
    let matcher = Matcher::new(index);
    let name_of = |name: &str| fold(name).split_whitespace().collect::<Vec<_>>().join(" ");
    let mine = client.username().to_lowercase();
    let existing: HashMap<String, String> = client
        .playlists()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|p| !crate::link::unlisted(&p.name) && (p.owner.is_empty() || p.owner.to_lowercase() == mine))
        .map(|p| (name_of(&p.name), p.id))
        .collect();
    let mut reports = Vec::new();
    for list in lists {
        let mut found: Vec<(String, i64)> = Vec::new();
        let mut missing = Vec::new();
        for wanted in &list.tracks {
            match matcher.find(wanted) {
                Some(song) => found.push((song.id.clone(), wanted.added)),
                None => missing.push(wanted.clone()),
            }
        }
        let mut report = ListReport {
            name: list.name.clone(),
            playlist: String::new(),
            total: list.tracks.len(),
            found: found.len(),
            added: 0,
            existed: false,
            missing,
            dated: Vec::new(),
        };
        if !found.is_empty() {
            match existing.get(&name_of(&list.name)) {
                Some(id) => {
                    let have: std::collections::HashSet<String> =
                        client.playlist(id).map_err(|e| e.to_string())?.songs.into_iter().map(|song| song.id).collect();
                    let mut new: Vec<(String, i64)> = Vec::new();
                    for (song, at) in found {
                        if !have.contains(&song) && !new.iter().any(|(other, _)| *other == song) {
                            new.push((song, at));
                        }
                    }
                    if !new.is_empty() {
                        let ids: Vec<String> = new.iter().map(|(id, _)| id.clone()).collect();
                        client.add_to_playlist(id, &ids).map_err(|e| e.to_string())?;
                    }
                    report.playlist = id.clone();
                    report.existed = true;
                    report.added = new.len();
                    report.dated = new;
                }
                None => {
                    let ids: Vec<String> = found.iter().map(|(id, _)| id.clone()).collect();
                    if let Some(kept) = crate::link::kept_name(&list.name) {
                        return Err(kept.to_string());
                    }
                    report.playlist = client.create_playlist(&list.name, &ids).map_err(|e| e.to_string())?.id;
                    report.added = found.len();
                    report.dated = found;
                }
            }
        }
        reports.push(report);
    }
    Ok(reports)
}

/// The songs that couldn't be found, as text to keep: one per line, by playlist.
pub fn missing_text(reports: &[ListReport]) -> String {
    let mut text = String::from("Songs from your Spotify playlists that weren't found in your library\r\n");
    for report in reports.iter().filter(|report| !report.missing.is_empty()) {
        text.push_str(&format!("\r\n{} ({} of {} not found)\r\n", report.name, report.missing.len(), report.total));
        for song in &report.missing {
            let album = if song.album.is_empty() { String::new() } else { format!("  [{}]", song.album) };
            text.push_str(&format!("  {} - {}{album}\r\n", song.artists.join(", "), song.title));
        }
    }
    text
}

// ---- finding the songs ---------------------------------------------------------------------------

pub(crate) use nexus_core::text::{undecorated, version};

/// The same, lower-cased and without accents or punctuation.
fn plain(title: &str) -> String {
    fold(&undecorated(title)).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Finds songs in the library.
pub struct Matcher<'a> {
    index: &'a Index,
    /// Songs by their plain title.
    titled: HashMap<String, Vec<usize>>,
}

impl<'a> Matcher<'a> {
    pub fn new(index: &'a Index) -> Matcher<'a> {
        let mut titled: HashMap<String, Vec<usize>> = HashMap::new();
        for (at, song) in index.library.songs.iter().enumerate() {
            titled.entry(plain(&song.title)).or_default().push(at);
        }
        Matcher { index, titled }
    }

    /// The library's copy of a song, if it has one: the same title (give or take a slip of
    /// the keyboard and anything in brackets) by one of the same artists. Of several, the
    /// one on the same album, of the same length and the same kind of recording.
    pub fn find(&self, wanted: &Wanted) -> Option<&'a Song> {
        let songs = &self.index.library.songs;
        let title = plain(&wanted.title);
        let mut candidates: Vec<&Song> =
            self.titled.get(&title).into_iter().flatten().map(|&at| &songs[at]).collect();
        if candidates.is_empty() {
            // Not there letter for letter: look for a near miss.
            let letters: Vec<char> = title.chars().collect();
            let limit = match letters.len() {
                0..=3 => 0,
                4..=7 => 1,
                _ => 2,
            };
            let query = format!("{title} {}", wanted.artists.first().map(String::as_str).unwrap_or_default());
            let near = self.index.search(&query, &|_| false, 0, 0, 8).songs;
            let near: Vec<String> = near
                .iter()
                .map(|song| plain(&song.title))
                .filter(|other| limit > 0 && slips(&letters, &other.chars().collect::<Vec<_>>(), limit) <= limit)
                .collect();
            candidates = near.iter().flat_map(|other| self.titled.get(other).into_iter().flatten()).map(|&at| &songs[at]).collect();
        }
        let artists: Vec<String> = wanted.artists.iter().map(|a| fold(a).split_whitespace().collect::<Vec<_>>().join(" ")).collect();
        let album = plain(&wanted.album);
        let kind = version(&wanted.title);
        candidates
            .into_iter()
            .filter(|song| {
                let theirs = fold(song.artist_label()).split_whitespace().collect::<Vec<_>>().join(" ");
                let also = fold(&song.artist).split_whitespace().collect::<Vec<_>>().join(" ");
                artists.is_empty()
                    || artists.iter().any(|mine| {
                        !mine.is_empty() && (theirs.contains(mine.as_str()) || also.contains(mine.as_str()) || mine.contains(also.as_str()) && !also.is_empty())
                    })
            })
            .map(|song| {
                let mut score = 0i32;
                if fold(&song.title).split_whitespace().eq(fold(&wanted.title).split_whitespace()) {
                    score += 3;
                }
                if !album.is_empty() && plain(&song.album) == album {
                    score += 2;
                }
                if wanted.seconds > 0 && song.duration > 0 {
                    score += match wanted.seconds.abs_diff(song.duration) {
                        0..=3 => 2,
                        4..=10 => 1,
                        11..=30 => 0,
                        _ => -2,
                    };
                }
                let theirs = [version(&song.title), version(&song.album)].concat();
                if kind.iter().any(|k| !theirs.contains(k)) || theirs.iter().any(|k| !kind.contains(k)) {
                    score -= 2;
                }
                (score, song)
            })
            // The first of equally good ones.
            .fold(None, |best: Option<(i32, &Song)>, (score, song)| match best {
                Some((top, _)) if top >= score => best,
                _ => Some((score, song)),
            })
            .map(|(_, song)| song)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Library;

    fn song(id: &str, title: &str, artist: &str, album: &str, seconds: u32) -> Song {
        Song { id: id.into(), title: title.into(), artist: artist.into(), album: album.into(), duration: seconds, ..Default::default() }
    }

    fn wanted(title: &str, artists: &[&str], album: &str, seconds: u32) -> Wanted {
        Wanted { title: title.into(), artists: artists.iter().map(|a| a.to_string()).collect(), album: album.into(), seconds, added: 0 }
    }

    #[test]
    fn reads_exportify_files_whatever_their_columns() {
        let newer = "\u{feff}Track URI,Track Name,Album Name,Artist Name(s),Release Date,Duration (ms),Popularity,Added By,Added At\n\
            spotify:track:1,\"What A Time (feat. Niall Horan)\",Inner Monologue Part 1,\"Julia Michaels,Niall Horan\",2019-01-24,173000,70,me,2023-12-20T10:15:30Z\n\
            spotify:track:2,\"Hello, \"\"World\"\"\",Greetings,Someone,2020-01-01,200000,1,me,2023-12-21T00:00:00Z\n";
        let list = read_csv(newer, "Home").unwrap();
        assert_eq!(list.name, "Home");
        assert_eq!(list.tracks.len(), 2);
        assert_eq!(list.tracks[0].title, "What A Time (feat. Niall Horan)");
        assert_eq!(list.tracks[0].artists, ["Julia Michaels", "Niall Horan"]);
        assert_eq!(list.tracks[0].album, "Inner Monologue Part 1");
        assert_eq!(list.tracks[0].seconds, 173);
        assert_eq!(list.tracks[0].added, crate::added::parse_time("2023-12-20T10:15:30Z"));
        assert_eq!(list.tracks[1].title, "Hello, \"World\"");
        // An older layout: ids first, no album, artists split with semicolons.
        let older = "Spotify ID,Artist IDs,Track Name,Artist Name(s),Added At\r\nabc,def,Rockstar,Nickelback;Someone Else,2023-11-10\r\n";
        let list = read_csv(older, "Old").unwrap();
        assert_eq!(list.tracks[0].title, "Rockstar");
        assert_eq!(list.tracks[0].artists, ["Nickelback", "Someone Else"]);
        assert!(list.tracks[0].added > 0);
        // Not a list of songs at all.
        assert!(read_csv("a,b\n1,2\n", "x").is_none());
    }

    #[test]
    fn reads_spotifys_own_download() {
        let playlists = r#"{"playlists":[{"name":"Road trip","lastModifiedDate":"2024-01-02","items":[
            {"track":{"trackName":"Psycho","artistName":"Maisie Peters","albumName":"You Signed Up For This","trackUri":"spotify:track:1"},"episode":null,"localTrack":null,"addedDate":"2023-10-26"},
            {"track":null,"episode":{"episodeName":"A podcast"},"addedDate":"2023-10-27"}]}]}"#;
        let lists = read_json(playlists);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0].name, "Road trip");
        assert_eq!(lists[0].tracks.len(), 1);
        assert_eq!(lists[0].tracks[0].artists, ["Maisie Peters"]);
        assert!(lists[0].tracks[0].added > 0);
        let library = r#"{"tracks":[{"artist":"3 Doors Down","album":"Away From The Sun","track":"When I'm Gone","uri":"spotify:track:2"}],"albums":[]}"#;
        let lists = read_json(library);
        assert_eq!(lists[0].name, "Liked Songs (Spotify)");
        assert_eq!(lists[0].tracks[0].title, "When I'm Gone");
        assert!(read_json("not json").is_empty());
    }

    #[test]
    fn finds_the_right_copy_of_a_song() {
        let index = Index::new(Library {
            songs: vec![
                song("1", "Rockstar", "Nickelback", "All the Right Reasons", 255),
                song("2", "Rockstar", "Post Malone", "beerbongs & bentleys", 218),
                song("3", "What a Time", "Julia Michaels", "Inner Monologue Part 1", 173),
                song("4", "When I'm Gone", "3 Doors Down", "Away from the Sun", 260),
                song("5", "When I'm Gone (Live)", "3 Doors Down", "Away from the Sun: Live", 290),
                song("6", "Beyoncé Interlude", "Beyoncé", "B'Day", 60),
                song("7", "Sweet Child o' Mine", "Guns N' Roses", "Appetite for Destruction", 356),
            ],
            ..Default::default()
        });
        let matcher = Matcher::new(&index);
        let id = |wanted: Wanted| matcher.find(&wanted).map(|song| song.id.as_str());
        // Same title, different artists: the artist decides.
        assert_eq!(id(wanted("Rockstar - 2020 Remaster", &["Nickelback"], "All The Right Reasons (15th Anniversary)", 255)), Some("1"));
        assert_eq!(id(wanted("rockstar (feat. 21 Savage)", &["Post Malone", "21 Savage"], "", 0)), Some("2"));
        // Decorations Spotify adds don't matter; nor do accents.
        assert_eq!(id(wanted("What A Time (feat. Niall Horan)", &["Julia Michaels", "Niall Horan"], "", 173)), Some("3"));
        assert_eq!(id(wanted("Beyonce Interlude", &["Beyonce"], "", 0)), Some("6"));
        // The studio recording was asked for, so not the live one, and the other way round.
        assert_eq!(id(wanted("When I'm Gone", &["3 Doors Down"], "Away From The Sun", 260)), Some("4"));
        assert_eq!(id(wanted("When I'm Gone - Live", &["3 Doors Down"], "", 0)), Some("5"));
        // A small slip in the title is forgiven; someone else's song of the same name isn't.
        assert_eq!(id(wanted("Sweet Child O Mine", &["Guns N' Roses"], "", 0)), Some("7"));
        assert_eq!(id(wanted("Sweet Chlid o' Mine", &["Guns N' Roses"], "", 0)), Some("7"));
        assert_eq!(id(wanted("Rockstar", &["Someone Else"], "", 0)), None);
        assert_eq!(id(wanted("A Song That Isn't Here", &["Nobody"], "", 0)), None);
    }
}
