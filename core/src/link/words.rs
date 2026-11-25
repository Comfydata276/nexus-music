//! What the person is told about their devices: every sentence, in one place, so that the
//! PC and the phone say the same thing of the same event.
//!
//! The only difference between them is what a device calls itself ([`Here`]).

use super::message::{Tell, Why};
use super::moving::Gave;
use super::secret::Stop;
use super::steer::{Outcome, Wait};

/// What kind of device is speaking: what it calls itself in a sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Here {
    Pc,
    Phone,
}

impl Here {
    /// At the start of a sentence.
    pub fn this(self) -> &'static str {
        match self {
            Here::Pc => "This PC",
            Here::Phone => "This phone",
        }
    }

    /// Inside one.
    pub fn this_lower(self) -> &'static str {
        match self {
            Here::Pc => "this PC",
            Here::Phone => "this phone",
        }
    }
}

pub const RELAY_NEEDS_UPDATING: &str = "This relay needs updating before your devices can be linked.";
pub const HOST_LINK_WRONG: &str = "The host link isn't right for this relay.";
pub const SERVER_SILENT: &str = "Your devices can't be seen: the music server hasn't answered for ten minutes.";
pub const RESETTING: &str = "Resetting linking…";
pub const WAS_PUBLIC: &str = "Your devices' link could be seen by others on the music server. It has been made private and reset.";
pub const NEEDS_RESETTING: &str = "Your devices' link on the music server needs resetting, and couldn't be. It will be tried again.";
pub const RESET_HALF_DONE: &str = "Linking was reset, but the music server stopped answering before that could be confirmed. It will be asked again.";
pub const RESET_NOT_DONE: &str = "Linking couldn't be reset: the music server didn't answer. Try again.";
pub const RESET_DONE: &str = "Linking was reset. Your other devices rejoin by themselves within two minutes.";
pub const NOT_RADIO: &str = "Radio stations can't be added from here";
pub const IN_A_JAM: &str = "Leave the jam before moving to another device";
/// A device seen before that has no place in the room: while this one has, and while it
/// is still finding its own.
pub const NOT_HERE: &str = "Not here just now";
pub const LOOKING_FOR_IT: &str = "Looking for it…";

/// Why a look came to nothing.
pub fn stopped(stop: Stop, here: Here) -> String {
    match stop {
        Stop::NotStated => "This music server doesn't say whose a playlist is or whether it is private, so devices can't be linked through it.".to_string(),
        Stop::Later => format!("Update Nexus Music on {} to link it with your other devices.", here.this_lower()),
        Stop::Mismatch => "The music server's answer about your devices' link didn't add up. It will be asked again.".to_string(),
        Stop::HasSongs => "You have a playlist named “Nexus Music link” with songs in it. Rename or delete it, and your devices can be linked.".to_string(),
        Stop::Taken => "You have a playlist named “Nexus Music link”. Rename or delete it, and your devices can be linked.".to_string(),
    }
}

pub fn no_random(here: Here) -> String {
    format!("{} couldn't make random numbers, so linking is off.", here.this())
}

/// What a device is doing, in a few words, for the list. `age_s` is how old the state is.
pub fn about(tell: &Tell, age_s: u64) -> String {
    let mut about = match (tell.steering.is_empty(), tell.busy, tell.title.is_empty(), tell.playing) {
        (false, ..) => format!("Steering {}", tell.steering),
        (_, true, _, _) => "In the middle of something…".to_string(),
        (_, _, true, _) => "Nothing playing".to_string(),
        (_, _, _, true) => format!("Playing {}", tell.title),
        (_, _, _, false) => format!("Paused on {}", tell.title),
    };
    // (Older than a look and its answer: it says how old it is.)
    if age_s >= 50 {
        about.push_str(&format!(" (as of {age_s} seconds ago)"));
    }
    about
}

/// Why something could not be asked of a device just now.
pub fn not_sent(wait: Wait, name: &str) -> String {
    match wait {
        Wait::Look => format!("Looking at {name} first. Press again in a moment."),
        Wait::Busy => format!("{name} is in the middle of something. Press again in a moment."),
        Wait::Unanswered => format!("Still waiting for {name} to answer."),
    }
}

pub fn asked_to_pause(name: &str) -> String {
    format!("Asked {name} to pause…")
}

pub fn paused(name: &str) -> String {
    format!("Paused {name}.")
}

pub fn added(count: usize, name: &str) -> String {
    if count == 1 { format!("Added to the queue on {name}") } else { format!("Added {count} songs to the queue on {name}") }
}

/// A queue played on the device being steered, of which some songs did not fit.
pub fn sent_as_many_as_fit(name: &str, left: usize) -> String {
    format!("{name} was sent as many songs as fit; {left} were left out.")
}

pub fn cannot_play_there(name: &str) -> String {
    format!("That can't be played on {name} from here")
}

