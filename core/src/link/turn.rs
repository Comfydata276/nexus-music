//! One thing at a time, on the thread the player lives on.
//!
//! The deciding asks its player and has the answer before the call returns: what is
//! sounding, carry out this command, load this queue. That only holds if the deciding and
//! the player live on one thread, and then there is one thing to get right, which is here:
//! **a player tells its listeners of a change before the call that made it returns.** The
//! deciding asks the player to pause; the player tells the app it has paused; the app would
//! tell the deciding "something changed", and the deciding is still inside the call that
//! asked.
//!
//! The rule, for whatever arrives while the deciding is inside a call:
//!
//! - **a change, while the deciding is itself changing the player** (carrying out a
//!   command, loading a queue, stopping the sound): it is that act, heard back. It is
//!   dropped. The deciding has the act's result in its hand already and counts it once;
//!   counted again as something chosen on this device it would move the order of choices a
//!   second time, forget which command made the state, and claim the queue for a press
//!   nobody made;
//! - **anything else that waits for no answer**: kept, and handed over in the order it
//!   arrived when the call returns, before the caller gets its own answer back;
//! - **something whose caller needs its answer as the call returns** (a press: is it this
//!   device's own?): refused. It cannot come from a listener, and whoever sent it is told
//!   so, not handed an answer made for something else.
//!
//! - **the end of the sign-in** (a sign-out, another account): **never refused and never
//!   behind anything.** Whatever is kept is dropped unread, it is the next thing handed
//!   over, and the deciding is told at once ([`Turns::ended`]) so that what is left of the
//!   call in flight asks nothing more of a player that may already be another account's.
//!
//! **And nothing is kept, or worked through, without end.** What is kept for a turn is
//! bounded by how many things ([`MOST_KEPT`]) and by how large they are together
//! ([`MOST_KEPT_BYTES`]); what does not fit is refused, and so is anything larger than
//! that bound by itself, wherever it arrives. A turn hands over what was waiting when it
//! began and a bounded number of things more ([`MOST_MORE`]): a listener that feeds the
//! turn as fast as the turn runs does not keep it running, and neither does an app that
//! says the sign-in is over each time it is told of one. What is left is first in the
//! next turn ([`Turn::left`]), which can be taken with nothing new ([`Turns::carry_on`]).
//!
//! [`Turns`] is that rule, and nothing else: it holds no player and decides nothing. An app
//! (through `super::bind`) enters with each thing that happens ([`Turns::enter`]); if the
//! turn is its own it hands over everything [`Turn::next`] gives, and the deciding marks
//! the stretches in which it is changing the player ([`Turns::changing`]).
//!
//! One thread: this is not shared between threads, and does not need to be. What happens on
//! another thread (a frame arriving) is handed to the player's thread first.

use std::cell::RefCell;
use std::collections::VecDeque;

/// What something that arrives is, to a turn it may arrive inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The player's listeners saying that what it holds, or how it plays, has changed.
    Change,
    /// Anything else that waits for no answer.
    News,
    /// Something whose caller needs its answer as the call returns.
    Question,
    /// The sign-in has ended, or is another's now.
    End,
}

/// The most kept for one turn. (A listener that tells of more than this inside one call is
/// telling of the same thing over and over.)
pub const MOST_KEPT: usize = 256;
/// The most kept for one turn, by size: a megabyte, which is three of the longest frames
/// there can be.
pub const MOST_KEPT_BYTES: usize = 1 << 20;
/// The most a turn hands over beyond what was waiting when something began it; and the
/// most a turn taken with nothing new hands over at all.
pub const MOST_MORE: usize = 64;

struct State<T> {
    inside: bool,
    /// How many acts on the player the deciding is in the middle of.
    changing: u32,
    kept: VecDeque<(T, usize)>,
    bytes: usize,
    /// How many more things the turn that is running may hand over.
    budget: usize,
    /// The sign-in ended while a turn was running, and that turn has not yet been told.
    ended: bool,
}

/// Whose turn it is on the player's thread.
pub struct Turns<T> {
    state: RefCell<State<T>>,
}

/// What came of arriving.
pub enum Entered<'a, T> {
    /// Nothing was under way: the turn is the caller's, with what was left from the turn
    /// before first in it, and then what it brought.
    Mine(Turn<'a, T>),
    /// The deciding is inside a call: this is kept, and handed over when that call returns.
    Kept,
    /// The deciding is changing the player and this is that change, heard back: dropped.
    PartOfIt,
    /// The deciding is inside a call and this cannot wait for it (or more is waiting than
    /// is kept): nothing was done with it.
    Refused,
}

