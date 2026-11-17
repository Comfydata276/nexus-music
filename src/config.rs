//! Where things live on disk, saved settings, and the stored password.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::subsonic::Song;

pub struct Paths {
    pub settings: PathBuf,
    pub queue: PathBuf,
    pub history: PathBuf,
    /// Likes, ratings and plays waiting to be sent to the server.
    pub pending: PathBuf,
    pub covers: PathBuf,
    pub pages: PathBuf,
    pub buffers: PathBuf,
    /// Songs saved for listening without the server.
    pub downloads: PathBuf,
    /// When songs were added to playlists, as far as this app has seen.
    pub added: PathBuf,
    #[cfg(not(windows))]
    secret: PathBuf,
}

impl Paths {
    pub fn new() -> Paths {
        let (config, cache, data) = match directories::ProjectDirs::from("", "", "Nexus Music") {
            Some(dirs) => (
                dirs.config_dir().to_path_buf(),
                dirs.cache_dir().to_path_buf(),
                dirs.data_local_dir().to_path_buf(),
            ),
            None => (PathBuf::from("."), std::env::temp_dir().join("nexus-music"), PathBuf::from(".")),
        };
        // An override so tests and second profiles don't touch the real one.
        let (config, cache, data) = match std::env::var_os("NEXUS_MUSIC_HOME") {
            Some(home) => (
                PathBuf::from(&home).join("config"),
                PathBuf::from(&home).join("cache"),
                PathBuf::from(&home).join("data"),
            ),
            None => (config, cache, data),
        };
        let paths = Paths {
            settings: config.join("settings.json"),
            queue: config.join("queue.json"),
            history: config.join("history.json"),
            pending: config.join("pending.json"),
            added: config.join("playlist-added.json"),
            covers: cache.join("covers"),
            pages: cache.join("pages"),
            buffers: cache.join("buffers"),
            downloads: data.join("downloads"),
            #[cfg(not(windows))]
            secret: config.join("credential"),
        };
        for dir in [&config, &paths.covers, &paths.pages, &paths.buffers] {
            let _ = fs::create_dir_all(dir);
        }
        paths
    }
}

/// What's remembered about one list of songs (a playlist, an album, Liked songs).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListPrefs {
    /// Play it shuffled.
    pub shuffle: bool,
    /// The order it's shown in (an index into `TRACK_SORTS`), and whether that's reversed.
    pub sort: usize,
    pub descending: bool,
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
    /// Which ReplayGain value evens out volume: 0 decides by what's playing, 1 each song's
    /// own, 2 its album's.
    pub level_mode: u8,
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
    /// The device to play through (a cpal device id); empty follows the system default.
    pub output_device: String,
    pub output_device_name: String,
    /// Take the device for this app alone and play each song at its own sample rate.
    pub exclusive: bool,
    /// Bitrate cap in kbps for new downloads; 0 saves the original files.
    pub download_quality: u32,
    /// Playlists kept at the top of the sidebar, in the order they were pinned.
    pub pinned_playlists: Vec<String>,
    /// Choices remembered for individual playlists, albums and so on, by route key.
    pub lists: std::collections::HashMap<String, ListPrefs>,
    /// Offline mode: don't contact the server; play downloads only.
    pub offline: bool,
    /// Keep the title bar Windows draws, instead of the app's own.
    pub standard_title_bar: bool,
    /// Name of the colour theme (see `THEMES`).
    pub theme: String,
    /// Size of the interface relative to the system's scaling; 1.0 leaves it alone.
    pub ui_scale: f32,
    /// Closing the window leaves the app running in the notification area.
    pub close_to_tray: bool,
    pub check_updates: bool,
    /// The newest version the user has already been told about.
    pub update_notified: String,
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
            level_mode: 0,
            scrobble: true,
            queue_panel: true,
            crossfade: 0.0,
            gapless: true,
            autoplay: true,
            mono: false,
            equalizer: false,
            eq_gains: [0.0; 6],
            output_device: String::new(),
            output_device_name: String::new(),
            exclusive: false,
            download_quality: 0,
            pinned_playlists: Vec::new(),
            lists: std::collections::HashMap::new(),
            offline: false,
            standard_title_bar: false,
            theme: THEMES[0].name.to_string(),
            ui_scale: 1.0,
            close_to_tray: true,
            check_updates: true,
            update_notified: String::new(),
        }
    }
}

