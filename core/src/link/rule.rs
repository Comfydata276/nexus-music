//! The one rule a device applies to a command from another of the person's devices.
//!
//! The rule exists for one purpose: so that a command from another device is judged against
//! what is true on this one. It never judges what the person does on this device itself:
//! nothing they press here is refused, delayed or dropped because of it.
//!
//! **A device has a generation,** a number that starts at 1 with the app and moves on
//! ([`Rule::moved`]) whenever what a command would act on changes: a choice made here
//! being carried out (not one that changes nothing: Next with nothing to go to), the next
//! song starting, autoplay adding songs, a timer's pause, the output being lost, a command
//! from another device being carried out.
//!
//! **Busy.** A choice made here whose work is still to do (a Play whose songs are being
//! looked up) marks the device busy from the press ([`Rule::wait`]) until the work is done
//! or has come to nothing ([`Rule::done`]). Its state says so, and a command from another
//! device is refused meanwhile: the state it was made against is about to change. Being
//! busy changes nothing for the person at the device, and several choices can be waiting
//! at once, each as it would be without this. A wait ends by itself after the time it
//! was given, so a look-up that never answers cannot leave a device busy for ever.
//!
//! **A command** quotes the generation and the minute of the state it was made against.
//! It is accepted ([`Rule::admit`]) only if the device is not busy, that is its generation
//! now, and its minute now is that minute or the one after; and it is carried out
//! ([`Rule::finish`]) only if it is still the command that was accepted and all of that
//! still holds at the moment the player is changed, under the same session. So a command
//! delivered twice, one held back, and one overtaken by something chosen at the device all
//! fail the same way, and nothing is remembered about past commands but which one was
//! carried out last ([`Rule::by`]).

use super::Nonce;
use super::message::{At, Why};

/// The most choices that can be waiting at once. More than this and the oldest stops
/// counting: a device does not stay busy on the strength of a pile of forgotten look-ups.
const MOST_WAITING: usize = 32;

/// A choice made on this device whose work is still to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiting(u64);

/// A command from another device that has been accepted and not yet carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    id: u64,
    /// The generation it was accepted at: still the generation when it is carried out, or
    /// something has been chosen since.
    generation: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    generation: u64,
    /// The choices that are waiting, and when each stops counting (the app's clock, in
    /// milliseconds).
    waiting: Vec<(u64, u64)>,
    /// The command that has been accepted and not yet carried out.
    command: Option<u64>,
    /// What the next wait or command is called.
    next: u64,
    /// The command that was last carried out, if nothing has changed since.
    by: Option<Nonce>,
}

impl Default for Rule {
    fn default() -> Rule {
        Rule { generation: 1, waiting: Vec::new(), command: None, next: 1, by: None }
    }
}

impl Rule {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether a choice made here is waiting for its work, or a command is being carried out.
    pub fn busy(&self, now_ms: u64) -> bool {
        self.command.is_some() || self.waiting.iter().any(|(_, until)| *until > now_ms)
    }

    pub fn by(&self) -> Option<Nonce> {
        self.by
    }

    fn name(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.wrapping_add(1);
        id
    }

    /// A choice is made here whose work is still to do: the device is busy until [`done`]
    /// is called for it, or `for_ms` has passed, whichever comes first. Nothing else
    /// changes: whatever else is waiting goes on waiting.
    ///
    /// [`done`]: Rule::done
    pub fn wait(&mut self, now_ms: u64, for_ms: u64) -> Waiting {
        self.waiting.retain(|(_, until)| *until > now_ms);
        if self.waiting.len() >= MOST_WAITING {
            self.waiting.remove(0);
        }
        let id = self.name();
        self.waiting.push((id, now_ms.saturating_add(for_ms)));
        Waiting(id)
    }

    /// The work for a choice is over: it was carried out (which moved the generation by
    /// itself, if it changed anything), or it came to nothing.
    pub fn done(&mut self, waiting: Waiting) {
        self.waiting.retain(|(id, _)| *id != waiting.0);
    }

    /// What a command would act on has changed.
    pub fn moved(&mut self) {
        self.generation = self.generation.saturating_add(1);
        self.by = None;
    }

    /// A command arrives, made against `at`; `minute` is this device's minute now and
    /// `now_ms` its clock. Checked before any work is done for it.
    pub fn admit(&mut self, at: At, minute: u64, now_ms: u64) -> Result<Reservation, Why> {
        if self.busy(now_ms) {
            return Err(Why::Busy);
        }
        if at.generation != self.generation {
            return Err(Why::Changed);
        }
        in_time(at, minute)?;
        let id = self.name();
        self.command = Some(id);
        Ok(Reservation { id, generation: self.generation })
    }

