//! How one person's devices find each other, and what one may ask of another.
//!
//! The design is `docs/linking-design.md` (version 3). In short: the devices of an account
//! share one random secret, kept in a hidden playlist on the music server ([`secret`]);
//! everything they say to each other through the relay is sealed with a key made from it
//! ([`frame`]); they say two kinds of thing, an ask and a tell ([`message`]); and a device
//! carries out what another asks only if nothing has been chosen on it since the state the
//! ask was made against ([`rule`]). [`steer`] is the asking side: what makes a state fresh
//! enough to act on, and what to conclude when no answer comes. What may be asked is in
//! [`command`]; [`moving`] is music going from one device to another; and [`news`] is a
//! device saying, unasked, that it has changed.
//!
//! Nothing here has a clock, a source of randomness, a connection or a player. An app
//! hands in the time (milliseconds, on a clock that **goes on counting while the device
//! sleeps**), random bytes from the system, and the server's answers, and is told what to do.
//! Nothing here panics, whatever it is given.

pub mod bind;
pub mod command;
pub mod device;
pub mod frame;
pub mod look;
pub mod message;
pub mod moving;
pub mod news;
pub mod relay;
pub mod rule;
pub mod secret;
pub mod seen;
pub mod steer;
pub mod text;
pub mod view;
pub mod words;

/// What the relay's route for this is called in its `/api/hello`.
pub const VERSION: u64 = 4;

/// A device's place at the relay: random for each run of the app.
pub type Place = [u8; 16];
/// A frame's nonce: random for every frame. It is also how an answer names its ask.
pub type Nonce = [u8; 24];
/// A device, as it names itself to the others: made once for an installation. Not secret,
/// and only ever sent sealed.
pub type DeviceId = [u8; 16];

/// How long after one look at the secret finished the next one starts.
pub const LOOK_AGAIN_MS: u64 = 2 * 60 * 1000;
/// How long a secret is used, counted from the start of the look that read it.
pub const SECRET_LIFE_MS: u64 = 10 * 60 * 1000;
/// How often the device this one is steering is looked at.
pub const LOOK_EVERY_MS: u64 = 5 * 1000;
/// How often every other device is looked at. In between, its news prompts a look.
pub const LOOK_IDLE_MS: u64 = 30 * 1000;
/// How old a state may be, counted from when its look was sent, and still be acted on.
pub const FRESH_MS: u64 = 45 * 1000;
/// How long an answer is waited for before the asking device stops waiting.
pub const WAIT_MS: u64 = 12 * 1000;
/// The most asks a device answers in a second.
pub const ASKS_A_SECOND: u32 = 20;

/// Whole minutes since the app started, from its clock in milliseconds.
pub fn minute(now_ms: u64) -> u64 {
    now_ms / 60_000
}

/// Whether a secret read by a look that started at `since_ms` may still be used. Asked
/// every time the secret is used, so that a device that slept through its ten minutes
/// finds out on waking.
pub fn secret_holds(since_ms: u64, now_ms: u64) -> bool {
    now_ms.checked_sub(since_ms).is_some_and(|age| age < SECRET_LIFE_MS)
}

/// How many asks a device has answered this second. More than [`ASKS_A_SECOND`] are dropped.
#[derive(Debug, Default)]
pub struct Asks {
    second: u64,
    answered: u32,
}

impl Asks {
    /// Whether one more ask may be answered now.
    pub fn allow(&mut self, now_ms: u64) -> bool {
        let second = now_ms / 1000;
        if second != self.second {
            (self.second, self.answered) = (second, 0);
        }
        self.answered = self.answered.saturating_add(1);
        self.answered <= ASKS_A_SECOND
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_runs_out_ten_minutes_after_its_look_began() {
        assert!(secret_holds(1_000, 1_000));
        assert!(secret_holds(1_000, 1_000 + SECRET_LIFE_MS - 1));
        assert!(!secret_holds(1_000, 1_000 + SECRET_LIFE_MS));
        // A clock that reads earlier than the look is not one to trust the secret by.
        assert!(!secret_holds(1_000, 999));
        assert_eq!((minute(0), minute(59_999), minute(60_000), minute(u64::MAX)), (0, 0, 1, u64::MAX / 60_000));
    }

    #[test]
    fn asks_past_twenty_a_second_are_dropped() {
        let mut asks = Asks::default();
        assert_eq!((0..25).filter(|_| asks.allow(5_000)).count(), 20);
        assert!(!asks.allow(5_999));
        assert!(asks.allow(6_000));
    }
}
