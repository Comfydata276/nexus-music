//! Cover art: fetched once, kept on disk, decoded off the UI thread and held in a bounded
//! in-memory cache.
//!
//! The interface asks for art from inside property bindings, so `get` must answer at once.
//! A miss returns nothing, starts a load, and bumps `Covers.rev` when the picture is ready,
//! which makes every binding that reads it ask again.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use slint::{Color, ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer};

use crate::controller::{Controller, post};
use crate::subsonic::Client;
use crate::ui;

/// Decoded pixels kept in memory at most.
const MEMORY_BUDGET: usize = 64 * 1024 * 1024;

type Key = (String, u32);

struct Entry {
    image: Image,
    bytes: usize,
    used: u64,
}

pub struct Covers {
    dir: PathBuf,
    cache: RefCell<HashMap<Key, Entry>>,
    bytes: Cell<usize>,
    clock: Cell<u64>,
    pending: RefCell<HashSet<Key>>,
    /// Pictures that couldn't be fetched, and when: each is left alone for `RETRY_AFTER`.
    failed: RefCell<HashMap<Key, Instant>>,
    tints: RefCell<HashMap<String, Color>>,
    bump: slint::Timer,
    bump_queued: Cell<bool>,
}

/// How long a picture that couldn't be fetched is left before it's asked for again. The
/// reason is usually the connection, and that comes back.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// The sizes pictures are kept at. Thumbnails are cut down locally from `MEDIUM`, so a cover
/// seen as a card needs nothing more from the server to appear in a track row or the player.
const SMALL: u32 = 96;
const MEDIUM: u32 = 320;
const LARGE: u32 = 640;

/// Picks the stored resolution for an on-screen size, counting real pixels on scaled displays.
fn bucket(size: i32, scale: f32) -> u32 {
    let pixels = size as f32 * scale.max(1.0);
    if pixels <= 110.0 {
        SMALL
    } else if pixels <= 420.0 {
        MEDIUM
    } else {
        LARGE
    }
}

/// Files being downloaded right now, so two requests for one picture share a single fetch.
static IN_FLIGHT: (Mutex<Option<HashSet<PathBuf>>>, Condvar) = (Mutex::new(None), Condvar::new());

