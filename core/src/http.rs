//! How this library asks for something on the network: it doesn't; the app does.
//!
//! Each service here is given an [`Http`] by the app. On Windows that is one of the app's own
//! agents; on Android it will be the app's own client, reached through a callback. The
//! library never opens a connection, so it carries no HTTP or TLS code, and every request it
//! wants made passes through whatever the app does for requests: its timeouts, its
//! certificates, its name for itself, its rules about when a request may be made at all.

use std::time::Duration;

/// One request, whole.
#[derive(Debug, Clone, PartialEq)]
pub struct Request<'a> {
    /// "GET", "POST" or "PUT".
    pub method: &'a str,
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    /// Text to send.
    pub body: Option<&'a str>,
    /// The longest the whole call may take, where that's shorter than the app's usual limit.
    pub patience: Option<Duration>,
}

impl<'a> Request<'a> {
    pub fn get(url: &'a str) -> Request<'a> {
        Request { method: "GET", url, headers: &[], body: None, patience: None }
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
    pub headers: Vec<(String, String)>,
    body: Box<dyn FnOnce() -> Result<String, String> + Send>,
}

impl Response {
    /// `body` waits for what the answer said and hands it over, or says why it didn't
    /// arrive whole, under whatever limits the app puts on reading one.
    pub fn new(status: u16, headers: Vec<(String, String)>, body: impl FnOnce() -> Result<String, String> + Send + 'static) -> Response {
        Response { status, headers, body: Box::new(body) }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(known, _)| known.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
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
