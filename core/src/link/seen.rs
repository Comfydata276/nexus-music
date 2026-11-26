//! The devices a person has seen before, kept by name so that the list can show them at
//! once the next time (`docs/linking-design.md`, 5.9).
//!
//! Sixteen at the most for an account, the ones seen most lately; each for thirty days
//! after it was last seen; each under the name it last gave. None of it is secret. The app
//! keeps the list (in its settings); the rule for what stays in it is here.
//!
//! **A device belongs to the account it was seen under.** Each is kept with the sign-in
//! that saw it, and is listed for that sign-in only: another account signed in on the
//! same PC or phone does not see the names of somebody else's devices, and they are there
//! again when the first account signs back in.

use serde::{Deserialize, Serialize};

/// The most devices whose names are remembered for one account.
pub const MOST: usize = 16;
/// The most remembered for all the accounts that have signed in here, together.
pub const MOST_IN_ALL: usize = 64;
/// How many days one is remembered without being seen.
pub const DAYS: u64 = 30;

/// One of the person's other devices, as last seen: the id it gives itself (22 characters
/// of base64url), the name the person gave it, and the day it was last seen (days since
/// 1970, on the device's own calendar: this is housekeeping, not part of what devices say
/// to each other).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Seen {
    pub id: String,
    pub name: String,
    pub day: u64,
    /// The sign-in it was seen under, as the app names a sign-in (what it gives each
    /// tick). One kept without it (by a version before this) is nobody's, and is not kept.
    pub account: String,
}

/// The list with one seen now, under this account, put first: the same device under a new
/// name is the same device; none unseen for [`DAYS`]; no more than [`MOST`] of one account
/// nor [`MOST_IN_ALL`] together, the ones seen longest ago making room.
pub fn remembered(mut list: Vec<Seen>, seen: Option<(&str, &str)>, day: u64, account: &str) -> Vec<Seen> {
    if let Some((id, name)) = seen {
        list.retain(|known| !(known.id == id && known.account == account));
        list.insert(0, Seen { id: id.to_string(), name: name.to_string(), day, account: account.to_string() });
    }
    list.retain(|known| day.saturating_sub(known.day) <= DAYS);
    bounded(list)
}

/// No more than [`MOST`] of one account, nor [`MOST_IN_ALL`] together: the first of each
/// are kept, as the list has them.
fn bounded(list: Vec<Seen>) -> Vec<Seen> {
    let mut kept: Vec<Seen> = Vec::new();
    for seen in list {
        if kept.len() < MOST_IN_ALL && kept.iter().filter(|known| known.account == seen.account).count() < MOST {
            kept.push(seen);
        }
    }
    kept
}

/// The devices of the list that were seen under this account.
pub fn of<'a>(list: &'a [Seen], account: &'a str) -> impl Iterator<Item = &'a Seen> {
    list.iter().filter(move |known| known.account == account)
}

/// A list as an app kept it, held to the list's own bounds before anything is done with
/// it: whatever a settings file has come to hold (written by another version, or by hand),
/// no more than [`MOST`] are taken for an account and [`MOST_IN_ALL`] together, the ones
/// seen most lately; each device once for an account, as it was last seen; each name no
/// longer than a device can give; and none that does not say whose it is.
pub fn loaded(mut list: Vec<Seen>) -> Vec<Seen> {
    // (A stable sort: of two seen on one day, the one the list had first stays first.)
    list.sort_by(|one, other| other.day.cmp(&one.day));
    let mut kept: Vec<Seen> = Vec::new();
    for seen in list {
        let whose = !seen.account.is_empty() && seen.account.len() <= MOST_ACCOUNT;
        if whose && !seen.id.is_empty() && seen.id.len() <= MOST_ID && !kept.iter().any(|known| known.id == seen.id && known.account == seen.account) {
            kept.push(Seen { name: super::text::cut(&seen.name, super::message::MOST_TEXT), ..seen });
        }
    }
    bounded(kept)
}

/// The longest the name of a sign-in is taken to be.
const MOST_ACCOUNT: usize = 512;

/// The longest an id is taken to be: longer than any device gives (22 characters).
const MOST_ID: usize = 64;

/// The list without the ones unseen for [`DAYS`], if there are any such: `None` if it is
/// as it should be. (A device that is never seen again is forgotten by the calendar, not
/// only when another is seen.)
pub fn without_the_forgotten(list: &[Seen], day: u64) -> Option<Vec<Seen>> {
    let kept: Vec<Seen> = list.iter().filter(|known| day.saturating_sub(known.day) <= DAYS).cloned().collect();
    (kept.len() != list.len()).then_some(kept)
}

