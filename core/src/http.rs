//! How this library asks for something on the network: it doesn't; the app does.
//!
//! Each service here is given an [`Http`] by the app. On Windows that is one of the app's own
//! agents; on Android it will be the app's own client, reached through a callback. The
//! library never opens a connection, so it carries no HTTP or TLS code, and every request it
//! wants made passes through whatever the app does for requests: its timeouts, its
//! certificates, its name for itself, its rules about when a request may be made at all.

use std::time::Duration;

/// What kind of request, with the text it sends if it sends any. These are the kinds the
/// app makes: there is no asking for a GET with a body or a POST without one, so neither
/// app has to decide what one would mean.
///
/// Shown (`{:?}`) as its name and how long its text is, never the text: that can hold a key,
/// a token or a sign-in's code.
#[derive(Clone, Copy, PartialEq)]
pub enum Method<'a> {
    Get,
    Post(&'a str),
    /// Lidarr's.
    Put(&'a str),
}

impl std::fmt::Debug for Method<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Method::Get => write!(f, "GET"),
            Method::Post(body) => write!(f, "POST ({} bytes)", body.len()),
            Method::Put(body) => write!(f, "PUT ({} bytes)", body.len()),
        }
    }
}

/// One request, whole.
///
/// Shown (`{:?}`) without its secrets, so that one written to a log gives none away: the
/// method and how long its text is, the address without its query, fragment or any name and
/// password in it, and the names of its headers, not their values.
#[derive(Clone, PartialEq)]
pub struct Request<'a> {
    pub method: Method<'a>,
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    /// The longest the whole call may take, where that's shorter than the app's usual limit.
    pub patience: Option<Duration>,
}

impl<'a> Request<'a> {
    pub fn get(url: &'a str) -> Request<'a> {
        Request { method: Method::Get, url, headers: &[], patience: None }
    }
}

impl std::fmt::Debug for Request<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let address = self.url.split(['?', '#']).next().unwrap_or_default();
        let address = match address.split_once("://") {
            Some((scheme, rest)) => {
                let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
                format!("{scheme}://{}{path}", authority.rsplit_once('@').map_or(authority, |(_, host)| host))
            }
            None => address.rsplit_once('@').map_or(address, |(_, rest)| rest).to_string(),
        };
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| *name).collect();
        f.debug_struct("Request").field("method", &self.method).field("url", &address).field("headers", &names).field("patience", &self.patience).finish()
    }
}

/// What came back: the status and headers, which have arrived, and the body, which is only
/// waited for if it's asked for. A status the service counts as an error is still an answer.
///
/// An answer is often judged by its status alone, and nothing need limit how long a body
/// takes: one that is waited for when nobody will read it can hold a lookup open for as
/// long as the other end likes. So the app hands the body over as something to ask for, and
/// one that is never asked for is never waited for (the app lets go of the connection).
pub struct Response {
    pub status: u16,
    /// Every header, in the order sent. A value that isn't text is `None`: it is still
    /// there, and still the first of its name if it was.
    pub headers: Vec<(String, Option<String>)>,
    body: Body,
}

/// A body that hasn't been read, handed over by the app so that the library can read it as
/// text or as JSON, the two ways the app has always read one.
pub trait Stream: Send {
    /// How long the answer says its body is, if it says.
    fn length(&self) -> Option<u64>;
    /// Waits for all of it and hands it over as text, as the app reads text.
    fn text(self: Box<Self>) -> Result<String, String>;
    /// The body as it arrives, no more of it than the app will read of a body
    /// (`BODY_LIMIT`): reading past that is an error.
    fn bytes(self: Box<Self>) -> Box<dyn std::io::Read + Send>;
}

/// The most of a body that is read: the Windows app's limit, ureq's.
pub const BODY_LIMIT: u64 = 10 * 1024 * 1024;
/// A body that says it's no longer than this is read whole before its JSON is read; any
/// other is read as it arrives, so that one that doesn't begin as JSON is given up at once.
/// This is how ureq's `read_json` reads (a third of its limit), which the app used before
/// the library read JSON itself.
const JSON_WHOLE: u64 = BODY_LIMIT / 3;

enum Body {
    Text(Box<dyn FnOnce() -> Result<String, String> + Send>),
    Stream(Box<dyn Stream>),
}

impl Response {
    /// `body` waits for what the answer said and hands it over, or says why it didn't
    /// arrive whole, under whatever limits the app puts on reading one. For an answer that
    /// is only ever text (a stand-in in a test); an app hands over its bodies as a `Stream`.
    pub fn new(status: u16, headers: Vec<(String, Option<String>)>, body: impl FnOnce() -> Result<String, String> + Send + 'static) -> Response {
        Response { status, headers, body: Body::Text(Box::new(body)) }
    }

