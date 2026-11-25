//! Where things live on disk, saved settings, and the stored password.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::subsonic::Song;

#[derive(Clone)]
pub struct Paths {
    pub settings: PathBuf,
    pub queue: PathBuf,
    /// Where in the queue playback has got to. Kept apart from the queue itself, which is
    /// large and changes far less often.
    pub queue_place: PathBuf,
    /// Where exported playlists go when there's no system window to ask where.
    pub exports: PathBuf,
    /// What went wrong the last time the app stopped by itself, and the file "Save
    /// diagnostics" writes.
    pub crash_log: PathBuf,
    pub diagnostics: PathBuf,
    pub history: PathBuf,
    /// Likes, ratings and plays waiting to be sent to the server.
    pub pending: PathBuf,
    pub covers: PathBuf,
    pub pages: PathBuf,
    pub buffers: PathBuf,
    /// Songs kept as they're played and fetched ahead (see `songcache`).
    pub songs: PathBuf,
    /// Songs saved for listening without the server.
    pub downloads: PathBuf,
    /// When songs were added to playlists, as far as this app has seen.
    pub added: PathBuf,
    /// What's been asked for through this app and is being watched for.
    pub asked: PathBuf,
    /// A batch of albums being asked for, so it can be carried on after a restart (see `bulk`).
    pub asking: PathBuf,
    /// Artists worth a look, as last worked out (see `discover`).
    pub discover: PathBuf,
    /// Where version 0.22 kept lyrics it fetched itself. Lyrics now come from the server
    /// alone, and this is cleared away at startup.
    pub old_lyrics: PathBuf,
    /// Every play and when it happened (see `plays`).
    pub plays: PathBuf,
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
            queue_place: config.join("queue-place.json"),
            exports: data.join("exports"),
            crash_log: data.join("crash.log"),
            diagnostics: data.join("diagnostics.txt"),
            history: config.join("history.json"),
            pending: config.join("pending.json"),
            added: config.join("playlist-added.json"),
            asked: config.join("asked.json"),
            asking: config.join("asking.json"),
            discover: cache.join("discover.json"),
            covers: cache.join("covers"),
            pages: cache.join("pages"),
            buffers: cache.join("buffers"),
            songs: cache.join("songs"),
            downloads: data.join("downloads"),
            old_lyrics: data.join("lyrics"),
            plays: config.join("plays.json"),
            #[cfg(not(windows))]
            secret: config.join("credential"),
        };
        for dir in [&config, &paths.covers, &paths.pages, &paths.buffers, &paths.songs] {
            let _ = fs::create_dir_all(dir);
        }
        paths
    }
}

pub use nexus_core::sync::ListPrefs;

