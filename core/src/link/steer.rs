//! The asking side: what a device knows of another, when it may send it a command, and
//! what it concludes when no answer comes.
//!
//! One [`Peer`] is kept for each place in the room. The app tells it what it sent and what
//! arrived, and the time (milliseconds, on a clock that counts sleep); it says what to
//! show and what to do.
//!
//! **What makes a state fresh.** Only an answer to a look this device sent and is still
//! waiting on. Each look is answered once: the same tell delivered again, or an old one
//! put in place of the answer, names a look that is no longer waited on and is dropped,
//! so a relay that repeats what it carried cannot keep an old state looking current. A
//! state's age is counted from when its look was **sent**, not from when the answer
//! came. An answer to an older look never replaces the answer to a newer one.
//!
//! **A command** is sent only against a fresh state that is not busy, one at a time. A
//! press made while one is unanswered waits ([`Peer::press`]), and is sent only once that
//! one is answered "carried out", against the state in that answer ([`Peer::next`]). An
//! answer settles its command and nothing else: it never changes the state on the screen. After
//! [`WAIT_MS`](super::WAIT_MS) with no answer the device looks once ([`Tick::Check`]) and
//! says what is true ([`Outcome`]). It never sends the command again by itself; and an
//! answer that arrives after that is still believed.
//!
//! **Only a device that speaks as this one does** (`docs/linking-design.md`, 5.10; a tell's
//! `speaks`). A state from one that does not is shown, and nothing is made against it
//! ([`Wait::Speaks`]). An answer from one that does not never says a command was carried
//! out, whatever it says: two generations read `ok`, `by` and `done` differently, which
//! is what makes them two. What became of the command is then not known.

use super::command::Command;
use super::message::{At, SPEAKS, Tell, Why};
use super::{FRESH_MS, Nonce, WAIT_MS};

/// The most looks waited on at once for one device. An older one is forgotten.
const MOST_LOOKS: usize = 4;
/// The most presses that wait behind a command that has not been answered.
pub const MOST_PRESSED: usize = 8;
/// How many times a command whose work is still under way on the other device is asked
/// after before the asking device stops waiting.
const MOST_CHECKS: u32 = 4;
/// News from a device prompts a look at it no more often than this.
pub const NEWS_LOOK_MS: u64 = 1_000;

/// What became of a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It was carried out.
    Done,
    /// The other device did nothing, and said why.
    Refused(Why),
    /// No answer; and when the other device was looked at, it was as it had been. The
    /// command had not reached it. It still may, for as long as the rule allows.
    NotReached,
    /// No answer; and the other device has changed, or could not be looked at. Whether the
    /// command was carried out is not known.
    Unknown,
}

/// Why a command cannot be sent just now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// There is no fresh state to make it against: look first.
    Look,
    /// The other device has accepted a choice and not carried it out: look again.
    Busy,
    /// The last command has not been answered or given up on, and as many presses as are
    /// kept are waiting behind it already.
    Unanswered,
    /// The other device does not speak as this one does (it says this number): nothing is
    /// sent to it but looks, until both are up to date (5.10).
    Speaks(u64),
}

/// What a press comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// Send it now, made against this state, and say so with [`Peer::sent`].
    Send(At),
    /// It waits for the answer to the command that is out, and is handed back by
    /// [`Peer::next`] if that answer is that it was carried out.
    Waits,
    /// It is not sent, and does not wait.
    No(Wait),
}

/// What a tell that arrived came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Told {
    /// Not an answer to anything waited on. Nothing changes.
    Dropped,
    /// A fresh state, to show.
    Shown,
    /// The command is settled. The state on the screen is as it was: look again.
    Settled(Outcome),
    /// Both: a look's answer that also says what became of the command.
    ShownAndSettled(Outcome),
}

/// What the passing of time calls for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    Nothing,
    /// The command has gone unanswered: send a look now, and say so with [`Peer::checking`].
    Check,
    /// The look that was to check went unanswered too.
    Settled(Outcome),
}

/// A state, and when the look it answers was sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub tell: Tell,
    pub sent_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct Sent {
    nonce: Nonce,
    at: At,
    sent_ms: u64,
    /// When the wait for its answer is counted from: when it was sent, or the last time
    /// the other device said its work was still under way.
    waited_from: u64,
    /// The look that asks what became of it, and when that was sent.
    check: Option<(Nonce, u64)>,
    checks: u32,
    outcome: Option<Outcome>,
    /// Its own answer has arrived. A second one is a repeat.
    answered: bool,
}

