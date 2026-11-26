//! Every way into the player, as one thing: who chose it, and what it touches.
//!
//! Two rules hang on what happens to a device's player, and each needs every way in to be
//! known to it:
//!
//! - **the order of choices** (`link::rule`): a command from another of the person's
//!   devices is carried out only if nothing has been chosen on this one since the state it
//!   was made against. Anything that changes what a command would act on must be counted,
//!   whoever or whatever did it;
//! - **whose the queue is** (`handover`): a queue the person has made their own is never
//!   replaced by itself. It is theirs from their own action, said at the action; what the
//!   app does by itself claims nothing.
//!
//! Both are derived here from one statement made at each way in: [`Who`] chose it and
//! [`What`] it touches. An app makes that statement once, at the function of its player
//! that the way in reaches, and does what [`Owed`] says. There is no second list.
//!
//! **What is under way is one choice.** What a way in does on its way (Play on an album
//! stops the song, replaces the queue, starts another) is part of it: counted once and
//! claimed once. And the one who chose the outermost chose all of it: the sleep timer's
//! pause "presses" Pause, and is the app's own doing all the same; a command from another
//! device runs the function its press runs, and was chosen at that other device.

use serde::{Deserialize, Serialize};

/// Who chose something that happens in the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Who {
    /// The person, at this device: a press, a key, the tray, a media key, a notification's
    /// button, a headset's, the car's.
    Here,
    /// The person, at another of their devices: a command carried out here for it.
    Elsewhere,
    /// A guest in a jam this device hosts.
    Guest,
    /// Nobody: the app itself. The next song beginning, a song that would not play being
    /// passed over, similar songs arriving, the sleep timer, the sound device going,
    /// another device's saved queue put in the player, the last run's queue put back, a
    /// followed playlist changing, the stop at sign-out, the pause that ends a move of
    /// music to another device.
    Nobody,
}

/// What a way in touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum What {
    /// What is in the player, or how it is playing: something played, Play, Pause, Next,
    /// Previous, a seek, shuffle, a song queued, moved or removed, the queue cleared or
    /// jumped in, a queue taken over. What a saved queue holds.
    Playing,
    /// How it sounds, or what is to happen later: the volume, mute, repeat, the sleep
    /// timer being set. A command would act on it, so it is counted; a saved queue holds
    /// none of it, so it claims nothing.
    Manner,
}

/// What an app owes for a way in, now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owed {
    /// The order of choices moves on: one place (`link::rule::Rule::moved`).
    pub count: bool,
    /// The queue is this device's own from this moment: no longer one taken up from
    /// another device (`handover::Here::taken`), changed now by this device's clock
    /// (`handover::Here::stamp`), and to be saved.
    pub claim: bool,
}

/// The choices under way on one device.
#[derive(Debug, Default)]
pub struct Ways {
    depth: u32,
    who: Option<Who>,
    counted: bool,
    claimed: bool,
    /// A way in has been taken in the choice under way.
    taken: bool,
}

impl Ways {
    /// Someone's choice is about to be carried out, and nothing has been touched yet: a
    /// command from another of the person's devices, or what a guest asked for. Whatever
    /// way in it then reaches is theirs. `counted`: it has taken its place in the order of
    /// choices already (the rule let a command through), and is not to be counted again.
    /// Ended by [`Ways::end`]. Inside a choice that is already under way it is part of that.
    pub fn begin(&mut self, who: Who, counted: bool) {
        if self.depth == 0 {
            (self.who, self.counted, self.claimed, self.taken) = (Some(who), counted, false, false);
        }
        self.depth = self.depth.saturating_add(1);
    }

    /// A way in is being taken: it is known that something will change. `who` chose it,
    /// **unless a choice is already under way**, whose it then is. Ended by [`Ways::end`].
    pub fn enter(&mut self, who: Who, what: What) -> Owed {
        if self.depth == 0 {
            (self.who, self.counted, self.claimed) = (Some(who), false, false);
        }
        self.depth = self.depth.saturating_add(1);
        self.taken = true;
        let count = !std::mem::replace(&mut self.counted, true);
        let claim = what == What::Playing && self.who != Some(Who::Nobody) && !std::mem::replace(&mut self.claimed, true);
        Owed { count, claim }
    }

    /// What was begun or entered is over.
    pub fn end(&mut self) {
        self.depth = self.depth.saturating_sub(1);
        if self.depth == 0 {
            self.who = None;
        }
    }

    /// A way in that is over as soon as it is taken (a seek, a song queued, the volume).
    pub fn touch(&mut self, who: Who, what: What) -> Owed {
        let owed = self.enter(who, what);
        self.end();
        owed
    }

    /// Whose choice is under way, if one is.
    pub fn whose(&self) -> Option<Who> {
        self.who
    }