impl Covers {
    pub fn new(dir: PathBuf) -> Covers {
        Covers {
            dir,
            cache: RefCell::default(),
            bytes: Cell::new(0),
            clock: Cell::new(0),
            pending: RefCell::default(),
            failed: RefCell::default(),
            tints: RefCell::default(),
            bump: slint::Timer::default(),
            bump_queued: Cell::new(false),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn get(&self, ctl: &Controller, id: &str, size: i32) -> Image {
        if id.is_empty() {
            return Image::default();
        }
        let key = (id.to_string(), bucket(size, ctl.ui.window().scale_factor()));
        let now = self.clock.get() + 1;
        self.clock.set(now);
        if let Some(entry) = self.cache.borrow_mut().get_mut(&key) {
            entry.used = now;
            return entry.image.clone();
        }
        let resting = self.failed.borrow().get(&key).is_some_and(|at| at.elapsed() < RETRY_AFTER);
        if !resting && self.pending.borrow_mut().insert(key.clone()) {
            if let Some(client) = ctl.client() {
                let dir = self.dir.clone();
                ctl.art_pool.spawn(move || {
                    let loaded = load(&client, &dir, &key.0, key.1);
                    post(move |ctl| ctl.covers.finish(ctl, key, loaded));
                });
            }
        }
        // While the right size loads, any other size of the same picture beats a blank.
        let mut cache = self.cache.borrow_mut();
        for other in [MEDIUM, LARGE, SMALL] {
            if let Some(entry) = cache.get_mut(&(id.to_string(), other)) {
                entry.used = now;
                return entry.image.clone();
            }
        }
        Image::default()
    }

    /// The wash colour for a cover, or a neutral plum until it has loaded.
    pub fn tint(&self, id: &str) -> Color {
        self.tints.borrow().get(id).copied().unwrap_or(Color::from_rgb_u8(0x3a, 0x30, 0x52))
    }

    fn finish(&self, ctl: &Controller, key: Key, loaded: Option<Loaded>) {
        self.pending.borrow_mut().remove(&key);
        match loaded {
            Some(loaded) => {
                let bytes = loaded.pixels.as_bytes().len();
                let [r, g, b] = loaded.tint;
                self.tints.borrow_mut().insert(key.0.clone(), Color::from_rgb_u8(r, g, b));
                let entry = Entry {
                    image: Image::from_rgba8(loaded.pixels),
                    bytes,
                    used: self.clock.get(),
                };
                self.failed.borrow_mut().remove(&key);
                self.cache.borrow_mut().insert(key, entry);
                self.bytes.set(self.bytes.get() + bytes);
                self.evict();
            }
            None => {
                self.failed.borrow_mut().insert(key, Instant::now());
            }
        }
        self.redraw(ctl);
    }

    /// The server is back: pictures that couldn't be fetched are asked for again, starting
    /// with the ones on screen.
    pub fn retry_failed(&self, ctl: &Controller) {
        if !self.failed.borrow().is_empty() {
            self.failed.borrow_mut().clear();
            self.redraw(ctl);
        }
    }

    /// Has everything showing a picture look it up again, shortly.
    fn redraw(&self, ctl: &Controller) {
        // Art tends to arrive in bursts; one refresh covers the lot.
        if !self.bump_queued.replace(true) {
            let weak = ctl.ui.as_weak();
            self.bump.start(slint::TimerMode::SingleShot, Duration::from_millis(40), move || {
                crate::controller::with(|ctl| ctl.covers.bump_queued.set(false));
                if let Some(ui) = weak.upgrade() {
                    let covers = ui.global::<ui::Covers>();
                    covers.set_rev(covers.get_rev().wrapping_add(1));
                }
            });
        }
    }

    fn evict(&self) {
        if self.bytes.get() <= MEMORY_BUDGET {
            return;
        }
        let mut cache = self.cache.borrow_mut();
        let mut by_age: Vec<(u64, Key)> = cache.iter().map(|(k, e)| (e.used, k.clone())).collect();
        by_age.sort_unstable_by_key(|(used, _)| *used);
        let mut total = self.bytes.get();
        for (_, key) in by_age {
            if total <= MEMORY_BUDGET * 3 / 4 {
                break;
            }
            if let Some(entry) = cache.remove(&key) {
                total -= entry.bytes;
            }
        }
        self.bytes.set(total);
    }

    /// Forgets everything, e.g. after signing in to a different server.
    pub fn reset(&self) {
        self.cache.borrow_mut().clear();
        self.bytes.set(0);
        self.failed.borrow_mut().clear();
        self.tints.borrow_mut().clear();
    }

    pub fn clear_disk(&self) {
        if let Ok(entries) = fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        self.failed.borrow_mut().clear();
    }
}

struct Loaded {
    pixels: SharedPixelBuffer<Rgba8Pixel>,
    tint: [u8; 3],
}

fn file_name(id: &str, bucket: u32) -> String {
    let safe: String = id
        .chars()
        .take(120)
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    format!("{safe}.{bucket}")
}

/// The cache file for a picture that's named by its web address, not by the server.
fn remote_file_name(address: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(address.as_bytes());
    format!("web-{}", digest.iter().take(12).map(|byte| format!("{byte:02x}")).collect::<String>())
}

/// Reads a cover file from the disk cache, downloading it first if needed. An `id` that's a
/// web address (the cover of an album that isn't in the library) is fetched from there.
fn cover_file(client: &Client, dir: &Path, id: &str, size: u32) -> Option<(Vec<u8>, PathBuf)> {
    let remote = id.starts_with("https://") || id.starts_with("http://");
    let path = dir.join(if remote { remote_file_name(id) } else { file_name(id, size) });
    let (lock, done) = &IN_FLIGHT;
    {
        let mut busy = lock.lock().unwrap();
        loop {
            if let Ok(bytes) = fs::read(&path) {
                return Some((bytes, path));
            }
            let set = busy.get_or_insert_with(HashSet::new);
            if set.insert(path.clone()) {
                break;
            }
            // Someone else is already fetching this file: wait for them to finish.
            busy = done.wait(busy).unwrap();
        }
    }
    let address = if remote { id.to_string() } else { client.cover_url(id, size) };
    // A cover from another site carries nothing of the account's; one from the server does.
    let fetched = if remote { client.fetch_elsewhere(&address, 16 * 1024 * 1024) } else { client.fetch_bytes(&address, 16 * 1024 * 1024) }.ok();
    if let Some(bytes) = &fetched {
        // Write beside it and rename, so nobody reads a half-written file.
        let partial = path.with_extension("part");
        if fs::write(&partial, bytes).is_ok() {
            let _ = fs::rename(&partial, &path);
        }
    }
    if let Some(set) = lock.lock().unwrap().as_mut() {
        set.remove(&path);
    }
    done.notify_all();
    fetched.map(|bytes| (bytes, path))
}

fn load(client: &Client, dir: &Path, id: &str, bucket: u32) -> Option<Loaded> {
    let source = if bucket == SMALL { MEDIUM } else { bucket };
    let (bytes, path) = cover_file(client, dir, id, source)?;
    let Ok(decoded) = image::load_from_memory(&bytes) else {
        let _ = fs::remove_file(&path);
        return None;
    };
    // Never hold more pixels than the bucket needs; servers may also ignore the size hint.
    let decoded = if decoded.width().max(decoded.height()) > bucket + bucket / 4 {
        decoded.thumbnail(bucket, bucket)
    } else {
        decoded
    };
    let rgba = decoded.to_rgba8();
    let tint = dominant_colour(&rgba);
    let pixels =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), rgba.width(), rgba.height());
    Some(Loaded { pixels, tint })
}

