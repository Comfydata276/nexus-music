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
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Method<'a> {
    Get,
    Post(&'a str),
    /// (Lidarr's, for when it moves here; nothing in the library makes one yet.)
    Put(&'a str),
}

/// One request, whole.
#[derive(Debug, Clone, PartialEq)]
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
