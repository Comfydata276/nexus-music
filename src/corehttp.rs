//! The Windows app's answer to the library's requests: its own HTTP agent.
//!
//! The library (`nexus_core`) opens no connections. Whatever it wants from the network it
//! asks for through `nexus_core::http::Http`, and this is that, on one of the app's agents:
//! the same connections, name lookups, timeouts, certificates and User-Agent as every other
//! request the app makes.

use nexus_core::http::{BODY_LIMIT, Failure, Http, Method, Request, Response, Stream};

pub struct Agent(pub ureq::Agent);

/// What the app asks the public services through (MusicBrainz, ListenBrainz): an agent that
/// lets go of a connection idle for longer than `idle`, never connects to this machine or its
/// network (`subsonic::make_public_agent`), and follows a redirect only by `Public`'s rule.
pub fn public(idle: std::time::Duration) -> std::sync::Arc<dyn Http> {
    std::sync::Arc::new(Public { inner: Box::new(Agent(crate::subsonic::make_public_agent(idle))) })
}

/// How many redirects in a row a public service's answer is followed through.
const HOPS: usize = 5;

/// A public service's requests, with its redirects followed by hand: only for a GET (a POST's
/// answer is the answer, its redirect included), only from HTTPS to HTTPS, at most `HOPS` in a
/// row, and never to an address written as one on this machine or its network (loopback,
/// link-local, private, "localhost"). Where a name leads is judged where it's connected to, by
/// the agent's resolver (`subsonic::dns::PublicResolver`), for every request. A redirect that
/// isn't followed is handed over as the answer.
pub struct Public {
    inner: Box<dyn Http>,
}

impl Public {
    /// Where a redirect from `from` to `location` leads, if it's to be followed.
    fn next(&self, from: &str, location: &str) -> Option<String> {
        let from: ureq::http::Uri = from.parse().ok()?;
        if from.scheme_str()? != "https" {
            return None;
        }
        // Resolved against the address that answered as the old client (ureq) resolved it, then
        // judged.
        let to = join(&from, location)?;
        let uri: ureq::http::Uri = to.parse().ok()?;
        if !uri.scheme_str()?.eq_ignore_ascii_case("https") || uri.authority()?.as_str().contains('@') {
            return None;
        }
        let host = uri.host()?.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
        if host == "localhost" || host.ends_with(".localhost") {
            return None;
        }
        // (A name is judged where it's connected to: see `PublicResolver`.)
        match host.parse::<std::net::IpAddr>() {
            Ok(ip) => crate::subsonic::far_away(ip).then_some(to),
            Err(_) => Some(to),
        }
    }
}

/// Where a redirect from `base` to `location` leads, resolved as ureq-proto 0.7.1 (the client
/// 0.40.10 followed redirects with) resolves it (`client::amended::join`): a whole address as it
/// is; "//host/path" that host on the base's scheme; "/path" a path on the base's host; anything
/// else against the base's folder, "." passed over and ".." a folder up, and refused above the
/// root (so "?query" lands in the folder, as there). `None` where it refused.
fn join(base: &ureq::http::Uri, location: &str) -> Option<String> {
    if location.parse::<ureq::http::Uri>().ok().is_some_and(|uri| uri.scheme().is_some()) {
        return Some(location.to_string());
    }
    let scheme = base.scheme_str()?;
    if location.starts_with("//") {
        let joined: ureq::http::Uri = format!("{scheme}:{location}").parse().ok()?;
        return Some(joined.to_string());
    }
    let authority = base.authority()?;
    let path = if location.starts_with('/') {
        location.to_string()
    } else {
        let mut joiner: Vec<&str> = base.path().split('/').collect();
        if joiner.len() > 1 {
            joiner.pop();
        }
        for segment in location.split('/') {
            match segment {
                "." => {}
                ".." if joiner.len() == 1 => return None,
                ".." => {
                    joiner.pop();
                }
                _ => joiner.push(segment),
            }
        }
        joiner.join("/")
    };
    let joined: ureq::http::Uri = format!("{scheme}://{authority}{path}").parse().ok()?;
    Some(joined.to_string())
}

impl Http for Public {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let mut answer = self.inner.send(request)?;
        if !matches!(request.method, Method::Get) {
            return Ok(answer);
        }
        let mut url = request.url.to_string();
        for _ in 0..HOPS {
            if !matches!(answer.status, 301 | 302 | 303 | 307 | 308) {
                break;
            }
            let Some(next) = answer.header("location").and_then(|to| self.next(&url, to)) else { break };
            // The redirect's own body is let go of unread.
            answer = self.inner.send(&Request { url: &next, ..request.clone() })?;
            url = next;
        }
        Ok(answer)
    }

    fn pause(&self, wait: std::time::Duration) -> Result<(), Failure> {
        self.inner.pause(wait)
    }
}

