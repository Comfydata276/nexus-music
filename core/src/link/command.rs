//! What one device may ask another to do: each command, as data.
//!
//! They are the things one PC could do to another before this, and no others
//! (`docs/linking-design.md`, 5.5). **None is a toggle:** each says the state it wants, so
//! that one delivered late, or twice, cannot undo itself.
//!
//! | `do` | `with` | |
//! |---|---|---|
//! | `play`, `pause`, `next`, `previous` | nothing | |
//! | `seek` | `to` | seconds into the song |
//! | `volume` | `to` | 0 to 1 |
//! | `jump` | `at`, `id` | play the entry `at` places after the current song now |
//! | `move` | `from`, `to`, `queued`, `id` | move an entry; `queued` is the part it lands in |
//! | `remove` | `at`, `id` | take an entry out |
//! | `clear` | `queued` | empty "Next in queue", or else the rest |
//! | `shuffle` | `on` | |
//! | `repeat` | `mode` | 0 off, 1 the queue, 2 the song |
//! | `add` | `ids`, `next` | queue these songs, at the bottom of "Next in queue" or the top |
//! | `adopt` | `songs`, `index`, `queued`, `position`, `context` | take this queue and play it |
//!
//! An entry of the queue is named twice, by its place and by the song that was seen
//! there: the device being asked finds the entry the sender meant even if its queue has
//! moved on. A song is named by its id on the music server, which every device of the
//! account shares.
//!
//! [`Command::read`] checks that a command has every field it needs, each of its kind and
//! within its bounds, before anything is done for it. A field a version does not know is
//! ignored; a field it knows and finds wrong is not guessed at.

use crate::ways::{What, Who};
use serde_json::{Map, Value, json};

use super::text;

/// The longest id of a song that is carried.
pub const MOST_ID: usize = 64;
/// The most songs one `add` queues.
pub const MOST_ADDED: usize = 50;
/// The furthest place in a queue a command can name.
pub const MOST_PLACE: u64 = 100_000;
/// The most a whole message may be, in bytes: one `adopt`, or one state with a queue in it.
pub const MOST_MESSAGE: usize = 210_000;
/// The most a `do` may be for a command that takes little with it, and for an `add`.
pub const MOST_SIMPLE: usize = 256;
pub const MOST_ADD: usize = 4096;
/// The most characters of a context: what the queue is of ("Harbour Lights").
pub const MOST_CONTEXT: usize = 80;

/// Whether this is an id a song can be named by here: one to sixty-four visible ASCII
/// characters, none of which needs escaping in the message.
pub fn id_ok(id: &str) -> bool {
    (1..=MOST_ID).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_graphic() && byte != b'"' && byte != b'\\')
}

/// A queue to take and play: the songs in the order they would play, the one to start
/// with, how many straight after it were queued by hand, and where in it to start.
#[derive(Debug, Clone, PartialEq)]
pub struct Adopt {
    pub songs: Vec<String>,
    pub index: usize,
    pub queued: usize,
    pub position: f64,
    pub context: String,
}

impl Adopt {
    /// Makes a queue to hand over that fits one message. Songs whose ids cannot be
    /// carried are left out (`None` if the one to start with is such a song, or there are
    /// none). A queue too long for one message starts at the song to start with, and is
    /// cut from its far end. Also returns how many songs were left behind.
    pub fn fitted(songs: &[String], index: usize, queued: usize, position: f64, context: &str) -> Option<(Adopt, usize)> {
        Adopt::fitted_within(songs, index, queued, position, context, MOST_MESSAGE)
    }

