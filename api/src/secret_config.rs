use std::collections::BTreeMap;

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, OsRng, rand_core::RngCore},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

const ENVELOPE_VERSION: u8 = 1;
const NONCE_LEN: usize = 12;

#[derive(Clone)]
pub struct SecretConfigCipher {
    active_key_id: String,
    keys: BTreeMap<String, [u8; 32]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretConfigError {
    InvalidKeySpec(String),
    UnknownKey(String),
    InvalidEnvelope(String),
    EncryptFailed,
    DecryptFailed,
}

impl std::fmt::Display for SecretConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKeySpec(message) | Self::InvalidEnvelope(message) => {
                formatter.write_str(message)
            }
            Self::UnknownKey(key_id) => write!(formatter, "unknown config encryption key {key_id}"),
            Self::EncryptFailed => formatter.write_str("encrypt config secrets failed"),
            Self::DecryptFailed => formatter.write_str("decrypt config secrets failed"),
        }
    }
}

impl std::error::Error for SecretConfigError {}

#[derive(Debug, Serialize, Deserialize)]
struct CipherEnvelope {
    version: u8,
    key_id: String,
    nonce: String,
    ciphertext: String,
}

impl SecretConfigCipher {
    pub fn from_key_spec(
        active_key_id: impl Into<String>,
        key_spec: &str,
    ) -> Result<Self, SecretConfigError> {
        let mut encoded_keys = BTreeMap::new();
        for entry in key_spec
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
        {
            let (key_id, encoded_key) = entry.split_once(':').ok_or_else(|| {
                SecretConfigError::InvalidKeySpec(
                    "config master keys must use key_id:base64 entries".to_owned(),
                )
            })?;
            if encoded_keys
                .insert(key_id.trim().to_owned(), encoded_key.trim().to_owned())
                .is_some()
            {
                return Err(SecretConfigError::InvalidKeySpec(format!(
                    "duplicate config encryption key id {}",
                    key_id.trim()
                )));
            }
        }
        Self::from_base64_keys(active_key_id, &encoded_keys)
    }

    pub fn from_base64_keys(
        active_key_id: impl Into<String>,
        encoded_keys: &BTreeMap<String, String>,
    ) -> Result<Self, SecretConfigError> {
        let active_key_id = active_key_id.into();
        if active_key_id.trim().is_empty() {
            return Err(SecretConfigError::InvalidKeySpec(
                "active config encryption key id is required".to_owned(),
            ));
        }

        let mut keys = BTreeMap::new();
        for (key_id, encoded) in encoded_keys {
            if key_id.trim().is_empty() {
                return Err(SecretConfigError::InvalidKeySpec(
                    "config encryption key id cannot be empty".to_owned(),
                ));
            }
            let decoded = STANDARD.decode(encoded.trim()).map_err(|_| {
                SecretConfigError::InvalidKeySpec(format!(
                    "config encryption key {key_id} must be base64"
                ))
            })?;
            let key: [u8; 32] = decoded.try_into().map_err(|_| {
                SecretConfigError::InvalidKeySpec(format!(
                    "config encryption key {key_id} must contain exactly 32 bytes"
                ))
            })?;
            keys.insert(key_id.clone(), key);
        }
        if !keys.contains_key(&active_key_id) {
            return Err(SecretConfigError::UnknownKey(active_key_id));
        }
        Ok(Self {
            active_key_id,
            keys,
        })
    }

