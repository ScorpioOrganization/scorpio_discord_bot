pub mod extract;
pub mod limiter;

use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use subtle::ConstantTimeEq;

use crate::db::now;

pub const SESSION_COOKIE: &str = "scorpio_session";
pub const SESSION_TTL_SECS: i64 = 7 * 24 * 60 * 60;
pub const ROOT_USERNAME: &str = "root";
pub const MIN_PASSWORD_LEN: usize = 10;
pub const MAX_PASSWORD_LEN: usize = 256;

/// Logged in dashboard user, attached to the request by the session middleware.
#[derive(Clone, Debug)]
pub struct SessionUser {
    pub user_id: i64,
    pub username: String,
    pub is_root: bool,
    pub csrf: String,
    pub token_hash: String,
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

/// Human friendly random password for generated accounts.
pub fn random_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    (0..24)
        .map(|_| ALPHABET[rand::random_range(0..ALPHABET.len())] as char)
        .collect()
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn ct_eq(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

pub async fn hash_password(password: String) -> Result<String> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| anyhow!("password hashing failed: {e}"))
    })
    .await?
}

/// Verifies a password. When `hash` is None a dummy hash is checked so that unknown
/// usernames take the same time as wrong passwords.
pub async fn verify_password(password: String, hash: Option<String>) -> bool {
    let known = hash.is_some();
    let result = tokio::task::spawn_blocking(move || {
        let hash = hash.unwrap_or_else(|| dummy_hash().to_owned());
        let Ok(parsed) = PasswordHash::new(&hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
    .await;
    known && result.unwrap_or(false)
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        Argon2::default()
            .hash_password(random_token().as_bytes())
            .map(|h| h.to_string())
            .unwrap_or_default()
    })
}

pub fn validate_username(name: &str) -> Result<(), &'static str> {
    let ok_len = (3..=32).contains(&name.len());
    let ok_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if ok_len && ok_chars {
        Ok(())
    } else {
        Err("user_invalid")
    }
}

pub fn validate_password(pw: &str, confirm: &str) -> Result<(), &'static str> {
    if pw != confirm {
        return Err("pw_mismatch");
    }
    let len = pw.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err("pw_short");
    }
    if len > MAX_PASSWORD_LEN {
        return Err("pw_long");
    }
    Ok(())
}

/// Creates a session and returns the raw token for the cookie. Only its hash is stored.
pub async fn create_session(db: &SqlitePool, user_id: i64) -> Result<String> {
    let token = random_token();
    let now = now();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, csrf_token, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(hash_token(&token))
    .bind(user_id)
    .bind(random_token())
    .bind(now)
    .bind(now + SESSION_TTL_SECS)
    .execute(db)
    .await?;
    sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
        .bind(now)
        .bind(user_id)
        .execute(db)
        .await?;
    Ok(token)
}

pub async fn load_session(db: &SqlitePool, token: &str) -> Result<Option<SessionUser>> {
    let token_hash = hash_token(token);
    let row: Option<(i64, String, bool, String)> = sqlx::query_as(
        "SELECT u.id, u.username, u.is_root, s.csrf_token
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = ? AND s.expires_at > ?",
    )
    .bind(&token_hash)
    .bind(now())
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(user_id, username, is_root, csrf)| SessionUser {
        user_id,
        username,
        is_root,
        csrf,
        token_hash,
    }))
}

pub async fn delete_session(db: &SqlitePool, token_hash: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_expired_sessions(db: &SqlitePool) -> Result<u64> {
    let res = sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now())
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

/// Makes sure the root account exists. Returns the generated password when one was created
/// without ROOT_PASSWORD being set.
pub async fn bootstrap_root(db: &SqlitePool, configured: Option<&str>) -> Result<Option<String>> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE is_root = 1")
        .fetch_optional(db)
        .await?;
    if exists.is_some() {
        return Ok(None);
    }

    let (password, generated) = match configured {
        Some(pw) => {
            validate_password(pw, pw).map_err(|_| {
                anyhow!(
                    "ROOT_PASSWORD must be {MIN_PASSWORD_LEN} to {MAX_PASSWORD_LEN} characters long"
                )
            })?;
            (pw.to_owned(), false)
        }
        None => (random_password(), true),
    };

    sqlx::query(
        "INSERT INTO users (username, password_hash, is_root, created_at) VALUES (?, ?, 1, ?)",
    )
    .bind(ROOT_USERNAME)
    .bind(hash_password(password.clone()).await?)
    .bind(now())
    .execute(db)
    .await
    .context("failed to create root account")?;

    if !generated {
        tracing::info!("created dashboard account `{ROOT_USERNAME}` using ROOT_PASSWORD");
    }
    Ok(generated.then_some(password))
}

/// Sets a new random root password and drops all root sessions. Used by the `reset-root` command.
pub async fn reset_root_password(db: &SqlitePool) -> Result<String> {
    let password = random_password();
    let hash = hash_password(password.clone()).await?;
    let res = sqlx::query("UPDATE users SET password_hash = ? WHERE is_root = 1")
        .bind(hash)
        .execute(db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(anyhow!(
            "no root account exists yet, start the bot once to create it"
        ));
    }
    sqlx::query("DELETE FROM sessions WHERE user_id IN (SELECT id FROM users WHERE is_root = 1)")
        .execute(db)
        .await?;
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_and_verify() {
        let hash = hash_password("correct horse battery".into()).await.unwrap();
        assert!(verify_password("correct horse battery".into(), Some(hash.clone())).await);
        assert!(!verify_password("wrong".into(), Some(hash)).await);
        assert!(!verify_password("anything".into(), None).await);
    }

    #[test]
    fn username_rules() {
        assert!(validate_username("alice").is_ok());
        assert!(validate_username("a.b-c_d").is_ok());
        assert!(validate_username("ab").is_err());
        assert!(validate_username("has space").is_err());
        assert!(validate_username("<script>").is_err());
        assert!(validate_username(&"x".repeat(33)).is_err());
    }

    #[test]
    fn password_rules() {
        assert_eq!(validate_password("short", "short"), Err("pw_short"));
        assert_eq!(
            validate_password("longenough1", "different1"),
            Err("pw_mismatch")
        );
        assert!(validate_password("longenough1", "longenough1").is_ok());
    }

    #[test]
    fn tokens() {
        let a = random_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, random_token());
        assert!(ct_eq(&a, &a.clone()));
        assert!(!ct_eq(&a, &random_token()));
        assert_eq!(hash_token("x"), hash_token("x"));
        assert_eq!(random_password().len(), 24);
    }
}
