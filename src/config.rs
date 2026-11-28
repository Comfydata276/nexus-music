//! Where things live on disk, saved settings, and the music login kept where secrets are kept.

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
        let paths = Paths::at(&config, &cache, &data);
        for dir in [&config, &paths.covers, &paths.pages, &paths.buffers, &paths.songs] {
            let _ = fs::create_dir_all(dir);
        }
        paths
    }

    /// Where each file goes, in these three folders (see `docs/FILES.md`).
    fn at(config: &Path, cache: &Path, data: &Path) -> Paths {
        Paths {
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
        }
    }
}

pub use nexus_core::sync::ListPrefs;


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
    /// A token kept by 0.40.10, never bound, was to be deleted and couldn't be: it's spent,
    /// neither used nor bound (see `keep::take_token_telling`), until a token is next kept
    /// bound.
    pub lidarr_token_spent: bool,
    /// What this device is called in the person's list of devices at the Nexus server; empty
    /// for the computer's name.
    pub device_name: String,
    /// Share the queue, pins and so on with the app on other devices.
    pub sync: bool,
    /// A made-up name for this device, so it can tell its own saved queue from others'.
    pub device_id: String,
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
    /// The person has been told that the settings folder's drive can't make hard links
    /// (`no_links_note`).
    pub said_no_links: bool,
    /// The address and email last signed in with to the Nexus server, put in the sign-in
    /// screen's fields (nothing secret: the session is kept where secrets are kept).
    pub sign_in_address: String,
    pub sign_in_email: String,
    /// Who took over the folder this PC had before the Nexus server (the backend's id and
    /// their account), and that folder's name: written once, at that sign-in (`folder_for`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carried_over: Option<(String, String)>,
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
            lidarr_token_spent: false,
            device_name: String::new(),
            sync: true,
            device_id: String::new(),
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
            said_no_links: false,
            sign_in_address: String::new(),
            sign_in_email: String::new(),
            carried_over: None,
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
        Settings::load_checked_by(path, place_new)
    }

    /// `load_checked`, moving a damaged file aside by `aside_by` (a test's, to have it fail).
    fn load_checked_by(path: &Path, aside_by: fn(&Path, &Path) -> std::io::Result<()>) -> (Settings, Option<String>) {
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
        // Kept under a name of its own (`set_aside`), and said to be only if it was.
        let note = match set_aside_by(path, aside_by) {
            Some(aside) => format!(
                "Your settings file couldn't be read and there was no copy to fall back on. It has been kept as {}, and the app has started with its default settings.",
                aside.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default()
            ),
            None => "Your settings file couldn't be read, there was no copy to fall back on, and it couldn't be moved aside. The app has started with its default settings, which will take its place when they are next saved.".to_string(),
        };
        (fresh(), Some(note))
    }

    /// Saves the settings after a change the person made: flushed to the disk.
    pub fn save(&self, path: &Path) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = write_surely(path, &json);
        }
    }

    /// Saves the settings in passing, as an exchange or a new queue starting does: not
    /// waiting for the disk (`Durable::InPassing`).
    pub fn save_in_passing(&self, path: &Path) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = write_in_passing(path, &json);
        }
    }

    /// Saves the settings and says whether they are on the disk: for a change that must be
    /// kept before it is shown as made (see `Controller::choose`).
    pub fn save_surely(&self, path: &Path) -> bool {
        self.save_checked(path, Durable::Flushed)
    }

    /// `save_surely`, written as `durable` says.
    pub fn save_checked(&self, path: &Path, durable: Durable) -> bool {
        serde_json::to_vec_pretty(self).is_ok_and(|json| write_whole(path, &json, durable, &REPLACING).is_ok())
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

/// Saves Recently played as a song starts, or as an exchange brings plays from elsewhere: in
/// passing (`Durable::InPassing`).
pub fn save_history(path: &Path, history: &[Played]) {
    if let Ok(json) = serde_json::to_vec(history) {
        let _ = write_in_passing(path, &json);
    }
}

/// **The one way the app writes a file it must not lose**, so that a kill or a power cut
/// leaves either the file as it was or the file as it is now, never one cut short, and never
/// an empty one: written beside its place under a name of this write's own (`temp_for`),
/// flushed to the disk, given its name by `steps.place`, and its folder synced (Linux). It
/// renames only its own file, and removes only its own if a step fails; then it is an
/// error, and the file as it was. What killed runs left beside it is never read, and is
/// cleared when the app starts (`clear_leftovers`), never by a write in passing: it could
/// be another write's, under way.
///
/// Whether it may replace a file that has the name is the steps': `REPLACING` (`fs::rename`,
/// for a file written again and again: `write_surely`) does, `ONCE` (for a file made once and
/// never replaced: the kept copy of the shared record, the start of the log of plays) never
/// does, and gives `AlreadyExists` instead.
/// A file written whole is in place: and its folder synced, or (Linux) not, so that its
/// name may not outlast a power cut. Either way it is written: the file has changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placed {
    Synced,
    Unsynced,
}

/// Whether a write waits for the disk: the rule at the top of `docs/FILES.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durable {
    /// Made because the person just did something, or wanted on the disk before the app
    /// acts on it: flushed before it is given its name, and its folder synced (Linux).
    Flushed,
    /// Made by a timer, or by playback moving on: renamed into place without a flush, as
    /// 0.40.10 wrote everything. A kill leaves the old file or the new one; a power cut within
    /// a few seconds of it can leave the name empty.
    InPassing,
}

pub fn write_whole(path: &Path, bytes: &[u8], durable: Durable, steps: &Steps) -> std::io::Result<Placed> {
    let started = std::time::Instant::now();
    let placed = write_whole_now(path, bytes, durable, steps);
    if durable == Durable::Flushed {
        flushed(path, started);
    }
    placed
}

