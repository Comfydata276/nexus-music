//! Jams and linked devices: what the apps have to read and say alike.
//!
//! A link pasted from the relay's log or from a friend, how a song is described to the
//! others in a jam, and what a device being steered does for whoever asks. Talking to the
//! relay, and steering a player, are each app's own.

use serde_json::{Value, json};

use crate::model::Song;

/// A relay's address from something pasted: `https://` is assumed, and plain HTTP is only
/// taken for somewhere on the home network, since a key travels with every request.
fn address(text: &str) -> Result<String, String> {
    let text = text.trim().trim_end_matches('/');
    if text.is_empty() || text.contains(char::is_whitespace) {
        return Err("That doesn't look like a link.".into());
    }
    let whole = if text.starts_with("https://") || text.starts_with("http://") { text.to_string() } else { format!("https://{text}") };
    if crate::address::unencrypted_beyond_home(&whole) {
        return Err("That link starts with http://, which others on the way could read. Use the https:// one.".into());
    }
    let host = whole.split_once("://").map_or("", |(_, rest)| rest).split('/').next().unwrap_or("");
    if host.is_empty() {
        return Err("That doesn't look like a link.".into());
    }
    Ok(whole)
}

/// A host link is the relay's address with the host key after a `#`:
/// `https://jam.example.org/#key`. Returns the address and the key.
pub fn parse_host_link(text: &str) -> Result<(String, String), String> {
    // The relay's whole log line may have been pasted: the link is the part with the # in it.
    let text = text.split_whitespace().find(|word| word.contains('#')).unwrap_or(text.trim());
    let Some((before, key)) = text.split_once('#') else {
        return Err("A host link ends with # and a key. Copy the whole link from your relay's log.".into());
    };
    let key = key.trim();
    if key.len() < 16 || key.contains(char::is_whitespace) {
        return Err("The key at the end of that link is too short. Copy the whole link from your relay's log.".into());
    }
    Ok((address(before)?, key.to_string()))
}

/// A jam's link is the relay's address, then `/j/` and the jam: `https://jam.example.org/j/abc`.
/// Returns the address and the jam.
pub fn parse_jam_link(text: &str) -> Result<(String, String), String> {
    let text = text.trim();
    let text = text.split(['?', '#']).next().unwrap_or(text).trim_end_matches('/');
    let Some((before, id)) = text.rsplit_once("/j/") else {
        return Err("That isn't a jam link. One looks like https://jam.example.org/j/abc123.".into());
    };
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("That isn't a jam link. One looks like https://jam.example.org/j/abc123.".into());
    }
    Ok((address(before)?, id.to_string()))
}

/// The part of an address people recognise: `jam.example.org`.
pub fn host_of(address: &str) -> &str {
    address.split_once("://").map_or(address, |(_, rest)| rest).split('/').next().unwrap_or(address)
}

pub fn song_json(song: &Song) -> Value {
    json!({
        "id": song.id,
        "title": song.title,
        "artist": song.artist_label(),
        "album": song.album,
        "cover": song.cover_art,
        "duration": song.duration,
    })
}

/// What a device's session at the relay is, to the app answering what's asked through it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Session {
    /// Open only so the person's other devices can steer this one. Anyone else who reaches
    /// it (its name can turn up wherever the relay's traffic is logged) has no business there.
    Private,
    /// A jam: friends have been given its link.
    Jam,
}

/// What to do about something asked through a session.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Do it. `steer` is whether the one asking may steer: it lets what they add go to the
    /// top of "Next in queue", and lets them take out any song, not only their own.
    Do { steer: bool },
    /// Refuse, saying this. Nothing is looked up, fetched or read for it.
    No(&'static str),
}

/// The refusal that says nothing about why: the one given for what nobody may ask.
pub const NOT_FOR_GUESTS: &str = "That isn't something a guest can do.";

/// What changes what plays, beyond adding and taking out.
const STEERING: [&str; 10] = ["skip", "previous", "pause", "play", "seek", "jump", "move", "clear", "shuffle", "repeat"];
/// What only the person's own devices ask: say hello, hand a queue over or take it, turn the volume.
const OWN_DEVICES: [&str; 4] = ["hello", "queue", "load", "volume"];