    /// A command that was accepted turns out not to be one this device can carry out (the
    /// song it names is no longer in the queue; none of the songs it brings are in this
    /// library). Nothing was changed for it, so nothing moves: the device is no longer
    /// busy with it, and that is all.
    pub fn refuse(&mut self, reservation: Reservation) {
        if self.command == Some(reservation.id) {
            self.command = None;
        }
    }

    /// The moment the player is changed for a command. `Ok`: change it; the command is the
    /// one that made this state. `Err`: change nothing. `same_session` is whether the
    /// sign-in and the secret the command arrived under are still the ones in use, and
    /// inside their time.
    ///
    /// Only the command that is open can be finished, and only once.
    pub fn finish(&mut self, reservation: Reservation, at: At, minute: u64, same_session: bool, nonce: Nonce) -> Result<(), Why> {
        if self.command != Some(reservation.id) {
            return Err(Why::Changed);
        }
        self.command = None;
        if reservation.generation != self.generation {
            return Err(Why::Changed);
        }
        if !same_session {
            return Err(Why::Cannot);
        }
        in_time(at, minute)?;
        self.moved();
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
    /// How long a look-up is given.
    const LOOK_UP: u64 = 60_000;

    fn at(rule: &Rule, minute: u64) -> At {
        At { generation: rule.generation(), minute }
    }

    /// A command that arrives and is carried out at once, as a Pause is.
    fn command(rule: &mut Rule, at: At, minute: u64, nonce: Nonce) -> Result<(), Why> {
        let reservation = rule.admit(at, minute, minute.saturating_mul(60_000))?;
        rule.finish(reservation, at, minute, true, nonce)
    }

    /// A command that is accepted and then found to be one this device cannot carry out
    /// (the song it names has gone from the queue) changes nothing and leaves nothing open.
    #[test]
    fn a_command_that_cannot_be_carried_out_is_let_go_and_moves_nothing() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        let reservation = rule.admit(seen, 0, 0).unwrap();
        assert!(rule.busy(0));
        rule.refuse(reservation);
        assert_eq!((rule.busy(0), rule.generation(), rule.by()), (false, 1, None));
        // It cannot be finished afterwards, and the next command is taken as usual.
        assert_eq!(rule.finish(reservation, seen, 0, true, X), Err(Why::Changed));
        assert_eq!(command(&mut rule, seen, 0, Y), Ok(()));
        // Letting go of a command that is not the open one touches nothing.
        let open = rule.admit(at(&rule, 0), 0, 0).unwrap();
        rule.refuse(reservation);
        assert!(rule.busy(0));
        assert_eq!(rule.finish(open, At { generation: 2, minute: 0 }, 0, true, X), Ok(()));
    }

    #[test]
    fn a_command_against_the_current_state_is_carried_out_and_named() {
        let mut rule = Rule::default();
        let seen = at(&rule, 3);
        assert_eq!(command(&mut rule, seen, 3, X), Ok(()));
        assert_eq!((rule.generation(), rule.by(), rule.busy(180_000)), (2, Some(X), false));
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
    fn held_play_refused_once_anything_changes_at_the_device() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        rule.moved();
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
        assert_eq!((late.generation(), late.busy(0)), (1, false));
        // A minute from before the device's own count began is not one to act on either.
        assert_eq!(command(&mut late, At { generation: 1, minute: 9 }, 7, X), Err(Why::Late));
    }

    #[test]
    fn a_command_that_arrives_in_time_and_whose_work_outlasts_the_minute() {
        let mut rule = Rule::default();
        let seen = at(&rule, 5);
        let reservation = rule.admit(seen, 6, 0).unwrap();
        assert!(rule.busy(0));
        assert_eq!(rule.finish(reservation, seen, 7, true, X), Err(Why::Late));
        assert_eq!((rule.by(), rule.busy(0), rule.generation()), (None, false, 1));
    }

    /// Review 2's sequence 14, as review 3 corrected it: Play is pressed at the device on a
    /// list that has to be looked up, and a Pause made before the press arrives meanwhile.
    #[test]
    fn sequence_14_a_held_pause_against_a_play_still_looking_its_songs_up() {
        let mut rule = Rule::default();
        let before = at(&rule, 0);
        let play = rule.wait(1_000, LOOK_UP);
        // The held Pause, and one from a device that looked during the look-up.
        assert_eq!(command(&mut rule, before, 0, X), Err(Why::Busy));
        let during = at(&rule, 0);
        assert_eq!(command(&mut rule, during, 0, Y), Err(Why::Busy));
        // The look-up answers and the Play goes ahead: that is what moves the generation.
        rule.done(play);
        rule.moved();
        assert_eq!(command(&mut rule, before, 0, X), Err(Why::Changed));
        assert_eq!((rule.generation(), rule.by(), rule.busy(2_000)), (2, None, false));
    }