    /// The same, for a queue that has to fit in `most` bytes: a tell that carries one has
    /// the rest of the tell to carry too.
    pub fn fitted_within(songs: &[String], index: usize, queued: usize, position: f64, context: &str, most: usize) -> Option<(Adopt, usize)> {
        let start = songs.get(index).filter(|id| id_ok(id))?;
        let context = text::cut(context, MOST_CONTEXT);
        // What the message costs apart from its songs, with room to spare; then each song
        // costs its id, two quotes and a comma.
        let fixed = 160 + context.len() * 2;
        let cost = |id: &String| id.len() + 3;
        let whole: usize = songs.iter().filter(|id| id_ok(id)).map(cost).sum();
        let from = if fixed + whole <= most { 0 } else { index };
        let (mut kept, mut used, mut at) = (Vec::new(), fixed, 0);
        for (place, id) in songs.iter().enumerate().skip(from) {
            if !id_ok(id) {
                continue;
            }
            if used + cost(id) > most {
                break;
            }
            if place == index {
                at = kept.len();
            }
            used += cost(id);
            kept.push(id.clone());
        }
        debug_assert_eq!(kept.get(at), Some(start));
        let left = songs.len() - kept.len();
        // Those queued by hand are the ones straight after the song to start with: no more
        // of them than are still there.
        let queued = queued.min(kept.len() - at - 1);
        let position = if position.is_finite() && position > 0.0 { position } else { 0.0 };
        Some((Adopt { songs: kept, index: at, queued, position, context }, left))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Play,
    Pause,
    Next,
    Previous,
    Seek { to: f64 },
    Volume { to: f64 },
    Jump { at: u64, id: String },
    Move { from: u64, to: u64, queued: bool, id: String },
    Remove { at: u64, id: String },
    Clear { queued: bool },
    Shuffle { on: bool },
    Repeat { mode: u8 },
    Add { ids: Vec<String>, next: bool },
    Adopt(Adopt),
    /// Stop: the music that was playing here has been taken up by the device that asks,
    /// and is sounding there ("Play here", pressed on it). Carried out as a pause is; but
    /// **it is nobody's choice at this device**, as the pause that ends a move the other
    /// way is. The person chose that the music be over there, and the queue is that
    /// device's from when it took it: this one going quiet claims nothing, so that the
    /// device now playing is the one whose queue is the newest.
    Moved,
}

fn number(with: &Map<String, Value>, name: &str, most: f64) -> Option<f64> {
    with.get(name)?.as_f64().filter(|value| value.is_finite() && (0.0..=most).contains(value))
}

fn place(with: &Map<String, Value>, name: &str) -> Option<u64> {
    with.get(name)?.as_u64().filter(|value| *value <= MOST_PLACE)
}

fn flag(with: &Map<String, Value>, name: &str) -> Option<bool> {
    with.get(name)?.as_bool()
}

fn id(with: &Map<String, Value>, name: &str) -> Option<String> {
    with.get(name)?.as_str().filter(|id| id_ok(id)).map(str::to_string)
}

fn ids(with: &Map<String, Value>, name: &str, most: usize) -> Option<Vec<String>> {
    let list = with.get(name)?.as_array()?;
    if list.is_empty() || list.len() > most {
        return None;
    }
    list.iter().map(|id| id.as_str().filter(|id| id_ok(id)).map(str::to_string)).collect()
}

impl Command {
    /// What it is called in a message.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Play => "play",
            Command::Pause => "pause",
            Command::Next => "next",
            Command::Previous => "previous",
            Command::Seek { .. } => "seek",
            Command::Volume { .. } => "volume",
            Command::Jump { .. } => "jump",
            Command::Move { .. } => "move",
            Command::Remove { .. } => "remove",
            Command::Clear { .. } => "clear",
            Command::Shuffle { .. } => "shuffle",
            Command::Repeat { .. } => "repeat",
            Command::Add { .. } => "add",
            Command::Adopt(_) => "adopt",
            Command::Moved => "moved",
        }
    }

    /// Who chose it, at the device that carries it out (`crate::ways`): the person, at
    /// another of their devices; but for the stop that ends a move of music, which is
    /// nobody's.
    pub fn chosen_by(&self) -> Who {
        match self {
            Command::Moved => Who::Nobody,
            _ => Who::Elsewhere,
        }
    }

    /// What it touches at the device that carries it out: what a saved queue holds, or
    /// only how the player sounds. It is sorted exactly as the press it stands for is
    /// (`crate::ways`): a command is the person's own choice, made at another device, and
    /// the queue is theirs by it as it would be by the same press there.
    pub fn touches(&self) -> What {
        match self {
            Command::Volume { .. } | Command::Repeat { .. } => What::Manner,
            Command::Play | Command::Pause | Command::Next | Command::Previous | Command::Seek { .. } => What::Playing,
            Command::Jump { .. } | Command::Move { .. } | Command::Remove { .. } | Command::Clear { .. } | Command::Shuffle { .. } => What::Playing,
            Command::Add { .. } | Command::Adopt(_) | Command::Moved => What::Playing,
        }
    }

    /// What goes with it, if anything does.
    pub fn with(&self) -> Option<Value> {
        Some(match self {
            Command::Play | Command::Pause | Command::Next | Command::Previous | Command::Moved => return None,
            Command::Seek { to } | Command::Volume { to } => json!({"to": to}),
            Command::Jump { at, id } | Command::Remove { at, id } => json!({"at": at, "id": id}),
            Command::Move { from, to, queued, id } => json!({"from": from, "to": to, "queued": queued, "id": id}),
            Command::Clear { queued } => json!({"queued": queued}),
            Command::Shuffle { on } => json!({"on": on}),
            Command::Repeat { mode } => json!({"mode": mode}),
            Command::Add { ids, next } => json!({"ids": ids, "next": next}),
            Command::Adopt(adopt) => json!({"songs": adopt.songs, "index": adopt.index, "queued": adopt.queued, "position": adopt.position, "context": adopt.context}),
        })
    }

    /// The most a whole `do` carrying a command of this name may be, in bytes.
    pub fn most(name: &str) -> usize {
        match name {
            "adopt" => MOST_MESSAGE,
            "add" => MOST_ADD,
            _ => MOST_SIMPLE,
        }
    }

    /// Reads a command from its name and what came with it. `None` for a name this version
    /// does not carry out, a field that is missing, of the wrong kind or out of bounds, or
    /// something sent with a command that takes nothing.
    pub fn read(name: &str, with: Option<&Value>) -> Option<Command> {
        let nothing = Map::new();
        let with = match with {
            None => &nothing,
            Some(Value::Object(with)) => with,
            Some(_) => return None,
        };
        Some(match name {
            // A command that takes nothing with it, and says it does, was made for something else.
            "play" | "pause" | "next" | "previous" | "moved" if !with.is_empty() => return None,
            "moved" => Command::Moved,
            "play" => Command::Play,
            "pause" => Command::Pause,
            "next" => Command::Next,
            "previous" => Command::Previous,
            // (A day is longer than any song.)
            "seek" => Command::Seek { to: number(with, "to", 86_400.0)? },
            "volume" => Command::Volume { to: number(with, "to", 1.0)? },
            "jump" => Command::Jump { at: place(with, "at")?, id: id(with, "id")? },
            "move" => Command::Move { from: place(with, "from")?, to: place(with, "to")?, queued: flag(with, "queued")?, id: id(with, "id")? },
            "remove" => Command::Remove { at: place(with, "at")?, id: id(with, "id")? },
            "clear" => Command::Clear { queued: flag(with, "queued")? },
            "shuffle" => Command::Shuffle { on: flag(with, "on")? },
            "repeat" => Command::Repeat { mode: with.get("mode")?.as_u64().filter(|mode| *mode <= 2)? as u8 },
            "add" => Command::Add { ids: ids(with, "ids", MOST_ADDED)?, next: flag(with, "next")? },
            "adopt" => {
                // (No more songs than fit a message with the shortest ids there are.)
                let songs = ids(with, "songs", MOST_MESSAGE / 4)?;
                let index = with.get("index")?.as_u64().filter(|index| (*index as usize) < songs.len())? as usize;
                let queued = with.get("queued")?.as_u64().filter(|queued| (*queued as usize) < songs.len() - index)? as usize;
                Command::Adopt(Adopt { songs, index, queued, position: number(with, "position", 86_400.0)?, context: text::cut(with.get("context")?.as_str()?, MOST_CONTEXT) })
            }
            _ => return None,
        })
    }

    /// As one value, the way it travels in a `do`: `{"do": name, "with": {…}}`.
    pub fn value(&self) -> Value {
        match self.with() {
            Some(with) => json!({"do": self.name(), "with": with}),
            None => json!({"do": self.name()}),
        }
    }

    /// Reads [`Command::value`] back, with the same checks as [`Command::read`].
    pub fn from_value(value: &Value) -> Option<Command> {
        Command::read(value.get("do")?.as_str()?, value.get("with"))
    }

    /// Whether only the latest of several waiting to be sent is worth sending: a slider
    /// dragged across its range asks for many, and means the last.
    pub fn latest_only(&self) -> bool {
        matches!(self, Command::Volume { .. })
    }
}

