//! The one rule a device applies to a command from another of the person's devices.
//!
//! **A device has a generation,** a number that starts at 1 with the app and moves on:
//!
//! * when a choice is accepted ([`Rule::accept`]): at the press, before any work is done
//!   for it. A choice that changes nothing counts;
//! * by itself, when what a command would act on changes ([`Rule::moved`]): the next
//!   song starting, autoplay adding songs, a timer's pause, the output being lost. The
//!   lesser choices go the same way: a seek, the volume, an edit to the queue.
//!
//! **Accepting and carrying out.** Accepting a choice *reserves* its generation, once.
//! Every later step that carries the choice out (a look-up's answer arriving, the player
//! being changed) first asks [`Rule::carry_out`], and does nothing if told no: something
//! newer has the say. Between the two the device is **busy**: its state says so, and a
//! command from another device is refused. While it is busy the generation does not move
//! by itself: no command can be made against a busy state, and the state that follows is
//! the one that counts.
//!
//! **A command** quotes the generation and the minute of the state it was made against.
//! It is accepted ([`Rule::admit`]) only if the device is not busy, that is its generation
//! now, and its minute now is that minute or the one after; and it is carried out
//! ([`Rule::finish`]) only if all of that still holds at the moment the player is changed,
//! under the same session. So a command delivered twice, one held back, and one overtaken
//! by something chosen at the device all fail the same way, and nothing is remembered
//! about past commands but which one was carried out last ([`Rule::by`]).

use super::Nonce;
use super::message::{At, Why};

/// A place in the order of choices, reserved when the choice was accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation(u64);

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    generation: u64,
    /// The choice that has been accepted and not yet carried out.
    open: Option<u64>,
    /// The command that was last carried out, if nothing has been chosen since.
    by: Option<Nonce>,
}

impl Default for Rule {
    fn default() -> Rule {
        Rule { generation: 1, open: None, by: None }
    }
}

impl Rule {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn busy(&self) -> bool {
        self.open.is_some()
    }

    pub fn by(&self) -> Option<Nonce> {
        self.by
    }

    /// A choice is accepted: its generation is reserved, and the device is busy until the
    /// choice is carried out or given up. Whatever was waiting to be carried out no longer
    /// has the say.
    pub fn accept(&mut self) -> Reservation {
        self.generation = self.generation.saturating_add(1);
        self.open = Some(self.generation);
        self.by = None;
        Reservation(self.generation)
    }

    /// Asked by each step that carries a choice out, before it does anything. `true`:
    /// nothing has been accepted since; go ahead (and for the last step, the device is no
    /// longer busy). `false`: something newer has the say; do nothing.
    pub fn carry_out(&mut self, reservation: Reservation) -> bool {
        if reservation.0 != self.generation {
            return false;
        }
        self.open = None;
        true
    }

    /// Whether a choice is still the newest, without ending it: for a step that is not the
    /// last.
    pub fn stands(&self, reservation: Reservation) -> bool {
        reservation.0 == self.generation
    }

    /// A choice that was accepted came to nothing (its look-up failed). The generation
    /// stays where the press put it; the device is no longer busy.
    pub fn give_up(&mut self, reservation: Reservation) {
        if self.open == Some(reservation.0) {
            self.open = None;
        }
    }

    /// What a command would act on has changed without a choice that takes a place of its
    /// own. While a choice is waiting to be carried out this changes nothing (see above).
    pub fn moved(&mut self) {
        if self.open.is_none() {
            self.generation = self.generation.saturating_add(1);
            self.by = None;
        }
    }

    /// A command arrives, made against `at`; `minute` is this device's minute now. Checked
    /// before any work is done for it. `Ok`: it is accepted like any other choice.
    pub fn admit(&mut self, at: At, minute: u64) -> Result<Reservation, Why> {
        self.check(at, minute)?;
        Ok(self.accept())
    }

    fn check(&self, at: At, minute: u64) -> Result<(), Why> {
        if self.busy() {
            return Err(Why::Busy);
        }
        if at.generation != self.generation {
            return Err(Why::Changed);
        }
        in_time(at, minute)
    }