/// Whether seeing this device under this name today, under this account, is news to the
/// list: it is new, has a new name, or was last seen on another day. (So the list is
/// written once a day for each device, not at every look.)
pub fn is_news(list: &[Seen], id: &str, name: &str, day: u64, account: &str) -> bool {
    !list.iter().any(|seen| seen.id == id && seen.name == name && seen.day == day && seen.account == account)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "the account";

    fn seen(id: &str, name: &str, day: u64) -> Seen {
        Seen { id: id.into(), name: name.into(), day, account: ME.into() }
    }

    /// Devices seen under one account are that account's: another, signed in on the same
    /// PC or phone, is not shown them, and neither takes the other's room in the list.
    #[test]
    fn a_device_belongs_to_the_account_it_was_seen_under() {
        let theirs = |id: &str, name: &str, day: u64| Seen { account: "somebody else".into(), ..seen(id, name, day) };
        let list = remembered(vec![seen("a", "Study PC", 100)], Some(("a", "Their study PC")), 101, "somebody else");
        // The same id under another account is another entry: each has its own name for it.
        assert_eq!(list, vec![theirs("a", "Their study PC", 101), seen("a", "Study PC", 100)]);
        assert_eq!(of(&list, ME).map(|known| known.name.as_str()).collect::<Vec<_>>(), ["Study PC"]);
        assert_eq!(of(&list, "somebody else").map(|known| known.name.as_str()).collect::<Vec<_>>(), ["Their study PC"]);
        assert_eq!(of(&list, "a third").count(), 0);
        assert!(is_news(&list, "a", "Study PC", 100, "a third") && !is_news(&list, "a", "Study PC", 100, ME));
        // Sixteen for each, whatever the other has; sixty-four together.
        let mut many: Vec<Seen> = (0..20).map(|n| seen(&format!("d{n}"), "PC", 100)).collect();
        many.extend((0..20).map(|n| theirs(&format!("t{n}"), "PC", 100)));
        let list = remembered(many, None, 100, ME);
        assert_eq!((of(&list, ME).count(), of(&list, "somebody else").count()), (MOST, MOST));
        let crowd: Vec<Seen> = (0..100).map(|n| Seen { account: format!("account {}", n / 2), ..seen(&format!("d{n}"), "PC", 100) }).collect();
        assert_eq!((loaded(crowd.clone()).len(), remembered(crowd, None, 100, ME).len()), (MOST_IN_ALL, MOST_IN_ALL));
        // One a file holds without saying whose it is (kept by a version before this), or
        // with something far too long to be a sign-in's name, is nobody's: not kept.
        let unowned = vec![Seen { account: String::new(), ..seen("a", "Study PC", 100) }, Seen { account: "x".repeat(10_000), ..seen("b", "Laptop", 100) }, seen("c", "Den PC", 100)];
        assert_eq!(loaded(unowned), vec![seen("c", "Den PC", 100)]);
    }

    #[test]
    fn devices_are_remembered_by_name_up_to_sixteen_and_for_thirty_days() {
        // One seen now goes first; under a new name it is the same device.
        let list = remembered(vec![seen("a", "Study PC", 100), seen("b", "Laptop", 90)], Some(("b", "Kitchen laptop")), 101, ME);
        assert_eq!(list, vec![seen("b", "Kitchen laptop", 101), seen("a", "Study PC", 100)]);
        // Thirty days unseen, it is kept; thirty-one, forgotten.
        assert_eq!(remembered(vec![seen("a", "Study PC", 100)], None, 130, ME).len(), 1);
        assert_eq!(remembered(vec![seen("a", "Study PC", 100)], None, 131, ME).len(), 0);
        // Sixteen at the most: the ones seen longest ago make room.
        let many: Vec<Seen> = (0..20).map(|n| seen(&format!("d{n}"), "PC", 100)).collect();
        let list = remembered(many, Some(("new", "New PC")), 100, ME);
        assert_eq!((list.len(), list[0].id.as_str(), list.iter().any(|known| known.id == "d19")), (16, "new", false));
    }

    #[test]
    fn a_list_read_from_a_file_is_held_to_the_list_s_bounds() {
        // Ten thousand of them, some twice, some with no id, one with a name as long as a book.
        let mut kept: Vec<Seen> = (0..10_000u64).map(|n| seen(&format!("d{}", n % 6_000), "PC", n)).collect();
        kept.push(seen("", "No id", 99_999));
        kept.push(seen("long", &"n".repeat(100_000), 99_998));
        kept.push(seen(&"i".repeat(1_000), "An id as long as a page", 99_997));
        let list = loaded(kept);
        assert_eq!((list.len(), list[0].id.as_str(), list[0].name.chars().count() <= super::super::message::MOST_TEXT), (MOST, "long", true));
        // The ones seen most lately, each once, as last seen.
        assert_eq!(list[1..4], [seen("d3999", "PC", 9_999), seen("d3998", "PC", 9_998), seen("d3997", "PC", 9_997)]);
        // One that is already as it should be is left as it is.
        let good = vec![seen("b", "Kitchen laptop", 101), seen("a", "Study PC", 100)];
        assert_eq!(loaded(good.clone()), good);
        // And the calendar forgets, whether or not another device is ever seen.
        assert_eq!(without_the_forgotten(&good, 130), None);
        assert_eq!(without_the_forgotten(&good, 131), Some(vec![seen("b", "Kitchen laptop", 101)]));
        assert_eq!(without_the_forgotten(&good, 500), Some(Vec::new()));
    }

    #[test]
    fn a_device_is_news_once_a_day_and_when_its_name_changes() {
        let list = vec![seen("a", "Study PC", 100)];
        assert!(!is_news(&list, "a", "Study PC", 100, ME));
        assert!(is_news(&list, "a", "Study PC", 101, ME) && is_news(&list, "a", "Den PC", 100, ME) && is_news(&list, "b", "Laptop", 100, ME));
        // As it is kept in a settings file, and read back from one written before `day`,
        // or whose it is, was there.
        assert_eq!(serde_json::to_string(&list[0]).unwrap(), r#"{"id":"a","name":"Study PC","day":100,"account":"the account"}"#);
        assert_eq!(serde_json::from_str::<Seen>(r#"{"id":"a","name":"Study PC"}"#).unwrap(), Seen { account: String::new(), ..seen("a", "Study PC", 0) });
    }
}
