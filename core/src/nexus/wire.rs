//! Complete, bounded JSON shared by the Nexus messages. Input is already in memory.

use super::proto::{BackendId, HELLO_MAX_DEPTH, Speaks, Version};
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Write};
use zeroize::Zeroizing;

pub const BODY_MAX_BYTES: usize = 1024 * 1024;

/// No parsing or serialization error contains any part of a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadMessage {
    TooLarge,
    Form,
    Status(u16),
}

/// The bytes returned by the encoder. Keep them only as long as the transport needs them.
pub struct WireBody(Zeroizing<Vec<u8>>);

impl fmt::Debug for WireBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WireBody(..)")
    }
}

impl WireBody {
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

mod typed {
    use super::{BadMessage, RawValue, Serialize};

    pub trait Value: Sized {
        fn read(raw: &RawValue) -> Result<Self, BadMessage>;

        fn check(&self) -> Result<(), BadMessage> {
            Ok(())
        }
    }

    pub trait Message: Value + Serialize {
        const MAX_BYTES: usize = super::BODY_MAX_BYTES;

        fn max_bytes(&self) -> usize {
            Self::MAX_BYTES
        }
    }
}

/// Reads the entire body supplied by the transport. The transport first has to confirm
/// it received the whole HTTP body, including when the connection closed early.
pub fn read<T: Message>(body: &[u8]) -> Result<T, BadMessage> {
    read_bounded(body, T::MAX_BYTES)
}

pub(crate) fn read_bounded<T: Message>(body: &[u8], maximum: usize) -> Result<T, BadMessage> {
    if body.len() > maximum.min(T::MAX_BYTES) {
        return Err(BadMessage::TooLarge);
    }
    let text = std::str::from_utf8(body).map_err(|_| BadMessage::Form)?;
    let mut walk = JsonWalk(text);
    walk.value(0)?;
    if !walk.rest().is_empty() {
        return Err(BadMessage::Form);
    }
    let value = T::read(plain_json(text)?)?;
    if body.len() > value.max_bytes() {
        return Err(BadMessage::TooLarge);
    }
    value.check()?;
    Ok(value)
}

pub(crate) use typed::{Message, Value};

/// Writes JSON with the byte ceiling the reader applies. Count first so a buffer
/// containing a password or bearer never has to grow and leave an unwiped allocation.
pub fn encode<T: Message>(value: &T) -> Result<WireBody, BadMessage> {
    struct Count {
        size: usize,
        maximum: usize,
    }
    impl Write for Count {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.size = self.size.saturating_add(bytes.len());
            if self.size > self.maximum {
                return Err(io::ErrorKind::FileTooLarge.into());
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    value.check()?;
    let mut count = Count { size: 0, maximum: value.max_bytes().min(T::MAX_BYTES) };
    serde_json::to_writer(&mut count, value).map_err(|_| {
        if count.size > count.maximum { BadMessage::TooLarge } else { BadMessage::Form }
    })?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(count.size));
    serde_json::to_writer(&mut *bytes, value).map_err(|_| BadMessage::Form)?;
    Ok(WireBody(bytes))
}

/// A structural pass checks the whole tree, including fields typed readers skip.
/// serde_json reads unescaped raw slices; strings are decoded in wiping buffers.
struct JsonWalk<'a>(&'a str);

impl<'a> JsonWalk<'a> {
    fn rest(&mut self) -> &'a str {
        self.0 = self.0.trim_start_matches([' ', '\n', '\r', '\t']);
        self.0
    }

    fn take(&mut self, next: char) -> bool {
        match self.rest().strip_prefix(next) {
            Some(rest) => {
                self.0 = rest;
                true
            }
            None => false,
        }
    }

    fn raw(&mut self) -> Result<&'a RawValue, BadMessage> {
        let rest = self.rest();
        let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<&RawValue>();
        let raw = stream.next().ok_or(BadMessage::Form)?.map_err(|_| BadMessage::Form)?;
        self.0 = rest.get(stream.byte_offset()..).ok_or(BadMessage::Form)?;
        Ok(raw)
    }

    fn value(&mut self, inside: usize) -> Result<(), BadMessage> {
        if self.take('{') {
            if inside == HELLO_MAX_DEPTH {
                return Err(BadMessage::Form);
            }
            if self.take('}') {
                return Ok(());
            }
            let mut keys = BTreeSet::new();
            loop {
                if !self.rest().starts_with('"') {
                    return Err(BadMessage::Form);
                }
                let key = json_string(self.raw()?)?;
                if !keys.insert(key.to_string()) || !self.take(':') {
                    return Err(BadMessage::Form);
                }
                self.value(inside + 1)?;
                if self.take('}') {
                    return Ok(());
                }
                if !self.take(',') {
                    return Err(BadMessage::Form);
                }
            }
        }
        if self.take('[') {
            if inside == HELLO_MAX_DEPTH {
                return Err(BadMessage::Form);
            }
            if self.take(']') {
                return Ok(());
            }
            loop {
                self.value(inside + 1)?;
                if self.take(']') {
                    return Ok(());
                }
                if !self.take(',') {
                    return Err(BadMessage::Form);
                }
            }
        }
        let raw = self.raw()?;
        if raw.get().starts_with('"') {
            json_string(raw)?;
        }
        Ok(())
    }
}

