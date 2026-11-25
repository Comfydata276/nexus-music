//! The shared record as text: reading the comment it is kept in, and writing it back.
//!
//! The record is written by every version of both apps, and whatever one of them writes with
//! a newer time wins on every device. So a device may only write the record back if it can
//! write back **everything it read**, and has to know when it can't:
//!
//! * Only the empty text is "nothing shared yet". A comment that is not empty and is not a
//!   record (someone's own note, spaces, an object that isn't marked as this app's) is
//!   [`Read::Unreadable`]: it is left exactly as it is.
//! * The layout number is read first, by itself, before anything else is taken to be in this
//!   layout. A record in a later layout is [`Read::Later`] whatever else it holds, and
//!   however its fields have changed: never "unreadable" (which a later version's own
//!   devices would not thank this one for stopping at), and never written.
//! * Fields this version doesn't know at the top of the record are kept as the text they
//!   were written as, and written back after the ones it does know. A field it doesn't know
//!   further in (inside a list's choices, or a note of when a song was added) has nowhere to
//!   be kept: then the record is read, and `whole` says it must not be written.
//!
//! What a record's known fields are read as is serde's reading of [`Shared`], unchanged from
//! every earlier version of the Windows app, so a text they read is read the same here.

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use serde::de::{MapAccess, Visitor};
use serde_json::value::RawValue;

use crate::sync::{Shared, VERSION};

/// What a playlist's comment turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    /// Nothing shared yet: the comment is the empty text.
    Empty,
    /// A record in this layout.
    Record(Record),
    /// A record in a later layout, and what of it this version understands (nothing, if its
    /// known fields have changed shape). Read, and never written.
    Later { layout: u64, shared: Option<Shared> },
    /// Not a record. Left exactly as it is.
    Unreadable,
}

/// A record in this layout, with what must go back out with it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    pub shared: Shared,
    /// Fields at the top of the record that this version doesn't know: each one's name, and
    /// its value as the text it was written as. They are written back after the known ones.
    pub unknown: Vec<(String, String)>,
    /// Whether everything that was read can be written back. False when there is a field
    /// this version doesn't know somewhere it can't carry it: the record is then read and
    /// not written.
    pub whole: bool,
}

/// The names of the fields of [`Shared`], as they are written.
const KNOWN: [&str; 10] = ["nexus_music_sync", "pins", "pins_at", "theme", "theme_at", "lists", "added", "history", "plays", "play_sources"];

/// The fields at the top of an object, each with its value as it was written.
struct Top(Vec<(String, Box<RawValue>)>);

impl<'de> Deserialize<'de> for Top {
    fn deserialize<D: serde::Deserializer<'de>>(from: D) -> Result<Top, D::Error> {
        struct Fields;
        impl<'de> Visitor<'de> for Fields {
            type Value = Top;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Top, A::Error> {
                let mut fields = Vec::new();
                while let Some(field) = map.next_entry::<String, Box<RawValue>>()? {
                    fields.push(field);
                }
                Ok(Top(fields))
            }
        }
        from.deserialize_map(Fields)
    }
}

// The two things with fields of their own further in, read so that a field nobody knows is
// an error rather than passed over: how it is found that the record can't be written whole.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct StrictPrefs {
    #[serde(default)]
    shuffle: bool,
    #[serde(default)]
    sort: usize,
    #[serde(default)]
    descending: bool,
    #[serde(default)]
    sort_chosen: bool,
    #[serde(default)]
    at: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct StrictNote {
    #[serde(default)]
    times: Vec<i64>,
    #[serde(default)]
    at: i64,
}

