//! Where things live on disk, saved settings, and the stored password.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::subsonic::Song;

pub struct Paths {
    pub settings: PathBuf,
    pub queue: PathBuf,
    pub history: PathBuf,
    pub covers: PathBuf,
    pub pages: PathBuf,
    pub buffers: PathBuf,
    #[cfg(not(windows))]
    secret: PathBuf,
}

impl Paths {
    pub fn new() -> Paths {
        let (config, cache) = match directories::ProjectDirs::from("", "", "Nexus Music") {
            Some(dirs) => (dirs.config_dir().to_path_buf(), dirs.cache_dir().to_path_buf()),
            None => (PathBuf::from("."), std::env::temp_dir().join("nexus-music")),
        };
        // An override so tests and second profiles don't touch the real one.
        let (config, cache) = match std::env::var_os("NEXUS_MUSIC_HOME") {
            Some(home) => (PathBuf::from(&home).join("config"), PathBuf::from(&home).join("cache")),
            None => (config, cache),
        };
        let paths = Paths {
            settings: config.join("settings.json"),
            queue: config.join("queue.json"),
            history: config.join("history.json"),
            covers: cache.join("covers"),
            pages: cache.join("pages"),
            buffers: cache.join("buffers"),
            #[cfg(not(windows))]
            secret: config.join("credential"),
        };
        for dir in [&config, &paths.covers, &paths.pages, &paths.buffers] {
            let _ = fs::create_dir_all(dir);
        }
        paths
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server: String,
    pub username: String,
    pub volume: f32,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: u8,
    /// Bitrate cap in kbps sent to the server; 0 streams the original files.
    pub bitrate: u32,
    pub normalize: bool,
    /// Loudness target when evening out volume: 0 quiet, 1 normal, 2 loud.
    pub volume_level: u8,
    pub scrobble: bool,
    /// Whether the queue panel is showing. (Named apart from the old `queue_open`, which was
    /// never saved, so the panel starts open for everyone.)
    pub queue_panel: bool,
    /// Seconds of overlap between tracks; 0 for none.
    pub crossfade: f32,
    pub gapless: bool,
    /// Keep playing similar music when the queue runs out.
    pub autoplay: bool,
    pub mono: bool,
    pub equalizer: bool,
    /// Gain in dB for each equaliser band.
    pub eq_gains: [f32; 6],
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            server: String::new(),
            username: String::new(),
            volume: 0.8,
            muted: false,
            shuffle: false,
            repeat: 0,
            bitrate: 0,
            normalize: false,
            volume_level: 1,
            scrobble: true,
            queue_panel: true,
            crossfade: 0.0,
            gapless: true,
            autoplay: true,
            mono: false,
            equalizer: false,
            eq_gains: [0.0; 6],
        }
    }
}

/// Streaming quality choices: name, what it costs, and the bitrate cap sent to the server
/// (0 = the original file).
pub const QUALITIES: [(&str, &str, u32); 4] = [
    ("Low", "96 kbps MP3, about 0.04 GB an hour", 96),
    ("Normal", "160 kbps MP3, about 0.07 GB an hour", 160),
    ("High", "320 kbps MP3, about 0.14 GB an hour", 320),
    (
        "Lossless",
        "Your files exactly as stored, at their own sample rate and bit depth. \
         About 0.4 GB an hour for CD-quality FLAC, more for hi-res.",
        0,
    ),
];

/// Loudness targets for evening out volume: name and the offset in dB from the ReplayGain
/// reference level (which sits between Quiet and Normal).
pub const VOLUME_LEVELS: [(&str, f32); 3] = [("Quiet", -1.0), ("Normal", 4.0), ("Loud", 7.0)];

