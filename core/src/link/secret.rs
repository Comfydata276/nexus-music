//! The secret the devices of an account share, and the playlist it is kept in.
//!
//! It is 32 random bytes in the comment of a playlist of the account's own, named
//! [`PLAYLIST_NAME`]. Any device signed in to the account can read it, and nothing else is
//! needed to be one of the person's devices: that is the design's decision, not an
//! oversight (see `docs/linking-design.md`, section 1).
//!
//! **A look** is how a device gets it. The app does the asking; what to make of each
//! answer is decided here:
//!
//! 1. List the playlists and ask [`choose`], with the id it used last time under this
//!    sign-in (`None` the first time).
//! 2. [`Next::Fetch`]: fetch that playlist and ask [`judge`]. [`Next::Make`]: make one,
//!    write a new secret into it, and start again from 1 with the new id as the one used.
//! 3. [`Verdict::Use`]: that is the secret. [`Verdict::Write`]: write a new secret into that
//!    playlist, keeping it private, and start again from 1 with its id as the one used.
//!    Either way the secret used is the one **a look read**, never the one this device
//!    wrote: two devices that write at the same moment then agree at their next look.
//! 4. [`Verdict::Stop`] and [`Next::Stop`]: this look found nothing to use. Whatever
//!    secret the device already holds is neither renewed nor replaced by it.
//!
//! A look that goes round more than three times has met something that keeps undoing it
//! and is given up until the next.

use std::fmt;

use zeroize::Zeroizing;

use super::text;
use crate::step::{Found, Stated, find_named, ours};

/// The playlist the secret is kept in. A name the app keeps for itself, where linking is on.
pub const PLAYLIST_NAME: &str = "Nexus Music link";

/// Whether a playlist of the person's own may have this name: not this one, **where
/// linking is in use**. An app in which it is not has no business with the name.
pub fn reserved(name: &str) -> bool {
    name.trim() == PLAYLIST_NAME || name.trim() == SETTING_UP
}

/// What a playlist is called for the moment between being made and being given the record
/// (see [`judge`]). **The name itself is only ever given together with the record**, in
/// one request: so a playlist that has the name and no record was not left by the app,
/// whatever happened to the app in between, and is the person's own.
pub const SETTING_UP: &str = "Nexus Music link (being set up)";

/// What the app says when a playlist of the person's own is given the name.
pub const RESERVED: &str = "Nexus Music keeps that name for itself, for linking your devices. Choose another.";
/// The most times one look goes round.
pub const ROUNDS: usize = 3;

const MARK: &str = "nexus_music_link";
const LAYOUT: u64 = 1;

/// The secret. It is wiped when dropped, and says nothing of itself when printed.
#[derive(Clone, PartialEq)]
pub struct Secret(Zeroizing<[u8; 32]>);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Secret {
    /// A new secret, from 32 bytes the system's source of randomness gave. An app that
    /// cannot get them makes no secret at all.
    pub fn new(random: [u8; 32]) -> Secret {
        Secret(Zeroizing::new(random))
    }

    /// The comment that holds it.
    pub fn comment(&self) -> String {
        format!(r#"{{"{MARK}":{LAYOUT},"secret":"{}"}}"#, text::base64(self.0.as_slice()))
    }

    pub(super) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// What the playlist's comment turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// The empty text: a device made the playlist and stopped before writing into it.
    Empty,
    /// Not a record of this kind, or one with no secret in it that can be read.
    Broken,
    /// A record in a later layout than this version knows. Never written over.
    Later(u64),
    Is(Secret),
}

/// Why a playlist of the person's own may not have this name, if it may not. Each feature
/// keeps a name to itself with its own words, and linking only where it is on (`linking`):
/// with it off, nothing of it exists, and a playlist of its name is the person's own.
pub fn kept_name(name: &str, linking: bool) -> Option<&'static str> {
    if crate::step::reserved(name) {
        Some(crate::step::RESERVED)
    } else if linking && reserved(name) {
        Some(RESERVED)
    } else {
        None
    }
}

/// Whether a playlist of this name is left out of every ordinary listing of playlists:
/// the one devices are kept in step through, always; and, where linking is on, the one
/// that holds the secret. What is not listed is not opened, and a page that is not opened
/// is not written to an app's cache of pages.
pub fn unlisted(name: &str, linking: bool) -> bool {
    name == crate::sync::PLAYLIST_NAME || (linking && (name == PLAYLIST_NAME || name == SETTING_UP))
}

