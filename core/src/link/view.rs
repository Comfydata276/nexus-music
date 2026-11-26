//! Steering another device: what the player shows of it, and what a press means.
//!
//! The device being steered says what it is doing in a `full` state. The steering device
//! shows that in its own player and queue panel, and each press there is asked of the
//! other device as a command. Two things are decided here, the same on every app:
//!
//! - **What a press means,** given the state that is on the screen ([`Press::command`]).
//!   No command is a toggle, so a press that toggles is turned into the state it wants: the
//!   play button on a device shown playing is `pause`; the shuffle button on one shown
//!   unshuffled is `shuffle` on. An entry of the queue is named by its place and by the
//!   song shown there.
//! - **What is shown straight away** ([`View::at_once`]). The player shows what was asked
//!   for at once; the other device's own word on it follows a moment later and replaces it.

use super::command::Command;
use super::message::Full;

/// What the device being steered was last heard to be doing, with what has been asked of it
/// since shown as done until its own word arrives.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct View {
    pub full: Full,
    pub playing: bool,
    /// When that was heard (or last settled by a press), on this device's clock.
    pub heard_ms: u64,
}

/// Something pressed in the player or the queue panel while another device is shown there.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Press {
    /// The play button, or the key that does the same.
    Toggle,
    /// Play, or pause, said outright (the media keys; a sleep timer).
    Play(bool),
    Next,
    Previous,
    Seek(f64),
    Volume(f64),
    /// The entry this many places after the current song: take it out; play it now.
    Remove(usize),
    Jump(usize),
    /// Move an entry from one place to another, into "Next in queue" or out of it.
    Move(usize, usize, bool),
    /// Empty "Next in queue", or else the rest.
    Clear(bool),
    Shuffle,
    Repeat,
}

impl Press {
    /// The command this press asks for, against what `view` shows. `None` for a press that
    /// asks for what is already so, or names an entry that is not on the screen.
    pub fn command(self, view: &View) -> Option<Command> {
        let entry = |at: usize| view.full.queue.get(at).map(|entry| entry.id.clone());
        Some(match self {
            Press::Toggle if view.playing => Command::Pause,
            Press::Toggle => Command::Play,
            Press::Play(on) if on == view.playing => return None,
            Press::Play(true) => Command::Play,
            Press::Play(false) => Command::Pause,
            Press::Next => Command::Next,
            Press::Previous => Command::Previous,
            Press::Seek(to) => Command::Seek { to: if to.is_finite() { to.max(0.0) } else { 0.0 } },
            Press::Volume(to) => Command::Volume { to: if to.is_finite() { to.clamp(0.0, 1.0) } else { 1.0 } },
            Press::Remove(at) => Command::Remove { at: at as u64, id: entry(at)? },
            Press::Jump(at) => Command::Jump { at: at as u64, id: entry(at)? },
            Press::Move(from, to, queued) => Command::Move { from: from as u64, to: to as u64, queued, id: entry(from)? },
            Press::Clear(queued) => Command::Clear { queued },
            Press::Shuffle => Command::Shuffle { on: !view.full.shuffle },
            Press::Repeat => Command::Repeat { mode: (view.full.repeat + 1) % 3 },
        })
    }
}

impl View {
    /// Where the song has got to: where it was when last heard, plus the time since if it
    /// is playing, and never past its end.
    pub fn position(&self, now_ms: u64) -> f64 {
        let duration = self.full.now.as_ref().map_or(0.0, |song| f64::from(song.duration));
        let since = if self.playing { now_ms.saturating_sub(self.heard_ms) as f64 / 1000.0 } else { 0.0 };
        (self.full.position + since).clamp(0.0, duration.max(0.0))
    }

