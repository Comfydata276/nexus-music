//! The two things devices say to each other: an ask and a tell.
//!
//! An ask is a look ("tell me your state") or a command ([`Command`]); a tell is a state,
//! and is also the answer to either. Their shapes are exact (`docs/linking-design.md`, 5.5):
//!
//! | Message | Fields | At most |
//! |---|---|---|
//! | a look | `t` = `look`, `want` = `brief`, `full` or `queue` | 64 bytes |
//! | a command | `t` = `do`, `do`, `at` = two whole numbers, `with` as the command needs | 256 bytes; 4,096 for `add`; 210,000 for `adopt` |
//! | a tell | `device`, `name`, `at`, `playing` always; `re`, `ok`, `why`, `by`, `busy`, `title`, `artist`, `steering` when they apply | 4,096 bytes |
//! | a full tell | the same, and `full`: what a device steering this one shows | 70,000 bytes |
//! | a tell with the queue | the same, and `queue`: what is playing and to come, to carry on with elsewhere | 210,000 bytes |
//!
//! Fields a version doesn't know are ignored. A message that breaks its shape is not
//! guessed at: an ask that opened is answered `invalid`, a tell is dropped.

use serde_json::{Map, Value, json};

pub use super::command::{Adopt, Command, MOST_MESSAGE};
use super::command::{MOST_CONTEXT, id_ok};
use super::{DeviceId, Nonce, text};

/// The most characters of a name, a title or an artist.
pub const MOST_TEXT: usize = 80;
const MOST_LOOK: usize = 64;
/// The most a tell may be: one that says what is playing; one for a device that steers
/// this one; and one that carries the queue.
pub const MOST_BRIEF: usize = 4096;
pub const MOST_FULL: usize = 70_000;
/// The most the entries of a full tell's queue come to, and the most of them there are.
/// The rest are counted, not sent.
pub const MOST_SHOWN_BYTES: usize = 60_000;
pub const MOST_SHOWN: usize = 200;

/// The state a command was made against, and the state a device is in: its generation,
/// and its minute (see [`super::rule`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub generation: u64,
    pub minute: u64,
}

/// How much of its state a look asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// What it is playing: for the list of devices.
    Brief,
    /// What a device steering it shows: the song, where it has got to, what is to come.
    Full,
    /// Everything playing and to come, by id: to carry on with it elsewhere.
    Queue,
}

impl Want {
    fn text(self) -> &'static str {
        match self {
            Want::Brief => "brief",
            Want::Full => "full",
            Want::Queue => "queue",
        }
    }
}

/// What one device asks of another.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    /// Tell me your state.
    Look(Want),
    /// Do this, if you are still as you were `at`.
    Do { command: Command, at: At },
}

impl Ask {
    pub fn text(&self) -> String {
        match self {
            Ask::Look(want) => json!({"t": "look", "want": want.text()}).to_string(),
            Ask::Do { command, at } => {
                let mut fields = Map::new();
                fields.insert("t".into(), json!("do"));
                fields.insert("do".into(), json!(command.name()));
                fields.insert("at".into(), json!([at.generation, at.minute]));
                if let Some(with) = command.with() {
                    fields.insert("with".into(), with);
                }
                Value::Object(fields).to_string()
            }
        }
    }
}

fn at(value: &Value) -> Option<At> {
    match value.as_array()?.as_slice() {
        [generation, minute] => Some(At { generation: generation.as_u64()?, minute: minute.as_u64()? }),
        _ => None,
    }
}

