//! The Windows app's answer to the library's requests: its own HTTP agent.
//!
//! The library (`nexus_core`) opens no connections. Whatever it wants from the network it
//! asks for through `nexus_core::http::Http`, and this is that, on one of the app's agents:
//! the same connections, name lookups, timeouts, certificates and User-Agent as every other
//! request the app makes.

use nexus_core::http::{BODY_LIMIT, Failure, Http, Method, Request, Response, Stream};

pub struct Agent(pub ureq::Agent);

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
}