/// One of the person's other devices, as the new linking last saw it (see `link`): the id
/// it gives itself, the name the person gave it, and the day it was last seen (days since
/// 1970), after which it is in time forgotten. None of it is secret.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkSeen {
    pub id: String,
    pub name: String,
    pub day: u64,
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
    /// How much songs with no levelling tags are turned down while levelling is on, in dB
    /// (0 or negative). Tagged songs are usually turned down by about this much.
    pub untagged_db: f32,
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
    /// Turns everything down before the equaliser, in dB (0 or negative), to leave room for
    /// bands that are turned up. Anything above 0 stands for Auto (see `eq_preamp_now`).
    pub eq_preamp: f32,
    /// Mixes made to rules of your own (see `mixes::Rules`).
    pub rule_mixes: Vec<crate::mixes::Rules>,
    /// Equaliser curves saved under names of your own.
    pub eq_saved: Vec<(String, [f32; 6])>,
    /// The device to play through (a cpal device id); empty follows the system default.
    pub output_device: String,
    pub output_device_name: String,
    /// Take the device for this app alone and play each song at its own sample rate.
    pub exclusive: bool,
    /// Bitrate cap in kbps for new downloads; 0 saves the original files.
    pub download_quality: u32,
    /// The folder downloads are kept in; empty for the app's own.
    pub downloads_dir: String,
    /// The folder songs are kept in as they play; empty for the app's own. Read when the app
    /// starts.
    pub songs_dir: String,
    /// Playlists kept at the top of the sidebar, in the order they were pinned.
    pub pinned_playlists: Vec<String>,
    /// Choices remembered for individual playlists, albums and so on, by route key.
    pub lists: std::collections::HashMap<String, ListPrefs>,
    /// Offer to ask a Lidarr server for albums that aren't in the library; where it is, and
    /// the name for a sign-in in front of it, if any. (Its key and that sign-in's password
    /// are kept with the account password, not here.)
    /// Space allowed for songs kept as they're played, in megabytes; 0 keeps none. And how
    /// many songs past the one playing are fetched ahead of time.
    pub cache_limit_mb: u32,
    pub cache_ahead: u32,
    pub requests: bool,
    /// Songs that never came out on an album are offered too, on their EP or single.
    pub request_singles: bool,
    pub lidarr_address: String,
    /// The sign-in service in front of Lidarr, if there is one: where it is and the name it
    /// knows Lidarr's sign-in by. Found by asking, not typed in.
    pub lidarr_gate: String,
    pub lidarr_gate_client: String,
    /// Who is signed in there, for showing. The token itself is kept with the passwords.
    pub lidarr_signed_in: String,
    /// Hosting jams is switched on, and the relay they're hosted at. (The host key for it is
    /// kept with the account password, not here.)
    pub jam_host: bool,
    pub jam_relay: String,
    /// The name to go by in jams; empty for the account's name.
    pub jam_name: String,
    /// Guests in a jam hosted here may pause and skip.
    pub jam_guests_control: bool,
    /// Other copies of the app signed in to the same account can see this one, steer it,
    /// and take over what it's playing (and it them). Needs the relay.
    pub link_devices: bool,
    /// What this device is called in other devices' lists; empty for the computer's name.
    pub device_name: String,
    /// Share the queue, pins and so on with the app on other devices.
    pub sync: bool,
    /// A made-up name for this device, so it can tell its own saved queue from others'.
    pub device_id: String,
    /// What this installation calls itself to the person's other devices under the new
    /// linking (see `link`): sixteen random bytes. Made the first time it is needed.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub link_device: String,
    /// The person's other devices as the new linking has seen them, so that the list can
    /// show them at once. Written only where that linking is switched on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub link_seen: Vec<LinkSeen>,
    /// The id of the playlist that holds what's shared between devices, once known.
    pub sync_playlist: String,
    /// When the pins and the theme that were last agreed with the server were chosen (see
    /// `choices`; kept beside them so a version before this one reads the file as its own).
    pub pins_at: i64,
    pub theme_at: i64,
    /// What this PC holds of the choices shared between devices (the pins, each list's
    /// shuffle and sort, the theme): what was last agreed with the server, and the changes
    /// made here since, each kept as the change (see `nexus_core::step`). What is shown,
    /// and what the rest of the app reads, is the two together: `pinned_playlists`, `lists`
    /// and `theme` above, brought up to date by `show`.
    pub choices: nexus_core::step::Choices,
    /// 1 once `choices` has been filled in. A file from a version that stamped whole values
    /// has none: its choices are then agreed, at the times they have (see `arrange`).
    pub in_step: u8,
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
            untagged_db: -6.0,
            scrobble: true,
            queue_panel: true,
            crossfade: 0.0,
            gapless: true,
            autoplay: true,
            mono: false,
            equalizer: false,
            eq_gains: [0.0; 6],
            eq_preamp: 0.0,
            eq_saved: Vec::new(),
            rule_mixes: Vec::new(),
            output_device: String::new(),
            output_device_name: String::new(),
            exclusive: false,
            download_quality: 0,
            downloads_dir: String::new(),
            songs_dir: String::new(),
            pinned_playlists: Vec::new(),
            lists: std::collections::HashMap::new(),
            cache_limit_mb: 2048,
            cache_ahead: 3,
            requests: false,
            request_singles: true,
            lidarr_address: String::new(),
            lidarr_gate: String::new(),
            lidarr_gate_client: String::new(),
            lidarr_signed_in: String::new(),
            jam_host: false,
            jam_relay: String::new(),
            jam_name: String::new(),
            jam_guests_control: true,
            link_devices: true,
            device_name: String::new(),
            sync: true,
            device_id: String::new(),
            link_device: String::new(),
            link_seen: Vec::new(),
            sync_playlist: String::new(),
            pins_at: 0,
            theme_at: 0,
            choices: Default::default(),
            in_step: 0,
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
/// Choices for how much space kept songs may take, in megabytes. The first keeps none.
pub const CACHE_LIMITS: [(&str, u32); 6] = [("Off", 0), ("1 GB", 1024), ("2 GB", 2048), ("5 GB", 5120), ("10 GB", 10240), ("20 GB", 20480)];
/// Choices for how many songs to fetch ahead of the one playing.
pub const CACHE_AHEAD: [u32; 5] = [0, 1, 3, 5, 10];
pub const CACHE_AHEAD_NAMES: [&str; 5] = ["Off", "1", "3", "5", "10"];

