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
//! The ranking is in the library (`nexus_core::discover`), for the Android app to use too.
//! This is the Windows app's end: asking the server and Lidarr, and keeping the answer.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};

use nexus_core::discover::{Kept, SUGGESTIONS, favourites, meant, ranked};

use crate::lidarr::{ArtistOffer, Lidarr};
use crate::search::{Index, fold};
use crate::subsonic::Client;

pub use nexus_core::discover::Suggestion;

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
    kept().fresh(now)
}

/// The suggestions to show: what's kept, less anyone the library has gained since.
pub fn shown(index: &Index) -> Vec<Suggestion> {
    let have: HashSet<String> = index.library.artists.iter().map(|artist| fold(&artist.title)).collect();
    kept().shown(&have)
}

/// Stops suggesting an artist, e.g. once they've been added.
pub fn forget(id: &str) {
    let mut kept = kept();
    if kept.forget(id) {
        keep(kept);
    }
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
    let favourites = favourites(&index.library.songs);
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
