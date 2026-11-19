use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use thiserror::Error;

const NONCE_LEN: usize = 12;

#[derive(Debug, Error, PartialEq)]
pub enum CryptoError {
    #[error("key is not valid base64")]
    InvalidKeyEncoding,
    #[error("key must be exactly 32 bytes, got {0}")]
    InvalidKeyLength(usize),
    #[error("ciphertext is not valid base64")]
    InvalidCiphertextEncoding,
    #[error("ciphertext is too short")]
    CiphertextTooShort,
    #[error("decryption failed (wrong key or tampered data)")]
    DecryptFailed,
    #[error("decrypted data is not valid UTF-8")]
    InvalidUtf8,
}

/// Mengenkripsi/mendekripsi rahasia (PAT, API key) dengan AES-256-GCM.
pub struct Cipher {
    inner: Aes256Gcm,
}

impl Cipher {
    /// `key` = base64 dari tepat 32 byte (nilai `ADUS_SECRET_KEY`).
    pub fn from_base64_key(key: &str) -> Result<Cipher, CryptoError> {
        let bytes = STANDARD
            .decode(key.trim())
            .map_err(|_| CryptoError::InvalidKeyEncoding)?;
        if bytes.len() != 32 {
            return Err(CryptoError::InvalidKeyLength(bytes.len()));
        }
        Ok(Cipher {
            inner: Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&bytes)),
        })
    }

    /// Hasil: base64 dari `nonce(12) || ciphertext`. Nonce acak baru di setiap panggilan.
    pub fn encrypt(&self, plaintext: &str) -> String {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .inner
            .encrypt(&nonce, plaintext.as_bytes())
            .expect("AES-GCM encrypt only fails on absurdly large input");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ciphertext);
        STANDARD.encode(out)
    }

    pub fn decrypt(&self, encoded: &str) -> Result<String, CryptoError> {
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| CryptoError::InvalidCiphertextEncoding)?;
        if bytes.len() <= NONCE_LEN {
            return Err(CryptoError::CiphertextTooShort);
        }
        let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
        let plaintext = self
            .inner
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            .map_err(|_| CryptoError::DecryptFailed)?;
        String::from_utf8(plaintext).map_err(|_| CryptoError::InvalidUtf8)
    }
}

/// 4 karakter terakhir rahasia untuk ditampilkan di UI. Kurang dari 4 karakter → semua `*`.
pub fn last4(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() < 4 {
        return "*".repeat(chars.len());
    }
    chars[chars.len() - 4..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const KEY_B: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";

    fn cipher(key: &str) -> Cipher {
        Cipher::from_base64_key(key).unwrap()
    }

    #[test]
    fn roundtrip() {
        let c = cipher(KEY_A);
        assert_eq!(c.decrypt(&c.encrypt("ghp_secret")).unwrap(), "ghp_secret");
    }

    #[test]
    fn nonce_is_random() {
        let c = cipher(KEY_A);
        assert_ne!(c.encrypt("same"), c.encrypt("same"));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let c = cipher(KEY_A);
        let mut bytes = STANDARD.decode(c.encrypt("secret")).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        assert_eq!(
            c.decrypt(&STANDARD.encode(bytes)),
            Err(CryptoError::DecryptFailed)
        );
    }

    #[test]
    fn wrong_key_fails() {
        let encrypted = cipher(KEY_A).encrypt("secret");
        assert!(cipher(KEY_B).decrypt(&encrypted).is_err());
    }

    #[test]
    fn key_must_be_32_bytes() {
        let key16 = STANDARD.encode([0u8; 16]);
        assert!(matches!(
            Cipher::from_base64_key(&key16),
            Err(CryptoError::InvalidKeyLength(16))
        ));
    }

    #[test]
    fn last4_examples() {
        assert_eq!(last4("ghp_abcd1234"), "1234");
        assert_eq!(last4("ab"), "**");
        assert_eq!(last4("héllo→→"), "lo→→");
    }
}
