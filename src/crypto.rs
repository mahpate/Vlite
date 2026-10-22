//! Opt-in authenticated encryption at rest for format-v5 frame bodies.
//!
//! Frame prefixes (operation, collection, sequence, ID, counts) stay readable
//! so the log remains scannable and crash-recoverable, but they are bound to
//! the ciphertext as AEAD associated data. Bodies (vectors, payloads, metadata,
//! expiries, settings, timestamps) are sealed with ChaCha20-Poly1305 under a
//! per-file key derived from the caller's key and a random per-file salt.

use crate::format::{FOOTER_SIZE, PREFIX_SIZE, crc32};
use crate::{Error, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit, Payload},
};
use sha2::{Digest, Sha256};
use std::fmt;

pub(crate) const DESCRIPTOR_SIZE: usize = 64;
/// Bytes added to every sealed frame body: 12-byte nonce plus 16-byte tag.
pub(crate) const SEAL_OVERHEAD: usize = 28;
const MAGIC: &[u8; 4] = b"VENC";
const KDF_RAW: u8 = 0;
const KDF_ARGON2ID: u8 = 1;

/// Argon2id cost parameters for passphrase keys. They are stored in the file
/// header, so opening needs only the passphrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Default for KdfParams {
    /// OWASP-recommended Argon2id minimum: 19 MiB, 2 passes, 1 lane.
    fn default() -> Self {
        Self {
            memory_kib: 19_456,
            iterations: 2,
            parallelism: 1,
        }
    }
}

impl KdfParams {
    fn validate(self) -> Result<()> {
        if self.parallelism == 0
            || self.parallelism > 16
            || self.iterations == 0
            || self.iterations > 64
            || self.memory_kib < 8 * self.parallelism
            || self.memory_kib > 4 * 1024 * 1024
        {
            return Err(Error::InvalidEncryptionOptions(
                "Argon2id parameters out of range",
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
enum Source {
    Raw([u8; 32]),
    Passphrase {
        passphrase: Vec<u8>,
        params: KdfParams,
    },
}

/// Caller-supplied encryption key: either a raw 32-byte key or a passphrase
/// stretched with Argon2id. Key material is wiped on drop (best effort).
#[derive(Clone)]
pub struct EncryptionKey {
    source: Source,
}

impl fmt::Debug for EncryptionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EncryptionKey(<redacted>)")
    }
}

impl Drop for EncryptionKey {
    fn drop(&mut self) {
        match &mut self.source {
            Source::Raw(key) => wipe(key),
            Source::Passphrase { passphrase, .. } => wipe(passphrase),
        }
    }
}

fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        // SAFETY: `byte` is a valid, exclusive reference.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

impl EncryptionKey {
    /// A raw 256-bit key from a secure random source or a key-management system.
    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self {
            source: Source::Raw(key),
        }
    }

    /// A passphrase key with default Argon2id parameters (19 MiB, 2 passes).
    pub fn from_passphrase(passphrase: impl AsRef<[u8]>) -> Result<Self> {
        Self::from_passphrase_with_params(passphrase, KdfParams::default())
    }

    /// A passphrase key with explicit Argon2id parameters. The parameters are
    /// used when creating or re-encrypting a file; when opening, the file's
    /// stored parameters apply.
    pub fn from_passphrase_with_params(
        passphrase: impl AsRef<[u8]>,
        params: KdfParams,
    ) -> Result<Self> {
        params.validate()?;
        let passphrase = passphrase.as_ref();
        if passphrase.is_empty() {
            return Err(Error::InvalidEncryptionOptions("empty passphrase"));
        }
        Ok(Self {
            source: Source::Passphrase {
                passphrase: passphrase.to_vec(),
                params,
            },
        })
    }

    fn kdf(&self) -> u8 {
        match self.source {
            Source::Raw(_) => KDF_RAW,
            Source::Passphrase { .. } => KDF_ARGON2ID,
        }
    }

    fn params(&self) -> KdfParams {
        match &self.source {
            Source::Raw(_) => KdfParams {
                memory_kib: 0,
                iterations: 0,
                parallelism: 0,
            },
            Source::Passphrase { params, .. } => *params,
        }
    }

    /// Derives the 32-byte master key. Passphrases use the stored salt/params.
    fn master(&self, salt: &[u8; 16], params: KdfParams) -> Result<[u8; 32]> {
        match &self.source {
            Source::Raw(key) => Ok(*key),
            Source::Passphrase { passphrase, .. } => {
                params.validate()?;
                let argon = Argon2::new(
                    Algorithm::Argon2id,
                    Version::V0x13,
                    Params::new(
                        params.memory_kib,
                        params.iterations,
                        params.parallelism,
                        Some(32),
                    )
                    .map_err(|_| Error::InvalidEncryptionOptions("invalid Argon2id parameters"))?,
                );
                let mut out = [0u8; 32];
                argon
                    .hash_password_into(passphrase, salt, &mut out)
                    .map_err(|_| Error::InvalidEncryptionOptions("Argon2id derivation failed"))?;
                Ok(out)
            }
        }
    }
}

/// The plaintext, checksummed encryption descriptor stored right after the
/// 32-byte file header of an encrypted file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Descriptor {
    kdf: u8,
    pub key_id: u64,
    salt: [u8; 16],
    check: [u8; 16],
    params: KdfParams,
}

impl Descriptor {
    pub(crate) fn encode(&self) -> [u8; DESCRIPTOR_SIZE] {
        let mut bytes = [0u8; DESCRIPTOR_SIZE];
        bytes[..4].copy_from_slice(MAGIC);
        bytes[4] = self.kdf;
        bytes[8..16].copy_from_slice(&self.key_id.to_le_bytes());
        bytes[16..32].copy_from_slice(&self.salt);
        bytes[32..48].copy_from_slice(&self.check);
        bytes[48..52].copy_from_slice(&self.params.memory_kib.to_le_bytes());
        bytes[52..56].copy_from_slice(&self.params.iterations.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.params.parallelism.to_le_bytes());
        let checksum = crc32(&bytes[..60]);
        bytes[60..].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }

    pub(crate) fn decode(bytes: &[u8; DESCRIPTOR_SIZE]) -> Result<Self> {
        let corrupt = |reason| Error::Corrupt { offset: 32, reason };
        if &bytes[..4] != MAGIC {
            return Err(corrupt("invalid encryption descriptor magic"));
        }
        if crc32(&bytes[..60]) != u32::from_le_bytes(bytes[60..].try_into().unwrap()) {
            return Err(corrupt("encryption descriptor checksum mismatch"));
        }
        if bytes[5..8] != [0; 3] || !matches!(bytes[4], KDF_RAW | KDF_ARGON2ID) {
            return Err(corrupt("invalid encryption descriptor fields"));
        }
        let params = KdfParams {
            memory_kib: u32::from_le_bytes(bytes[48..52].try_into().unwrap()),
            iterations: u32::from_le_bytes(bytes[52..56].try_into().unwrap()),
            parallelism: u32::from_le_bytes(bytes[56..60].try_into().unwrap()),
        };
        if bytes[4] == KDF_ARGON2ID && params.validate().is_err() {
            return Err(corrupt("invalid encryption KDF parameters"));
        }
        Ok(Self {
            kdf: bytes[4],
            key_id: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            salt: bytes[16..32].try_into().unwrap(),
            check: bytes[32..48].try_into().unwrap(),
            params,
        })
    }
}

fn digest(label: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(label);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// A usable cipher bound to one file's key, salt and descriptor.
#[derive(Clone)]
pub(crate) struct Cipher {
    aead: ChaCha20Poly1305,
    pub descriptor: Descriptor,
}

impl fmt::Debug for Cipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cipher(key_id={:016x})", self.descriptor.key_id)
    }
}

impl Cipher {
    fn from_master(master: [u8; 32], descriptor_salt: [u8; 16]) -> (ChaCha20Poly1305, [u8; 16]) {
        let mut file_key = digest(b"vlite/v1/file-key", &[&master, &descriptor_salt]);
        let aead = ChaCha20Poly1305::new(Key::from_slice(&file_key));
        let check_full = digest(b"vlite/v1/key-check", &[&file_key]);
        wipe(&mut file_key);
        (aead, check_full[..16].try_into().unwrap())
    }

