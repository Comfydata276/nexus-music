//! Lyrics from LRCLIB, for songs the server has none for.
//!
//! The server only has lyrics that are in a song's file or in a `.lrc` beside it, which
//! music fetched by Lidarr rarely has. LRCLIB is a free, open collection of lyrics, many of
//! them timed line by line, that needs no account. It's asked by a song's title, artist,
//! album and length, and what it sends back is kept on this PC so each song is asked about
//! once.
//!
//! Two things ask. Opening a song's lyrics asks about that song at once. And a worker goes
//! through the whole library in the background, at the unhurried pace LRCLIB asks of apps
//! that fetch in bulk, so that in time every song's lyrics are here before they're wanted.
//!
//! All of it is off unless "Find lyrics online" is switched on, since it tells LRCLIB what
//! is in the library.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

use crate::subsonic::{LyricLine, Lyrics, Song};

const BASE: &str = "https://lrclib.net/api";
/// The gap kept between songs when going through the library. With the time each question
/// takes that's 50 to 75 questions a minute, under the 90 LRCLIB allows an app that names
/// itself.
const GAP: Duration = Duration::from_millis(750);
/// How long a song LRCLIB had nothing for is left before it's asked about again.
const ASK_AGAIN: i64 = 30 * 24 * 3600;

static DIR: OnceLock<PathBuf> = OnceLock::new();
static ONLINE: AtomicBool = AtomicBool::new(false);
static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

/// How far the worker has got: songs with lyrics kept, songs looked up and found to have
/// none, and songs in the library.
static FOUND: AtomicUsize = AtomicUsize::new(0);
static WITHOUT: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);
/// Set when the library or the setting changes, to have the worker look again.
static WAKE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());
static STARTED: AtomicBool = AtomicBool::new(false);

/// Says where lyrics are kept.
pub fn set_place(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = DIR.set(dir);
}

/// Switches asking LRCLIB on or off. Lyrics already kept are shown either way.
pub fn set_online(on: bool) {
    if ONLINE.swap(on, Ordering::Relaxed) != on {
        wake();
    }
}

/// Has the worker go through the library again, e.g. because it has gained songs.
pub fn wake() {
    let (lock, signal) = &WAKE;
    *lock.lock().unwrap_or_else(|stuck| stuck.into_inner()) = true;
    signal.notify_all();
}

/// How far along the library is: songs with lyrics kept, songs found to have none, and
/// songs in all. All zero until the worker has first looked.
pub fn progress() -> (usize, usize, usize) {
    (FOUND.load(Ordering::Relaxed), WITHOUT.load(Ordering::Relaxed), TOTAL.load(Ordering::Relaxed))
}

fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| crate::subsonic::make_agent_idle(Duration::from_secs(50)))
}

