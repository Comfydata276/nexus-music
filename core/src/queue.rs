//! The queue, when the list it was started from changes.
//!
//! A queue started from a playlist or an album goes on following it: a song added to the
//! list (here or on another device) joins the queue, one removed leaves it, a reordered list
//! is played in its new order. This is the working out, on the songs and their places alone;
//! the player, and noticing that a list has changed, are each app's own.

use crate::model::Song;

/// Compares a list of song ids as it was with how it is now. Returns where each old entry
/// has gone (`None` if it's no longer there), and which of the new entries are arrivals.
/// A song that's in the list more than once is matched copy for copy.
pub fn compare_lists(old: &[String], new: &[String]) -> (Vec<Option<usize>>, Vec<usize>) {
    let mut places: std::collections::HashMap<&str, std::collections::VecDeque<usize>> = Default::default();
    for (index, id) in new.iter().enumerate() {
        places.entry(id.as_str()).or_default().push_back(index);
    }
    let moved: Vec<Option<usize>> =
        old.iter().map(|id| places.get_mut(id.as_str()).and_then(|at| at.pop_front())).collect();
    let mut arrivals: Vec<usize> = places.into_values().flatten().collect();
    arrivals.sort_unstable();
    (moved, arrivals)
}

/// The queue: its songs, where each sits in the list it came from, the order they play in
/// (as positions in `songs`), and which of those is playing.
#[derive(Debug, Clone, PartialEq)]
pub struct Lineup {
    pub songs: Vec<Song>,
    pub origins: Vec<Option<usize>>,
    pub order: Vec<usize>,
    pub cursor: usize,
}