/// What may be done about `action` (as the relay names it; `cover` for a cover), asked
/// through a session of the kind `session`. `owner` is whether the asker showed the word
/// that only the person's own devices can work out; `guests_steer` is the host's "Guests
/// can steer". In a private session nobody without the word is given anything.
pub fn answer(session: Session, owner: bool, guests_steer: bool, action: &str) -> Answer {
    let known = matches!(action, "search" | "cover" | "add" | "remove") || STEERING.contains(&action) || OWN_DEVICES.contains(&action);
    if !known {
        return Answer::No(NOT_FOR_GUESTS);
    }
    if owner {
        return Answer::Do { steer: true };
    }
    if session == Session::Private {
        return Answer::No(NOT_FOR_GUESTS);
    }
    match action {
        "search" | "cover" | "add" | "remove" => Answer::Do { steer: guests_steer },
        _ if OWN_DEVICES.contains(&action) => Answer::No("That's for your own devices."),
        _ if guests_steer => Answer::Do { steer: true },
        _ => Answer::No("The host is choosing what plays. You can still add songs."),
    }
}

/// The guests a host has removed from the session it is hosting, by the id the relay gave
/// each. **A guest who has been removed is no longer answered or obeyed**, whatever of
/// theirs was under way: a song they asked for that was still being looked up when they
/// were removed is not queued when it is found. (The relay stops taking their asks; this is
/// for what it had already passed on.) The most lately removed are kept, and no more than
/// a session could have.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Removed(Vec<String>);

/// The most removed guests remembered for one session.
const MOST_REMOVED: usize = 64;

impl Removed {
    pub fn add(&mut self, guest: &str) {
        if guest.is_empty() || self.has(guest) {
            return;
        }
        if self.0.len() >= MOST_REMOVED {
            self.0.remove(0);
        }
        self.0.push(guest.to_string());
    }

    pub fn has(&self, guest: &str) -> bool {
        !guest.is_empty() && self.0.iter().any(|removed| removed == guest)
    }
}

