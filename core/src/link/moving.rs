//! Moving music from one of a person's devices to another (`docs/linking-design.md`, 5.7).
//!
//! Two ordinary commands, each judged by the one rule. The order is always **the other
//! starts, then this one stops**, so that anything that gets in the way leaves music
//! playing somewhere, and the person told where.
//!
//! - **Giving** ("play this on Study PC"): this device notes its own generation and sends
//!   `adopt` with its queue. The other takes the queue, starts it, and answers once its
//!   sound has started. On that answer this device pauses, only if its own generation is
//!   still the one it noted and the press was no more than [`MOVE_MS`] ago.
//! - **Taking** ("play here"): this device asks for the other's queue, takes it and starts.
//!   Once its own sound has started, if nothing else has been chosen here and the press was
//!   no more than [`MOVE_MS`] ago, it sends the other `pause`, quoting the state the queue
//!   was taken from.
//! - **Adopting**: the device that was sent `adopt` answers "carried out" only when its
//!   sound has started. Until then it says it is busy. If something else is chosen on it
//!   first it says so; if no sound comes in [`START_MS`] it pauses and says it could not.
//!
//! Each of these is a decision with no clock and no player in it: the app says what has
//! happened and when, and is told what to do and what to tell the person.

use super::message::{At, Why};
use super::steer::Outcome;

/// How long after the press the pause that completes a move may still be made.
pub const MOVE_MS: u64 = 30_000;
/// How long a device that has taken a queue over gives its sound to start: less than
/// [`MOVE_MS`], so that its answer can still complete the move.
pub const START_MS: u64 = 20_000;

/// Music is being given to another device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Giving {
    /// This device's generation when the person asked.
    noted: u64,
    pressed_ms: u64,
}

/// How giving music to another device ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gave {
    /// The other has started: pause this one. The music is there.
    Moved,
    /// The other has started, and something else was chosen on this one meanwhile: this
    /// one is left as it is. Both may be playing.
    StartedAndThisChanged,
    /// The other has started, too long after the press for this one to be stopped by
    /// itself. Both are playing.
    StartedLate,
    /// The other did nothing, and said why. The music is still here.
    Stayed(Why),
    /// It had not reached the other when checked. The music is still here; the other may
    /// yet start.
    NotReached,
    /// No answer, and the other could not be looked at or had changed: whether it started
    /// is not known. The music is still here.
    Unknown,
}

impl Giving {
    pub fn begin(own_generation: u64, now_ms: u64) -> Giving {
        Giving { noted: own_generation, pressed_ms: now_ms }
    }

    /// What became of the `adopt`, and so of the move. `own_generation` is this device's now.
    pub fn ends(&self, outcome: Outcome, own_generation: u64, now_ms: u64) -> Gave {
        match outcome {
            Outcome::Done if own_generation != self.noted => Gave::StartedAndThisChanged,
            Outcome::Done if !in_time(self.pressed_ms, now_ms) => Gave::StartedLate,
            Outcome::Done => Gave::Moved,
            Outcome::Refused(why) => Gave::Stayed(why),
            Outcome::NotReached => Gave::NotReached,
            Outcome::Unknown => Gave::Unknown,
        }
    }
}

fn in_time(pressed_ms: u64, now_ms: u64) -> bool {
    now_ms.checked_sub(pressed_ms).is_some_and(|since| since <= MOVE_MS)
}

/// Music is being taken from another device: its queue has been loaded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taking {
    /// The other's state when its queue was read: what its `pause` will quote.
    from: At,
    /// This device's generation once the queue was loaded.
    noted: u64,
    pressed_ms: u64,
}

/// What to do once the queue taken from another device has been loaded here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Took {
    /// The sound here has started: send the other `pause`, made against this state.
    Pause(At),
    /// Something else was chosen here before the sound started: the other is left as it
    /// is, and is still playing.
    ThisChanged,
    /// The sound here did not start in time: the other is left playing.
    NotStarted,
}

impl Taking {
    pub fn begin(from: At, own_generation_loaded: u64, pressed_ms: u64) -> Taking {
        Taking { from, noted: own_generation_loaded, pressed_ms }
    }

    /// Asked until it gives an answer. `sounding` is whether this device's own sound has
    /// started on the queue it took.
    pub fn poll(&self, sounding: bool, own_generation: u64, now_ms: u64) -> Option<Took> {
        if own_generation != self.noted {
            Some(Took::ThisChanged)
        } else if !in_time(self.pressed_ms, now_ms) {
            Some(Took::NotStarted)
        } else if sounding {
            Some(Took::Pause(self.from))
        } else {
            None
        }
    }
}

