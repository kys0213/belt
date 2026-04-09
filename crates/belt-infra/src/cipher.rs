//! Symmetric encryption/decryption implementation.
//!
//! Provides [`XorStreamCipher`], a stream cipher that derives a pseudo-random
//! keystream from the provided key via iterative mixing. Each encryption call
//! generates a random nonce that is prepended to the ciphertext, ensuring that
//! encrypting the same plaintext twice produces different outputs.
//!
//! **Scope**: designed for protecting local secrets (API tokens, workspace
//! credentials) at rest. Not intended for network transport security.

use belt_core::cipher::{Cipher, CipherError};

/// Size of the random nonce prepended to each ciphertext.
const NONCE_LEN: usize = 16;

/// XOR stream cipher with nonce-based keystream derivation.
///
/// # Format
///
/// ```text
/// ciphertext = nonce (16 bytes) || encrypted_data
/// ```
///
/// The keystream is derived by iteratively mixing the key bytes with the nonce,
/// producing a deterministic but unpredictable byte sequence for each
/// (key, nonce) pair.
pub struct XorStreamCipher;

impl XorStreamCipher {
    /// Create a new `XorStreamCipher` instance.
    pub fn new() -> Self {
        Self
    }
}

impl Default for XorStreamCipher {
    fn default() -> Self {
        Self::new()
    }
}

impl Cipher for XorStreamCipher {
    fn encrypt(&self, plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>, CipherError> {
        if key.is_empty() {
            return Err(CipherError::InvalidKey("key must not be empty".to_string()));
        }

        let nonce = generate_nonce();
        let keystream = derive_keystream(key, &nonce, plaintext.len());

        let mut ciphertext = Vec::with_capacity(NONCE_LEN + plaintext.len());
        ciphertext.extend_from_slice(&nonce);
        for (i, &byte) in plaintext.iter().enumerate() {
            ciphertext.push(byte ^ keystream[i]);
        }

        Ok(ciphertext)
    }

    fn decrypt(&self, ciphertext: &[u8], key: &[u8]) -> Result<Vec<u8>, CipherError> {
        if key.is_empty() {
            return Err(CipherError::InvalidKey("key must not be empty".to_string()));
        }

        if ciphertext.len() < NONCE_LEN {
            return Err(CipherError::DecryptionFailed(
                "ciphertext too short to contain nonce".to_string(),
            ));
        }

        let nonce = &ciphertext[..NONCE_LEN];
        let encrypted = &ciphertext[NONCE_LEN..];
        let keystream = derive_keystream(key, nonce, encrypted.len());

        let mut plaintext = Vec::with_capacity(encrypted.len());
        for (i, &byte) in encrypted.iter().enumerate() {
            plaintext.push(byte ^ keystream[i]);
        }

        Ok(plaintext)
    }

    fn algorithm(&self) -> &str {
        "xor-stream-v1"
    }
}

/// Generate a random nonce using OS entropy (via `getrandom`-style approach).
///
/// Falls back to a time-based + counter seed if `/dev/urandom` is unavailable.
fn generate_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];

    // Try OS randomness first.
    if try_fill_random(&mut nonce) {
        return nonce;
    }

    // Fallback: derive from high-resolution time + pointer entropy.
    let time_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let ptr_entropy = (&nonce as *const _ as u64) as u128;
    let combined = time_seed ^ ptr_entropy;

    for (i, byte) in nonce.iter_mut().enumerate() {
        *byte = ((combined >> ((i % 16) * 8)) & 0xFF) as u8;
        // Mix further to avoid obvious patterns.
        *byte = byte.wrapping_mul(0x6D).wrapping_add(i as u8);
    }

    nonce
}

/// Try to fill `buf` with random bytes from the OS.
#[cfg(unix)]
fn try_fill_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        f.read_exact(buf).is_ok()
    } else {
        false
    }
}

#[cfg(not(unix))]
fn try_fill_random(buf: &mut [u8]) -> bool {
    // On non-Unix, use time-based fallback (the caller handles this).
    let _ = buf;
    false
}

