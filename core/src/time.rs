//! Reading the times a server sends, and setting a device's clock beside the server's.
//!
//! Times that devices compare with each other (which of two saved queues is the newer,
//! which choice was made last) are only as good as the clocks they were read from. The
//! server's is the one every device can see: each answer carries it, as `Date`. So a device
//! measures how far its own clock is from the server's at each exchange, shares nothing while
//! the two are more than five minutes apart, and otherwise allows for the difference.
//!
//! The library has no clock: every time here is handed in.

use chrono::DateTime;

/// How far a device's clock may be from the server's and still be shared from, in seconds.
pub const CLOCK_SLACK: i64 = 5 * 60;

/// Reads a time as a Subsonic server writes it, e.g. "2024-03-09T10:15:00Z", in seconds
/// since 1970; 0 if it isn't one.
pub fn parse_time(text: &str) -> i64 {
    DateTime::parse_from_rfc3339(text).map(|time| time.timestamp()).unwrap_or(0)
}

/// Reads an answer's `Date` header, e.g. "Tue, 24 Nov 2026 09:15:00 GMT", in seconds since
/// 1970. `None` if it isn't a date: then there is nothing to compare the clock with.
pub fn http_date(text: &str) -> Option<i64> {
    DateTime::parse_from_rfc2822(text.trim()).ok().map(|time| time.timestamp())
}

/// The server's clock minus this device's, in seconds: the server's `Date` on an answer,
/// against the device's clock as that answer arrived. `None` when the server gave no date.
pub fn offset(date: Option<i64>, received: i64) -> Option<i64> {
    date.map(|date| date - received)
}

/// Whether the device's clock is too far from the server's to share anything from. A server
/// (or a proxy in front of it) that gives no date can't be compared with, and the clock is
/// then taken to be right.
pub fn clock_wrong(offset: Option<i64>) -> bool {
    offset.is_some_and(|offset| offset.abs() > CLOCK_SLACK)
}

/// A time on this device's clock, as the server's clock would have had it.
pub fn aligned(at: i64, offset: Option<i64>) -> i64 {
    at + offset.unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_server_times() {
        assert_eq!(parse_time("1970-01-02T00:00:00Z"), 86400);
        assert_eq!(parse_time("1970-01-02T00:00:00.123456789Z"), 86400);
        assert_eq!(parse_time("1970-01-02T01:00:00+01:00"), 86400);
        assert_eq!(parse_time(""), 0);
        assert_eq!(parse_time("yesterday"), 0);
    }

    #[test]
    fn reads_the_date_on_an_answer() {
        assert_eq!(http_date("Fri, 02 Jan 1970 00:00:00 GMT"), Some(86400));
        assert_eq!(http_date(" Tue, 24 Nov 2026 09:15:00 GMT "), Some(1_795_511_700));
        assert_eq!(http_date(""), None);
        assert_eq!(http_date("soon"), None);
    }

    #[test]
    fn the_clock_is_wrong_more_than_five_minutes_from_the_servers() {
        // The server says 10:00:00 as the device's clock says 11:00:00: an hour ahead.
        let ahead = offset(Some(36_000), 39_600);
        assert_eq!(ahead, Some(-3_600));
        assert!(clock_wrong(ahead) && clock_wrong(offset(Some(39_600), 36_000)));
        // Five minutes exactly is still allowed; a second more is not.
        assert!(!clock_wrong(Some(300)) && !clock_wrong(Some(-300)));
        assert!(clock_wrong(Some(301)) && clock_wrong(Some(-301)));
        // No date, nothing to compare with.
        assert!(!clock_wrong(offset(None, 39_600)));
        // What the device calls 11:00:20, the server calls 10:00:20.
        assert_eq!(aligned(39_620, ahead), 36_020);
        assert_eq!(aligned(39_620, None), 39_620);
    }
}