/// Whether what came with an ask shows the owner word `word` (`None` when this device has
/// none to compare with). Compared in full whatever differs, so the time taken says nothing.
pub fn shows_owner(word: Option<&str>, data: &Value) -> bool {
    let (Some(word), Some(shown)) = (word.filter(|word| !word.is_empty()), data["owner"].as_str()) else { return false };
    word.len() == shown.len() && word.bytes().zip(shown.bytes()).fold(0u8, |differ, (a, b)| differ | (a ^ b)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guest_who_has_been_removed_is_known_as_removed_and_the_list_has_a_bound() {
        let mut removed = Removed::default();
        assert!(!removed.has("g1") && !removed.has(""));
        removed.add("g1");
        removed.add("g1");
        removed.add("");
        assert!(removed.has("g1") && !removed.has("g2") && !removed.has("") && removed.0.len() == 1);
        // More than a session could have: the earliest are forgotten, the latest kept.
        for n in 0..1_000 {
            removed.add(&format!("guest-{n}"));
        }
        assert!(removed.0.len() == MOST_REMOVED && removed.has("guest-999") && !removed.has("g1"));
    }

    #[test]
    fn reads_host_links() {
        assert_eq!(parse_host_link(" https://jam.example.org/#abcdefghijklmnop "), Ok(("https://jam.example.org".into(), "abcdefghijklmnop".into())));
        // The whole line from the relay's log, pasted as it stands.
        assert_eq!(parse_host_link("Host link, for Nexus Music's settings (keep it private): https://jam.example.org/#abcdefghijklmnop\n"), Ok(("https://jam.example.org".into(), "abcdefghijklmnop".into())));
        // Under a path on a shared address, and with no scheme typed.
        assert_eq!(parse_host_link("example.org/jam/#abcdefghijklmnop"), Ok(("https://example.org/jam".into(), "abcdefghijklmnop".into())));
        assert_eq!(parse_host_link("http://192.168.1.5:8080/#abcdefghijklmnop"), Ok(("http://192.168.1.5:8080".into(), "abcdefghijklmnop".into())));
        // No key, a key that's too short, and a key that would cross the internet in the clear.
        assert!(parse_host_link("https://jam.example.org/").is_err());
        assert!(parse_host_link("https://jam.example.org/#short").is_err());
        assert!(parse_host_link("http://jam.example.org/#abcdefghijklmnop").is_err());
        assert!(parse_host_link("#abcdefghijklmnop").is_err());
    }

    #[test]
    fn reads_jam_links() {
        assert_eq!(parse_jam_link("https://jam.example.org/j/abc234"), Ok(("https://jam.example.org".into(), "abc234".into())));
        assert_eq!(parse_jam_link(" jam.example.org/j/abc234/ "), Ok(("https://jam.example.org".into(), "abc234".into())));
        assert_eq!(parse_jam_link("https://example.org/jam/j/abc234?from=chat#top"), Ok(("https://example.org/jam".into(), "abc234".into())));
        assert_eq!(parse_jam_link("http://localhost:8080/j/abc234"), Ok(("http://localhost:8080".into(), "abc234".into())));
        // A host link isn't a jam link, and nor is anything else.
        assert!(parse_jam_link("https://jam.example.org/#abcdefghijklmnop").is_err());
        assert!(parse_jam_link("https://jam.example.org/j/").is_err());
        assert!(parse_jam_link("https://jam.example.org/j/../x").is_err());
        assert!(parse_jam_link("http://jam.example.org/j/abc234").is_err());
        assert!(parse_jam_link("abc234").is_err());
        assert_eq!(host_of("https://jam.example.org/sub"), "jam.example.org");
    }

    #[test]
    fn describes_a_song_for_guests() {
        let song = Song { id: "s1".into(), title: "Blue".into(), artist: "A".into(), display_artist: "A & B".into(), album: "Sky".into(), cover_art: "al-1".into(), duration: 185, ..Default::default() };
        assert_eq!(song_json(&song), json!({"id": "s1", "title": "Blue", "artist": "A & B", "album": "Sky", "cover": "al-1", "duration": 185}));
    }

    /// Every kind of ask, in a private session and a jam, with the owner word and without,
    /// with "Guests can steer" on and off.
    #[test]
    fn who_may_ask_what() {
        use Answer::{Do, No};
        let steering = Do { steer: true };
        let held = No("The host is choosing what plays. You can still add songs.");
        let own = No("That's for your own devices.");
        let none = No(NOT_FOR_GUESTS);
        let mut asks: Vec<&str> = vec!["search", "cover", "add", "remove"];
        asks.extend(STEERING);
        asks.extend(OWN_DEVICES);
        asks.push("anything else");
        for action in asks {
            let steers = STEERING.contains(&action);
            let owns = OWN_DEVICES.contains(&action);
            let known = action != "anything else";
            for guests_steer in [false, true] {
                let at = |session, owner| (answer(session, owner, guests_steer, action), (action, session, owner, guests_steer));
                // The person's own devices have the run of a device, private or in a jam.
                for session in [Session::Private, Session::Jam] {
                    let (got, case) = at(session, true);
                    assert_eq!(got, if known { steering } else { none }, "{case:?}");
                }
                // Anyone else gets nothing at all from a private session...
                let (got, case) = at(Session::Private, false);
                assert_eq!(got, none, "{case:?}");
                // ...and in a jam, what a guest gets.
                let (got, case) = at(Session::Jam, false);
                let wanted = match () {
                    _ if !known => none,
                    _ if owns => own,
                    _ if steers && guests_steer => steering,
                    _ if steers => held,
                    _ => Do { steer: guests_steer },
                };
                assert_eq!(got, wanted, "{case:?}");
            }
        }
    }

    #[test]
    fn the_owner_word_is_shown_or_not() {
        let word = "0123456789abcdef0123456789abcdef";
        assert!(shows_owner(Some(word), &json!({"owner": word})));
        assert!(!shows_owner(Some(word), &json!({"owner": "0123456789abcdef0123456789abcdee"})));
        assert!(!shows_owner(Some(word), &json!({"owner": "0123456789abcdef"})));
        assert!(!shows_owner(Some(word), &json!({"owner": ""})));
        assert!(!shows_owner(Some(word), &json!({"owner": 7})));
        assert!(!shows_owner(Some(word), &json!({})));
        assert!(!shows_owner(Some(word), &Value::Null));
        // A device with no word of its own recognises nobody, not even someone showing none.
        assert!(!shows_owner(None, &json!({"owner": word})));
        assert!(!shows_owner(Some(""), &json!({"owner": ""})));
    }
}
