//! Asking for albums that aren't in the library.
//!
//! Optional, and off unless switched on in Settings. Lidarr is a separate program some people
//! run beside their music server to keep track of the albums they want. This looks albums up
//! through it, adds one to its list when asked, and reports how the ones on that list are
//! getting on. Where Lidarr gets albums from is its own business and set up there.
//!
//! The questions, and the rules for what to make of the answers, are in the library
//! (`nexus_core::lidarr`), for the Android app to use too. This is the Windows app's end:
//! the connection they're asked over, the Lidarr in use, and what it has offered.

use std::sync::{Arc, RwLock};

pub use nexus_core::lidarr::{ArtistOffer, Lidarr, Offers, mask, tidy};
pub use nexus_core::offer::Offer;

/// Lidarr at `address`, asked over an agent of the app's own. `gate` is the sign-in in front
/// of it, if there is one.
pub fn open(address: &str, key: &str, gate: Option<Arc<crate::gate::Session>>) -> Lidarr {
    // Redirects aren't followed: Lidarr's own answers never are one, and a sign-in in
    // front of it shows itself by sending one.
    let gate = gate.map(|gate| gate as Arc<dyn nexus_core::lidarr::SignIn>);
    Lidarr::new(address, key, gate, Arc::new(crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects())))
}


/// Whether singles and EPs are offered as well as albums. Set from Settings.
static SINGLES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_singles(on: bool) {
    SINGLES.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn singles() -> bool {
    SINGLES.load(std::sync::atomic::Ordering::Relaxed)
}

// ---- the connection in use, and what it has offered -----------------------------------------------

/// The Lidarr in use; the number of its connection, which goes up whenever what it's made of
/// changes (its address, its key, the sign-in in front); and what it's made of.
static CURRENT: RwLock<(Option<Lidarr>, u64, String)> = RwLock::new((None, 0, String::new()));
/// Albums and artists that have been offered, by Lidarr's names for them, so a click on one
/// can be acted on.
static OFFERS: Offers = Offers::new();

/// What has been offered, for what offers more.
pub fn offers() -> &'static Offers {
    &OFFERS
}

pub fn current() -> Option<Lidarr> {
    CURRENT.read().ok()?.0.clone()
}

/// The Lidarr in use, and the number of its connection, read together: what a page and what's
/// started from it carry (see `for_connection`).
pub fn current_numbered() -> (Option<Lidarr>, u64) {
    CURRENT.read().map(|current| (current.0.clone(), current.1)).unwrap_or_default()
}

/// The Lidarr in use, if it's still the connection numbered `number`: an action begun under
/// an earlier one (a row of a page drawn before the address, key or sign-in changed) is for
/// a Lidarr that isn't this one, and does nothing.
pub fn for_connection(number: u64) -> Option<Lidarr> {
    let current = CURRENT.read().ok()?;
    current.0.clone().filter(|_| current.1 == number)
}

/// Sets the Lidarr in use, for a test.
#[cfg(test)]
pub fn set_current(lidarr: Option<Lidarr>) {
    set_current_made_of(lidarr, String::new());
}

/// Sets the Lidarr in use, `made_of` saying what it's made of: its address, its key and the
/// sign-in in front. Its connection's number goes up if that has changed.
pub fn set_current_made_of(lidarr: Option<Lidarr>, made_of: String) {
    if let Ok(mut current) = CURRENT.write() {
        if current.2 != made_of || current.0.is_some() != lidarr.is_some() {
            current.1 += 1;
        }
        *current = (lidarr, current.1, made_of);
    }
}

pub fn remember_artists(artists: &[ArtistOffer]) {
    OFFERS.remember_artists(artists)
}

pub fn remember(offers: &[Offer]) {
    OFFERS.remember(offers)
}

pub fn offered(album: &str) -> Option<Offer> {
    OFFERS.offered(album)
}

/// Forgets everything offered, for a test that starts from nothing.
#[cfg(test)]
pub fn forget_offers() {
    OFFERS.forget()
}

/// Notes that an album has been asked for (or no longer is), so it's shown that way.
pub fn mark(album: &str, requested: bool) {
    OFFERS.mark(album, requested)
}

/// What goes to Lidarr and back, against a stand-in server: written against the app as it
/// was before Lidarr's rules moved to the library, so that the move can be seen to change
/// nothing on the wire.
#[cfg(test)]
mod origins {
    use super::*;
    use nexus_core::lidarr::same_origin;