/// Reads a playlist's comment.
pub fn read(text: &str) -> Read {
    if text.is_empty() {
        return Read::Empty;
    }
    let Ok(Top(fields)) = serde_json::from_str::<Top>(text) else { return Read::Unreadable };
    // The layout number first, by itself. It is what marks the text as this app's: an object
    // without it, or with something else under its name, is somebody else's.
    let mut marks = fields.iter().filter(|(name, _)| name == KNOWN[0]);
    let (Some((_, mark)), None) = (marks.next(), marks.next()) else { return Read::Unreadable };
    let Ok(layout) = serde_json::from_str::<u64>(mark.get()) else { return Read::Unreadable };
    if layout == 0 {
        return Read::Unreadable;
    }
    let shared = serde_json::from_str::<Shared>(text).ok();
    if layout > VERSION as u64 {
        return Read::Later { layout, shared };
    }
    // In this layout, and yet not what this layout holds: not a record.
    let Some(shared) = shared else { return Read::Unreadable };
    let mut whole = true;
    let mut unknown = Vec::new();
    for (name, value) in &fields {
        match name.as_str() {
            "lists" => whole &= serde_json::from_str::<HashMap<String, StrictPrefs>>(value.get()).is_ok(),
            "added" => whole &= serde_json::from_str::<HashMap<String, HashMap<String, StrictNote>>>(value.get()).is_ok(),
            known if KNOWN.contains(&known) => {}
            _ => unknown.push((name.clone(), value.get().to_string())),
        }
    }
    Read::Record(Record { shared, unknown, whole })
}