/// Whether a playlist's page may be written to an app's cache of pages on disk: never the
/// one devices are kept in step through, whose comment is not a description; and never one
/// whose comment is a linking record, **whatever the playlist is called**, and whether or
/// not linking is on in this copy (it may be on in another, and what that one keeps there
/// is a secret). A playlist renamed somewhere else (on the music server's own page, in
/// another app) is an ordinary playlist to every listing, and its comment is still the
/// record: a page that holds one is not written, and one found on disk is thrown away
/// unread.
///
/// **By what it holds, and by nothing else.** A playlist that only has linking's name,
/// with songs of the person's own or a description of their own, is theirs: its page is
/// kept as any other is. So is one whose description merely looks like a record.
pub fn page_may_be_kept(playlist: &str, comment: &str) -> bool {
    playlist != crate::sync::PLAYLIST_NAME && !is_record(comment)
}

/// Whether a comment is a linking record: whether it **reads as one**. That is the mark
/// with a layout this version knows and a well-formed secret; or the mark with a later
/// layout, whose secret this version cannot know the form of and will not put on a disk.
/// A description that only has the mark's name in it (with nothing behind it, with no
/// secret, with a secret that is not one) is a description.
pub fn is_record(comment: &str) -> bool {
    matches!(read(comment), Record::Is(_) | Record::Later(_))
}

/// Reads the comment. The layout number is read first, by itself: a later layout is
/// [`Record::Later`] whatever else it holds.
pub fn read(comment: &str) -> Record {
    if comment.is_empty() {
        return Record::Empty;
    }
    let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(comment) else { return Record::Broken };
    match fields.get(MARK).and_then(|layout| layout.as_u64()) {
        None | Some(0) => Record::Broken,
        Some(layout) if layout > LAYOUT => Record::Later(layout),
        Some(_) => match fields.get("secret").and_then(|secret| secret.as_str()).and_then(text::from_base64).and_then(|bytes| <[u8; 32]>::try_from(bytes).ok()) {
            Some(bytes) => Record::Is(Secret::new(bytes)),
            None => Record::Broken,
        },
    }
}

/// A playlist as the server gave it when asked for that one. What an answer left out is
/// `None`, which is never taken to mean "no" or "empty" except where said.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fetched {
    pub id: String,
    pub name: String,
    pub owner: Option<String>,
    /// Whether other accounts on the server can see it.
    pub public: Option<bool>,
    /// Navidrome leaves an empty comment out of its answer, so with the owner stated a
    /// missing comment is an empty one.
    pub comment: Option<String>,
    /// How many songs it holds. The record has none.
    pub songs: Option<u64>,
}

/// A playlist as one XML answer to "fetch this playlist" gives it: what it is, whose,
/// whether it is public, and its comment, **all from the one answer**.
///
/// It has to be the XML answer. Navidrome leaves `public` out of a JSON answer when it is
/// false, as it does an empty comment, so a JSON answer cannot **state** that a playlist is
/// private; its XML always writes the attribute. And it has to be one answer: with the
/// comment from one and the privacy from another, a secret that was replaced between the
/// two would be renewed on the strength of an answer about its successor.
///
/// `None` unless the answer is an "ok" one with a playlist in it. What the playlist's tag
/// leaves out is left out here, and [`judge`] decides what that means.
pub fn fetched(xml: &str) -> Option<Fetched> {
    if !attributes(xml, "subsonic-response")?.into_iter().any(|(name, value)| name == "status" && value == "ok") {
        return None;
    }
    let playlist = attributes(xml, "playlist")?;
    let said = |wanted: &str| playlist.iter().find(|(name, _)| *name == wanted).map(|(_, value)| unescaped(value));
    Some(Fetched {
        id: said("id")?,
        name: said("name").unwrap_or_default(),
        owner: said("owner"),
        public: match said("public").as_deref() {
            Some("true") => Some(true),
            Some("false") => Some(false),
            _ => None,
        },
        comment: said("comment"),
        songs: said("songCount").and_then(|count| count.parse().ok()),
    })
}

