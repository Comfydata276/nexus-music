//! The spellings: base32 for a room and a place, base64url for everything else, both
//! without padding, and names cut to length.
//!
//! Each value has **one** spelling. Reading refuses any other: a different length, a
//! letter outside the alphabet, padding, or spare bits that are not clear. So two devices
//! that hold the same bytes always write the same text, and a relay that checks spellings
//! (it does) checks the same thing.

use super::{Nonce, Place};

const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn spell(bytes: &[u8], alphabet: &[u8], bits: u32) -> String {
    let mut text = String::with_capacity((bytes.len() * 8).div_ceil(bits as usize));
    let (mut held, mut count) = (0u32, 0u32);
    let mask = (1u32 << bits) - 1;
    let letter = |value: u32| alphabet.get((value & mask) as usize).map_or('?', |byte| *byte as char);
    for byte in bytes {
        held = (held << 8) | u32::from(*byte);
        count += 8;
        while count >= bits {
            count -= bits;
            text.push(letter(held >> count));
        }
        held &= (1 << count) - 1;
    }
    if count > 0 {
        text.push(letter(held << (bits - count)));
    }
    text
}

/// The bytes a text spells, or `None` if it is not the one spelling of any.
fn read(text: &str, alphabet: &[u8], bits: u32) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * bits as usize / 8);
    let (mut held, mut count) = (0u32, 0u32);
    for letter in text.bytes() {
        let value = alphabet.iter().position(|other| *other == letter)? as u32;
        held = (held << bits) | value;
        count += bits;
        if count >= 8 {
            count -= 8;
            bytes.push((held >> count) as u8);
            held &= (1 << count) - 1;
        }
    }
    // What is left over is less than a byte, is all clear, and is no more than one letter's
    // worth: a whole spare letter is padding by another name.
    (held == 0 && count < bits).then_some(bytes)
}

pub fn base32(bytes: &[u8]) -> String {
    spell(bytes, BASE32, 5)
}

pub fn from_base32(text: &str) -> Option<Vec<u8>> {
    read(text, BASE32, 5)
}

pub fn base64(bytes: &[u8]) -> String {
    spell(bytes, BASE64, 6)
}

pub fn from_base64(text: &str) -> Option<Vec<u8>> {
    read(text, BASE64, 6)
}

/// A place as the relay is told it: 26 letters.
pub fn place(place: &Place) -> String {
    base32(place)
}

pub fn read_place(text: &str) -> Option<Place> {
    from_base32(text)?.try_into().ok()
}

/// A nonce as an answer names it: 32 characters.
pub fn nonce(nonce: &Nonce) -> String {
    base64(nonce)
}

pub fn read_nonce(text: &str) -> Option<Nonce> {
    from_base64(text)?.try_into().ok()
}

/// Sixteen bytes as 22 characters: a device's id.
pub fn read_id(text: &str) -> Option<[u8; 16]> {
    from_base64(text)?.try_into().ok()
}

/// A name or a title as it is sent and as it is shown: one line, nothing invisible, and at
/// most `most` characters. Done by the sender, and again by the receiver, which has no
/// reason to believe the sender did.
pub fn cut(text: &str, most: usize) -> String {
    let hidden = |c: char| c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}');
    text.chars().filter(|c| !hidden(*c)).take(most).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spells_as_the_published_examples_do() {
        // RFC 4648, section 10, without the padding and in lower case.
        for (plain, as32, as64) in [("", "", ""), ("f", "my", "Zg"), ("fo", "mzxq", "Zm8"), ("foo", "mzxw6", "Zm9v"), ("foob", "mzxw6yq", "Zm9vYg"), ("fooba", "mzxw6ytb", "Zm9vYmE"), ("foobar", "mzxw6ytboi", "Zm9vYmFy")] {
            assert_eq!((base32(plain.as_bytes()), base64(plain.as_bytes())), (as32.to_string(), as64.to_string()));
            assert_eq!((from_base32(as32).unwrap(), from_base64(as64).unwrap()), (plain.as_bytes().to_vec(), plain.as_bytes().to_vec()));
        }
        assert_eq!(base64(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn every_value_reads_back_and_has_one_spelling() {
        for length in 0..40usize {
            let bytes: Vec<u8> = (0..length).map(|at| (at * 37 + length * 11) as u8).collect();
            assert_eq!(from_base32(&base32(&bytes)).unwrap(), bytes);
            assert_eq!(from_base64(&base64(&bytes)).unwrap(), bytes);
        }
        // Spare bits set, padding, the other case, a letter too many, a letter outside.
        for wrong in ["mz", "my======", "MY", "mya", "m1", "m y"] {
            assert_eq!(from_base32(wrong), None, "{wrong}");
        }
        for wrong in ["Zh", "Zg==", "Zg=", "Z", "Z+8", "Z/8", "Zm 8"] {
            assert_eq!(from_base64(wrong), None, "{wrong}");
        }
        let there = place(&[0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xff]);
        assert_eq!(there, "6dy7f47u6x3pp6hz7l57z7p674");
        assert_eq!(read_place(&there).map(|read| read[15]), Some(0xff));
        assert_eq!(read_place(&there[..25]), None);
        assert_eq!(read_place(&format!("{}5", &there[..25])), None);
        assert_eq!(read_nonce(&nonce(&[7; 24])), Some([7; 24]));
        assert_eq!(read_nonce(&base64(&[7; 23])), None);
    }

    #[test]
    fn cuts_names_to_length_and_to_what_can_be_seen() {
        assert_eq!(cut("Study PC", 80), "Study PC");
        assert_eq!(cut(&"é".repeat(100), 80).chars().count(), 80);
        assert_eq!(cut("a\nb\u{202E}c\u{0}d", 80), "abcd");
        assert_eq!(cut("🎵🎵🎵", 2), "🎵🎵");
    }
}
