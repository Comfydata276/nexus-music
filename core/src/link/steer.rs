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
//! **A command** is sent only against a fresh state that is not busy, one at a time. Its
//! answer settles it and nothing else: it never changes the state on the screen. After
//! [`WAIT_MS`](super::WAIT_MS) with no answer the device looks once ([`Tick::Check`]) and
//! says what is true ([`Outcome`]). It never sends the command again by itself; and an
//! answer that arrives after that is still believed.

use super::message::{At, Tell, Why};
use super::{FRESH_MS, Nonce, WAIT_MS};

/// The most looks waited on at once for one device. An older one is forgotten.
const MOST_LOOKS: usize = 4;

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
    /// The last command has not been answered or given up on.
    Unanswered,
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
struct Command {
    nonce: Nonce,
    at: At,
    sent_ms: u64,
    /// The look that asks what became of it, and when that was sent.
    check: Option<(Nonce, u64)>,
    outcome: Option<Outcome>,
    /// Its own answer has arrived. A second one is a repeat.
    answered: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Peer {
    shown: Option<Shown>,
    looks: Vec<(Nonce, u64)>,
    command: Option<Command>,
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
        let Some(shown) = &self.shown else { return Err(Wait::Look) };
        // A state from before the last command says nothing of what that command did.
        let since_command = self.command.as_ref().is_none_or(|command| shown.sent_ms > command.sent_ms);
        let age = now_ms.checked_sub(shown.sent_ms);
        if !since_command || !age.is_some_and(|age| age < FRESH_MS) {
            return Err(Wait::Look);
        }
        if shown.tell.busy {
            return Err(Wait::Busy);
        }
        Ok(shown.tell.at)
    }

    /// A command was sent, made against `at`.
    pub fn sent(&mut self, nonce: Nonce, at: At, now_ms: u64) {
        self.command = Some(Command { nonce, at, sent_ms: now_ms, check: None, outcome: None, answered: false });
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
            None if waited(command.sent_ms) => Tick::Check,
            Some((_, sent)) if waited(sent) => {
                command.outcome = Some(Outcome::Unknown);
                Tick::Settled(Outcome::Unknown)
            }
            _ => Tick::Nothing,
        }
    }

    /// A tell arrived from this device's place and opened.
    pub fn told(&mut self, tell: Tell) -> Told {
        let Some(re) = tell.re else { return Told::Dropped };
        // The answer to the command itself: once.
        if let Some(command) = &mut self.command
            && command.nonce == re
        {
            // A success is believed whenever it comes, once: a refusal that got here first (of
            // a repeat the relay's path made, say) does not stand against it. A refusal is
            // believed once, and never over a success.
            let done = tell.by == Some(command.nonce) || tell.ok == Some(true);
            let known = command.outcome == Some(Outcome::Done);
            if known || (command.answered && !done) {
                return Told::Dropped;
            }
            command.answered = true;
            let outcome = if done { Outcome::Done } else { Outcome::Refused(tell.why.unwrap_or(Why::Changed)) };
            command.outcome = Some(outcome);
            return Told::Settled(outcome);
        }
        // The answer to a look that is waited on: once.
        let Some(waiting) = self.looks.iter().position(|(nonce, _)| *nonce == re) else { return Told::Dropped };
        let (_, sent_ms) = self.looks.remove(waiting);
        let settled = self.command.as_mut().filter(|command| command.outcome.is_none()).and_then(|command| {
            let outcome = if tell.by == Some(command.nonce) {
                Outcome::Done
            } else if command.check.is_some_and(|(check, _)| check == re) {
                if tell.at.generation == command.at.generation { Outcome::NotReached } else { Outcome::Unknown }
            } else {
                return None;
            };
            command.outcome = Some(outcome);
            Some(outcome)
        });
        // An answer to an older look never replaces the answer to a newer one.
        let newer = self.shown.as_ref().is_none_or(|shown| sent_ms >= shown.sent_ms);
        if newer {
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

    const LOOK: Nonce = [1; 24];
    const LOOK_2: Nonce = [2; 24];
    const PAUSE: Nonce = [9; 24];

    fn state(generation: u64, re: Nonce) -> Tell {
        Tell { re: Some(re), ok: None, why: None, device: [7; 16], name: "Study PC".into(), at: At { generation, minute: 0 }, busy: false, by: None, playing: true, title: "Song".into(), artist: "Artist".into(), steering: String::new(), full: None, queue: None }
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
        // Once it is answered, the state it was made against is no state to make another
        // against: a look sent since is needed.
        assert_eq!(peer.told(Tell { ok: Some(true), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        assert_eq!(peer.may_send(2_000), Err(Wait::Look));
        peer.looked(LOOK_2, 2_000);
        assert_eq!(peer.told(Tell { by: Some(PAUSE), ..state(6, LOOK_2) }), Told::Shown);
        assert_eq!(peer.may_send(2_100), Ok(At { generation: 6, minute: 0 }));
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

    /// The double skip, from the sender's side: the only answer to arrive is the refusal of
    /// the repeat the relay made, and it says who made the state.
    #[test]
    fn by_is_believed_whatever_else_the_tell_says() {
        let mut peer = paused();
        assert_eq!(peer.told(Tell { ok: Some(false), why: Some(Why::Changed), by: Some(PAUSE), ..state(6, PAUSE) }), Told::Settled(Outcome::Done));
        // And a look's answer that names it settles it before any answer of its own comes.
        let mut peer = paused();
        peer.looked(LOOK_2, 2_000);
        assert_eq!(peer.told(Tell { by: Some(PAUSE), ..state(6, LOOK_2) }), Told::ShownAndSettled(Outcome::Done));
        // Another look's answer that does not name it settles nothing.
        let mut peer = paused();
        peer.looked(LOOK_2, 2_000);
        assert_eq!(peer.told(state(6, LOOK_2)), Told::Shown);
        assert!(peer.waiting());
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
        assert_eq!(outcome_of(Tell { by: Some(PAUSE), ..state(6, LOOK_2) }).0, Told::ShownAndSettled(Outcome::Done));
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
