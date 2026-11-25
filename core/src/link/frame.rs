//! A frame: what one device sends another through the relay, sealed so that nothing on the
//! way can read it, change it, or pass it off as something else.
//!
//! | Bytes | |
//! |---|---|
//! | 4 | `"NML4"` |
//! | 1 | the kind: 1 an ask, 2 a tell |
//! | 24 | the nonce, random for every frame |
//! | the rest | the message, sealed with XChaCha20-Poly1305 |
//!
//! The sealing key is made from the account's secret, so only its devices can seal or
//! open. Bound into the seal with the message are the kind, the place of the device that
//! sent it and the place of the device it is for; the receiver takes the first from the
//! relay and uses its own for the second. So a frame the relay delivers to another place,
//! hands back to its sender, relabels, passes off as the other kind, or carries over to a
//! later run of the app (which has a new place) does not open.
//!
//! What a seal does **not** do is stop the same frame being delivered again to the place
//! it was for: that is for [`super::rule`] (an ask) and [`super::steer`] (a tell).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use super::secret::Secret;
use super::{Nonce, Place, text};

pub const ASK: u8 = 1;
pub const TELL: u8 = 2;

/// The most a message may be, in bytes, of either kind: one `adopt`, or one state with a
/// queue in it. What each message may be is narrower, and is [`super::message`]'s to say.
pub use super::command::MOST_MESSAGE;

const MAGIC: &[u8; 4] = b"NML4";
const HEAD: usize = 4 + 1 + 24;
const TAG: usize = 16;

fn most(kind: u8) -> Option<usize> {
    matches!(kind, ASK | TELL).then_some(MOST_MESSAGE)
}

/// What is made from the secret: the key frames are sealed with, and the room's name at
/// the relay. Neither says anything about the secret, or about the other.
pub struct Keys {
    seal: Zeroizing<[u8; 32]>,
    room: String,
}

fn derived(secret: &Secret, info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    // (HKDF-SHA256 gives up to 8,160 bytes; 32 is never refused.)
    let _ = Hkdf::<Sha256>::new(None, secret.bytes()).expand(info, out.as_mut_slice());
    out
}

impl Keys {
    pub fn of(secret: &Secret) -> Keys {
        Keys { seal: derived(secret, b"nexus-music link 4 seal"), room: text::base32(derived(secret, b"nexus-music link 4 room").as_slice()) }
    }

    /// The room's name: 52 letters.
    pub fn room(&self) -> &str {
        &self.room
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(self.seal.as_slice()))
    }
}

fn bound(kind: u8, from: &Place, to: &Place) -> [u8; 37] {
    let mut with = [0u8; 37];
    with[..4].copy_from_slice(MAGIC);
    with[4] = kind;
    with[5..21].copy_from_slice(from);
    with[21..].copy_from_slice(to);
    with
}

/// Seals a message as a frame, as text for the relay. `nonce` is 24 bytes from the
/// system's source of randomness, never used before. `None` for a kind there isn't or a
/// message over its size.
pub fn seal(keys: &Keys, kind: u8, nonce: &Nonce, from: &Place, to: &Place, message: &[u8]) -> Option<String> {
    if message.len() > most(kind)? {
        return None;
    }
    let sealed = keys.cipher().encrypt(XNonce::from_slice(nonce), Payload { msg: message, aad: &bound(kind, from, to) }).ok()?;
    let mut frame = Vec::with_capacity(HEAD + sealed.len());
    frame.extend_from_slice(MAGIC);
    frame.push(kind);
    frame.extend_from_slice(nonce);
    frame.extend_from_slice(&sealed);
    Some(text::base64(&frame))
}

/// A frame that opened.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub kind: u8,
    pub nonce: Nonce,
    pub message: Vec<u8>,
}

