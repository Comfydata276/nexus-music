//! Jams and linked devices: what the apps have to read and say alike.
//!
//! A link pasted from the relay's log or from a friend, and how a song is described to the
//! others in a jam. Talking to the relay, and steering a player, are each app's own.

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