/// A song's name as a file name.
fn safe(song: &str) -> String {
    song.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

/// Where a song's lyrics are kept: timed ones as `.lrc`, plain ones as `.txt`, and a note
/// that there are none as `.none`.
fn file(song: &str, kind: &str) -> Option<PathBuf> {
    DIR.get().map(|dir| dir.join(format!("{}.{kind}", safe(song))))
}

/// The lyrics kept for a song, if any.
pub fn kept(song: &str) -> Option<Lyrics> {
    if let Some(text) = file(song, "lrc").and_then(|path| std::fs::read_to_string(path).ok()) {
        let lyrics = timed(&text);
        if !lyrics.line.is_empty() {
            return Some(lyrics);
        }
    }
    let text = file(song, "txt").and_then(|path| std::fs::read_to_string(path).ok())?;
    let lyrics = plain(&text);
    (!lyrics.line.is_empty()).then_some(lyrics)
}

/// Whether a song was asked about lately and LRCLIB had nothing.
fn known_without(song: &str, now: i64) -> bool {
    file(song, "none").and_then(|path| std::fs::read_to_string(path).ok()).is_some_and(|noted| still_without(&noted, now))
}

/// Whether a note that a song has no lyrics still stands. Its first line is when LRCLIB
/// was asked, or "instrumental", which doesn't go out of date.
fn still_without(noted: &str, now: i64) -> bool {
    let first = noted.lines().next().unwrap_or_default().trim();
    first == "instrumental" || first.parse::<i64>().is_ok_and(|at| now - at < ASK_AGAIN)
}

/// Lyrics as LRCLIB writes timed ones: `[01:23.45] a line`, one to a line.
fn timed(text: &str) -> Lyrics {
    let mut lines: Vec<LyricLine> = Vec::new();
    for row in text.lines() {
        // A line sung more than once can carry several times.
        let mut rest = row.trim();
        let mut starts: Vec<i64> = Vec::new();
        while let Some(end) = rest.strip_prefix('[').and_then(|after| after.find(']')) {
            match stamp(&rest[1..=end]) {
                Some(start) => starts.push(start),
                // "[ar: Someone]" and the like say things about the file, not the song.
                None if starts.is_empty() => break,
                None => {}
            }
            rest = rest[end + 2..].trim_start();
        }
        for start in starts {
            lines.push(LyricLine { start: Some(start), value: rest.to_string() });
        }
    }
    lines.sort_by_key(|line| line.start);
    Lyrics { synced: !lines.is_empty(), line: lines }
}

/// "01:23.45" in milliseconds.
fn stamp(text: &str) -> Option<i64> {
    let (minutes, seconds) = text.split_once(':')?;
    let minutes: i64 = minutes.trim().parse().ok()?;
    let seconds: f64 = seconds.trim().parse().ok()?;
    (minutes >= 0 && (0.0..60.0).contains(&seconds)).then(|| minutes * 60_000 + (seconds * 1000.0).round() as i64)
}

fn plain(text: &str) -> Lyrics {
    let lines: Vec<LyricLine> = text.lines().map(|line| LyricLine { start: None, value: line.trim_end().to_string() }).collect();
    let empty = lines.iter().all(|line| line.value.trim().is_empty());
    Lyrics { synced: false, line: if empty { Vec::new() } else { lines } }
}

/// What LRCLIB said of a song.
#[derive(Debug, PartialEq)]
enum Answer {
    /// Timed lyrics, as they're to be kept.
    Timed(String),
    Plain(String),
    Instrumental,
    Nothing,
}

fn answer(record: &Value) -> Answer {
    let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::trim).filter(|text| !text.is_empty());
    if let Some(lyrics) = text("syncedLyrics") {
        Answer::Timed(lyrics.to_string())
    } else if let Some(lyrics) = text("plainLyrics") {
        Answer::Plain(lyrics.to_string())
    } else if record.get("instrumental").and_then(Value::as_bool) == Some(true) {
        Answer::Instrumental
    } else {
        Answer::Nothing
    }
}

/// Why a question went unanswered: told to wait this many seconds, or couldn't be asked.
enum Trouble {
    Wait(u64),
    Unreachable,
}

fn get(path: &str) -> Result<Option<Value>, Trouble> {
    let response = agent()
        .get(&format!("{BASE}/{path}"))
        .config()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .call()
        .map_err(|_| Trouble::Unreachable)?;
    let status = response.status().as_u16();
    if status == 429 {
        let wait = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()).unwrap_or(30);
        let _ = response.into_body().read_to_string();
        // A little longer than asked, and not the same for every copy of the app.
        return Err(Trouble::Wait(wait + 1 + fastrand::u64(0..=wait / 3 + 1)));
    }
    let text = response.into_body().read_to_string().map_err(|_| Trouble::Unreachable)?;
    match status {
        200 => Ok(serde_json::from_str(&text).ok()),
        404 => Ok(None),
        _ => Err(Trouble::Unreachable),
    }
}

/// Asks LRCLIB for a song's lyrics: by title, artist, album and length, and if it has
/// nothing under that album, by the rest alone. `thorough` goes on to look through
/// whatever it has by that title and artist, for when someone is waiting to read them.
fn ask(song: &Song, thorough: bool) -> Result<Answer, Trouble> {
    let encode = crate::subsonic::encode;
    let by = format!("track_name={}&artist_name={}", encode(&song.title), encode(&song.artist));
    let length = if song.duration > 0 { format!("&duration={}", song.duration) } else { String::new() };
    let mut found = Answer::Nothing;
    if !song.album.is_empty() {
        if let Some(record) = get(&format!("get?{by}&album_name={}{length}", encode(&song.album)))? {
            found = answer(&record);
        }
    }
    if found == Answer::Nothing {
        if !thorough {
            // Going through the library, even a second question about a song waits its turn.
            std::thread::sleep(Duration::from_millis(250));
        }
        if let Some(record) = get(&format!("get?{by}{length}"))? {
            found = answer(&record);
        }
    }
    if found == Answer::Nothing && thorough {
        let records = get(&format!("search?{by}"))?.and_then(|found| found.as_array().cloned()).unwrap_or_default();
        found = nearest(&records, song.duration);
    }
    Ok(found)
}