    /// An answer whose body is read only as it's asked for, as text or as JSON.
    pub fn streamed(status: u16, headers: Vec<(String, Option<String>)>, body: impl Stream + 'static) -> Response {
        Response { status, headers, body: Body::Stream(Box::new(body)) }
    }

    /// The first header of that name, if it is there and is text. Where a name is sent
    /// twice the first is the one that counts, and a first that can't be read is not passed
    /// over for a later one that can.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).and_then(|(_, value)| value.as_deref())
    }

    /// Waits for what it said. The words, if it didn't arrive whole, are the app's.
    pub fn body(self) -> Result<String, String> {
        match self.body {
            Body::Text(read) => read(),
            Body::Stream(stream) => stream.text(),
        }
    }

    /// Reads what it said as JSON, as ureq's `read_json` does: a body that says it's short
    /// is read whole and then parsed; any other is parsed as it arrives, so that one that
    /// begins with something that isn't JSON is given up on without waiting for the rest.
    /// Read as bytes, not text: bytes that aren't UTF-8 matter only where they are read.
    pub fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, String> {
        let stream = match self.body {
            Body::Text(read) => return serde_json::from_str(&read()?).map_err(|e| e.to_string()),
            Body::Stream(stream) => stream,
        };
        if stream.length().is_some_and(|length| length <= JSON_WHOLE) {
            let mut whole = Vec::new();
            std::io::Read::read_to_end(&mut stream.bytes(), &mut whole).map_err(|e| e.to_string())?;
            return serde_json::from_slice(&whole).map_err(|e| e.to_string());
        }
        serde_json::from_reader(stream.bytes()).map_err(|e| e.to_string())
    }
}

/// Why there was no answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// The service couldn't be reached. The words are the app's, to end a sentence with.
    Reach(String),
    /// The app has no more use for the answer (the page was left, the account changed, the
    /// phone went offline): give up what's being done. The Windows app never says this.
    Stopped,
}

/// What `Failure::Stopped` is reported as, by callers that only pass words on.
pub const STOPPED: &str = "Stopped.";

pub trait Http: Send + Sync {
    fn send(&self, request: &Request) -> Result<Response, Failure>;

    /// Waits, as between rationed requests. An app that can be told to stop says
    /// `Failure::Stopped` instead of waiting it out.
    fn pause(&self, wait: Duration) -> Result<(), Failure> {
        std::thread::sleep(wait);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_never_shows_its_secrets() {
        // Lidarr's key, a pass, the sign-in's code, check and token, a password in an address.
        let form = "grant_type=authorization_code&code=the-code-1&code_verifier=the-verifier-2&refresh_token=the-token-3";
        let headers = [("X-Api-Key", "the-key-4"), ("Authorization", "Bearer the-pass-5"), ("Content-Type", "application/x-www-form-urlencoded")];
        let requests = [
            Request { method: Method::Post(form), url: "https://someone:the-password-6@auth.example.org/application/o/token/?state=the-state-7#the-end-8", headers: &headers, patience: None },
            Request { method: Method::Put("{\"key\":\"the-key-4\"}"), url: "http://lidarr.example.org/api/v1/artist/5", headers: &headers, patience: Some(Duration::from_secs(3)) },
            Request { headers: &headers, ..Request::get("http://lidarr.example.org/api/v1/album/lookup?term=the-search-9") },
        ];
        let shown: Vec<String> = requests.iter().map(|request| format!("{request:?} {request:#?} {:?}", request.method)).collect();
        for shown in &shown {
            for secret in ["the-code-1", "the-verifier-2", "the-token-3", "the-key-4", "the-pass-5", "the-password-6", "someone", "the-state-7", "the-end-8", "the-search-9"] {
                assert!(!shown.contains(secret), "{secret} in {shown}");
            }
        }
        // What it does show: the method, where, the headers' names and how long the body is.
        assert!(shown[0].starts_with(&format!("Request {{ method: POST ({} bytes), url: \"https://auth.example.org/application/o/token/\", headers: [\"X-Api-Key\", \"Authorization\", \"Content-Type\"]", form.len())), "{}", shown[0]);
        assert!(shown[1].contains("PUT (19 bytes)") && shown[1].contains("\"http://lidarr.example.org/api/v1/artist/5\"") && shown[1].contains("patience: Some(3s)"), "{}", shown[1]);
        assert!(shown[2].contains("method: GET, url: \"http://lidarr.example.org/api/v1/album/lookup\""), "{}", shown[2]);
    }
}