/// A turn: until it is dropped, whatever else arrives is kept, dropped or refused by the rule.
pub struct Turn<'a, T> {
    turns: &'a Turns<T>,
}

/// The deciding is changing the player until this is dropped.
pub struct Changing<'a, T> {
    turns: &'a Turns<T>,
}

impl<T> Default for Turns<T> {
    fn default() -> Turns<T> {
        Turns { state: RefCell::new(State { inside: false, changing: 0, kept: VecDeque::new(), bytes: 0, budget: 0, ended: false }) }
    }
}

impl<T> Turns<T> {
    /// Something has happened. `bytes` is about how large it is, for the bound on what is
    /// kept. (Nothing is held here while the caller acts on the answer: arriving from
    /// inside a turn is exactly what this is for.)
    pub fn enter(&self, what: T, kind: Kind, bytes: usize) -> Entered<'_, T> {
        let mut state = self.state.borrow_mut();
        if kind == Kind::End {
            // Whatever was waiting belonged to the sign-in that is over: dropped unread.
            state.kept.clear();
            state.bytes = 0;
        }
        // (Counted at its size wherever it arrives: what is larger than all that may wait
        // is not taken, even to begin a turn with.)
        if kind != Kind::End && bytes > MOST_KEPT_BYTES {
            return Entered::Refused;
        }
        if !state.inside {
            state.inside = true;
            state.bytes = state.bytes.saturating_add(bytes);
            state.kept.push_back((what, bytes));
            // (What was left waiting is handed over first, whatever its number, so that
            // this reaches what it was entered with: a press is answered in its own call.)
            state.budget = state.kept.len().saturating_add(MOST_MORE);
            return Entered::Mine(Turn { turns: self });
        }
        match kind {
            Kind::End => {
                // It is next, and the turn is told now. (It does not lengthen the turn: if
                // the turn has handed over all it may, the end is first in the next one,
                // with nothing in front of it.)
                state.ended = true;
            }
            Kind::Change if state.changing > 0 => return Entered::PartOfIt,
            Kind::Question => return Entered::Refused,
            _ if state.kept.len() >= MOST_KEPT || state.bytes.saturating_add(bytes) > MOST_KEPT_BYTES => return Entered::Refused,
            _ => {}
        }
        state.bytes = state.bytes.saturating_add(bytes);
        state.kept.push_back((what, bytes));
        Entered::Kept
    }

    /// The deciding is about to change the player itself. A change that arrives before
    /// what this gives back is dropped is that act's own.
    pub fn changing(&self) -> Changing<'_, T> {
        let mut state = self.state.borrow_mut();
        state.changing = state.changing.saturating_add(1);
        Changing { turns: self }
    }

    /// A turn with nothing new, to carry on with what the one before left; none, if a
    /// turn is running.
    pub fn carry_on(&self) -> Option<Turn<'_, T>> {
        let mut state = self.state.borrow_mut();
        if state.inside {
            return None;
        }
        state.inside = true;
        state.budget = MOST_MORE;
        Some(Turn { turns: self })
    }

    /// Drops whatever is kept, unread: for when everything it was kept for has been
    /// replaced.
    pub fn forget(&self) {
        let mut state = self.state.borrow_mut();
        state.kept.clear();
        state.bytes = 0;
    }

    /// Whether the sign-in has ended since the turn that is running was last told
    /// ([`Turn::overtaken`]): asked by the deciding before it asks anything more of the
    /// player in the call that is in flight.
    pub fn ended(&self) -> bool {
        self.state.borrow().ended
    }
}

impl<T> Turn<'_, T> {
    /// The next thing to hand over: what was left from the turn before, what the turn was
    /// entered with, then what arrived while those were being handled, in the order it
    /// arrived, up to the turn's bound. `None` when there is nothing more for this turn:
    /// it is then over, once this is dropped.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<T> {
        let mut state = self.turns.state.borrow_mut();
        if state.budget == 0 {
            return None;
        }
        let (what, bytes) = state.kept.pop_front()?;
        state.budget -= 1;
        state.bytes = state.bytes.saturating_sub(bytes);
        Some(what)
    }

    /// Whether the sign-in ended while the thing last handed over was being handled: what
    /// came of that thing is then nobody's. Asked once for each: asking forgets it.
    pub fn overtaken(&mut self) -> bool {
        std::mem::take(&mut self.turns.state.borrow_mut().ended)
    }

    /// Takes one of the things this turn may still hand over, for something that comes
    /// from elsewhere than what was kept. Says whether there was one to take.
    pub fn spend(&mut self) -> bool {
        let mut state = self.turns.state.borrow_mut();
        let any = state.budget > 0;
        state.budget = state.budget.saturating_sub(1);
        any
    }

    /// How many things are left for the next turn: the caller comes back for them.
    pub fn left(&self) -> usize {
        self.turns.state.borrow().kept.len()
    }
}