thread_local! {
    /// This is the window's thread (`mark_window_thread`), and how many files it has flushed.
    static WINDOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static WINDOW_FLUSHES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Marks the calling thread as the window's: each file flushed on it is counted and, with
/// `NEXUS_TRACE` set, printed. On it a write waits for the disk only inside the handler of a
/// press on a discrete control, once per press (`docs/FILES.md`): the trace is the proof.
pub fn mark_window_thread() {
    WINDOW.set(true);
}

#[cfg(test)]
pub fn window_flushes() -> u64 {
    WINDOW_FLUSHES.get()
}

/// Notes that a file was flushed: counted and traced, if this is the window's thread.
pub fn flushed(path: &Path, started: std::time::Instant) {
    if !WINDOW.get() {
        return;
    }
    WINDOW_FLUSHES.set(WINDOW_FLUSHES.get() + 1);
    if std::env::var_os("NEXUS_TRACE").is_some() {
        // Which of the app's own functions asked for it, nearest first: the press, or not.
        let trace = std::backtrace::Backtrace::force_capture().to_string();
        let callers: Vec<&str> = trace
            .lines()
            .filter_map(|line| line.trim().split_once(": ").map(|(_, name)| name))
            .filter(|name| name.starts_with("nexus_music::") && !name.starts_with("nexus_music::config::"))
            .map(|name| name.trim_start_matches("nexus_music::").split("::{{closure}}").next().unwrap_or(name))
            .take(4)
            .collect();
        eprintln!("[trace] flushed on the window's thread in {:.1} ms: {} (from {})", started.elapsed().as_secs_f64() * 1000.0, path.display(), callers.join(" < "));
    }
}

fn write_whole_now(path: &Path, bytes: &[u8], durable: Durable, steps: &Steps) -> std::io::Result<Placed> {
    use std::io::Write;
    // A new file, never one that is there: a name of a killed run's that came round again
    // (the same process number, and its count) is passed over for the next.
    let (temp, mut file) = {
        let mut tries = 0;
        loop {
            let temp = temp_for(path);
            match (steps.create)(&temp) {
                Ok(file) => break (temp, file),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
                Err(error) => return Err(error),
            }
        }
    };
    let placed = (|| {
        file.write_all(bytes)?;
        if durable == Durable::Flushed {
            (steps.flush)(&file)?;
        }
        drop(file);
        (steps.place)(&temp, path)
    })();
    if placed.is_err() {
        // This attempt's own file, which never had the name.
        let _ = fs::remove_file(&temp);
    }
    placed?;
    if durable == Durable::InPassing {
        return Ok(Placed::Unsynced);
    }
    Ok(if (steps.sync_folder)(path).is_ok() { Placed::Synced } else { Placed::Unsynced })
}

/// `write_whole`, over whatever has the name: for what is written again and again (the
/// settings, the queue, what waits to be sent, ...). A caller that has nothing to do about a
/// failure leaves the error be: the file is then as it was.
pub fn write_surely(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_whole(path, bytes, Durable::Flushed, &REPLACING).map(drop)
}

/// `write_whole`, over whatever has the name, not waiting for the disk (`Durable::InPassing`).
pub fn write_in_passing(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_whole(path, bytes, Durable::InPassing, &REPLACING).map(drop)
}

/// The name a write puts its file under, beside `path`, before it gives it the name: the
/// whole of `path`'s name, this process's number, and a count of this process's writes. No
/// other write, here or in another copy of the app, makes the same one.
pub fn temp_for(path: &Path) -> PathBuf {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{name}.{}.{count}.tmp", std::process::id()))
}

/// Moves a file that couldn't be read aside, under a name of its own (`asked.json` to
/// `asked.damaged-20261126-101500.json`, with `-2` and so on after it if that is had), never
/// over another. Says where it went, or `None` if it couldn't be moved: then it is where it
/// was.
pub fn set_aside(path: &Path) -> Option<PathBuf> {
    set_aside_by(path, place_new)
}

fn set_aside_by(path: &Path, place: fn(&Path, &Path) -> std::io::Result<()>) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    let extension = path.extension().map(|extension| format!(".{}", extension.to_string_lossy())).unwrap_or_default();
    let when = chrono::Local::now().format("%Y%m%d-%H%M%S");
    for count in 1..100 {
        let more = if count == 1 { String::new() } else { format!("-{count}") };
        let aside = path.with_file_name(format!("{stem}.damaged-{when}{more}{extension}"));
        match place(path, &aside) {
            // On Linux the move is a second name and then the first removed: if the first is
            // still there, it wasn't moved, and the second name goes again.
            Ok(()) if fs::symlink_metadata(path).is_ok() => {
                let _ = fs::remove_file(&aside);
                return None;
            }
            Ok(()) => return Some(aside),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// The settings folder's drive can't make hard links (`place_new`, Linux), noted the first
/// time a file made once is tried there.
static NO_LINKS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn note_no_links() {
    NO_LINKS.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// What to tell the person, **once ever** (`said` is kept in the settings), when the drive
/// the settings are on can't make hard links: without them the files that are made once
/// (the kept copy of the shared record, the start of the log of plays) can't be made safely
/// on Linux, so they aren't made at all. Not each time it is tried, nor at each start.
pub fn no_links_note(said: &mut bool) -> Option<&'static str> {
    no_links_note_given(NO_LINKS.load(std::sync::atomic::Ordering::SeqCst), said)
}

/// `no_links_note`, given whether the drive has been found without hard links.
fn no_links_note_given(found: bool, said: &mut bool) -> Option<&'static str> {
    if *said || !found {
        return None;
    }
    *said = true;
    Some(
        "The drive this PC keeps Nexus Music's settings on can't make hard links, which the app needs on Linux to keep some files safely. So nothing is shared from this PC with your other devices, and “Your stats” counts only what's played while the app runs. Moving the settings to a drive formatted for Linux puts that right.",
    )
}


/// How much of the crash log has been mentioned already, kept beside it.
pub fn crash_seen(paths: &Paths) -> PathBuf {
    paths.crash_log.with_extension("seen")
}

/// What waits to be sent to the server for an account (`controller::account_stem`).
pub fn pending_file(paths: &Paths, account: &str) -> PathBuf {
    paths.pending.with_file_name(format!("pending-{account}.json"))
}

/// The copy of the library search goes by, and the lyrics kept, in a sign-in's folder of
/// the page cache.
pub fn library_file(pages: &Path) -> PathBuf {
    pages.join("library.json")
}

pub fn lyrics_dir(pages: &Path) -> PathBuf {
    pages.join("lyrics")
}

/// The file a write's own file (`temp_for`) was made for, if `name` is one:
/// `<file>.<process>.<count>.tmp`.
fn made_for(name: &str) -> Option<&str> {
    let rest = name.strip_suffix(".tmp")?;
    let (rest, count) = rest.rsplit_once('.')?;
    let (file, process) = rest.rsplit_once('.')?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    (digits(process) && digits(count) && !file.is_empty()).then_some(file)
}

/// Clears from `dir` what writes killed part-way left, and **only what is provably the
/// app's**: a file whose whole name is one of its own files' (`own`) followed by this
/// version's temporary ending (`temp_for`), or is exactly one of the names versions up to
/// 0.40.10 could leave (`old`). Never by a pattern over a folder the person also uses.
/// **Only when the app starts, once, before the first write**: later it could take a write's
/// file from under it.
fn clear_leftovers(dir: &Path, own: impl Fn(&str) -> bool, old: &[String]) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = old.contains(&name) || made_for(&name).is_some_and(&own);
        if ours && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// The settings folder's leftovers, for the account signed in: `account_file` as the kept
/// copy of the shared record names it (`Settings::account_file`), `account_stem` as what
/// waits to be sent does (`controller::account_stem`). Another account's are left: they
/// are never read, and are cleared when it next starts the app.
pub fn clear_settings_leftovers(dir: &Path, account_file: &str, account_stem: &str) {
    const FILES: [&str; 8] = ["settings.json", "queue.json", "queue-place.json", "history.json", "playlist-added.json", "asked.json", "asking.json", "plays.log"];
    let pending = format!("pending-{account_stem}.json");
    let kept = format!("shared-record-kept-{account_file}-");
    let own = |name: &str| {
        // (The kept copy's version is digits and dots, and nothing else.)
        let version = name.strip_prefix(kept.as_str()).and_then(|rest| rest.strip_suffix(".json"));
        let kept_copy = version.is_some_and(|version| !version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.'));
        FILES.contains(&name) || (!account_stem.is_empty() && name == pending) || (!account_file.is_empty() && kept_copy)
    };
    // What 0.40.10 and the versions before it could leave: each file's stem with `.tmp`
    // (written beside its place and renamed: the settings, the queue's two files, Recently
    // played, when songs were added, the log of plays rewritten, what waits to be sent), the
    // batch's `asking.json.part`, and the kept copy's `.tmp` from 0.40.8 to 0.40.10.
    let mut old: Vec<String> = ["settings.tmp", "queue.tmp", "queue-place.tmp", "history.tmp", "playlist-added.tmp", "plays.tmp", "asking.json.part"].map(String::from).to_vec();
    if !account_stem.is_empty() {
        old.push(format!("pending-{account_stem}.tmp"));
    }
    if !account_file.is_empty() {
        old.extend(["0.40.8", "0.40.9", "0.40.10"].map(|version| format!("shared-record-kept-{account_file}-{version}.tmp")));
    }
    clear_leftovers(dir, own, &old);
}

/// The data folder's: the crash note's marker's (0.40.10 wrote it in place: nothing old).
pub fn clear_data_leftovers(dir: &Path) {
    clear_leftovers(dir, |name| name == "crash.seen", &[]);
}

/// A downloads folder's, which may be the person's own: only the two lists of the account
/// signed in, by exact name.
pub fn clear_downloads_leftovers(dir: &Path, account_stem: &str) {
    if account_stem.is_empty() {
        return;
    }
    let (index, collections) = (format!("index-{account_stem}.json"), format!("collections-{account_stem}.json"));
    let old = [format!("index-{account_stem}.tmp"), format!("collections-{account_stem}.tmp")];
    clear_leftovers(dir, |name| name == index || name == collections, &old);
}

/// The steps by which `write_whole` puts a file on the disk, given so that a test can have
/// the app stopped at one of them, or have one fail. `ONCE` and `REPLACING` are the real
/// ones.
#[derive(Clone, Copy)]
pub struct Steps {
    /// Makes a new, empty file to write, only if nothing has its name.
    pub create: fn(&Path) -> std::io::Result<fs::File>,
    /// Makes sure what was written to it is on the disk.
    pub flush: fn(&fs::File) -> std::io::Result<()>,
    /// Gives the file written beside its place its own name: over what has it
    /// (`REPLACING`), or only if nothing has it (`ONCE`).
    pub place: fn(&Path, &Path) -> std::io::Result<()>,
    /// Makes the names in a file's folder outlast a power cut.
    pub sync_folder: fn(&Path) -> std::io::Result<()>,
}

/// For a file made once and never replaced.
pub const ONCE: Steps = Steps {
    create: |path| fs::OpenOptions::new().write(true).create_new(true).open(path),
    flush: fs::File::sync_all,
    place: place_new,
    sync_folder,
};

/// For a file written again and again: it replaces what has the name. (Without
/// write-through on Windows: the file was flushed before the rename, so a power cut leaves
/// the old file or the new one, whichever the rename had got to.)
pub const REPLACING: Steps = Steps { place: |from, to| fs::rename(from, to), ..ONCE };

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
    if let Err(error) = fs::hard_link(from, to) {
        // A drive that can't make a second name for a file (FAT, exFAT, some network ones):
        // "not permitted", "not supported", "too many links".
        if error.kind() == std::io::ErrorKind::Unsupported || matches!(error.raw_os_error(), Some(1 | 31 | 95)) {
            note_no_links();
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, format!("this drive can't make hard links ({error})")));
        }
        return Err(error);
    }
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

#[cfg(windows)]
const KEYRING_SERVICE: &str = "Nexus Music";

/// How 0.40 and before named the music-server password it kept: removed at the first start of
/// this version (`forget_old_password`), and never read.
#[cfg_attr(not(windows), allow(dead_code))]
fn old_password_entry(server: &str, username: &str) -> String {
    format!("{username}@{server}")
}

/// The music-server password that 0.40 and before kept for a sign-in of their own, removed.
/// Nothing reads it: from this version the app is handed a salt and token instead (design
/// 9.4) and keeps no music-server password at all.
#[cfg(windows)]
pub fn forget_old_password(_paths: &Paths, server: &str, username: &str) {
    if server.is_empty() || username.is_empty() {
        return;
    }
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &old_password_entry(server, username)) {
        let _ = entry.delete_credential();
    }
}