pub const UI_SCALES: [(&str, f32); 4] = [("Small", 0.9), ("Normal", 1.0), ("Large", 1.12), ("Larger", 1.25)];

/// The equaliser preamp choices: the name, and the amount in dB (1 for Auto).
pub const PREAMPS: [(&str, f32); 6] = [("Auto", 1.0), ("Off", 0.0), ("3 dB", -3.0), ("6 dB", -6.0), ("9 dB", -9.0), ("12 dB", -12.0)];

/// How much to turn down songs that have no levelling tags: the name, and the amount in dB.
pub const UNTAGGED_LEVELS: [(&str, f32); 4] = [("Leave", 0.0), ("3 dB", -3.0), ("6 dB", -6.0), ("9 dB", -9.0)];

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
    /// Reads the settings, for a look at one of them. See `load_checked`.
    pub fn load(path: &Path) -> Settings {
        Settings::read(path).or_else(|| Settings::read(&backup_of(path))).unwrap_or_default()
    }

    fn read(path: &Path) -> Option<Settings> {
        let mut settings: Settings = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        settings.arrange();
        Some(settings)
    }

    /// Brings a file from a version that stamped whole values (0.40.4 and before) under the
    /// present arrangement: every choice already in it is "agreed", at the time it has, or
    /// "long ago" where it has none. Nothing is lost and nothing is given a new time; what
    /// is shown is what was. Done once (`in_step`).
    fn arrange(&mut self) {
        if self.in_step >= 1 {
            return;
        }
        let agreed = nexus_core::step::Agreed::from_stamped(&self.pinned_playlists, self.pins_at, &self.theme, self.theme_at, &self.lists, THEMES[0].name);
        self.choices = nexus_core::step::Choices { agreed, ..Default::default() };
        self.in_step = 1;
        self.show();
    }

    /// The sign-in these settings hold: the server's address as the app keeps it, and the
    /// username, each exactly as it is. Two sign-ins are the same account only when this is
    /// the same text. A change the person makes is made under it and is only ever shared
    /// for it (see `nexus_core::step`). (An address can't hold a line end, so the two parts
    /// can't run into each other.)
    pub fn account(&self) -> String {
        format!("{}\n{}", self.server, self.username)
    }

    /// That account as part of a file's name: as much of it as reads easily, and then a
    /// short hash of exactly what it is, so that no two accounts share a file, on a disk
    /// that ignores case either.
    pub fn account_file(&self) -> String {
        use sha2::{Digest, Sha256};
        let host = self.server.trim_start_matches("https://").trim_start_matches("http://");
        let mut readable = String::new();
        for c in format!("{host} {}", self.username).chars() {
            match c.to_ascii_lowercase() {
                c if c.is_ascii_lowercase() || c.is_ascii_digit() => readable.push(c),
                _ if readable.ends_with('-') || readable.is_empty() => {}
                _ => readable.push('-'),
            }
        }
        let readable: String = readable.chars().take(40).collect();
        let hash: String = Sha256::digest(self.account().as_bytes()).iter().take(8).map(|byte| format!("{byte:02x}")).collect();
        format!("{}-{hash}", readable.trim_end_matches('-'))
    }

    /// Brings what is shown up to date with `choices`: what was agreed with the server,
    /// with the changes made here, under this sign-in, on top.
    pub fn show(&mut self) {
        let shown = self.choices.shown(&self.account());
        self.pinned_playlists = shown.pins;
        self.lists = shown.lists;
        // (A theme this version doesn't have, chosen on a device that does, is left to it.)
        if THEMES.iter().any(|theme| theme.name == shown.theme) {
            self.theme = shown.theme;
        }
        self.pins_at = self.choices.agreed.pins_at;
        self.theme_at = self.choices.agreed.theme_at;
    }

    /// Reads the settings when the app starts. A file that can't be read (cut short by a
    /// power cut, edited by hand into something else) is never quietly replaced by a fresh
    /// one: the copy kept from the last good start is used, and failing that the damaged
    /// file is put aside under another name. The second value says so when either happened.
    pub fn load_checked(path: &Path) -> (Settings, Option<String>) {
        let backup = backup_of(path);
        let fresh = || {
            let mut settings = Settings::default();
            settings.arrange();
            settings
        };
        if !path.exists() {
            // A first run, or the file was deleted to start afresh.
            return (fresh(), None);
        }
        if let Some(settings) = Settings::read(path) {
            // Good: this is the copy to fall back on next time.
            let _ = fs::copy(path, &backup);
            return (settings, None);
        }
        if let Some(settings) = Settings::read(&backup) {
            let _ = fs::copy(&backup, path);
            return (settings, Some("Your settings file was damaged, so the copy from the last time the app started was used.".into()));
        }
        let aside = path.with_extension("damaged.json");
        let _ = fs::rename(path, &aside);
        let note = format!(
            "Your settings file couldn't be read and there was no copy to fall back on. It has been kept as {}, and the app has started with its default settings.",
            aside.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default()
        );
        (fresh(), Some(note))
    }

    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            write_atomically(path, &json);
        }
    }

    /// Saves the settings and says whether they are on the disk: for a change that must be
    /// kept before it is shown as made (see `Controller::choose`).
    pub fn save_surely(&self, path: &Path) -> bool {
        serde_json::to_vec_pretty(self).is_ok_and(|json| write_surely(path, &json).is_ok())
    }

    pub fn theme(&self) -> &'static ThemeColors {
        THEMES.iter().find(|t| t.name == self.theme).unwrap_or(&THEMES[0])
    }

    pub fn max_bitrate(&self) -> u32 {
        self.bitrate
    }

    /// The name of the preset matching the current equaliser curve: one of the app's, one
    /// of your own, or "Custom" for neither.
    pub fn eq_preset(&self) -> String {
        let same = |gains: &[f32; 6]| gains.iter().zip(self.eq_gains).all(|(a, b)| (a - b).abs() < 0.05);
        let built_in = EQ_PRESETS.iter().find(|(_, gains)| same(gains)).map(|(name, _)| name.to_string());
        built_in.or_else(|| self.eq_saved.iter().find(|(_, gains)| same(gains)).map(|(name, _)| name.clone())).unwrap_or_else(|| "Custom".into())
    }

    /// The preamp in use, in dB: the amount set, or for Auto exactly enough room for the
    /// band turned up the most.
    pub fn eq_preamp_now(&self) -> f32 {
        if self.eq_preamp > 0.0 { -self.eq_gains.iter().fold(0.0f32, |most, gain| most.max(*gain)) } else { self.eq_preamp }
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
    /// When the queue last changed, in seconds since 1970 (see `sync`).
    pub stamp: i64,
    /// The ids of every song in the list the queue was started from, as last seen.
    pub source_list: Vec<String>,
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
}

