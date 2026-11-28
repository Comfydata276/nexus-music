use super::{BadMessage, InternalAddress, read_text, wire};
use crate::nexus::{kept::Fingerprint, proto::BackendId};
use chacha20poly1305::{
    Key, XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use std::fmt;
use zeroize::Zeroizing;

// The design does not say what a key may contain. Keep visible ASCII for the
// X-Api-Key header and bound it within the password-bearing grant request.
pub const LIDARR_KEY_MAX_BYTES: usize = 512;
pub const LIDARR_NONCE_BYTES: usize = 24;
const TAG_BYTES: usize = 16;

pub struct LidarrKey {
    text: Zeroizing<String>,
    fingerprint: Fingerprint,
}

impl LidarrKey {
    pub fn new(text: &str) -> Option<Self> {
        if text.is_empty()
            || text.len() > LIDARR_KEY_MAX_BYTES
            || !text.bytes().all(|b| b.is_ascii_graphic())
        {
            return None;
        }
        let fingerprint = Sha256::new()
            .chain_update(b"Nexus Lidarr grant key v1\0")
            .chain_update(text.as_bytes())
            .finalize();
        Some(Self {
            text: Zeroizing::new(text.to_owned()),
            fingerprint: Fingerprint::from_bytes(fingerprint.into()),
        })
    }

    /// Supply only to the configured Lidarr transport or the admin request writer.
    pub fn expose(&self) -> &str {
        &self.text
    }

    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }
}

impl fmt::Debug for LidarrKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LidarrKey(..)")
    }
}

impl PartialEq for LidarrKey {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
    }
}

impl Eq for LidarrKey {}

impl Serialize for LidarrKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}

impl wire::Value for LidarrKey {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        read_text(raw, Self::new)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LidarrBinding {
    pub backend: BackendId,
    /// Exact configured NEXUS_LIDARR address, from container settings.
    pub address: InternalAddress,
    pub revision: u64,
}

impl LidarrBinding {
    fn associated_data(&self) -> Vec<u8> {
        // The design does not say how to encode the binding. The address follows
        // fixed-width fields, so it consumes the rest without ambiguity.
        let mut bytes = b"Nexus Lidarr key v1\0".to_vec();
        bytes.extend_from_slice(self.backend.as_bytes());
        bytes.extend_from_slice(&self.revision.to_be_bytes());
        bytes.extend_from_slice(self.address.as_str().as_bytes());
        bytes
    }
}

pub struct LidarrSealingKey(Zeroizing<[u8; 32]>);

impl LidarrSealingKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(self.0.as_slice()))
    }

    /// Supply a fresh random nonce never used with this key, including for music.
    pub(in crate::nexus) fn seal(
        &self,
        binding: &LidarrBinding,
        key: &LidarrKey,
        nonce: [u8; LIDARR_NONCE_BYTES],
    ) -> Option<SealedLidarrKey> {
        let encrypted = self
            .cipher()
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: key.expose().as_bytes(),
                    aad: &binding.associated_data(),
                },
            )
            .ok()?;
        let mut bytes = Vec::with_capacity(LIDARR_NONCE_BYTES + encrypted.len());
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&encrypted);
        Some(SealedLidarrKey(bytes))
    }

    /// Build the binding from the current backend ID, configured address and
    /// apart-store revision. A changed address needs a newly granted key.
    /// The design does not say what happens if a previous address is restored.
    /// When the keeper observes an address change, retire the old revision so
    /// going back to it cannot make an old key usable again.
    pub fn open(&self, binding: &LidarrBinding, sealed: &SealedLidarrKey) -> Option<LidarrKey> {
        let (nonce, msg) = sealed.0.split_first_chunk::<LIDARR_NONCE_BYTES>()?;
        let bytes = Zeroizing::new(
            self.cipher()
                .decrypt(
                    XNonce::from_slice(nonce),
                    Payload { msg, aad: &binding.associated_data() },
                )
                .ok()?,
        );
        LidarrKey::new(std::str::from_utf8(&bytes).ok()?)
    }
}

impl fmt::Debug for LidarrSealingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LidarrSealingKey(..)")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SealedLidarrKey(Vec<u8>);

impl SealedLidarrKey {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (LIDARR_NONCE_BYTES + TAG_BYTES + 1..=LIDARR_NONCE_BYTES + TAG_BYTES + LIDARR_KEY_MAX_BYTES)
            .contains(&bytes.len())
            .then(|| Self(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SealedLidarrKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SealedLidarrKey(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lidarr_seal_requires_the_current_backend_address_revision_and_key() {
        let sealing = LidarrSealingKey::new([5; 32]);
        let key = LidarrKey::new("made-up-key").unwrap();
        let bound = LidarrBinding {
            backend: BackendId::from_bytes([7; 16]),
            address: InternalAddress::new("http://lidarr.example.test:8686").unwrap(),
            revision: 2,
        };
        let sealed = sealing.seal(&bound, &key, [9; LIDARR_NONCE_BYTES]).unwrap();
        let read = SealedLidarrKey::from_bytes(sealed.as_bytes()).unwrap();
        assert_eq!(sealing.open(&bound, &read), Some(key));
        for moved in [
            LidarrBinding { backend: BackendId::from_bytes([8; 16]), ..bound.clone() },
            LidarrBinding { address: InternalAddress::new("http://next.example.test:8686").unwrap(), ..bound.clone() },
            LidarrBinding { revision: 3, ..bound.clone() },
        ] {
            assert!(sealing.open(&moved, &read).is_none());
        }
        assert!(LidarrSealingKey::new([6; 32]).open(&bound, &read).is_none());
        let mut tampered = read.as_bytes().to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        let tampered = SealedLidarrKey::from_bytes(&tampered).unwrap();
        assert!(sealing.open(&bound, &tampered).is_none());
        assert!(SealedLidarrKey::from_bytes(&[0; LIDARR_NONCE_BYTES]).is_none());
        assert_eq!(format!("{sealing:?} {sealed:?}"), "LidarrSealingKey(..) SealedLidarrKey(..)");
    }
}