pub(crate) fn plain_json<'a, T: serde::Deserialize<'a>>(text: &'a str) -> Result<T, BadMessage> {
    serde_json::from_str(text).map_err(|_| BadMessage::Form)
}

// RawValue finds the end of a JSON string. Decode it here so escapes, including
// UTF-16 pairs, never leave a password in serde_json's string scratch allocation.
pub(crate) fn json_string(raw: &RawValue) -> Result<Zeroizing<String>, BadMessage> {
    let inner = raw.get().strip_prefix('"').and_then(|s| s.strip_suffix('"')).ok_or(BadMessage::Form)?;
    let mut out = Zeroizing::new(String::with_capacity(inner.len()));
    let mut chars = inner.chars();
    fn word(chars: &mut std::str::Chars<'_>) -> Result<u16, BadMessage> {
        let mut word = 0u16;
        for _ in 0..4 {
            let digit = chars.next().and_then(|c| c.to_digit(16)).ok_or(BadMessage::Form)?;
            word = (word << 4) | digit as u16;
        }
        Ok(word)
    }
    while let Some(ch) = chars.next() {
        let ch = match ch {
            '\\' => match chars.next() {
                Some(escaped @ ('"' | '\\' | '/')) => escaped,
                Some('b') => '\u{08}',
                Some('f') => '\u{0c}',
                Some('n') => '\n',
                Some('r') => '\r',
                Some('t') => '\t',
                Some('u') => {
                    let hi = word(&mut chars)?;
                    if (0xd800..=0xdbff).contains(&hi) {
                        if chars.next() != Some('\\') || chars.next() != Some('u') {
                            return Err(BadMessage::Form);
                        }
                        let lo = word(&mut chars)?;
                        let mut pair = char::decode_utf16([hi, lo]);
                        let scalar = pair.next().and_then(Result::ok).ok_or(BadMessage::Form)?;
                        if pair.next().is_some() {
                            return Err(BadMessage::Form);
                        }
                        scalar
                    } else {
                        char::from_u32(u32::from(hi)).ok_or(BadMessage::Form)?
                    }
                }
                _ => return Err(BadMessage::Form),
            },
            '"' | '\u{00}'..='\u{1f}' => return Err(BadMessage::Form),
            _ => ch,
        };
        out.push(ch);
    }
    Ok(out)
}

pub(crate) fn read_text<T>(raw: &RawValue, parse: impl FnOnce(&str) -> Option<T>) -> Result<T, BadMessage> {
    let text = json_string(raw)?;
    parse(&text).ok_or(BadMessage::Form)
}

pub(crate) struct Fields<'a>(pub(crate) BTreeMap<String, &'a RawValue>);

impl<'a> Fields<'a> {
    pub(crate) fn read(raw: &'a RawValue) -> Result<Self, BadMessage> {
        if !raw.get().starts_with('{') {
            return Err(BadMessage::Form);
        }
        plain_json(raw.get()).map(Self)
    }

    pub(crate) fn get<T: Value>(&self, key: &str) -> Result<T, BadMessage> {
        T::read(self.0.get(key).ok_or(BadMessage::Form)?)
    }

    // An enum is one selected member. Unknown members are skipped, just as in a
    // struct. Two known members in one enum are conflicting.
    pub(crate) fn select(&self, keys: &[&'static str]) -> Result<(&'static str, &'a RawValue), BadMessage> {
        let mut found = keys.iter().filter_map(|&key| self.0.get(key).map(|&raw| (key, raw)));
        let selected = found.next().ok_or(BadMessage::Form)?;
        if found.next().is_some() {
            return Err(BadMessage::Form);
        }
        Ok(selected)
    }
}

impl Value for Speaks {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let [low, high] = plain_json::<[u32; 2]>(raw.get())?;
        Speaks::new(low, high).ok_or(BadMessage::Form)
    }
}

pub(crate) fn write_speaks<S: Serializer>(speaks: &Speaks, serializer: S) -> Result<S::Ok, S::Error> {
    [speaks.low(), speaks.high()].serialize(serializer)
}

pub(crate) fn write_version<S: Serializer>(version: &Version, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(version)
}

pub(crate) fn write_backend<S: Serializer>(backend: &BackendId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&backend.to_hex())
}

impl Value for u64 {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        plain_json(raw.get())
    }
}

impl Value for bool {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        plain_json(raw.get())
    }
}

impl<T: Value> Value for Option<T> {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        if raw.get() == "null" { Ok(None) } else { T::read(raw).map(Some) }
    }
}

impl Value for Version {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        read_text(raw, Self::parse)
    }
}

impl Value for BackendId {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        read_text(raw, Self::from_hex)
    }
}

/// A typed JSON object with every declared field required.
macro_rules! object {
    ($(#[$doc:meta])* $name:ident { $($(#[$attr:meta])* $field:ident : $kind:ty),* $(,)? }) => {
        $(#[$doc])*
        #[derive(Debug, PartialEq, Eq, Serialize)]
        pub struct $name {
            $($(#[$attr])* pub $field: $kind,)*
        }

        impl $crate::nexus::wire::Value for $name {
            fn read(raw: &serde_json::value::RawValue) -> Result<Self, $crate::nexus::wire::BadMessage> {
                let _fields = $crate::nexus::wire::Fields::read(raw)?;
                Ok(Self { $($field: _fields.get(stringify!($field))?,)* })
            }
        }
    };
}

pub(crate) use object;