    #[test]
    fn an_action_is_for_the_lidarr_it_was_begun_under() {
        let _apart = crate::pins::apart();
        let at = |address: &str| Some(Lidarr::new(address, "key", None, Arc::new(crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects()))));
        set_current_made_of(at("http://a.example"), "a\nkey-a\n".into());
        let (_, begun) = current_numbered();
        assert!(for_connection(begun).is_some());
        // Nothing changed: the same connection.
        set_current_made_of(at("http://a.example"), "a\nkey-a\n".into());
        assert!(for_connection(begun).is_some());
        // Another address, another key, a sign-in: another connection, and the row's action
        // does nothing.
        for made_of in ["b\nkey-a\n", "b\nkey-b\n", "b\nkey-b\n1"] {
            set_current_made_of(at("http://b.example"), made_of.into());
            assert!(for_connection(begun).is_none(), "{made_of}");
        }
        set_current(None);
    }

    #[test]
    fn a_key_belongs_to_the_scheme_host_and_port_it_was_typed_for() {
        assert!(!same_origin("https://lidarr.example.org", "http://lidarr.example.org"));
        assert!(!same_origin("lidarr.example.org:8686", "https://lidarr.example.org:8686"));
        assert!(same_origin("https://Lidarr.Example.org/", "https://lidarr.example.org:443/lidarr"));
        assert!(!same_origin("http://lidarr.example.org:8686", "http://lidarr.example.org:8687"));
    }

    /// Where the app's HTTP client would connect for an address: its scheme, host and port as
    /// its own parser reads them.
    fn where_the_agent_goes(address: &str) -> Option<(String, String, u16)> {
        let uri: ureq::http::Uri = tidy(address).parse().ok()?;
        let scheme = uri.scheme_str()?.to_ascii_lowercase();
        let port = uri.port_u16().or(match scheme.as_str() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        })?;
        Some((scheme, uri.host()?.to_ascii_lowercase(), port))
    }

    #[test]
    fn the_library_reads_an_address_as_the_agent_does_or_refuses_it() {
        // Each address, and what the library makes of it: where it leads, or refused.
        let table: &[(&str, Option<(&str, &str, u16)>)] = &[
            ("https://lidarr.example.org", Some(("https", "lidarr.example.org", 443))),
            ("lidarr.example.org:8686", Some(("http", "lidarr.example.org", 8686))),
            ("192.168.1.5:8686/", Some(("http", "192.168.1.5", 8686))),
            ("HTTPS://LIDARR.Example.ORG:443/Lidarr/", Some(("https", "lidarr.example.org", 443))),
            ("https://lidarr.example.org:443", Some(("https", "lidarr.example.org", 443))),
            ("http://lidarr.example.org:80/", Some(("http", "lidarr.example.org", 80))),
            ("https://lidarr.example.org.", Some(("https", "lidarr.example.org.", 443))),
            ("http://[::1]:8686", Some(("http", "[::1]", 8686))),
            ("http://[FE80::1]/lidarr", Some(("http", "[fe80::1]", 80))),
            ("https://lidarr.example.org?x=1", Some(("https", "lidarr.example.org", 443))),
            ("https://lidarr.example.org#here", Some(("https", "lidarr.example.org", 443))),
            // Only the origin is judged: a path is the address's own, as 0.40.10 took it, and a
            // port is its number.
            ("https://lidarr.example.org/musique/lidärr", Some(("https", "lidarr.example.org", 443))),
            ("http://192.168.1.5:08686/lidarr", Some(("http", "192.168.1.5", 8686))),
            ("http://192.168.1.5:0000008686", Some(("http", "192.168.1.5", 8686))),
            ("https://lidarr.example.org/a b", Some(("https", "lidarr.example.org", 443))),
            // Refused: a name and password; what the client might read otherwise.
            ("https://me:secret@lidarr.example.org", None),
            ("https://evil.example@lidarr.example.org", None),
            ("http://lidarr.example.org\\@evil.example/", None),
            ("http://evil.example\\.lidarr.example.org", None),
            ("https://lidarr.example.org:", None),
            ("https://lidarr.example.org:0", None),
            ("https://lidarr.example.org:65536", None),
            ("https://lidarr.example.org:000", None),
            ("https://lidarr.example.org:8x", None),
            ("https://lidärr.example.org", None),
            ("https://lidarr example.org", None),
            ("ftp://lidarr.example.org", None),
            ("https://", None),
            ("https://[::1", None),
            ("https://%6cidarr.example.org", None),
            ("", None),
        ];
        for (address, expected) in table {
            let found = nexus_core::lidarr::origin(address).ok().map(|origin| (origin.scheme, origin.host, origin.port));
            let expected = expected.map(|(scheme, host, port)| (scheme.to_string(), host.to_string(), port));
            assert_eq!(found, expected, "the library's reading of {address:?}");
            // Wherever the library goes by an address, the client goes to the same place, or
            // (an address its parser won't take, by its path) nowhere.
            if let (Some(found), Some(agent)) = (found.clone(), where_the_agent_goes(address)) {
                assert_eq!(agent, found, "the client's reading of {address:?}");
            }
            // (Only a path beyond plain ASCII, or with a space, keeps the client from it.)
            if found.is_some() && address.is_ascii() && !address.contains(' ') {
                assert!(where_the_agent_goes(address).is_some(), "the client reads {address:?}");
            }
        }
        assert_eq!(nexus_core::lidarr::origin("https://me:secret@lidarr.example.org").unwrap_err(), nexus_core::lidarr::ADDRESS_WITH_NAME);
        // An address already saved is read as 0.40.10 used it, and goes where the client goes.
        for address in ["http://me:secret@lidarr.example.org:8686", "https://me@lidarr.example.org/lidarr", "http://lidarr.example.org:", "http://192.168.1.5:/"] {
            let found = nexus_core::lidarr::origin_saved(address).ok().map(|origin| (origin.scheme, origin.host, origin.port));
            assert!(found.is_some(), "{address:?}");
            assert_eq!(where_the_agent_goes(address), found, "the client's reading of {address:?}");
        }
    }
}