    /// The moment the player is changed for a command. `Ok`: change it; the command is the
    /// one that made this state. `Err`: change nothing. `same_session` is whether the
    /// sign-in and the secret the command arrived under are still the ones in use.
    pub fn finish(&mut self, reservation: Reservation, at: At, minute: u64, same_session: bool, nonce: Nonce) -> Result<(), Why> {
        if !self.stands(reservation) {
            return Err(Why::Changed);
        }
        self.open = None;
        if !same_session {
            return Err(Why::Cannot);
        }
        in_time(at, minute)?;
        self.by = Some(nonce);
        Ok(())
    }
}

/// The minute a command was made against, or the one after.
fn in_time(at: At, minute: u64) -> Result<(), Why> {
    match minute.checked_sub(at.minute) {
        Some(0 | 1) => Ok(()),
        _ => Err(Why::Late),
    }
}

/// The decisions the reviews asked for, each as what arrives and what must be decided.
/// `docs/linking-vectors.py` runs the same sequences against a model written separately.
#[cfg(test)]
mod tests {
    use super::*;

    const X: Nonce = [1; 24];
    const Y: Nonce = [2; 24];

    fn at(rule: &Rule, minute: u64) -> At {
        At { generation: rule.generation(), minute }
    }

    /// A command that arrives and is carried out at once, as a Pause is.
    fn command(rule: &mut Rule, at: At, minute: u64, nonce: Nonce) -> Result<(), Why> {
        let reservation = rule.admit(at, minute)?;
        rule.finish(reservation, at, minute, true, nonce)
    }

    /// A choice made and carried out at the device in one step.
    fn press(rule: &mut Rule) {
        let reservation = rule.accept();
        assert!(rule.carry_out(reservation));
    }

    #[test]
    fn a_command_against_the_current_state_is_carried_out_and_named() {
        let mut rule = Rule::default();
        let seen = at(&rule, 3);
        assert_eq!(command(&mut rule, seen, 3, X), Ok(()));
        assert_eq!((rule.generation(), rule.by(), rule.busy()), (2, Some(X), false));
    }

