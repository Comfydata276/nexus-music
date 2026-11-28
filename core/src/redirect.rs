//! The music line's verdict on the response to a built request. All other signed
//! lines follow nothing. Only `Follow` supplies a request for another dispatch.

use crate::origin::{Address, Difference, MAX_ADDRESS_BYTES, MusicRequest, Origin};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Operation {
    Read,
    Change,
}

/// The read names used by the Windows and Android music transports (design 5.5).
/// Every mutation and every unrecognized name takes the send-once path.
pub fn operation(name: &str) -> Operation {
    match name {
        "ping"
        | "getAlbum"
        | "getAlbumList2"
        | "getArtist"
        | "getArtistInfo2"
        | "getArtists"
        | "getCoverArt"
        | "getGenres"
        | "getInternetRadioStations"
        | "getLyricsBySongId"
        | "getPlayQueue"
        | "getPlaylist"
        | "getPlaylists"
        | "getRandomSongs"
        | "getScanStatus"
        | "getSimilarSongs2"
        | "getSong"
        | "getStarred2"
        | "getTopSongs"
        | "search3"
        | "stream" => Operation::Read,
        _ => Operation::Change,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Form,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A non-redirect response. The transport still checks its normal status and body rules.
    Answer,
    Follow(MusicRequest),
    Refuse(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Unreadable,
    Downgrade,
    Elsewhere,
    Changed,
    TooMany,
    Form,
    Change,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    detail: Reason,
    target: Option<Origin>,
    // Only a recognizable music base may be suggested. Never store the raw Location.
    path_changed: bool,
    base: Option<String>,
    form: bool,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AccountAddress {
    Typed,
    HandedOver,
}

pub const CHANGE_UNCERTAIN: &str = "Nexus Music can't tell whether that change was made: your server sent the app on to another address instead of answering. Look before trying again.";
pub const LONG_FORM: &str =
    "This was too long to send the usual way, and your server's proxy does not answer such requests itself.";

pub fn waiting_words(count: usize) -> String {
    let waiting = if count == 1 {
        "1 change is waiting".to_owned()
    } else {
        format!("{count} changes are waiting")
    };
    format!(
        "Your server's proxy redirects requests that change something instead of answering them. {waiting}. Change the proxy so that it answers without redirecting."
    )
}

impl Refusal {
    pub fn reason(&self) -> Reason {
        // Eligibility comes first (5.3). Keep the target-specific detail for the
        // refusal sentence that 5.5 requires alongside the form explanation.
        if self.form { Reason::Form } else { self.detail }
    }
    pub fn target(&self) -> Option<&Origin> {
        self.target.as_ref()
    }
    /// The target origin with its music base, learned from a changed, same-origin path.
    pub fn suggested_base(&self) -> Option<&str> {
        self.base.as_deref()
    }

    pub fn words(&self, account: AccountAddress) -> String {
        let mut words = match (self.detail, &self.target, &self.base) {
            (Reason::Change, _, _) => CHANGE_UNCERTAIN.into(),
            (Reason::Downgrade, _, _) => "Your server sent the app on to a plain HTTP address, where your sign-in could be read on the way. The app didn't go.".into(),
            (Reason::Elsewhere | Reason::Form, Some(origin), _) => {
                let mut message = format!("Something at this address sent the app on to {origin}. Your sign-in wasn't passed on.");
                if self.detail == Reason::Elsewhere && account == AccountAddress::Typed {
                    message.push_str(" If that is your server, sign in with that address.");
                }
                message
            }
            (Reason::Changed, _, Some(base)) if account == AccountAddress::Typed =>
                format!("Your server sent the app on to another place on the same server. If your server is at {base}, use that address."),
            (Reason::Changed, _, Some(base)) => format!("Your server sent the app on to another place on the same server, at {base}."),
            (Reason::Changed, _, None) if self.path_changed =>
                "Your server sent the app on to another place on the same server. The app didn't go.".into(),
            (Reason::Changed, _, _) => "Something at this address sent the app on to an address that left out, or changed, what was asked. The app didn't go.".into(),
            (Reason::TooMany, _, _) => "Something at this address kept sending the app on from one address to another.".into(),
            _ => "Something at this address sent the app on to an address the app couldn't make sense of.".into(),
        };
        if self.form {
            words.push(' ');
            words.push_str(LONG_FORM);
        }
        if account == AccountAddress::HandedOver {
            words.push_str(" Tell whoever runs your Nexus server.");
        }
        words
    }

    fn new(reason: Reason, target: Option<&Address>, form: bool) -> Self {
        Self {
            detail: reason,
            target: target.map(|a| a.origin().clone()),
            path_changed: false,
            base: None,
            form,
        }
    }
}

/// `previous` lists requests already dispatched *before* `from`, starting with the
/// original. Its length is the number of redirects already followed. Pass every raw
/// Location field separately, including non-text values. No body or clock is needed.
pub fn judge(
    from: &MusicRequest,
    method: Method,
    status: u16,
    locations: &[&[u8]],
    previous: &[Address],
) -> Verdict {
    if !(300..400).contains(&status) {
        return Verdict::Answer;
    }

    let target = match locations {
        [raw] if raw.len() <= MAX_ADDRESS_BYTES => std::str::from_utf8(raw)
            .ok()
            .and_then(|s| from.address().location(s).ok()),
        _ => None,
    };
    // Parse the target for safe diagnostics even on send-once requests. Neither
    // a change nor a form can get a follow verdict, whatever it contains.
    if operation(from.operation()) == Operation::Change {
        return Verdict::Refuse(Refusal::new(Reason::Change, target.as_ref(), false));
    }

    let form = method == Method::Form;
    // Sections 5.3 and 5.5 disagree: the design does not say how to return an
    // unlisted 3xx as an answer while keeping it out of the music answer reader.
    // Refuse it even if it carries a valid Location.
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Verdict::Refuse(Refusal::new(Reason::Unreadable, target.as_ref(), form));
    }
    let Some(target) = target else {
        return Verdict::Refuse(Refusal::new(Reason::Unreadable, None, form));
    };
    let refuse = |reason| Verdict::Refuse(Refusal::new(reason, Some(&target), form));
    match from.with_added_slash(&target) {
        Err(Difference::Downgrade) => refuse(Reason::Downgrade),
        Err(Difference::Elsewhere) => refuse(Reason::Elsewhere),
        Err(Difference::Path) => {
            let mut refusal = Refusal::new(Reason::Changed, Some(&target), form);
            refusal.path_changed = target.path() != from.address().path();
            if refusal.path_changed && !target.under_nexus() {
                // the design does not say how to derive a music base from a portal or
                // arbitrary path. Suggest one only for an actual base/rest/name shape.
                if let Some((base, name)) = target.path().trim_end_matches('/').rsplit_once("/rest/")
                    && !name.is_empty()
                    && !name.contains('/')
                {
                    refusal.base = Some(format!("{}{base}", target.origin()));
                }
            }
            Verdict::Refuse(refusal)
        }
        Err(Difference::Query) => refuse(Reason::Changed),
        Ok(_) if form => refuse(Reason::Form),
        Ok(_) if previous.len() >= 5 || previous.contains(&target) => refuse(Reason::TooMany),
        Ok(next) => Verdict::Follow(next),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::origin::MusicBase;

    fn built(name: &str, q: Option<&str>) -> MusicRequest {
        MusicBase::typed("music.example.org/navidrome")
            .unwrap()
            .request(name, q)
            .unwrap()
    }
    fn refusal(verdict: Verdict, why: Reason) -> Refusal {
        let Verdict::Refuse(r) = verdict else {
            panic!("expected {why:?}");
        };
        assert_eq!(r.reason(), why);
        for account in [AccountAddress::Typed, AccountAddress::HandedOver] {
            for words in [r.words(account), format!("{r:?}")] {
                assert!(!words.contains('?') && !words.contains("private"), "{words}");
            }
        }
        r
    }

    #[test]
    fn exactly_one_added_slash_on_the_built_request() {
        let source = built("getPlaylist", Some("u=private&id=%2F"));
        for loc in [
            "getPlaylist/?u=private&id=%2F",
            "/navidrome/rest/getPlaylist/?u=private&id=%2F",
            "//MUSIC.example.org/navidrome/rest/getPlaylist/?u=private&id=%2F",
            "HTTPS://MUSIC.EXAMPLE.ORG:443/navidrome/rest/getPlaylist/?u=private&id=%2F#top",
        ] {
            for status in [301, 302, 303, 307, 308] {
                let Verdict::Follow(next) = judge(&source, Method::Get, status, &[loc.as_bytes()], &[]) else {
                    panic!("expected follow");
                };
                assert_eq!(
                    next.address().wire(),
                    "https://music.example.org/navidrome/rest/getPlaylist/?u=private&id=%2F"
                );
                assert_eq!(next.operation(), "getPlaylist");
                assert!(!format!("{next:?}").contains("private"));
                refusal(
                    judge(
                        &next,
                        Method::Get,
                        status,
                        &[b"/navidrome/rest/getPlaylist?u=private&id=%2F"],
                        &[],
                    ),
                    Reason::Changed,
                );
                refusal(
                    judge(
                        &next,
                        Method::Get,
                        status,
                        &[b"/navidrome/rest/getPlaylist//?u=private&id=%2F"],
                        &[],
                    ),
                    Reason::Unreadable,
                );
            }
        }
        for (loc, reason) in [
            ("getPlaylist?u=private&id=%2F", Reason::Changed),
            ("#fragment", Reason::Changed),
            ("", Reason::Changed),
            (
                "HTTPS://MUSIC.EXAMPLE.ORG/navidrome/rest/getPlaylist?u=private&id=%2F",
                Reason::Changed,
            ),
            ("getplaylist/?u=private&id=%2F", Reason::Changed),
            ("deletePlaylist/?u=private&id=%2F", Reason::Changed),
            ("/rest/getPlaylist/?u=private&id=%2F", Reason::Changed),
            ("getPlaylist/", Reason::Changed),
            ("getPlaylist/?", Reason::Changed),
            ("getPlaylist/?u=private&id=%2f", Reason::Changed),
            ("getPlaylist/?id=%2F&u=private", Reason::Changed),
            ("getPlaylist/?u=private&id=%2F&x=1", Reason::Changed),
            ("https://music.example.org:444/rest/ping", Reason::Elsewhere),
            ("https://other.example.org/rest/ping?u=private", Reason::Elsewhere),
            ("http://other.example.org/else", Reason::Downgrade),
            ("http://other.example.org./else", Reason::Unreadable),
            ("https://music.example.org./", Reason::Unreadable),
            ("//name:private@music.example.org/", Reason::Unreadable),
            ("./getPlaylist/?u=private&id=%2F", Reason::Unreadable),
            ("../rest/getPlaylist/", Reason::Unreadable),
            ("getPlaylist%2f", Reason::Unreadable),
            ("getPlaylist\\@other/", Reason::Unreadable),
        ] {
            refusal(judge(&source, Method::Get, 302, &[loc.as_bytes()], &[]), reason);
        }
        let bare = built("ping", None);
        refusal(
            judge(&bare, Method::Get, 301, &[b"ping/?x=private"], &[]),
            Reason::Changed,
        );
        refusal(judge(&bare, Method::Get, 301, &[b"ping/?"], &[]), Reason::Changed);
        assert!(matches!(
            judge(&built("ping", Some("")), Method::Get, 301, &[b"ping/?"], &[]),
            Verdict::Follow(_)
        ));
    }

    #[test]
    fn statuses_raw_location_count_and_chain_boundaries() {
        let source = built("ping", None);
        let one: &[&[u8]] = &[b"ping/"];
        for status in [200, 204, 299, 400, 408, 421, 503] {
            assert_eq!(judge(&source, Method::Get, status, one, &[]), Verdict::Answer);
        }
        for status in [300, 304, 305, 306, 399] {
            refusal(judge(&source, Method::Get, status, one, &[]), Reason::Unreadable);
        }
        let long = vec![b'a'; MAX_ADDRESS_BYTES + 1];
        for locations in [
            &[][..],
            &[b"ping/".as_slice(), b"ping/"],
            &[b"\xff"],
            &[b"a\0b"],
            &[long.as_slice()],
        ] {
            refusal(judge(&source, Method::Get, 301, locations, &[]), Reason::Unreadable);
        }
        let previous = vec![source.address().clone(); 5];
        assert!(matches!(
            judge(&source, Method::Get, 301, one, &previous[..4]),
            Verdict::Follow(_)
        ));
        refusal(judge(&source, Method::Get, 301, one, &previous), Reason::TooMany);
        let asked = source.address().resolve("ping/").unwrap();
        refusal(judge(&source, Method::Get, 301, one, &[asked]), Reason::TooMany);
        // A failed path check precedes both the hop cap and query mismatch.
        refusal(
            judge(&source, Method::Get, 301, &[b"/other?q=private"], &previous),
            Reason::Changed,
        );
    }

    #[test]
    fn changes_forms_and_unknown_operations_are_never_followed() {
        for name in [
            "createPlaylist",
            "updatePlaylist",
            "deletePlaylist",
            "star",
            "unstar",
            "setRating",
            "scrobble",
            "savePlayQueue",
            "startScan",
            "getUnknown",
            "Ping",
            "link",
        ] {
            let req = built(name, None);
            let loc = format!("{name}/");
            assert_eq!(operation(name), Operation::Change);
            for method in [Method::Form, Method::Get] {
                for status in [301, 302, 303, 307, 308] {
                    let r = refusal(judge(&req, method, status, &[loc.as_bytes()], &[]), Reason::Change);
                    assert_eq!(r.words(AccountAddress::Typed), CHANGE_UNCERTAIN);
                }
            }
        }
        for name in [
            "ping",
            "getAlbum",
            "getAlbumList2",
            "getArtist",
            "getArtistInfo2",
            "getArtists",
            "getCoverArt",
            "getGenres",
            "getInternetRadioStations",
            "getLyricsBySongId",
            "getPlayQueue",
            "getPlaylist",
            "getPlaylists",
            "getRandomSongs",
            "getScanStatus",
            "getSimilarSongs2",
            "getSong",
            "getStarred2",
            "getTopSongs",
            "search3",
            "stream",
        ] {
            let req = built(name, None);
            let loc = format!("{name}/");
            assert_eq!(operation(name), Operation::Read);
            for status in [301, 302, 303, 307, 308] {
                let r = refusal(judge(&req, Method::Form, status, &[loc.as_bytes()], &[]), Reason::Form);
                assert!(r.words(AccountAddress::Typed).ends_with(LONG_FORM));
            }
        }
        let form = refusal(
            judge(
                &built("search3", None),
                Method::Form,
                302,
                &[b"https://else.example.org/?q=private"],
                &[],
            ),
            Reason::Form,
        );
        assert!(form.words(AccountAddress::Typed).contains("https://else.example.org"));
        assert!(form.words(AccountAddress::Typed).ends_with(LONG_FORM));
        assert!(waiting_words(1).contains("1 change is waiting"));
        assert!(waiting_words(2).contains("2 changes are waiting"));
    }

    #[test]
    fn refusal_keeps_only_the_origin_and_a_recognizable_same_origin_base() {
        let source = built("ping", None);
        for (loc, origin, base, why) in [
            (
                "https://Other.Example.org:444/new/rest/ping?u=private",
                Some("https://other.example.org:444"),
                None,
                Reason::Elsewhere,
            ),
            (
                "/new/rest/ping/?u=private#private",
                Some("https://music.example.org"),
                Some("https://music.example.org/new"),
                Reason::Changed,
            ),
            (
                "/rest/ping/",
                Some("https://music.example.org"),
                Some("https://music.example.org"),
                Reason::Changed,
            ),
            (
                "/portal?u=private",
                Some("https://music.example.org"),
                None,
                Reason::Changed,
            ),
            (
                "/Nexus/rest/ping",
                Some("https://music.example.org"),
                None,
                Reason::Changed,
            ),
            (
                "http://music.example.org:8080/?x=private",
                Some("http://music.example.org:8080"),
                None,
                Reason::Downgrade,
            ),
            ("http://private@host/", None, None, Reason::Unreadable),
        ] {
            let r = refusal(judge(&source, Method::Get, 302, &[loc.as_bytes()], &[]), why);
            assert_eq!(r.target().map(ToString::to_string).as_deref(), origin);
            assert_eq!(r.suggested_base(), base);
            if let Some(base) = base {
                assert!(r.words(AccountAddress::Typed).contains(base));
            }
            assert!(
                r.words(AccountAddress::HandedOver)
                    .ends_with("Tell whoever runs your Nexus server.")
            );
        }
    }
}
