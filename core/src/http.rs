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
    body: Box<dyn FnOnce() -> Result<String, String> + Send>,
}

impl Response {
    /// `body` waits for what the answer said and hands it over, or says why it didn't
    /// arrive whole, under whatever limits the app puts on reading one.
    pub fn new(status: u16, headers: Vec<(String, Option<String>)>, body: impl FnOnce() -> Result<String, String> + Send + 'static) -> Response {
        Response { status, headers, body: Box::new(body) }
    }

    /// The first header of that name, if it is there and is text. Where a name is sent
    /// twice the first is the one that counts, and a first that can't be read is not passed
    /// over for a later one that can.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).and_then(|(_, value)| value.as_deref())
    }

    /// Waits for what it said. The words, if it didn't arrive whole, are the app's.
    pub fn body(self) -> Result<String, String> {
        (self.body)()
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