/// Reads an ask that opened. `None` is "not an ask this version carries out": the device
/// answers `invalid` and does nothing.
pub fn read_ask(message: &[u8]) -> Option<Ask> {
    // (Nothing bigger than the biggest there is gets as far as being read.)
    if message.len() > MOST_MESSAGE {
        return None;
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice(message) else { return None };
    match fields.get("t")?.as_str()? {
        "look" if message.len() <= MOST_LOOK => {
            let want = fields.get("want")?.as_str()?;
            [Want::Brief, Want::Full, Want::Queue].into_iter().find(|known| known.text() == want).map(Ask::Look)
        }
        "do" => {
            let name = fields.get("do")?.as_str()?;
            if message.len() > Command::most(name) {
                return None;
            }
            Some(Ask::Do { command: Command::read(name, fields.get("with"))?, at: at(fields.get("at")?)? })
        }
        _ => None,
    }
}

/// Why a command was not carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Something has been chosen on the device since the state the command was made against.
    Changed,
    /// A choice has been accepted there and not yet carried out.
    Busy,
    /// The state it was made against is more than a minute or two old.
    Late,
    /// Not an ask this version carries out.
    Invalid,
    /// The device could not do it.
    Cannot,
    /// There was nothing to do: the device was already as the command asked (playing, for a
    /// Play; at that volume). Said with `ok`, not as a refusal: nothing was changed, nothing
    /// moved, and the command is not the one that made the state.
    Nothing,
}

impl Why {
    fn text(self) -> &'static str {
        match self {
            Why::Changed => "changed",
            Why::Busy => "busy",
            Why::Late => "late",
            Why::Invalid => "invalid",
            Why::Cannot => "cannot",
            Why::Nothing => "nothing",
        }
    }

    fn read(text: &str) -> Option<Why> {
        [Why::Changed, Why::Busy, Why::Late, Why::Invalid, Why::Cannot, Why::Nothing].into_iter().find(|why| why.text() == text)
    }
}

/// A song as a device steering another shows it: enough to draw a row with, for a song
/// the steering device's own library does not have.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    /// In seconds.
    pub duration: u32,
    /// Queued by hand: part of "Next in queue".
    pub queued: bool,
}

impl Entry {
    fn value(&self) -> Value {
        let mut fields = Map::new();
        fields.insert("id".into(), json!(self.id));
        fields.insert("title".into(), json!(text::cut(&self.title, MOST_TEXT)));
        fields.insert("artist".into(), json!(text::cut(&self.artist, MOST_TEXT)));
        fields.insert("album".into(), json!(text::cut(&self.album, MOST_TEXT)));
        fields.insert("duration".into(), json!(self.duration));
        if self.queued {
            fields.insert("queued".into(), json!(true));
        }
        Value::Object(fields)
    }

    fn read(value: &Value) -> Option<Entry> {
        let fields = value.as_object()?;
        let words = |name: &str| match fields.get(name) {
            None => Some(String::new()),
            Some(value) => value.as_str().map(|text| text::cut(text, MOST_TEXT)),
        };
        Some(Entry {
            id: fields.get("id")?.as_str().filter(|id| id_ok(id))?.to_string(),
            title: words("title")?,
            artist: words("artist")?,
            album: words("album")?,
            duration: fields.get("duration").map_or(Some(0), Value::as_u64)?.min(u64::from(u32::MAX)) as u32,
            queued: fields.get("queued").map_or(Some(false), Value::as_bool)?,
        })
    }
}

/// What a device steering this one shows: what a jam's guests are shown, between a
/// person's own devices.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Full {
    /// How far into the song it is, in seconds, as the tell was written.
    pub position: f64,
    pub now: Option<Entry>,
    /// What is to come, as far as it was sent; `more` counts the rest.
    pub queue: Vec<Entry>,
    pub more: u64,
    pub context: String,
    pub shuffle: bool,
    pub repeat: u8,
    pub volume: f64,
}