pub fn moving(name: &str) -> String {
    format!("Moving what's playing to {name}…")
}

/// What the person is told when something asked of another device was not done, or what
/// became of it is not known. Nothing, when it was done: the screen shows that.
pub fn outcome(outcome: Outcome, name: &str) -> String {
    match outcome {
        Outcome::Done => String::new(),
        Outcome::Refused(Why::Changed) => format!("{name} changed before that reached it. Nothing was done."),
        Outcome::Refused(Why::Busy) => format!("{name} was in the middle of something. Nothing was done."),
        Outcome::Refused(Why::Late) => format!("That took too long to reach {name}. Nothing was done."),
        Outcome::Refused(Why::Invalid | Why::Cannot) => format!("{name} couldn't do that."),

        // (Never said as a refusal; and if it were, there would be nothing to tell.)
        Outcome::Refused(Why::Nothing) => String::new(),
        Outcome::NotReached => format!("That hadn't reached {name} when checked."),
        // (An answer of "not known" is `Outcome::Unknown`, and is never said as a refusal.)
        Outcome::Unknown | Outcome::Refused(Why::Unknown) => format!("{name} changed, or didn't answer. Check what is playing before you press again."),
    }
}

/// What the person is told when music was to move to another device. `sent` songs went,
/// and `left` did not fit.
pub fn gave(gave: Gave, name: &str, left: usize, sent: usize, here: Here) -> String {
    let (this, this_lower) = (here.this(), here.this_lower());
    match gave {
        Gave::Moved if left > 0 => format!("Playing on {name}. The next {sent} of {} songs were moved.", sent + left),
        Gave::Moved => format!("Playing on {name}."),
        Gave::StartedAndThisChanged => format!("{name} has started. {this} was given something else to do meanwhile, so it was left as it is: both may be playing."),
        Gave::StartedLate => format!("{name} has started, too late for {this_lower} to stop by itself. Both are playing."),
        Gave::Stayed(Why::Busy) => format!("{name} was in the middle of something, so nothing was moved. Still playing here."),
        // (Before the queue reached it, or after it took the queue and before it sounded.)
        Gave::Stayed(Why::Changed) => format!("{name} was given something else to do before it started. Still playing here."),
        Gave::Stayed(Why::Late) => format!("That took too long to reach {name}, so nothing was moved. Still playing here."),
        // (A queue to take over is never "nothing to do": if that is ever said of one, it was not taken.)
        Gave::Stayed(Why::Cannot | Why::Invalid | Why::Nothing | Why::Unknown) => format!("{name} couldn't play that. Still playing here."),
        Gave::NotReached => format!("That hadn't reached {name} when checked. Still playing here; {name} may yet start."),
        Gave::Unknown => format!("{name} didn't answer. Still playing here; check whether {name} started too."),
    }
}

/// "Play here": what became of the pause that was to stop the other device.
pub fn took(outcome: Outcome, name: &str) -> String {
    match outcome {
        Outcome::Done => format!("Playing here. {name} is paused."),
        // (It did not know what it was told: an older version of the app.)
        Outcome::Refused(Why::Invalid) => format!("Playing here. {name} couldn't be told to stop, so it may still be playing."),
        // (It was told, and its player did not stop.)
        Outcome::Refused(Why::Cannot) => format!("Playing here. {name} couldn't be stopped, so it is still playing too."),
        Outcome::Refused(_) => format!("Playing here. {name} was given something else to do before it could be paused, so it may still be playing."),
        Outcome::NotReached | Outcome::Unknown => format!("Playing here. {name} didn't answer, so it may still be playing."),
    }
}

/// "Play here", when linking ended (the secret was reset, the sign-in changed, the relay
/// could not be reached any more) before the other device could be told to stop.
pub fn took_and_linking_ended(name: &str) -> String {
    format!("Playing here. Linking stopped before {name} could be told, so it may still be playing.")
}

/// This device's music has started on the other, and this one did not stop when asked.
pub fn gave_and_did_not_stop(name: &str, here: Here) -> String {
    format!("{name} has started, and {} couldn't be stopped. Both are playing.", here.this_lower())
}

/// "Play here", when the other device's queue could not be put in this device's player.
pub fn could_not_load_here(name: &str, here: Here) -> String {
    format!("{} couldn't load that. {name} is still playing.", here.this())
}

pub fn queue_never_came(name: &str) -> String {
    format!("{name} didn't answer, so nothing was moved. It may still be playing.")
}

pub fn cannot_be_reached_to_move(name: &str) -> String {
    format!("{name} can't be reached, so nothing was moved.")
}

pub fn none_of_those_songs(name: &str, here: Here) -> String {
    format!("{}'s library has none of those songs. {name} is still playing.", here.this())
}

pub fn chosen_here_meanwhile(name: &str) -> String {
    format!("Something else was chosen here, so {name} was left as it is: it is still playing.")
}