/// Makes sure a cover is in the disk cache and returns where. Used for the Windows media
/// overlay, and to keep artwork for downloaded songs.
pub fn ensure_file(client: &Client, dir: &Path, id: &str) -> Option<PathBuf> {
    cover_file(client, dir, id, MEDIUM).map(|(_, path)| path)
}

/// A cover's file as the server sent it, for passing on to someone else: at the size the
/// big artwork uses when `large`, otherwise at the size rows are cut from.
pub fn file_bytes(client: &Client, dir: &Path, id: &str, large: bool) -> Option<Vec<u8>> {
    cover_file(client, dir, id, if large { LARGE } else { MEDIUM }).map(|(bytes, _)| bytes)
}

/// A representative colour for the artwork, weighted towards its vivid pixels and adjusted so
/// white text stays readable on top of it.
fn dominant_colour(image: &image::RgbaImage) -> [u8; 3] {
    let step = (image.width().max(image.height()) / 32).max(1) as usize;
    let (mut r, mut g, mut b, mut weight) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for y in (0..image.height()).step_by(step) {
        for x in (0..image.width()).step_by(step) {
            let p = image.get_pixel(x, y).0;
            let (pr, pg, pb) = (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
            let max = pr.max(pg).max(pb);
            let min = pr.min(pg).min(pb);
            let saturation = if max > 0.0 { (max - min) / max } else { 0.0 };
            let w = 0.05 + saturation * max;
            r += pr * w;
            g += pg * w;
            b += pb * w;
            weight += w;
        }
    }
    if weight <= 0.0 {
        return [0x3a, 0x30, 0x52];
    }
    let (r, g, b) = (r / weight, g / weight, b / weight);
    // Pull the brightness into a mid band: dark enough for text, bright enough to read as colour.
    let max = r.max(g).max(b).max(0.001);
    let target = max.clamp(0.42, 0.72);
    let scale = target / max;
    [(r * scale * 255.0) as u8, (g * scale * 255.0) as u8, (b * scale * 255.0) as u8]
}

/// Deletes the oldest files in a cache folder once it outgrows `budget` bytes.
pub fn trim_disk(dir: &Path, budget: u64) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            meta.is_file().then(|| (meta.modified().unwrap_or(std::time::UNIX_EPOCH), meta.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= budget {
        return;
    }
    files.sort_unstable_by_key(|f| f.0);
    for (_, size, path) in files {
        if total <= budget * 3 / 4 {
            break;
        }
        if fs::remove_file(path).is_ok() {
            total -= size;
        }
    }
}
