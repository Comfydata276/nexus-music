//! Exclusive-mode output on Windows (WASAPI).
//!
//! In the usual shared mode, Windows resamples every app to one rate and mixes them. In
//! exclusive mode the app has the device to itself and hands it samples in the device's own
//! format, at the song's own sample rate: nothing in between changes them.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_E_DEVICE_IN_USE, AUDCLNT_SHAREMODE_EXCLUSIVE,
    IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::core::{HSTRING, PCWSTR};

use super::dsp::Effects;
use super::engine::{CallbackState, Shared, fill};

/// How samples are laid out in the device's buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    I16,
    /// Three bytes per sample.
    I24,
    /// 24 meaningful bits at the top of a 32-bit sample.
    I24In32,
    I32,
    F32,
}

impl Layout {
    /// (bits stored per sample, bits that carry audio)
    fn bits(self) -> (u16, u16) {
        match self {
            Layout::I16 => (16, 16),
            Layout::I24 => (24, 24),
            Layout::I24In32 => (32, 24),
            Layout::I32 | Layout::F32 => (32, 32),
        }
    }

    /// The layouts to try for a source of `bits` bits, best match first.
    fn candidates(bits: u32) -> [Layout; 5] {
        if bits != 0 && bits <= 16 {
            [Layout::I16, Layout::I24, Layout::I24In32, Layout::I32, Layout::F32]
        } else {
            [Layout::I24, Layout::I24In32, Layout::I32, Layout::F32, Layout::I16]
        }
    }
}

fn wave_format(rate: u32, layout: Layout) -> WAVEFORMATEXTENSIBLE {
    let (stored, valid) = layout.bits();
    let block = 2 * stored / 8;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: 2,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block as u32,
            nBlockAlign: block,
            wBitsPerSample: stored,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: valid },
        // Front left and front right.
        dwChannelMask: 0x3,
        SubFormat: if layout == Layout::F32 { KSDATAFORMAT_SUBTYPE_IEEE_FLOAT } else { KSDATAFORMAT_SUBTYPE_PCM },
    }
}

/// Rates to try when the device won't take the song's own: first the base rate of the same
/// family (so the conversion is by a whole number), then the common ones.
fn fallback_rates(rate: u32) -> Vec<u32> {
    let mut rates = vec![rate];
    let family = if rate % 44_100 == 0 { 44_100 } else { 48_000 };
    for candidate in [family, 48_000, 44_100, 96_000] {
        if !rates.contains(&candidate) {
            rates.push(candidate);
        }
    }
    rates
}

/// Writes interleaved stereo floats into the device buffer in its layout.
///
/// # Safety
/// `out` must point to room for `samples.len()` samples in `layout`.
unsafe fn write_samples(out: *mut u8, samples: &[f32], layout: Layout) {
    unsafe {
        match layout {
            Layout::I16 => {
                let out = std::slice::from_raw_parts_mut(out as *mut i16, samples.len());
                for (slot, s) in out.iter_mut().zip(samples) {
                    *slot = (s * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16;
                }
            }
            Layout::I24 => {
                let out = std::slice::from_raw_parts_mut(out, samples.len() * 3);
                for (slot, s) in out.chunks_exact_mut(3).zip(samples) {
                    let v = (s * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                    slot.copy_from_slice(&v.to_le_bytes()[..3]);
                }
            }
            Layout::I24In32 => {
                let out = std::slice::from_raw_parts_mut(out as *mut i32, samples.len());
                for (slot, s) in out.iter_mut().zip(samples) {
                    let v = (s * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                    *slot = v << 8;
                }
            }
            Layout::I32 => {
                let out = std::slice::from_raw_parts_mut(out as *mut i32, samples.len());
                for (slot, s) in out.iter_mut().zip(samples) {
                    *slot = (*s as f64 * 2_147_483_648.0).round().clamp(-2_147_483_648.0, 2_147_483_647.0) as i32;
                }
            }
            Layout::F32 => {
                std::slice::from_raw_parts_mut(out as *mut f32, samples.len()).copy_from_slice(samples);
            }
        }
    }
}

/// A running exclusive stream. Dropping it stops playback and frees the device.
pub struct ExclusiveStream {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub rate: u32,
    /// Bits of audio per sample the device is being sent.
    pub bits: u32,
}

impl Drop for ExclusiveStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Opened {
    client: IAudioClient,
    render: IAudioRenderClient,
    /// Frames the device's buffer holds.
    frames: u32,
    rate: u32,
    layout: Layout,
}

/// How much audio the device buffer holds. Long enough to ride out a busy moment on the PC,
/// short enough that pause and volume changes still feel immediate.
const BUFFER_HNS: i64 = 1_000_000; // 100 ms, in units of 100 nanoseconds

/// Describes a failed step for the status line: what was being done, and Windows' reason.
fn at(step: &'static str) -> impl Fn(windows::core::Error) -> String {
    move |error| {
        let reason = error.message();
        if reason.is_empty() {
            format!("{step} failed with error {:#010X}", error.code().0)
        } else {
            format!("{step} failed: {reason}")
        }
    }
}

fn find_device(id: &str) -> Result<IMMDevice, String> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(at("listing devices"))?;
        if !id.is_empty() {
            if let Ok(device) = enumerator.GetDevice(PCWSTR(HSTRING::from(id).as_ptr())) {
                return Ok(device);
            }
        }
        enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(at("finding the device"))
    }
}

fn open_client(device_id: &str, rate: u32, bits: u32) -> Result<Opened, String> {
    unsafe {
        let device = find_device(device_id)?;
        let mut client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(at("opening the device"))?;

        // The first format the device takes, preferring the song's rate and bit depth.
        let mut chosen = None;
        'search: for candidate_rate in fallback_rates(rate) {
            for layout in Layout::candidates(bits) {
                let format = wave_format(candidate_rate, layout);
                let supported =
                    client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &format.Format, None);
                if supported == S_OK {
                    chosen = Some((format, layout));
                    break 'search;
                }
            }
        }
        let (format, layout) =
            chosen.ok_or("the device doesn't offer a stereo format for exclusive use")?;
        let rate = format.Format.nSamplesPerSec;

        // The buffer is topped up on a timer ("push" mode) rather than on the device's
        // wake-up events: it is the form of exclusive mode every driver supports.
        let mut period = 0i64;
        client.GetDevicePeriod(Some(&mut period), None).map_err(at("reading the device's timing"))?;
        let mut duration = BUFFER_HNS.max(period * 2);
        let mut first = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, duration, 0, &format.Format, None);
        // The stream this one replaces can take a moment to let go of the device.
        for _ in 0..10 {
            if !first.as_ref().is_err_and(|error| error.code() == AUDCLNT_E_DEVICE_IN_USE) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            client = device.Activate(CLSCTX_ALL, None).map_err(at("opening the device"))?;
            first = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, duration, 0, &format.Format, None);
        }
        if let Err(error) = first {
            if error.code() != AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
                return Err(at("starting the stream")(error));
            }
            // Some devices want a whole number of frames: ask what that came to and try again
            // with a fresh client, as the API requires.
            let frames = client.GetBufferSize().map_err(at("sizing the buffer"))?;
            duration = (10_000_000.0 * frames as f64 / rate as f64 + 0.5) as i64;
            client = device.Activate(CLSCTX_ALL, None).map_err(at("opening the device"))?;
            client
                .Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, 0, duration, 0, &format.Format, None)
                .map_err(at("starting the stream"))?;
        }

        let frames = client.GetBufferSize().map_err(at("sizing the buffer"))?;
        let render: IAudioRenderClient = client.GetService().map_err(at("setting up the stream"))?;
        Ok(Opened { client, render, frames, rate, layout })
    }
}

