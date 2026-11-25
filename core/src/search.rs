//! How well what was typed matches a name: the scoring behind search that forgives typing
//! mistakes.
//!
//! The copy of the library that is searched, and going through it, are each app's own. This
//! is what they have to agree on for the same words to find the same things in the same
//! order: how many slips of the keyboard lie between two words, how like a library's word a
//! typed one is, and what a song, album or artist scores against everything typed.

/// How many single-letter slips (a letter wrong, missing, extra, or two swapped) separate
/// two words; anything over `limit` comes back as `limit + 1`.
pub fn slips(a: &[char], b: &[char], limit: usize) -> usize {
    if a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }
    let mut before: Vec<usize> = vec![0; b.len() + 1];
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current: Vec<usize> = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        let mut least = current[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (previous[j] + 1).min(current[j - 1] + 1).min(previous[j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(before[j - 2] + 1);
            }
            current[j] = best;
            least = least.min(best);
        }
        if least > limit {
            return limit + 1;
        }
        std::mem::swap(&mut before, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()].min(limit + 1)
}

/// How well a typed word matches a word from the library, from 0 (not at all) to 1 (the
/// same word). The start of a word counts nearly as much, since the rest may yet be typed;
/// a slip or two is forgiven in words long enough for that not to match everything.
pub fn likeness(typed: &[char], word: &[char]) -> f32 {
    if typed == word {
        return 1.0;
    }
    if word.starts_with(typed) {
        return 0.85 + 0.1 * typed.len() as f32 / word.len() as f32;
    }
    let limit = match typed.len() {
        0..=3 => return 0.0,
        4..=7 => 1,
        _ => 2,
    };
    // A short word is only taken for a mistyping of one that starts the same way. Without
    // that, four or five letters are near enough to a good many words that have nothing to
    // do with them: "home" would find "Someday".
    if typed.len() <= 5 && typed.first() != word.first() {
        return 0.0;
    }
    let whole = slips(typed, word, limit);
    if whole <= limit {
        return if whole == 1 { 0.75 } else { 0.6 };
    }
    // A mistyped start of a longer word: "masi" for "maisie".
    let mut best = limit + 1;
    for length in typed.len().saturating_sub(1)..=(typed.len() + 1).min(word.len().saturating_sub(1)) {
        best = best.min(slips(typed, &word[..length], limit));
    }
    // Worth much less than a whole word that's nearly right: it's a guess on a guess.
    match best {
        1 => 0.4,
        2 if limit == 2 => 0.3,
        _ => 0.0,
    }
}

/// One thing's match against what was typed.
#[derive(Default, Clone, Copy)]
pub struct Match {
    /// Overall, counting every field.
    pub score: f32,
    /// Counting the name (or title) alone; 0 if some typed word isn't in it.
    pub name: f32,
}

/// Scores something whose name is `name` and which also answers to `others` (an artist's or
/// album's words, each with how much they count), against the typed words. `scores[i][w]`
/// is how well typed word `i` matches library word `w`.
pub fn rate(scores: &[Vec<f32>], name: &[u32], others: &[(&[u32], f32)]) -> Option<Match> {
    let mut total = 0.0;
    let mut name_total = 0.0;
    let mut in_name = true;
    // Which of the name's words were what a typed word matched best.
    let mut used = 0u64;
    for typed in scores {
        let (mut best_name, mut at) = (0.0f32, usize::MAX);
        for (position, &word) in name.iter().enumerate() {
            if typed[word as usize] > best_name {
                best_name = typed[word as usize];
                at = position;
            }
        }
        let mut best = best_name;
        for (words, weight) in others {
            for &word in *words {
                best = best.max(typed[word as usize] * weight);
            }
        }
        if best <= 0.0 {
            return None;
        }
        total += best;
        if best_name > 0.0 {
            name_total += best_name;
            if best_name >= best && at < 64 {
                used |= 1 << at;
            }
        } else {
            in_name = false;
        }
    }
    let typed = scores.len() as f32;
    // Bonuses for the typed words making up more of the name: all of it, or its start.
    let covered = if name.is_empty() { 0.0 } else { used.count_ones() as f32 / name.len() as f32 };
    let in_order = name.len() >= scores.len() && scores.iter().zip(name).all(|(typed, &word)| typed[word as usize] >= 0.85);
    let exact = in_order && name.len() == scores.len() && scores.iter().zip(name).all(|(typed, &word)| typed[word as usize] >= 1.0);
    let bonus = 0.3 * covered + if exact { 0.5 } else { 0.0 } + if in_order { 0.2 } else { 0.0 };
    // ...which counts for less the shakier the match with the name is.
    let bonus = bonus * name_total / typed;
    Some(Match {
        score: total / typed + bonus,
        name: if in_name { name_total / typed + bonus } else { 0.0 },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_slips() {
        let chars = |text: &str| text.chars().collect::<Vec<char>>();
        assert_eq!(slips(&chars("masie"), &chars("maisie"), 2), 1);
        assert_eq!(slips(&chars("pyscho"), &chars("psycho"), 2), 1);
        assert_eq!(slips(&chars("queen"), &chars("queen"), 1), 0);
        assert_eq!(slips(&chars("abc"), &chars("xyz"), 1), 2);
    }
}