// Other platforms are only used for development, and kept it in a file readable by the
// current user.
#[cfg(not(windows))]
pub fn forget_old_password(paths: &Paths, _server: &str, _username: &str) {
    let _ = fs::remove_file(&paths.secret);
}

/// The sign-in to the music server this device was handed by the Nexus server (design 9.4):
/// the server's address, the name of the person's user there, and a salt and token made for
/// this device, with the number of the login they were made from. Never a password.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MusicLogin {
    pub server: String,
    pub username: String,
    pub salt: String,
    pub token: String,
    /// The Nexus server's count of this person's music login when it was handed (design 9.4).
    pub count: u64,
}

impl std::fmt::Debug for MusicLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MusicLogin {{ server: {:?}, username: {:?}, count: {} }}", self.server, self.username, self.count)
    }
}

/// What a sign-in to the Nexus server leaves on this PC (design 11): **one thing, kept where
/// secrets are kept and put in place in one step**, so that a write cut short leaves the one
/// before it whole. Its parts are what the shared library's client says to keep
/// (its `client::Keep`): the session, the music login, and the account (the
/// Nexus address, the backend's id, who the person is to it, the numbers last seen), kept
/// here as the library wrote it; and the folder this person's things are under. A cold start
/// reads it and plays from its music login without asking the Nexus server anything
/// (design 6.1). The device's own secret is kept beside it; the password typed, nowhere.
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SignedIn {
    /// What the shared library's client said to keep, whole (its `client::Saved`,
    /// as it serialises it): the Nexus origin, the backend's id, who the person is to it,
    /// the session and the note that can end it, and the music login with its number.
    #[serde(default)]
    pub saved: Option<serde_json::Value>,
    /// The music login in it, as a cold start reads it (written with it, in the same step).
    #[serde(default)]
    pub music: Option<MusicLogin>,
    /// The name of the folder this person's things are kept under on this PC (downloads,
    /// the changes waiting for the music server): decided once, at the sign-in that made it
    /// (`folder_for`), and from then on tied to who they are to the backend, never to the
    /// music server and user.
    #[serde(default)]
    pub folder: String,
}