/// A colour theme: surfaces from darkest to lightest, text from strongest to faintest, and
/// the accent. Colours are 0xRRGGBB.
pub struct ThemeColors {
    pub name: &'static str,
    pub base: u32,
    pub panel: u32,
    pub raised: u32,
    pub overlay: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    pub accent: u32,
    pub accent_bright: u32,
    /// Text and icons drawn on top of the accent.
    pub on_accent: u32,
    /// Marks things you've liked.
    pub warm: u32,
}

pub const THEMES: [ThemeColors; 6] = [
    ThemeColors {
        name: "Violet",
        base: 0x0b0811, panel: 0x14101d, raised: 0x1e182b, overlay: 0x251e35,
        text: 0xf3effa, muted: 0xa89fbb, faint: 0x6f6783,
        accent: 0x8b5cf6, accent_bright: 0xa98bfb, on_accent: 0x150c2b, warm: 0xf5b85c,
    },
    ThemeColors {
        name: "Green",
        base: 0x000000, panel: 0x121212, raised: 0x1f1f1f, overlay: 0x2a2a2a,
        text: 0xffffff, muted: 0xb3b3b3, faint: 0x6a6a6a,
        accent: 0x1db954, accent_bright: 0x1ed760, on_accent: 0x04130a, warm: 0x1ed760,
    },
    ThemeColors {
        name: "Ember",
        base: 0x0e0907, panel: 0x17100c, raised: 0x241811, overlay: 0x2f2017,
        text: 0xfbf3ec, muted: 0xc2ab98, faint: 0x7f6b5c,
        accent: 0xf97316, accent_bright: 0xfb923c, on_accent: 0x2a1203, warm: 0xfbbf24,
    },
    ThemeColors {
        name: "Ocean",
        base: 0x070b12, panel: 0x0e1420, raised: 0x172033, overlay: 0x1e2a42,
        text: 0xeef3fb, muted: 0x9fb0c9, faint: 0x647692,
        accent: 0x3b82f6, accent_bright: 0x60a5fa, on_accent: 0x06142e, warm: 0xf5b85c,
    },
    ThemeColors {
        name: "Rose",
        base: 0x110810, panel: 0x1b0f18, raised: 0x291724, overlay: 0x341d2e,
        text: 0xfbeff6, muted: 0xc4a4b7, faint: 0x866b7b,
        accent: 0xec4899, accent_bright: 0xf472b6, on_accent: 0x2d0a1c, warm: 0xf5b85c,
    },
    ThemeColors {
        name: "Graphite",
        base: 0x0a0a0b, panel: 0x131315, raised: 0x1e1e21, overlay: 0x28282c,
        text: 0xf4f4f5, muted: 0xa1a1aa, faint: 0x6b6b74,
        accent: 0xd4d4d8, accent_bright: 0xf4f4f5, on_accent: 0x18181b, warm: 0xf5b85c,
    },
];

/// Interface sizes offered in Settings: label and scale.
pub const UI_SCALES: [(&str, f32); 4] = [("Small", 0.9), ("Normal", 1.0), ("Large", 1.12), ("Larger", 1.25)];

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

/// How volume is evened out: by song, by album, or whichever suits what's playing.
pub const LEVEL_MODES: [&str; 3] = ["Auto", "Song", "Album"];

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

    pub fn theme(&self) -> &'static ThemeColors {
        THEMES.iter().find(|t| t.name == self.theme).unwrap_or(&THEMES[0])
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
    /// The page the queue was started from, and each song's place in that page's list.
    pub source: String,
    pub origins: Vec<Option<usize>>,
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