    /// Whether the choice under way has taken any way in yet: whether carrying it out has
    /// changed anything. A way in is taken only once it is known that something will
    /// change, so a choice that reaches the player and finds nothing to do (Play when it
    /// is playing, a seek on a radio, Next with nothing to go to) has taken none.
    pub fn taken(&self) -> bool {
        self.depth > 0 && self.taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COUNT: Owed = Owed { count: true, claim: false };
    const BOTH: Owed = Owed { count: true, claim: true };
    const CLAIM: Owed = Owed { count: false, claim: true };
    const NOTHING: Owed = Owed { count: false, claim: false };

    #[test]
    fn who_chose_and_what_it_touches_say_what_is_owed() {
        let mut ways = Ways::default();
        // The person's own press, on what plays: counted, and the queue is theirs.
        assert_eq!(ways.touch(Who::Here, What::Playing), BOTH);
        // On how it sounds: counted, and nothing is claimed.
        assert_eq!(ways.touch(Who::Here, What::Manner), COUNT);
        // A guest's, which the host let through: as the host's own press would be.
        ways.begin(Who::Guest, false);
        assert_eq!((ways.touch(Who::Here, What::Playing), ways.whose()), (BOTH, Some(Who::Guest)));
        ways.end();
        ways.begin(Who::Guest, false);
        assert_eq!(ways.touch(Who::Here, What::Manner), COUNT);
        ways.end();
        // The app's own doing: counted, and it claims nothing whatever it touches.
        assert_eq!(ways.touch(Who::Nobody, What::Playing), COUNT);
        assert_eq!(ways.touch(Who::Nobody, What::Manner), COUNT);
        assert_eq!(ways.whose(), None);
    }

    #[test]
    fn a_command_from_another_device_is_counted_by_the_rule_and_claims_as_its_press_would() {
        let mut ways = Ways::default();
        // The rule let it through, which moved the order on: the player's function is not
        // counted again. It is the person's own choice, made elsewhere.
        ways.begin(Who::Elsewhere, true);
        assert_eq!(ways.enter(Who::Here, What::Playing), CLAIM);
        ways.end();
        ways.end();
        ways.begin(Who::Elsewhere, true);
        assert_eq!(ways.touch(Who::Here, What::Manner), NOTHING);
        ways.end();
        // One that arrives the old way has taken no place yet: it is counted as it is done.
        ways.begin(Who::Elsewhere, false);
        assert_eq!(ways.touch(Who::Here, What::Playing), BOTH);
        ways.end();
        // One that comes to nothing (an entry that has gone) owes nothing, and is known to
        // have changed nothing; one that takes a way in is known to have.
        ways.begin(Who::Elsewhere, true);
        assert!(!ways.taken());
        ways.end();
        ways.begin(Who::Elsewhere, true);
        ways.touch(Who::Here, What::Manner);
        assert!(ways.taken());
        ways.end();
        assert!(!ways.taken());
        // (What was taken by the choice before is not this one's.)
        ways.begin(Who::Guest, false);
        assert!(!ways.taken());
        ways.end();
        assert_eq!(ways.whose(), None);
        // And afterwards a press is a press.
        assert_eq!(ways.touch(Who::Here, What::Playing), BOTH);
    }

    #[test]
    fn what_a_way_in_does_on_its_way_is_part_of_it() {
        let mut ways = Ways::default();
        // Play on an album: the song is stopped, the queue replaced, another started.
        assert_eq!(ways.enter(Who::Here, What::Playing), BOTH);
        assert_eq!(ways.enter(Who::Here, What::Playing), NOTHING);
        assert_eq!(ways.touch(Who::Nobody, What::Playing), NOTHING);
        ways.end();
        // (It is still the person's until the outermost is over.)
        assert_eq!(ways.whose(), Some(Who::Here));
        ways.end();
        // The sleep timer pauses by "pressing" Pause: the app's own doing all the same.
        assert_eq!(ways.enter(Who::Nobody, What::Playing), COUNT);
        assert_eq!(ways.enter(Who::Here, What::Playing), NOTHING);
        ways.end();
        ways.end();
        // A queue saved by another device is put in the player, and played because the
        // person said "Continue here": the putting in claims nothing (the app says whose
        // it is afterwards, by the next line).
        assert_eq!(ways.enter(Who::Nobody, What::Playing), COUNT);
        assert_eq!(ways.touch(Who::Here, What::Playing), NOTHING);
        ways.end();
        // Something that touches how it sounds and then what plays claims at the second.
        assert_eq!(ways.enter(Who::Here, What::Manner), COUNT);
        assert_eq!(ways.touch(Who::Here, What::Playing), CLAIM);
        assert_eq!(ways.touch(Who::Here, What::Playing), NOTHING);
        ways.end();
        // Ending more than was begun leaves it as it was.
        ways.end();
        assert_eq!(ways.touch(Who::Here, What::Playing), BOTH);
    }

    /// Whatever it is handed, in whatever order, it neither stops the app nor runs out of
    /// room: beginnings that are never ended are one choice that goes on, and are over
    /// when as many endings have come.
    #[test]
    fn beginnings_without_endings_cannot_run_it_out() {
        let mut ways = Ways { depth: u32::MAX - 1, who: Some(Who::Guest), counted: true, claimed: true, taken: false };
        ways.begin(Who::Elsewhere, false);
        assert_eq!(ways.enter(Who::Here, What::Playing), NOTHING);
        ways.begin(Who::Here, false);
        assert_eq!((ways.depth, ways.whose()), (u32::MAX, Some(Who::Guest)));
        let mut ways = Ways::default();
        for _ in 0..1_000 {
            ways.begin(Who::Guest, false);
        }
        for _ in 0..1_000 {
            assert_eq!(ways.whose(), Some(Who::Guest));
            ways.end();
        }
        assert_eq!((ways.whose(), ways.touch(Who::Here, What::Playing)), (None, BOTH));
    }
}