    /// Creates the cipher and descriptor for a new file (fresh random salt).
    pub(crate) fn create(key: &EncryptionKey) -> Result<Self> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt).map_err(|error| {
            Error::Io(std::io::Error::other(format!(
                "operating-system randomness unavailable: {error}"
            )))
        })?;
        let params = key.params();
        let mut master = key.master(&salt, params)?;
        let key_id = u64::from_le_bytes(
            digest(b"vlite/v1/key-id", &[&master])[..8]
                .try_into()
                .unwrap(),
        );
        let (aead, check) = Self::from_master(master, salt);
        wipe(&mut master);
        Ok(Self {
            aead,
            descriptor: Descriptor {
                kdf: key.kdf(),
                key_id,
                salt,
                check,
                params,
            },
        })
    }

    /// Unlocks an existing file; a wrong key is an explicit error, never
    /// reported as corruption and never producing garbage plaintext.
    pub(crate) fn open(key: &EncryptionKey, descriptor: &Descriptor) -> Result<Self> {
        let mismatch = Error::EncryptionKeyMismatch {
            key_id: descriptor.key_id,
        };
        if key.kdf() != descriptor.kdf {
            return Err(mismatch);
        }
        let mut master = key.master(&descriptor.salt, descriptor.params)?;
        let (aead, check) = Self::from_master(master, descriptor.salt);
        wipe(&mut master);
        if check != descriptor.check {
            return Err(mismatch);
        }
        Ok(Self {
            aead,
            descriptor: *descriptor,
        })
    }

    fn aad(&self, prefix: &[u8]) -> Vec<u8> {
        let mut aad = Vec::with_capacity(48);
        aad.extend_from_slice(&self.descriptor.salt);
        aad.extend_from_slice(&prefix[..32]);
        aad
    }

    /// Converts a plaintext frame (`wrap` layout) to its sealed on-disk form.
    pub(crate) fn seal_frame(&self, plain: &[u8]) -> Result<Vec<u8>> {
        let prefix = &plain[..PREFIX_SIZE];
        let body = &plain[PREFIX_SIZE..plain.len() - FOOTER_SIZE];
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|error| {
            Error::Io(std::io::Error::other(format!(
                "operating-system randomness unavailable: {error}"
            )))
        })?;
        let sealed = self
            .aead
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: body,
                    aad: &self.aad(prefix),
                },
            )
            .map_err(|_| Error::InvalidEncryptionOptions("frame encryption failed"))?;
        let mut out = Vec::with_capacity(PREFIX_SIZE + 12 + sealed.len() + FOOTER_SIZE);
        out.extend_from_slice(prefix);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        let checksum = crc32(&out);
        out.extend_from_slice(b"VCMT");
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&prefix[8..16]);
        Ok(out)
    }

    /// Converts a checksum-verified on-disk frame back to the plaintext layout.
    /// None means the authentication tag did not verify.
    pub(crate) fn open_frame(&self, disk: &[u8]) -> Option<Vec<u8>> {
        let prefix = &disk[..PREFIX_SIZE];
        let sealed = &disk[PREFIX_SIZE..disk.len() - FOOTER_SIZE];
        if sealed.len() < SEAL_OVERHEAD {
            return None;
        }
        let (nonce, ciphertext) = sealed.split_at(12);
        let body = self
            .aead
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: ciphertext,
                    aad: &self.aad(prefix),
                },
            )
            .ok()?;
        let mut out = Vec::with_capacity(PREFIX_SIZE + body.len() + FOOTER_SIZE);
        out.extend_from_slice(prefix);
        out.extend_from_slice(&body);
        let checksum = crc32(&out);
        out.extend_from_slice(b"VCMT");
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(&prefix[8..16]);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_round_trips_and_detects_damage() {
        let key = EncryptionKey::from_bytes([7; 32]);
        let cipher = Cipher::create(&key).unwrap();
        let bytes = cipher.descriptor.encode();
        assert_eq!(Descriptor::decode(&bytes).unwrap(), cipher.descriptor);
        let mut damaged = bytes;
        damaged[20] ^= 1;
        assert!(Descriptor::decode(&damaged).is_err());
    }

    #[test]
    fn wrong_key_is_rejected_by_the_key_check() {
        let cipher = Cipher::create(&EncryptionKey::from_bytes([1; 32])).unwrap();
        assert!(Cipher::open(&EncryptionKey::from_bytes([1; 32]), &cipher.descriptor).is_ok());
        assert!(matches!(
            Cipher::open(&EncryptionKey::from_bytes([2; 32]), &cipher.descriptor),
            Err(Error::EncryptionKeyMismatch { .. })
        ));
    }
}
