//! The sending half (13.3, 14.4): what was listened to, and the Diagnostics report, sent
//! to the backend under the session, by the same rules as every other ask.
//!
//! **Events.** The journal's summary ([`Happened::EventsWaiting`]) is all the machine holds
//! of it. A round of sending begins on something that may have changed: a new summary, the
//! backend being reachable again (a start, the network back, Offline off, an answer after
//! it could not be reached), or any answer once what kept it from sending is gone (the
//! `hello` read, a sign-in). **Never on a bare tick.** A round reads waiting events from the
//! journal's reader, sends at most 500 and 512 KiB, applies each event's answer, and reads
//! again from after what it sent, until a read comes back empty. An answer that takes or
//! refuses nothing (all "not dealt with"), "busy", a refusal of the whole call, a failure or
//! an answer that does not read **holds** sending: it waits for the backend to be reachable
//! again (never from that same answer), or, while the backend answers, for more events to
//! wait than when it held, past any "come back in" it said.
//!
//! **Diagnostics.** On a tick only, because 14.4 names its own timing (C6's
//! [`diag::Schedule`]): when an attempt is due, the schedule is kept (before anything is
//! read or sent), the text is read, and one request is asked. A failure or no answer wastes
//! that attempt only. Nothing is tried again outside the schedule.

use super::machine::{hidden, request_id};
use super::*;

/// The sending half's events, between `start` and `stop`.
#[derive(Default)]
pub(super) struct Sending {
    /// The journal's latest summary for the account signed in and its life; one for any
    /// other is ignored entirely.
    waiting: Option<Waiting>,
    /// A summary came while a read was out: an empty read then reads once more, so that a
    /// flush after the read's snapshot is not lost.
    fresh: bool,
    /// A round under way: the sequence the next read starts from.
    round: Option<u64>,
    /// The read waited for, by its number.
    reading: Option<u64>,
    /// Sending waits for something to change.
    held: Option<Held>,
    /// The hold was made by the answer being dealt with now: that answer's own settling
    /// does not lift it (the B1 rule).
    held_now: bool,
}

#[derive(Clone, Copy)]
struct Waiting {
    account: JournalAccount,
    life: u64,
    pending: u64,
    from_sequence: u64,
}

#[derive(Clone, Copy)]
struct Held {
    /// How many waited when it held: more is a change.
    pending: u64,
    /// "Come back in N seconds": not before this time.
    until_ms: Option<u64>,
}

/// Diagnostics, between `start` and `stop`.
pub(super) struct Diagnostics {
    schedule: Option<diag::Schedule>,
    /// The person's own choice on this device.
    choice: Option<bool>,
    in_use: bool,
    /// The read waited for, by its number.
    reading: Option<u64>,
}

impl Diagnostics {
    /// The device schedule from an earlier sign-in, if one was kept.
    pub(super) fn from_kept(choice: Option<bool>, kept: Option<DiagSchedule>, _now_ms: u64, out: &mut Vec<Do>) -> Diagnostics {
        let schedule = match kept {
            Some(kept) => match kept.schedule() {
                Some(schedule) => Some(schedule),
                None => {
                    out.push(Do::Forget(Forget::DiagSchedule));
                    None
                }
            },
            None => None,
        };
        Diagnostics { schedule, choice, in_use: false, reading: None }
    }

    pub(super) fn signed_in(&mut self, now_ms: u64, out: &mut Vec<Do>) {
        if self.schedule.is_none() {
            let schedule = diag::Schedule::new(now_ms.min(events::TIME_MAX_MS)).expect("a valid time");
            out.push(Do::Keep(Keep::DiagSchedule(DiagSchedule::of(&schedule))));
            self.schedule = Some(schedule);
        }
    }
}

impl Sending {
    /// Everything but the journal's summary goes at an ending, an account change or
    /// Offline: what was being read or sent is no longer waited for.
    pub(super) fn reset(&mut self) {
        self.round = None;
        self.reading = None;
        self.held = None;
        self.held_now = false;
        self.fresh = false;
    }