/// An attribute's value as the text it stands for.
fn unescaped(value: &str) -> String {
    let mut text = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        text.push_str(&rest[..at]);
        rest = &rest[at..];
        let named = rest.find(';').filter(|end| *end <= 10).and_then(|end| {
            let name = &rest[1..end];
            let c = match name {
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                _ => match name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok().and_then(char::from_u32),
                    None => name.strip_prefix('#').and_then(|digits| digits.parse().ok()).and_then(char::from_u32),
                },
            }?;
            Some((c, end))
        });
        match named {
            Some((c, end)) => {
                text.push(c);
                rest = &rest[end + 1..];
            }
            // Not a reference this knows: the text is as written.
            None => {
                text.push('&');
                rest = &rest[1..];
            }
        }
    }
    text.push_str(rest);
    text
}

/// The attributes of the first `<element ...>` in an XML text, as written (not unescaped:
/// the ones read here are an id, "ok", and true or false). `None` if the tag is not there
/// or is not well formed.
fn attributes<'a>(xml: &'a str, element: &str) -> Option<Vec<(&'a str, &'a str)>> {
    let open = format!("<{element}");
    let mut from = 0;
    let mut rest = loop {
        let at = xml.get(from..)?.find(&open)? + from;
        let after = xml.get(at + open.len()..)?;
        // (Not an element whose name merely begins the same way.)
        if after.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            break after;
        }
        from = at + open.len();
    };
    let mut found = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.starts_with('>') || rest.starts_with("/>") {
            return Some(found);
        }
        let (name, after) = rest.split_once('=')?;
        let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let (value, after) = after.get(1..)?.split_once(quote)?;
        found.push((name.trim(), value));
        rest = after;
    }
}

/// Why a look came to nothing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Stop {
    /// The server did not say whose a playlist is, or whether it is private. Such a server
    /// cannot be used for linking.
    NotStated,
    /// The record is in a later layout: this version of the app is too old to link.
    Later,
    /// The answer was not about the playlist that was asked for, or it is not ours after
    /// all. Nothing is concluded from it.
    Mismatch,
    /// The playlist of that name has songs in it: it is a playlist of the person's own,
    /// made before linking kept the name, and is left exactly as it is. Linking does not
    /// start until it has been renamed or deleted, and the person is told so.
    HasSongs,
    /// The playlist of that name holds no record: empty, or with a description of the
    /// person's own. It is theirs as much as one with songs in it, and is left exactly as
    /// it is: nothing is written into it and it is not made private. Linking does not
    /// start until it has been renamed or deleted, and the person is told so.
    Taken,
}

/// What to do after listing the playlists.
#[derive(Debug, Clone, PartialEq)]
pub enum Next {
    /// Fetch this one. `replace` says the playlist used before has gone: whatever this
    /// one holds, it is to be given a new secret, not taken up with its old one.
    Fetch { id: String, replace: bool },
    /// There is none: make one.
    Make,
    Stop(Stop),
}

/// Which playlist holds the secret: the lowest id among those of the name that the server
/// says are this account's, looked for afresh at every look (two devices that each made
/// one agree on the lower). `used` is the id this device used last under this sign-in.
pub fn choose(listed: &[Stated], username: &str, used: Option<&str>) -> Next {
    match find_named(listed, username, PLAYLIST_NAME) {
        Found::NotStated => Next::Stop(Stop::NotStated),
        Found::None => Next::Make,
        Found::Ours(id) => {
            // The one used before is no longer there to be used. A playlist of the same
            // name that is still there may hold a secret from before a reset: it is not
            // fallen back on.
            let gone = used.is_some_and(|used| used != id && !listed.iter().any(|playlist| playlist.id == used && playlist.name == PLAYLIST_NAME && ours(playlist, username)));
            Next::Fetch { id, replace: gone }
        }
    }
}

/// Why a new secret was written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Why {
    /// There was no record: a playlist was made for it.
    Made,
    /// The playlist used before has gone.
    Gone,
    /// Other accounts could see it. The person is told.
    Public,
    /// The person asked for a new one ("Reset linking").
    Reset,
}

/// What a fetched playlist means.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Use(Secret),
    /// Write a new secret into it (and keep it private), then look again. Only ever said
    /// of a playlist that holds a record.
    Write(Why),
    Stop(Stop),
}