/// Derive a keystream of `len` bytes from `key` and `nonce`.
///
/// Uses iterative mixing: each 32-byte block is produced by hashing the
/// previous state combined with the key material.
fn derive_keystream(key: &[u8], nonce: &[u8], len: usize) -> Vec<u8> {
    let mut keystream = Vec::with_capacity(len);

    // Initial state: interleave key and nonce bytes.
    let mut state = [0u8; 32];
    for (i, byte) in state.iter_mut().enumerate() {
        let k = key[i % key.len()];
        let n = nonce[i % nonce.len()];
        *byte = k.wrapping_add(n).wrapping_mul(0xA5).wrapping_add(i as u8);
    }

    let mut counter: u64 = 0;
    while keystream.len() < len {
        // Mix counter into state.
        let counter_bytes = counter.to_le_bytes();
        for (i, byte) in state.iter_mut().enumerate() {
            *byte ^= counter_bytes[i % 8];
        }

        // Apply mixing rounds.
        mix_state(&mut state);

        // Emit state bytes to keystream.
        let remaining = len - keystream.len();
        let to_emit = remaining.min(state.len());
        keystream.extend_from_slice(&state[..to_emit]);

        counter += 1;
    }

    keystream.truncate(len);
    keystream
}

/// Apply mixing rounds to the state buffer.
///
/// This is a simplified substitution-permutation network that ensures
/// diffusion of key and nonce material across all state bytes.
fn mix_state(state: &mut [u8; 32]) {
    for round in 0..8u8 {
        // Forward pass: each byte depends on its predecessor.
        for i in 1..32 {
            state[i] = state[i]
                .wrapping_add(state[i - 1])
                .wrapping_mul(0x6D)
                .wrapping_add(round);
        }
        // Backward pass: each byte depends on its successor.
        for i in (0..31).rev() {
            state[i] ^= state[i + 1].wrapping_mul(0x3B);
        }
        // Rotate bytes.
        state.rotate_left((round as usize % 7) + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let cipher = XorStreamCipher::new();
        let key = b"my-secret-key-123";
        let plaintext = b"Hello, Belt! Sensitive API token here.";

        let ciphertext = cipher.encrypt(plaintext, key).unwrap();
        let decrypted = cipher.decrypt(&ciphertext, key).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_produces_different_ciphertext_each_time() {
        let cipher = XorStreamCipher::new();
        let key = b"key";
        let plaintext = b"same input";

        let ct1 = cipher.encrypt(plaintext, key).unwrap();
        let ct2 = cipher.encrypt(plaintext, key).unwrap();

        // Nonces differ, so ciphertexts should differ.
        assert_ne!(ct1, ct2);

        // But both decrypt to the same plaintext.
        assert_eq!(cipher.decrypt(&ct1, key).unwrap(), plaintext);
        assert_eq!(cipher.decrypt(&ct2, key).unwrap(), plaintext);
    }

    #[test]
    fn empty_plaintext_roundtrip() {
        let cipher = XorStreamCipher::new();
        let key = b"key";
        let plaintext = b"";

        let ct = cipher.encrypt(plaintext, key).unwrap();
        assert_eq!(ct.len(), NONCE_LEN); // nonce only, no encrypted data
        let pt = cipher.decrypt(&ct, key).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn empty_key_rejected() {
        let cipher = XorStreamCipher::new();

        let err = cipher.encrypt(b"data", b"").unwrap_err();
        assert!(matches!(err, CipherError::InvalidKey(_)));

        let err = cipher.decrypt(&[0u8; NONCE_LEN], b"").unwrap_err();
        assert!(matches!(err, CipherError::InvalidKey(_)));
    }

    #[test]
    fn truncated_ciphertext_rejected() {
        let cipher = XorStreamCipher::new();
        let short = vec![0u8; NONCE_LEN - 1];

        let err = cipher.decrypt(&short, b"key").unwrap_err();
        assert!(matches!(err, CipherError::DecryptionFailed(_)));
    }

    #[test]
    fn wrong_key_produces_wrong_plaintext() {
        let cipher = XorStreamCipher::new();
        let plaintext = b"secret data";

        let ct = cipher.encrypt(plaintext, b"correct-key").unwrap();
        let wrong_pt = cipher.decrypt(&ct, b"wrong-key").unwrap();

        assert_ne!(wrong_pt.as_slice(), plaintext);
    }

    #[test]
    fn large_plaintext_roundtrip() {
        let cipher = XorStreamCipher::new();
        let key = b"large-data-key";
        let plaintext: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();

        let ct = cipher.encrypt(&plaintext, key).unwrap();
        let pt = cipher.decrypt(&ct, key).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn algorithm_name() {
        let cipher = XorStreamCipher::new();
        assert_eq!(cipher.algorithm(), "xor-stream-v1");
    }

    #[test]
    fn default_trait() {
        let cipher = XorStreamCipher::default();
        let ct = cipher.encrypt(b"test", b"key").unwrap();
        let pt = cipher.decrypt(&ct, b"key").unwrap();
        assert_eq!(pt, b"test");
    }
}