impl<T> Drop for Turn<'_, T> {
    /// However the turn ends. (What it left is first in the next.)
    fn drop(&mut self) {
        let mut state = self.turns.state.borrow_mut();
        (state.inside, state.changing, state.ended, state.budget) = (false, 0, false, 0);
    }
}

impl<T> Drop for Changing<'_, T> {
    fn drop(&mut self) {
        let mut state = self.turns.state.borrow_mut();
        state.changing = state.changing.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mine<'a>(entered: Entered<'a, &'static str>) -> Turn<'a, &'static str> {
        match entered {
            Entered::Mine(turn) => turn,
            _ => panic!("the turn was somebody else's"),
        }
    }

    fn outcome(entered: &Entered<&'static str>) -> &'static str {
        match entered {
            Entered::Mine(_) => "mine",
            Entered::Kept => "kept",
            Entered::PartOfIt => "part of it",
            Entered::Refused => "refused",
        }
    }

    #[test]
    fn what_arrives_inside_a_turn_is_handed_over_when_it_returns_in_the_order_it_arrived() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a frame", Kind::News, 1));
        assert_eq!(turn.next(), Some("a frame"));
        // While it is handled the player's listeners tell of two things, and a third
        // arrives while the first of those is being handed over.
        assert_eq!(outcome(&turns.enter("the next song began", Kind::Change, 1)), "kept");
        assert_eq!(outcome(&turns.enter("the library changed", Kind::News, 1)), "kept");
        assert_eq!(turn.next(), Some("the next song began"));
        assert_eq!(outcome(&turns.enter("a look-up ended", Kind::News, 1)), "kept");
        assert_eq!((turn.next(), turn.next(), turn.next()), (Some("the library changed"), Some("a look-up ended"), None));
        drop(turn);
        // The turn is over: the next thing to happen has a turn of its own.
        let mut turn = mine(turns.enter("a tick", Kind::News, 1));
        assert_eq!((turn.next(), turn.next()), (Some("a tick"), None));
    }

    #[test]
    fn a_change_heard_while_the_deciding_changes_the_player_is_that_act_and_is_dropped() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a command", Kind::News, 1));
        assert_eq!(turn.next(), Some("a command"));
        {
            // The deciding asks the player to pause, and the player tells its listeners so
            // before that call returns.
            let _carrying = turns.changing();
            assert_eq!(outcome(&turns.enter("it paused", Kind::Change, 1)), "part of it");
            // (One act inside another, a queue loaded and then played, is still the act.)
            {
                let _loading = turns.changing();
                assert_eq!(outcome(&turns.enter("its queue changed", Kind::Change, 1)), "part of it");
            }
            assert_eq!(outcome(&turns.enter("it paused, said again", Kind::Change, 1)), "part of it");
            // What is not a change is not the act's, whenever it arrives.
            assert_eq!(outcome(&turns.enter("the library changed", Kind::News, 1)), "kept");
        }
        // The act is done. A change heard now was made by something else (the sound device
        // went as the frame was being answered): it is this device's own, and is kept.
        assert_eq!(outcome(&turns.enter("the sound device went", Kind::Change, 1)), "kept");
        assert_eq!((turn.next(), turn.next(), turn.next()), (Some("the library changed"), Some("the sound device went"), None));
    }

    #[test]
    fn what_needs_its_answer_at_once_is_refused_inside_a_turn_and_nothing_is_kept_without_end() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a tick", Kind::News, 1));
        assert_eq!(outcome(&turns.enter("a press", Kind::Question, 1)), "refused");
        for _ in 0..MOST_KEPT - 1 {
            assert_eq!(outcome(&turns.enter("a change", Kind::Change, 1)), "kept");
        }
        assert_eq!(outcome(&turns.enter("one more", Kind::Change, 1)), "refused");
        assert_eq!(outcome(&turns.enter("and news", Kind::News, 1)), "refused");
        // (The tick itself, and as much of what was kept behind it as one turn hands over.)
        let mut handed = 0;
        while turn.next().is_some() {
            handed += 1;
        }
        assert_eq!((handed, turn.left()), (1 + MOST_MORE, MOST_KEPT - 1 - MOST_MORE));
        drop(turn);
        // Outside a turn a press is answered like anything else: in the turn it begins,
        // after what was left waiting, however much that is.
        let mut turn = mine(turns.enter("a press", Kind::Question, 1));
        let mut last = None;
        while let Some(what) = turn.next() {
            last = Some(what);
        }
        assert_eq!((last, turn.left()), (Some("a press"), 0));
    }

    /// A turn that ends early (whoever held it stopped) is over all the same: nothing is
    /// left thinking the deciding is inside a call, or changing the player, for ever.
    #[test]
    fn a_turn_that_ends_early_is_over_and_what_it_left_is_first_in_the_next() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a frame", Kind::News, 1));
        assert_eq!(turn.next(), Some("a frame"));
        let changing = turns.changing();
        assert_eq!(outcome(&turns.enter("the library changed", Kind::News, 1)), "kept");
        // (The act's guard is forgotten, as when what held it stops half-way.)
        std::mem::forget(changing);
        drop(turn);
        let mut turn = mine(turns.enter("a tick", Kind::News, 1));
        assert_eq!((turn.next(), turn.next(), turn.next()), (Some("the library changed"), Some("a tick"), None));
        // And a change heard in this turn is not taken for an act that is long over.
        assert_eq!(outcome(&turns.enter("the next song began", Kind::Change, 1)), "kept");
    }

    /// What is kept is bounded by its size too: a megabyte of frames waiting on one call
    /// is not held while more arrive.
    #[test]
    fn what_is_kept_for_a_turn_is_bounded_by_its_size() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a hand-over of frames", Kind::News, MOST_KEPT_BYTES));
        assert_eq!(turn.next(), Some("a hand-over of frames"));
        assert_eq!(outcome(&turns.enter("half of what may wait", Kind::News, MOST_KEPT_BYTES / 2)), "kept");
        assert_eq!(outcome(&turns.enter("the other half", Kind::News, MOST_KEPT_BYTES / 2)), "kept");
        assert_eq!(outcome(&turns.enter("one more character", Kind::Change, 1)), "refused");
        // As it is handed over, there is room again.
        assert_eq!(turn.next(), Some("half of what may wait"));
        assert_eq!(outcome(&turns.enter("a change", Kind::Change, 1)), "kept");
        assert_eq!((turn.next(), turn.next(), turn.next()), (Some("the other half"), Some("a change"), None));
    }

    /// A listener that is told of something each time the deciding handles something (the
    /// app records a claim, and its own bookkeeping tells of a change) would keep one turn
    /// running for ever. A turn hands over what was waiting and a bounded number more.
    #[test]
    fn a_turn_ends_though_each_thing_it_hands_over_brings_another() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a change", Kind::Change, 1));
        let mut handed = 0;
        while turn.next().is_some() {
            handed += 1;
            assert_eq!(outcome(&turns.enter("and another", Kind::Change, 1)), "kept");
        }
        assert_eq!((handed, turn.left()), (1 + MOST_MORE, 1));
        drop(turn);
        // What was left is first in the next turn, which hands over all that was waiting
        // when it began, whatever their number.
        let mut turn = mine(turns.enter("a tick", Kind::News, 1));
        assert_eq!((turn.next(), turn.next(), turn.next(), turn.left()), (Some("and another"), Some("a tick"), None, 0));
        drop(turn);
        let turn = mine(turns.enter("a frame", Kind::News, 1));
        for _ in 0..MOST_KEPT - 1 {
            turns.enter("a change", Kind::Change, 1);
        }
        drop(turn);
        let mut turn = mine(turns.enter("a tick", Kind::News, 1));
        let mut handed = 0;
        while turn.next().is_some() {
            handed += 1;
        }
        assert_eq!(handed, MOST_KEPT + 1);
    }

    /// The sign-in ends: that is never refused and never waits behind what was kept, which
    /// belonged to the sign-in that is over and is dropped unread.
    #[test]
    fn the_end_of_the_sign_in_is_never_refused_and_what_was_kept_is_dropped_unread() {
        let turns = Turns::default();
        // Inside a turn, with as much waiting as is kept: the end is taken all the same,
        // the turn is told at once, and it is the next and only thing handed over.
        let mut turn = mine(turns.enter("a hand-over of frames", Kind::News, 1));
        assert_eq!(turn.next(), Some("a hand-over of frames"));
        for _ in 0..MOST_KEPT {
            turns.enter("a frame of the old sign-in", Kind::News, 1);
        }
        assert_eq!(outcome(&turns.enter("one more", Kind::News, 1)), "refused");
        assert!(!turns.ended());
        assert_eq!(outcome(&turns.enter("signed out", Kind::End, 1)), "kept");
        assert!(turns.ended());
        // (What came of the thing in flight is nobody's: asked once.)
        assert!(turn.overtaken() && !turn.overtaken() && !turns.ended());
        assert_eq!((turn.next(), turn.next()), (Some("signed out"), None));
        drop(turn);
        // Outside a turn, with something left from the turn before: dropped, and the end
        // is all the turn holds.
        let mut turn = mine(turns.enter("a change", Kind::Change, 1));
        while turn.next().is_some() {
            turns.enter("and another", Kind::Change, 1);
        }
        assert_eq!(turn.left(), 1);
        drop(turn);
        let mut turn = mine(turns.enter("another account", Kind::End, 1));
        assert_eq!((turn.overtaken(), turn.next(), turn.next()), (false, Some("another account"), None));
    }

    /// A turn that has handed over all it may does not go on for the end of a sign-in
    /// either (an app that says "another account" each time it is told of one would keep
    /// it running for ever). The end waits, alone and first, for the next turn: nothing
    /// that arrives before then can get in front of it.
    #[test]
    fn the_end_of_the_sign_in_does_not_lengthen_a_turn_and_is_first_in_the_next() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a change", Kind::Change, 1));
        while turn.next().is_some() {
            turns.enter("and another", Kind::Change, 1);
        }
        for _ in 0..3 {
            assert_eq!(outcome(&turns.enter("signed out", Kind::End, 1)), "kept");
            // (The turn is told, and has no more to hand over.)
            assert!(turns.ended() && turn.overtaken());
            assert_eq!((turn.next(), turn.left()), (None, 1));
        }
        drop(turn);
        let mut turn = mine(turns.enter("a press", Kind::Question, 1));
        assert_eq!((turn.next(), turn.next(), turn.next()), (Some("signed out"), Some("a press"), None));
    }

    /// A turn can be taken with nothing new, to carry on with what the one before left:
    /// it hands over a bounded number of things, and none if a turn is already running.
    #[test]
    fn a_turn_with_nothing_new_carries_on_with_what_was_left() {
        let turns = Turns::default();
        let turn = mine(turns.enter("a frame", Kind::News, 1));
        for _ in 0..MOST_MORE + 10 {
            turns.enter("a change", Kind::Change, 1);
        }
        assert!(turns.carry_on().is_none(), "a turn is running");
        drop(turn);
        let mut turn = turns.carry_on().expect("nothing is running");
        let mut handed = 0;
        while turn.next().is_some() {
            handed += 1;
        }
        // (The frame the first turn never handed over is among them.)
        assert_eq!((handed, turn.left()), (MOST_MORE, 11));
        // What a turn may still hand over can be spent on things that come from elsewhere.
        drop(turn);
        let mut turn = turns.carry_on().expect("nothing is running");
        let spent = (0..MOST_MORE + 5).filter(|_| turn.spend()).count();
        assert_eq!((spent, turn.next()), (MOST_MORE, None));
    }

    /// Whatever begins a turn is counted at its size too: something larger than all that
    /// may wait is not taken at all. (The end of a sign-in is, whatever its size.)
    #[test]
    fn what_begins_a_turn_is_held_to_the_bound_on_size_as_well() {
        let turns = Turns::default();
        assert_eq!(outcome(&turns.enter("more than may wait", Kind::News, MOST_KEPT_BYTES + 1)), "refused");
        // (Nothing was begun by it.)
        let mut turn = mine(turns.enter("as much as may wait", Kind::News, MOST_KEPT_BYTES));
        assert_eq!((turn.next(), turn.next()), (Some("as much as may wait"), None));
        drop(turn);
        let mut turn = mine(turns.enter("signed out", Kind::End, MOST_KEPT_BYTES + 1));
        assert_eq!(turn.next(), Some("signed out"));
    }
}