/// Where the copy of the settings from the last good start is kept.
fn backup_of(path: &Path) -> PathBuf {
    path.with_extension("backup.json")
}

/// Where playback has got to in the saved queue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueuePlace {
    /// Which queue this belongs to (see `queue_shape`); a place saved for a different queue
    /// is ignored.
    pub shape: u64,
    pub cursor: usize,
    pub position: f64,
    pub stamp: i64,
    /// Set while the queue is one taken up from another device with nothing done to it here
    /// since: when that device saved it, as the server said (see `nexus_core::handover`).
    /// Kept so that a restart doesn't make the queue this PC's own.
    pub taken: Option<String>,
}

impl QueuePlace {
    pub fn load(path: &Path) -> Option<QueuePlace> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }
}

/// A number that changes whenever what's in the queue or its order does, and not when
/// playback only moves along it. The same from one run of the app to the next.
pub fn queue_shape(
    songs: &[Song],
    order: &[usize],
    context: &str,
    source: &str,
    origins: &[Option<usize>],
    source_list: &[String],
) -> u64 {
    // FNV-1a, written out so the result doesn't depend on the standard library's hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for byte in bytes.iter().chain(&[0xff]) {
            hash = (hash ^ *byte as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for song in songs {
        mix(song.id.as_bytes());
    }
    for place in order {
        mix(&(*place as u64).to_le_bytes());
    }
    mix(context.as_bytes());
    mix(source.as_bytes());
    for origin in origins {
        mix(&origin.map_or(u64::MAX, |at| at as u64).to_le_bytes());
    }
    for id in source_list {
        mix(id.as_bytes());
    }
    hash
}

/// A song in "Recently played", and when it was played (seconds since 1970).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Played {
    pub song: Song,
    pub at: i64,
}