/// Works out the queue after the list it came from has changed.
///
/// Songs that left the list leave the queue, except the one playing. Songs that joined it
/// are added: in their place if the queue plays in the list's order, somewhere among what's
/// still to come if it's shuffled. Songs the list was reordered around take up the new
/// order. Songs that were never part of the list (added to the queue by hand) keep their
/// place after whatever they followed.
///
/// `moved` and `arrivals` are from `compare_lists`; `view` is the new list in the order its
/// page shows it; `song_at` gives the song at a position in the new list; `pick(n)` chooses
/// a number below `n`.
pub fn follow_list(
    lineup: Lineup,
    shuffled: bool,
    moved: &[Option<usize>],
    arrivals: &[usize],
    view: &[usize],
    song_at: &dyn Fn(usize) -> Option<Song>,
    pick: &mut dyn FnMut(usize) -> usize,
) -> Lineup {
    let Lineup { songs, origins, order, cursor } = lineup;
    let Some(&current) = order.get(cursor) else {
        return Lineup { songs, origins, order, cursor };
    };
    // Where each entry now sits in the list, and which entries go.
    let now: Vec<Option<usize>> =
        origins.iter().map(|origin| origin.and_then(|at| moved.get(at).copied().flatten())).collect();
    let gone = |entry: usize| origins[entry].is_some() && now[entry].is_none() && entry != current;
    // The arrangement to keep for entries that aren't in the list: the playing order, or
    // when shuffled the order underneath the shuffle.
    let base: Vec<usize> = if shuffled { (0..songs.len()).collect() } else { order.clone() };
    let mut followers: std::collections::HashMap<Option<usize>, Vec<usize>> = Default::default();
    let mut holder: std::collections::HashMap<usize, usize> = Default::default();
    let mut last = None;
    for &entry in base.iter().filter(|&&entry| !gone(entry)) {
        match now[entry] {
            Some(place) => {
                holder.insert(place, entry);
                last = Some(entry);
            }
            None => followers.entry(last).or_default().push(entry),
        }
    }
    enum Item {
        Kept(usize),
        Arrival(usize),
    }
    let mut items: Vec<Item> = followers.remove(&None).unwrap_or_default().into_iter().map(Item::Kept).collect();
    for &place in view {
        if let Some(&entry) = holder.get(&place) {
            items.push(Item::Kept(entry));
            items.extend(followers.remove(&Some(entry)).unwrap_or_default().into_iter().map(Item::Kept));
        } else if arrivals.binary_search(&place).is_ok() && song_at(place).is_some() {
            items.push(Item::Arrival(place));
        }
    }
    // The new set of songs, and where each old entry went in it.
    let mut new_songs = Vec::with_capacity(items.len());
    let mut new_origins = Vec::with_capacity(items.len());
    let mut went: Vec<Option<usize>> = vec![None; songs.len()];
    let mut added = Vec::new();
    for item in &items {
        match *item {
            Item::Kept(entry) => {
                went[entry] = Some(new_songs.len());
                new_songs.push(songs[entry].clone());
                new_origins.push(now[entry]);
            }
            Item::Arrival(place) => {
                added.push(new_songs.len());
                new_songs.extend(song_at(place));
                new_origins.push(Some(place));
            }
        }
    }
    let playing = went[current].unwrap_or(0);
    let (new_order, new_cursor) = if shuffled {
        let mut new_order: Vec<usize> = order.iter().filter_map(|&entry| went[entry]).collect();
        let at = new_order.iter().position(|&entry| entry == playing).unwrap_or(0);
        for entry in added {
            // Somewhere after the song that's playing.
            let slot = at + 1 + pick(new_order.len() - at);
            new_order.insert(slot.min(new_order.len()), entry);
        }
        (new_order, at)
    } else {
        ((0..new_songs.len()).collect(), playing)
    };
    Lineup { songs: new_songs, origins: new_origins, order: new_order, cursor: new_cursor }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(id: &str) -> Song {
        Song { id: id.to_string(), ..Default::default() }
    }

    /// A queue of these songs, each from the same place in its list, playing the one at `cursor`.
    fn lineup(ids: &[&str], cursor: usize) -> Lineup {
        Lineup {
            songs: ids.iter().map(|id| named(id)).collect(),
            origins: (0..ids.len()).map(Some).collect(),
            order: (0..ids.len()).collect(),
            cursor,
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    /// The queue in playing order, with the song that's playing in brackets.
    fn playing_order(lineup: &Lineup) -> String {
        lineup
            .order
            .iter()
            .enumerate()
            .map(|(at, &entry)| {
                let id = &lineup.songs[entry].id;
                if at == lineup.cursor { format!("[{id}]") } else { id.clone() }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn follow(lineup: Lineup, shuffled: bool, old: &[&str], new: &[&str], view: Option<&[usize]>) -> Lineup {
        let (moved, arrivals) = compare_lists(&ids(old), &ids(new));
        let in_order: Vec<usize> = (0..new.len()).collect();
        let song_at = |place: usize| new.get(place).map(|id| named(id));
        // "Random" here means as late as possible, to keep the tests the same every run.
        follow_list(lineup, shuffled, &moved, &arrivals, view.unwrap_or(&in_order), &song_at, &mut |n| n - 1)
    }

    #[test]
    fn compares_lists_copy_for_copy() {
        let (moved, arrivals) = compare_lists(&ids(&["a", "b", "a", "c"]), &ids(&["b", "a", "d", "a", "a"]));
        assert_eq!(moved, [Some(1), Some(0), Some(3), None]);
        assert_eq!(arrivals, [2, 4]);
    }

    #[test]
    fn the_queue_follows_its_list() {
        let list = ["a", "b", "c", "d"];
        // Songs added to the list join the queue in their place; removed ones leave it.
        let after = follow(lineup(&list, 1), false, &list, &["a", "b", "x", "c", "y"], None);
        assert_eq!(playing_order(&after), "a [b] x c y");
        assert_eq!(after.origins, [Some(0), Some(1), Some(2), Some(3), Some(4)]);
        // The song that's playing stays even if it was removed, and what followed it still does.
        let after = follow(lineup(&list, 1), false, &list, &["a", "c", "d"], None);
        assert_eq!(playing_order(&after), "a [b] c d");
        assert_eq!(after.origins, [Some(0), None, Some(1), Some(2)]);
        // A reordered list is played in its new order from the current song on.
        let after = follow(lineup(&list, 1), false, &list, &["d", "b", "a", "c"], None);
        assert_eq!(playing_order(&after), "d [b] a c");
        // The page may show the list in another order (newest first, say): that's the order.
        // (The queue was started from that view, so its entries came from places 2, 1, 0.)
        let mut queue = lineup(&["c", "b", "a"], 0);
        queue.origins = vec![Some(2), Some(1), Some(0)];
        let after = follow(queue, false, &["a", "b", "c"], &["a", "b", "c", "x"], Some(&[3, 2, 1, 0]));
        assert_eq!(playing_order(&after), "x [c] b a");
    }

    #[test]
    fn songs_queued_by_hand_keep_their_place() {
        // "q" was added to play after "b"; it isn't part of the list.
        let mut queue = lineup(&["a", "b", "q", "c"], 1);
        queue.origins = vec![Some(0), Some(1), None, Some(2)];
        let after = follow(queue, false, &["a", "b", "c"], &["a", "b", "x", "c"], None);
        assert_eq!(playing_order(&after), "a [b] q x c");
        // A queue played from part of a list (a filtered view) doesn't gain the rest of it.
        let mut queue = lineup(&["b", "d"], 0);
        queue.origins = vec![Some(1), Some(3)];
        let after = follow(queue, false, &["a", "b", "c", "d"], &["a", "b", "c", "d", "e"], None);
        assert_eq!(playing_order(&after), "[b] d e");
    }

    #[test]
    fn a_shuffled_queue_takes_arrivals_among_what_is_to_come() {
        let list = ["a", "b", "c", "d"];
        let mut queue = lineup(&list, 1);
        queue.order = vec![2, 0, 3, 1];
        // Playing "a"; "c" has been played, "d" and "b" are to come. "b" is removed, "x" added.
        let after = follow(queue, true, &list, &["a", "x", "c", "d"], None);
        assert_eq!(playing_order(&after), "c [a] d x");
        // Underneath the shuffle the songs are in the list's order, for when shuffle is turned off.
        let titles: Vec<&str> = after.songs.iter().map(|song| song.id.as_str()).collect();
        assert_eq!(titles, ["a", "x", "c", "d"]);
    }
}