    /// The answer being dealt with is done: a hold it made may be lifted from now.
    pub(super) fn clear_held_now(&mut self) {
        self.held_now = false;
    }
}

impl Diagnostics {
    /// A read asked for is no longer waited for (its attempt is spent).
    pub(super) fn drop_read(&mut self) {
        self.reading = None;
    }
}

impl Live {
    /// The journal of the account signed in now, and its life.
    fn journal(&self) -> Option<(JournalAccount, u64)> {
        let saved = self.saved.as_ref()?;
        saved.session.as_ref()?;
        Some((JournalAccount { backend_id: BackendId::from_hex(&saved.backend_id)?, account: saved.account }, saved.life))
    }
}

impl Client {
    /// Whether anything of the sending half may be read or sent now: signed in, the backend
    /// at the active address read this run and the one expected, not a stranger, not too
    /// old, not Offline.
    fn may_send(&self) -> bool {
        let Some(live) = self.live.as_ref() else { return false };
        let Some(saved) = live.saved.as_ref() else { return false };
        live.signed_in()
            && !live.stranger
            && !live.offline
            && live.too_old.as_deref() != Some(saved.nexus.as_str())
            && BackendId::from_hex(&saved.backend_id).is_some_and(|expected| self.seen(&saved.nexus) == Some(expected))
    }

    fn next_read(&mut self) -> u64 {
        self.next_read = self.next_read.wrapping_add(1).max(1);
        self.next_read
    }

    // -----------------------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------------------

    /// Begins a round if one may begin now: the journal's summary is the signed-in
    /// account's, in its life, with events waiting; nothing is being read or sent; nothing
    /// holds it.
    pub(super) fn events_due(&mut self, out: &mut Vec<Do>) {
        if !self.may_send() {
            return;
        }
        let Some(live) = self.live.as_ref() else { return };
        let Some((account, life)) = live.journal() else { return };
        let sending = &live.sending;
        if sending.round.is_some() || sending.reading.is_some() || sending.held.is_some() || live.waiting(|what| matches!(what, Purpose::Events { .. })) {
            return;
        }
        let Some(waiting) = sending.waiting.filter(|waiting| waiting.account == account && waiting.life == life && waiting.pending > 0) else { return };
        self.read_events(account, life, waiting.from_sequence, out);
    }

    fn read_events(&mut self, account: JournalAccount, life: u64, from_sequence: u64, out: &mut Vec<Do>) {
        let read = self.next_read();
        let Some(live) = self.live.as_mut() else { return };
        live.sending.round = Some(from_sequence);
        live.sending.reading = Some(read);
        live.sending.fresh = false;
        out.push(Do::Journal(JournalDo::ReadEvents {
            read,
            account,
            life,
            from_sequence,
            max_events: events::BATCH_EVENTS,
            max_bytes: events::BATCH_BYTES,
        }));
    }

    /// Lifts a hold, if what lifts it has happened: the backend reachable again (not from
    /// the answer that made the hold), or, while it answers, more events waiting than when
    /// it held, past any wait it asked for.
    pub(super) fn lift_hold(&mut self, reached: Option<Reached>, given: &Given) {
        let Some(live) = self.live.as_mut() else { return };
        let Some(held) = live.sending.held else { return };
        let lifted = match reached {
            Some(Reached::Answer) => !live.sending.held_now,
            Some(_) => true,
            None => {
                let journal = live.journal();
                live.reach == Reach::Up
                    && live.sending.waiting.is_some_and(|waiting| Some((waiting.account, waiting.life)) == journal && waiting.pending > held.pending)
                    && held.until_ms.is_none_or(|until| given.now_ms >= until)
            }
        };
        if lifted {
            live.sending.held = None;
        }
    }

