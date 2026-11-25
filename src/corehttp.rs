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
        // A body that doesn't arrive whole isn't a failure to answer: for some answers the
        // status is all the library looks at. So the reason is handed over beside it.
        let (body, cut_short) = match response.into_body().read_to_string() {
            Ok(body) => (body, None),
            Err(e) => (String::new(), Some(e.to_string())),
        };
        Ok(Response { status, headers, body, cut_short })
    }
}
