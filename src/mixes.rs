//! Lists of songs put together from what's been played: the "Made for you" row on Home.
//!
//! They're built from the app's own copy of the library (see `search`), which carries how
//! often and when each song was last played, and from Recently played.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::search::Index;
use crate::subsonic::Song;

const DAY: i64 = 24 * 3600;

pub struct Mix {
    /// Names the mix in its address, e.g. "on-repeat" or "genre:Rock".
    pub id: String,
    pub title: String,
    pub about: String,
    pub songs: Vec<Song>,
}

/// A mix made to rules of your own. Every rule left at its starting value lets any song in.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Rules {
    /// Tells it apart from the others for good, whatever it's renamed to.
    pub id: u64,
    pub name: String,
    /// Part of a genre's name; empty for any.
    pub genre: String,
    /// At least this many stars; 0 for any.
    pub rating: u8,
    /// Only songs you've liked.
    pub liked: bool,
    /// Released in these years; 0 for no limit at that end.
    pub from_year: u32,
    pub to_year: u32,
    /// An entry of `PLAYED`.
    pub played: u8,
    /// An entry of `ADDED`.
    pub added: u8,
    /// An entry of `ORDERS`.
    pub order: u8,
    /// An entry of `SIZES`.
    pub size: u8,
}

/// When a song was last played: the name of each choice, and the fewest days ago (-1 for
/// "never played", 0 for any).
pub const PLAYED: [(&str, i64); 5] =
    [("Any time", 0), ("Never played", -1), ("Not in the last month", 30), ("Not in the last six months", 182), ("Not in the last year", 365)];
/// When a song came into the library: the name of each choice, and the most days ago (0 for any).
pub const ADDED: [(&str, i64); 4] = [("Any time", 0), ("In the last month", 30), ("In the last six months", 182), ("In the last year", 365)];
pub const ORDERS: [&str; 4] = ["Shuffled", "Most played first", "Newest first", "Longest since played first"];
pub const SIZES: [usize; 4] = [25, 50, 100, 200];

impl Rules {
    /// The rules in words, for under the mix's name.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.genre.trim().is_empty() {
            parts.push(self.genre.trim().to_string());
        }
        match self.rating {
            0 => {}
            5 => parts.push("5 stars".into()),
            n => parts.push(format!("{n} stars or more")),
        }
        if self.liked {
            parts.push("liked".into());
        }
        match (self.from_year, self.to_year) {
            (0, 0) => {}
            (from, 0) => parts.push(format!("from {from} on")),
            (0, to) => parts.push(format!("up to {to}")),
            (from, to) if from == to => parts.push(from.to_string()),
            (from, to) => parts.push(format!("{from} to {to}")),
        }
        if self.played > 0 {
            parts.push(PLAYED[(self.played as usize).min(PLAYED.len() - 1)].0.to_lowercase());
        }
        if self.added > 0 {
            parts.push(format!("added {}", ADDED[(self.added as usize).min(ADDED.len() - 1)].0.to_lowercase()));
        }
        parts.push(ORDERS[(self.order as usize).min(ORDERS.len() - 1)].to_lowercase());
        if parts.len() == 1 { format!("Any song, {}", parts[0]) } else { parts.join(", ") }
    }

    fn lets_in(&self, song: &Song, now: i64) -> bool {
        let genre = crate::search::fold(self.genre.trim());
        let (_, played) = PLAYED[(self.played as usize).min(PLAYED.len() - 1)];
        let (_, added) = ADDED[(self.added as usize).min(ADDED.len() - 1)];
        let last = crate::added::parse_time(&song.played);
        (genre.is_empty() || crate::search::fold(&song.genre).contains(&genre))
            && song.user_rating >= self.rating
            && (!self.liked || crate::search::is_liked(song))
            && (self.from_year == 0 || song.year >= self.from_year)
            && (self.to_year == 0 || (song.year > 0 && song.year <= self.to_year))
            && match played {
                0 => true,
                -1 => song.play_count == 0,
                days => last == 0 || now - last > days * DAY,
            }
            && (added == 0 || now - crate::added::parse_time(&song.created) <= added * DAY)
    }

    /// The songs these rules pick out, in the order asked for.
    pub fn songs(&self, index: &Index, now: i64) -> Vec<Song> {
        let mut songs: Vec<&Song> = index.library.songs.iter().filter(|song| self.lets_in(song, now)).collect();
        match self.order {
            1 => songs.sort_by(|a, b| b.play_count.cmp(&a.play_count).then_with(|| a.title.cmp(&b.title))),
            2 => songs.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.title.cmp(&b.title))),
            3 => songs.sort_by_key(|song| crate::added::parse_time(&song.played)),
            _ => shuffle_for_today(&mut songs, &format!("rule:{}", self.id), now),
        }
        songs.into_iter().take(SIZES[(self.size as usize).min(SIZES.len() - 1)]).cloned().collect()
    }
}

/// The mixes made to rules, kept here so they can be built off the UI thread.
static RULES: RwLock<Vec<Rules>> = RwLock::new(Vec::new());

