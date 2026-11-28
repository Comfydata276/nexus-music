//! The page-only listening query. Cursor contents are private to the backend.

use serde::Serialize;
use serde_json::value::RawValue;

use super::{AccountId, Name, RequestHead, ResultBody};
use crate::nexus::events::text_ok;
use crate::nexus::wire::{self, BadMessage, Fields};

pub const PATH: &str = "/nexus/api/admin/listening";
pub const PAGE_SIZE: usize = 25;
pub const TOP_SIZE: usize = 10;
const CURSOR_BYTES: usize = 192;
const DAY_MS: u64 = 86_400_000;

// The result contains private listening and account data. Its wire types omit Debug.
macro_rules! object {
    ($name:ident { $($field:ident: $kind:ty),+ $(,)? }) => {
        #[derive(Serialize)]
        pub struct $name { $(pub $field: $kind,)+ }

        impl wire::Value for $name {
            fn read(raw: &RawValue) -> Result<Self, BadMessage> {
                let fields = Fields::read(raw)?;
                Ok(Self { $($field: fields.get(stringify!($field))?,)+ })
            }
        }
    };
}

#[derive(Clone, Copy, Serialize)]
#[serde(transparent)]
pub struct Days(u64);

impl Days {
    pub fn new(days: u64) -> Option<Self> {
        matches!(days, 7 | 30 | 90).then_some(Self(days))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn start_ms(self, end_ms: u64) -> u64 {
        end_ms.saturating_sub(self.0 * DAY_MS)
    }
}

impl wire::Value for Days {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::new(wire::plain_json(raw.get())?).ok_or(BadMessage::Form)
    }
}

#[derive(Serialize)]
#[serde(transparent)]
pub struct Cursor(String);

impl Cursor {
    pub fn new(text: String) -> Option<Self> {
        (!text.is_empty()
            && text.len() <= CURSOR_BYTES
            && text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')))
        .then_some(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl wire::Value for Cursor {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::new(wire::plain_json(raw.get())?).ok_or(BadMessage::Form)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Page {
    People(Cursor),
    Recent(Cursor),
}

impl wire::Value for Page {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        match Fields::read(raw)?.select(&["people", "recent"])? {
            ("people", raw) => Cursor::read(raw).map(Self::People),
            ("recent", raw) => Cursor::read(raw).map(Self::Recent),
            _ => Err(BadMessage::Form),
        }
    }
}

object! { Request { head: RequestHead, account: Option<AccountId>, days: Days, page: Option<Page> } }
impl wire::Message for Request {
    const MAX_BYTES: usize = super::REQUEST_MAX_BYTES;
}

#[derive(Clone, Hash, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Text(String);

impl Text {
    pub fn new(value: String) -> Option<Self> {
        text_ok(&value).then_some(Self(value))
    }
}

impl wire::Value for Text {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::new(wire::plain_json(raw.get())?).ok_or(BadMessage::Form)
    }
}

#[derive(Serialize)]
#[serde(transparent)]
pub struct Rows<T, const N: usize>(Vec<T>);

impl<T, const N: usize> Rows<T, N> {
    pub fn new(rows: Vec<T>) -> Option<Self> {
        (rows.len() <= N).then_some(Self(rows))
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

impl<T: wire::Value, const N: usize> wire::Value for Rows<T, N> {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let rows: Vec<&RawValue> = wire::plain_json(raw.get())?;
        if rows.len() > N {
            return Err(BadMessage::Form);
        }
        rows.into_iter()
            .map(|row| {
                let value = T::read(row)?;
                value.check()?;
                Ok(value)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

object! { Artist { artist: Text, starts: u64 } }
object! { Album { artist: Text, album: Text, starts: u64 } }
object! { Song { artist: Text, album: Text, title: Text, starts: u64 } }
object! { Person { account: AccountId, name: Name, starts: u64 } }
object! { Play { id: u64, account: AccountId, name: Name, at_ms: u64, title: Text, artist: Text, album: Text } }
object! { PeoplePage { rows: Rows<Person, PAGE_SIZE>, next: Option<Cursor> } }
object! { RecentPage { rows: Rows<Play, PAGE_SIZE>, next: Option<Cursor> } }
object! {
    Overview {
        start_ms: u64, end_ms: u64, starts: u64,
        artists: Rows<Artist, TOP_SIZE>, albums: Rows<Album, TOP_SIZE>, songs: Rows<Song, TOP_SIZE>,
        people: PeoplePage, recent: RecentPage,
    }
}

impl ResultBody for Overview { const MAX_BYTES: usize = super::LIST_REPLY_MAX_BYTES; }
impl ResultBody for PeoplePage { const MAX_BYTES: usize = super::LIST_REPLY_MAX_BYTES; }
impl ResultBody for RecentPage { const MAX_BYTES: usize = super::LIST_REPLY_MAX_BYTES; }
