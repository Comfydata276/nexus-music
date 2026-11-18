//! Artists the library doesn't have, like the ones that are played most.
//!
//! Requests can only fetch what's asked for by name, which leaves out everything nobody
//! has thought to ask for. This fills the gap: the server is asked who is similar to the
//! artists played most, the ones that aren't in the library are kept, and those named by
//! several of the favourites come first. Lidarr then says who each one is, so their page
//! can be opened and their music asked for.
//!
//! It takes a dozen or so questions, so the answer is kept for a few days.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::lidarr::{ArtistOffer, Lidarr};
use crate::musicbrainz::same_words;
use crate::search::{Index, fold};
use crate::subsonic::Client;

/// How long suggestions are kept before they're looked for again; and how long to wait
/// before trying again when there were none to be had.
const KEPT_FOR: i64 = 3 * 24 * 3600;
const TRY_AGAIN: i64 = 6 * 3600;
/// How many favourites are asked about, and how many suggestions are kept.
const FAVOURITES: usize = 10;
const SUGGESTIONS: usize = 14;

/// An artist worth a look.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Suggestion {
    /// Lidarr's name for the artist (a MusicBrainz id), and what they're called.
    pub id: String,
    pub name: String,
    pub cover: String,
    /// The favourite they're most like.
    pub like: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Kept {
    /// When they were looked for.
    at: i64,
    artists: Vec<Suggestion>,
}

static PLACE: OnceLock<PathBuf> = OnceLock::new();
static KEPT: RwLock<Option<Kept>> = RwLock::new(None);

/// Says where suggestions are kept between runs.
pub fn set_place(path: PathBuf) {
    let _ = PLACE.set(path);
}

fn kept() -> Kept {
    if let Some(kept) = KEPT.read().ok().and_then(|kept| kept.clone()) {
        return kept;
    }
    let kept: Kept = PLACE.get().and_then(|path| std::fs::read(path).ok()).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
    if let Ok(mut shared) = KEPT.write() {
        *shared = Some(kept.clone());
    }
    kept
}

fn keep(kept: Kept) {
    if let (Some(path), Ok(json)) = (PLACE.get(), serde_json::to_vec_pretty(&kept)) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, json);
    }
    if let Ok(mut shared) = KEPT.write() {
        *shared = Some(kept);
    }
}

/// Whether what's kept will do, or it's time to look again.
pub fn fresh(now: i64) -> bool {
    let kept = kept();
    now - kept.at < if kept.artists.is_empty() { TRY_AGAIN } else { KEPT_FOR }
}

/// The suggestions to show: what's kept, less anyone the library has gained since.
pub fn shown(index: &Index) -> Vec<Suggestion> {
    let have: HashSet<String> = index.library.artists.iter().map(|artist| fold(&artist.title)).collect();
    kept().artists.into_iter().filter(|artist| !have.contains(&fold(&artist.name))).collect()
}

/// Stops suggesting an artist, e.g. once they've been added.
pub fn forget(id: &str) {
    let mut kept = kept();
    let before = kept.artists.len();
    kept.artists.retain(|artist| artist.id != id);
    if kept.artists.len() != before {
        keep(kept);
    }
}

/// The artists played most, favourite first, as the server's id for each and their name.
/// With nothing played yet, the ones there's most music by.
fn favourites(index: &Index) -> Vec<(String, String)> {
    // For each artist: plays, songs, and what they're called.
    let mut tally: HashMap<&str, (u64, u32, &str)> = HashMap::new();
    for song in index.library.songs.iter().filter(|song| !song.artist_id.is_empty()) {
        let entry = tally.entry(song.artist_id.as_str()).or_insert((0, 0, song.artist.as_str()));
        entry.0 += song.play_count as u64;
        entry.1 += 1;
    }
    let mut ranked: Vec<(&str, (u64, u32, &str))> = tally.into_iter().collect();
    ranked.sort_by(|a, b| (b.1.0, b.1.1).cmp(&(a.1.0, a.1.1)).then(a.1.2.cmp(b.1.2)));
    ranked.into_iter().take(FAVOURITES).map(|(id, (_, _, name))| (id.to_string(), name.to_string())).collect()
}

/// Puts the names the server gave for each favourite in order: one that several favourites
/// lead to, or that a top favourite leads to first, is the better bet. `similar` holds, for
/// each favourite in order, who they're like, most alike first. Each name comes with the
/// favourite that counted most towards it.
fn ranked(similar: &[(String, Vec<String>)], have: &HashSet<String>) -> Vec<(String, String)> {
    struct Bet {
        name: String,
        score: f64,
        like: String,
        best: f64,
    }
    let mut bets: Vec<Bet> = Vec::new();
    let mut places: HashMap<String, usize> = HashMap::new();
    for (rank, (favourite, names)) in similar.iter().enumerate() {
        let weight = (FAVOURITES.saturating_sub(rank)).max(1) as f64;
        for (place, name) in names.iter().enumerate() {
            let key = fold(name);
            if key.is_empty() || have.contains(&key) {
                continue;
            }
            let share = weight * (1.0 - place as f64 / (names.len().max(1) as f64 * 1.5));
            let at = *places.entry(key).or_insert_with(|| {
                bets.push(Bet { name: name.clone(), score: 0.0, like: favourite.clone(), best: 0.0 });
                bets.len() - 1
            });
            let bet = &mut bets[at];
            bet.score += share;
            if share > bet.best {
                bet.best = share;
                bet.like = favourite.clone();
            }
        }
    }
    bets.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    bets.into_iter().map(|bet| (bet.name, bet.like)).collect()
}

