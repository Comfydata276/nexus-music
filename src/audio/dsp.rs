//! Output effects applied just before audio reaches the device: the six-band equaliser, mono
//! fold-down and a soft limiter. Settings are read from atomics, so changes are heard at once
//! instead of after the buffered audio has played out.

use std::f32::consts::PI;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Centre frequencies of the equaliser bands, in Hz. The first and last are shelves.
pub const EQ_BANDS: [f32; 6] = [60.0, 150.0, 400.0, 1000.0, 2400.0, 15000.0];
/// Furthest a band can be raised or lowered, in dB.
pub const EQ_RANGE: f32 = 12.0;

/// Effect settings shared between the UI thread and the audio thread.
pub struct Controls {
    pub mono: AtomicBool,
    eq_on: AtomicBool,
    eq_gains: [AtomicU32; 6],
    /// Linear gain applied before the bands, to leave room for the ones turned up.
    eq_preamp: AtomicU32,
    /// Bumped on every equaliser change so the audio thread knows to recompute its filters.
    eq_version: AtomicU64,
}

impl Controls {
    pub fn new() -> Controls {
        Controls {
            mono: AtomicBool::new(false),
            eq_on: AtomicBool::new(false),
            eq_gains: std::array::from_fn(|_| AtomicU32::new(0f32.to_bits())),
            eq_preamp: AtomicU32::new(1f32.to_bits()),
            eq_version: AtomicU64::new(1),
        }
    }

    /// `preamp` is in dB, 0 or below.
    pub fn set_equalizer(&self, on: bool, gains: [f32; 6], preamp: f32) {
        for (slot, gain) in self.eq_gains.iter().zip(gains) {
            slot.store(gain.clamp(-EQ_RANGE, EQ_RANGE).to_bits(), Ordering::Relaxed);
        }
        self.eq_preamp.store(10f32.powf(preamp.clamp(-EQ_RANGE, 0.0) / 20.0).to_bits(), Ordering::Relaxed);
        self.eq_on.store(on, Ordering::Relaxed);
        self.eq_version.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    /// Filter memory, per channel.
    z1: [f32; 2],
    z2: [f32; 2],
}

enum Shape {
    LowShelf,
    Peak,
    HighShelf,
}

impl Biquad {
    /// Coefficients from the Audio EQ Cookbook (Robert Bristow-Johnson).
    fn set(&mut self, shape: Shape, frequency: f32, gain_db: f32, rate: f32) {
        let a = 10f32.powf(gain_db / 40.0);
        let w = 2.0 * PI * frequency.min(rate * 0.45) / rate;
        let (sin, cos) = w.sin_cos();
        let (b0, b1, b2, a0, a1, a2) = match shape {
            Shape::Peak => {
                let alpha = sin / 2.0; // Q of 1: bands about an octave and a third apart
                (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
            }
            Shape::LowShelf => {
                let beta = 2.0 * a.sqrt() * (sin / 2.0 * 2f32.sqrt());
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + beta),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - beta),
                    (a + 1.0) + (a - 1.0) * cos + beta,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - beta,
                )
            }
            Shape::HighShelf => {
                let beta = 2.0 * a.sqrt() * (sin / 2.0 * 2f32.sqrt());
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + beta),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - beta),
                    (a + 1.0) - (a - 1.0) * cos + beta,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - beta,
                )
            }
        };
        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    #[inline]
    fn run(&mut self, channel: usize, x: f32) -> f32 {
        let y = self.b0 * x + self.z1[channel];
        self.z1[channel] = self.b1 * x - self.a1 * y + self.z2[channel];
        self.z2[channel] = self.b2 * x - self.a2 * y;
        y
    }
}

/// Keeps peaks under full scale by rounding them off instead of chopping them flat.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let magnitude = x.abs();
    if magnitude <= KNEE {
        x
    } else {
        let over = (magnitude - KNEE) / (1.0 - KNEE);
        x.signum() * (KNEE + (1.0 - KNEE) * over.tanh())
    }
}

/// The audio thread's side of the effects: filter state and the settings it last saw.
pub struct Effects {
    rate: f32,
    version: u64,
    active: bool,
    preamp: f32,
    bands: [Biquad; 6],
}

impl Effects {
    pub fn new(rate: u32) -> Effects {
        Effects { rate: rate.max(8000) as f32, version: 0, active: false, preamp: 1.0, bands: [Biquad::default(); 6] }
    }

