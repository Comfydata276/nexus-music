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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub use nexus_core::lidarr::{ArtistOffer, Lidarr, Setup, host, mask, tidy};
pub use nexus_core::offer::{Offer, VARIOUS};

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

static CURRENT: RwLock<Option<Lidarr>> = RwLock::new(None);
/// Albums that have been offered, by Lidarr's name for them, so a click on one can be acted on.
static OFFERS: RwLock<Option<HashMap<String, Offer>>> = RwLock::new(None);

pub fn current() -> Option<Lidarr> {
    CURRENT.read().ok()?.clone()
}

pub fn set_current(lidarr: Option<Lidarr>) {
    if let Ok(mut current) = CURRENT.write() {
        *current = lidarr;
    }
}

/// Artists that have been offered, by Lidarr's name for them.
static ARTISTS: RwLock<Option<HashMap<String, ArtistOffer>>> = RwLock::new(None);

pub fn remember_artists(artists: &[ArtistOffer]) {
    if let Ok(mut all) = ARTISTS.write() {
        let all = all.get_or_insert_with(HashMap::new);
        for artist in artists {
            // A later mention without a picture doesn't replace one with.
            if !artist.cover.is_empty() || !all.contains_key(&artist.id) {
                all.insert(artist.id.clone(), artist.clone());
            }
        }
    }
}

pub fn offered_artist(artist: &str) -> Option<ArtistOffer> {
    ARTISTS.read().ok()?.as_ref()?.get(artist).cloned()
}

pub fn remember(offers: &[Offer]) {
    if let Ok(mut all) = OFFERS.write() {
        let all = all.get_or_insert_with(HashMap::new);
        for offer in offers {
            all.insert(offer.album.clone(), offer.clone());
        }
    }
}

pub fn offered(album: &str) -> Option<Offer> {
    OFFERS.read().ok()?.as_ref()?.get(album).cloned()
}

/// Notes that an album has been asked for (or no longer is), so it's shown that way.
pub fn mark(album: &str, requested: bool) {
    if let Ok(mut all) = OFFERS.write() {
        if let Some(offer) = all.as_mut().and_then(|all| all.get_mut(album)) {
            offer.requested = requested;
        }
    }
}

/// What goes to Lidarr and back, against a stand-in server: written against the app as it
/// was before Lidarr's rules moved to the library, so that the move can be seen to change
/// nothing on the wire.
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
        assert_eq!(lidarr(&address, None).request(&wanted), Ok(()));
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
