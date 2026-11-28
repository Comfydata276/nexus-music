//! Every way into this PC's player says, here, who chose it and what it touches.
//!
//! Two rules need to know of everything that happens to the player, and each used to keep
//! its own list: the order of choices that a command from another of the person's devices
//! is judged by (`link`), and whose the queue is (`sync`, by `nexus_core::handover`). There
//! is one list now. A function of the player that a way in reaches makes one statement,
//! [`Controller::way_in`] or [`Controller::came_in`], and what is owed for it is worked out
//! in one place (`nexus_core::ways`): whether the order moves on, and whether the queue is
//! this PC's own from this moment.
//!
//! **Who chose it** is the person at this PC unless something says otherwise. What says
//! otherwise:
//!
//! - a function the app runs by itself says [`Who::Nobody`]: the next song beginning, a
//!   song that failed being passed over, similar songs arriving, the sleep timer's pause,
//!   the sound device going, a queue another device saved being put in the player, the
//!   last run's queue being put back, a followed playlist changing, the stop at sign-out;
//! - whoever carries out somebody else's choice says whose it is first
//!   ([`Controller::for_another`]): a command from another of the person's devices
//!   ([`Who::Elsewhere`]), or what a guest in a jam asked for ([`Who::Guest`]). The
//!   functions it then runs are the ones a press runs, and the choice is still theirs.
//!
//! And **what is under way is one choice**: what a way in does on its way is part of it,
//! counted once and claimed once, and is whoever's the outermost is. The sleep timer
//! pauses by "pressing" Pause, and that is the app's own doing all the same.
//!
//! **What it touches:** [`What::Playing`] is what is in the player or how it is playing
//! (what a saved queue holds), and is the person's claim on the queue when a person chose
//! it; [`What::Manner`] is the volume, mute, repeat and the sleep timer being set, which a
//! command would act on and a saved queue does not hold.
//!
//! Nothing the person does is ever refused, delayed or dropped on account of any of this:
//! these calls keep count, and say whose the queue is.

use std::cell::RefCell;

use nexus_core::ways::{Owed, Ways};
pub use nexus_core::ways::{What, Who};

use crate::controller::Controller;

/// The choices under way in this PC's player. A choice is under way from when it is begun
/// or its first way in is taken until what that gave back is dropped, however the function
/// that holds it ends.
#[derive(Default)]
pub struct Choices(RefCell<Ways>);

/// A way into the player is being taken, or somebody's choice carried out, until this is
/// dropped.
pub struct WayIn<'a> {
    ways: &'a RefCell<Ways>,
}

impl Drop for WayIn<'_> {
    fn drop(&mut self) {
        self.ways.borrow_mut().end();
    }
}

impl Choices {
    /// A way in is taken: what is owed for it, and whose choice it is part of.
    pub fn enter(&self, who: Who, what: What) -> (WayIn<'_>, Owed, Option<Who>) {
        let mut ways = self.0.borrow_mut();
        let owed = ways.enter(who, what);
        (WayIn { ways: &self.0 }, owed, ways.whose())
    }

    /// Somebody else's choice is about to be carried out.
    pub fn begin(&self, who: Who, counted: bool) -> WayIn<'_> {
        self.0.borrow_mut().begin(who, counted);
        WayIn { ways: &self.0 }
    }

    pub(crate) fn whose(&self) -> Option<Who> {
        self.0.borrow().whose()
    }

    /// Whether the choice under way has taken any way in: whether carrying it out has
    /// changed anything.
    pub fn taken(&self) -> bool {
        self.0.borrow().taken()
    }
}