    /// Processes interleaved stereo in place.
    pub fn process(&mut self, controls: &Controls, samples: &mut [f32]) {
        let version = controls.eq_version.load(Ordering::Relaxed);
        if version != self.version {
            self.version = version;
            let on = controls.eq_on.load(Ordering::Relaxed);
            let mut any = false;
            for (index, band) in self.bands.iter_mut().enumerate() {
                let gain = f32::from_bits(controls.eq_gains[index].load(Ordering::Relaxed));
                any |= gain.abs() > 0.05;
                let shape = match index {
                    0 => Shape::LowShelf,
                    5 => Shape::HighShelf,
                    _ => Shape::Peak,
                };
                band.set(shape, EQ_BANDS[index], gain, self.rate);
            }
            self.preamp = f32::from_bits(controls.eq_preamp.load(Ordering::Relaxed));
            self.active = on && (any || self.preamp < 0.999);
        }
        if self.active {
            for frame in samples.chunks_exact_mut(2) {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    let mut value = *sample * self.preamp;
                    for band in self.bands.iter_mut() {
                        value = band.run(channel, value);
                    }
                    // Boosted bands can push peaks past full scale.
                    *sample = soft_clip(value);
                }
            }
        }
        if controls.mono.load(Ordering::Relaxed) {
            for frame in samples.chunks_exact_mut(2) {
                let mixed = (frame[0] + frame[1]) * 0.5;
                frame[0] = mixed;
                frame[1] = mixed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Level of a sine at `frequency` after the equaliser, in dB relative to the input.
    fn response(gains: [f32; 6], frequency: f32) -> f32 {
        response_with(gains, 0.0, frequency)
    }

    fn response_with(gains: [f32; 6], preamp: f32, frequency: f32) -> f32 {
        let rate = 48_000;
        let controls = Controls::new();
        controls.set_equalizer(true, gains, preamp);
        let mut effects = Effects::new(rate);
        let mut samples: Vec<f32> = (0..rate * 2)
            .flat_map(|n| {
                let v = 0.1 * (2.0 * PI * frequency * n as f32 / rate as f32).sin();
                [v, v]
            })
            .collect();
        effects.process(&controls, &mut samples);
        // Skip the first second while the filters settle.
        let rms = |s: &[f32]| (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt();
        20.0 * (rms(&samples[rate as usize * 2..]) / (0.1 / 2f32.sqrt())).log10()
    }

    #[test]
    fn flat_settings_leave_audio_untouched() {
        assert!(response([0.0; 6], 1000.0).abs() < 0.01);
    }

    #[test]
    fn a_band_boosts_its_own_frequency_and_little_else() {
        let gains = [0.0, 0.0, 0.0, 6.0, 0.0, 0.0];
        assert!((response(gains, 1000.0) - 6.0).abs() < 0.3);
        assert!(response(gains, 60.0).abs() < 0.5);
        assert!(response(gains, 12_000.0).abs() < 0.5);
    }

    #[test]
    fn shelves_reach_their_gain_at_the_extremes() {
        assert!((response([-9.0, 0.0, 0.0, 0.0, 0.0, 0.0], 25.0) + 9.0).abs() < 0.6);
        assert!((response([0.0, 0.0, 0.0, 0.0, 0.0, 8.0], 20_000.0) - 8.0).abs() < 0.6);
    }

    #[test]
    fn soft_clip_stays_below_full_scale() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(3.0) <= 1.0 && soft_clip(3.0) > 0.95);
        assert!(soft_clip(-3.0) >= -1.0);
    }

    #[test]
    fn the_preamp_turns_everything_down_before_the_bands() {
        // Flat bands with 6 dB of preamp: 6 dB quieter everywhere.
        assert!((response_with([0.0; 6], -6.0, 1000.0) + 6.0).abs() < 0.2);
        // A 6 dB boost at 1 kHz with 6 dB of preamp comes out level there, and quieter elsewhere.
        let boosted = [0.0, 0.0, 0.0, 6.0, 0.0, 0.0];
        assert!(response_with(boosted, -6.0, 1000.0).abs() < 0.3);
        assert!((response_with(boosted, -6.0, 60.0) + 6.0).abs() < 0.5);
        // It only ever turns down.
        assert!(response_with([0.0; 6], 6.0, 1000.0).abs() < 0.2);
    }
}
