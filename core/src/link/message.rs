//! The two things devices say to each other: an ask and a tell.
//!
//! The first slice has two asks, a look and a Pause, and one tell, a brief state that is
//! also the answer to either. Their shapes are exact (`docs/linking-design.md`, 5.5):
//!
//! | Message | Fields | At most |
//! |---|---|---|
//! | a look | `t` = `look`, `want` = `brief` | 64 bytes |
//! | a Pause | `t` = `do`, `do` = `pause`, `at` = two whole numbers; `with` absent or `{}` | 256 bytes |
//! | a tell | `device`, `name`, `at`, `playing` always; `re`, `ok`, `why`, `by`, `busy`, `title`, `artist` when they apply | 4,096 bytes |
//!
//! Fields a version doesn't know are ignored. A message that breaks its shape is not
//! guessed at: an ask that opened is answered `invalid`, a tell is dropped.

use serde_json::{Map, Value, json};

use super::{DeviceId, Nonce, text};

/// The most characters of a name, a title or an artist.
pub const MOST_TEXT: usize = 80;
const MOST_LOOK: usize = 64;

/// The state a command was made against, and the state a device is in: its generation,
/// and its minute (see [`super::rule`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub generation: u64,
    pub minute: u64,
}

/// What one device asks of another.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ask {
    /// Tell me your state.
    Look,
    /// Pause, if you are still as you were `at`.
    Pause { at: At },
}

impl Ask {
    pub fn text(&self) -> String {
        match self {
            Ask::Look => json!({"t": "look", "want": "brief"}).to_string(),
            Ask::Pause { at } => json!({"t": "do", "do": "pause", "at": [at.generation, at.minute]}).to_string(),
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
    let Ok(Value::Object(fields)) = serde_json::from_slice(message) else { return None };
    match fields.get("t")?.as_str()? {
        "look" if message.len() <= MOST_LOOK && fields.get("want")?.as_str()? == "brief" => Some(Ask::Look),
        "do" if fields.get("do")?.as_str()? == "pause" => {
            // A Pause takes nothing with it. One that says it does was made for something else.
            match fields.get("with") {
                None => {}
                Some(Value::Object(with)) if with.is_empty() => {}
                Some(_) => return None,
            }
            Some(Ask::Pause { at: at(fields.get("at")?)? })
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
}

impl Why {
    fn text(self) -> &'static str {
        match self {
            Why::Changed => "changed",
            Why::Busy => "busy",
            Why::Late => "late",
            Why::Invalid => "invalid",
            Why::Cannot => "cannot",
        }
    }

    fn read(text: &str) -> Option<Why> {
        [Why::Changed, Why::Busy, Why::Late, Why::Invalid, Why::Cannot].into_iter().find(|why| why.text() == text)
    }
}

/// What a device says of itself: in answer to an ask, or (later) unasked.
#[derive(Debug, Clone, PartialEq)]
pub struct Tell {
    /// The nonce of the ask this answers.
    pub re: Option<Nonce>,
    /// In an answer to a command: whether it was carried out, and if not, why.
    pub ok: Option<bool>,
    pub why: Option<Why>,
    pub device: DeviceId,
    pub name: String,
    pub at: At,
    /// A choice has been accepted and not yet carried out.
    pub busy: bool,
    /// The nonce of the command last carried out, if nothing has been chosen since.
    pub by: Option<Nonce>,
    pub playing: bool,
    pub title: String,
    pub artist: String,
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
        Value::Object(fields).to_string()
    }
}

/// Reads a tell that opened. `None` for one that breaks its shape: it is dropped. A field
/// that may be absent must still be of its kind when it is there.
pub fn read_tell(message: &[u8]) -> Option<Tell> {
    let Ok(Value::Object(fields)) = serde_json::from_slice(message) else { return None };
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
    })
}

#[cfg(test)]
mod tests {
    use super::super::frame::{MOST_ASK, MOST_TELL};
    use super::*;

    fn tell() -> Tell {
        Tell { re: Some([0x60; 24]), ok: Some(true), why: None, device: [0x81; 16], name: "Phone".into(), at: At { generation: 8, minute: 12 }, busy: false, by: Some([0x60; 24]), playing: false, title: "Song".into(), artist: "Artist".into() }
    }

    #[test]
    fn the_two_asks_are_written_and_read() {
        let pause = Ask::Pause { at: At { generation: 7, minute: 12 } };
        assert_eq!(Ask::Look.text(), r#"{"t":"look","want":"brief"}"#);
        assert_eq!(pause.text(), r#"{"at":[7,12],"do":"pause","t":"do"}"#);
        assert_eq!(read_ask(Ask::Look.text().as_bytes()), Some(Ask::Look));
        assert_eq!(read_ask(pause.text().as_bytes()), Some(pause));
        // As the design's vector writes it, with an empty `with` and a field not known.
        assert_eq!(read_ask(br#"{"t":"do","do":"pause","at":[7,12],"with":{},"later":[1]}"#), Some(pause));
        assert!(Ask::Look.text().len() <= 64 && Ask::Pause { at: At { generation: u64::MAX, minute: u64::MAX } }.text().len() <= MOST_ASK);
    }

    #[test]
    fn an_ask_that_breaks_its_shape_is_not_guessed_at() {
        for wrong in [
            "",
            "pause",
            "[]",
            r#"{"t":"look"}"#,
            r#"{"t":"look","want":"full"}"#,
            r#"{"t":"look","want":"brief","padding":"0000000000000000000000000000000000000000"}"#,
            r#"{"t":"do","do":"play","at":[7,12]}"#,
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
}