/// Of several records for a song, the one to use: timed before plain, and the nearest in
/// length. One that's well off in length is another version, whose timing wouldn't fit.
fn nearest(records: &[Value], seconds: u32) -> Answer {
    // With no length to go by, they're all as near as each other and the first is used.
    let off = |record: &Value| if seconds == 0 { 0.0 } else { (record.get("duration").and_then(Value::as_f64).unwrap_or(0.0) - seconds as f64).abs() };
    let mut best: Option<(u8, f64, Answer)> = None;
    for record in records {
        let found = answer(record);
        let rank = match found {
            Answer::Timed(_) => 0,
            Answer::Plain(_) => 1,
            _ => continue,
        };
        let off = off(record);
        // Timed lyrics are only any use if the lengths agree; plain ones allow more.
        if off > if rank == 0 { 3.0 } else { 15.0 } {
            continue;
        }
        if best.as_ref().is_none_or(|(have, was, _)| (rank, off) < (*have, *was)) {
            best = Some((rank, off, found));
        }
    }
    best.map_or(Answer::Nothing, |(_, _, found)| found)
}

/// Keeps what LRCLIB said of a song.
fn keep(song: &str, answer: &Answer, now: i64) {
    let write = |kind: &str, text: &str| {
        if let Some(path) = file(song, kind) {
            let _ = std::fs::write(path, text);
        }
    };
    match answer {
        Answer::Timed(text) => write("lrc", text),
        Answer::Plain(text) => write("txt", text),
        Answer::Instrumental => write("none", "instrumental"),
        Answer::Nothing => write("none", &now.to_string()),
    }
    if matches!(answer, Answer::Timed(_) | Answer::Plain(_)) {
        if let Some(path) = file(song, "none") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// A song's lyrics for showing now: what's kept, or failing that what LRCLIB has, if
/// asking is switched on. Slow when it has to ask.
pub fn for_song(song: &Song) -> Option<Lyrics> {
    if let Some(lyrics) = kept(&song.id) {
        return Some(lyrics);
    }
    let now = chrono::Utc::now().timestamp();
    if !ONLINE.load(Ordering::Relaxed) || song.title.is_empty() || song.artist.is_empty() || !song.radio_url.is_empty() {
        return None;
    }
    // Asked about before in passing and nothing found: look harder now it's wanted, once.
    let looked_hard = file(&song.id, "none").and_then(|path| std::fs::read_to_string(path).ok()).is_some_and(|noted| noted.trim() == "instrumental" || noted.contains("thorough"));
    if looked_hard && known_without(&song.id, now) {
        return None;
    }
    let found = ask(song, true).ok()?;
    keep(&song.id, &found, now);
    if found == Answer::Nothing {
        if let Some(path) = file(&song.id, "none") {
            let _ = std::fs::write(path, format!("{now}\nthorough"));
        }
    }
    kept(&song.id)
}

/// The songs whose lyrics are kept, and those known to have none, by their file names.
fn on_disk(dir: &Path, now: i64) -> (HashSet<String>, HashSet<String>) {
    let (mut found, mut without) = (HashSet::new(), HashSet::new());
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()).map(str::to_string) else { continue };
        match path.extension().and_then(|kind| kind.to_str()) {
            Some("lrc" | "txt") => {
                found.insert(stem);
            }
            Some("none") => {
                if still_without(&std::fs::read_to_string(&path).unwrap_or_default(), now) {
                    without.insert(stem);
                }
            }
            _ => {}
        }
    }
    (found, without)
}

/// Starts the worker that goes through the library, if it isn't going already. It does
/// nothing until asking is switched on and the library is known.
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new().name("lyrics".into()).spawn(|| {
        loop {
            let done = pass();
            // Wait to be told there's more to do; after trouble, try again by and by.
            let (lock, signal) = &WAKE;
            let mut woken = lock.lock().unwrap_or_else(|stuck| stuck.into_inner());
            if !*woken {
                let patience = if done { Duration::from_secs(6 * 3600) } else { Duration::from_secs(300) };
                woken = signal.wait_timeout(woken, patience).map(|(woken, _)| woken).unwrap_or_else(|stuck| stuck.into_inner().0);
            }
            *woken = false;
        }
    });
}