    pub fn active_key_id(&self) -> &str {
        &self.active_key_id
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<String, SecretConfigError> {
        let key = self
            .keys
            .get(&self.active_key_id)
            .ok_or_else(|| SecretConfigError::UnknownKey(self.active_key_id.clone()))?;
        let cipher =
            Aes256Gcm::new_from_slice(key).map_err(|_| SecretConfigError::EncryptFailed)?;
        let mut nonce_bytes = [0_u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
            .map_err(|_| SecretConfigError::EncryptFailed)?;
        serde_json::to_string(&CipherEnvelope {
            version: ENVELOPE_VERSION,
            key_id: self.active_key_id.clone(),
            nonce: STANDARD.encode(nonce_bytes),
            ciphertext: STANDARD.encode(ciphertext),
        })
        .map_err(|_| SecretConfigError::EncryptFailed)
    }

    pub fn decrypt(&self, envelope: &str) -> Result<Vec<u8>, SecretConfigError> {
        let envelope: CipherEnvelope = serde_json::from_str(envelope).map_err(|_| {
            SecretConfigError::InvalidEnvelope("invalid config secret envelope".to_owned())
        })?;
        if envelope.version != ENVELOPE_VERSION {
            return Err(SecretConfigError::InvalidEnvelope(format!(
                "unsupported config secret envelope version {}",
                envelope.version
            )));
        }
        let key = self
            .keys
            .get(&envelope.key_id)
            .ok_or_else(|| SecretConfigError::UnknownKey(envelope.key_id.clone()))?;
        let nonce = STANDARD.decode(envelope.nonce).map_err(|_| {
            SecretConfigError::InvalidEnvelope("invalid config secret nonce".to_owned())
        })?;
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| {
            SecretConfigError::InvalidEnvelope("invalid config secret nonce length".to_owned())
        })?;
        let ciphertext = STANDARD.decode(envelope.ciphertext).map_err(|_| {
            SecretConfigError::InvalidEnvelope("invalid config secret ciphertext".to_owned())
        })?;
        let cipher =
            Aes256Gcm::new_from_slice(key).map_err(|_| SecretConfigError::DecryptFailed)?;
        cipher
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| SecretConfigError::DecryptFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded_key(seed: u8) -> String {
        STANDARD.encode([seed; 32])
    }

    #[test]
    fn encrypts_round_trips_and_does_not_embed_plaintext() {
        let cipher = SecretConfigCipher::from_base64_keys(
            "v1",
            &BTreeMap::from([("v1".to_owned(), encoded_key(7))]),
        )
        .expect("create cipher");

        let envelope = cipher.encrypt(b"APP_SECRET=top-secret").expect("encrypt");

        assert!(!envelope.contains("top-secret"));
        assert_eq!(
            cipher.decrypt(&envelope).expect("decrypt"),
            b"APP_SECRET=top-secret"
        );
    }

    #[test]
    fn key_ring_decrypts_old_key_and_encrypts_with_active_key() {
        let old = SecretConfigCipher::from_base64_keys(
            "old",
            &BTreeMap::from([("old".to_owned(), encoded_key(1))]),
        )
        .expect("create old cipher");
        let envelope = old.encrypt(b"secret").expect("encrypt with old key");
        let rotated = SecretConfigCipher::from_base64_keys(
            "new",
            &BTreeMap::from([
                ("old".to_owned(), encoded_key(1)),
                ("new".to_owned(), encoded_key(2)),
            ]),
        )
        .expect("create rotated cipher");

        assert_eq!(rotated.decrypt(&envelope).expect("decrypt old"), b"secret");
        let next = rotated.encrypt(b"next").expect("encrypt new");
        assert!(next.contains("\"key_id\":\"new\""));
    }

    #[test]
    fn rejects_missing_bad_and_tampered_keys() {
        assert!(SecretConfigCipher::from_base64_keys("v1", &BTreeMap::new()).is_err());
        assert!(
            SecretConfigCipher::from_base64_keys(
                "v1",
                &BTreeMap::from([("v1".to_owned(), STANDARD.encode([1_u8; 31]))]),
            )
            .is_err()
        );

        let cipher = SecretConfigCipher::from_base64_keys(
            "v1",
            &BTreeMap::from([("v1".to_owned(), encoded_key(3))]),
        )
        .expect("create cipher");
        let mut envelope: serde_json::Value =
            serde_json::from_str(&cipher.encrypt(b"secret").expect("encrypt"))
                .expect("parse envelope");
        envelope["ciphertext"] = serde_json::Value::String(STANDARD.encode([9_u8; 32]));

        assert!(cipher.decrypt(&envelope.to_string()).is_err());
    }

    #[test]
    fn parses_external_key_ring_spec() {
        let spec = format!("old:{},new:{}", encoded_key(1), encoded_key(2));
        let cipher = SecretConfigCipher::from_key_spec("new", &spec).expect("parse key spec");

        assert_eq!(cipher.active_key_id(), "new");
        assert!(SecretConfigCipher::from_key_spec("new", "invalid").is_err());
        assert!(
            SecretConfigCipher::from_key_spec(
                "new",
                &format!("new:{},new:{}", encoded_key(1), encoded_key(2))
            )
            .is_err()
        );
    }
}
