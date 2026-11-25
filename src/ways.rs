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

impl Controller {
    /// What is in the player, or how it sounds, is about to change. Called only once it is
    /// known that something will: a press that changes nothing (Next with nothing to go
    /// to) takes no place and claims nothing. `who` chose it unless a choice is already
    /// under way, whose it then is.
    pub fn way_in(&self, who: Who, what: What) -> WayIn<'_> {
        let (owed, whose) = {
            let mut ways = self.ways.borrow_mut();
            (ways.enter(who, what), ways.whose())
        };
        let way = WayIn { ways: &self.ways };
        self.owe(owed, whose);
        way
    }

    /// The same, for a change that is over as soon as it is made (a seek, a song queued,
    /// the volume).
    pub fn came_in(&self, who: Who, what: What) {
        let _way = self.way_in(who, what);
    }

    /// Somebody else's choice is about to be carried out, by the functions a press runs:
    /// until this is dropped, what they do is that one's choice. `counted`: it has taken
    /// its place in the order of choices already (a command the rule let through), and
    /// what the player does for it is not counted again.
    pub fn for_another(&self, who: Who, counted: bool) -> WayIn<'_> {
        self.ways.borrow_mut().begin(who, counted);
        WayIn { ways: &self.ways }
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