pub fn set_rules(rules: Vec<Rules>) {
    if let Ok(mut all) = RULES.write() {
        *all = rules;
    }
}

fn rules() -> Vec<Rules> {
    RULES.read().map(|rules| rules.clone()).unwrap_or_default()
}

/// Recently played, as song ids and when: kept here so mixes can be built off the UI thread.
static PLAYS: RwLock<Vec<(String, i64)>> = RwLock::new(Vec::new());

pub fn set_plays(plays: Vec<(String, i64)>) {
    if let Ok(mut all) = PLAYS.write() {
        *all = plays;
    }
}

/// Shuffles the same way all day, so a mix doesn't change each time it's opened.
fn shuffle_for_today(songs: &mut [&Song], id: &str, now: i64) {
    let seed = id.bytes().fold((now / DAY) as u64, |seed, byte| seed.wrapping_mul(31).wrapping_add(byte as u64));
    fastrand::Rng::with_seed(seed).shuffle(songs);
}

fn on_repeat(index: &Index, plays: &[(String, i64)], now: i64) -> Vec<Song> {
    // How often each song was played in the last month, and when last.
    let mut counts: HashMap<&str, (u32, i64)> = HashMap::new();
    for (id, at) in plays.iter().filter(|(_, at)| now - at < 30 * DAY) {
        let entry = counts.entry(id.as_str()).or_default();
        entry.0 += 1;
        entry.1 = entry.1.max(*at);
    }
    let mut repeated: Vec<(&Song, (u32, i64))> = index
        .library
        .songs
        .iter()
        .filter_map(|song| counts.get(song.id.as_str()).filter(|(times, _)| *times >= 2).map(|&count| (song, count)))
        .collect();
    repeated.sort_by(|a, b| b.1.cmp(&a.1));
    repeated.into_iter().take(30).map(|(song, _)| song.clone()).collect()
}

fn top_songs(index: &Index) -> Vec<Song> {
    let mut played: Vec<&Song> = index.library.songs.iter().filter(|song| song.play_count > 0).collect();
    played.sort_by(|a, b| b.play_count.cmp(&a.play_count));
    played.into_iter().take(50).cloned().collect()
}

fn rediscover(index: &Index, now: i64) -> Vec<Song> {
    let mut old: Vec<&Song> = index
        .library
        .songs
        .iter()
        .filter(|song| song.play_count >= 3 || (song.play_count >= 1 && crate::search::is_liked(song)))
        .filter(|song| {
            let last = crate::added::parse_time(&song.played);
            last > 0 && now - last > 60 * DAY
        })
        .collect();
    shuffle_for_today(&mut old, "rediscover", now);
    old.into_iter().take(40).cloned().collect()
}

/// The genres played most, with enough songs to make a mix of.
fn top_genres(index: &Index) -> Vec<String> {
    let mut genres: HashMap<&str, (u32, u32)> = HashMap::new();
    for song in index.library.songs.iter().filter(|song| !song.genre.is_empty()) {
        let entry = genres.entry(song.genre.as_str()).or_default();
        entry.0 += song.play_count;
        entry.1 += 1;
    }
    let mut ranked: Vec<(&str, u32)> =
        genres.into_iter().filter(|(_, (plays, songs))| *plays > 0 && *songs >= 15).map(|(genre, (plays, _))| (genre, plays)).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    ranked.into_iter().take(2).map(|(genre, _)| genre.to_string()).collect()
}

fn genre_mix(index: &Index, genre: &str, now: i64) -> Vec<Song> {
    let mut songs: Vec<&Song> = index.library.songs.iter().filter(|song| song.genre == genre).collect();
    shuffle_for_today(&mut songs, genre, now);
    songs.into_iter().take(40).cloned().collect()
}

/// The mix with this id, if there's enough to make it from.
pub fn one(index: &Index, id: &str) -> Option<Mix> {
    let now = chrono::Utc::now().timestamp();
    let plays = PLAYS.read().map(|plays| plays.clone()).unwrap_or_default();
    // One made to rules is there as soon as it has been made, even while nothing fits it.
    if let Some(rule) = id.strip_prefix("rule:").and_then(|id| id.parse::<u64>().ok()) {
        let rules = rules().into_iter().find(|rules| rules.id == rule)?;
        return Some(Mix { id: id.to_string(), title: rules.name.clone(), about: rules.summary(), songs: rules.songs(index, now) });
    }
    let (title, about, songs, least) = match id {
        "on-repeat" => ("On repeat".to_string(), "What you've played most lately", on_repeat(index, &plays, now), 5),
        "top" => ("Your top songs".to_string(), "Your most played of all time", top_songs(index), 10),
        "rediscover" => ("Rediscover".to_string(), "Favourites you haven't played in a while", rediscover(index, now), 10),
        other => {
            let genre = other.strip_prefix("genre:")?;
            (format!("{genre} mix"), "From a genre you play a lot", genre_mix(index, genre, now), 15)
        }
    };
    (songs.len() >= least).then(|| Mix { id: id.to_string(), title, about: about.to_string(), songs })
}