/// Songs that have been played, newest first.
pub fn load_history(path: &Path) -> Vec<Played> {
    let bytes = fs::read(path).unwrap_or_default();
    if let Ok(history) = serde_json::from_slice::<Vec<Played>>(&bytes) {
        return history;
    }
    // The file as it was before plays carried a time: give them made-up ones, in order.
    let songs: Vec<Song> = serde_json::from_slice(&bytes).unwrap_or_default();
    let newest = fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    songs.into_iter().enumerate().map(|(index, song)| Played { song, at: newest - 200 * index as i64 }).collect()
}

pub fn save_history(path: &Path, history: &[Played]) {
    if let Ok(json) = serde_json::to_vec(history) {
        write_atomically(path, &json);
    }
}

pub fn write_atomically(path: &Path, bytes: &[u8]) {
    let temp = path.with_extension("tmp");
    if fs::write(&temp, bytes).is_ok() {
        let _ = fs::rename(&temp, path);
    }
}

/// As `write_atomically`, for what has to be known to be on the disk: written, flushed to
/// the disk, and put in place, or an error and the file as it was.
pub fn write_surely(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let temp = beside(path);
    let mut file = fs::File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, path)
}

/// The name beside a file's that it is written under before it is put in place.
pub fn beside(path: &Path) -> PathBuf {
    path.with_extension("tmp")
}

/// As `write_surely`, for a file that is made once and never replaced: written beside its
/// place, flushed to the disk, and only then given its name, if nothing has that name; then
/// its folder is synced. So whatever is at the name is the whole file, or nothing is. If
/// something has the name already it is left as it is, and the error is `AlreadyExists`.
/// What a stopped attempt left beside the name is never read: it is cleared first.
pub fn write_once(path: &Path, bytes: &[u8], steps: &Steps) -> std::io::Result<()> {
    use std::io::Write;
    let temp = beside(path);
    // Removed, not written over: the next file there is a new one, so nothing else that
    // names the old one (on Linux, the copy itself, if an attempt was stopped between the
    // two steps of its move) is written through it.
    match fs::remove_file(&temp) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let placed = (|| {
        let mut file = (steps.create)(&temp)?;
        file.write_all(bytes)?;
        (steps.flush)(&file)?;
        drop(file);
        (steps.place)(&temp, path)
    })();
    if placed.is_err() {
        // This attempt's own file, which never had the name.
        let _ = fs::remove_file(&temp);
    }
    placed?;
    (steps.sync_folder)(path)
}