/// The state the next command may be made against: a fresh look's answer, or the answer
/// that said the last command was carried out; and when it was asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Base {
    at: At,
    busy: bool,
    ms: u64,
    /// How the device that said it speaks.
    speaks: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Peer {
    shown: Option<Shown>,
    looks: Vec<(Nonce, u64)>,
    command: Option<Sent>,
    base: Option<Base>,
    /// Presses made while a command is unanswered, in the order they were made.
    pressed: Vec<Command>,
    /// When news from this device last prompted a look.
    news_ms: Option<u64>,
}

impl Peer {
    /// The last fresh state, however old it now is.
    pub fn shown(&self) -> Option<&Shown> {
        self.shown.as_ref()
    }

    /// What became of the last command, once that is known.
    pub fn outcome(&self) -> Option<Outcome> {
        self.command.as_ref().and_then(|command| command.outcome)
    }

    /// Whether a command is out and not yet answered or given up on.
    pub fn waiting(&self) -> bool {
        self.command.as_ref().is_some_and(|command| command.outcome.is_none())
    }

    /// How many presses are waiting behind the command that is out.
    pub fn pressed(&self) -> usize {
        self.pressed.len()
    }

    /// A look was sent.
    pub fn looked(&mut self, nonce: Nonce, now_ms: u64) {
        if self.looks.len() >= MOST_LOOKS {
            self.looks.remove(0);
        }
        self.looks.push((nonce, now_ms));
    }

    /// The state a command may be made against now.
    pub fn may_send(&self, now_ms: u64) -> Result<At, Wait> {
        if self.waiting() {
            return Err(Wait::Unanswered);
        }
        // (A state from before the last command says nothing of what that command did: it
        // was let go of when the command was sent.)
        let Some(base) = &self.base else { return Err(Wait::Look) };
        if !now_ms.checked_sub(base.ms).is_some_and(|age| age < FRESH_MS) {
            return Err(Wait::Look);
        }
        if base.speaks != SPEAKS {
            return Err(Wait::Speaks(base.speaks));
        }
        if base.busy {
            return Err(Wait::Busy);
        }
        Ok(base.at)
    }

    /// Something was pressed for this device. One command is out at a time: a press made
    /// while one is unanswered waits behind it, up to [`MOST_PRESSED`] of them; of presses
    /// that mean only their last (a volume slider's), only the last waits.
    pub fn press(&mut self, command: &Command, now_ms: u64) -> Press {
        if self.waiting() {
            if command.latest_only() {
                self.pressed.retain(|earlier| earlier.name() != command.name());
            }
            if self.pressed.len() >= MOST_PRESSED {
                return Press::No(Wait::Unanswered);
            }
            self.pressed.push(command.clone());
            return Press::Waits;
        }
        match self.may_send(now_ms) {
            Ok(at) => Press::Send(at),
            Err(wait) => Press::No(wait),
        }
    }

    /// The next press that was waiting, and the state to make it against, once the command
    /// before it has been answered "carried out". If that one was refused, or what became
    /// of it is not known, those waiting were let go of when that was learnt: they were
    /// made on the strength of something that did not happen.
    pub fn next(&mut self, now_ms: u64) -> Option<(Command, At)> {
        if self.waiting() || self.pressed.is_empty() {
            return None;
        }
        match self.may_send(now_ms) {
            Ok(at) => Some((self.pressed.remove(0), at)),
            Err(_) => {
                self.pressed.clear();
                None
            }
        }
    }

    /// A command was sent, made against `at`.
    pub fn sent(&mut self, nonce: Nonce, at: At, now_ms: u64) {
        self.command = Some(Sent { nonce, at, sent_ms: now_ms, waited_from: now_ms, check: None, checks: 0, outcome: None, answered: false });
        self.base = None;
    }

    /// The look [`Tick::Check`] called for was sent.
    pub fn checking(&mut self, nonce: Nonce, now_ms: u64) {
        self.looked(nonce, now_ms);
        if let Some(command) = &mut self.command {
            command.check = Some((nonce, now_ms));
        }
    }

    pub fn tick(&mut self, now_ms: u64) -> Tick {
        let Some(command) = &mut self.command else { return Tick::Nothing };
        let waited = |since: u64| now_ms.checked_sub(since).is_some_and(|age| age >= WAIT_MS);
        match command.check {
            _ if command.outcome.is_some() => Tick::Nothing,
            None if waited(command.waited_from) => Tick::Check,
            Some((_, sent)) if waited(sent) => {
                command.outcome = Some(Outcome::Unknown);
                self.pressed.clear();
                Tick::Settled(Outcome::Unknown)
            }
            _ => Tick::Nothing,
        }
    }