/// Opens a frame the relay delivered as coming from `from`, at the place `to` (this
/// device's own). `None` for anything that does not open, whatever the reason: it is
/// dropped, and never answered.
pub fn open(keys: &Keys, frame: &str, from: &Place, to: &Place) -> Option<Opened> {
    // Nothing longer than the longest frame there can be is even decoded.
    if frame.len() > (HEAD + MOST_MESSAGE + TAG).div_ceil(3) * 4 {
        return None;
    }
    let bytes = text::from_base64(frame)?;
    let (head, sealed) = bytes.split_at_checked(HEAD)?;
    let (magic, rest) = head.split_at_checked(4)?;
    let (kind, nonce) = rest.split_first()?;
    let nonce: Nonce = nonce.try_into().ok()?;
    if magic != MAGIC || sealed.len() < TAG || sealed.len() - TAG > most(*kind)? {
        return None;
    }
    let message = keys.cipher().decrypt(XNonce::from_slice(&nonce), Payload { msg: sealed, aad: &bound(*kind, from, to) }).ok()?;
    Some(Opened { kind: *kind, nonce, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len() / 2).map(|at| u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).unwrap()).collect()
    }

    fn run(from: u8, length: usize) -> Vec<u8> {
        (0..length).map(|at| from.wrapping_add(at as u8)).collect()
    }

    fn keys() -> Keys {
        Keys::of(&Secret::new(run(0x20, 32).try_into().unwrap()))
    }

    const PC: Place = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f];
    const PHONE: Place = [0xf0, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0xfb, 0xfc, 0xfd, 0xfe, 0xff];
    const LOOK: &[u8] = br#"{"t":"look","want":"brief"}"#;
    const LOOK_SENT: &str = "Tk1MNAGgoaKjpKWmp6ipqqusra6vsLGys7S1trcUuaE5S13eLMstpiVnQszIhnutDXboHaDw4qjJQ5-Tu8us8U7tlcaHYClu";

    #[test]
    fn hkdf_is_the_published_one() {
        // RFC 5869, appendix A, test cases 1 and 3.
        let mut out = [0u8; 42];
        Hkdf::<Sha256>::new(Some(&hex("000102030405060708090a0b0c")), &[0x0b; 22]).expand(&hex("f0f1f2f3f4f5f6f7f8f9"), &mut out).unwrap();
        assert_eq!(out.to_vec(), hex("3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"));
        Hkdf::<Sha256>::new(None, &[0x0b; 22]).expand(&[], &mut out).unwrap();
        assert_eq!(out.to_vec(), hex("8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"));
    }

    #[test]
    fn the_seal_is_the_published_one() {
        // draft-irtf-cfrg-xchacha-03, appendix A.3.1.
        let cipher = XChaCha20Poly1305::new(Key::from_slice(&run(0x80, 32)));
        let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
        let sealed = cipher.encrypt(XNonce::from_slice(&run(0x40, 24)), Payload { msg: plain, aad: &hex("50515253c0c1c2c3c4c5c6c7") }).unwrap();
        assert_eq!(
            sealed,
            hex(concat!(
                "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b452",
                "2f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff921f9664c97637da9768812f615c68b13b52e",
                "c0875924c1c7987947deafd8780acf49"
            ))
        );
    }

    /// The bytes `docs/linking-vectors.py` works out by itself, in Python.
    #[test]
    fn the_design_s_own_bytes() {
        let keys = keys();
        assert_eq!(keys.seal.to_vec(), hex("f910379eb11ec698962649fedca639eb6abac0f94011cf859909f270c9e3180f"));
        assert_eq!(keys.room(), "gmpsssbzsbysp73rfdjtchkolqpwl2uvsjde7s2hiioejtponbaq");
        assert_eq!(seal(&keys, ASK, &run(0xa0, 24).try_into().unwrap(), &PC, &PHONE, LOOK).as_deref(), Some(LOOK_SENT));
        let pause = br#"{"t":"do","do":"pause","at":[7,12],"with":{}}"#;
        assert_eq!(
            seal(&keys, ASK, &run(0x60, 24).try_into().unwrap(), &PC, &PHONE, pause).as_deref(),
            Some("Tk1MNAFgYWJjZGVmZ2hpamtsbW5vcHFyc3R1dnft9CyUZendEejVQJ6K7S2U4xTd1u3iYHe57yPUanyVaeB7FEYlWTNlg1ocauQ_XiO7Qn5DsuyB9gskSBiE")
        );
        let done = br#"{"re":"YGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3","ok":true,"device":"gYKDhIWGh4iJiouMjY6PkA","name":"Phone","at":[8,12],"by":"YGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3","playing":false,"title":"Song","artist":"Artist"}"#;
        let sent = "Tk1MNAJAQUJDREVGR0hJSktMTU5PUFFSU1RVVlcHoswmaA_HoL4XXfUdi4etKoq_qfi7l-CeFFmlHb8cJ14duGClmzMm8KpL1pYdss-aOtrtveytVJ32aDWdpYVhcYM17W9oJafftz6s228pBBlnt2B7Ox6_gCGPxy9XVn9yHz8VVzdL0wVAL5DUxnaT5_iHdKuk0xPe9-2_5x75IgPgFY2Q701G_KCNQDbavAGeFmvzb5pbRt1lxPhfS1H1kjfUPW1NuLyH4Xa-0tLqOxvqhmIATB_zl3Mrfm96c69pnldjkDRnfs20c9-LmveDlJJIGsk_9WTI";
        assert_eq!(seal(&keys, TELL, &run(0x40, 24).try_into().unwrap(), &PHONE, &PC, done).as_deref(), Some(sent));
        assert_eq!(open(&keys, sent, &PHONE, &PC), Some(Opened { kind: TELL, nonce: run(0x40, 24).try_into().unwrap(), message: done.to_vec() }));
    }

    #[test]
    fn a_frame_opens_only_where_it_was_sealed_for() {
        let keys = keys();
        assert_eq!(open(&keys, LOOK_SENT, &PC, &PHONE).map(|opened| (opened.kind, opened.message)), Some((ASK, LOOK.to_vec())));
        // Handed back to its sender; delivered to another place; said to be from another;
        // carried over to a later run, which has a new place.
        assert_eq!(open(&keys, LOOK_SENT, &PHONE, &PC), None);
        assert_eq!(open(&keys, LOOK_SENT, &PC, &[0; 16]), None);
        assert_eq!(open(&keys, LOOK_SENT, &[0; 16], &PHONE), None);
        assert_eq!(open(&keys, LOOK_SENT, &PC, &PC), None);
        // Sealed under another secret.
        assert_eq!(open(&Keys::of(&Secret::new([0; 32])), LOOK_SENT, &PC, &PHONE), None);
    }

    #[test]
    fn a_frame_changed_in_any_bit_cut_short_or_passed_off_as_the_other_kind_does_not_open() {
        let keys = keys();
        let bytes = text::from_base64(LOOK_SENT).unwrap();
        for at in 0..bytes.len() {
            for bit in 0..8 {
                let mut changed = bytes.clone();
                changed[at] ^= 1 << bit;
                assert_eq!(open(&keys, &text::base64(&changed), &PC, &PHONE), None, "byte {at}, bit {bit}");
            }
        }
        for length in 0..bytes.len() {
            assert_eq!(open(&keys, &text::base64(&bytes[..length]), &PC, &PHONE), None, "cut to {length}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(open(&keys, &text::base64(&longer), &PC, &PHONE), None);
        // Not text the relay would have carried at all.
        for wrong in ["", "=", "not base64", &format!("{LOOK_SENT}="), &LOOK_SENT.replace('-', "+")] {
            assert_eq!(open(&keys, wrong, &PC, &PHONE), None, "{wrong}");
        }
    }

    #[test]
    fn a_message_over_its_size_is_neither_sealed_nor_opened() {
        let keys = keys();
        let nonce = [9u8; 24];
        assert!(seal(&keys, ASK, &nonce, &PC, &PHONE, &vec![b'x'; MOST_MESSAGE]).is_some());
        assert_eq!(seal(&keys, ASK, &nonce, &PC, &PHONE, &vec![b'x'; MOST_MESSAGE + 1]), None);
        assert_eq!(seal(&keys, TELL, &nonce, &PC, &PHONE, &vec![b'x'; MOST_MESSAGE + 1]), None);
        assert_eq!(seal(&keys, 3, &nonce, &PC, &PHONE, b"x"), None);
        // An ask as long as a tell may be: sealed by a device that does not keep the rule, it
        // is refused by its length before it is refused by anything else.
        let big = vec![b'x'; MOST_MESSAGE];
        let as_tell = seal(&keys, TELL, &nonce, &PC, &PHONE, &big).unwrap();
        assert!(open(&keys, &as_tell, &PC, &PHONE).is_some());
        let mut bytes = text::from_base64(&as_tell).unwrap();
        bytes[4] = ASK;
        assert_eq!(open(&keys, &text::base64(&bytes), &PC, &PHONE), None);
        assert_eq!(open(&keys, &"A".repeat(400_000), &PC, &PHONE), None);
    }

    #[test]
    fn the_room_and_the_key_say_nothing_of_each_other_and_change_with_the_secret() {
        let (one, other) = (keys(), Keys::of(&Secret::new([1; 32])));
        assert_ne!(one.room(), other.room());
        assert_ne!(one.seal.as_slice(), other.seal.as_slice());
        assert_ne!(text::base32(one.seal.as_slice()), one.room());
        assert_eq!(one.room().len(), 52);
    }
}
