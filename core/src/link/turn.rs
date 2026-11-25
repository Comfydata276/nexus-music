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
}

/// The most kept for one turn. (A listener that tells of more than this inside one call is
/// telling of the same thing over and over.)
pub const MOST_KEPT: usize = 256;

struct State<T> {
    inside: bool,
    /// How many acts on the player the deciding is in the middle of.
    changing: u32,
    kept: VecDeque<T>,
}

/// Whose turn it is on the player's thread.
pub struct Turns<T> {
    state: RefCell<State<T>>,
}

/// What came of arriving.
pub enum Entered<'a, T> {
    /// Nothing was under way: the turn is the caller's, with what it brought first in it.
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
        Turns { state: RefCell::new(State { inside: false, changing: 0, kept: VecDeque::new() }) }
    }
}

impl<T> Turns<T> {
    /// Something has happened. (Nothing is held here while the caller acts on the answer:
    /// arriving from inside a turn is exactly what this is for.)
    pub fn enter(&self, what: T, kind: Kind) -> Entered<'_, T> {
        let mut state = self.state.borrow_mut();
        if !state.inside {
            state.inside = true;
            state.kept.push_back(what);
            return Entered::Mine(Turn { turns: self });
        }
        match kind {
            Kind::Change if state.changing > 0 => Entered::PartOfIt,
            Kind::Question => Entered::Refused,
            _ if state.kept.len() >= MOST_KEPT => Entered::Refused,
            _ => {
                state.kept.push_back(what);
                Entered::Kept
            }
        }
    }

    /// The deciding is about to change the player itself. A change that arrives before
    /// what this gives back is dropped is that act's own.
    pub fn changing(&self) -> Changing<'_, T> {
        let mut state = self.state.borrow_mut();
        state.changing = state.changing.saturating_add(1);
        Changing { turns: self }
    }
}

impl<T> Turn<'_, T> {
    /// The next thing to hand over: what the turn was entered with, then whatever arrived
    /// while that was being handled, in the order it arrived, and whatever arrives while
    /// *those* are. `None` when there is nothing more: the turn is then over, once this is
    /// dropped.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<T> {
        self.turns.state.borrow_mut().kept.pop_front()
    }
}

impl<T> Drop for Turn<'_, T> {
    /// However the turn ends. (If it ended early, what was kept is first in the next.)
    fn drop(&mut self) {
        let mut state = self.turns.state.borrow_mut();
        (state.inside, state.changing) = (false, 0);
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
        let mut turn = mine(turns.enter("a frame", Kind::News));
        assert_eq!(turn.next(), Some("a frame"));
        // While it is handled the player's listeners tell of two things, and a third
        // arrives while the first of those is being handed over.
        assert_eq!(outcome(&turns.enter("the next song began", Kind::Change)), "kept");
        assert_eq!(outcome(&turns.enter("the library changed", Kind::News)), "kept");
        assert_eq!(turn.next(), Some("the next song began"));
        assert_eq!(outcome(&turns.enter("a look-up ended", Kind::News)), "kept");
        assert_eq!((turn.next(), turn.next(), turn.next()), (Some("the library changed"), Some("a look-up ended"), None));
        drop(turn);
        // The turn is over: the next thing to happen has a turn of its own.
        let mut turn = mine(turns.enter("a tick", Kind::News));
        assert_eq!((turn.next(), turn.next()), (Some("a tick"), None));
    }

    #[test]
    fn a_change_heard_while_the_deciding_changes_the_player_is_that_act_and_is_dropped() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a command", Kind::News));
        assert_eq!(turn.next(), Some("a command"));
        {
            // The deciding asks the player to pause, and the player tells its listeners so
            // before that call returns.
            let _carrying = turns.changing();
            assert_eq!(outcome(&turns.enter("it paused", Kind::Change)), "part of it");
            // (One act inside another, a queue loaded and then played, is still the act.)
            {
                let _loading = turns.changing();
                assert_eq!(outcome(&turns.enter("its queue changed", Kind::Change)), "part of it");
            }
            assert_eq!(outcome(&turns.enter("it paused, said again", Kind::Change)), "part of it");
            // What is not a change is not the act's, whenever it arrives.
            assert_eq!(outcome(&turns.enter("the library changed", Kind::News)), "kept");
        }
        // The act is done. A change heard now was made by something else (the sound device
        // went as the frame was being answered): it is this device's own, and is kept.
        assert_eq!(outcome(&turns.enter("the sound device went", Kind::Change)), "kept");
        assert_eq!((turn.next(), turn.next(), turn.next()), (Some("the library changed"), Some("the sound device went"), None));
    }

    #[test]
    fn what_needs_its_answer_at_once_is_refused_inside_a_turn_and_nothing_is_kept_without_end() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a tick", Kind::News));
        assert_eq!(outcome(&turns.enter("a press", Kind::Question)), "refused");
        for _ in 0..MOST_KEPT - 1 {
            assert_eq!(outcome(&turns.enter("a change", Kind::Change)), "kept");
        }
        assert_eq!(outcome(&turns.enter("one more", Kind::Change)), "refused");
        assert_eq!(outcome(&turns.enter("and news", Kind::News)), "refused");
        // (The tick itself and what was kept behind it.)
        let mut handed = 0;
        while turn.next().is_some() {
            handed += 1;
        }
        assert_eq!(handed, MOST_KEPT);
        drop(turn);
        // Outside a turn a press is answered like anything else.
        let mut turn = mine(turns.enter("a press", Kind::Question));
        assert_eq!(turn.next(), Some("a press"));
    }

    /// A turn that ends early (whoever held it stopped) is over all the same: nothing is
    /// left thinking the deciding is inside a call, or changing the player, for ever.
    #[test]
    fn a_turn_that_ends_early_is_over_and_what_it_left_is_first_in_the_next() {
        let turns = Turns::default();
        let mut turn = mine(turns.enter("a frame", Kind::News));
        assert_eq!(turn.next(), Some("a frame"));
        let changing = turns.changing();
        assert_eq!(outcome(&turns.enter("the library changed", Kind::News)), "kept");
        // (The act's guard is forgotten, as when what held it stops half-way.)
        std::mem::forget(changing);
        drop(turn);
        let mut turn = mine(turns.enter("a tick", Kind::News));
        assert_eq!((turn.next(), turn.next(), turn.next()), (Some("the library changed"), Some("a tick"), None));
        // And a change heard in this turn is not taken for an act that is long over.
        assert_eq!(outcome(&turns.enter("the next song began", Kind::Change)), "kept");
    }
}