impl Full {
    /// A state with as much of what is to come as a tell carries.
    pub fn fitted(position: f64, now: Option<Entry>, coming: impl Iterator<Item = Entry>, context: &str, shuffle: bool, repeat: u8, volume: f64) -> Full {
        let (mut queue, mut used, mut more) = (Vec::new(), 0usize, 0u64);
        for entry in coming {
            let cost = entry.value().to_string().len() + 1;
            if more > 0 || queue.len() >= MOST_SHOWN || used + cost > MOST_SHOWN_BYTES {
                more += 1;
                continue;
            }
            used += cost;
            queue.push(entry);
        }
        Full { position: if position.is_finite() && position > 0.0 { position } else { 0.0 }, now, queue, more, context: text::cut(context, MOST_CONTEXT), shuffle, repeat: repeat.min(2), volume: if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 } }
    }

    fn value(&self) -> Value {
        json!({
            "position": self.position,
            "now": self.now.as_ref().map(Entry::value),
            "queue": self.queue.iter().map(Entry::value).collect::<Vec<_>>(),
            "more": self.more,
            "context": text::cut(&self.context, MOST_CONTEXT),
            "shuffle": self.shuffle,
            "repeat": self.repeat,
            "volume": self.volume,
        })
    }

    fn read(value: &Value) -> Option<Full> {
        let fields = value.as_object()?;
        let now = match fields.get("now") {
            None | Some(Value::Null) => None,
            Some(entry) => Some(Entry::read(entry)?),
        };
        let listed = fields.get("queue")?.as_array()?;
        if listed.len() > MOST_SHOWN {
            return None;
        }
        Some(Full {
            position: fields.get("position")?.as_f64().filter(|at| at.is_finite() && *at >= 0.0)?,
            now,
            queue: listed.iter().map(Entry::read).collect::<Option<Vec<_>>>()?,
            more: fields.get("more").map_or(Some(0), Value::as_u64)?,
            context: text::cut(fields.get("context").map_or(Some(""), Value::as_str)?, MOST_CONTEXT),
            shuffle: fields.get("shuffle")?.as_bool()?,
            repeat: fields.get("repeat")?.as_u64().filter(|mode| *mode <= 2)? as u8,
            volume: fields.get("volume")?.as_f64().filter(|volume| (0.0..=1.0).contains(volume))?,
        })
    }
}

/// What a device is playing and has to come, by id, for another to carry on with: the song
/// that is playing first, then the rest in the order they would play.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Handed {
    pub songs: Vec<String>,
    /// How many straight after the first were queued by hand.
    pub queued: usize,
    pub position: f64,
    pub context: String,
    /// How many more there were than one message carries.
    pub more: u64,
}

impl Handed {
    /// What is playing and to come, cut from its far end to fit one message.
    pub fn fitted(songs: &[String], queued: usize, position: f64, context: &str) -> Handed {
        // (Room is left for the rest of the tell it travels in.)
        match Adopt::fitted_within(songs, 0, queued, position, context, MOST_MESSAGE - MOST_BRIEF) {
            Some((adopt, left)) => Handed { songs: adopt.songs, queued: adopt.queued, position: adopt.position, context: adopt.context, more: left as u64 },
            None => Handed::default(),
        }
    }

    fn value(&self) -> Value {
        json!({"songs": self.songs, "queued": self.queued, "position": self.position, "context": text::cut(&self.context, MOST_CONTEXT), "more": self.more})
    }

    fn read(value: &Value) -> Option<Handed> {
        let fields = value.as_object()?;
        let songs: Vec<String> = fields.get("songs")?.as_array()?.iter().map(|id| id.as_str().filter(|id| id_ok(id)).map(str::to_string)).collect::<Option<_>>()?;
        Some(Handed {
            queued: fields.get("queued")?.as_u64().filter(|queued| (*queued as usize) < songs.len().max(1))? as usize,
            songs,
            // (A day is longer than any song: the bound a queue sent as a command has.)
            position: fields.get("position")?.as_f64().filter(|at| at.is_finite() && (0.0..=86_400.0).contains(at))?,
            context: text::cut(fields.get("context").map_or(Some(""), Value::as_str)?, MOST_CONTEXT),
            more: fields.get("more").map_or(Some(0), Value::as_u64)?,
        })
    }
}