/// Every mix there's enough listening to make.
pub fn all(index: &Index) -> Vec<Mix> {
    let genres: Vec<String> = top_genres(index).into_iter().map(|genre| format!("genre:{genre}")).collect();
    // Yours first, then the ones the app makes from your listening.
    let own: Vec<String> = rules().into_iter().map(|rules| format!("rule:{}", rules.id)).collect();
    own.iter()
        .map(String::as_str)
        .chain(["on-repeat", "top", "rediscover"])
        .chain(genres.iter().map(String::as_str))
        .filter_map(|id| one(index, id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Library;

    fn song(id: usize, genre: &str, plays: u32, last_played_days_ago: i64, now: i64) -> Song {
        let played = chrono::DateTime::from_timestamp(now - last_played_days_ago * DAY, 0).unwrap().to_rfc3339();
        Song {
            id: id.to_string(),
            title: format!("Song {id}"),
            genre: genre.into(),
            play_count: plays,
            played: if plays > 0 { played } else { String::new() },
            ..Default::default()
        }
    }

    #[test]
    fn builds_mixes_from_listening() {
        let now = chrono::Utc::now().timestamp();
        // Thirty rock songs played a lot long ago, twenty jazz songs played once this week,
        // and ten songs never played.
        let mut songs: Vec<Song> = (0..30).map(|i| song(i, "Rock", 30 - i as u32 + 3, 90, now)).collect();
        songs.extend((30..50).map(|i| song(i, "Jazz", 1, 2, now)));
        songs.extend((50..60).map(|i| song(i, "Folk", 0, 0, now)));
        let index = Index::new(Library { songs, ..Default::default() });

        let top = one(&index, "top").unwrap();
        assert_eq!(top.songs.len(), 50);
        assert_eq!(top.songs[0].id, "0");
        // Only what was played often and not lately is worth rediscovering.
        let old = one(&index, "rediscover").unwrap();
        assert_eq!(old.songs.len(), 30);
        assert!(old.songs.iter().all(|song| song.genre == "Rock"));
        // The same all day.
        let again: Vec<String> = one(&index, "rediscover").unwrap().songs.into_iter().map(|song| song.id).collect();
        assert_eq!(again, old.songs.iter().map(|song| song.id.clone()).collect::<Vec<_>>());
        // Genres by how much they're played; one never played doesn't get a mix.
        assert_eq!(top_genres(&index), ["Rock", "Jazz"]);
        assert_eq!(one(&index, "genre:Jazz").unwrap().songs.len(), 20);
        assert!(one(&index, "genre:Nothing").is_none());

        // "On repeat" needs songs played more than once lately.
        set_plays(vec![]);
        assert!(one(&index, "on-repeat").is_none());
        let mut plays: Vec<(String, i64)> = Vec::new();
        for id in 30..36 {
            plays.push((id.to_string(), now - 100));
            plays.push((id.to_string(), now - 5000));
        }
        plays.push(("36".into(), now - 50));
        plays.push(("30".into(), now - 10));
        // Old plays don't count.
        plays.push(("40".into(), now - 40 * DAY));
        plays.push(("40".into(), now - 41 * DAY));
        set_plays(plays);
        let repeat = one(&index, "on-repeat").unwrap();
        assert_eq!(repeat.songs.len(), 6);
        assert_eq!(repeat.songs[0].id, "30");
        let offered: Vec<String> = all(&index).into_iter().map(|mix| mix.id).collect();
        assert_eq!(offered, ["on-repeat", "top", "rediscover", "genre:Rock", "genre:Jazz"]);
        set_plays(vec![]);

        // A mix made to rules: rock not played in the last month, most played first.
        let rule = Rules { id: 7, name: "Old rock".into(), genre: "rock".into(), played: 2, order: 1, size: 0, ..Default::default() };
        let picked = rule.songs(&index, now);
        assert_eq!(picked.len(), 25);
        assert!(picked.iter().all(|song| song.genre == "Rock") && picked[0].id == "0");
        assert_eq!(rule.summary(), "rock, not in the last month, most played first");
        // Never played, any genre: the ten folk songs.
        let fresh = Rules { played: 1, ..Default::default() };
        assert_eq!(fresh.songs(&index, now).len(), 10);
        assert_eq!(fresh.summary(), "never played, shuffled");
        assert_eq!(Rules::default().summary(), "Any song, shuffled");
        // Played in the last month counts as played lately; the jazz was two days ago.
        assert!(Rules { genre: "jazz".into(), played: 2, ..Default::default() }.songs(&index, now).is_empty());
        // Once made it's among the mixes, ahead of the app's own, and has a page.
        set_rules(vec![rule.clone()]);
        assert_eq!(all(&index).first().map(|mix| (mix.id.clone(), mix.title.clone())), Some(("rule:7".to_string(), "Old rock".to_string())));
        assert_eq!(one(&index, "rule:7").unwrap().about, rule.summary());
        assert!(one(&index, "rule:8").is_none());
        set_rules(vec![]);
    }
}
