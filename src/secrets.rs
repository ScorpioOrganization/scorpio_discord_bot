//! Encryption at rest for third party API keys stored in the database.
//!
//! Keys are sealed with XChaCha20-Poly1305 under a 256 bit key that never touches the
//! database: it comes from `SECRETS_KEY` or from a `secret.key` file next to the database.
//! A copied database file alone therefore does not reveal any API key.

use std::fmt;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use crate::config::Config;
use crate::db;

pub const KEY_FILE: &str = "secret.key";
const VERSION: u8 = 1;
const NONCE_LEN: usize = 24;

pub struct Vault {
    cipher: XChaCha20Poly1305,
}

impl fmt::Debug for Vault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Vault(..)")
    }
}

impl Vault {
    pub fn from_key(key: [u8; 32]) -> Self {
        Self {
            cipher: XChaCha20Poly1305::new(&Key::from(key)),
        }
    }

    /// Random key that only lives as long as the process. Used for in-memory databases.
    pub fn ephemeral() -> Self {
        Self::from_key(random_key())
    }

    /// Key from `SECRETS_KEY`, else from the key file next to the database, created on
    /// first start.
    pub fn load(config: &Config) -> Result<Self> {
        if let Some(hex_key) = &config.secrets_key {
            return parse_key(hex_key).map(Self::from_key).context(
                "SECRETS_KEY must be 64 hex characters, e.g. from `openssl rand -hex 32`",
            );
        }
        let Some(dir) = db::data_dir(&config.database_url) else {
            tracing::warn!(
                "in-memory database without SECRETS_KEY: stored API keys are lost on restart"
            );
            return Ok(Self::ephemeral());
        };
        let path = dir.join(KEY_FILE);
        let key = load_or_create_key_file(&path)?;
        Ok(Self::from_key(key))
    }

    /// Encrypts `plaintext`. `context` is authenticated but not stored, so a sealed value
    /// only opens with the same context (e.g. the row it belongs to).
    pub fn seal(&self, plaintext: &str, context: &str) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        rand::fill(&mut nonce);
        let payload = Payload {
            msg: plaintext.as_bytes(),
            aad: context.as_bytes(),
        };
        let sealed = self
            .cipher
            .encrypt(&XNonce::from(nonce), payload)
            .expect("XChaCha20-Poly1305 encryption cannot fail for in-memory buffers");
        let mut out = Vec::with_capacity(1 + NONCE_LEN + sealed.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&sealed);
        out
    }

    /// Decrypts a value from [`seal`](Self::seal). None when the key or context differ, or
    /// the data was tampered with.
    pub fn open(&self, sealed: &[u8], context: &str) -> Option<String> {
        let (&version, rest) = sealed.split_first()?;
        if version != VERSION || rest.len() < NONCE_LEN {
            return None;
        }
        let (nonce, ciphertext) = rest.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().ok()?;
        let payload = Payload {
            msg: ciphertext,
            aad: context.as_bytes(),
        };
        let plain = self.cipher.decrypt(&XNonce::from(nonce), payload).ok()?;
        String::from_utf8(plain).ok()
    }
}

fn random_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rand::fill(&mut key);
    key
}

fn parse_key(hex_key: &str) -> Option<[u8; 32]> {
    hex::decode(hex_key.trim()).ok()?.try_into().ok()
}

fn load_or_create_key_file(path: &Path) -> Result<[u8; 32]> {
    let key = random_key();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);

    match options.open(path) {
        Ok(mut file) => {
            file.write_all(hex::encode(key).as_bytes())
                .and_then(|_| file.sync_all())
                .with_context(|| format!("failed to write {}", path.display()))?;
            tracing::warn!(
                "created {} to encrypt stored API keys. Back it up together with the database, \
                 without it saved keys have to be entered again.",
                path.display()
            );
            Ok(key)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            match parse_key(&content) {
                Some(key) => Ok(key),
                None => bail!(
                    "{} is corrupt: it must hold 64 hex characters",
                    path.display()
                ),
            }
        }
        Err(e) => Err(e).with_context(|| format!("failed to create {}", path.display())),
    }
}

/// A string that must never end up in logs. `Debug` prints a placeholder.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(database_url: &str, secrets_key: Option<&str>) -> Config {
        let mut config = crate::test_util::config();
        config.database_url = database_url.to_owned();
        config.secrets_key = secrets_key.map(str::to_owned);
        config
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "scorpio-test-{name}-{}-{}",
            std::process::id(),
            crate::auth::random_token()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_with_context() {
        let vault = Vault::ephemeral();
        let sealed = vault.seal("pk_123_secret", "clickup:1:clickup");
        assert_eq!(
            vault.open(&sealed, "clickup:1:clickup").as_deref(),
            Some("pk_123_secret")
        );
        assert!(
            !sealed
                .windows("pk_123_secret".len())
                .any(|w| w == b"pk_123_secret")
        );
        // Same plaintext seals differently every time.
        assert_ne!(sealed, vault.seal("pk_123_secret", "clickup:1:clickup"));
    }

    #[test]
    fn wrong_context_key_or_tampering_fails() {
        let vault = Vault::from_key([7; 32]);
        let sealed = vault.seal("secret", "clickup:1:clickup");
        assert_eq!(vault.open(&sealed, "clickup:2:clickup"), None);
        assert_eq!(
            Vault::from_key([8; 32]).open(&sealed, "clickup:1:clickup"),
            None
        );

        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(vault.open(&tampered, "clickup:1:clickup"), None);
        let mut wrong_version = sealed.clone();
        wrong_version[0] = 9;
        assert_eq!(vault.open(&wrong_version, "clickup:1:clickup"), None);
        assert_eq!(vault.open(&[], "x"), None);
        assert_eq!(vault.open(&sealed[..10], "clickup:1:clickup"), None);
    }

    #[test]
    fn env_key_is_validated() {
        let hex_key = "ab".repeat(32);
        let a = Vault::load(&config("sqlite::memory:", Some(&hex_key))).unwrap();
        let b = Vault::load(&config("sqlite::memory:", Some(&hex_key))).unwrap();
        assert_eq!(b.open(&a.seal("x", "c"), "c").as_deref(), Some("x"));

        for bad in ["abc", "zz".repeat(32).as_str(), "ab".repeat(31).as_str()] {
            assert!(
                Vault::load(&config("sqlite::memory:", Some(bad))).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn key_file_created_once_and_reused() {
        let dir = temp_dir("vault");
        let url = format!("sqlite://{}/scorpio.db?mode=rwc", dir.display());
        let first = Vault::load(&config(&url, None)).unwrap();
        let path = dir.join(KEY_FILE);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.len(), 64);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let second = Vault::load(&config(&url, None)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        assert_eq!(
            second.open(&first.seal("x", "c"), "c").as_deref(),
            Some("x")
        );

        std::fs::write(&path, "garbage").unwrap();
        assert!(Vault::load(&config(&url, None)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn secret_string_debug_is_redacted() {
        let s = SecretString::new("pk_1_abc");
        assert_eq!(format!("{s:?}"), "SecretString(***)");
        assert_eq!(s.expose(), "pk_1_abc");
    }
}