    /// Shows a command as done, as far as the player and the queue panel can show it
    /// without the other device's word.
    pub fn at_once(&mut self, command: &Command, now_ms: u64) {
        let (position, playing) = (self.position(now_ms), self.playing);
        let settle = |view: &mut View, playing: bool, at: f64| {
            view.playing = playing;
            view.full.position = at;
            view.heard_ms = now_ms;
        };
        match command {
            Command::Play => settle(self, true, position),
            Command::Pause => settle(self, false, position),
            Command::Seek { to } => settle(self, playing, *to),
            Command::Volume { to } => self.full.volume = *to,
            Command::Shuffle { on } => self.full.shuffle = *on,
            Command::Repeat { mode } => self.full.repeat = *mode,
            Command::Remove { at, .. } => {
                if (*at as usize) < self.full.queue.len() {
                    self.full.queue.remove(*at as usize);
                }
            }
            Command::Move { from, to, queued, .. } => {
                let (from, to) = (*from as usize, *to as usize);
                if from < self.full.queue.len() && to < self.full.queue.len() {
                    let mut moved = self.full.queue.remove(from);
                    moved.queued = *queued;
                    self.full.queue.insert(to, moved);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::message::Entry;
    use super::*;

    fn view() -> View {
        let entry = |n: usize| Entry { id: format!("s{n}"), title: format!("Song {n}"), duration: 200, queued: n == 1, ..Default::default() };
        View { full: Full::fitted(40.0, Some(entry(0)), (1..5).map(entry), 4, "Morning mix", false, 0, 0.5), playing: true, heard_ms: 10_000 }
    }

    #[test]
    fn a_press_asks_for_the_state_it_wants_against_what_is_shown() {
        let shown = view();
        let paused = View { playing: false, ..view() };
        assert_eq!((Press::Toggle.command(&shown), Press::Toggle.command(&paused)), (Some(Command::Pause), Some(Command::Play)));
        // Asked for what it is already doing: nothing to send.
        assert_eq!((Press::Play(true).command(&shown), Press::Play(false).command(&paused)), (None, None));
        assert_eq!((Press::Play(false).command(&shown), Press::Play(true).command(&paused)), (Some(Command::Pause), Some(Command::Play)));
        assert_eq!(Press::Shuffle.command(&shown), Some(Command::Shuffle { on: true }));
        assert_eq!(Press::Repeat.command(&shown), Some(Command::Repeat { mode: 1 }));
        let mut twice = view();
        twice.full.repeat = 2;
        twice.full.shuffle = true;
        assert_eq!((Press::Repeat.command(&twice), Press::Shuffle.command(&twice)), (Some(Command::Repeat { mode: 0 }), Some(Command::Shuffle { on: false })));
        // An entry is named by its place and by the song shown there.
        assert_eq!(Press::Jump(2).command(&shown), Some(Command::Jump { at: 2, id: "s3".into() }));
        assert_eq!(Press::Remove(0).command(&shown), Some(Command::Remove { at: 0, id: "s1".into() }));
        assert_eq!(Press::Move(3, 0, true).command(&shown), Some(Command::Move { from: 3, to: 0, queued: true, id: "s4".into() }));
        assert_eq!((Press::Jump(9).command(&shown), Press::Remove(4).command(&shown), Press::Move(7, 0, false).command(&shown)), (None, None, None));
        // What a slider gives is kept within what a command may say.
        assert_eq!(Press::Seek(-3.0).command(&shown), Some(Command::Seek { to: 0.0 }));
        assert_eq!(Press::Volume(1.7).command(&shown), Some(Command::Volume { to: 1.0 }));
        assert_eq!(Press::Volume(f64::NAN).command(&shown), Some(Command::Volume { to: 1.0 }));
        assert_eq!((Press::Next.command(&shown), Press::Previous.command(&shown), Press::Clear(true).command(&shown)), (Some(Command::Next), Some(Command::Previous), Some(Command::Clear { queued: true })));
    }

    #[test]
    fn the_player_shows_what_was_asked_of_the_other_device_straight_away() {
        // The progress bar moves on by itself while the other device plays, and not past the end.
        let mut shown = view();
        assert_eq!((shown.position(10_000), shown.position(15_000), shown.position(999_000)), (40.0, 45.0, 200.0));
        // Pause holds it where it had got to.
        shown.at_once(&Command::Pause, 15_000);
        assert_eq!((shown.playing, shown.position(60_000)), (false, 45.0));
        shown.at_once(&Command::Seek { to: 120.0 }, 61_000);
        assert_eq!((shown.playing, shown.position(70_000)), (false, 120.0));
        shown.at_once(&Command::Play, 71_000);
        assert_eq!(shown.position(73_000), 122.0);
        shown.at_once(&Command::Volume { to: 0.2 }, 0);
        shown.at_once(&Command::Shuffle { on: true }, 0);
        shown.at_once(&Command::Repeat { mode: 2 }, 0);
        assert_eq!((shown.full.volume, shown.full.shuffle, shown.full.repeat), (0.2, true, 2));
        // The queue's own edits: an entry taken out, and one moved into "Next in queue".
        shown.at_once(&Command::Remove { at: 1, id: "s2".into() }, 0);
        assert_eq!(shown.full.queue.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(), ["s1", "s3", "s4"]);
        shown.at_once(&Command::Move { from: 2, to: 0, queued: true, id: "s4".into() }, 0);
        assert_eq!(shown.full.queue.iter().map(|entry| (entry.id.as_str(), entry.queued)).collect::<Vec<_>>(), [("s4", true), ("s1", true), ("s3", false)]);
        // One that names a place that isn't there changes nothing.
        shown.at_once(&Command::Remove { at: 9, id: "s9".into() }, 0);
        assert_eq!(shown.full.queue.len(), 3);
    }
}
