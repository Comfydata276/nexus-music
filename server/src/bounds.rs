//! Every bound the backend holds a caller to, as a number, in this one place (design 7.8 and
//! 13.4; experiment E6 is their test, in `tests/e6.rs`).
//!
//! The proxy comes first: it stops a body over one megabyte itself (design 6.2, its
//! `client_max_body_size 1m`), and takes a whole body before the backend sees a byte of it.
//! These bounds hold whatever the proxy does, and for anything that reaches the container
//! another way.

use std::time::Duration;

/// The whole head of a request (its line and every header) must arrive within this, counted
/// from when the backend starts waiting for it. An idle connection between requests is
/// waiting for a head, so it is closed after this too.
pub const HEAD_DEADLINE: Duration = Duration::from_secs(10);

/// The most a request's head may be, in bytes. Past it the connection is answered 431 and
/// closed.
pub const HEAD_MAX_BYTES: usize = 16 * 1024;

/// The most headers a request may carry.
pub const HEAD_MAX_HEADERS: usize = 64;

/// The whole body of a request must arrive within this, counted from when its head has.
pub const BODY_DEADLINE: Duration = Duration::from_secs(10);

/// The most a request's body may be, in bytes: one mebibyte, the proxy's `1m`. A body that
/// says it is larger is refused before a byte of it is read; one that turns out larger is
/// refused at the first byte past this.
pub const BODY_MAX_BYTES: usize = 1024 * 1024;

/// The most connections open at once. One more is closed as soon as it is accepted, without
/// a byte read or written.
pub const CONNECTIONS_MAX: usize = 64;

/// How long a write may make no progress (the caller not reading) before the connection is
/// closed.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(5);

/// How long what answers a request may take, once its body is in. Past it the answer is 503.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(10);

/// How often the log says how many requests of each kind were refused, when any were.
pub const COUNTS_EVERY: Duration = Duration::from_secs(15 * 60);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bounds_fit_together() {
        // hyper will not take a head buffer smaller than this.
        assert!(HEAD_MAX_BYTES >= 8192);
        // The largest call is a batch of plays (design 9.1); the proxy's own limit is the same.
        assert_eq!(BODY_MAX_BYTES, 1 << 20);
        // A write that makes no progress is told apart from a head that never comes.
        assert!(WRITE_DEADLINE < HEAD_DEADLINE);
        assert!(CONNECTIONS_MAX > 0);
    }
}