/// Equaliser presets: name and the gain in dB for each of the six bands.
pub const EQ_PRESETS: [(&str, [f32; 6]); 10] = [
    ("Flat", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Bass boost", [6.0, 4.0, 0.0, 0.0, 0.0, 0.0]),
    ("Bass reduce", [-6.0, -4.0, 0.0, 0.0, 0.0, 0.0]),
    ("Treble boost", [0.0, 0.0, 0.0, 1.0, 4.0, 6.0]),
    ("Vocal", [-2.0, -1.0, 1.0, 4.0, 3.0, 0.0]),
    ("Rock", [4.0, 2.0, -2.0, -1.0, 2.0, 4.0]),
    ("Pop", [-1.0, 1.0, 3.0, 3.0, 1.0, -1.0]),
    ("Jazz", [3.0, 1.0, -1.0, 1.0, 2.0, 3.0]),
    ("Classical", [3.0, 1.0, 0.0, 0.0, 1.0, 3.0]),
    ("Electronic", [5.0, 3.0, 0.0, -1.0, 2.0, 4.0]),
];

impl Settings {
    pub fn load(path: &Path) -> Settings {
        fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            write_atomically(path, &json);
        }
    }

    pub fn max_bitrate(&self) -> u32 {
        self.bitrate
    }

    /// The name of the preset matching the current equaliser curve, if any.
    pub fn eq_preset(&self) -> &'static str {
        EQ_PRESETS
            .iter()
            .find(|(_, gains)| gains.iter().zip(self.eq_gains).all(|(a, b)| (a - b).abs() < 0.05))
            .map(|(name, _)| *name)
            .unwrap_or("Custom")
    }
}

/// The play queue as it was when the app last closed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedQueue {
    pub songs: Vec<Song>,
    pub order: Vec<usize>,
    pub cursor: usize,
    pub position: f64,
    pub context: String,
}

impl SavedQueue {
    pub fn load(path: &Path) -> Option<SavedQueue> {
        let queue: SavedQueue = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        let valid = !queue.songs.is_empty()
            && queue.order.len() == queue.songs.len()
            && queue.cursor < queue.order.len()
            && queue.order.iter().all(|&i| i < queue.songs.len());
        valid.then_some(queue)
    }

    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_vec(self) {
            write_atomically(path, &json);
        }
    }
}

/// Songs that have been played, newest first.
pub fn load_history(path: &Path) -> Vec<Song> {
    fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub fn save_history(path: &Path, songs: &[Song]) {
    if let Ok(json) = serde_json::to_vec(songs) {
        write_atomically(path, &json);
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) {
    let temp = path.with_extension("tmp");
    if fs::write(&temp, bytes).is_ok() {
        let _ = fs::rename(&temp, path);
    }
}

const KEYRING_SERVICE: &str = "Nexus Music";

fn account(server: &str, username: &str) -> String {
    format!("{username}@{server}")
}

// On Windows the password goes to Credential Manager. Other platforms are only used for
// development, and keep it in a file readable by the current user.

#[cfg(windows)]
pub fn save_password(_paths: &Paths, server: &str, username: &str, password: &str) -> bool {
    keyring::Entry::new(KEYRING_SERVICE, &account(server, username))
        .and_then(|entry| entry.set_password(password))
        .is_ok()
}

#[cfg(windows)]
pub fn load_password(_paths: &Paths, server: &str, username: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, &account(server, username)).ok()?.get_password().ok()
}

#[cfg(windows)]
pub fn delete_password(_paths: &Paths, server: &str, username: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &account(server, username)) {
        let _ = entry.delete_credential();
    }
}

#[cfg(not(windows))]
pub fn save_password(paths: &Paths, server: &str, username: &str, password: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let body = format!("{KEYRING_SERVICE}\n{}\n{password}", account(server, username));
    if fs::write(&paths.secret, body).is_err() {
        return false;
    }
    let _ = fs::set_permissions(&paths.secret, fs::Permissions::from_mode(0o600));
    true
}

#[cfg(not(windows))]
pub fn load_password(paths: &Paths, server: &str, username: &str) -> Option<String> {
    let body = fs::read_to_string(&paths.secret).ok()?;
    let mut lines = body.splitn(3, '\n');
    let (_, who, password) = (lines.next()?, lines.next()?, lines.next()?);
    (who == account(server, username)).then(|| password.to_string())
}

#[cfg(not(windows))]
pub fn delete_password(paths: &Paths, _server: &str, _username: &str) {
    let _ = fs::remove_file(&paths.secret);
}

/// Total size of the files directly inside `dir`, in bytes.
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}