    /// What the rule must never do: decide between two things the person did here.
    #[test]
    fn nothing_chosen_at_the_device_displaces_anything_else_chosen_there() {
        let mut rule = Rule::default();
        // Play on an album, then Next with nothing to go to (which changes nothing), then
        // Play on another album: both look-ups are still waiting, each as it was.
        let first = rule.wait(0, LOOK_UP);
        let second = rule.wait(500, LOOK_UP);
        assert!(rule.busy(600));
        // The second fails. The first answers and plays.
        rule.done(second);
        assert!(rule.busy(700));
        rule.done(first);
        rule.moved();
        assert_eq!((rule.busy(800), rule.generation()), (false, 2));
        // The volume is nudged and a song ends while a look-up waits: each moves the
        // generation, and the look-up is still waiting.
        let third = rule.wait(1_000, LOOK_UP);
        rule.moved();
        rule.moved();
        assert!(rule.busy(1_100));
        rule.done(third);
        // Done twice, or for a wait that ran out, it changes nothing.
        rule.done(third);
        assert_eq!((rule.busy(1_200), rule.generation()), (false, 4));
    }

    #[test]
    fn a_look_up_that_never_answers_does_not_leave_the_device_busy_for_ever() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        let _never = rule.wait(1_000, LOOK_UP);
        assert_eq!(rule.admit(seen, 0, 60_999), Err(Why::Busy));
        // Its time is up. Nothing changed meanwhile, so the command stands.
        assert!(!rule.busy(61_000));
        let reservation = rule.admit(seen, 1, 61_000).unwrap();
        assert_eq!(rule.finish(reservation, seen, 1, true, X), Ok(()));
        // However many are left waiting, only so many count, and each for its own time.
        let mut rule = Rule::default();
        for n in 0..100 {
            rule.wait(n, LOOK_UP);
        }
        assert_eq!(rule.waiting.len(), MOST_WAITING);
        assert!(rule.busy(60_050) && !rule.busy(60_100));
    }

    #[test]
    fn a_choice_at_the_device_while_a_command_s_work_is_under_way() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        let reservation = rule.admit(seen, 0, 0).unwrap();
        rule.moved();
        assert_eq!(rule.finish(reservation, seen, 0, true, X), Err(Why::Changed));
        assert_eq!((rule.by(), rule.busy(0)), (None, false));
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
    fn two_devices_at_the_same_instant() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        assert_eq!((command(&mut rule, seen, 0, X), command(&mut rule, seen, 0, Y)), (Ok(()), Err(Why::Changed)));
        assert_eq!(rule.by(), Some(X));
        // And while one is being carried out, the other is told the device is busy.
        let mut rule = Rule::default();
        let first = rule.admit(seen, 0, 0).unwrap();
        assert_eq!(rule.admit(seen, 0, 0), Err(Why::Busy));
        assert_eq!(rule.finish(first, seen, 0, true, X), Ok(()));
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
        let reservation = rule.admit(seen, 0, 0).unwrap();
        assert_eq!(rule.finish(reservation, seen, 0, false, X), Err(Why::Cannot));
        // It is not carried out and not named, and the device is as it was and free.
        assert_eq!((rule.by(), rule.busy(0), rule.generation()), (None, false, 1));
    }

    /// Only the command that is open can be finished, and only once.
    #[test]
    fn a_command_is_finished_once_and_only_while_it_is_the_open_one() {
        let mut rule = Rule::default();
        let seen = at(&rule, 0);
        let reservation = rule.admit(seen, 0, 0).unwrap();
        assert_eq!(rule.finish(reservation, seen, 0, true, X), Ok(()));
        // The same reservation again: the player must not be changed a second time.
        assert_eq!(rule.finish(reservation, seen, 0, true, X), Err(Why::Changed));
        assert_eq!((rule.generation(), rule.by()), (2, Some(X)));
        // One that was refused at the last step is over too.
        let seen = at(&rule, 0);
        let reservation = rule.admit(seen, 0, 0).unwrap();
        assert_eq!(rule.finish(reservation, seen, 5, true, Y), Err(Why::Late));
        assert_eq!(rule.finish(reservation, seen, 0, true, Y), Err(Why::Changed));
        assert_eq!((rule.generation(), rule.by()), (2, Some(X)));
    }

    #[test]
    fn the_numbers_cannot_run_out_or_wrap() {
        let mut rule = Rule { generation: u64::MAX, next: u64::MAX, ..Rule::default() };
        rule.moved();
        let waiting = rule.wait(u64::MAX, u64::MAX);
        rule.done(waiting);
        assert_eq!(rule.generation(), u64::MAX);
        assert_eq!(command(&mut Rule::default(), At { generation: 1, minute: u64::MAX }, 0, X), Err(Why::Late));
        assert_eq!(command(&mut Rule::default(), At { generation: 1, minute: u64::MAX - 1 }, u64::MAX, X), Ok(()));
    }
}