/// What a device says of itself: in answer to an ask, or unasked, as news.
#[derive(Debug, Clone, PartialEq)]
pub struct Tell {
    /// The nonce of the ask this answers. News has none.
    pub re: Option<Nonce>,
    /// In an answer to a command: whether it was carried out, and if not, why.
    pub ok: Option<bool>,
    pub why: Option<Why>,
    pub device: DeviceId,
    pub name: String,
    pub at: At,
    /// A choice has been accepted and not yet carried out, or its work is still under way.
    pub busy: bool,
    /// The nonce of the command last carried out, if nothing has been chosen since.
    pub by: Option<Nonce>,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    /// The device this one is steering, by name, if it is steering one.
    pub steering: String,
    /// In answer to a look that asked for it.
    pub full: Option<Full>,
    pub queue: Option<Handed>,
}

impl Tell {
    /// The text to seal. Names are cut to length here; with them cut it is always within a
    /// tell's size.
    pub fn text(&self) -> String {
        let mut fields = Map::new();
        if let Some(re) = &self.re {
            fields.insert("re".into(), json!(text::nonce(re)));
        }
        if let Some(ok) = self.ok {
            fields.insert("ok".into(), json!(ok));
        }
        if let Some(why) = self.why {
            fields.insert("why".into(), json!(why.text()));
        }
        fields.insert("device".into(), json!(text::base64(&self.device)));
        fields.insert("name".into(), json!(text::cut(&self.name, MOST_TEXT)));
        fields.insert("at".into(), json!([self.at.generation, self.at.minute]));
        if self.busy {
            fields.insert("busy".into(), json!(true));
        }
        if let Some(by) = &self.by {
            fields.insert("by".into(), json!(text::nonce(by)));
        }
        fields.insert("playing".into(), json!(self.playing));
        fields.insert("title".into(), json!(text::cut(&self.title, MOST_TEXT)));
        fields.insert("artist".into(), json!(text::cut(&self.artist, MOST_TEXT)));
        if !self.steering.is_empty() {
            fields.insert("steering".into(), json!(text::cut(&self.steering, MOST_TEXT)));
        }
        if let Some(full) = &self.full {
            fields.insert("full".into(), full.value());
        }
        if let Some(queue) = &self.queue {
            fields.insert("queue".into(), queue.value());
        }
        Value::Object(fields).to_string()
    }

    /// Whether this is news: a state nobody asked for.
    pub fn news(&self) -> bool {
        self.re.is_none()
    }
}