/// Goes through the library once, asking about each song nothing is known of. Says whether
/// it got to the end; it stops early if asking is switched off or LRCLIB can't be reached.
fn pass() -> bool {
    // Switched off: there's nothing to do until it's switched on, which wakes the worker.
    if !ONLINE.load(Ordering::Relaxed) {
        return true;
    }
    let (Some(dir), Some(index)) = (DIR.get(), crate::search::current()) else { return false };
    let now = chrono::Utc::now().timestamp();
    let (found, without) = on_disk(dir, now);
    let songs: Vec<&Song> = index.library.songs.iter().filter(|song| !song.title.is_empty() && !song.artist.is_empty()).collect();
    let have = songs.iter().filter(|song| found.contains(&safe(&song.id))).count();
    let none = songs.iter().filter(|song| without.contains(&safe(&song.id))).count();
    FOUND.store(have, Ordering::Relaxed);
    WITHOUT.store(none, Ordering::Relaxed);
    TOTAL.store(songs.len(), Ordering::Relaxed);
    // The most played first: those are the ones whose lyrics are likeliest to be opened.
    let mut wanted: Vec<&Song> = songs.into_iter().filter(|song| !found.contains(&safe(&song.id)) && !without.contains(&safe(&song.id))).collect();
    wanted.sort_by_key(|song| std::cmp::Reverse(song.play_count));
    let mut failures = 0;
    for song in wanted {
        if !ONLINE.load(Ordering::Relaxed) {
            return false;
        }
        // Opening the song's lyrics may have asked meanwhile.
        if kept(&song.id).is_some() || known_without(&song.id, chrono::Utc::now().timestamp()) {
            continue;
        }
        match ask(song, false) {
            Ok(answer) => {
                failures = 0;
                keep(&song.id, &answer, chrono::Utc::now().timestamp());
                let counter = if matches!(answer, Answer::Timed(_) | Answer::Plain(_)) { &FOUND } else { &WITHOUT };
                counter.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(GAP);
            }
            Err(Trouble::Wait(seconds)) => std::thread::sleep(Duration::from_secs(seconds.min(600))),
            Err(Trouble::Unreachable) => {
                // No connection, most likely. Give up for now rather than hammer at it.
                failures += 1;
                if failures >= 3 {
                    return false;
                }
                std::thread::sleep(Duration::from_secs(20));
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_timed_lyrics() {
        let lyrics = timed("[ar: Someone]\n[00:12.50] First line\n[01:02.345]Second\n[00:40.00][02:00.00] Chorus\n[00:20.00]\nnot timed\n");
        assert!(lyrics.synced);
        let lines: Vec<(i64, &str)> = lyrics.line.iter().map(|line| (line.start.unwrap(), line.value.as_str())).collect();
        assert_eq!(lines, [(12_500, "First line"), (20_000, ""), (40_000, "Chorus"), (62_345, "Second"), (120_000, "Chorus")]);
        assert!(timed("just words\nno times").line.is_empty());
        assert_eq!(stamp("3:07"), Some(187_000));
        assert_eq!(stamp("ar: x"), None);
        assert!(still_without("instrumental", 0) && still_without("100\nthorough", 100 + ASK_AGAIN - 1) && !still_without("100", 100 + ASK_AGAIN));

        let words = plain("One\n\nTwo  \n");
        assert_eq!(words.line.iter().map(|line| line.value.as_str()).collect::<Vec<_>>(), ["One", "", "Two"]);
        assert!(!words.synced && plain(" \n\n").line.is_empty());
    }

    #[test]
    fn picks_among_what_lrclib_has() {
        let record = |seconds: f64, timed: Option<&str>, plain: Option<&str>| serde_json::json!({ "duration": seconds, "syncedLyrics": timed, "plainLyrics": plain, "instrumental": false });
        assert_eq!(answer(&record(200.0, Some("[00:01.00] a"), Some("a"))), Answer::Timed("[00:01.00] a".into()));
        assert_eq!(answer(&record(200.0, Some("  "), Some("a"))), Answer::Plain("a".into()));
        assert_eq!(answer(&serde_json::json!({ "instrumental": true, "syncedLyrics": null })), Answer::Instrumental);
        assert_eq!(answer(&record(200.0, None, None)), Answer::Nothing);

        let records = [record(260.0, Some("[00:01.00] live"), None), record(203.0, None, Some("words")), record(201.0, Some("[00:01.00] album"), None), record(200.0, Some("[00:01.00] exact"), None)];
        // Timed and nearest in length; timed lyrics of another version aren't used.
        assert_eq!(nearest(&records, 200), Answer::Timed("[00:01.00] exact".into()));
        assert_eq!(nearest(&records[..2], 200), Answer::Plain("words".into()));
        assert_eq!(nearest(&records[..1], 200), Answer::Nothing);
        // With no length to go by, the first timed one.
        assert_eq!(nearest(&records, 0), Answer::Timed("[00:01.00] live".into()));
    }
}