    /// News arrived from this device and opened: whether to look at it now. News is never
    /// itself a state to show or to act on (anything on the way can deliver it again
    /// later); it is a reason to ask, and no more often than once a second.
    pub fn news(&mut self, now_ms: u64) -> bool {
        if self.news_ms.is_some_and(|last| now_ms.checked_sub(last).is_none_or(|since| since < NEWS_LOOK_MS)) {
            return false;
        }
        self.news_ms = Some(now_ms);
        true
    }

    /// A tell arrived from this device's place and opened.
    pub fn told(&mut self, tell: Tell) -> Told {
        let Some(re) = tell.re else { return Told::Dropped };
        // The answer to the command itself: once.
        if let Some(command) = &mut self.command
            && command.nonce == re
        {
            // From a device that does not speak as this one does, it is not read: once,
            // it says that what became of the command is not known. (A success in this
            // device's own generation, if one comes, is still believed.)
            if tell.speaks != SPEAKS {
                if command.answered || command.outcome == Some(Outcome::Done) {
                    return Told::Dropped;
                }
                command.answered = true;
                command.outcome = Some(Outcome::Unknown);
                self.pressed.clear();
                return Told::Settled(Outcome::Unknown);
            }
            // A success is believed whenever it comes, once: a refusal that got here first (of
            // a repeat the relay's path made, say) does not stand against it. A refusal is
            // believed once, and never over a success. **Success is what the device says
            // it is**: its `ok`, or that the command it names as having made its state was
            // carried out to its end. That it names the command and is not busy is not
            // that: a queue it took may have run past its wait without ever sounding.
            let done = tell.ok == Some(true) || (tell.by == Some(command.nonce) && tell.done);
            // The device says this command made its state and that its work is still under
            // way there (a queue it took has not sounded yet). Whatever this is the answer
            // to (a repeat of the command made on the way, refused as busy), it settles
            // nothing: the command was taken, and its own answer is still to come. What
            // waits behind it goes on waiting.
            if !done && tell.busy && tell.by == Some(command.nonce) {
                return Told::Dropped;
            }
            let known = command.outcome == Some(Outcome::Done);
            if known || (command.answered && !done) {
                return Told::Dropped;
            }
            command.answered = true;
            let outcome = if done { Outcome::Done } else { Outcome::Refused(tell.why.unwrap_or(Why::Changed)) };
            command.outcome = Some(outcome);
            if done {
                // What it says of the device is the state the next command is made against.
                self.base = Some(Base { at: tell.at, busy: tell.busy, ms: command.sent_ms, speaks: tell.speaks });
            } else {
                self.pressed.clear();
            }
            return Told::Settled(outcome);
        }
        // The answer to a look that is waited on: once.
        let Some(waiting) = self.looks.iter().position(|(nonce, _)| *nonce == re) else { return Told::Dropped };
        let (_, sent_ms) = self.looks.remove(waiting);
        let settled = self.command.as_mut().filter(|command| command.outcome.is_none()).and_then(|command| {
            let checked = command.check.is_some_and(|(check, _)| check == re);
            let outcome = if tell.speaks != SPEAKS {
                // (Not read, whatever it names: the look that was to say what became of
                // the command has not said, and no other look says anything of it.)
                if checked { Outcome::Unknown } else { return None }
            } else if tell.by == Some(command.nonce) && tell.done {
                Outcome::Done
            } else if checked && tell.by == Some(command.nonce) && tell.busy && command.checks + 1 < MOST_CHECKS {
                // It made the state, and its work is still under way there (a queue taken
                // over that has not begun to sound): wait on, and ask again.
                command.checks += 1;
                command.check = None;
                command.waited_from = sent_ms;
                return None;
            } else if checked && tell.by == Some(command.nonce) && !tell.busy {
                // It names the command, is no longer at work on it, and does not say it
                // was carried out: the work came to nothing (the queue never sounded).
                Outcome::Refused(Why::Cannot)
            } else if checked {
                if tell.at.generation == command.at.generation { Outcome::NotReached } else { Outcome::Unknown }
            } else {
                return None;
            };
            command.outcome = Some(outcome);
            Some(outcome)
        });
        if settled.is_some_and(|outcome| outcome != Outcome::Done) {
            self.pressed.clear();
        }
        // An answer to an older look never replaces the answer to a newer one.
        let newer = self.shown.as_ref().is_none_or(|shown| sent_ms >= shown.sent_ms);
        if newer {
            // A state to act on only if it was asked for after the last command was sent.
            if self.command.as_ref().is_none_or(|command| sent_ms > command.sent_ms) {
                self.base = Some(Base { at: tell.at, busy: tell.busy, ms: sent_ms, speaks: tell.speaks });
            }
            self.shown = Some(Shown { tell, sent_ms });
        }
        match (settled, newer) {
            (Some(outcome), true) => Told::ShownAndSettled(outcome),
            (Some(outcome), false) => Told::Settled(outcome),
            (None, true) => Told::Shown,
            (None, false) => Told::Dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::command::Command;

    const LOOK: Nonce = [1; 24];
    const LOOK_2: Nonce = [2; 24];
    const PAUSE: Nonce = [9; 24];

    fn state(generation: u64, re: Nonce) -> Tell {
        Tell { re: Some(re), ok: None, why: None, device: [7; 16], name: "Study PC".into(), speaks: SPEAKS, at: At { generation, minute: 0 }, busy: false, by: None, done: false, playing: true, title: "Song".into(), artist: "Artist".into(), steering: String::new(), full: None, queue: None }
    }

    /// A peer that has been looked at once, at `sent`, and answered with generation 5.
    fn seen(sent: u64) -> Peer {
        let mut peer = Peer::default();
        peer.looked(LOOK, sent);
        assert_eq!(peer.told(state(5, LOOK)), Told::Shown);
        peer
    }

    /// And then sent a Pause, a second after its look.
    fn paused() -> Peer {
        let mut peer = seen(0);
        let at = peer.may_send(1_000).unwrap();
        peer.sent(PAUSE, at, 1_000);
        peer
    }

    #[test]
    fn only_an_answer_to_a_look_of_its_own_makes_a_state_fresh() {
        let mut peer = Peer::default();
        assert_eq!(peer.may_send(0), Err(Wait::Look));
        // A tell nobody asked for, and an answer to somebody else's look.
        assert_eq!(peer.told(Tell { re: None, ..state(5, LOOK) }), Told::Dropped);
        assert_eq!(peer.told(state(5, LOOK)), Told::Dropped);
        peer.looked(LOOK, 100);
        assert_eq!(peer.told(state(5, LOOK)), Told::Shown);
        assert_eq!(peer.may_send(200), Ok(At { generation: 5, minute: 0 }));
    }

    /// 5.10: a state from a device that speaks otherwise is shown, and nothing is made
    /// against it. Its answers settle nothing as done.
    #[test]
    fn nothing_is_made_against_a_state_from_another_generation_and_its_answers_say_nothing_was_done() {
        for theirs in [1, SPEAKS + 1] {
            let mut peer = Peer::default();
            peer.looked(LOOK, 0);
            assert_eq!(peer.told(Tell { speaks: theirs, ..state(5, LOOK) }), Told::Shown);
            assert_eq!(peer.shown().map(|shown| shown.tell.speaks), Some(theirs));
            assert_eq!(peer.may_send(100), Err(Wait::Speaks(theirs)));
            assert_eq!(peer.press(&Command::Pause, 100), Press::No(Wait::Speaks(theirs)));
            // Looked at again once it speaks as this device does: as any other.
            peer.looked(LOOK_2, 1_000);
            assert_eq!(peer.told(state(5, LOOK_2)), Told::Shown);
            assert!(peer.may_send(1_100).is_ok());
            // The command's own answer, saying `ok` in another generation: what became of
            // the command is not known, and nothing is made against what it says.
            let mut peer = paused();
            assert_eq!(peer.press(&Command::Next, 1_100), Press::Waits);
            assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), done: true, speaks: theirs, ..state(6, PAUSE) }), Told::Settled(Outcome::Unknown));
            assert_eq!((peer.outcome(), peer.pressed(), peer.may_send(1_200)), (Some(Outcome::Unknown), 0, Err(Wait::Look)));
            // (A success in this device's own generation is still believed when it comes.)
            assert_eq!(peer.told(Tell { ok: Some(true), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
            // The look that checks on an unanswered command, answered "it made my state,
            // to its end" in another generation: not known, and never done.
            let mut peer = paused();
            assert_eq!(peer.tick(1_000 + WAIT_MS), Tick::Check);
            peer.checking(LOOK_2, 1_000 + WAIT_MS);
            assert_eq!(peer.told(Tell { by: Some(PAUSE), done: true, speaks: theirs, ..state(6, LOOK_2) }), Told::ShownAndSettled(Outcome::Unknown));
            assert_eq!(peer.may_send(1_100 + WAIT_MS), Err(Wait::Speaks(theirs)));
            // And an ordinary look's answer that names the command so settles nothing.
            let mut peer = paused();
            peer.looked(LOOK_2, 2_000);
            assert_eq!(peer.told(Tell { by: Some(PAUSE), done: true, speaks: theirs, ..state(6, LOOK_2) }), Told::Shown);
            assert!(peer.waiting());
        }
    }

    #[test]
    fn a_tell_delivered_again_is_dropped_and_makes_nothing_fresh() {
        let mut peer = seen(0);
        // The relay repeats the genuine answer for as long as it likes, and withholds the
        // answers to every later look.
        peer.looked(LOOK_2, 30_000);
        for _ in 0..3 {
            assert_eq!(peer.told(state(5, LOOK)), Told::Dropped);
        }
        assert_eq!(peer.shown().map(|shown| shown.sent_ms), Some(0));
        // The state is as old as its look, and is not acted on once that is too old.
        assert!(peer.may_send(FRESH_MS - 1).is_ok());
        assert_eq!(peer.may_send(FRESH_MS), Err(Wait::Look));
        // A clock that reads earlier than the look is no reason to trust it.
        assert_eq!(seen(5_000).may_send(4_000), Err(Wait::Look));
    }

    #[test]
    fn an_answer_to_an_older_look_never_replaces_a_newer_one() {
        let mut peer = Peer::default();
        peer.looked(LOOK, 0);
        peer.looked(LOOK_2, 5_000);
        assert_eq!(peer.told(state(6, LOOK_2)), Told::Shown);
        assert_eq!(peer.told(state(5, LOOK)), Told::Dropped);
        assert_eq!(peer.shown().map(|shown| (shown.tell.at.generation, shown.sent_ms)), Some((6, 5_000)));
        // Only so many looks are waited on: the oldest is forgotten, and its answer with it.
        let mut peer = Peer::default();
        for n in 0..6u8 {
            peer.looked([n; 24], u64::from(n));
        }
        assert_eq!(peer.told(state(5, [0; 24])), Told::Dropped);
        assert_eq!(peer.told(state(5, [5; 24])), Told::Shown);
    }

    #[test]
    fn a_command_is_not_sent_against_a_busy_state_or_while_one_is_unanswered() {
        let mut peer = Peer::default();
        peer.looked(LOOK, 0);
        peer.told(Tell { busy: true, ..state(5, LOOK) });
        assert_eq!(peer.may_send(100), Err(Wait::Busy));
        let mut peer = paused();
        assert_eq!(peer.may_send(1_500), Err(Wait::Unanswered));
        // Once it is answered "carried out", the state in that answer is the one the next is
        // made against: not the one from before it, which it changed.
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        assert_eq!(peer.may_send(2_000), Ok(At { generation: 6, minute: 0 }));
        // It is as old as the command it answers, and no fresher.
        assert_eq!(peer.may_send(1_000 + FRESH_MS), Err(Wait::Look));
        // A refusal leaves nothing to send against: look first.
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Changed), ..state(7, PAUSE) }), Told::Settled(Outcome::Refused(Why::Changed)));
        assert_eq!(peer.may_send(2_000), Err(Wait::Look));
        // And an answer to a look sent before the command says nothing of what it did.
        let mut peer = Peer::default();
        peer.looked(LOOK, 0);
        peer.told(state(5, LOOK));
        peer.looked(LOOK_2, 500);
        peer.sent(PAUSE, At { generation: 5, minute: 0 }, 1_000);
        peer.told(Tell { ok: Some(false), why: Some(Why::Late), ..state(5, PAUSE) });
        assert_eq!(peer.told(state(5, LOOK_2)), Told::Shown);
        assert_eq!(peer.may_send(2_000), Err(Wait::Look));
        peer.looked([3; 24], 2_000);
        assert_eq!(peer.told(state(5, [3; 24])), Told::Shown);
        assert_eq!(peer.may_send(2_100), Ok(At { generation: 5, minute: 0 }));
    }

    #[test]
    fn presses_made_while_a_command_is_unanswered_wait_and_go_one_at_a_time_after_an_ok() {
        let mut peer = paused();
        assert_eq!(peer.press(&Command::Next, 1_100), Press::Waits);
        assert_eq!(peer.press(&Command::Next, 1_200), Press::Waits);
        // Nothing goes until the one that is out is answered.
        assert_eq!(peer.next(1_300), None);
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        // Then the first, against the state in that answer; and the second only after the
        // first's own answer.
        assert_eq!(peer.next(1_400), Some((Command::Next, At { generation: 6, minute: 0 })));
        peer.sent(LOOK_2, At { generation: 6, minute: 0 }, 1_400);
        assert_eq!((peer.next(1_500), peer.pressed()), (None, 1));
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(LOOK_2), ..state(7, LOOK_2) }), Told::Settled(Outcome::Done));
        assert_eq!(peer.next(1_600), Some((Command::Next, At { generation: 7, minute: 0 })));
        assert_eq!(peer.pressed(), 0);
        // With nothing out, a press is sent at once, or not at all: it never waits on a look.
        let mut fresh = seen(0);
        assert_eq!(fresh.press(&Command::Next, 100), Press::Send(At { generation: 5, minute: 0 }));
        assert_eq!(Peer::default().press(&Command::Next, 100), Press::No(Wait::Look));
    }

    #[test]
    fn eight_presses_wait_at_the_most_and_of_a_slider_s_only_the_last() {
        let mut peer = paused();
        for n in 0..12 {
            assert_eq!(peer.press(&Command::Volume { to: f64::from(n) / 20.0 }, 1_100), Press::Waits);
        }
        assert_eq!(peer.pressed(), 1);
        for n in 0..7 {
            assert_eq!(peer.press(&Command::Next, 1_200), Press::Waits, "{n}");
        }
        assert_eq!(peer.press(&Command::Next, 1_300), Press::No(Wait::Unanswered));
        // A later turn of the slider still takes the place of the earlier one.
        assert_eq!(peer.press(&Command::Volume { to: 0.9 }, 1_300), Press::Waits);
        assert_eq!(peer.pressed(), MOST_PRESSED);
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        assert_eq!(peer.next(1_400).map(|(command, _)| command), Some(Command::Next));
    }

    #[test]
    fn a_refusal_or_an_unknown_outcome_lets_go_of_what_was_waiting() {
        let mut peer = paused();
        peer.press(&Command::Next, 1_100);
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Changed), ..state(7, PAUSE) }), Told::Settled(Outcome::Refused(Why::Changed)));
        assert_eq!((peer.pressed(), peer.next(1_200)), (0, None));
        // No answer, and the check not answered either.
        let mut peer = paused();
        peer.press(&Command::Next, 1_100);
        assert_eq!(peer.tick(13_000), Tick::Check);
        peer.checking(LOOK_2, 13_000);
        assert_eq!(peer.tick(13_000 + WAIT_MS), Tick::Settled(Outcome::Unknown));
        assert_eq!(peer.pressed(), 0);
        // It had not arrived when checked: what was pressed after it is not sent ahead of it.
        let mut peer = paused();
        peer.press(&Command::Next, 1_100);
        peer.tick(13_000);
        peer.checking(LOOK_2, 13_000);
        assert_eq!(peer.told(state(5, LOOK_2)), Told::ShownAndSettled(Outcome::NotReached));
        assert_eq!((peer.pressed(), peer.next(13_100)), (0, None));
    }

    /// A queue taken over is the state's maker from the moment it is loaded, and is not
    /// "done" until it sounds: until then the device says it is busy.
    #[test]
    fn a_command_whose_work_is_still_under_way_is_waited_on_and_asked_after_again() {
        let mut peer = paused();
        peer.looked(LOOK_2, 2_000);
        assert_eq!(peer.told(Tell { by: Some(PAUSE), busy: true, ..state(6, LOOK_2) }), Told::Shown);
        assert!(peer.waiting());
        // No answer in twelve seconds: it asks, is told the work is under way, and waits
        // another twelve from then before asking again.
        assert_eq!(peer.tick(13_000), Tick::Check);
        peer.checking([3; 24], 13_000);
        assert_eq!(peer.told(Tell { by: Some(PAUSE), busy: true, ..state(6, [3; 24]) }), Told::Shown);
        assert_eq!((peer.waiting(), peer.tick(13_000 + WAIT_MS - 1)), (true, Tick::Nothing));
        assert_eq!(peer.tick(13_000 + WAIT_MS), Tick::Check);
        peer.checking([4; 24], 25_000);
        // Its own answer, when the sound has started.
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        // One that never finishes is not waited on for ever.
        let mut peer = paused();
        let mut now = 1_000;
        let outcome = loop {
            now += WAIT_MS;
            assert_eq!(peer.tick(now), Tick::Check);
            let look = [(now / 1_000) as u8; 24];
            peer.checking(look, now);
            if let Told::ShownAndSettled(outcome) = peer.told(Tell { by: Some(PAUSE), busy: true, ..state(6, look) }) {
                break outcome;
            }
        };
        assert_eq!((outcome, now), (Outcome::Unknown, 1_000 + 4 * WAIT_MS));
        // And an answer that says "busy" beside the name of the command is no success yet,
        // and no refusal either: the command is the one under way, and is waited on.
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Busy), by: Some(PAUSE), busy: true, ..state(6, PAUSE) }), Told::Dropped);
        assert!(peer.waiting());
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
    }

    #[test]
    fn news_prompts_a_look_once_a_second_at_the_most_and_is_never_a_state() {
        let mut peer = seen(0);
        let news = Tell { re: None, ..state(9, LOOK) };
        assert_eq!(peer.told(news.clone()), Told::Dropped);
        assert_eq!(peer.shown().map(|shown| shown.tell.at.generation), Some(5));
        assert_eq!(peer.may_send(100), Ok(At { generation: 5, minute: 0 }));
        assert!(peer.news(1_000));
        assert!(!peer.news(1_500) && !peer.news(1_999));
        assert!(peer.news(2_000));
        // A clock that reads earlier is no reason to look more often.
        assert!(!peer.news(1_000));
    }

    #[test]
    fn an_answer_settles_its_command_and_does_not_touch_the_screen() {
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Busy), busy: true, ..state(9, PAUSE) }), Told::Settled(Outcome::Refused(Why::Busy)));
        assert_eq!(peer.outcome(), Some(Outcome::Refused(Why::Busy)));
        // The state shown is still the one a look fetched.
        assert_eq!(peer.shown().map(|shown| shown.tell.at.generation), Some(5));
        // The same refusal again changes nothing.
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Late), ..state(9, PAUSE) }), Told::Dropped);
        assert_eq!(peer.outcome(), Some(Outcome::Refused(Why::Busy)));
    }

    /// B carries the Pause out; someone presses Play on B; the relay's path delivers the
    /// Pause again, and B's refusal of the repeat reaches A before the first answer does.
    #[test]
    fn a_success_that_arrives_after_a_refusal_of_the_same_command_corrects_it() {
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Changed), ..state(7, PAUSE) }), Told::Settled(Outcome::Refused(Why::Changed)));
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        assert_eq!(peer.outcome(), Some(Outcome::Done));
        // And then nothing takes it back: not the refusal again, nor the success again.
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Changed), ..state(7, PAUSE) }), Told::Dropped);
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Dropped);
        assert_eq!(peer.outcome(), Some(Outcome::Done));
    }

    /// A queue is sent, taken, and has not sounded yet; the path delivers the same frame
    /// again, and the device refuses the repeat as busy. That refusal carries the command's
    /// own name, and says the command is the one under way.
    #[test]
    fn a_repeat_refused_while_the_command_itself_is_under_way_settles_nothing() {
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Busy), by: Some(PAUSE), busy: true, ..state(6, PAUSE) }), Told::Dropped);
        assert_eq!(peer.outcome(), None);
        // Its own answer then settles it, either way.
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Busy), by: Some(PAUSE), busy: true, ..state(6, PAUSE) }), Told::Dropped);
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Cannot), ..state(7, PAUSE) }), Told::Settled(Outcome::Refused(Why::Cannot)));
        // A refusal as busy that does not name the command is the device being busy with
        // something else: the command was not taken, and that is that.
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Busy), busy: true, ..state(5, PAUSE) }), Told::Settled(Outcome::Refused(Why::Busy)));
    }

    /// The double skip, from the sender's side: the only answer to arrive is the refusal of
    /// the repeat the relay made, and it says who made the state.
    #[test]
    fn by_is_believed_whatever_else_the_tell_says() {
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Changed), by: Some(PAUSE), done: true, ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        // And a look's answer that names it settles it before any answer of its own comes.
        let mut peer = paused();
        peer.looked(LOOK_2, 2_000);
        assert_eq!(peer.told(Tell { by: Some(PAUSE), done: true, ..state(6, LOOK_2) }), Told::ShownAndSettled(Outcome::Done));
        // Another look's answer that does not name it settles nothing.
        let mut peer = paused();
        peer.looked(LOOK_2, 2_000);
        assert_eq!(peer.told(state(6, LOOK_2)), Told::Shown);
        assert!(peer.waiting());
    }

    /// The device took the queue, ran past its wait without its sound ever starting, and is
    /// looked at before it has said so: it names the command and is not busy. That is who
    /// made its state, not how the command ended. A sender that took it for "done" would
    /// stop its own music for a device that never started.
    #[test]
    fn a_command_named_by_a_device_that_is_not_busy_is_not_done_unless_the_device_says_so() {
        let named = |re: Nonce| Tell { by: Some(PAUSE), ..state(6, re) };
        // A look's answer before any check: it settles nothing.
        let mut peer = paused();
        peer.looked(LOOK_2, 2_000);
        assert_eq!(peer.told(named(LOOK_2)), Told::Shown);
        assert!(peer.waiting());
        // Its own answer delivered again as a refusal, naming it: a refusal, not a success.
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Changed), ..named(PAUSE) }), Told::Settled(Outcome::Refused(Why::Changed)));
        // At the check after twelve seconds: its work came to nothing.
        let mut peer = paused();
        peer.press(&Command::Next, 1_100);
        assert_eq!(peer.tick(1_000 + WAIT_MS), Tick::Check);
        peer.checking(LOOK_2, 13_000);
        assert_eq!(peer.told(named(LOOK_2)), Told::ShownAndSettled(Outcome::Refused(Why::Cannot)));
        assert_eq!((peer.pressed(), peer.waiting()), (0, false));
        // And when the device does say it: done, by a look's answer or by its own.
        let mut peer = paused();
        peer.looked(LOOK_2, 2_000);
        assert_eq!(peer.told(Tell { done: true, ..named(LOOK_2) }), Told::ShownAndSettled(Outcome::Done));
    }

    #[test]
    fn after_twelve_seconds_it_looks_once_and_says_what_is_true() {
        let outcome_of = |answer: Tell| {
            let mut peer = paused();
            assert_eq!(peer.tick(1_000 + WAIT_MS - 1), Tick::Nothing);
            assert_eq!(peer.tick(1_000 + WAIT_MS), Tick::Check);
            peer.checking(LOOK_2, 13_000);
            // Asked again before the check is answered, it does not ask twice.
            assert_eq!(peer.tick(14_000), Tick::Nothing);
            (peer.told(answer), peer)
        };
        // It made the state: done. The device is as it was: it had not arrived. Anything else: not known.
        assert_eq!(outcome_of(Tell { by: Some(PAUSE), done: true, ..state(6, LOOK_2) }).0, Told::ShownAndSettled(Outcome::Done));
        assert_eq!(outcome_of(state(5, LOOK_2)).0, Told::ShownAndSettled(Outcome::NotReached));
        assert_eq!(outcome_of(state(7, LOOK_2)).0, Told::ShownAndSettled(Outcome::Unknown));
        // Nothing more is sent for it, whatever the outcome.
        let (_, mut peer) = outcome_of(state(5, LOOK_2));
        assert_eq!(peer.tick(60_000), Tick::Nothing);
    }

    #[test]
    fn a_check_that_is_not_answered_either_is_not_known() {
        let mut peer = paused();
        assert_eq!(peer.tick(13_000), Tick::Check);
        peer.checking(LOOK_2, 13_000);
        assert_eq!(peer.tick(13_000 + WAIT_MS), Tick::Settled(Outcome::Unknown));
        assert_eq!((peer.outcome(), peer.waiting(), peer.tick(99_000)), (Some(Outcome::Unknown), false, Tick::Nothing));
    }

    /// "That hadn't reached Study PC when checked" has been said; the command then arrives
    /// in time and is carried out.
    #[test]
    fn an_arrival_after_the_timeout_was_reported_is_still_believed() {
        let mut peer = paused();
        peer.tick(13_000);
        peer.checking(LOOK_2, 13_000);
        assert_eq!(peer.told(state(5, LOOK_2)), Told::ShownAndSettled(Outcome::NotReached));
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        assert_eq!(peer.outcome(), Some(Outcome::Done));
        // And after "not known", a late refusal is believed the same.
        let mut peer = paused();
        peer.tick(13_000);
        peer.checking(LOOK_2, 13_000);
        peer.tick(13_000 + WAIT_MS);
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Late), ..state(5, PAUSE) }), Told::Settled(Outcome::Refused(Why::Late)));
    }

    /// The app is closed after a Pause is sent and opened again. The new run has a new
    /// place, so the answer could not even be opened; were it delivered and opened all the
    /// same, it names nothing this run sent.
    #[test]
    fn the_sender_restarting_learns_nothing_from_what_the_old_run_asked() {
        let mut peer = Peer::default();
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Dropped);
        assert_eq!((peer.outcome(), peer.shown().is_some(), peer.may_send(0)), (None, false, Err(Wait::Look)));
        // What the other device is doing, it learns from its own first look.
        peer.looked(LOOK, 0);
        assert_eq!(peer.told(Tell { by: Some(PAUSE), ..state(6, LOOK) }), Told::Shown);
        assert_eq!(peer.outcome(), None);
    }
}
