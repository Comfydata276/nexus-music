//! The Windows app's answer to the library's requests: its own HTTP agent.
//!
//! The library (`nexus_core`) opens no connections. Whatever it wants from the network it
//! asks for through `nexus_core::http::Http`, and this is that, on one of the app's agents:
//! the same connections, name lookups, timeouts, certificates and User-Agent as every other
//! request the app makes.

use nexus_core::http::{Failure, Http, Request, Response};

pub struct Agent(pub ureq::Agent);

impl Http for Agent {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let unreachable = |e: ureq::Error| Failure::Reach(e.to_string());
        let response = match request.body {
            None => {
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
            Some(body) => {
                let mut ask = if request.method == "PUT" { self.0.put(request.url) } else { self.0.post(request.url) };
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
        let headers = response.headers().iter().filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))).collect();
        // The body is read only if the library asks for it, and then as the app has always
        // read one. An answer the library judges by its status is not waited on: nothing
        // limits how long a body may take, and a body nobody asks for is dropped unread.
        let mut body = response.into_body();
        Ok(Response::new(status, headers, move || body.read_to_string().map_err(|e| e.to_string())))
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
    }

    impl Http for StandIn {
        fn send(&self, request: &Request) -> Result<Response, Failure> {
            let url = request.url.replacen("https://musicbrainz.org", &self.address, 1);
            self.agent.send(&Request { url: &url, ..request.clone() })
        }

        fn pause(&self, _: Duration) -> Result<(), Failure> {
            Ok(())
        }
    }

    fn musicbrainz(address: String) -> MusicBrainz {
        let http = Arc::new(StandIn { agent: Agent(crate::subsonic::make_agent()), address });
        MusicBrainz::new(http.clone(), ListenBrainz::new(http))
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
}
