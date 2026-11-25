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
    fn a_device_is_news_once_a_day_and_when_its_name_changes() {
        let list = vec![seen("a", "Study PC", 100)];
        assert!(!is_news(&list, "a", "Study PC", 100));
        assert!(is_news(&list, "a", "Study PC", 101) && is_news(&list, "a", "Den PC", 100) && is_news(&list, "b", "Laptop", 100));
        // As it is kept in a settings file, and read back from one written before `day` was there.
        assert_eq!(serde_json::to_string(&list[0]).unwrap(), r#"{"id":"a","name":"Study PC","day":100}"#);
        assert_eq!(serde_json::from_str::<Seen>(r#"{"id":"a","name":"Study PC"}"#).unwrap(), seen("a", "Study PC", 0));
    }
}