    #[test]
    fn double_skip_the_frame_delivered_twice() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        assert_eq!(command(&mut rule, seen, 0, X), Ok(()));
        // The same frame again, and a faulty sender asking anew against what it last saw.
        assert_eq!(command(&mut rule, seen, 0, X), Err(Why::Changed));
        assert_eq!(command(&mut rule, seen, 0, Y), Err(Why::Changed));
        // The refusal still says the first was carried out: that is how its sender knows.
        assert_eq!((rule.generation(), rule.by()), (2, Some(X)));
    }

    #[test]
    fn held_play_refused_once_anything_is_chosen_at_the_device() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        // The person pauses at the device, though it was paused already: it counts.
        press(&mut rule);
        assert_eq!(command(&mut rule, seen, 0, X), Err(Why::Changed));
        assert_eq!(rule.by(), None);
    }

    #[test]
    fn held_play_carried_out_a_minute_on_and_not_two() {
        let (mut on_time, mut late) = (Rule::default(), Rule::default());
        let seen = at(&on_time, 5);
        assert_eq!(command(&mut on_time, seen, 6, X), Ok(()));
        assert_eq!(command(&mut late, seen, 7, X), Err(Why::Late));
        // Refused for its age, it has taken no place: the device is as it was.
        assert_eq!((late.generation(), late.busy()), (1, false));
        // A minute from before the device's own count began is not one to act on either.
        assert_eq!(command(&mut late, At { generation: 1, minute: 9 }, 7, X), Err(Why::Late));
    }

    #[test]
    fn a_command_that_arrives_in_time_and_whose_work_outlasts_the_minute() {
        let mut rule = Rule::default();
        let seen = at(&rule, 5);
        let reservation = rule.admit(seen, 6).unwrap();
        assert!(rule.busy());
        assert_eq!(rule.finish(reservation, seen, 7, true, X), Err(Why::Late));
        assert_eq!((rule.by(), rule.busy()), (None, false));
    }

    /// Review 2's sequence 14, as review 3 corrected it: Play is pressed at the device on a
    /// list that has to be looked up, and a Pause made before the press arrives meanwhile.
    #[test]
    fn sequence_14_a_held_pause_against_a_play_still_looking_its_songs_up() {
        let mut rule = Rule::default();
        let before = at(&rule, 0);
        let play = rule.accept();
        // The held Pause, and one from a device that looked during the look-up and was
        // shown the new generation beside the old queue.
        assert_eq!(command(&mut rule, before, 0, X), Err(Why::Busy));
        let during = at(&rule, 0);
        assert_eq!(command(&mut rule, during, 0, Y), Err(Why::Busy));
        // The look-up answers and the Play goes ahead.
        assert!(rule.carry_out(play));
        assert_eq!(command(&mut rule, before, 0, X), Err(Why::Changed));
        assert_eq!((rule.generation(), rule.by(), rule.busy()), (2, None, false));
    }

    #[test]
    fn a_look_up_that_answers_after_a_newer_choice_does_nothing() {
        let mut rule = Rule::default();
        let play = rule.accept();
        press(&mut rule);
        assert!(!rule.carry_out(play));
        // And one that fails leaves the device free again, where the press put it.
        let another = rule.accept();
        rule.give_up(another);
        assert_eq!((rule.generation(), rule.busy()), (4, false));
        // Giving up a choice that is no longer the open one does not end a newer one.
        let (older, newer) = (rule.accept(), rule.accept());
        rule.give_up(older);
        assert!(rule.busy() && rule.carry_out(newer));
    }

    #[test]
    fn a_choice_at_the_device_while_a_command_s_work_is_under_way() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        let reservation = rule.admit(seen, 0).unwrap();
        press(&mut rule);
        assert_eq!(rule.finish(reservation, seen, 0, true, X), Err(Why::Changed));
        assert_eq!((rule.by(), rule.busy()), (None, false));
    }

    #[test]
    fn a_pause_that_crosses_the_start_of_the_next_song_is_refused_once() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        rule.moved();
        assert_eq!(command(&mut rule, seen, 0, X), Err(Why::Changed));
        let again = at(&rule, 0);
        assert_eq!(command(&mut rule, again, 0, X), Ok(()));
    }

    #[test]
    fn while_a_choice_waits_the_generation_does_not_move_by_itself() {
        let mut rule = Rule::default();
        let play = rule.accept();
        // The song that was playing ends, and the volume is nudged, while the list is
        // looked up: the Play still goes ahead.
        rule.moved();
        rule.moved();
        assert!(rule.carry_out(play));
        assert_eq!(rule.generation(), 2);
    }

    #[test]
    fn two_devices_at_the_same_instant() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        assert_eq!((command(&mut rule, seen, 0, X), command(&mut rule, seen, 0, Y)), (Ok(()), Err(Why::Changed)));
        assert_eq!(rule.by(), Some(X));
    }

    #[test]
    fn an_old_volume_delivered_again_after_a_newer_one() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        assert_eq!(command(&mut rule, seen, 0, X), Ok(()));
        rule.moved();
        assert_eq!(command(&mut rule, seen, 0, X), Err(Why::Changed));
        assert_eq!(rule.by(), None);
    }

    /// The secret is reset between a Pause arriving and the player being changed.
    #[test]
    fn the_secret_rolling_over_at_the_moment_of_carrying_out() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        let reservation = rule.admit(seen, 0).unwrap();
        assert_eq!(rule.finish(reservation, seen, 0, false, X), Err(Why::Cannot));
        // It is not carried out and not named, and the device is free for the next choice.
        assert_eq!((rule.by(), rule.busy(), rule.generation()), (None, false, 2));
    }

    #[test]
    fn the_numbers_cannot_run_out_or_wrap() {
        let mut rule = Rule { generation: u64::MAX, open: None, by: None };
        let reservation = rule.accept();
        assert!(rule.carry_out(reservation));
        rule.moved();
        assert_eq!(rule.generation(), u64::MAX);
        assert_eq!(command(&mut Rule::default(), At { generation: 1, minute: u64::MAX }, 0, X), Err(Why::Late));
        assert_eq!(command(&mut Rule::default(), At { generation: 1, minute: u64::MAX - 1 }, u64::MAX, X), Ok(()));
    }
}
