//! Every bound the backend holds a caller to, as a number, in this one place (design 7.8 and
//! 13.4; experiment E6 is their test, in `tests/e6.rs`).
//!
//! The proxy comes first: it stops a body over one megabyte itself (design 6.2, its
//! `client_max_body_size 1m`), and takes a whole body before the backend sees a byte of it.
//! These bounds hold whatever the proxy does, and for anything that reaches the container
//! another way.

use std::time::Duration;

/// The whole head of a request (its line and every header), and a sign-in's body, must arrive
/// within this, counted from when the connection is taken, before it may take a work slot (the
/// review of S8, C1: short, as the proxy sends a request whole at once). A connection carries
/// one request, so this is also how long one may stay open saying nothing.
pub const HEAD_DEADLINE: Duration = Duration::from_secs(3);

/// The most a request's head may be, in bytes, and the most read before its end is found.
/// Past it, the proxy is given the gate's one answer and anyone else nothing.
pub const HEAD_MAX_BYTES: usize = 16 * 1024;

/// The most headers a request may carry.
pub const HEAD_MAX_HEADERS: usize = 64;

/// The whole body of a request must arrive within this, counted from when its head has.
pub const BODY_DEADLINE: Duration = Duration::from_secs(10);

/// The most a request's body may be, in bytes: one mebibyte, the proxy's `1m`. A body that
/// says it is larger is refused before a byte of it is read; one that turns out larger is
/// refused at the first byte past this.
pub const BODY_MAX_BYTES: usize = 1024 * 1024;

/// The most requests served at once (work slots), once their heads are read.
pub const CONNECTIONS_MAX: usize = 64;

/// Of [`CONNECTIONS_MAX`], the slots only a request that is someone's may take (the review of
/// S8, C1; the lead's ruling of 28 Nov 15:38): one whose session resolves, a known device
/// signing in to its own account, or a live one-time code. A flood of strangers can take the
/// other slots, never these.
pub const RESERVED_SLOTS: usize = 16;

/// How long a write may make no progress (the caller not reading) before the connection is
/// closed.
pub const WRITE_DEADLINE: Duration = Duration::from_secs(5);

/// How long what answers a request may take, once its body is in. Past it the answer is 503.
pub const ANSWER_DEADLINE: Duration = Duration::from_secs(10);

/// After a bare answer, whatever more the caller sends is discarded for at most this long
/// and this many bytes, so that closing does not reset the connection under the answer.
pub const LINGER_DEADLINE: Duration = Duration::from_secs(2);
pub const LINGER_MAX_BYTES: usize = BODY_MAX_BYTES + HEAD_MAX_BYTES;

/// How many connections past [`CONNECTIONS_MAX`] may be open at once: having their heads read,
/// or only being given the gate's answer. Past these, no more is taken: one waits in the
/// kernel's backlog.
pub const TURNED_AWAY_MAX: usize = 16;

/// How many connections the kernel holds, ready, before they are taken. With every one of
/// those held too, the kernel answers a new one with nothing, and the caller (the proxy)
/// tries again until its own deadline to connect (60 s in nginx unless set), then answers its
/// caller 504. With `error_log /dev/null crit;` in the proxy's rule, that is not logged.
pub const LISTEN_BACKLOG: u32 = 1024;

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
        // A head comes whole at once from the proxy: its deadline is short (C1).
        assert!(HEAD_DEADLINE <= Duration::from_secs(5));
        assert!(CONNECTIONS_MAX > RESERVED_SLOTS && RESERVED_SLOTS > 0);
    }
}