impl std::fmt::Debug for SignedIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SignedIn {{ saved: {}, music: {:?}, folder: {:?} }}", self.saved.is_some(), self.music, self.folder)
    }
}

impl SignedIn {
    /// Whether a cold start can play from it: a music login, and the folder it is for.
    pub fn plays(&self) -> Option<&MusicLogin> {
        self.music.as_ref().filter(|music| !self.folder.is_empty() && !music.server.is_empty() && !music.username.is_empty() && !music.salt.is_empty() && !music.token.is_empty())
    }
}

/// The name it is kept under.
const SIGNED_IN: &str = "nexus-sign-in";

/// What the last sign-in left, if anything is kept and it can be read whole.
pub fn load_sign_in(paths: &Paths) -> Option<SignedIn> {
    let text = read_kept(paths, SIGNED_IN).ok()??;
    serde_json::from_str(&text).ok()
}

/// Puts what a sign-in left in place, whole, in one step; nothing kept, if it holds nothing.
/// Says whether it was done.
pub fn save_sign_in(paths: &Paths, kept: &SignedIn) -> bool {
    if kept.saved.is_none() {
        return write_kept(paths, SIGNED_IN, "");
    }
    serde_json::to_string(kept).is_ok_and(|text| write_kept(paths, SIGNED_IN, &text))
}

/// The folder a person's things are kept under, decided at their first sign-in on this PC
/// (design 11). **What is carried over, exactly:** the folder of the sign-in this PC had
/// before the Nexus server (named by the music server and user, `account_stem`) becomes
/// the person's, **once**, when the shared library says the music login they are handed
/// names that same music server and user (its `client::carries_over`, which
/// the caller asks and passes as `same`). Every other person, and anyone handed that music
/// user later, gets a folder named by who they are to the backend: its id and their
/// account's number. Whoever calls this writes the carry-over down
/// (`Settings::carried_over`) when it says one was made, so that it happens once.
pub fn folder_for(settings: &Settings, backend: &str, account: u64, same: bool) -> (String, bool) {
    let who = format!("{backend}\n{account}");
    if let Some((owner, folder)) = &settings.carried_over {
        if owner == &who {
            return (folder.clone(), false);
        }
    } else if same && !settings.server.is_empty() && !settings.username.is_empty() {
        return (crate::controller::account_stem(&settings.server, &settings.username), true);
    }
    (person_folder(backend, account), false)
}