/// Reads a tell that opened. `None` for one that breaks its shape or is over its size: it
/// is dropped. A field that may be absent must still be of its kind when it is there.
pub fn read_tell(message: &[u8]) -> Option<Tell> {
    if message.len() > MOST_MESSAGE {
        return None;
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice(message) else { return None };
    let most = match (fields.contains_key("queue"), fields.contains_key("full")) {
        (true, _) => MOST_MESSAGE,
        (false, true) => MOST_FULL,
        (false, false) => MOST_BRIEF,
    };
    if message.len() > most {
        return None;
    }
    let nonce = |name: &str| match fields.get(name) {
        None => Some(None),
        Some(value) => text::read_nonce(value.as_str()?).map(Some),
    };
    let flag = |name: &str| match fields.get(name) {
        None => Some(None),
        Some(value) => value.as_bool().map(Some),
    };
    let words = |name: &str| match fields.get(name) {
        None => Some(String::new()),
        Some(value) => value.as_str().map(|text| text::cut(text, MOST_TEXT)),
    };
    let why = match fields.get("why") {
        None => None,
        Some(value) => Some(Why::read(value.as_str()?)?),
    };
    let full = match fields.get("full") {
        None => None,
        Some(value) => Some(Full::read(value)?),
    };
    let queue = match fields.get("queue") {
        None => None,
        Some(value) => Some(Handed::read(value)?),
    };
    Some(Tell {
        re: nonce("re")?,
        ok: flag("ok")?,
        why,
        device: text::read_id(fields.get("device")?.as_str()?)?,
        name: text::cut(fields.get("name")?.as_str()?, MOST_TEXT),
        at: at(fields.get("at")?)?,
        busy: flag("busy")?.unwrap_or(false),
        by: nonce("by")?,
        playing: fields.get("playing")?.as_bool()?,
        title: words("title")?,
        artist: words("artist")?,
        steering: words("steering")?,
        full,
        queue,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where in the song a queue handed over says it is: a number another device made.
    #[test]
    fn a_queue_handed_over_names_a_place_no_later_than_a_day_into_its_song() {
        let handed = |position: Value| Handed::read(&json!({"songs": ["s1", "s2"], "queued": 0, "position": position, "context": "", "more": 0}));
        assert_eq!(handed(json!(12.5)).map(|handed| handed.position), Some(12.5));
        assert!(handed(json!(0)).is_some() && handed(json!(86_400)).is_some());
        for not_one in [json!(86_400.5), json!(1e18), json!(1.7e308), json!(-0.5), json!("12"), Value::Null] {
            assert_eq!(handed(not_one.clone()), None, "{not_one}");
        }
    }

    const MOST_ASK: usize = super::super::command::MOST_SIMPLE;
    const MOST_TELL: usize = MOST_BRIEF;

    fn tell() -> Tell {
        Tell { re: Some([0x60; 24]), ok: Some(true), why: None, device: [0x81; 16], name: "Phone".into(), at: At { generation: 8, minute: 12 }, busy: false, by: Some([0x60; 24]), playing: false, title: "Song".into(), artist: "Artist".into(), steering: String::new(), full: None, queue: None }
    }

    #[test]
    fn the_asks_are_written_and_read() {
        let pause = Ask::Do { command: Command::Pause, at: At { generation: 7, minute: 12 } };
        assert_eq!(Ask::Look(Want::Brief).text(), r#"{"t":"look","want":"brief"}"#);
        assert_eq!(pause.text(), r#"{"at":[7,12],"do":"pause","t":"do"}"#);
        for want in [Want::Brief, Want::Full, Want::Queue] {
            let look = Ask::Look(want);
            assert!(look.text().len() <= 64);
            assert_eq!(read_ask(look.text().as_bytes()), Some(look));
        }
        assert_eq!(read_ask(pause.text().as_bytes()), Some(pause.clone()));
        // As the design's vector writes it, with an empty `with` and a field not known.
        assert_eq!(read_ask(br#"{"t":"do","do":"pause","at":[7,12],"with":{},"later":[1]}"#), Some(pause));
        assert!(Ask::Do { command: Command::Pause, at: At { generation: u64::MAX, minute: u64::MAX } }.text().len() <= MOST_ASK);
        // A command that takes something with it.
        let seek = Ask::Do { command: Command::Seek { to: 61.5 }, at: At { generation: 7, minute: 12 } };
        assert_eq!(seek.text(), r#"{"at":[7,12],"do":"seek","t":"do","with":{"to":61.5}}"#);
        assert_eq!(read_ask(seek.text().as_bytes()), Some(seek));
    }

    #[test]
    fn a_command_over_the_size_its_kind_is_allowed_is_not_read() {
        // A Pause padded past 256 bytes, and an `add` past 4,096: each would be read if it
        // were an `adopt`, which is why the size goes by the command.
        let padded = format!(r#"{{"t":"do","do":"pause","at":[7,12],"later":"{}"}}"#, "x".repeat(300));
        assert_eq!(read_ask(padded.as_bytes()), None);
        let add = format!(r#"{{"t":"do","do":"add","at":[7,12],"with":{{"ids":["a"],"next":false}},"later":"{}"}}"#, "x".repeat(4_100));
        assert_eq!(read_ask(add.as_bytes()), None);
        let ids: Vec<String> = (0..3_000).map(|n| format!("{n:064}")).collect();
        let (adopt, left) = Adopt::fitted(&ids, 0, 0, 0.0, "Everything").unwrap();
        assert_eq!(left, 0);
        let adopt = Ask::Do { command: Command::Adopt(adopt), at: At { generation: 7, minute: 12 } };
        assert!(adopt.text().len() > 190_000 && adopt.text().len() <= MOST_MESSAGE, "{}", adopt.text().len());
        assert_eq!(read_ask(adopt.text().as_bytes()), Some(adopt));
        assert_eq!(read_ask(&vec![b' '; MOST_MESSAGE + 1]), None);
    }

    #[test]
    fn an_ask_that_breaks_its_shape_is_not_guessed_at() {
        for wrong in [
            "",
            "pause",
            "[]",
            r#"{"t":"look"}"#,
            r#"{"t":"look","want":"everything"}"#,
            r#"{"t":"look","want":"brief","padding":"0000000000000000000000000000000000000000"}"#,
            r#"{"t":"do","do":"toggle","at":[7,12]}"#,
            r#"{"t":"do","do":"seek","at":[7,12]}"#,
            r#"{"t":"do","do":"pause"}"#,
            r#"{"t":"do","do":"pause","at":7}"#,
            r#"{"t":"do","do":"pause","at":[7]}"#,
            r#"{"t":"do","do":"pause","at":[7,12,1]}"#,
            r#"{"t":"do","do":"pause","at":[-7,12]}"#,
            r#"{"t":"do","do":"pause","at":[7.5,12]}"#,
            r#"{"t":"do","do":"pause","at":["7","12"]}"#,
            r#"{"t":"do","do":"pause","at":[7,12],"with":{"song":"a"}}"#,
            r#"{"t":"do","do":"pause","at":[7,12],"with":[]}"#,
            r#"{"t":"news","at":[7,12]}"#,
            r#"{"do":"pause","at":[7,12]}"#,
        ] {
            assert_eq!(read_ask(wrong.as_bytes()), None, "{wrong}");
        }
        assert_eq!(read_ask(&[0xff, 0xfe]), None);
    }

    #[test]
    fn a_tell_is_written_and_read_back() {
        let told = tell();
        assert_eq!(read_tell(told.text().as_bytes()), Some(told.clone()));
        // The design's own, as its vector has it.
        let vector = br#"{"re":"YGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3","ok":true,"device":"gYKDhIWGh4iJiouMjY6PkA","name":"Phone","at":[8,12],"by":"YGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3","playing":false,"title":"Song","artist":"Artist"}"#;
        let read = read_tell(vector).unwrap();
        assert_eq!((read.re.map(|re| re[0]), read.device[0], read.at, read.by == read.re, read.busy), (Some(0x60), 0x81, At { generation: 8, minute: 12 }, true, false));
        // What need not be there, isn't: a state nobody asked a command of, with nothing playing.
        let bare = Tell { re: None, ok: None, why: None, by: None, busy: true, title: String::new(), artist: String::new(), ..told.clone() };
        assert_eq!(bare.text(), r#"{"artist":"","at":[8,12],"busy":true,"device":"gYGBgYGBgYGBgYGBgYGBgQ","name":"Phone","playing":false,"title":""}"#);
        assert_eq!(read_tell(bare.text().as_bytes()), Some(bare));
        let refused = Tell { ok: Some(false), why: Some(Why::Busy), ..told };
        assert_eq!(read_tell(refused.text().as_bytes()).and_then(|tell| tell.why), Some(Why::Busy));
        for why in [Why::Changed, Why::Busy, Why::Late, Why::Invalid, Why::Cannot] {
            assert_eq!(Why::read(why.text()), Some(why));
        }
    }

    #[test]
    fn names_are_cut_by_the_one_that_writes_and_again_by_the_one_that_reads() {
        // Eighty characters of four bytes each, three times over, is still within a tell.
        let long = Tell { name: "🎵".repeat(500), title: "🎵".repeat(500), artist: "🎵".repeat(500), ..tell() };
        let text = long.text();
        assert!(text.len() <= MOST_TELL, "{}", text.len());
        let read = read_tell(text.as_bytes()).unwrap();
        assert_eq!((read.name.chars().count(), read.title.chars().count(), read.artist.chars().count()), (80, 80, 80));
        // One that was not cut by its sender is cut all the same, and what can't be seen goes.
        let uncut = format!(r#"{{"device":"gYGBgYGBgYGBgYGBgYGBgQ","name":"{}","at":[1,0],"playing":true,"title":"a\u202eb"}}"#, "x".repeat(200));
        let read = read_tell(uncut.as_bytes()).unwrap();
        assert_eq!((read.name.len(), read.title.as_str(), read.artist.as_str()), (80, "ab", ""));
    }

    #[test]
    fn a_tell_that_breaks_its_shape_is_dropped() {
        let good = r#"{"device":"gYGBgYGBgYGBgYGBgYGBgQ","name":"Phone","at":[8,12],"playing":false}"#;
        assert!(read_tell(good.as_bytes()).is_some());
        for (from, to) in [
            (r#""device":"gYGBgYGBgYGBgYGBgYGBgQ","#, ""),
            (r#""gYGBgYGBgYGBgYGBgYGBgQ""#, r#""gYGBgYGBgYGBgYGBgYGBgR""#),
            (r#""gYGBgYGBgYGBgYGBgYGBgQ""#, r#""short""#),
            (r#""name":"Phone","#, ""),
            (r#""name":"Phone""#, r#""name":7"#),
            (r#""at":[8,12]"#, r#""at":[8]"#),
            (r#""at":[8,12]"#, r#""at":"8,12""#),
            (r#""playing":false"#, r#""playing":"no""#),
            (r#""playing":false"#, r#""playing":false,"busy":1"#),
            (r#""playing":false"#, r#""playing":false,"ok":"yes""#),
            (r#""playing":false"#, r#""playing":false,"why":"because""#),
            (r#""playing":false"#, r#""playing":false,"re":"not a nonce""#),
            (r#""playing":false"#, r#""playing":false,"by":7"#),
            (r#""playing":false"#, r#""playing":false,"title":["a"]"#),
        ] {
            let wrong = good.replace(from, to);
            assert_ne!(wrong, good);
            assert_eq!(read_tell(wrong.as_bytes()), None, "{wrong}");
        }
        for wrong in ["", "[]", "null", "7"] {
            assert_eq!(read_tell(wrong.as_bytes()), None);
        }
    }

    fn entry(n: usize) -> Entry {
        Entry { id: format!("song-{n}"), title: format!("Song {n}"), artist: "Artist".into(), album: "Album".into(), duration: 200, queued: n < 2 }
    }

    #[test]
    fn a_full_tell_and_one_with_the_queue_are_written_and_read_back() {
        let full = Full::fitted(41.5, Some(entry(0)), (1..6).map(entry), "Harbour Lights", true, 1, 0.6);
        assert_eq!((full.queue.len(), full.more), (5, 0));
        let told = Tell { full: Some(full), steering: "Study PC".into(), ..tell() };
        assert_eq!(read_tell(told.text().as_bytes()), Some(told));
        let ids: Vec<String> = (0..40).map(|n| format!("song-{n}")).collect();
        let handed = Handed::fitted(&ids, 3, 12.0, "Morning mix");
        assert_eq!((handed.songs.len(), handed.queued, handed.more), (40, 3, 0));
        let told = Tell { queue: Some(handed), playing: true, ..tell() };
        assert_eq!(read_tell(told.text().as_bytes()), Some(told));
        // Nothing playing: nothing to carry on with, and a tell all the same.
        let empty = Tell { queue: Some(Handed::fitted(&[], 0, 0.0, "")), ..tell() };
        assert_eq!(read_tell(empty.text().as_bytes()).and_then(|tell| tell.queue).map(|queue| queue.songs.len()), Some(0));
    }

    #[test]
    fn a_full_tell_carries_only_so_much_of_what_is_to_come_and_counts_the_rest() {
        // Titles as long as they are let be, in letters of four bytes each.
        let wide = |n: usize| Entry { id: format!("{n:064}"), title: "🎵".repeat(200), artist: "🎵".repeat(200), album: "🎵".repeat(200), duration: 200, queued: false };
        let full = Full::fitted(0.0, Some(wide(0)), (1..500).map(wide), &"🎵".repeat(200), false, 0, 1.0);
        assert!(full.queue.len() >= 50 && full.queue.len() < 200, "{}", full.queue.len());
        assert_eq!(full.queue.len() as u64 + full.more, 499);
        let told = Tell { full: Some(full), name: "🎵".repeat(200), title: "🎵".repeat(200), artist: "🎵".repeat(200), steering: "🎵".repeat(200), ..tell() };
        assert!(told.text().len() <= MOST_FULL, "{}", told.text().len());
        assert!(read_tell(told.text().as_bytes()).is_some());
        // Short ones: no more than two hundred are sent whatever their size.
        let full = Full::fitted(0.0, None, (0..1_000).map(entry), "", false, 0, 1.0);
        assert_eq!((full.queue.len(), full.more), (MOST_SHOWN, 800));
        // A queue of five thousand songs with the longest ids fits one message, cut.
        let ids: Vec<String> = (0..5_000).map(|n| format!("{n:064}")).collect();
        let handed = Handed::fitted(&ids, 2, 0.0, "Everything");
        let told = Tell { queue: Some(handed.clone()), ..tell() };
        assert!(told.text().len() <= MOST_MESSAGE && handed.more > 1_000, "{} {}", told.text().len(), handed.more);
        assert!(read_tell(told.text().as_bytes()).is_some());
    }

    #[test]
    fn a_tell_over_the_size_its_kind_is_allowed_is_dropped() {
        // A brief tell padded past 4,096 bytes; a full one past 70,000.
        let good = r#"{"device":"gYGBgYGBgYGBgYGBgYGBgQ","name":"Phone","at":[8,12],"playing":false"#;
        let brief = format!(r#"{good},"later":"{}"}}"#, "x".repeat(MOST_BRIEF));
        assert_eq!(read_tell(brief.as_bytes()), None);
        let full = r#""full":{"position":0,"queue":[],"shuffle":false,"repeat":0,"volume":1}"#;
        assert!(read_tell(format!("{good},{full}}}").as_bytes()).is_some());
        assert!(read_tell(format!(r#"{good},{full},"later":"{}"}}"#, "x".repeat(MOST_BRIEF)).as_bytes()).is_some());
        assert_eq!(read_tell(format!(r#"{good},{full},"later":"{}"}}"#, "x".repeat(MOST_FULL)).as_bytes()), None);
        // What is in them is checked like everything else.
        for wrong in [
            r#""full":{"position":0,"queue":[],"shuffle":false,"repeat":3,"volume":1}"#,
            r#""full":{"position":-1,"queue":[],"shuffle":false,"repeat":0,"volume":1}"#,
            r#""full":{"position":0,"queue":[{"title":"no id"}],"shuffle":false,"repeat":0,"volume":1}"#,
            r#""full":{"position":0,"queue":[],"shuffle":false,"repeat":0,"volume":2}"#,
            r#""full":{"position":0,"queue":[],"now":"a song","shuffle":false,"repeat":0,"volume":1}"#,
            r#""full":[]"#,
            r#""queue":{"songs":["a","not an id"],"queued":0,"position":0}"#,
            r#""queue":{"songs":["a"],"queued":1,"position":0}"#,
            r#""queue":{"songs":"a","queued":0,"position":0}"#,
        ] {
            assert_eq!(read_tell(format!("{good},{wrong}}}").as_bytes()), None, "{wrong}");
        }
    }

    #[test]
    fn news_is_a_tell_that_answers_nothing() {
        let news = Tell { re: None, ok: None, by: None, ..tell() };
        assert!(news.news() && !tell().news());
        assert_eq!(read_tell(news.text().as_bytes()), Some(news));
    }
}