/// What to make of the playlist that was fetched. It counts only if it is the one asked
/// for, by id and by name, and the server **states** that it is this account's and that it
/// is private.
///
/// **Nothing is ever written into a playlist that does not hold a record.** One of the
/// name with songs in it, with a description of the person's own, or with nothing in it at
/// all, is theirs: the app never leaves such a one behind (the name is given with the
/// record, in one request: [`SETTING_UP`]), so it did not make it.
pub fn judge(asked: &str, replace: bool, fetched: &Fetched, username: &str) -> Verdict {
    if fetched.id != asked || fetched.name != PLAYLIST_NAME {
        return Verdict::Stop(Stop::Mismatch);
    }
    match &fetched.owner {
        None => return Verdict::Stop(Stop::NotStated),
        Some(owner) if owner.to_lowercase() != username.to_lowercase() => return Verdict::Stop(Stop::Mismatch),
        Some(_) => {}
    }
    let Some(public) = fetched.public else { return Verdict::Stop(Stop::NotStated) };
    // The record is a playlist with no songs. One with songs in it is somebody's playlist:
    // nothing is written into it, its comment is not taken for a secret, and it is not
    // made private. Whether it has any has to be said, like whose it is.
    match fetched.songs {
        None => return Verdict::Stop(Stop::NotStated),
        Some(songs) if songs > 0 => return Verdict::Stop(Stop::HasSongs),
        Some(_) => {}
    }
    let record = read(fetched.comment.as_deref().unwrap_or(""));
    match record {
        // Whatever else is true of it, a later layout is left exactly as it is.
        Record::Later(_) => Verdict::Stop(Stop::Later),
        // No record: not the app's to touch, public or private, whatever has gone.
        Record::Empty | Record::Broken => Verdict::Stop(Stop::Taken),
        Record::Is(_) if public => Verdict::Write(Why::Public),
        Record::Is(_) if replace => Verdict::Write(Why::Gone),
        Record::Is(secret) => Verdict::Use(secret),
    }
}

/// The playlist a look that could not finish left being set up, if there is one: the
/// lowest id among those of that name that the server says are this account's.
pub fn left_being_set_up(listed: &[Stated], username: &str) -> Option<String> {
    match find_named(listed, username, SETTING_UP) {
        Found::Ours(id) => Some(id),
        _ => None,
    }
}