/// A queue sent by another device has been loaded here, and its sound is awaited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Adopting {
    /// This device's generation once the queue was loaded.
    noted: u64,
    loaded_ms: u64,
}

/// What to answer the device that sent the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adopted {
    /// The sound has started: "carried out".
    Started,
    /// Something else was chosen here first: refused, `changed`. This device is doing
    /// whatever was chosen.
    Changed,
    /// No sound in time: pause, and refuse, `cannot`.
    Failed,
}

impl Adopting {
    pub fn begin(own_generation_loaded: u64, now_ms: u64) -> Adopting {
        Adopting { noted: own_generation_loaded, loaded_ms: now_ms }
    }

    /// Asked until it gives an answer.
    pub fn poll(&self, sounding: bool, own_generation: u64, now_ms: u64) -> Option<Adopted> {
        if own_generation != self.noted {
            Some(Adopted::Changed)
        } else if sounding {
            Some(Adopted::Started)
        } else if now_ms.checked_sub(self.loaded_ms).is_none_or(|since| since >= START_MS) {
            Some(Adopted::Failed)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn giving_pauses_this_device_only_on_a_start_in_time_and_only_if_it_is_as_it_was() {
        let giving = Giving::begin(7, 1_000);
        assert_eq!(giving.ends(Outcome::Done, 7, 5_000), Gave::Moved);
        assert_eq!(giving.ends(Outcome::Done, 7, 1_000 + MOVE_MS), Gave::Moved);
        // Thirty seconds on, the person has long since stopped expecting this one to stop.
        assert_eq!(giving.ends(Outcome::Done, 7, 1_001 + MOVE_MS), Gave::StartedLate);
        // Something was pressed here while the other was starting: it is not undone.
        assert_eq!(giving.ends(Outcome::Done, 8, 5_000), Gave::StartedAndThisChanged);
        // Every other end leaves the music here, and says which end it was.
        assert_eq!(giving.ends(Outcome::Refused(Why::Cannot), 7, 5_000), Gave::Stayed(Why::Cannot));
        assert_eq!(giving.ends(Outcome::Refused(Why::Busy), 8, 5_000), Gave::Stayed(Why::Busy));
        assert_eq!(giving.ends(Outcome::NotReached, 7, 14_000), Gave::NotReached);
        assert_eq!(giving.ends(Outcome::Unknown, 7, 26_000), Gave::Unknown);
        // A clock that reads earlier than the press is no reason to stop this one.
        assert_eq!(giving.ends(Outcome::Done, 7, 999), Gave::StartedLate);
    }

    #[test]
    fn taking_pauses_the_other_only_once_this_one_sounds() {
        let from = At { generation: 12, minute: 3 };
        let taking = Taking::begin(from, 4, 1_000);
        assert_eq!(taking.poll(false, 4, 2_000), None);
        assert_eq!(taking.poll(true, 4, 2_500), Some(Took::Pause(from)));
        // Pause was pressed here before the first sound, or something else was chosen: the
        // other is not stopped on the strength of a queue this one is no longer playing.
        assert_eq!(taking.poll(false, 5, 2_000), Some(Took::ThisChanged));
        assert_eq!(taking.poll(true, 5, 2_000), Some(Took::ThisChanged));
        // No sound in thirty seconds, or sound only after them: the other plays on.
        assert_eq!(taking.poll(false, 4, 1_000 + MOVE_MS), None);
        assert_eq!(taking.poll(false, 4, 1_001 + MOVE_MS), Some(Took::NotStarted));
        assert_eq!(taking.poll(true, 4, 1_001 + MOVE_MS), Some(Took::NotStarted));
    }

    #[test]
    fn a_device_that_took_a_queue_says_so_only_when_it_sounds() {
        let adopting = Adopting::begin(9, 1_000);
        assert_eq!(adopting.poll(false, 9, 1_500), None);
        assert_eq!(adopting.poll(true, 9, 1_500), Some(Adopted::Started));
        assert_eq!(adopting.poll(false, 10, 1_500), Some(Adopted::Changed));
        assert_eq!(adopting.poll(false, 9, 999 + START_MS), None);
        assert_eq!(adopting.poll(false, 9, 1_000 + START_MS), Some(Adopted::Failed));
        // It gives up sooner than the device that sent the queue stops waiting to stop.
        assert!(START_MS < MOVE_MS);
    }
}
