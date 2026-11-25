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

/// The playlist the secret is kept in. A name the app keeps for itself (`step::reserved`).
pub const PLAYLIST_NAME: &str = "Nexus Music link";
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

/// Why a new secret is to be written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Why {
    /// Made and never written into.
    Empty,
    /// Not a record that can be read.
    Broken,
    /// The playlist used before has gone.
    Gone,
    /// Other accounts could see it. The person is told.
    Public,
}

/// What a fetched playlist means.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Use(Secret),
    /// Write a new secret into it (and keep it private), then look again.
    Write(Why),
    Stop(Stop),
}

/// What to make of the playlist that was fetched. It counts only if it is the one asked
/// for, by id and by name, and the server **states** that it is this account's and that it
/// is private.
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
    let record = read(fetched.comment.as_deref().unwrap_or(""));
    match record {
        // Whatever else is true of it, a later layout is left exactly as it is.
        Record::Later(_) => Verdict::Stop(Stop::Later),
        _ if public => Verdict::Write(Why::Public),
        _ if replace => Verdict::Write(Why::Gone),
        Record::Empty => Verdict::Write(Why::Empty),
        Record::Broken => Verdict::Write(Why::Broken),
        Record::Is(secret) => Verdict::Use(secret),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn fetched(id: &str, comment: Option<&str>) -> Fetched {
        Fetched { id: id.into(), name: PLAYLIST_NAME.into(), owner: Some("tester".into()), public: Some(false), comment: comment.map(str::to_string) }
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
        assert_eq!(judge("m", true, &fetched("m", Some(COMMENT)), "tester"), Verdict::Write(Why::Gone));
        // With none left, one is made. With the one used now somebody else's, it has gone too.
        assert_eq!(choose(&[], "tester", Some("k")), Next::Make);
        assert_eq!(choose(&[listed("k", Some("other")), listed("m", Some("tester"))], "tester", Some("k")), Next::Fetch { id: "m".into(), replace: true });
    }

    #[test]
    fn a_fetched_playlist_counts_only_if_it_is_stated_to_be_ours_and_private() {
        assert_eq!(judge("k", false, &fetched("k", Some(COMMENT)), "Tester"), Verdict::Use(secret()));
        // Not the one asked for, by id or by name.
        assert_eq!(judge("k", false, &fetched("j", Some(COMMENT)), "tester"), Verdict::Stop(Stop::Mismatch));
        assert_eq!(judge("k", false, &Fetched { name: "Renamed".into(), ..fetched("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::Mismatch));
        // Whose it is, and whether it is private, have to be said.
        assert_eq!(judge("k", false, &Fetched { owner: None, ..fetched("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::NotStated));
        assert_eq!(judge("k", false, &Fetched { owner: Some("other".into()), ..fetched("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::Mismatch));
        assert_eq!(judge("k", false, &Fetched { public: None, ..fetched("k", Some(COMMENT)) }, "tester"), Verdict::Stop(Stop::NotStated));
        // One that others could see is given a new secret, whatever it holds.
        assert_eq!(judge("k", false, &Fetched { public: Some(true), ..fetched("k", Some(COMMENT)) }, "tester"), Verdict::Write(Why::Public));
    }

    #[test]
    fn an_empty_or_broken_record_of_ours_is_repaired_and_a_later_one_left_alone() {
        assert_eq!(judge("k", false, &fetched("k", None), "tester"), Verdict::Write(Why::Empty));
        assert_eq!(judge("k", false, &fetched("k", Some("")), "tester"), Verdict::Write(Why::Empty));
        assert_eq!(judge("k", false, &fetched("k", Some("someone's note")), "tester"), Verdict::Write(Why::Broken));
        let later = r#"{"nexus_music_link":2,"secret":"x"}"#;
        assert_eq!(judge("k", false, &fetched("k", Some(later)), "tester"), Verdict::Stop(Stop::Later));
        // Not even to make it private, or because another has gone.
        assert_eq!(judge("k", true, &Fetched { public: Some(true), ..fetched("k", Some(later)) }, "tester"), Verdict::Stop(Stop::Later));
    }
}
