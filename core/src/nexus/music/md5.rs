//! RFC 1321 for Subsonic's legacy password-plus-salt wire formula (§9.1).
//! The core has no MD5 dependency and this implementation has no unsalted entry.

use super::{MusicSalt, SALT_LENGTH};
use zeroize::{Zeroize, Zeroizing};

pub(super) fn with_salt(password: &[u8], salt: &MusicSalt) -> Zeroizing<[u8; 16]> {
    let mut state = Zeroizing::new([0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476]);
    let (blocks, rem) = password.as_chunks::<64>();
    for block in blocks {
        compress(&mut state, block);
    }

    // The hex salt is one block. At most one partial password block, the salt,
    // an 0x80 byte and the 64-bit bit count remain.
    let mut tail = Zeroizing::new([0u8; 192]);
    let padded = if rem.len() < 56 { 128 } else { 192 };
    let bytes = rem
        .iter()
        .copied()
        .chain(salt.as_str().bytes())
        .chain(std::iter::once(0x80));
    for (out, byte) in tail.iter_mut().zip(bytes) {
        *out = byte;
    }
    let bits = (password.len() as u64)
        .wrapping_add(SALT_LENGTH as u64)
        .wrapping_mul(8);
    for (out, byte) in tail
        .iter_mut()
        .take(padded)
        .rev()
        .zip(bits.to_le_bytes().into_iter().rev())
    {
        *out = byte;
    }
    let (blocks, _) = tail.as_chunks::<64>();
    for block in blocks.iter().take(padded / 64) {
        compress(&mut state, block);
    }

    let mut digest = Zeroizing::new([0u8; 16]);
    for (bytes, word) in digest.chunks_exact_mut(4).zip(state.iter()) {
        bytes.copy_from_slice(&word.to_le_bytes());
    }
    digest
}

// RFC 1321's T array, shift counts, and the first index / stride through the words.
const K: [[u32; 16]; 4] = [
    [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821,
    ],
    [
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681,
        0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8,
        0x676f02d9, 0x8d2a4c8a,
    ],
    [
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60,
        0xbebfbc70, 0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5,
        0x1fa27cf8, 0xc4ac5665,
    ],
    [
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d,
        0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235,
        0x2ad7d2bb, 0xeb86d391,
    ],
];
const SHIFTS: [[u32; 4]; 4] = [
    [7, 12, 17, 22],
    [5, 9, 14, 20],
    [4, 11, 16, 23],
    [6, 10, 15, 21],
];
const WORDS: [(usize, usize); 4] = [(0, 1), (1, 5), (5, 3), (0, 7)];

fn compress(state: &mut [u32; 4], bytes: &[u8; 64]) {
    let mut words = Zeroizing::new([0u32; 16]);
    let (chunks, _) = bytes.as_chunks::<4>();
    for (word, bytes) in words.iter_mut().zip(chunks) {
        *word = u32::from_le_bytes(*bytes);
    }

    let [mut a, mut b, mut c, mut d] = *state;
    for (round, ((terms, shifts), (first, stride))) in
        K.into_iter().zip(SHIFTS).zip(WORDS).enumerate()
    {
        let terms = terms.into_iter().zip(shifts.into_iter().cycle());
        let words = words.iter().cycle().skip(first).step_by(stride);
        for ((term, shift), &word) in terms.zip(words) {
            let mix = match round {
                0 => (b & c) | (!b & d),
                1 => (b & d) | (c & !d),
                2 => b ^ c ^ d,
                _ => c ^ (b | !d),
            };
            let sum = a.wrapping_add(mix).wrapping_add(term).wrapping_add(word);
            (a, b, c, d) = (d, b.wrapping_add(sum.rotate_left(shift)), b, c);
        }
    }
    for (out, part) in state.iter_mut().zip([a, b, c, d]) {
        *out = out.wrapping_add(part);
    }
    a.zeroize();
    b.zeroize();
    c.zeroize();
    d.zeroize();
}
