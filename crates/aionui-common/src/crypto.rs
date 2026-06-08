use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

const NONCE_SIZE: usize = 12;
const KEY_SIZE: usize = 32;

/// Crypto helper error independent of HTTP/API boundaries.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("AES-256 key must be exactly {expected} bytes, got {actual}")]
    InvalidKeySize { expected: usize, actual: usize },

    #[error("Failed to create cipher: {0}")]
    CipherInit(String),

    #[error("RNG failure: {0}")]
    Random(String),

    #[error("Encryption failed: {0}")]
    Encryption(String),

    #[error("Invalid base64: {0}")]
    InvalidBase64(String),

    #[error("Ciphertext too short")]
    CiphertextTooShort,

    #[error("Decryption failed: invalid key or corrupted data")]
    DecryptionFailed,

    #[error("Invalid UTF-8 in decrypted data: {0}")]
    InvalidUtf8(String),
}

impl CryptoError {
    /// Returns true for caller/data problems that API boundaries should map to 400.
    pub fn is_bad_request(&self) -> bool {
        matches!(
            self,
            Self::InvalidKeySize { .. } | Self::InvalidBase64(_) | Self::CiphertextTooShort | Self::DecryptionFailed
        )
    }
}

/// Encrypt a string value using AES-256-GCM.
///
/// The key must be exactly 32 bytes. Output is base64-encoded (nonce + ciphertext + tag).
pub fn encrypt_string(plaintext: &str, key: &[u8]) -> Result<String, CryptoError> {
    validate_key_size(key)?;

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| CryptoError::CipherInit(e.to_string()))?;

    let mut nonce_bytes = [0u8; NONCE_SIZE];
    getrandom::getrandom(&mut nonce_bytes).map_err(|e| CryptoError::Random(e.to_string()))?;
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| CryptoError::Encryption(e.to_string()))?;

    let mut combined = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(BASE64.encode(combined))
}

/// Decrypt an AES-256-GCM encrypted string.
///
/// The key must be exactly 32 bytes. Input is base64-encoded (nonce + ciphertext + tag).
pub fn decrypt_string(ciphertext: &str, key: &[u8]) -> Result<String, CryptoError> {
    validate_key_size(key)?;

    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| CryptoError::CipherInit(e.to_string()))?;

    let combined = BASE64
        .decode(ciphertext)
        .map_err(|e| CryptoError::InvalidBase64(e.to_string()))?;

    if combined.len() < NONCE_SIZE {
        return Err(CryptoError::CiphertextTooShort);
    }

    let (nonce_bytes, encrypted) = combined.split_at(NONCE_SIZE);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, encrypted)
        .map_err(|_| CryptoError::DecryptionFailed)?;

    String::from_utf8(plaintext).map_err(|e| CryptoError::InvalidUtf8(e.to_string()))
}

fn validate_key_size(key: &[u8]) -> Result<(), CryptoError> {
    if key.len() != KEY_SIZE {
        return Err(CryptoError::InvalidKeySize {
            expected: KEY_SIZE,
            actual: key.len(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [0x42; 32]
    }

    #[test]
    fn test_roundtrip() {
        let key = test_key();
        let encrypted = encrypt_string("hello", &key).unwrap();
        let decrypted = decrypt_string(&encrypted, &key).unwrap();
        assert_eq!(decrypted, "hello");
    }

    #[test]
    fn test_empty_string() {
        let key = test_key();
        let encrypted = encrypt_string("", &key).unwrap();
        let decrypted = decrypt_string(&encrypted, &key).unwrap();
        assert_eq!(decrypted, "");
    }

    #[test]
    fn test_unicode() {
        let key = test_key();
        let encrypted = encrypt_string("你好世界", &key).unwrap();
        let decrypted = decrypt_string(&encrypted, &key).unwrap();
        assert_eq!(decrypted, "你好世界");
    }

    #[test]
    fn test_wrong_key_fails() {
        let key = test_key();
        let encrypted = encrypt_string("hello", &key).unwrap();
        let wrong_key = [0x99; 32];
        assert!(matches!(
            decrypt_string(&encrypted, &wrong_key),
            Err(CryptoError::DecryptionFailed)
        ));
    }

    #[test]
    fn test_nonce_randomness() {
        let key = test_key();
        let enc1 = encrypt_string("hello", &key).unwrap();
        let enc2 = encrypt_string("hello", &key).unwrap();
        assert_ne!(enc1, enc2);
    }

    #[test]
    fn test_invalid_key_size() {
        let short_key = [0u8; 16];
        assert!(matches!(
            encrypt_string("hello", &short_key),
            Err(CryptoError::InvalidKeySize {
                expected: KEY_SIZE,
                actual: 16
            })
        ));
        assert!(matches!(
            decrypt_string("dGVzdA==", &short_key),
            Err(CryptoError::InvalidKeySize {
                expected: KEY_SIZE,
                actual: 16
            })
        ));
    }

    #[test]
    fn test_invalid_base64() {
        let key = test_key();
        assert!(matches!(
            decrypt_string("not-valid-base64!!!", &key),
            Err(CryptoError::InvalidBase64(_))
        ));
    }

    #[test]
    fn test_ciphertext_too_short() {
        let key = test_key();
        // Base64 of less than 12 bytes
        let short = BASE64.encode([0u8; 5]);
        assert!(matches!(
            decrypt_string(&short, &key),
            Err(CryptoError::CiphertextTooShort)
        ));
    }
}