impl Http for Agent {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let unreachable = |e: ureq::Error| Failure::Reach(e.to_string());
        let response = match request.method {
            Method::Get => {
                let mut ask = self.0.get(request.url);
                for (name, value) in request.headers {
                    ask = ask.header(*name, *value);
                }
                match request.patience {
                    Some(patience) => ask.config().timeout_global(Some(patience)).build().call(),
                    None => ask.call(),
                }
                .map_err(unreachable)?
            }
            Method::Post(body) | Method::Put(body) => {
                let mut ask = if matches!(request.method, Method::Put(_)) { self.0.put(request.url) } else { self.0.post(request.url) };
                for (name, value) in request.headers {
                    ask = ask.header(*name, *value);
                }
                match request.patience {
                    Some(patience) => ask.config().timeout_global(Some(patience)).build().send(body),
                    None => ask.send(body),
                }
                .map_err(unreachable)?
            }
        };
        let status = response.status().as_u16();
        // Every header, a value that isn't text included (as one that can't be read): the
        // first of a name is the one the library goes by, readable or not.
        let headers = response.headers().iter().map(|(name, value)| (name.as_str().to_string(), value.to_str().ok().map(str::to_string))).collect();
        // The body is read only if the library asks for it, and then as the app has always
        // read one. An answer the library judges by its status is not waited on: nothing
        // limits how long a body may take, and a body nobody asks for is dropped unread.
        Ok(Response::streamed(status, headers, Body(response.into_body())))
    }
}

/// An answer's body, unread.
struct Body(ureq::Body);

impl Stream for Body {
    fn length(&self) -> Option<u64> {
        self.0.content_length()
    }

    fn text(mut self: Box<Self>) -> Result<String, String> {
        self.0.read_to_string().map_err(|e| e.to_string())
    }

    fn bytes(self: Box<Self>) -> Box<dyn std::io::Read + Send> {
        Box::new(self.0.into_with_config().limit(BODY_LIMIT).reader())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use nexus_core::listenbrainz::ListenBrainz;
    use nexus_core::musicbrainz::MusicBrainz;

    use super::*;
    use crate::subsonic::fake;

    /// The app's own agent, as the library is given it, with MusicBrainz's address swapped
    /// for a stand-in server's and no waiting between requests.
    struct StandIn {
        agent: Agent,
        address: String,
        /// The waits the library asked for, in order.
        waits: std::sync::Mutex<Vec<Duration>>,
    }

    impl Http for StandIn {
        fn send(&self, request: &Request) -> Result<Response, Failure> {
            let url = request.url.replacen("https://musicbrainz.org", &self.address, 1);
            self.agent.send(&Request { url: &url, ..request.clone() })
        }

        fn pause(&self, wait: Duration) -> Result<(), Failure> {
            self.waits.lock().unwrap().push(wait);
            Ok(())
        }
    }

    fn stand_in(address: String) -> (MusicBrainz, Arc<StandIn>) {
        let http = Arc::new(StandIn { agent: Agent(crate::subsonic::make_agent()), address, waits: Default::default() });
        (MusicBrainz::new(http.clone(), ListenBrainz::new(http.clone())), http)
    }

    fn musicbrainz(address: String) -> MusicBrainz {
        stand_in(address).0
    }

    /// How long a stand-in server holds an answer open after its headers, saying nothing more.
    const STALL: Duration = Duration::from_secs(20);

    #[test]
    fn an_answer_that_ends_a_lookup_is_judged_without_waiting_for_its_body() {
        // An error's headers, promising a body that never comes: a server or a proxy in
        // trouble. Nothing limits how long a body may take, so waiting for it is waiting for
        // as long as the other end likes.
        let address = fake::server(|_, _, stream| {
            let _ = write!(stream, "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/html\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n");
            let _ = stream.flush();
            std::thread::sleep(STALL);
        });
        let started = Instant::now();
        let found = musicbrainz(address).album_tracks("an-album");
        assert_eq!(found.err().as_deref(), Some("MusicBrainz answered with an error (500)."));
        assert!(started.elapsed() < Duration::from_secs(5), "waited {:?} for a body nothing was going to read", started.elapsed());
    }

    #[test]
    fn a_busy_answer_is_read_through_until_the_one_that_ends_the_lookup() {
        // Busy three times, each said in full; the fourth time the body never comes. The first
        // three are asked again; the fourth is the answer, on its status alone.
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        let address = fake::server(move |_, _, stream| {
            if count.fetch_add(1, Ordering::SeqCst) < 3 {
                let _ = write!(stream, "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy");
            } else {
                let _ = write!(stream, "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 1\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n");
                let _ = stream.flush();
                std::thread::sleep(STALL);
            }
        });
        let started = Instant::now();
        let found = musicbrainz(address).album_tracks("an-album");
        assert_eq!(found.err().as_deref(), Some("MusicBrainz answered with an error (503)."));
        assert_eq!(asked.load(Ordering::SeqCst), 4);
        assert!(started.elapsed() < Duration::from_secs(5), "waited {:?} for a body nothing was going to read", started.elapsed());
    }

    #[test]
    fn the_first_retry_after_decides_the_wait_readable_or_not() {
        // Busy once, with two Retry-After headers: the first isn't text, the second says
        // three seconds. The first is the one that counts, and one that can't be read means
        // a second.
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        let address = fake::server(move |_, _, stream| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: \xff\xfe\r\nRetry-After: 3\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy");
            } else {
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
            }
        });
        let (service, http) = stand_in(address);
        assert_eq!(service.album_tracks("an-album"), Ok(Vec::new()));
        let waits = http.waits.lock().unwrap().clone();
        assert_eq!(waits.len(), 1, "{waits:?}");
        assert!(waits[0] <= Duration::from_secs(1) && waits[0] > Duration::from_millis(500), "waited {:?}", waits[0]);