    /// Holds sending after a batch of this account and life. The baseline is that scope's
    /// own summary (it is the only one kept); with none, only reachability lifts it.
    fn hold(&mut self, scope: (JournalAccount, u64), seconds: Option<u64>, given: &Given) {
        let Some(live) = self.live.as_mut() else { return };
        let pending = live.sending.waiting.filter(|waiting| (waiting.account, waiting.life) == scope).map_or(u64::MAX, |waiting| waiting.pending);
        let until_ms = seconds.map(|seconds| given.now_ms.saturating_add(seconds.saturating_mul(1000)));
        live.sending.round = None;
        live.sending.held = Some(Held { pending, until_ms });
        live.sending.held_now = true;
    }

    pub(super) fn events_waiting(&mut self, account: JournalAccount, life: u64, pending: u64, from_sequence: u64, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        // Another account's, or another life's: ignored entirely. It sets no baseline and
        // lifts no hold.
        if live.journal() != Some((account, life)) {
            return;
        }
        live.sending.waiting = Some(Waiting { account, life, pending, from_sequence });
        if live.sending.reading.is_some() {
            live.sending.fresh = true;
        }
        self.lift_hold(None, given);
        self.events_due(out);
    }

    /// The journal's reader answered: the events are sent if they are still the signed-in
    /// account's, in its life, and sending is allowed now; none, and the round is over.
    pub(super) fn events_read(&mut self, read: u64, account: JournalAccount, life: u64, found: Vec<Placed>, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        if live.sending.reading != Some(read) {
            return;
        }
        live.sending.reading = None;
        let fresh = std::mem::take(&mut live.sending.fresh);
        let cursor = live.sending.round;
        let ours = live.journal() == Some((account, life));
        if !ours || found.is_empty() || !self.may_send() {
            if let Some(live) = self.live.as_mut() {
                live.sending.round = None;
            }
            // A flush was reported after this read's snapshot: read once more, from the same
            // place. Once: a second empty read with nothing new ends the round.
            if ours && found.is_empty() && fresh && self.may_send()
                && let Some(from) = cursor
            {
                self.read_events(account, life, from, out);
            }
            return;
        }
        let (Some(session), Some(head)) = (self.session(), self.head()) else { return };
        let batch = events::Batch::prefix(head, life, &found, events::BATCH_BYTES);
        let asked = match batch {
            Ok(batch) => match batch.write().ok().and_then(|body| hidden(Ok(body))) {
                Some(body) => self.ask(Purpose::Events { account, batch }, events::SEND_PATH, Some(&session), Some(body), out),
                None => false,
            },
            Err(_) => false,
        };
        if !asked && let Some(live) = self.live.as_mut() {
            live.sending.round = None;
        }
    }