/// The steps by which `write_once` puts a file on the disk, given so that a test can have
/// the app stopped at one of them, or have one fail. `ON_DISK` are the real ones.
#[derive(Clone, Copy)]
pub struct Steps {
    /// Makes a new, empty file to write, only if nothing has its name.
    pub create: fn(&Path) -> std::io::Result<fs::File>,
    /// Makes sure what was written to it is on the disk.
    pub flush: fn(&fs::File) -> std::io::Result<()>,
    /// Gives the file written beside its place its own name, if nothing has that name.
    pub place: fn(&Path, &Path) -> std::io::Result<()>,
    /// Makes the names in a file's folder outlast a power cut.
    pub sync_folder: fn(&Path) -> std::io::Result<()>,
}

pub const ON_DISK: Steps = Steps {
    create: |path| fs::OpenOptions::new().write(true).create_new(true).open(path),
    flush: fs::File::sync_all,
    place: place_new,
    sync_folder,
};

/// Moves `from` to `to` only if nothing has the name `to`, and never over what has it:
/// `AlreadyExists` then, and both left as they are. (`fs::rename` replaces.) Windows:
/// `MoveFileExW` without `MOVEFILE_REPLACE_EXISTING`, which fails if the name is had; with
/// `MOVEFILE_WRITE_THROUGH`, which Microsoft documents as not returning until the file is
/// actually moved on the disk.
#[cfg(windows)]
fn place_new(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let wide = |path: &Path| path.as_os_str().encode_wide().chain(Some(0)).collect::<Vec<u16>>();
    let (from, to) = (wide(from), wide(to));
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Elsewhere (the Linux build): a second name is made for the file with `link`, which fails
/// if the name is had and never replaces, and then its first name is removed. The new name
/// comes with the whole file. (If the removal doesn't happen, the file under its first name
/// is cleared by the next attempt, and never read.)
#[cfg(not(windows))]
fn place_new(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::hard_link(from, to)?;
    let _ = fs::remove_file(from);
    Ok(())
}

/// Makes the names in a file's folder, made or removed, outlast a power cut: the folder is
/// synced (Linux).
#[cfg(not(windows))]
fn sync_folder(path: &Path) -> std::io::Result<()> {
    let folder = path.parent().filter(|folder| !folder.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::File::open(folder)?.sync_all()
}

/// Windows lets no ordinary program sync a folder. What stands for it is the move's
/// `MOVEFILE_WRITE_THROUGH` (`place_new`).
#[cfg(windows)]
fn sync_folder(_: &Path) -> std::io::Result<()> {
    Ok(())
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

/// Keeps a secret other than the account password, by name, where that one is kept: the
/// Windows credential store, or elsewhere a file only this user can read. An empty value
/// forgets it.
#[cfg(windows)]
pub fn save_secret(_paths: &Paths, name: &str, value: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &format!("secret:{name}")) {
        let _ = if value.is_empty() { entry.delete_credential() } else { entry.set_password(value) };
    }
}

#[cfg(windows)]
pub fn load_secret(_paths: &Paths, name: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("secret:{name}")).ok()?.get_password().ok()
}

#[cfg(not(windows))]
pub fn save_secret(paths: &Paths, name: &str, value: &str) {
    use std::os::unix::fs::PermissionsExt;
    let file = paths.secret.with_file_name(format!("secret-{name}"));
    if value.is_empty() {
        let _ = fs::remove_file(file);
    } else if fs::write(&file, value).is_ok() {
        let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o600));
    }
}

#[cfg(not(windows))]
pub fn load_secret(paths: &Paths, name: &str) -> Option<String> {
    fs::read_to_string(paths.secret.with_file_name(format!("secret-{name}"))).ok().filter(|value| !value.is_empty())
}

