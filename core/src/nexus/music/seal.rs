use super::*;
use chacha20poly1305::{
    Key, XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};

pub const NONCE_BYTES: usize = 24;
const TAG_BYTES: usize = 16;
// The design does not say how large a music password can be. Use the size of
// the entire admin request as an upper bound before allocating or encrypting.
const PASSWORD_MAX_BYTES: usize = 4096;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CredentialBinding {
    pub backend: BackendId,
    pub account: AccountId,
    /// The exact NEXUS_MUSIC_SERVER setting at the time of the check.
    pub internal: String,
    pub name: Name,
    pub count: u64,
}

impl CredentialBinding {
    fn associated_data(&self) -> Vec<u8> {
        let internal = self.internal.as_bytes();
        let name = self.name.as_str().as_bytes();
        // V1 authenticated the device address. Keep v2 separate even if the
        // two addresses happen to have the same spelling.
        let mut bytes = b"Nexus Music credential v2\0".to_vec();
        bytes.extend_from_slice(self.backend.as_bytes());
        bytes.extend_from_slice(&self.account.get().to_be_bytes());
        bytes.extend_from_slice(&self.count.to_be_bytes());
        // The design does not say how to encode consecutive variable fields.
        // Widths and lengths keep every binding byte unambiguous.
        bytes.extend_from_slice(&(internal.len() as u64).to_be_bytes());
        bytes.extend_from_slice(internal);
        bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
        bytes.extend_from_slice(name);
        bytes
    }
}

pub struct SealingKey(Zeroizing<[u8; 32]>);

impl SealingKey {
    pub fn new(key: [u8; 32]) -> Self {
        Self(Zeroizing::new(key))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(Key::from_slice(self.0.as_slice()))
    }

    /// Use a random nonce never used before with this key.
    pub fn seal(
        &self,
        binding: &CredentialBinding,
        password: &[u8],
        nonce: [u8; NONCE_BYTES],
    ) -> Option<SealedCredential> {
        if password.is_empty() || password.len() > PASSWORD_MAX_BYTES {
            return None;
        }
        let encrypted = self
            .cipher()
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: password,
                    aad: &binding.associated_data(),
                },
            )
            .ok()?;
        let mut bytes = Vec::with_capacity(NONCE_BYTES + encrypted.len());
        bytes.extend_from_slice(&nonce);
        bytes.extend_from_slice(&encrypted);
        Some(SealedCredential(bytes))
    }

    /// The binding must come from current authority, including the apart count.
    pub(super) fn open(
        &self,
        binding: &CredentialBinding,
        sealed: &SealedCredential,
    ) -> Option<Zeroizing<Vec<u8>>> {
        let (nonce, msg) = sealed.0.split_first_chunk::<NONCE_BYTES>()?;
        self.cipher()
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg,
                    aad: &binding.associated_data(),
                },
            )
            .ok()
            .map(Zeroizing::new)
    }
}

impl fmt::Debug for SealingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SealingKey(..)")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SealedCredential(Vec<u8>);

impl SealedCredential {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        (NONCE_BYTES + TAG_BYTES + 1..=NONCE_BYTES + TAG_BYTES + PASSWORD_MAX_BYTES)
            .contains(&bytes.len())
            .then(|| Self(bytes.to_vec()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SealedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SealedCredential(..)")
    }
}