    /// A batch's answer (13.3): each event answered by itself. Whatever is shared with every
    /// call (another backend, the session ended, the data put back, a moved life) is dealt
    /// with first; a life that moved applies none of the outcomes.
    pub(super) fn answered_events(&mut self, account: JournalAccount, batch: events::Batch, status: u16, bytes: &[u8], given: &Given, out: &mut Vec<Do>) {
        let scope = (account, batch.life());
        let before = self.live.as_ref().map(|live| live.reach);
        let shared = outcome(account::read_reply::<events::BatchAnswer>(status, bytes));
        let Some(shared) = self.common(shared, given, out) else {
            if let Some(live) = self.live.as_mut() {
                live.sending.round = None;
            }
            return;
        };
        // The answer itself moved the life on: it is retired (in `absorb`); nothing of it is
        // applied and it makes no hold.
        if self.live.as_ref().and_then(Live::journal) != Some(scope) {
            return;
        }
        match shared {
            Outcome::Done(..) => {}
            Outcome::Refused(why, seen) => {
                // A refusal of the whole call, stamped or not, is the backend answering: it
                // holds as "busy" does, and is not "down".
                if seen.is_none() {
                    if matches!(before, Some(Reach::Down { .. })) {
                        self.reached();
                    } else if let Some(live) = self.live.as_mut() {
                        live.reach = Reach::Up;
                    }
                }
                let seconds = match why {
                    Why::Busy { seconds } => seconds,
                    _ => None,
                };
                return self.hold(scope, seconds, given);
            }
            Outcome::Nothing => return self.hold(scope, None, given),
        }
        let answer = match events::read_reply(status, bytes, &batch) {
            Ok(Reply::Done { result, .. }) => result,
            // The life moved on: what was written under the earlier one is not sent, and the
            // journal is told once the new life is kept (in `absorb`). Nothing is applied.
            Err(events::Wrong::LifeMoved { life }) if life > batch.life() => {
                if let Some(live) = self.live.as_mut() {
                    live.sending.round = None;
                }
                return;
            }
            _ => return self.hold(scope, None, given),
        };
        let mut taken = Vec::new();
        let mut refused = Vec::new();
        for event in batch.events() {
            let id = event.record().id();
            match answer.outcome(id) {
                events::Outcome::Taken => taken.push(id),
                events::Outcome::Refused => refused.push(id),
                events::Outcome::NotDealtWith => {}
            }
        }
        let life = batch.life();
        let progress = !taken.is_empty() || !refused.is_empty();
        if !taken.is_empty() {
            out.push(Do::Journal(JournalDo::Taken { account, life, ids: taken, at_ms: given.now_ms }));
        }
        if !refused.is_empty() {
            out.push(Do::Journal(JournalDo::Refused { account, life, ids: refused }));
        }
        if !progress {
            // Nothing taken and nothing refused: the backend is full (13.4). Wait.
            return self.hold(scope, None, given);
        }
        // More may wait: read on from after what was sent, in this same round.
        let next = batch.events().iter().map(Placed::sequence).max().unwrap_or(0).saturating_add(1);
        if self.may_send() && self.live.as_ref().and_then(Live::journal) == Some((account, life)) {
            self.read_events(account, life, next, out);
        } else if let Some(live) = self.live.as_mut() {
            live.sending.round = None;
        }
    }

    pub(super) fn failed_events(&mut self, scope: (JournalAccount, u64), given: &Given) {
        self.unreached(given);
        self.hold(scope, None, given);
        if let Some(live) = self.live.as_mut() {
            // A failure is not an answer: nothing settles after it to lift this.
            live.sending.held_now = false;
        }
    }

    /// The account's life moved on to `life`: every read, round, hold and batch of an
    /// earlier life is retired at once, so that nothing of it is posted (its batch fails
    /// `waits_for`, its answers and reads are dropped) and the new life's events go at the
    /// next thing that changes.
    pub(super) fn retire_life(&mut self, life: u64) {
        let Some(live) = self.live.as_mut() else { return };
        live.flight.retain(|flight| !matches!(&flight.what, Purpose::Events { batch, .. } if batch.life() < life));
        live.sending.reset();
        if live.sending.waiting.is_some_and(|waiting| waiting.life < life) {
            live.sending.waiting = None;
        }
    }

    // -----------------------------------------------------------------------------------
    // Diagnostics
    // -----------------------------------------------------------------------------------

    /// The effective switch now: the person's choice, else the default this run's `hello`
    /// said at the active address. Not known is off.
    fn diagnostics_on(&self) -> bool {
        let Some(live) = self.live.as_ref() else { return false };
        let Some(saved) = live.saved.as_ref() else { return false };
        self.default_at(&saved.nexus).is_some_and(|default| diag::enabled(live.diagnostics.choice, default))
    }

    /// Once the switch is off, nothing of Diagnostics leaves: a read out is dropped when it
    /// answers, and an upload queued fails `waits_for`. The attempt stays spent.
    pub(super) fn fence_diagnostics(&mut self) {
        if self.diagnostics_on() {
            return;
        }
        if let Some(live) = self.live.as_mut() {
            live.diagnostics.reading = None;
            live.flight.retain(|flight| !matches!(flight.what, Purpose::Diagnostics));
        }
    }