/// Total size of the files directly inside `dir`, in bytes.
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_damaged_settings_file_is_never_quietly_replaced() {
        let dir = std::env::temp_dir().join(format!("nexus-settings-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        // A first run: defaults, and nothing to say.
        let (settings, note) = Settings::load_checked(&path);
        assert!(note.is_none() && settings.server.is_empty());
        // A good file is read, and a copy of it kept.
        Settings { server: "https://music.example.org".into(), volume: 0.4, ..Default::default() }.save(&path);
        let (settings, note) = Settings::load_checked(&path);
        assert!(note.is_none() && settings.server == "https://music.example.org");
        // Cut short: the copy stands in, the file is mended, and the app says so.
        fs::write(&path, br#"{"server": "https://music.ex"#).unwrap();
        let (settings, note) = Settings::load_checked(&path);
        assert!(settings.server == "https://music.example.org" && settings.volume == 0.4);
        assert!(note.unwrap().contains("damaged"));
        assert_eq!(Settings::load_checked(&path).0.server, "https://music.example.org");
        // No copy either: the damaged file is kept under another name, not overwritten.
        fs::write(&path, b"not settings at all").unwrap();
        fs::remove_file(dir.join("settings.backup.json")).unwrap();
        let (settings, note) = Settings::load_checked(&path);
        assert!(settings.server.is_empty() && note.unwrap().contains("settings.damaged.json"));
        assert_eq!(fs::read(dir.join("settings.damaged.json")).unwrap(), b"not settings at all");
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_the_equaliser_curve_and_works_out_the_preamp() {
        let mut settings = Settings::default();
        assert_eq!(settings.eq_preset(), EQ_PRESETS[0].0);
        settings.eq_gains = [1.5, 0.0, -2.0, 0.0, 4.5, 3.0];
        assert_eq!(settings.eq_preset(), "Custom");
        settings.eq_saved.push(("Headphones".into(), settings.eq_gains));
        assert_eq!(settings.eq_preset(), "Headphones");
        // Off, a set amount, and Auto: as much as the band raised the most.
        assert_eq!(settings.eq_preamp_now(), 0.0);
        settings.eq_preamp = -3.0;
        assert_eq!(settings.eq_preamp_now(), -3.0);
        settings.eq_preamp = 1.0;
        assert_eq!(settings.eq_preamp_now(), -4.5);
        settings.eq_gains = [-3.0; 6];
        assert_eq!(settings.eq_preamp_now(), 0.0);
    }

    #[test]
    fn a_settings_file_from_a_version_that_stamped_its_choices_loses_and_redates_nothing() {
        use nexus_core::step::Change;
        use nexus_core::sync::LONG_AGO;
        let dir = std::env::temp_dir().join(format!("nexus-settings-step-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        // As 0.40.4 wrote it: pins with their time, a theme chosen before sharing existed
        // (no time), a list sorted at a known time and one shuffled before lists had times.
        fs::write(
            &path,
            br#"{"server":"https://music.example.org","pinned_playlists":["p1","p2"],"pins_at":1700000000,"theme":"Ocean","theme_at":0,
                "lists":{"liked":{"shuffle":false,"sort":2,"descending":true,"sort_chosen":true,"at":1700000500},"album:a":{"shuffle":true,"sort":0,"descending":false,"sort_chosen":false,"at":0}},
                "sync":true,"device_id":"0badc0de","sync_playlist":"abc"}"#,
        )
        .unwrap();
        let (mut settings, note) = Settings::load_checked(&path);
        assert!(note.is_none());
        // Everything it showed, it shows.
        assert_eq!((settings.pinned_playlists.as_slice(), settings.theme.as_str()), (["p1".to_string(), "p2".to_string()].as_slice(), "Ocean"));
        assert_eq!((settings.lists["liked"].sort, settings.lists["liked"].descending, settings.lists["album:a"].shuffle), (2, true, true));
        // And each choice is agreed, at the time it had, or "long ago" where it had none.
        let agreed = &settings.choices.agreed;
        assert_eq!((agreed.pins.len(), agreed.pins_at, agreed.theme.as_str(), agreed.theme_at), (2, 1_700_000_000, "Ocean", LONG_AGO));
        assert_eq!((agreed.lists["liked"].at, agreed.lists["album:a"].at), (1_700_000_500, LONG_AGO));
        assert!(settings.choices.waiting.is_empty() && settings.in_step == 1);
        // A change made now is kept as the change, puts no time on anything, and is on the
        // disk before it is shown.
        assert!(settings.choices.change(&settings.account(), Change::Pin("p3".into())));
        assert_eq!(settings.account(), "https://music.example.org\n");
        settings.show();
        assert!(settings.save_surely(&path));
        let (again, _) = Settings::load_checked(&path);
        assert_eq!(again.pinned_playlists, ["p1", "p2", "p3"]);
        assert_eq!((again.pins_at, again.choices.agreed.pins.len(), again.choices.waiting.len()), (1_700_000_000, 2, 1));
        // The usual theme, never chosen, is nobody's choice; and a first run has nothing agreed.
        fs::write(&path, br#"{"theme":"Violet"}"#).unwrap();
        assert_eq!(Settings::load_checked(&path).0.choices.agreed.theme_at, 0);
        fs::remove_file(&path).unwrap();
        let (fresh, _) = Settings::load_checked(&path);
        let agreed = &fresh.choices.agreed;
        assert_eq!((fresh.in_step, agreed.theme.as_str(), agreed.theme_at, agreed.pins_at, agreed.lists.len()), (1, "Violet", 0, 0, 0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_account_is_told_from_another_by_exactly_what_it_is() {
        use nexus_core::step::{Change, Choices};
        let of = |server: &str, username: &str| Settings { server: server.into(), username: username.into(), ..Default::default() };
        let home = "https://music.example.org";
        // Two usernames that differ only in punctuation are two accounts: a change made
        // under one is not the other's, to show or to send.
        let (dot, under) = (of(home, "alice.smith"), of(home, "alice_smith"));
        assert_ne!(dot.account(), under.account());
        let mut choices = Choices::default();
        choices.change(&dot.account(), Change::Pin("p".into()));
        assert_eq!((choices.shown(&dot.account()).pins.len(), choices.shown(&under.account()).pins.len()), (1, 0));
        // However long the server's address, the username is part of who it is.
        let long = format!("https://{}.example.org", "a".repeat(120));
        assert_ne!(of(&long, "one").account(), of(&long, "two").account());
        // The same server and name are the same account, every time.
        assert_eq!(dot.account(), of(home, "alice.smith").account());
        // In a file's name: no two accounts share one, on a disk that ignores case either;
        // and it is made of nothing a file's name can't hold, and isn't long.
        let all = [dot, under, of(home, "Alice"), of(home, "alice"), of(&long, "one"), of(&long, "two"), of("http://music.example.org", "alice"), of(home, "")];
        let names: Vec<String> = all.iter().map(|settings| settings.account_file()).collect();
        let folded: std::collections::HashSet<String> = names.iter().map(|name| name.to_lowercase()).collect();
        assert_eq!(folded.len(), names.len(), "{names:?}");
        assert!(names.iter().all(|name| name.len() <= 64 && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')), "{names:?}");
        // Readable, as far as it goes.
        assert!(names[0].starts_with("music-example-org-alice-smith-"), "{names:?}");
    }

    #[test]
    fn the_shape_of_a_queue_follows_what_is_in_it() {
        let song = |id: &str| Song { id: id.into(), ..Default::default() };
        let songs = [song("a"), song("b"), song("c")];
        let shape = |songs: &[Song], order: &[usize], context: &str| queue_shape(songs, order, context, "album:1", &[], &[]);
        let base = shape(&songs, &[0, 1, 2], "An album");
        // The same queue has the same shape, every time and on every run.
        assert_eq!(base, shape(&songs, &[0, 1, 2], "An album"));
        assert_eq!(queue_shape(&[], &[], "", "", &[], &[]), 0x0a99_c907_b6f6_4763);
        // A different order, a different song or a different name is a different queue.
        assert_ne!(base, shape(&songs, &[0, 2, 1], "An album"));
        assert_ne!(base, shape(&[song("a"), song("b"), song("x")], &[0, 1, 2], "An album"));
        assert_ne!(base, shape(&songs, &[0, 1, 2], "Another"));
        // Ids that run together differently are told apart.
        assert_ne!(shape(&[song("ab"), song("c")], &[0, 1], ""), shape(&[song("a"), song("bc")], &[0, 1], ""));
    }
}
