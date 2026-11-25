//! Comparing names: folding text, taking it apart into words, trimming what titles have
//! tacked on. Every matching rule in the app is built on these, so every app on an account
//! has to agree on them to the letter.

/// Lower-case, without accents or punctuation, so "Beyoncé" and "beyonce" are the same.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
            'ç' | 'ć' | 'č' => out.push('c'),
            'ď' | 'đ' => out.push('d'),
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => out.push('e'),
            'ğ' => out.push('g'),
            'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' | 'ı' => out.push('i'),
            'ł' => out.push('l'),
            'ñ' | 'ń' | 'ň' => out.push('n'),
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => out.push('o'),
            'ř' => out.push('r'),
            'ś' | 'š' | 'ş' => out.push('s'),
            'ť' | 'ţ' => out.push('t'),
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' | 'ų' => out.push('u'),
            'ý' | 'ÿ' => out.push('y'),
            'ź' | 'ż' | 'ž' => out.push('z'),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ß' => out.push_str("ss"),
            // "don't" and "dont" are the same word.
            '\'' | '’' | '‘' | '`' => {}
            c if c.is_alphanumeric() => out.push(c),
            _ => out.push(' '),
        }
    }
    out
}

/// The words of a piece of text, for comparing: folded, without punctuation.
pub fn words(text: &str) -> Vec<String> {
    let folded: String = fold(text).chars().filter(|c| !matches!(c, '\'' | '’')).collect();
    folded.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(str::to_string).collect()
}

/// Whether two pieces of text are made of the same words, in any order.
pub fn same_words(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    !a.is_empty() && a.iter().all(|word| b.contains(word)) && b.iter().all(|word| a.contains(word))
}

/// A title without what's in brackets after it: "Lonely No More (Acoustic)" is, for finding
/// the song, "Lonely No More".
pub fn base(title: &str) -> &str {
    let cut = title.find(" (").or_else(|| title.find(" [")).unwrap_or(title.len());
    if cut == 0 { title } else { &title[..cut] }
}

/// A title without what's often tacked on to it ("(feat. X)", "- 2011 Remaster", "[Live]"),
/// otherwise as written. A title that's nothing but a bracket ("(Intro)") is left alone.
pub fn undecorated(title: &str) -> String {
    let mut kept = String::new();
    let mut depth = 0usize;
    for c in title.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            c if depth == 0 => kept.push(c),
            _ => {}
        }
    }
    let kept = kept.split(" - ").next().unwrap_or_default().trim();
    if kept.chars().any(char::is_alphanumeric) { kept.to_string() } else { title.trim().to_string() }
}

/// Which kind of recording a title says it is, if it says. A live version isn't the song
/// the playlist asked for, unless that's what it asked for.
pub fn version(title: &str) -> Vec<&'static str> {
    let folded = fold(title);
    let words: Vec<&str> = folded.split_whitespace().collect();
    ["live", "remix", "acoustic", "instrumental", "demo", "karaoke"].into_iter().filter(|kind| words.contains(kind)).collect()
}

/// Whether two artist credits name the same artist, or one is among the other's.
pub fn same_artist(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    !a.is_empty() && !b.is_empty() && (a.iter().all(|word| b.contains(word)) || b.iter().all(|word| a.contains(word)))
}

/// A value made safe to put in a web address.
pub fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_accents_and_punctuation() {
        assert_eq!(fold("Beyoncé — Déjà Vu!"), "beyonce   deja vu ");
        assert_eq!(fold("Don’t Stop"), "dont stop");
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("Marrow & Pine"), "Marrow%20%26%20Pine");
        assert_eq!(encode("Études"), "%C3%89tudes");
    }
}