    /// On a tick: an attempt, if C6's schedule says one is due. The schedule is kept before
    /// anything is read or sent (C6: recorded before the I/O).
    pub(super) fn diagnostics_due(&mut self, given: &Given, out: &mut Vec<Do>) {
        if !self.may_send() {
            return;
        }
        let Some(live) = self.live.as_ref() else { return };
        let Some(nexus) = live.saved.as_ref().map(|saved| saved.nexus.clone()) else { return };
        if live.diagnostics.reading.is_some() || live.waiting(|what| matches!(what, Purpose::Diagnostics)) {
            return;
        }
        // The server's default, from this run's `hello` there; not known, nothing is sent.
        let Some(default) = self.default_at(&nexus) else { return };
        let Some(live) = self.live.as_mut() else { return };
        let diagnostics = &mut live.diagnostics;
        let enabled = diag::enabled(diagnostics.choice, default);
        let Some(schedule) = diagnostics.schedule.as_mut() else { return };
        if schedule.take_due(given.now_ms, enabled, diagnostics.in_use).is_none() {
            return;
        }
        out.push(Do::Keep(Keep::DiagSchedule(DiagSchedule::of(schedule))));
        let read = self.next_read();
        if let Some(live) = self.live.as_mut() {
            live.diagnostics.reading = Some(read);
        }
        out.push(Do::Journal(JournalDo::ReadDiagnostics { read }));
    }

    /// The report's text came: one request, if it is a report and sending is still allowed;
    /// otherwise the attempt is spent.
    pub(super) fn diagnostics_read(&mut self, read: u64, text: Option<String>, given: &Given, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        if live.diagnostics.reading != Some(read) {
            return;
        }
        live.diagnostics.reading = None;
        if !self.may_send() || !self.diagnostics_on() {
            return;
        }
        let Some(report) = text.and_then(|text| diag::Report::read(&text).ok()) else { return };
        let (Some(session), Some(head), Some(request_id)) = (self.session(), self.head(), request_id(given)) else { return };
        let Some(body) = diag::Upload::new(head, request_id, &report).ok().and_then(|upload| upload.write().ok()).and_then(|body| hidden(Ok(body))) else { return };
        self.ask(Purpose::Diagnostics, diag::SEND_PATH, Some(&session), Some(body), out);
    }

    /// Diagnostics' answer: what every call shares, and nothing more. Not sent is not tried
    /// again outside the schedule.
    pub(super) fn answered_diagnostics(&mut self, status: u16, bytes: &[u8], given: &Given, out: &mut Vec<Do>) {
        let shared = outcome(account::read_reply::<diag::Uploaded>(status, bytes));
        let _ = self.common(shared, given, out);
    }

    pub(super) fn diagnostics_fault(&mut self, at_ms: u64, out: &mut Vec<Do>) {
        let Some(live) = self.live.as_mut() else { return };
        let Some(schedule) = live.diagnostics.schedule.as_mut() else { return };
        let before = DiagSchedule::of(schedule);
        schedule.note_fault(at_ms);
        let after = DiagSchedule::of(schedule);
        if after != before {
            out.push(Do::Keep(Keep::DiagSchedule(after)));
        }
    }

    pub(super) fn diagnostics_switch(&mut self, choice: Option<bool>) {
        if let Some(live) = self.live.as_mut() {
            live.diagnostics.choice = choice;
        }
        self.fence_diagnostics();
    }

    pub(super) fn in_use(&mut self, on: bool) {
        if let Some(live) = self.live.as_mut() {
            live.diagnostics.in_use = on;
        }
    }

    /// What the backend's `hello` at this origin said Diagnostics start as, this run: `None`
    /// when no `hello` has been read there.
    pub(super) fn default_at(&self, origin: &str) -> Option<Option<bool>> {
        let origin = canonical(origin);
        self.defaults_at.iter().find(|(at, _)| *at == origin).map(|(_, default)| *default)
    }

    /// Records what a `hello` read at this origin said.
    pub(super) fn heard_default(&mut self, origin: &str, default: Option<bool>) {
        let origin = canonical(origin);
        self.defaults_at.retain(|(at, _)| *at != origin);
        self.defaults_at.push((origin, default));
    }
}