/// Of the artists Lidarr found for a name, the one that's meant: called exactly that, and
/// if there are several, the one of some note (Lidarr has their picture).
fn meant(found: Vec<ArtistOffer>, name: &str) -> Option<ArtistOffer> {
    let mut same = found.into_iter().filter(|artist| same_words(&artist.name, name));
    let first = same.next()?;
    if first.cover.is_empty() { same.find(|artist| !artist.cover.is_empty()).or(Some(first)) } else { Some(first) }
}

/// Looks for suggestions afresh and keeps them. Slow: a question to the server for each
/// favourite and one to Lidarr for each suggestion.
pub fn find(client: &Client, index: &Index, lidarr: &Lidarr, now: i64) -> Vec<Suggestion> {
    // One search at a time: Home can be opened again while the first is still under way.
    if LOOKING.swap(true, Ordering::SeqCst) {
        return kept().artists;
    }
    let found = look(client, index, lidarr, now);
    LOOKING.store(false, Ordering::SeqCst);
    found
}

static LOOKING: AtomicBool = AtomicBool::new(false);

fn look(client: &Client, index: &Index, lidarr: &Lidarr, now: i64) -> Vec<Suggestion> {
    let favourites = favourites(index);
    let have: HashSet<String> = index.library.artists.iter().map(|artist| fold(&artist.title)).collect();
    // Four questions at a time, so neither the server nor Lidarr is leaned on.
    let mut similar: Vec<(String, Vec<String>)> = Vec::new();
    for group in favourites.chunks(4) {
        let answers: Vec<Vec<String>> = std::thread::scope(|scope| {
            let asked: Vec<_> = group.iter().map(|(id, _)| scope.spawn(move || client.similar_outside(id).unwrap_or_default())).collect();
            asked.into_iter().map(|asked| asked.join().unwrap_or_default()).collect()
        });
        similar.extend(group.iter().map(|(_, name)| name.clone()).zip(answers));
    }
    // Anyone Lidarr is already keeping track of is on their way, not news.
    let tracked = lidarr.tracked().unwrap_or_default();
    let mut artists: Vec<Suggestion> = Vec::new();
    let bets = ranked(&similar, &have);
    // A few more than are wanted are asked about: Lidarr won't know every one.
    for group in bets.chunks(4).take(SUGGESTIONS.div_ceil(4) + 2) {
        let answers: Vec<Option<ArtistOffer>> = std::thread::scope(|scope| {
            let asked: Vec<_> = group.iter().map(|(name, _)| scope.spawn(move || lidarr.find_artists(name).ok().and_then(|found| meant(found, name)))).collect();
            asked.into_iter().map(|asked| asked.join().ok().flatten()).collect()
        });
        for ((_, like), artist) in group.iter().zip(answers) {
            let Some(artist) = artist else { continue };
            if tracked.contains(&artist.id) || artists.iter().any(|known| known.id == artist.id) {
                continue;
            }
            artists.push(Suggestion { id: artist.id, name: artist.name, cover: artist.cover, like: like.clone() });
        }
        if artists.len() >= SUGGESTIONS {
            break;
        }
    }
    artists.truncate(SUGGESTIONS);
    keep(Kept { at: now, artists: artists.clone() });
    artists
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::Library;
    use crate::subsonic::Song;

    #[test]
    fn ranks_favourites_and_who_they_lead_to() {
        let song = |artist: &str, id: &str, plays: u32| Song { artist: artist.into(), artist_id: id.into(), play_count: plays, ..Default::default() };
        let index = Index::new(Library {
            songs: vec![song("Quiet", "q", 1), song("Loud", "l", 30), song("Loud", "l", 5), song("Unplayed", "u", 0), song("Nobody", "", 99)],
            ..Default::default()
        });
        assert_eq!(favourites(&index), [("l".to_string(), "Loud".to_string()), ("q".to_string(), "Quiet".to_string()), ("u".to_string(), "Unplayed".to_string())]);

        let names = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let similar = [("Loud".to_string(), names(&["Train", "Owned", "Hanson"])), ("Quiet".to_string(), names(&["Other", "hanson"]))];
        let have = HashSet::from([fold("Owned")]);
        // Hanson is named by both favourites, which beats being first for one of them.
        let bets = ranked(&similar, &have);
        assert_eq!(bets.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["Hanson", "Train", "Other"]);
        // It's captioned with the favourite it's nearer the top for.
        assert_eq!(bets[0].1, "Quiet");
        assert_eq!(bets[1].1, "Loud");

        let artist = |id: &str, name: &str, cover: &str| ArtistOffer { id: id.into(), name: name.into(), cover: cover.into(), ..Default::default() };
        let found = vec![artist("1", "The Train", ""), artist("2", "Train", ""), artist("3", "Train", "pic")];
        assert_eq!(meant(found, "train").map(|artist| artist.id), Some("3".to_string()));
        assert_eq!(meant(vec![artist("1", "Someone Else", "pic")], "Train"), None);
    }
}
