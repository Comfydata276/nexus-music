//! The devices a person has seen before, kept by name so that the list can show them at
//! once the next time (`docs/linking-design.md`, 5.9).
//!
//! Sixteen at the most, the ones seen most lately; each for thirty days after it was last
//! seen; each under the name it last gave. None of it is secret. The app keeps the list
//! (in its settings); the rule for what stays in it is here.

use serde::{Deserialize, Serialize};

/// The most devices whose names are remembered.
pub const MOST: usize = 16;
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
}

/// The list with one seen now put first: the same device under a new name is the same
/// device; none unseen for [`DAYS`]; no more than [`MOST`].
pub fn remembered(mut list: Vec<Seen>, seen: Option<(&str, &str)>, day: u64) -> Vec<Seen> {
    if let Some((id, name)) = seen {
        list.retain(|known| known.id != id);
        list.insert(0, Seen { id: id.to_string(), name: name.to_string(), day });
    }
    list.retain(|known| day.saturating_sub(known.day) <= DAYS);
    list.truncate(MOST);
    list
}

/// A list as an app kept it, held to the list's own bounds before anything is done with
/// it: whatever a settings file has come to hold (written by another version, or by hand),
/// no more than [`MOST`] are taken, the ones seen most lately; each device once, as it was
/// last seen; and each name no longer than a device can give.
pub fn loaded(mut list: Vec<Seen>) -> Vec<Seen> {
    // (A stable sort: of two seen on one day, the one the list had first stays first.)
    list.sort_by(|one, other| other.day.cmp(&one.day));
    let mut kept: Vec<Seen> = Vec::new();
    for seen in list {
        if kept.len() < MOST && !seen.id.is_empty() && seen.id.len() <= MOST_ID && !kept.iter().any(|known| known.id == seen.id) {
            kept.push(Seen { name: super::text::cut(&seen.name, super::message::MOST_TEXT), ..seen });
        }
    }
    kept
}

/// The longest an id is taken to be: longer than any device gives (22 characters).
const MOST_ID: usize = 64;

/// The list without the ones unseen for [`DAYS`], if there are any such: `None` if it is
/// as it should be. (A device that is never seen again is forgotten by the calendar, not
/// only when another is seen.)
pub fn without_the_forgotten(list: &[Seen], day: u64) -> Option<Vec<Seen>> {
    let kept: Vec<Seen> = list.iter().filter(|known| day.saturating_sub(known.day) <= DAYS).cloned().collect();
    (kept.len() != list.len()).then_some(kept)
}

/// Whether seeing this device under this name today is news to the list: it is new, has a
/// new name, or was last seen on another day. (So the list is written once a day for
/// each device, not at every look.)
pub fn is_news(list: &[Seen], id: &str, name: &str, day: u64) -> bool {
    !list.iter().any(|seen| seen.id == id && seen.name == name && seen.day == day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(id: &str, name: &str, day: u64) -> Seen {
        Seen { id: id.into(), name: name.into(), day }
    }

    #[test]
    fn devices_are_remembered_by_name_up_to_sixteen_and_for_thirty_days() {
        // One seen now goes first; under a new name it is the same device.
        let list = remembered(vec![seen("a", "Study PC", 100), seen("b", "Laptop", 90)], Some(("b", "Kitchen laptop")), 101);
        assert_eq!(list, vec![seen("b", "Kitchen laptop", 101), seen("a", "Study PC", 100)]);
        // Thirty days unseen, it is kept; thirty-one, forgotten.
        assert_eq!(remembered(vec![seen("a", "Study PC", 100)], None, 130).len(), 1);
        assert_eq!(remembered(vec![seen("a", "Study PC", 100)], None, 131).len(), 0);
        // Sixteen at the most: the ones seen longest ago make room.
        let many: Vec<Seen> = (0..20).map(|n| seen(&format!("d{n}"), "PC", 100)).collect();
        let list = remembered(many, Some(("new", "New PC")), 100);
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
        assert!(!is_news(&list, "a", "Study PC", 100));
        assert!(is_news(&list, "a", "Study PC", 101) && is_news(&list, "a", "Den PC", 100) && is_news(&list, "b", "Laptop", 100));
        // As it is kept in a settings file, and read back from one written before `day` was there.
        assert_eq!(serde_json::to_string(&list[0]).unwrap(), r#"{"id":"a","name":"Study PC","day":100}"#);
        assert_eq!(serde_json::from_str::<Seen>(r#"{"id":"a","name":"Study PC"}"#).unwrap(), seen("a", "Study PC", 0));
    }
}