/// Takes the device for exclusive use, as close to `rate` and `bits` as it allows.
pub fn open(shared: Arc<Shared>, device_id: String, rate: u32, bits: u32) -> Result<ExclusiveStream, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = stop.clone();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u32), String>>();
    // The device is opened and driven from one thread, as its interfaces expect.
    let thread = std::thread::Builder::new()
        .name("exclusive-output".into())
        .spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let opened = match open_client(&device_id, rate, bits) {
                Ok(opened) => opened,
                Err(reason) => {
                    let _ = ready_tx.send(Err(reason));
                    return;
                }
            };
            let Opened { client, render, frames, rate, layout } = opened;
            let _ = ready_tx.send(Ok((rate, layout.bits().1 as u32)));

            let mut state = CallbackState {
                effects: Effects::new(rate),
                scratch: Vec::with_capacity(frames as usize * 2),
                fade: 0.0,
                volume: 1.0,
                level: 0.0,
            };
            let fade_step = 1.0 / (rate as f32 * 0.012);
            let mut mix = vec![0f32; frames as usize * 2];
            let mut started = false;
            // Top the buffer up several times per buffer-length, so it never runs dry.
            let nap = Duration::from_micros((frames as u64 * 1_000_000 / rate.max(1) as u64 / 5).clamp(2_000, 20_000));
            while !stopped.load(Ordering::Relaxed) {
                // A failure here means the device has gone away (unplugged, or taken over).
                let Ok(queued) = client.GetCurrentPadding() else {
                    shared.rebuild.store(true, Ordering::Relaxed);
                    break;
                };
                let room = frames.saturating_sub(queued);
                if room > 0 {
                    let Ok(buffer) = render.GetBuffer(room) else {
                        shared.rebuild.store(true, Ordering::Relaxed);
                        break;
                    };
                    let chunk = &mut mix[..room as usize * 2];
                    fill::<f32>(chunk, 2, fade_step, &shared, &mut state);
                    write_samples(buffer, chunk, layout);
                    if render.ReleaseBuffer(room, 0).is_err() {
                        shared.rebuild.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                if !started {
                    if client.Start().is_err() {
                        shared.rebuild.store(true, Ordering::Relaxed);
                        break;
                    }
                    started = true;
                }
                std::thread::sleep(nap);
            }
            let _ = client.Stop();
        })
        .map_err(|e| e.to_string())?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok((rate, bits))) => Ok(ExclusiveStream { stop, thread: Some(thread), rate, bits }),
        Ok(Err(reason)) => {
            let _ = thread.join();
            Err(reason)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err("the device didn't respond".into())
        }
    }
}