/// A folder named by who a person is to the backend: its id and their account's number.
pub fn person_folder(backend: &str, account: u64) -> String {
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(format!("{backend}\n{account}").as_bytes()).iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    format!("nexus-{account}-{hash}")
}

/// The sign-outs the Nexus server has not taken yet (the shared library's notes, as it
/// serialises them, each ending one session and nothing else), kept beside the sign-in.
const NOTES: &str = "nexus-sign-out-notes";

pub fn load_notes(paths: &Paths) -> Vec<serde_json::Value> {
    read_kept(paths, NOTES).ok().flatten().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// Puts the notes in place, whole, in one step; says whether it was done.
pub fn save_notes(paths: &Paths, notes: &[serde_json::Value]) -> bool {
    if notes.is_empty() {
        return write_kept(paths, NOTES, "");
    }
    serde_json::to_string(notes).is_ok_and(|text| write_kept(paths, NOTES, &text))
}

/// Keeps a secret by name: in the Windows credential store, or elsewhere a file only this
/// user can read. An empty value
/// forgets it. Says whether that was done (forgetting one that isn't there is done).
#[cfg(windows)]
pub fn save_secret(_paths: &Paths, name: &str, value: &str) -> bool {
    let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &format!("secret:{name}")) else { return false };
    if value.is_empty() {
        matches!(entry.delete_credential(), Ok(()) | Err(keyring::Error::NoEntry))
    } else {
        entry.set_password(value).is_ok()
    }
}

#[cfg(windows)]
pub fn load_secret(_paths: &Paths, name: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("secret:{name}")).ok()?.get_password().ok()
}

#[cfg(not(windows))]
pub fn save_secret(paths: &Paths, name: &str, value: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let file = paths.secret.with_file_name(format!("secret-{name}"));
    if value.is_empty() {
        match fs::remove_file(file) {
            Ok(()) => true,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    } else {
        fs::write(&file, value).is_ok() && fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).is_ok()
    }
}

// ---- what's kept for Lidarr (see `keep`): written as bytes ------------------------------------

/// The first bytes of what `keep` binds to a place: what tells this version's entries, written
/// as UTF-8 bytes, from 0.40.10's, written on Windows as UTF-16 text.
#[cfg_attr(not(windows), allow(dead_code))]
const KEPT_MARK: &[u8] = b"nexus-bound-1\n";

/// What's written for a value `keep` keeps: its UTF-8 bytes. (Windows allows a credential
/// 2,560 bytes: as UTF-16 text that's 1,280 characters; as these, 2,560 of plain ASCII.)
pub fn kept_bytes(value: &str) -> Vec<u8> {
    value.as_bytes().to_vec()
}

/// What `kept_bytes` wrote, read back; `None` if it isn't text.
pub fn kept_text(bytes: &[u8]) -> Option<String> {
    String::from_utf8(bytes.to_vec()).ok()
}

/// On Windows: what this version wrote (`KEPT_MARK` first, UTF-8), or what 0.40.10 wrote
/// (UTF-16 text, little-endian, as the credential store's password).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn kept_text_windows(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(KEPT_MARK) {
        return kept_text(bytes);
    }
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    String::from_utf16(&units).ok()
}

/// What `keep` keeps under `name`: there, not there (`Ok(None)`), or couldn't be read (`Err`,
/// which is not the same as not there).
#[cfg(windows)]
pub fn read_kept(_paths: &Paths, name: &str) -> Result<Option<String>, ()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &format!("secret:{name}")).map_err(drop)?;
    match entry.get_secret() {
        Ok(bytes) => kept_text_windows(&bytes).map(Some).ok_or(()),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(()),
    }
}

/// Writes what `keep` keeps under `name` as bytes (`kept_bytes`); empty forgets it. Says
/// whether that was done.
#[cfg(windows)]
pub fn write_kept(_paths: &Paths, name: &str, value: &str) -> bool {
    let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &format!("secret:{name}")) else { return false };
    if value.is_empty() {
        matches!(entry.delete_credential(), Ok(()) | Err(keyring::Error::NoEntry))
    } else {
        entry.set_secret(&kept_bytes(value)).is_ok()
    }
}