        // And when the first can be read, it's the one: two seconds, not the three after it.
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        let address = fake::server(move |_, _, stream| {
            if count.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = write!(stream, "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 2\r\nRetry-After: 3\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbusy");
            } else {
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
            }
        });
        let (service, http) = stand_in(address);
        assert_eq!(service.album_tracks("an-album"), Ok(Vec::new()));
        let waits = http.waits.lock().unwrap().clone();
        assert!(waits.len() == 1 && waits[0] <= Duration::from_secs(2) && waits[0] > Duration::from_millis(1500), "{waits:?}");
    }

    #[test]
    fn json_is_read_as_ureq_reads_it() {
        // The same answers read by ureq's own read_json, as the app read JSON before the
        // library did, and by the library through the app's agent: the same outcome each time.
        #[derive(serde::Deserialize, Default, Debug, PartialEq)]
        #[serde(default)]
        struct Person {
            preferred_username: String,
            name: String,
        }
        let length = |kind: &str, body: &[u8]| {
            let mut answer = format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
            answer.extend_from_slice(body);
            answer
        };
        let chunked = |kind: &str, body: &[u8]| {
            let mut answer = format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n", body.len()).into_bytes();
            answer.extend_from_slice(body);
            answer.extend_from_slice(b"\r\n0\r\n\r\n");
            answer
        };
        let cases: Vec<Vec<u8>> = vec![
            length("application/json", br#"{"preferred_username":"someone","name":"Some One"}"#),
            chunked("application/json", br#"{"name":"Some One"}"#),
            length("application/json", b"{\"sub\":\"\xff\",\"preferred_username\":\"someone\"}"),
            chunked("text/plain", b"{\"sub\":\"\xff\",\"preferred_username\":\"someone\"}"),
            length("text/plain", b"{\"preferred_username\":\"some\xffone\"}"),
            chunked("application/json", b"{\"preferred_username\":\"some\xffone\"}"),
            length("text/html", b"<html>not this</html>"),
            chunked("text/html", b"<html>not this</html>"),
            length("application/json", b"{\"name\":\"x\"} trailing"),
            length("application/json; charset=iso-8859-1", b"{\"name\":\"Ren\xe9\"}"),
            length("application/json", b"\xef\xbb\xbf{\"name\":\"x\"}"),
            length("application/json", b""),
        ];
        for case in cases {
            let answer = Arc::new(case);
            let sent = answer.clone();
            let address = fake::server(move |_, _, stream| {
                let _ = stream.write_all(&sent);
            });
            let agent = crate::subsonic::make_agent_without_redirects();
            let theirs: Option<Person> = agent.get(&address).call().unwrap().into_body().read_json().ok();
            let ours: Option<Person> = Agent(agent).send(&Request::get(&address)).unwrap().json().ok();
            assert_eq!(ours, theirs, "for {:?}", String::from_utf8_lossy(&answer));
        }
    }

    #[test]
    fn the_public_services_agent_never_connects_to_this_machine_or_its_network() {
        // A server here, on this machine: asked by its address and by a name that leads to
        // it, it is never reached. What's judged is what the agent connects to.
        let asked = Arc::new(AtomicUsize::new(0));
        let count = asked.clone();
        let address = fake::server(move |_, _, stream| {
            count.fetch_add(1, Ordering::SeqCst);
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        });
        let public = super::public(Duration::from_secs(5));
        let by_name = address.replace("127.0.0.1", "localhost");
        for url in [address.clone(), by_name] {
            assert!(public.send(&Request::get(&format!("{url}/ws/2/release"))).is_err(), "{url}");
        }
        assert_eq!(asked.load(Ordering::SeqCst), 0);
        // The app's other agents still reach it (the music server is often on this network).
        let got = Agent(crate::subsonic::make_agent_without_redirects()).send(&Request::get(&address)).unwrap();
        assert_eq!((got.status, asked.load(Ordering::SeqCst)), (200, 1));
        // A proxy on this machine is the person's own choice: it's reached (asked to connect
        // on to the service, which it then looks up itself; this stand-in can't, which is
        // no matter here).
        let through = Agent(crate::subsonic::make_public_agent_through(&address));
        let _ = through.send(&Request::get("http://musicbrainz.invalid/ws/2/release"));
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    /// A public service that answers each address with a redirect to the next one listed,
    /// and the last with what was asked for; every request written down.
    struct Hops {
        to: Vec<(&'static str, &'static str)>,
        asked: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Http for Hops {
        fn send(&self, request: &Request) -> Result<Response, Failure> {
            self.asked.lock().unwrap().push(request.url.to_string());
            Ok(match self.to.iter().find(|(from, _)| *from == request.url) {
                Some((_, to)) => Response::new(302, vec![("Location".into(), Some(to.to_string()))], || Ok(String::new())),
                None => Response::new(200, Vec::new(), || Ok("ok".into())),
            })
        }
    }

    fn hops(to: Vec<(&'static str, &'static str)>) -> (u16, Vec<String>) {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let public = super::Public { inner: Box::new(Hops { to, asked: asked.clone() }) };
        let status = public.send(&Request::get("https://a.example/start")).unwrap().status;
        let asked = asked.lock().unwrap().clone();
        (status, asked)
    }

    #[test]
    fn a_public_services_redirects_follow_the_rule() {
        // Five redirects from HTTPS to HTTPS on the open internet are followed; a sixth isn't.
        let five = vec![
            ("https://a.example/start", "https://b.example/1"),
            ("https://b.example/1", "/2"),
            ("https://b.example/2", "https://c.example/3"),
            ("https://c.example/3", "https://d.example/4"),
            ("https://d.example/4", "https://e.example/5"),
        ];
        let (status, asked) = hops(five.clone());
        assert_eq!((status, asked.last().map(String::as_str)), (200, Some("https://e.example/5")));
        let mut six = five;
        six.push(("https://e.example/5", "https://f.example/6"));
        let (status, asked) = hops(six);
        assert_eq!((status, asked.len()), (302, 6));
        // Not to plain HTTP, nor anywhere written as on this machine or its network (a name
        // that leads there is the resolver's: see the test above).
        for to in ["http://b.example/", "https://127.0.0.1/", "https://[::1]/", "https://10.0.0.5/", "https://169.254.1.1/", "https://[fe80::1]/", "https://[fd00::1]/", "https://localhost/", "https://user@b.example/"] {
            let (status, asked) = hops(vec![("https://a.example/start", to)]);
            assert_eq!((status, asked.len()), (302, 1), "a redirect to {to}");
        }
        // A relative or "//host" place is resolved against the address that answered, then
        // judged by the same rule.
        // ("?query" is resolved as the old client resolved it: in the place's folder.)
        for (to, lands) in [("//b.example/x", "https://b.example/x"), ("next", "https://a.example/next"), ("?page=2", "https://a.example/?page=2")] {
            let (status, asked) = hops(vec![("https://a.example/start", to)]);
            assert_eq!((status, asked.last().map(String::as_str)), (200, Some(lands)), "a redirect to {to}");
        }
        // "../" and "./" are walked as the old client walked them, then judged.
        for (to, lands) in [("../g", "https://a.example/d/g"), ("./h", "https://a.example/d/e/h"), ("../../../x", "")] {
            let (status, asked) = hops(vec![("https://a.example/start", "/d/e/f"), ("https://a.example/d/e/f", to)]);
            if lands.is_empty() {
                // Above the root: not followed, as the old client didn't.
                assert_eq!((status, asked.len()), (302, 2), "a redirect to {to}");
            } else {
                assert_eq!((status, asked.last().map(String::as_str)), (200, Some(lands)), "a redirect to {to}");
            }
        }
        for to in ["//127.0.0.1/x", "//localhost/x", "//user@b.example/x"] {
            let (status, asked) = hops(vec![("https://a.example/start", to)]);
            assert_eq!((status, asked.len()), (302, 1), "a redirect to {to}");
        }
        // A POST's redirect is its answer.
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let public = super::Public { inner: Box::new(Hops { to: vec![("https://a.example/start", "https://b.example/1")], asked: asked.clone() }) };
        let got = public.send(&Request { method: Method::Post("{}"), ..Request::get("https://a.example/start") }).unwrap();
        assert_eq!((got.status, asked.lock().unwrap().len()), (302, 1));
    }

    /// A server for the writes test: every request on every connection written down by its
    /// first line, and each answered as `answer` says (`None`: the connection is closed
    /// without a word). A connection is let go of after an answer that says
    /// "Connection: close", or one ending "\r\n\r\n{}" (an answer that doesn't say so, from
    /// a server that drops idle connections at once); otherwise kept for the next request.
    fn counting(answer: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{BufRead, BufReader, Read};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (answer, written) = (Arc::new(answer), seen.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (answer, written) = (answer.clone(), written.clone());
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut lines = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut first = String::new();
                        if lines.read_line(&mut first).unwrap_or(0) == 0 {
                            return;
                        }
                        let mut length = 0;
                        loop {
                            let mut line = String::new();
                            if lines.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                                break;
                            }
                            if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                                length = value.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0; length];
                        let _ = lines.read_exact(&mut body);
                        let first = first.trim_end().to_string();
                        written.lock().unwrap().push(first.clone());
                        match answer(&first) {
                            Some(said) => {
                                let _ = stream.write_all(said.as_bytes());
                                if said.contains("Connection: close") || said.ends_with("\r\n\r\n{}") {
                                    return;
                                }
                            }
                            None => return,
                        }
                    }
                });
            }
        });
        (address, seen)
    }

    #[test]
    fn the_agent_sends_a_write_to_lidarr_or_the_sign_in_once_whatever_comes_back() {
        // Lidarr's requests and the sign-in's go out on this agent (`make_agent_without_redirects`,
        // through `Agent`). Whatever answers a POST or a PUT, ureq doesn't send it again: not
        // when busy, not when sent elsewhere, not when told it went to the wrong server, not
        // when the connection is closed on it. (Lidarr's own client asks again once after a
        // redirect to the sign-in, with a fresh pass: that's `nexus_core::lidarr`, by design.)
        let answers = [
            Some("HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\n\r\n"),
            Some("HTTP/1.1 307 Temporary Redirect\r\nLocation: /api/v1/album/monitor\r\nContent-Length: 0\r\n\r\n"),
            Some("HTTP/1.1 308 Permanent Redirect\r\nLocation: /api/v1/album/monitor\r\nContent-Length: 0\r\n\r\n"),
            Some("HTTP/1.1 421 Misdirected Request\r\nContent-Length: 0\r\n\r\n"),
            Some("HTTP/1.1 408 Request Timeout\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
            None,
        ];
        for answer in answers {
            for put in [false, true] {
                let (address, seen) = counting(move |_| answer.map(str::to_string));
                let agent = Agent(crate::subsonic::make_agent_without_redirects());
                let url = format!("{address}/api/v1/album/monitor");
                let body = "{\"albumIds\":[1],\"monitored\":false}";
                let method = if put { Method::Put(body) } else { Method::Post(body) };
                let _ = agent.send(&Request { method, ..Request::get(&url) });
                std::thread::sleep(Duration::from_millis(300));
                assert_eq!(seen.lock().unwrap().len(), 1, "{} answered {answer:?}", if put { "PUT" } else { "POST" });
            }
        }
        // A connection kept from an earlier request, closed by the other end since: the write
        // goes out once, on a new one.
        let (address, seen) = counting(|_| Some("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_string()));
        let agent = Agent(crate::subsonic::make_agent_without_redirects());
        let url = format!("{address}/api/v1/album/monitor");
        let got = agent.send(&Request::get(&url)).unwrap();
        assert_eq!(got.body().as_deref(), Ok("{}"));
        // The server has let the kept connection go.
        std::thread::sleep(Duration::from_millis(100));
        let _ = agent.send(&Request { method: Method::Put("{}"), ..Request::get(&url) });
        std::thread::sleep(Duration::from_millis(300));
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.iter().filter(|first| first.starts_with("PUT")).count(), 1, "{seen:?}");
    }
}