impl Controller {
    /// What is in the player, or how it sounds, is about to change. Called only once it is
    /// known that something will: a press that changes nothing (Next with nothing to go
    /// to) takes no place and claims nothing. `who` chose it unless a choice is already
    /// under way, whose it then is.
    pub fn way_in(&self, who: Who, what: What) -> WayIn<'_> {
        let (way, owed, whose) = self.ways.enter(who, what);
        self.owe(owed, whose);
        way
    }

    /// The same, for a change that is over as soon as it is made (a seek, a song queued,
    /// the volume).
    pub fn came_in(&self, who: Who, what: What) {
        let _way = self.way_in(who, what);
    }

    /// Somebody else's choice is about to be carried out, by the functions a press runs:
    /// until this is dropped, what they do is that one's choice. `counted`: it is counted
    /// by whoever carries it out (a command the rule let through, counted once if the
    /// player changes anything), and what the player does for it is not counted again.
    pub fn for_another(&self, who: Who, counted: bool) -> WayIn<'_> {
        self.ways.begin(who, counted)
    }

    fn owe(&self, owed: Owed, whose: Option<Who>) {
        if owed.count {
            self.link_moved();
        }
        if owed.claim {
            // (Written down only where a run is being watched from outside: see `link::log`.)
            crate::link::log(format!(
                "queue: this PC's own from now, chosen {}",
                match whose {
                    Some(Who::Elsewhere) => "at another of the person's devices",
                    Some(Who::Guest) => "by a guest",
                    _ => "here",
                }
            ));
            self.claim_queue();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: Owed = Owed { count: true, claim: true };
    const COUNT: Owed = Owed { count: true, claim: false };
    const CLAIM: Owed = Owed { count: false, claim: true };
    const NOTHING: Owed = Owed { count: false, claim: false };

    /// What a way in owes and whose it is, without the guard that keeps it under way.
    fn enter(choices: &Choices, who: Who, what: What) -> (Owed, Option<Who>) {
        let (way, owed, whose) = choices.enter(who, what);
        drop(way);
        (owed, whose)
    }

    /// A function of the player that ends early, however it ends, has ended its way in:
    /// the next press is a press of its own.
    #[test]
    fn a_way_in_is_under_way_until_what_it_gave_back_is_dropped() {
        let choices = Choices::default();
        {
            let (_playing, owed, whose) = choices.enter(Who::Here, What::Playing);
            assert_eq!((owed, whose), (BOTH, Some(Who::Here)));
            // What it does on its way is part of it: counted once, claimed once.
            assert_eq!(enter(&choices, Who::Here, What::Playing), (NOTHING, Some(Who::Here)));
            assert_eq!(enter(&choices, Who::Nobody, What::Playing), (NOTHING, Some(Who::Here)));
        }
        // Over: the next is its own, and owes for itself.
        assert_eq!(enter(&choices, Who::Here, What::Playing), (BOTH, Some(Who::Here)));
        assert_eq!(enter(&choices, Who::Here, What::Manner), (COUNT, Some(Who::Here)));
        // A function that returns early (nothing to play) drops its way in as it returns.
        let early = |choices: &Choices| {
            let (_way, owed, _) = choices.enter(Who::Here, What::Playing);
            owed
        };
        assert_eq!((early(&choices), early(&choices)), (BOTH, BOTH));
    }

    /// The sleep timer pauses by running Pause's function; a failed song is passed over by
    /// running Next's. Each is the app's own doing, whatever the function inside says.
    #[test]
    fn what_the_app_does_by_itself_claims_nothing_whatever_it_runs() {
        let choices = Choices::default();
        let (timer, owed, whose) = choices.enter(Who::Nobody, What::Playing);
        assert_eq!((owed, whose), (COUNT, Some(Who::Nobody)));
        assert_eq!(enter(&choices, Who::Here, What::Playing), (NOTHING, Some(Who::Nobody)));
        drop(timer);
        // And a press straight after it is the person's again.
        assert_eq!(enter(&choices, Who::Here, What::Playing), (BOTH, Some(Who::Here)));
    }

    /// A command from another of the person's devices, and what a guest asked for, run the
    /// functions a press runs: the choice is theirs, and whether it changed anything is
    /// whether any of those functions took its way in.
    #[test]
    fn somebody_else_s_choice_is_theirs_and_is_known_to_have_changed_something_or_not() {
        let choices = Choices::default();
        // A command the rule let through: counted by the rule, so not here; the queue is
        // the person's by it.
        let carrying = choices.begin(Who::Elsewhere, true);
        assert!(!choices.taken());
        assert_eq!(enter(&choices, Who::Here, What::Playing), (CLAIM, Some(Who::Elsewhere)));
        assert!(choices.taken());
        drop(carrying);
        assert!(!choices.taken());
        // One that found nothing to do (Play, and it was playing): no way in was taken.
        let carrying = choices.begin(Who::Elsewhere, true);
        drop(carrying);
        assert!(!choices.taken());
        // The volume from another device: counted by the rule, and claims nothing.
        let carrying = choices.begin(Who::Elsewhere, true);
        assert_eq!(enter(&choices, Who::Here, What::Manner), (NOTHING, Some(Who::Elsewhere)));
        assert!(choices.taken());
        drop(carrying);
        // The stop that ends a move of music to the device that asks: nobody's.
        let carrying = choices.begin(Who::Nobody, true);
        assert_eq!(enter(&choices, Who::Here, What::Playing), (NOTHING, Some(Who::Nobody)));
        drop(carrying);
        // What a guest asked for, and a device linked the old way: counted as it is
        // carried out, and the queue is the person's by it. An ask that changes nothing
        // (a search) owes nothing at all.
        for who in [Who::Guest, Who::Elsewhere] {
            let asked = choices.begin(who, false);
            assert_eq!(enter(&choices, Who::Here, What::Playing), (BOTH, Some(who)));
            // (A song queued, and then played because the queue was empty: one choice.)
            assert_eq!(enter(&choices, Who::Here, What::Playing), (NOTHING, Some(who)));
            drop(asked);
            let asked = choices.begin(who, false);
            drop(asked);
        }
        assert_eq!(enter(&choices, Who::Here, What::Playing), (BOTH, Some(Who::Here)));
    }
}