/// Whether that playlist, as fetched, is one to carry on with: the one asked for, by id
/// and by its name, stated to be this account's, with no songs in it and nothing written
/// in it. Anything else (somebody has put songs or a description in it) is left alone, and
/// another is made.
pub fn may_be_set_up(asked: &str, fetched: &Fetched, username: &str) -> bool {
    fetched.id == asked
        && fetched.name == SETTING_UP
        && fetched.owner.as_ref().is_some_and(|owner| owner.to_lowercase() == username.to_lowercase())
        && fetched.songs == Some(0)
        && read(fetched.comment.as_deref().unwrap_or("")) == Record::Empty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_the_app_keeps_for_itself() {
        // Not listed: the sync record's always; linking's where linking is on. With it
        // off, a playlist of that name is the person's own and is listed like any other.
        assert!(unlisted("Nexus Music sync", false) && unlisted("Nexus Music sync", true));
        assert!(unlisted("Nexus Music link", true) && !unlisted("Nexus Music link", false));
        assert!(!unlisted("Morning mix", true));
        // Not to be given to a playlist of the person's own, each with its own words.
        assert_eq!(kept_name("Nexus Music link", false), None);
        assert_eq!(kept_name("Nexus Music link", true), Some(RESERVED));
        assert_eq!(kept_name("Nexus Music sync", false), Some(crate::step::RESERVED));
        assert_eq!(kept_name("Nexus Music sync", true), Some(crate::step::RESERVED));
        assert_eq!(kept_name("Morning mix", true), None);
        // Never on disk, by what it holds, whether linking is on here or not.
        assert!(page_may_be_kept("Morning mix", "For the walk to work"));
        for (name, comment) in [("Nexus Music link", COMMENT), ("Nexus Music sync", ""), ("Renamed elsewhere", COMMENT), ("A later layout", r#"{"nexus_music_link":2,"key":"…"}"#)] {
            assert!(!page_may_be_kept(name, comment), "{name}");
        }
    }

    #[test]
    fn a_comment_is_a_record_only_if_it_reads_as_one() {
        // The mark and a well-formed secret; or a later layout, whose secret this version
        // cannot know the form of and so will not put on a disk.
        let spaced = format!(" {COMMENT} ");
        for record in [COMMENT, spaced.as_str(), r#"{"nexus_music_link":2,"key":"later"}"#, r#"{"nexus_music_link":9}"#] {
            assert!(is_record(record), "{record}");
        }
        // A description that only looks like one is somebody's description: a key of that
        // name with nothing behind it, a layout that is no layout, a secret that is not one.
        let others = [
            "",
            "For the walk to work",
            r#"{"nexus_music_sync":1}"#,
            r#"["nexus_music_link"]"#,
            r#""nexus_music_link""#,
            "{nexus_music_link",
            r#"{"nexus_music_link":null,"secret":7}"#,
            r#"{"nexus_music_link":null}"#,
            r#"{"nexus_music_link":0,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#,
            r#"{"nexus_music_link":"1","secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#,
            r#"{"nexus_music_link":1}"#,
            r#"{"nexus_music_link":1,"secret":"x"}"#,
            r#"{"nexus_music_link":1,"secret":"ICEiIyQl"}"#,
            r#"{"nexus_music_link":1,"secret":7}"#,
        ];
        for other in others {
            assert!(!is_record(other), "{other}");
        }
        // And a page is kept or not by that alone, for every playlist but the one devices
        // are kept in step through: an ordinary playlist that has the name, and one whose
        // description looks like a record and is not, are the person's own, cached as any.
        assert!(!page_may_be_kept("Road trip", COMMENT) && !page_may_be_kept(PLAYLIST_NAME, COMMENT));
        assert!(page_may_be_kept(PLAYLIST_NAME, "") && page_may_be_kept(PLAYLIST_NAME, "Songs for linking arms to"));
        assert!(page_may_be_kept("Road trip", r#"{"nexus_music_link":null}"#) && page_may_be_kept("Road trip", ""));
        assert!(!page_may_be_kept(crate::sync::PLAYLIST_NAME, ""));
    }

    const COMMENT: &str = r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#;

    fn secret() -> Secret {
        let mut bytes = [0u8; 32];
        for (at, byte) in bytes.iter_mut().enumerate() {
            *byte = 0x20 + at as u8;
        }
        Secret::new(bytes)
    }

    fn listed(id: &str, owner: Option<&str>) -> Stated {
        Stated { id: id.into(), name: PLAYLIST_NAME.into(), owner: owner.map(str::to_string), comment: None }
    }

    fn got(id: &str, comment: Option<&str>) -> Fetched {
        Fetched { id: id.into(), name: PLAYLIST_NAME.into(), owner: Some("tester".into()), public: Some(false), comment: comment.map(str::to_string), songs: Some(0) }
    }

    #[test]
    fn each_feature_keeps_its_own_name_with_its_own_words() {
        assert!(reserved("Nexus Music link") && reserved(" Nexus Music link ") && !reserved("Nexus Music sync") && !reserved("Morning mix"));
        // The record that keeps devices in step has nothing to say about this name, nor this
        // about that one: an app with linking off refuses only the first.
        assert!(crate::step::reserved("Nexus Music sync") && !crate::step::reserved("Nexus Music link"));
        assert!(RESERVED.contains("linking") && crate::step::RESERVED.contains("in step"));
    }

    #[test]
    fn the_comment_is_written_and_read_one_way() {
        assert_eq!(secret().comment(), COMMENT);
        assert_eq!(read(COMMENT), Record::Is(secret()));
        assert_eq!(format!("{:?}", secret()), "Secret(..)");
        // Fields this version doesn't know are no obstacle; the order is not either.
        assert_eq!(read(r#"{"note":"x","secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8","nexus_music_link":1}"#), Record::Is(secret()));
    }

    #[test]
    fn what_is_not_a_record_is_told_from_one_that_is_later() {
        assert_eq!(read(""), Record::Empty);
        for broken in [" ", "my notes", "[]", "7", "{}", r#"{"nexus_music_sync":1}"#, r#"{"nexus_music_link":"1","secret":"x"}"#, r#"{"nexus_music_link":0}"#, r#"{"nexus_music_link":1}"#, r#"{"nexus_music_link":1,"secret":7}"#, r#"{"nexus_music_link":1,"secret":"ICEi"}"#, r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8="}"#, r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj9"}"#] {
            assert_eq!(read(broken), Record::Broken, "{broken}");
        }
        // A later layout is later whatever else it holds, even nothing this version can read.
        assert_eq!(read(r#"{"nexus_music_link":2}"#), Record::Later(2));
        assert_eq!(read(r#"{"nexus_music_link":9,"secret":["new shape"]}"#), Record::Later(9));
    }

    #[test]
    fn the_playlist_is_the_lowest_of_ours_and_only_if_the_server_says_whose() {
        assert_eq!(choose(&[], "tester", None), Next::Make);
        assert_eq!(choose(&[listed("b", Some("Tester")), listed("a", Some("someone")), listed("c", Some("tester"))], "tester", None), Next::Fetch { id: "b".into(), replace: false });
        // One of the name whose owner is not stated may be anybody's: nothing is made beside it.
        assert_eq!(choose(&[listed("a", None)], "tester", None), Next::Stop(Stop::NotStated));
        assert_eq!(choose(&[listed("a", None), listed("b", Some("tester"))], "tester", None), Next::Fetch { id: "b".into(), replace: false });
        // Another list's name is nothing to do with it.
        let other = Stated { id: "a".into(), name: "Nexus Music sync".into(), owner: Some("tester".into()), comment: None };
        assert_eq!(choose(&[other], "tester", None), Next::Make);
    }

    #[test]
    fn two_devices_that_each_made_one_agree_on_the_lower() {
        let both = [listed("m", Some("tester")), listed("k", Some("tester"))];
        // The one that made "m" moves to "k": "m" is still there, so nothing has gone.
        assert_eq!(choose(&both, "tester", Some("m")), Next::Fetch { id: "k".into(), replace: false });
        assert_eq!(choose(&both, "tester", Some("k")), Next::Fetch { id: "k".into(), replace: false });
    }

    #[test]
    fn a_playlist_that_has_gone_is_never_replaced_by_another_s_old_secret() {
        // "k" was used and has been deleted. "m" is still there, with whatever it held.
        let left = [listed("m", Some("tester"))];
        assert_eq!(choose(&left, "tester", Some("k")), Next::Fetch { id: "m".into(), replace: true });
        assert_eq!(judge("m", true, &got("m", Some(COMMENT)), "tester"), Verdict::Write(Why::Gone));
        // With none left, one is made. With the one used now somebody else's, it has gone too.
        assert_eq!(choose(&[], "tester", Some("k")), Next::Make);
        assert_eq!(choose(&[listed("k", Some("other")), listed("m", Some("tester"))], "tester", Some("k")), Next::Fetch { id: "m".into(), replace: true });
    }

    #[test]
    fn a_fetched_playlist_counts_only_if_it_is_stated_to_be_ours_and_private() {
        assert_eq!(judge("k", false, &got("k", Some(COMMENT)), "Tester"), Verdict::Use(secret()));
        // Not the one asked for, by id or by name.
        assert_eq!(judge("k", false, &got("j", Some(COMMENT)), "tester"), Verdict::Stop(Stop::Mismatch));
        assert_eq!(judge("k", false, &Fetched { name: "Renamed".into(), ..got("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::Mismatch));
        // Whose it is, and whether it is private, have to be said.
        assert_eq!(judge("k", false, &Fetched { owner: None, ..got("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::NotStated));
        assert_eq!(judge("k", false, &Fetched { owner: Some("other".into()), ..got("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::Mismatch));
        assert_eq!(judge("k", false, &Fetched { public: None, ..got("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::NotStated));
        // One that others could see is given a new secret, whatever it holds.
        assert_eq!(judge("k", false, &Fetched { public: Some(true), ..got("k", Some(COMMENT)) }, "tester"), Verdict::Write(Why::Public));
    }

    /// What Navidrome answers, private and public.
    fn answer(public: &str) -> String {
        format!(r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1" type="navidrome" serverVersion="0.58.0" openSubsonic="true"><playlist id="7K5dLzMvMcCEj4us2t4RO0" name="Nexus Music link" comment="{{&#34;nexus_music_link&#34;:1,&#34;secret&#34;:&#34;ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8&#34;}}" songCount="0" duration="0" public="{public}" owner="tester" created="2026-11-25T07:05:11Z" changed="2026-11-25T07:05:11Z"></playlist></subsonic-response>"#)
    }

    #[test]
    fn a_playlist_is_read_whole_from_the_one_answer_that_says_whether_it_is_public() {
        let private = fetched(&answer("false")).unwrap();
        assert_eq!(private, Fetched { id: "7K5dLzMvMcCEj4us2t4RO0".into(), name: PLAYLIST_NAME.into(), owner: Some("tester".into()), public: Some(false), comment: Some(COMMENT.into()), songs: Some(0) });
        // And so judged: the secret in it is the one to use.
        assert_eq!(judge("7K5dLzMvMcCEj4us2t4RO0", false, &private, "tester"), Verdict::Use(secret()));
        assert_eq!(judge("7K5dLzMvMcCEj4us2t4RO0", false, &fetched(&answer("true")).unwrap(), "tester"), Verdict::Write(Why::Public));
        // Not saying, or saying something else, it states nothing, and nothing is used.
        for unsaid in [answer("false").replace(r#" public="false""#, ""), answer("no")] {
            let read = fetched(&unsaid).unwrap();
            assert_eq!((read.public, judge(&read.id, false, &read, "tester")), (None, Verdict::Stop(Stop::NotStated)));
        }
        // An empty comment is left out, as Navidrome leaves it; so is an owner it won't state.
        let bare = fetched(r#"<subsonic-response status="ok"><playlist id="k" name="Nexus Music link" songCount="0" public="false" owner="tester"/></subsonic-response>"#).unwrap();
        assert_eq!((bare.comment.clone(), judge("k", false, &bare, "tester")), (None, Verdict::Stop(Stop::Taken)));
        assert_eq!(fetched(r#"<subsonic-response status="ok"><playlist id='k' public='true'/></subsonic-response>"#).map(|read| (read.owner, read.public)), Some((None, Some(true))));
    }

    #[test]
    fn an_answer_that_is_not_an_ok_one_about_a_playlist_gives_nothing() {
        assert_eq!(fetched(&answer("false").replace(r#"status="ok""#, r#"status="failed""#)), None);
        assert_eq!(fetched(r#"<subsonic-response status="failed"><error code="70" message="playlist not found"/></subsonic-response>"#), None);
        assert_eq!(fetched(r#"<subsonic-response status="ok"></subsonic-response>"#), None);
        assert_eq!(fetched(r#"<subsonic-response status="ok"><playlist name="Nexus Music link"/></subsonic-response>"#), None);
        // The element is the playlist, not one whose name begins the same way.
        assert_eq!(fetched(r#"<subsonic-response status="ok"><playlists><playlist id="k" public="false"/></playlists></subsonic-response>"#).map(|read| read.id), Some("k".into()));
        // Cut short or not XML at all, it gives nothing and does not trip anything up.
        for broken in ["", "<playlist", "<playlist id=", r#"<playlist id="k"#, r#"<subsonic-response status="ok"><playlist id="k" public="false""#, "{\"playlist\":{}}", "<playlist é=\"é\">"] {
            assert_eq!(fetched(broken), None, "{broken}");
        }
    }

    #[test]
    fn what_a_name_or_a_comment_says_is_inside_its_own_quotes() {
        // Text made to look like an attribute, in a name and in a comment.
        let sly = r#"<subsonic-response status="ok"><playlist id="k" name="x public=&quot;false&quot; owner=&quot;tester&quot;" comment=' public="false" > ' owner="other"/></subsonic-response>"#;
        let read = fetched(sly).unwrap();
        assert_eq!((read.public, read.owner.as_deref(), read.comment.as_deref()), (None, Some("other"), Some(r#" public="false" > "#)));
        assert_eq!(read.name, r#"x public="false" owner="tester""#);
        // References are read as what they stand for, each way they can be written, and
        // what is not a reference is left as it is.
        for (written, meant) in [("a&amp;b", "a&b"), ("&lt;&gt;&quot;&apos;", "<>\"'"), ("&#34;&#x22;&#X22;", "\"\"\""), ("fish &chips; & more", "fish &chips; & more"), ("&#xD83D;", "&#xD83D;"), ("&", "&"), ("caf&#233;", "café")] {
            assert_eq!(unescaped(written), meant, "{written}");
        }
    }

    /// Switching linking on must not take over a playlist the person made themselves and
    /// happened to call by the name: overwrite its description and hide it from others.
    #[test]
    fn a_playlist_with_songs_in_it_is_never_taken_for_the_record() {
        let theirs = |comment: Option<&str>, public: bool| Fetched { songs: Some(12), public: Some(public), ..got("k", comment) };
        // Whatever its description says, and whether or not others can see it, it is left
        // exactly as it is: nothing is written, and nothing in it is used.
        for comment in [None, Some(""), Some("songs for the car"), Some(COMMENT)] {
            for public in [false, true] {
                for replace in [false, true] {
                    assert_eq!(judge("k", replace, &theirs(comment, public), "tester"), Verdict::Stop(Stop::HasSongs), "{comment:?}, public {public}, replace {replace}");
                }
            }
        }
        // How many songs it holds has to be said, as whose it is has to be.
        assert_eq!(judge("k", false, &Fetched { songs: None, ..got("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::NotStated));
        // With none, it is the record, as before.
        assert_eq!(judge("k", false, &got("k", Some(COMMENT)), "tester"), Verdict::Use(secret()));
        // And the number is read from the answer the rest is read from.
        assert_eq!(fetched(&answer("false")).unwrap().songs, Some(0));
        assert_eq!(fetched(&answer("false").replace(r#"songCount="0""#, r#"songCount="12""#)).unwrap().songs, Some(12));
        assert_eq!(fetched(&answer("false").replace(r#" songCount="0""#, "")).unwrap().songs, None);
        assert_eq!(fetched(&answer("false").replace(r#"songCount="0""#, r#"songCount="many""#)).unwrap().songs, None);
    }

    /// A playlist of the name that holds no record is the person's own, with songs in it
    /// or without: nothing is written into it, whatever else is true.
    #[test]
    fn a_playlist_that_holds_no_record_is_never_written_into_and_a_later_layout_is_left_alone() {
        let not_records = [None, Some(""), Some("someone's note"), Some(r#"{"nexus_music_link":null}"#), Some(r#"{"nexus_music_link":1}"#), Some(r#"{"nexus_music_link":1,"secret":"short"}"#)];
        for comment in not_records {
            for public in [false, true] {
                for replace in [false, true] {
                    assert_eq!(judge("k", replace, &Fetched { public: Some(public), ..got("k", comment) }, "tester"), Verdict::Stop(Stop::Taken), "{comment:?}, public {public}, replace {replace}");
                }
            }
        }
        // The name is kept from a playlist of the person's own where linking is in use, and
        // so is the one a playlist has for the moment it is being set up.
        assert_eq!((kept_name(SETTING_UP, true), kept_name(SETTING_UP, false), unlisted(SETTING_UP, true), unlisted(SETTING_UP, false)), (Some(RESERVED), None, true, false));
        // One left being set up is carried on with only as the app left it: its own, by
        // that name, with nothing in it.
        let left = |name: &str, owner: Option<&str>| Stated { id: "s".into(), name: name.into(), owner: owner.map(str::to_string), comment: None };
        assert_eq!(left_being_set_up(&[left(SETTING_UP, Some("tester")), listed("k", Some("tester"))], "tester"), Some("s".into()));
        assert_eq!(left_being_set_up(&[left(SETTING_UP, Some("other")), left(SETTING_UP, None), left("Morning mix", Some("tester"))], "tester"), None);
        let as_left = Fetched { id: "s".into(), name: SETTING_UP.into(), owner: Some("Tester".into()), public: Some(true), comment: None, songs: Some(0) };
        assert!(may_be_set_up("s", &as_left, "tester"));
        for touched in [
            Fetched { songs: Some(1), ..as_left.clone() },
            Fetched { songs: None, ..as_left.clone() },
            Fetched { comment: Some("mine now".into()), ..as_left.clone() },
            Fetched { comment: Some(COMMENT.into()), ..as_left.clone() },
            Fetched { owner: Some("other".into()), ..as_left.clone() },
            Fetched { owner: None, ..as_left.clone() },
            Fetched { name: PLAYLIST_NAME.into(), ..as_left.clone() },
            Fetched { id: "t".into(), ..as_left.clone() },
        ] {
            assert!(!may_be_set_up("s", &touched, "tester"), "{touched:?}");
        }
        let later = r#"{"nexus_music_link":2,"secret":"x"}"#;
        assert_eq!(judge("k", false, &got("k", Some(later)), "tester"), Verdict::Stop(Stop::Later));
        // Not even to make it private, or because another has gone.
        assert_eq!(judge("k", true, &Fetched { public: Some(true), ..got("k", Some(later)) }, "tester"), Verdict::Stop(Stop::Later));
    }
}
