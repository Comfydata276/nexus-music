//! Lyrics kept on this PC so they show the instant they're wanted.
//!
//! Lyrics come from the server (embedded in a song, or in a file beside it). Asking takes a
//! moment, long enough to see an empty page first. So whatever the server says is kept here,
//! and fetched ahead for the songs coming up in the queue: opening the lyrics then shows
//! what's kept at once, and the server is asked again in the background in case they've
//! changed. It's a copy, cleared with the rest of the cache.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::subsonic::Lyrics;

/// What's kept for a song, and how long ago the server was asked.
pub struct Kept {
    /// `None` if the server had no lyrics for it.
    pub lyrics: Option<Lyrics>,
    pub age: Duration,
}

fn file(dir: &Path, song: &str) -> PathBuf {
    let safe: String = song.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    dir.join(format!("{safe}.json"))
}

pub fn kept(dir: &Path, song: &str) -> Option<Kept> {
    let path = file(dir, song);
    let lyrics: Option<Lyrics> = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    let age = std::fs::metadata(&path).and_then(|meta| meta.modified()).ok().and_then(|at| at.elapsed().ok()).unwrap_or(Duration::MAX);
    Some(Kept { lyrics, age })
}

/// Keeps what the server said of a song, including that it has none.
pub fn keep(dir: &Path, song: &str, lyrics: &Option<Lyrics>) {
    let _ = std::fs::create_dir_all(dir);
    if let Ok(json) = serde_json::to_vec(lyrics) {
        let path = file(dir, song);
        let temp = path.with_extension("tmp");
        if std::fs::write(&temp, json).is_ok() {
            let _ = std::fs::rename(&temp, &path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsonic::LyricLine;

    #[test]
    fn keeps_what_the_server_said() {
        let dir = std::env::temp_dir().join(format!("nexus-lyrics-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(kept(&dir, "a/1").is_none());
        let lyrics = Lyrics { synced: true, line: vec![LyricLine { start: Some(1200), value: "One".into() }, LyricLine { start: None, value: "".into() }] };
        keep(&dir, "a/1", &Some(lyrics.clone()));
        keep(&dir, "b", &None);
        let one = kept(&dir, "a/1").unwrap();
        assert_eq!(one.lyrics, Some(lyrics));
        assert!(one.age < Duration::from_secs(60));
        // That a song has none is kept too, which is different from never having asked.
        assert_eq!(kept(&dir, "b").map(|kept| kept.lyrics), Some(None));
        assert!(dir.join("a_1.json").exists());
        // A file a power cut emptied or cut short (it is renamed in place without being
        // flushed) is nothing kept, not "the song has none": the server is asked again.
        let whole = std::fs::read(dir.join("a_1.json")).unwrap();
        for cut in [&whole[..0], &whole[..whole.len() / 2], &b"nul"[..]] {
            std::fs::write(dir.join("a_1.json"), cut).unwrap();
            assert!(kept(&dir, "a/1").is_none(), "{}", String::from_utf8_lossy(cut));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