#[cfg(not(windows))]
pub fn read_kept(paths: &Paths, name: &str) -> Result<Option<String>, ()> {
    match fs::read(paths.secret.with_file_name(format!("secret-{name}"))) {
        Ok(bytes) => kept_text(&bytes).map(Some).ok_or(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

#[cfg(not(windows))]
pub fn write_kept(paths: &Paths, name: &str, value: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let file = paths.secret.with_file_name(format!("secret-{name}"));
    if value.is_empty() {
        return save_secret(paths, name, "");
    }
    // Whole or not at all, as the credential store writes it: a file only this user can
    // read, renamed into place.
    let temp = temp_for(&file);
    let written = fs::write(&temp, kept_bytes(value)).is_ok() && fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).is_ok() && fs::rename(&temp, &file).is_ok();
    if !written {
        let _ = fs::remove_file(&temp);
    }
    written
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
        // Empty, as a power cut could leave it when it was renamed into place unflushed: the
        // same.
        fs::write(&path, b"").unwrap();
        let (settings, note) = Settings::load_checked(&path);
        assert!(settings.server == "https://music.example.org" && note.unwrap().contains("damaged"));
        // No copy either: the damaged file is kept under a name of its own, which is said.
        fs::write(&path, b"not settings at all").unwrap();
        fs::remove_file(dir.join("settings.backup.json")).unwrap();
        let (settings, note) = Settings::load_checked(&path);
        let aside = fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).find(|name| name.starts_with("settings.damaged-")).expect("not kept");
        assert!(settings.server.is_empty() && note.unwrap().contains(&aside));
        assert_eq!(fs::read(dir.join(&aside)).unwrap(), b"not settings at all");
        assert!(!path.exists());
        // It couldn't be moved: it isn't said to have been kept. (A folder where its name
        // would go, for every name: the move fails.)
        fs::write(&path, b"not settings at all").unwrap();
        let note = Settings::load_checked_by(&path, |_, _| Err(std::io::Error::other("the disk said no"))).1.unwrap();
        assert!(!note.contains("kept as") && note.contains("couldn't be moved"), "{note}");
        let _ = fs::remove_dir_all(&dir);
    }


    /// A disk on which what was never flushed is lost at a power cut: `flush` notes that
    /// what was written is on it, and `place`, given the file its name, then cuts the power,
    /// and the name keeps only what was flushed.
    mod power_cut {
        use super::*;
        use std::cell::Cell;

        thread_local!(static FLUSHED: Cell<bool> = const { Cell::new(false) });

        pub const STEPS: Steps = Steps {
            flush: |file| {
                FLUSHED.set(true);
                file.sync_all()
            },
            place: |from, to| {
                fs::rename(from, to)?;
                if !FLUSHED.get() {
                    fs::File::create(to)?;
                }
                panic!("the power went")
            },
            ..REPLACING
        };

        pub fn reset() {
            FLUSHED.set(false);
        }
    }

    /// Writes `bytes` over `path` and has the app "killed" where `steps` panic: what was done
    /// up to there stays on the disk, and nothing after it is done.
    fn killed(path: &Path, bytes: &str, steps: Steps) -> bool {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write_whole(path, bytes.as_bytes(), Durable::Flushed, &steps))).is_err()
    }

    #[test]
    fn a_file_written_over_is_the_old_one_or_the_new_one_after_a_kill_or_a_power_cut() {
        let dir = std::env::temp_dir().join(format!("nexus-whole-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.json");
        fs::write(&path, "the old queue").unwrap();
        // Killed once the file beside it was made, and once it was written and not yet
        // renamed: the file is as it was.
        let made_then_killed = Steps {
            create: |path| {
                let _empty = (REPLACING.create)(path)?;
                panic!("killed")
            },
            ..REPLACING
        };
        assert!(killed(&path, "the new queue", made_then_killed));
        assert_eq!(fs::read_to_string(&path).unwrap(), "the old queue");
        assert!(killed(&path, "the new queue", Steps { place: |_, _| panic!("killed"), ..REPLACING }));
        assert_eq!(fs::read_to_string(&path).unwrap(), "the old queue");
        // What a killed run left beside it, half written, is never read; nor is it cleared
        // by a write in passing (it could be another write's, under way), only when the app
        // starts, before anything is written.
        let left = [dir.join("queue.json.999999.0.tmp"), dir.join("queue.tmp")];
        for left in &left {
            fs::write(left, "the new q").unwrap();
        }
        write_surely(&path, b"the new queue").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "the new queue");
        assert!(left.iter().all(|left| fs::read_to_string(left).unwrap() == "the new q"));
        clear_settings_leftovers(&dir, "", "");
        assert_eq!(names_in(&dir), ["queue.json"]);
        // The power goes the moment the new file has its name: it is the whole new file,
        // because it was flushed before it was given the name. (Not flushed, as
        // `write_atomically` did before, the name would be left with nothing in it.)
        power_cut::reset();
        assert!(killed(&path, "the newest queue", power_cut::STEPS));
        assert_eq!(fs::read_to_string(&path).unwrap(), "the newest queue");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_flush_or_rename_that_fails_leaves_the_file_as_it_was() {
        let dir = std::env::temp_dir().join(format!("nexus-whole-fail-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pending.json");
        fn fails() -> std::io::Result<()> {
            Err(std::io::Error::other("the disk said no"))
        }
        fs::write(&path, "as it was").unwrap();
        for steps in [Steps { flush: |_| fails(), ..REPLACING }, Steps { place: |_, _| fails(), ..REPLACING }] {
            assert!(write_whole(&path, b"new", Durable::Flushed, &steps).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), "as it was");
            assert_eq!(names_in(&dir), ["pending.json"]);
        }
        // Renamed, and its folder not synced (Linux): it is in place, and so it is written,
        // not "as it was": a caller that takes a change back on an error would otherwise
        // take back what is on the disk. (A caller that must know the name will outlast a
        // power cut is told that it may not.)
        assert!(write_whole(&path, b"new", Durable::Flushed, &Steps { sync_folder: |_| fails(), ..REPLACING }).is_ok());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let _ = fs::remove_dir_all(&dir);
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// A second write of the same file, run whole while the first is at one of its steps.
    mod two_writers {
        use super::*;
        use std::cell::RefCell;

        thread_local! {
            /// The file both write.
            pub static DEST: RefCell<PathBuf> = RefCell::new(PathBuf::new());
            static FIRST_PLACE: RefCell<Option<(PathBuf, PathBuf)>> = const { RefCell::new(None) };
        }

        pub const FIRST: &[u8] = b"the first write, whole";
        pub const SECOND: &[u8] = b"the second write, whole";

        fn dest() -> PathBuf {
            DEST.with(|dest| dest.borrow().clone())
        }

        pub fn second(steps: &Steps) -> std::io::Result<()> {
            write_whole(&dest(), SECOND, Durable::Flushed, steps).map(drop)
        }

        pub const AT_CREATE: Steps = Steps {
            create: |path| {
                let file = (REPLACING.create)(path)?;
                second(&REPLACING)?;
                Ok(file)
            },
            ..REPLACING
        };
        pub const AT_FLUSH: Steps = Steps {
            flush: |file| {
                (REPLACING.flush)(file)?;
                second(&REPLACING)
            },
            ..REPLACING
        };
        pub const AT_PLACE: Steps = Steps {
            place: |from, to| {
                second(&REPLACING)?;
                (REPLACING.place)(from, to)
            },
            ..REPLACING
        };
        /// The first gives its file its name in the middle of the second's write: once the
        /// second has flushed its own, and before the second gives it the name.
        pub const INSIDE: Steps = Steps {
            place: |from, to| {
                FIRST_PLACE.with(|first| *first.borrow_mut() = Some((from.to_path_buf(), to.to_path_buf())));
                second(&Steps {
                    flush: |file| {
                        (REPLACING.flush)(file)?;
                        let (from, to) = FIRST_PLACE.with(|first| first.borrow_mut().take()).unwrap();
                        (REPLACING.place)(&from, &to)
                    },
                    ..REPLACING
                })
            },
            ..REPLACING
        };
        /// Made once: the second is given the name while the first is flushing its own.
        pub const ONCE_RACED: Steps = Steps {
            flush: |file| {
                (ONCE.flush)(file)?;
                second(&ONCE)
            },
            ..ONCE
        };
    }

    #[test]
    fn two_writes_of_one_file_never_publish_each_others_bytes() {
        use two_writers::*;
        let dir = std::env::temp_dir().join(format!("nexus-two-writers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("asking.json");
        DEST.with(|dest| *dest.borrow_mut() = path.clone());
        // Whichever step the first is at when the second runs, each writes only its own file
        // and gives the name only its own: both are done, the one that gave the name last is
        // there whole, and nothing is left beside it.
        for (at, steps, last) in [("create", AT_CREATE, FIRST), ("flush", AT_FLUSH, FIRST), ("place", AT_PLACE, FIRST), ("inside", INSIDE, SECOND)] {
            fs::write(&path, "as it was").unwrap();
            assert!(write_whole(&path, FIRST, Durable::Flushed, &steps).is_ok(), "at {at}");
            assert_eq!(fs::read(&path).unwrap(), last, "at {at}");
            assert_eq!(names_in(&dir), ["asking.json"], "at {at}");
        }
        // Made once (the kept copy, the start of the log): the second gives it the name
        // first, and the first finds it there, and leaves it.
        fs::remove_file(&path).unwrap();
        let first = write_whole(&path, FIRST, Durable::Flushed, &ONCE_RACED);
        assert_eq!(first.map_err(|error| error.kind()), Err(std::io::ErrorKind::AlreadyExists));
        assert_eq!(fs::read(&path).unwrap(), SECOND);
        assert_eq!(names_in(&dir), ["asking.json"]);
        // Two files of one name but for what follows the last dot, in one folder (the log of
        // plays and the list from before it), written at once.
        let (log, list) = (dir.join("plays.log"), dir.join("plays.json"));
        DEST.with(|dest| *dest.borrow_mut() = list.clone());
        assert!(write_whole(&log, FIRST, Durable::Flushed, &AT_FLUSH).is_ok());
        assert_eq!((fs::read(&log).unwrap(), fs::read(&list).unwrap()), (FIRST.to_vec(), SECOND.to_vec()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_in_passing_is_renamed_into_place_without_waiting_for_the_disk() {
        let dir = std::env::temp_dir().join(format!("nexus-in-passing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.json");
        fs::write(&path, "as it was").unwrap();
        // Neither flushed nor its folder synced: whole at its name all the same, and said not
        // to be synced.
        let waits = Steps { flush: |_| panic!("flushed"), sync_folder: |_| panic!("synced"), ..REPLACING };
        assert_eq!(write_whole(&path, b"new", Durable::InPassing, &waits).unwrap(), Placed::Unsynced);
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        // Flushed, it is both.
        let flushed = std::panic::catch_unwind(|| write_whole(&path, b"newer", Durable::Flushed, &waits));
        assert!(flushed.is_err(), "a flushed write that didn't flush");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_change_is_on_the_disk_when_its_save_returns_whatever_else_is_being_written() {
        let dir = std::env::temp_dir().join(format!("nexus-save-now-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        // The queue's write, slow on its own thread (a stalled disk, a network drive).
        let saver = crate::saver::Saver::new();
        saver.write(dir.join("queue.json"), || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            Some(b"[]".to_vec())
        });
        let started = std::time::Instant::now();
        Settings { theme: "Violet".into(), ..Default::default() }.save(&path);
        assert!(path.exists(), "the settings weren't on the disk when their save returned");
        assert!(Settings { theme: "Ocean".into(), ..Default::default() }.save_surely(&path));
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "a save waited for another file's write");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_move_aside_that_couldnt_remove_the_original_is_no_move() {
        let dir = std::env::temp_dir().join(format!("nexus-aside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("asked.json");
        fs::write(&path, "half a list").unwrap();
        // (On Linux the move is a second name, then the first removed: here the second is
        // made and the first can't be removed.)
        let linked_only: fn(&Path, &Path) -> std::io::Result<()> = |from, to| fs::copy(from, to).map(drop);
        assert_eq!(set_aside_by(&path, linked_only), None);
        let names: Vec<String> = fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["asked.json"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drive_that_cant_make_hard_links_is_said_once() {
        // Linux keeps what is made once with a second name for it (`link`): on a drive that
        // can't make one, that is said, once ever, plainly; not each time it is tried.
        // (Given the finding, not the one flag the whole run shares: another test may set it.)
        let mut said = false;
        assert_eq!(no_links_note_given(false, &mut said), None);
        let first = no_links_note_given(true, &mut said);
        assert!(first.is_some_and(|note| note.contains("hard links")), "{first:?}");
        assert_eq!(no_links_note_given(true, &mut said), None);
        let mut said_before = true;
        assert_eq!(no_links_note_given(true, &mut said_before), None);
    }

    #[test]
    fn what_killed_runs_left_is_cleared_at_the_start_and_only_what_is_provably_the_apps() {
        let root = std::env::temp_dir().join(format!("nexus-leftovers-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let names = |dir: &Path| {
            let mut names: Vec<String> = fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        let made = |dir: &Path, list: &[&str]| {
            fs::create_dir_all(dir).unwrap();
            for name in list {
                fs::write(dir.join(name), "x").unwrap();
            }
        };
        // The settings folder, for the account signed in ("acct-1a2b" in the kept copy's
        // name, "server_ann" in the lists'): this version's names for its own files, and the
        // exact names 0.40.10 and before could leave. Not another account's, nor anything
        // else.
        let settings = root.join("config");
        let cleared = [
            "settings.json.4242.0.tmp",
            "plays.log.1.17.tmp",
            "pending-server_ann.json.9.3.tmp",
            "shared-record-kept-acct-1a2b-0.40.11.json.77.0.tmp",
            "settings.tmp",
            "queue.tmp",
            "queue-place.tmp",
            "history.tmp",
            "playlist-added.tmp",
            "plays.tmp",
            "asking.json.part",
            "pending-server_ann.tmp",
            "shared-record-kept-acct-1a2b-0.40.10.tmp",
        ];
        let left = [
            "asked.json",
            "notes.tmp",
            "pending-server_bob.tmp",
            "pending-server_bob.json.9.3.tmp",
            "queue.json.12.tmp",
            "settings.json.x.0.tmp",
            "settings.json.4242.0.tmp.txt",
            "settingsx.tmp",
            "shared-record-kept-acct-9f9f-0.40.10.tmp",
            "shared-record-kept-acct-1a2b-notes.json.77.0.tmp",
        ];
        made(&settings, &cleared);
        made(&settings, &left);
        clear_settings_leftovers(&settings, "acct-1a2b", "server_ann");
        let mut left = left.to_vec();
        left.sort();
        assert_eq!(names(&settings), left);
        // The data folder: the crash note's marker's.
        let data = root.join("data");
        made(&data, &["crash.seen.5.0.tmp", "crash.tmp", "diagnostics.txt"]);
        clear_data_leftovers(&data);
        assert_eq!(names(&data), ["crash.tmp", "diagnostics.txt"]);
        // A downloads folder, which may be one of the person's own: only the account's two
        // lists', by exact name. Their own index-tax.tmp is never touched.
        let downloads = root.join("Music");
        made(&downloads, &["index-server_ann.json.4242.1.tmp", "collections-server_ann.json.4242.2.tmp", "index-server_ann.tmp", "collections-server_ann.tmp"]);
        made(&downloads, &["index-tax.tmp", "index-server_bob.tmp", "collections-2024.tmp", "song.flac"]);
        clear_downloads_leftovers(&downloads, "server_ann");
        assert_eq!(names(&downloads), ["collections-2024.tmp", "index-server_bob.tmp", "index-tax.tmp", "song.flac"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn every_file_the_app_keeps_is_in_the_table_of_them() {
        let paths = Paths::at(Path::new("config"), Path::new("cache"), Path::new("data"));
        let table = include_str!("../docs/FILES.md");
        let name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();
        let folder = |path: &Path| format!("{}\\", name(path));
        // Each named by what the app names it with, for an account and a version.
        let (account, version) = ("<account>", "<version>");
        let pages = paths.pages.join("<sign-in>");
        let mut named = vec![
            name(&crash_seen(&paths)),
            name(&pending_file(&paths, account)),
            name(&library_file(&pages)),
            folder(&lyrics_dir(&pages)),
            name(&crate::sync::kept_file(&paths.settings, account, version)),
            name(&crate::downloads::index_file(&paths.downloads, account)),
            name(&crate::downloads::collections_file(&paths.downloads, account)),
            name(&crate::plays::log_file(&paths.plays)),
            name(&crate::plays::side_file(&crate::plays::log_file(&paths.plays))),
            name(&backup_of(&paths.settings)),
            "settings.damaged-<date and time>.json".into(),
            // Moved aside under a name of its own (`set_aside`), and a write's own file
            // beside its place (`temp_for`).
            "asked.damaged-<date and time>.json".into(),
            "asking.damaged-<date and time>.json".into(),
            "<file>.<process>.<count>.tmp".into(),
        ];
        let temp = name(&temp_for(Path::new("<file>")));
        let parts: Vec<&str> = temp.split('.').collect();
        assert!(parts.len() == 4 && parts[0] == "<file>" && parts[3] == "tmp" && parts[1..3].iter().all(|part| part.parse::<u64>().is_ok()), "{temp}");
        // And every one of `Paths`, named one by one, so that a file added there can't be
        // left out of this.
        let Paths {
            settings,
            queue,
            queue_place,
            exports,
            crash_log,
            diagnostics,
            history,
            pending: _,
            covers,
            pages: _,
            buffers,
            songs,
            downloads,
            added,
            asked,
            asking,
            discover,
            old_lyrics,
            plays,
            #[cfg(not(windows))]
            secret,
        } = paths;
        named.extend([&settings, &queue, &queue_place, &crash_log, &diagnostics, &history, &added, &asked, &asking, &discover, &plays].map(|path| name(path)));
        named.extend([&exports, &covers, &buffers, &songs, &downloads, &old_lyrics].map(|path| folder(path)));
        named.push("pages\\".into());
        #[cfg(not(windows))]
        named.extend([name(&secret), "secret-<name>".into()]);
        for named in named {
            assert!(table.contains(&named), "docs/FILES.md doesn't say how {named} is kept");
        }
        // Every row says all of it: what it holds, how it is written, what a kill leaves, what
        // a power cut leaves, and how one that can't be read is taken.
        for row in table.lines().filter(|line| line.starts_with("| ")) {
            assert_eq!(row.matches(" | ").count() + 2, 7, "a row without all six: {row}");
        }
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