#[cfg(test)]
mod wire {
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::subsonic::fake::{Sent, json, recorder};

    fn lidarr(address: &str, gate: Option<Arc<crate::gate::Session>>) -> Lidarr {
        open(address, " a-key ", gate)
    }

    fn session(service: &str, token: &str) -> Arc<crate::gate::Session> {
        let found = crate::gate::Found { service: service.into(), client: "lidarr-proxy".into() };
        Arc::new(crate::gate::session(found, token.into(), |_| {}))
    }

    /// A request's head as the app has always sent it: what every request carries, in the
    /// order it goes, then the caller's own headers.
    fn head(line: &str, length: Option<usize>, own: &str) -> String {
        let length = length.map(|length| format!("content-length: {length}\r\n")).unwrap_or_default();
        format!("{line} HTTP/1.1\r\naccept-encoding: gzip\r\n{length}user-agent: NexusMusic/{}\r\naccept: */*\r\nhost: standin\r\n{own}", env!("CARGO_PKG_VERSION"))
    }

    fn heads(sent: &[Sent]) -> Vec<String> {
        sent.iter().map(|sent| sent.head.clone()).collect()
    }

    #[test]
    fn asks_lidarr_as_it_always_has() {
        let (address, sent) = recorder(|_, request, stream| match request.line() {
            line if line.starts_with("GET /api/v1/system/status ") => json(stream, "200 OK", r#"{"version":"2.12.4.4658"}"#),
            line if line.starts_with("GET /api/v1/artist/7 ") => json(stream, "200 OK", r#"{"id":7,"artistName":"Someone","monitored":false,"monitorNewItems":"all"}"#),
            line if line.starts_with("GET /api/v1/wanted/missing") || line.starts_with("GET /api/v1/queue") => {
                json(stream, "200 OK", r#"{"page":1,"pageSize":500,"totalRecords":0,"records":[]}"#)
            }
            line if line.starts_with("GET ") => json(stream, "200 OK", "[]"),
            _ => json(stream, "200 OK", "{}"),
        });
        let lidarr = lidarr(&address, None);
        assert_eq!(lidarr.version().as_deref(), Ok("2.12.4.4658"));
        assert_eq!(lidarr.find_any("Thriller & more/é+1 ?#%").map(|found| found.len()), Ok(0));
        assert_eq!(lidarr.want(7, &[1, 2]), Ok(()));
        assert_eq!(lidarr.pending(), Ok(Vec::new()));
        let sent = sent.lock().unwrap().clone();
        let get = |path: &str| head(&format!("GET /api/v1/{path}"), None, "x-api-key: a-key\r\n");
        let put = |path: &str, length: usize| head(&format!("PUT /api/v1/{path}"), Some(length), "x-api-key: a-key\r\ncontent-type: application/json\r\n");
        let post = |path: &str, length: usize| head(&format!("POST /api/v1/{path}"), Some(length), "x-api-key: a-key\r\ncontent-type: application/json\r\n");
        let record = r#"{"artistName":"Someone","id":7,"monitorNewItems":"all","monitored":true}"#;
        let monitor = r#"{"albumIds":[1,2],"monitored":true}"#;
        let search = r#"{"albumIds":[1,2],"name":"AlbumSearch"}"#;
        assert_eq!(
            heads(&sent),
            [
                get("system/status"),
                get("album/lookup?term=Thriller%20%26%20more%2F%C3%A9%2B1%20%3F%23%25"),
                get("artist/7"),
                put("artist/7", record.len()),
                put("album/monitor", monitor.len()),
                post("command", search.len()),
                get("wanted/missing?includeArtist=true&page=1&pageSize=500"),
                get("queue?page=1&pageSize=500"),
            ]
        );
        let bodies: Vec<&str> = sent.iter().map(|sent| sent.body.as_str()).collect();
        assert_eq!(bodies, ["", "", "", record, monitor, search, "", ""]);
    }

    #[test]
    fn a_redirect_is_a_sign_in_in_front() {
        let (address, sent) = recorder(|_, _, stream| {
            let _ = write!(stream, "HTTP/1.1 302 Found\r\nLocation: /outpost.goauthentik.io/start\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        assert_eq!(lidarr(&address, None).version(), Err(crate::gate::NEEDED.to_string()));
        // Not followed.
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_pass_turned_away_is_renewed_once() {
        let passes = std::sync::atomic::AtomicUsize::new(0);
        let (address, sent) = recorder(move |_, request, stream| match request.line() {
            line if line.starts_with("POST /application/o/token/ ") => {
                if request.body.contains("client_credentials") {
                    let pass = passes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                    json(stream, "200 OK", &format!(r#"{{"access_token":"pass-{pass}","expires_in":"600"}}"#));
                } else {
                    json(stream, "200 OK", r#"{"access_token":"access","refresh_token":"renewed+token"}"#);
                }
            }
            _ => {
                let _ = write!(stream, "HTTP/1.1 302 Found\r\nLocation: /outpost.goauthentik.io/start\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        let found = lidarr(&address, Some(session(&address, "kept token/1"))).version();
        assert_eq!(
            found.err().as_deref(),
            Some("You're signed in, but the sign-in in front of Lidarr still turned the app away. The proxy in front of Lidarr may not be passing the sign-in on.")
        );
        let sent = sent.lock().unwrap().clone();
        let token = |length: usize| head("POST /application/o/token/", Some(length), "content-type: application/x-www-form-urlencoded\r\n");
        let status = |pass: &str| head("GET /api/v1/system/status", None, &format!("x-api-key: a-key\r\nauthorization: Bearer {pass}\r\n"));
        let refresh = |token: &str| format!("grant_type=refresh_token&refresh_token={token}&client_id=nexus-music");
        let trade = "grant_type=client_credentials&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer&client_assertion=access&client_id=lidarr-proxy&scope=openid+profile+email+ak_proxy";
        let (first, second) = (refresh("kept+token%2F1"), refresh("renewed%2Btoken"));
        assert_eq!(
            heads(&sent),
            [token(first.len()), token(trade.len()), status("pass-1"), token(second.len()), token(trade.len()), status("pass-2")]
        );
        let bodies: Vec<&str> = sent.iter().map(|sent| sent.body.as_str()).collect();
        assert_eq!(bodies, [first.as_str(), trade, "", second.as_str(), trade, ""]);
    }

    /// How long a stand-in server holds a body open, saying nothing more, before it gives up.
    const STALL: Duration = Duration::from_secs(3);

    fn stalls(status: &'static str) -> String {
        recorder(move |_, _, stream: &mut TcpStream| {
            let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{{\"ver");
            let _ = stream.flush();
            std::thread::sleep(STALL);
        })
        .0
    }

    #[test]
    fn an_error_is_judged_once_its_body_has_been_waited_for() {
        // As it stands, every answer's body is read before its status is looked at: an error
        // whose body never comes holds the request until the other end gives up on it.
        let started = Instant::now();
        assert_eq!(lidarr(&stalls("500 Internal Server Error"), None).version(), Err("Lidarr answered with an error (500).".to_string()));
        assert!(started.elapsed() >= STALL - Duration::from_millis(500), "took {:?}", started.elapsed());
    }

    #[test]
    fn an_answer_cut_short_is_not_lidarr() {
        let started = Instant::now();
        assert_eq!(lidarr(&stalls("200 OK"), None).version(), Err("That address answered, but not as Lidarr. Check the address.".to_string()));
        assert!(started.elapsed() >= STALL - Duration::from_millis(500), "took {:?}", started.elapsed());
    }

    #[test]
    fn busy_is_an_error_like_any_other() {
        // Lidarr's own answers aren't asked again, whatever they say about waiting.
        let (address, sent) = recorder(|_, _, stream| {
            let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: \xff\r\nRetry-After: 1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy");
        });
        assert_eq!(lidarr(&address, None).version(), Err("Lidarr answered with an error (503).".to_string()));
        assert_eq!(sent.lock().unwrap().len(), 1);
        let (address, _) = recorder(|_, _, stream| json(stream, "401 Unauthorized", ""));
        assert_eq!(lidarr(&address, None).version(), Err("Lidarr turned the request away. Check the API key.".to_string()));
        let (address, _) = recorder(|_, _, stream| json(stream, "200 OK", "<html>"));
        assert_eq!(lidarr(&address, None).version(), Err("That address answered, but not as Lidarr. Check the address.".to_string()));
    }

    #[test]
    fn says_when_lidarr_cannot_be_reached() {
        let address = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", listener.local_addr().unwrap())
        };
        let found = lidarr(&address, None).version().unwrap_err();
        assert!(found.starts_with("Couldn't reach Lidarr. "), "{found}");
    }

    #[test]
    fn waits_for_lidarr_to_keep_an_album_on_its_list() {
        // Asking for an album of an artist Lidarr has: it's marked, a moment is given, and
        // it's seen to have stayed marked before the search for it starts.
        let (address, sent) = recorder(|_, request, stream| match request.line() {
            line if line.starts_with("GET /api/v1/artist ") => json(stream, "200 OK", r#"[{"id":5,"artistName":"Someone","foreignArtistId":"ar1"}]"#),
            line if line.starts_with("GET /api/v1/album?artistId=5 ") => json(stream, "200 OK", r#"[{"id":9,"title":"First","foreignAlbumId":"al1","albumType":"Album"}]"#),
            line if line.starts_with("GET /api/v1/command ") => json(stream, "200 OK", r#"[{"name":"RefreshArtist","status":"completed"}]"#),
            line if line.starts_with("GET /api/v1/artist/5 ") => json(stream, "200 OK", r#"{"id":5,"monitored":true}"#),
            line if line.starts_with("GET /api/v1/album/9 ") => json(stream, "200 OK", r#"{"id":9,"monitored":true}"#),
            _ => json(stream, "200 OK", "{}"),
        });
        let wanted = Offer { album: "al1".into(), artist_id: "ar1".into(), title: "First".into(), artist: "Someone".into(), ..Default::default() };
        let started = Instant::now();
        // (An album: nothing to put back, and nothing written down.)
        struct Nowhere;
        impl nexus_core::lidarr::WayBack for Nowhere {
            fn turn(&self) -> Box<dyn nexus_core::lidarr::Turn + '_> {
                Box::new(())
            }
            fn keep(&self, _: &str, _: i64, _: &str) -> Result<bool, String> {
                Err("not here".into())
            }
            fn put_back(&self, _: &str, _: i64, _: &str) {}
        }
        assert_eq!(lidarr(&address, None).request(&wanted, &Nowhere), Ok(()));
        let took = started.elapsed();
        assert!(took >= Duration::from_secs(3) && took < Duration::from_secs(6), "took {took:?}");
        let lines: Vec<String> = sent.lock().unwrap().iter().map(|sent| format!("{} {}", sent.line(), sent.body)).collect();
        assert_eq!(
            lines,
            [
                "GET /api/v1/artist HTTP/1.1 ",
                "GET /api/v1/album?artistId=5 HTTP/1.1 ",
                "GET /api/v1/command HTTP/1.1 ",
                "GET /api/v1/artist/5 HTTP/1.1 ",
                r#"PUT /api/v1/album/monitor HTTP/1.1 {"albumIds":[9],"monitored":true}"#,
                "GET /api/v1/album/9 HTTP/1.1 ",
                "GET /api/v1/artist/5 HTTP/1.1 ",
                r#"POST /api/v1/command HTTP/1.1 {"albumIds":[9],"name":"AlbumSearch"}"#,
            ]
        );
    }
}
