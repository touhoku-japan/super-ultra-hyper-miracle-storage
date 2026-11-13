//! Encryption of access key secrets at rest, and generation of credentials.
//!
//! SigV4 verification needs the raw secret, so secrets cannot be hashed like passwords. They are
//! encrypted with AES-256-GCM under a master key that lives outside the metadata database, with
//! the access key id as associated data so a ciphertext cannot be moved to another key's row.

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, Generate, KeyInit, Payload},
};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};

use crate::config::AuthConfig;

const VERSION_PREFIX: &str = "v1:";
const NONCE_LEN: usize = 12;

pub struct SecretCipher {
    cipher: Aes256Gcm,
}

impl SecretCipher {
    pub fn from_hex(hex_key: &str) -> anyhow::Result<Self> {
        let bytes = hex::decode(hex_key.trim()).context("master key is not valid hex")?;
        if bytes.len() != 32 {
            bail!("master key must be 32 bytes (64 hex characters)");
        }
        let cipher = Aes256Gcm::new_from_slice(&bytes).context("invalid master key")?;
        Ok(Self { cipher })
    }

    /// Loads the master key from `SUHMS_MASTER_KEY` or the configured file.
    pub fn load(config: &AuthConfig) -> anyhow::Result<Self> {
        if let Ok(key) = std::env::var("SUHMS_MASTER_KEY") {
            return Self::from_hex(&key).context("SUHMS_MASTER_KEY");
        }
        let Some(path) = &config.master_key_file else {
            bail!(
                "no master key: set SUHMS_MASTER_KEY or [auth] master_key_file \
                 (create one with `suhms gen-master-key`)"
            );
        };
        let key = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read master key file {}", path.display()))?;
        Self::from_hex(&key).with_context(|| format!("master key file {}", path.display()))
    }

    pub fn generate_hex() -> String {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("OS random number generator is available");
        hex::encode(key)
    }

    pub fn encrypt(&self, key_id: &str, secret: &str) -> String {
        let nonce = Nonce::generate();
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: secret.as_bytes(),
                    aad: key_id.as_bytes(),
                },
            )
            .expect("AES-GCM encryption of a short secret cannot fail");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ciphertext);
        format!("{VERSION_PREFIX}{}", B64.encode(out))
    }

    pub fn decrypt(&self, key_id: &str, encrypted: &str) -> anyhow::Result<String> {
        let data = encrypted
            .strip_prefix(VERSION_PREFIX)
            .context("unknown secret encryption version")?;
        let data = B64.decode(data).context("secret is not valid base64")?;
        if data.len() <= NONCE_LEN {
            bail!("encrypted secret is too short");
        }
        let (nonce, ciphertext) = data.split_at(NONCE_LEN);
        let nonce = Nonce::try_from(nonce).expect("nonce has the right length");
        let plain = self
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: key_id.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("secret cannot be decrypted (wrong master key?)"))?;
        String::from_utf8(plain).context("decrypted secret is not UTF-8")
    }
}

/// A random string over `alphabet`, without modulo bias.
fn random_string(len: usize, alphabet: &[u8]) -> String {
    let limit = 256 - (256 % alphabet.len());
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        getrandom::fill(&mut buf).expect("OS random number generator is available");
        for &b in &buf {
            if out.len() == len {
                break;
            }
            if usize::from(b) < limit {
                out.push(char::from(alphabet[usize::from(b) % alphabet.len()]));
            }
        }
    }
    out
}

/// A new access key id: 20 upper-case alphanumerics starting with `SU`, like AWS's `AKIA...`.
pub fn generate_access_key_id() -> String {
    format!(
        "SU{}",
        random_string(18, b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
    )
}

/// A new 40-character secret key (about 238 bits of entropy).
pub fn generate_secret_key() -> String {
    random_string(
        40,
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> SecretCipher {
        SecretCipher::from_hex(&SecretCipher::generate_hex()).unwrap()
    }

    #[test]
    fn round_trips_and_binds_to_key_id() {
        let c = cipher();
        let enc = c.encrypt("SUKEY1", "secret-value");
        assert!(enc.starts_with("v1:"));
        assert_eq!(c.decrypt("SUKEY1", &enc).unwrap(), "secret-value");
        assert!(
            c.decrypt("SUKEY2", &enc).is_err(),
            "ciphertext moved to another key"
        );
        assert!(
            cipher().decrypt("SUKEY1", &enc).is_err(),
            "different master key"
        );
        assert_ne!(
            enc,
            c.encrypt("SUKEY1", "secret-value"),
            "fresh nonce each time"
        );
    }

    #[test]
    fn rejects_bad_master_keys() {
        assert!(SecretCipher::from_hex("zz").is_err());
        assert!(SecretCipher::from_hex(&"ab".repeat(16)).is_err());
    }

    #[test]
    fn generated_credentials_have_expected_shape() {
        let id = generate_access_key_id();
        assert_eq!(id.len(), 20);
        assert!(id.starts_with("SU"));
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        );
        let secret = generate_secret_key();
        assert_eq!(secret.len(), 40);
        assert!(secret.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(secret, generate_secret_key());
    }
}
