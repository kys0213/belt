use thiserror::Error;

/// Errors that can occur during encryption or decryption.
#[derive(Debug, Error)]
pub enum CipherError {
    /// The provided key is empty or invalid.
    #[error("invalid key: {0}")]
    InvalidKey(String),

    /// Encryption failed.
    #[error("encryption failed: {0}")]
    EncryptionFailed(String),

    /// Decryption failed (e.g. corrupted data, wrong key, bad format).
    #[error("decryption failed: {0}")]
    DecryptionFailed(String),
}

/// Trait for symmetric encryption and decryption of byte sequences.
///
/// Implementations must guarantee that decrypting the output of `encrypt`
/// with the same key produces the original plaintext:
///
/// ```text
/// let ct = cipher.encrypt(plaintext, key)?;
/// let pt = cipher.decrypt(&ct, key)?;
/// assert_eq!(pt, plaintext);
/// ```
pub trait Cipher: Send + Sync {
    /// Encrypt `plaintext` using the provided `key`.
    ///
    /// Returns the ciphertext on success. The ciphertext format is
    /// implementation-defined but must be accepted by [`Cipher::decrypt`].
    fn encrypt(&self, plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>, CipherError>;

    /// Decrypt `ciphertext` using the provided `key`.
    ///
    /// Returns the original plaintext on success.
    fn decrypt(&self, ciphertext: &[u8], key: &[u8]) -> Result<Vec<u8>, CipherError>;

    /// Returns the human-readable name of this cipher algorithm.
    fn algorithm(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial no-op cipher for testing the trait interface.
    struct IdentityCipher;

    impl Cipher for IdentityCipher {
        fn encrypt(&self, plaintext: &[u8], _key: &[u8]) -> Result<Vec<u8>, CipherError> {
            Ok(plaintext.to_vec())
        }

        fn decrypt(&self, ciphertext: &[u8], _key: &[u8]) -> Result<Vec<u8>, CipherError> {
            Ok(ciphertext.to_vec())
        }

        fn algorithm(&self) -> &str {
            "identity"
        }
    }

    #[test]
    fn identity_cipher_roundtrip() {
        let cipher = IdentityCipher;
        let plaintext = b"hello belt";
        let key = b"secret";
        let ct = cipher.encrypt(plaintext, key).unwrap();
        let pt = cipher.decrypt(&ct, key).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn cipher_error_display() {
        let err = CipherError::InvalidKey("empty key".to_string());
        assert_eq!(err.to_string(), "invalid key: empty key");

        let err = CipherError::EncryptionFailed("buffer overflow".to_string());
        assert_eq!(err.to_string(), "encryption failed: buffer overflow");

        let err = CipherError::DecryptionFailed("bad padding".to_string());
        assert_eq!(err.to_string(), "decryption failed: bad padding");
    }
}