/// A command crosses to an app's own side (the phone's player) spelt as it is in a
/// message, and is read back through the one function that checks it.
impl serde::Serialize for Command {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value().serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Command {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Command, D::Error> {
        let value = <Value as serde::Deserialize>::deserialize(deserializer)?;
        Command::from_value(&value).ok_or_else(|| serde::de::Error::custom("not a command"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every() -> Vec<Command> {
        vec![
            Command::Play,
            Command::Pause,
            Command::Moved,
            Command::Next,
            Command::Previous,
            Command::Seek { to: 61.5 },
            Command::Volume { to: 0.4 },
            Command::Jump { at: 3, id: "s-3".into() },
            Command::Move { from: 4, to: 0, queued: true, id: "s-4".into() },
            Command::Remove { at: 2, id: "s-2".into() },
            Command::Clear { queued: false },
            Command::Shuffle { on: true },
            Command::Repeat { mode: 2 },
            Command::Add { ids: vec!["a".into(), "b".into()], next: true },
            Command::Adopt(Adopt { songs: vec!["a".into(), "b".into(), "c".into()], index: 1, queued: 1, position: 12.25, context: "Harbour Lights".into() }),
        ]
    }

    #[test]
    fn every_command_is_written_and_read_back() {
        let all = every();
        assert_eq!(all.len(), 15);
        for command in all {
            let with = command.with();
            assert_eq!(Command::read(command.name(), with.as_ref()), Some(command.clone()), "{}", command.name());
            // And through the text it travels as.
            let text = with.map(|with| with.to_string());
            let back: Option<Value> = text.as_deref().map(|text| serde_json::from_str(text).unwrap());
            assert_eq!(Command::read(command.name(), back.as_ref()), Some(command));
        }
        // And as one value, for an app's own side: spelt the same, checked the same.
        for command in every() {
            let text = serde_json::to_string(&command).unwrap();
            assert_eq!(serde_json::from_str::<Command>(&text).ok(), Some(command), "{text}");
        }
        assert_eq!(serde_json::to_string(&Command::Seek { to: 5.0 }).unwrap(), r#"{"do":"seek","with":{"to":5.0}}"#);
        assert!(serde_json::from_str::<Command>(r#"{"do":"seek","with":{"to":-1}}"#).is_err());
        // What takes nothing may come with an empty `with`, and a field not known is ignored.
        assert_eq!(Command::read("pause", Some(&json!({}))), Some(Command::Pause));
        assert_eq!(Command::read("seek", Some(&json!({"to": 5, "later": [1]}))), Some(Command::Seek { to: 5.0 }));
    }

    #[test]
    fn a_command_missing_a_field_or_with_one_of_the_wrong_kind_is_not_carried_out() {
        for (name, with) in [
            ("toggle", json!({})),
            ("stop", json!({})),
            ("pause", json!({"to": 1})),
            ("next", json!([])),
            ("seek", json!({})),
            ("seek", json!({"to": "5"})),
            ("seek", json!({"to": -1})),
            ("seek", json!({"to": 1e9})),
            ("volume", json!({"to": 1.5})),
            ("volume", json!({"to": true})),
            ("jump", json!({"at": 1})),
            ("jump", json!({"at": -1, "id": "a"})),
            ("jump", json!({"at": 1.5, "id": "a"})),
            ("jump", json!({"at": 100_001, "id": "a"})),
            ("jump", json!({"at": 1, "id": ""})),
            ("jump", json!({"at": 1, "id": "a b"})),
            ("jump", json!({"at": 1, "id": "a\"b"})),
            ("jump", json!({"at": 1, "id": "é"})),
            ("jump", json!({"at": 1, "id": "x".repeat(65)})),
            ("move", json!({"from": 1, "to": 2, "id": "a"})),
            ("move", json!({"from": 1, "to": 2, "queued": 1, "id": "a"})),
            ("remove", json!({"id": "a"})),
            ("clear", json!({})),
            ("clear", json!({"queued": "yes"})),
            ("shuffle", json!({"on": 1})),
            ("repeat", json!({"mode": 3})),
            ("repeat", json!({"mode": -1})),
            ("add", json!({"ids": [], "next": false})),
            ("add", json!({"ids": ["a"]})),
            ("add", json!({"ids": "a", "next": false})),
            ("add", json!({"ids": ["a", 7], "next": false})),
            ("add", json!({"ids": vec!["a"; 51], "next": false})),
            ("adopt", json!({"songs": [], "index": 0, "queued": 0, "position": 0, "context": ""})),
            ("adopt", json!({"songs": ["a"], "index": 1, "queued": 0, "position": 0, "context": ""})),
            ("adopt", json!({"songs": ["a", "b"], "index": 1, "queued": 1, "position": 0, "context": ""})),
            ("adopt", json!({"songs": ["a"], "index": 0, "queued": 0, "position": -3, "context": ""})),
            ("adopt", json!({"songs": ["a"], "index": 0, "queued": 0, "position": 0})),
            ("adopt", json!({"songs": ["a"], "index": 0, "queued": 0, "position": 0, "context": 7})),
        ] {
            assert_eq!(Command::read(name, Some(&with)), None, "{name} {with}");
        }
        assert_eq!(Command::read("seek", None), None);
        // The fiftieth song is the last that is taken.
        assert!(Command::read("add", Some(&json!({"ids": vec!["a"; 50], "next": false}))).is_some());
    }

    #[test]
    fn each_command_fits_what_its_kind_is_allowed() {
        let wrapped = |command: &Command| json!({"t": "do", "do": command.name(), "at": [u64::MAX, u64::MAX], "with": command.with()}).to_string().len();
        let longest = "x".repeat(MOST_ID);
        for command in [Command::Seek { to: 86_399.999_999 }, Command::Move { from: MOST_PLACE, to: MOST_PLACE, queued: false, id: longest.clone() }, Command::Jump { at: MOST_PLACE, id: longest.clone() }] {
            assert!(wrapped(&command) <= Command::most(command.name()), "{}", command.name());
        }
        let add = Command::Add { ids: vec![longest.clone(); MOST_ADDED], next: false };
        assert!(wrapped(&add) <= Command::most("add"), "{}", wrapped(&add));
    }

    #[test]
    fn a_queue_is_fitted_to_one_message_from_the_song_to_start_with() {
        let songs = |count: usize, length: usize| -> Vec<String> { (0..count).map(|n| format!("{n:0length$}")).collect() };
        // One that fits is handed over whole.
        let short = songs(100, 22);
        let (adopt, left) = Adopt::fitted(&short, 40, 3, 12.0, "Morning mix").unwrap();
        assert_eq!((adopt.songs.len(), adopt.index, adopt.queued, left), (100, 40, 3, 0));
        // Five thousand songs with the longest ids there can be: it starts at the song that
        // was chosen, and is cut from its far end.
        let long = songs(5_000, MOST_ID);
        let (adopt, left) = Adopt::fitted(&long, 1_000, 2, 0.0, "Everything").unwrap();
        assert_eq!((adopt.index, adopt.songs.first(), adopt.queued), (0, long.get(1_000), 2));
        assert!((3_000..3_200).contains(&adopt.songs.len()), "{}", adopt.songs.len());
        assert_eq!(left, 5_000 - adopt.songs.len());
        let message = json!({"t": "do", "do": "adopt", "at": [u64::MAX, u64::MAX], "with": Command::Adopt(adopt.clone()).with()}).to_string();
        assert!(message.len() <= MOST_MESSAGE, "{}", message.len());
        assert_eq!(Command::read("adopt", Command::Adopt(adopt.clone()).with().as_ref()), Some(Command::Adopt(adopt)));
        // A song that cannot be named is left out, and the count of those queued by hand
        // never reaches past the end.
        let mixed = vec!["a".to_string(), "not an id".to_string(), "b".to_string(), "c".to_string()];
        let (adopt, left) = Adopt::fitted(&mixed, 2, 9, f64::NAN, "").unwrap();
        assert_eq!((adopt.songs, adopt.index, adopt.queued, adopt.position, left), (vec!["a".to_string(), "b".into(), "c".into()], 1, 1, 0.0, 1));
        // Nothing to start with: nothing to hand over.
        assert_eq!(Adopt::fitted(&mixed, 1, 0, 0.0, ""), None);
        assert_eq!(Adopt::fitted(&[], 0, 0, 0.0, ""), None);
    }
}
