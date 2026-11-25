//! Artists the library doesn't have, like the ones that are played most.
//!
//! Requests can only fetch what's asked for by name, which leaves out everything nobody
//! has thought to ask for. This fills the gap: the server is asked who is similar to the
//! artists played most, the ones that aren't in the library are kept, and those named by
//! several of the favourites come first. Lidarr then says who each one is, so their page
//! can be opened and their music asked for.
//!
//! It takes a dozen or so questions, so the answer is kept for a few days.
//!
//! Who the favourites are, how what the server says of them is ranked, which of Lidarr's
//! answers is meant, and how long the answer is kept for are here. Asking the server and
//! Lidarr, and keeping the answer, are the app's.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::lidarr::{ArtistOffer, Lidarr};
use crate::requests::{Library, Stopped, unless_stopped};
use crate::model::Song;
use crate::text::{fold, same_words};

/// How long suggestions are kept before they're looked for again; and how long to wait
/// before trying again when there were none to be had.
pub const KEPT_FOR: i64 = 3 * 24 * 3600;
pub const TRY_AGAIN: i64 = 6 * 3600;
/// How many favourites are asked about, and how many suggestions are kept.
pub const FAVOURITES: usize = 10;
pub const SUGGESTIONS: usize = 14;

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

/// The suggestions as they're kept between runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Kept {
    /// When they were looked for.
    pub at: i64,
    pub artists: Vec<Suggestion>,
}

impl Kept {
    /// Whether these will do, or it's time to look again.
    pub fn fresh(&self, now: i64) -> bool {
        now - self.at < if self.artists.is_empty() { TRY_AGAIN } else { KEPT_FOR }
    }

    /// The suggestions to show: these, less anyone the library has gained since. `have` is
    /// the library's artists, each folded (see `text::fold`).
    pub fn shown(self, have: &HashSet<String>) -> Vec<Suggestion> {
        self.artists.into_iter().filter(|artist| !have.contains(&fold(&artist.name))).collect()
    }

    /// Stops suggesting an artist, e.g. once they've been added. Says whether that changed
    /// anything.
    pub fn forget(&mut self, id: &str) -> bool {
        let before = self.artists.len();
        self.artists.retain(|artist| artist.id != id);
        self.artists.len() != before
    }
}

/// The artists played most, favourite first, as the server's id for each and their name.
/// With nothing played yet, the ones there's most music by.
pub fn favourites(songs: &[Song]) -> Vec<(String, String)> {
    // For each artist: plays, songs, and what they're called.
    let mut tally: HashMap<&str, (u64, u32, &str)> = HashMap::new();
    for song in songs.iter().filter(|song| !song.artist_id.is_empty()) {
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
pub fn ranked(similar: &[(String, Vec<String>)], have: &HashSet<String>) -> Vec<(String, String)> {
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
pub fn meant(found: Vec<ArtistOffer>, name: &str) -> Option<ArtistOffer> {
    let mut same = found.into_iter().filter(|artist| same_words(&artist.name, name));
    let first = same.next()?;
    if first.cover.is_empty() { same.find(|artist| !artist.cover.is_empty()).or(Some(first)) } else { Some(first) }
}

/// Looks for suggestions afresh, from what the music server says of who is like each of the
/// favourites (`similar`, in the order of `favourites`, most alike first): Lidarr is asked
/// who each name is, a few at a time, and anyone it keeps track of already, or the library
/// has, is left out. What comes of it is for the app to keep. Slow: a question to Lidarr for
/// each suggestion. A stopped look gives `Err(Stopped)`, and is not to be kept as an answer.
pub fn look(lidarr: &Lidarr, similar: &[(String, Vec<String>)], library: &dyn Library, now: i64) -> Result<Kept, Stopped> {
    let have: HashSet<String> = library.artist_names();
    // Anyone Lidarr is already keeping track of is on their way, not news.
    let tracked = unless_stopped(lidarr.tracked())?.unwrap_or_default();
    let mut artists: Vec<Suggestion> = Vec::new();
    let bets = ranked(similar, &have);
    // A few more than are wanted are asked about: Lidarr won't know every one.
    for group in bets.chunks(4).take(SUGGESTIONS.div_ceil(4) + 2) {
        let answers: Vec<Result<Option<ArtistOffer>, Stopped>> = std::thread::scope(|scope| {
            let asked: Vec<_> = group.iter().map(|(name, _)| scope.spawn(move || unless_stopped(lidarr.find_artists(name)).map(|found| found.ok().and_then(|found| meant(found, name))))).collect();
            asked.into_iter().map(|asked| asked.join().unwrap_or(Ok(None))).collect()
        });
        for ((_, like), artist) in group.iter().zip(answers) {
            let Some(artist) = artist? else { continue };
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
    Ok(Kept { at: now, artists })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_favourites_and_who_they_lead_to() {
        let song = |artist: &str, id: &str, plays: u32| Song { artist: artist.into(), artist_id: id.into(), play_count: plays, ..Default::default() };
        let songs = [song("Quiet", "q", 1), song("Loud", "l", 30), song("Loud", "l", 5), song("Unplayed", "u", 0), song("Nobody", "", 99)];
        assert_eq!(favourites(&songs), [("l".to_string(), "Loud".to_string()), ("q".to_string(), "Quiet".to_string()), ("u".to_string(), "Unplayed".to_string())]);

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

    #[test]
    fn keeps_suggestions_for_a_while() {
        let suggestion = |id: &str, name: &str| Suggestion { id: id.into(), name: name.into(), ..Default::default() };
        let mut kept = Kept { at: 1000, artists: vec![suggestion("1", "Hanson"), suggestion("2", "Train")] };
        assert!(kept.fresh(1000 + KEPT_FOR - 1) && !kept.fresh(1000 + KEPT_FOR));
        assert_eq!(kept.clone().shown(&HashSet::from([fold("hanson")])), [suggestion("2", "Train")]);
        assert!(kept.forget("1") && !kept.forget("1"));
        kept.artists.clear();
        // None to be had: looked for again sooner.
        assert!(kept.fresh(1000 + TRY_AGAIN - 1) && !kept.fresh(1000 + TRY_AGAIN));
    }

    #[test]
    fn a_stopped_look_is_not_an_answer() {
        // Over a Lidarr that has been told to stop: no suggestions are made of it, not even
        // none (which would hide the row for days).
        struct Stopping;
        impl crate::http::Http for Stopping {
            fn send(&self, _: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
                Err(crate::http::Failure::Stopped)
            }
        }
        let lidarr = Lidarr::new("http://lidarr.test", "key", None, std::sync::Arc::new(Stopping));
        let similar = [("Loud".to_string(), vec!["Train".to_string()])];
        let found = look(&lidarr, &similar, &crate::requests::Snapshot::default(), 100);
        assert_eq!(found.map(|kept| kept.at), Err(Stopped));
    }
}