/// Writes a record: the known fields as every version has written them, then the unknown
/// ones as they were read.
pub fn write(shared: &Shared, unknown: &[(String, String)]) -> String {
    let mut text = serde_json::to_string(shared).unwrap_or_default();
    if unknown.is_empty() || !text.ends_with('}') {
        return text;
    }
    text.pop();
    for (name, value) in unknown {
        text.push(',');
        text.push_str(&serde_json::to_string(name).unwrap_or_default());
        text.push(':');
        text.push_str(value);
    }
    text.push('}');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notes::Note;
    use crate::sync::ListPrefs;

    fn record(text: &str) -> Record {
        match read(text) {
            Read::Record(record) => record,
            other => panic!("{text} was read as {other:?}"),
        }
    }

    #[test]
    fn only_the_empty_text_is_nothing_shared_yet() {
        assert_eq!(read(""), Read::Empty);
        // Everything else that isn't a record is left alone: spaces, a note of someone's
        // own, text cut short, and an object that isn't marked as this app's.
        for text in [" ", "\n", "do not delete: my notes", "hello", "[1,2]", "12", "null", r#"{"nexus_music_sync":1,"pins":["a""#, "{}", r#"{"pins":["a"],"pins_at":3}"#] {
            assert_eq!(read(text), Read::Unreadable, "{text:?}");
        }
        // The mark has to be the layout's number, once.
        for mark in [r#""1""#, "1.0", "-1", "0", "true", "null", "[1]"] {
            assert_eq!(read(&format!(r#"{{"nexus_music_sync":{mark}}}"#)), Read::Unreadable, "{mark}");
        }
        assert_eq!(read(r#"{"nexus_music_sync":1,"nexus_music_sync":1}"#), Read::Unreadable);
        // In this layout, with a field that isn't what this layout holds: not a record.
        assert_eq!(read(r#"{"nexus_music_sync":1,"pins":"a"}"#), Read::Unreadable);
        assert_eq!(read(r#"{"nexus_music_sync":1,"pins_at":"12"}"#), Read::Unreadable);
        assert_eq!(read(r#"{"nexus_music_sync":1,"history":[["a",1,2]]}"#), Read::Unreadable);
    }

    #[test]
    fn a_record_is_read_as_every_version_has_read_it() {
        let read = record(r#" {"nexus_music_sync":1,"pins":["p1","p2"],"pins_at":90,"theme":"Ocean","theme_at":60,"lists":{"playlist:x":{"shuffle":true,"sort":4,"at":80}},"added":{"x":{"s":{"times":[5],"at":5}}},"history":[["a",30]],"plays":[["a",30],["a",20]],"play_sources":[["a",30,"playlist:x"]]} "#);
        assert!(read.whole && read.unknown.is_empty());
        let shared = read.shared;
        assert_eq!((shared.pins.as_slice(), shared.pins_at, shared.theme.as_str(), shared.theme_at), (["p1".to_string(), "p2".to_string()].as_slice(), 90, "Ocean", 60));
        assert_eq!(shared.lists["playlist:x"], ListPrefs { shuffle: true, sort: 4, at: 80, ..Default::default() });
        assert_eq!(shared.added["x"]["s"], Note { times: vec![5], at: 5 });
        assert_eq!(shared.plays.len(), 2);
        // Missing fields are their defaults, as they always were.
        assert_eq!(record(r#"{"nexus_music_sync":1}"#).shared, Shared { nexus_music_sync: 1, ..Default::default() });
        // What is written is read back as it was.
        assert_eq!(record(&write(&shared, &[])).shared, shared);
        assert_eq!(write(&shared, &[]), serde_json::to_string(&shared).unwrap());
    }

    #[test]
    fn the_layout_number_is_read_before_anything_else() {
        // A later layout that this version can still make something of.
        let later = r#"{"nexus_music_sync":2,"pins":["from-later"],"pins_at":50,"more":[1,2,3]}"#;
        let Read::Later { layout: 2, shared: Some(shared) } = read(later) else { panic!("{:?}", read(later)) };
        assert_eq!(shared.pins, ["from-later"]);
        // One whose known fields have changed shape is still a later layout: not
        // unreadable, and never layout 1 (read as a whole first, as it used to be, this
        // failed, was taken for "nothing shared yet", and was written over).
        for text in [r#"{"nexus_music_sync":2,"pins":{"a":1}}"#, r#"{"pins":"all of them","nexus_music_sync":3}"#, r#"{"nexus_music_sync":99999999999,"history":7}"#] {
            assert!(matches!(read(text), Read::Later { shared: None, .. }), "{text}");
        }
    }

    #[test]
    fn what_this_version_does_not_know_goes_back_out_exactly_or_the_record_is_not_written() {
        // At the top: kept as the text it was written as, whatever is in it.
        let text = r#"{"nexus_music_sync":1,"pins":["a"],"devices":{"pc":[1,2,{"x":null}],"n":1.50,"big":123456789012345678901234567890},"pins_at":3,"note":"later é"}"#;
        let read = record(text);
        assert!(read.whole);
        assert_eq!(read.unknown, [("devices".to_string(), r#"{"pc":[1,2,{"x":null}],"n":1.50,"big":123456789012345678901234567890}"#.to_string()), ("note".to_string(), r#""later é""#.to_string())]);
        let mut shared = read.shared.clone();
        shared.pins.push("b".into());
        let out = write(&shared, &read.unknown);
        assert!(out.ends_with(r#","devices":{"pc":[1,2,{"x":null}],"n":1.50,"big":123456789012345678901234567890},"note":"later é"}"#), "{out}");
        // Read again it is the same record with the same unknown fields, and an earlier
        // version reads the known part of it as it reads any record.
        let again = record(&out);
        assert_eq!((again.shared.pins.len(), &again.unknown, again.whole), (2, &read.unknown, true));
        assert_eq!(serde_json::from_str::<Shared>(&out).unwrap(), shared);
        // Further in there is nowhere to keep it: read, and not to be written.
        for text in [
            r#"{"nexus_music_sync":1,"lists":{"liked":{"shuffle":true,"at":4,"view":"grid"}}}"#,
            r#"{"nexus_music_sync":1,"added":{"p":{"s":{"times":[4],"at":4,"by":"pc"}}}}"#,
        ] {
            let read = record(text);
            assert!(!read.whole, "{text}");
            assert!(read.shared.lists.contains_key("liked") || read.shared.added.contains_key("p"));
        }
    }
}
