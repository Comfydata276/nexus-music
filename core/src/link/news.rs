//! News: a device says, unasked, that what it is doing has changed, so that the others
//! need not wait for their next look to find out (`docs/linking-design.md`, 5.5).
//!
//! News is a tell with no `re`, sent to every other place in the room. It is **only ever a
//! reason to look**: the device that receives it asks, and shows the answer
//! ([`super::steer::Peer::news`]). It is never shown or acted on itself, because anything
//! on the way can deliver it again later.
//!
//! **Bounded.** A device sends news only when what its brief state says has changed, and
//! then no more often than once in [`gap_ms`]: two seconds, or longer in a crowded room, so
//! that news to every place never comes to more than two and a half frames a second, half
//! of what the relay lets one place send. A change inside the gap is told when the gap is
//! over, once, as the state then stands.

/// The least time between one round of news and the next, for a room with this many other
/// places in it.
pub fn gap_ms(others: usize) -> u64 {
    (others as u64).saturating_mul(400).max(2_000)
}

/// What this device last told the others, and when.
#[derive(Debug, Default)]
pub struct News {
    /// A fingerprint of the state last told.
    told: Option<u64>,
    told_ms: Option<u64>,
}

impl News {
    /// Whether to send news now. `print` is a fingerprint of what the brief state says;
    /// `others` is how many other places are in the room. Called as often as the app
    /// likes; says yes only when the state has changed since the last news and the gap has
    /// passed, and takes that as the news having been sent.
    pub fn due(&mut self, print: u64, others: usize, now_ms: u64) -> bool {
        if others == 0 || self.told == Some(print) {
            return false;
        }
        if self.told_ms.is_some_and(|last| now_ms.checked_sub(last).is_none_or(|since| since < gap_ms(others))) {
            return false;
        }
        (self.told, self.told_ms) = (Some(print), Some(now_ms));
        true
    }

    /// The room is a new one, or this device has a new place in it: whoever is there has
    /// been told nothing.
    pub fn forget(&mut self) {
        *self = News::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn news_is_sent_when_the_state_changes_and_no_more_often_than_the_gap() {
        let mut news = News::default();
        // Alone in the room there is nobody to tell.
        assert!(!news.due(1, 0, 0));
        assert!(news.due(1, 1, 0));
        // The same state again is not news.
        assert!(!news.due(1, 1, 10_000));
        // A change long after the last news is told at once.
        assert!(news.due(2, 1, 10_500));
        // A change inside the gap waits for the gap to pass, and is told as it then stands.
        assert!(!news.due(3, 1, 11_000));
        assert!(!news.due(4, 1, 12_499));
        assert!(news.due(4, 1, 12_500));
        assert!(!news.due(4, 1, 20_000));
        // A clock that reads earlier is no reason to send more.
        assert!(!news.due(5, 1, 11_000));
    }

    #[test]
    fn a_crowded_room_is_told_less_often() {
        assert_eq!((gap_ms(1), gap_ms(5), gap_ms(6), gap_ms(15)), (2_000, 2_000, 2_400, 6_000));
        // Never more than two and a half frames a second in all.
        for others in 1..=15u64 {
            assert!(others * 1_000 <= gap_ms(others as usize) * 5 / 2, "{others}");
        }
        let mut news = News::default();
        assert!(news.due(1, 15, 0));
        assert!(!news.due(2, 15, 5_999));
        assert!(news.due(2, 15, 6_000));
        news.forget();
        assert!(news.due(2, 15, 6_001));
    }
}