pub fn did_not_start_here(name: &str, here: Here) -> String {
    format!("{} didn't start in time, so {name} was left playing.", here.this())
}

pub fn gone(name: &str, here: Here) -> String {
    format!("{name} can't be reached any more, so the player is back on {}.", here.this_lower())
}

#[cfg(test)]
mod tests {
    use super::super::message::At;
    use super::*;

    fn state(playing: bool) -> Tell {
        Tell { re: None, ok: None, why: None, device: [7; 16], name: "Study PC".into(), at: At { generation: 4, minute: 0 }, busy: false, by: None, done: false, playing, title: "Harbour Lights".into(), artist: "The Tide".into(), steering: String::new(), full: None, queue: None }
    }

    #[test]
    fn what_a_device_is_doing_is_said_in_a_few_words() {
        assert_eq!(about(&state(true), 3), "Playing Harbour Lights");
        assert_eq!(about(&state(false), 3), "Paused on Harbour Lights");
        assert_eq!(about(&Tell { title: String::new(), ..state(false) }, 3), "Nothing playing");
        assert_eq!(about(&Tell { busy: true, ..state(true) }, 3), "In the middle of something…");
        // One that is steering another says so, whatever its own player holds.
        assert_eq!(about(&Tell { steering: "Laptop".into(), busy: true, ..state(false) }, 3), "Steering Laptop");
        // What is shown says how old it is once it is older than a look and its answer.
        assert_eq!(about(&state(true), 49), "Playing Harbour Lights");
        assert_eq!(about(&state(true), 50), "Playing Harbour Lights (as of 50 seconds ago)");
    }

    #[test]
    fn the_person_is_told_which_device_is_playing_however_a_move_ends() {
        // Every end of a move says where the music is, by name.
        for ended in [Gave::Moved, Gave::StartedAndThisChanged, Gave::StartedLate, Gave::Stayed(Why::Busy), Gave::Stayed(Why::Changed), Gave::Stayed(Why::Late), Gave::Stayed(Why::Cannot), Gave::Stayed(Why::Invalid), Gave::NotReached, Gave::Unknown] {
            for here in [Here::Pc, Here::Phone] {
                let words = gave(ended, "Study PC", 0, 12, here);
                assert!(words.contains("Study PC"), "{words}");
                let stays = words.contains("here") || words.contains(here.this()) || words.contains("Both") || words.contains(here.this_lower());
                assert_eq!(stays, ended != Gave::Moved, "{ended:?}: {words}");
            }
        }
        assert_eq!(gave(Gave::Moved, "Study PC", 2_000, 3_000, Here::Pc), "Playing on Study PC. The next 3000 of 5000 songs were moved.");
        assert_eq!(gave(Gave::StartedLate, "Study PC", 0, 1, Here::Phone), "Study PC has started, too late for this phone to stop by itself. Both are playing.");
        // "Play here": each end says the music is here, and what is known of the other.
        for ended in [Outcome::Done, Outcome::Refused(Why::Changed), Outcome::Refused(Why::Invalid), Outcome::NotReached, Outcome::Unknown] {
            let words = took(ended, "Study PC");
            assert!(words.starts_with("Playing here. Study PC "), "{words}");
            assert_eq!(words.ends_with("may still be playing."), ended != Outcome::Done, "{words}");
        }
        // A device on a version that does not know it is being told the music has moved
        // answers that it did not understand: said as what is true, not as a choice made there.
        assert_eq!(took(Outcome::Refused(Why::Invalid), "Study PC"), "Playing here. Study PC couldn't be told to stop, so it may still be playing.");
        // And anything else asked of a device that was not done says so, and why.
        for ended in [Outcome::Refused(Why::Changed), Outcome::Refused(Why::Busy), Outcome::Refused(Why::Late), Outcome::Refused(Why::Cannot), Outcome::NotReached, Outcome::Unknown] {
            assert!(outcome(ended, "Study PC").contains("Study PC"));
        }
        assert_eq!(outcome(Outcome::Done, "Study PC"), "");
    }

    #[test]
    fn a_device_calls_itself_what_it_is() {
        assert_eq!(no_random(Here::Pc), "This PC couldn't make random numbers, so linking is off.");
        assert_eq!(gone("Study PC", Here::Phone), "Study PC can't be reached any more, so the player is back on this phone.");
        assert_eq!(stopped(Stop::Later, Here::Pc), "Update Nexus Music on this PC to link it with your other devices.");
        assert_eq!(none_of_those_songs("Study PC", Here::Pc), "This PC's library has none of those songs. Study PC is still playing.");
        assert_eq!((added(1, "Den"), added(3, "Den")), ("Added to the queue on Den".to_string(), "Added 3 songs to the queue on Den".to_string()));
        assert_eq!(not_sent(Wait::Unanswered, "Den"), "Still waiting for Den to answer.");
    }
}
