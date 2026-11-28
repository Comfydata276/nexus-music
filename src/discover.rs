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

use nexus_core::discover::{Kept, favourites};

use nexus_core::lidarr::RequestLidarr;
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
    let kept = PLACE.get().map(|path| read_kept(path)).unwrap_or_default();
    if let Ok(mut shared) = KEPT.write() {
        *shared = Some(kept.clone());
    }
    kept
}

/// What's kept, read back. They can be looked for again, so they are written in place: a
/// file that a kill or a power cut cut short, or emptied, is read as none kept, looked for
/// as soon as Home is opened, and written over. Never as suggestions, or as an error.
fn read_kept(path: &std::path::Path) -> Kept {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
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
pub fn find(client: &Client, index: &Index, lidarr: &dyn RequestLidarr, now: i64) -> Vec<Suggestion> {
    // One search at a time: Home can be opened again while the first is still under way.
    if LOOKING.swap(true, Ordering::SeqCst) {
        return kept().artists;
    }
    let found = look(client, index, lidarr, now);
    LOOKING.store(false, Ordering::SeqCst);
    found
}

static LOOKING: AtomicBool = AtomicBool::new(false);

fn look(client: &Client, index: &Index, lidarr: &dyn RequestLidarr, now: i64) -> Vec<Suggestion> {
    let favourites = favourites(&index.library.songs);
    // Four questions at a time, so neither the server nor Lidarr is leaned on.
    let mut similar: Vec<(String, Vec<String>)> = Vec::new();
    for group in favourites.chunks(4) {
        let answers: Vec<Vec<String>> = std::thread::scope(|scope| {
            let asked: Vec<_> = group.iter().map(|(id, _)| scope.spawn(move || client.similar_outside(id).unwrap_or_default())).collect();
            asked.into_iter().map(|asked| asked.join().unwrap_or_default()).collect()
        });
        similar.extend(group.iter().map(|(_, name)| name.clone()).zip(answers));
    }
    // Who each of those is, by Lidarr; what it keeps, or the library has, left out. (Nothing
    // on Windows stops a request, so "stopped" can't come of it.)
    let Ok(found) = nexus_core::discover::look(lidarr, &similar, index, now) else { return Vec::new() };
    keep(found.clone());
    found.artists
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_cut_short_are_none_kept_and_are_looked_for_again() {
        let path = std::env::temp_dir().join(format!("nexus-discover-{}.json", std::process::id()));
        let kept = Kept { at: 1_700_000_000, artists: vec![Suggestion { id: "1".into(), name: "Hanson".into(), ..Default::default() }] };
        let whole = serde_json::to_vec_pretty(&kept).unwrap();
        std::fs::write(&path, &whole).unwrap();
        assert!(read_kept(&path).fresh(1_700_000_001) && read_kept(&path).artists.len() == 1);
        for cut in [&whole[..0], &whole[..whole.len() / 2], &whole[..whole.len() - 1]] {
            std::fs::write(&path, cut).unwrap();
            let read = read_kept(&path);
            assert!(read.artists.is_empty() && !read.fresh(1_700_000_001), "{}", String::from_utf8_lossy(cut));
        }
        let _ = std::fs::remove_file(&path);
    }
}
