//! Per guild ClickUp API keys.
//!
//! Keys are stored sealed by [`Vault`] and are write-only for the dashboard: after saving,
//! only the ClickUp account name and the last four characters are ever shown. The ClickUp
//! module owns the shared key ([`SHARED`]); any other module can either use that one or
//! keep its own key under its module id.

use anyhow::Result;
use sqlx::SqlitePool;

use super::ClickUpError;
use crate::db::{now, sf};
use crate::modules::{self, bump_counter};
use crate::secrets::{SecretString, Vault};
use crate::state::AppState;

/// Owner of the shared key, which is also the ClickUp module's id.
pub const SHARED: &str = "clickup";
const MIN_LEN: usize = 10;
const MAX_LEN: usize = 200;
const MAX_ERROR_LEN: usize = 300;

/// Trims a pasted key and checks it looks like a ClickUp personal API key.
pub fn check_format(raw: &str) -> Option<SecretString> {
    let key = raw.trim();
    let ok = key.starts_with("pk_")
        && (MIN_LEN..=MAX_LEN).contains(&key.len())
        && key.chars().all(|c| c.is_ascii_graphic());
    ok.then(|| SecretString::new(key))
}

/// Authenticated data for the sealed key, so a key copied to another row does not open.
fn context(guild_id: u64, owner: &str) -> String {
    format!("clickup:{guild_id}:{owner}")
}

/// Everything about a stored key except the key itself.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TokenInfo {
    pub hint: String,
    pub clickup_user: String,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
    pub updated_at: i64,
    pub updated_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenProblem {
    NotSet,
    /// The module uses the shared key but the ClickUp module is switched off.
    SharedModuleDisabled,
    /// Stored, but cannot be decrypted, e.g. after `SECRETS_KEY` changed.
    Unreadable,
}

impl TokenProblem {
    pub fn message(self) -> &'static str {
        match self {
            TokenProblem::NotSet => "No ClickUp API key is set.",
            TokenProblem::SharedModuleDisabled => {
                "Set to use the shared key, but the ClickUp module is disabled in this server."
            }
            TokenProblem::Unreadable => {
                "The saved ClickUp API key cannot be decrypted (was SECRETS_KEY or secret.key \
                 changed?). Set the key again."
            }
        }
    }
}

/// A usable key and the row it came from.
#[derive(Debug, Clone)]
pub struct Credential {
    pub owner: String,
    pub token: SecretString,
}

#[allow(clippy::too_many_arguments)]
pub async fn store(
    db: &SqlitePool,
    vault: &Vault,
    guild_id: u64,
    owner: &str,
    token: &SecretString,
    clickup_user: &str,
    by: &str,
) -> Result<()> {
    let key = token.expose();
    let hint: String = key
        .chars()
        .skip(key.chars().count().saturating_sub(4))
        .collect();
    let sealed = vault.seal(key, &context(guild_id, owner));
    sqlx::query(
        "INSERT INTO clickup_tokens
             (guild_id, owner, sealed, hint, clickup_user, last_ok_at, last_error, last_error_at,
              updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?)
         ON CONFLICT (guild_id, owner) DO UPDATE SET
             sealed = excluded.sealed,
             hint = excluded.hint,
             clickup_user = excluded.clickup_user,
             last_ok_at = excluded.last_ok_at,
             last_error = NULL,
             last_error_at = NULL,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by",
    )
    .bind(sf(guild_id))
    .bind(owner.to_owned())
    .bind(sealed)
    .bind(hint)
    .bind(clickup_user.to_owned())
    .bind(now())
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn info(db: &SqlitePool, guild_id: u64, owner: &str) -> Result<Option<TokenInfo>> {
    Ok(sqlx::query_as(
        "SELECT hint, clickup_user, last_ok_at, last_error, last_error_at, updated_at, updated_by
         FROM clickup_tokens WHERE guild_id = ? AND owner = ?",
    )
    .bind(sf(guild_id))
    .bind(owner.to_owned())
    .fetch_optional(db)
    .await?)
}

pub async fn delete(db: &SqlitePool, guild_id: u64, owner: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM clickup_tokens WHERE guild_id = ? AND owner = ?")
        .bind(sf(guild_id))
        .bind(owner.to_owned())
        .execute(db)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// The key stored for exactly this owner.
pub async fn load(
    db: &SqlitePool,
    vault: &Vault,
    guild_id: u64,
    owner: &str,
) -> Result<Result<SecretString, TokenProblem>> {
    let sealed: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT sealed FROM clickup_tokens WHERE guild_id = ? AND owner = ?")
            .bind(sf(guild_id))
            .bind(owner.to_owned())
            .fetch_optional(db)
            .await?;
    let Some(sealed) = sealed else {
        return Ok(Err(TokenProblem::NotSet));
    };
    Ok(vault
        .open(&sealed, &context(guild_id, owner))
        .map(SecretString::new)
        .ok_or(TokenProblem::Unreadable))
}

/// Whether a module uses the shared key. True unless it was switched to its own key.
pub async fn use_shared(db: &SqlitePool, guild_id: u64, owner: &str) -> Result<bool> {
    if owner == SHARED {
        return Ok(false);
    }
    let value: Option<bool> = sqlx::query_scalar(
        "SELECT use_shared FROM clickup_token_source WHERE guild_id = ? AND owner = ?",
    )
    .bind(sf(guild_id))
    .bind(owner.to_owned())
    .fetch_optional(db)
    .await?;
    Ok(value.unwrap_or(true))
}

pub async fn set_use_shared(
    db: &SqlitePool,
    guild_id: u64,
    owner: &str,
    use_shared: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO clickup_token_source (guild_id, owner, use_shared) VALUES (?, ?, ?)
         ON CONFLICT (guild_id, owner) DO UPDATE SET use_shared = excluded.use_shared",
    )
    .bind(sf(guild_id))
    .bind(owner.to_owned())
    .bind(use_shared)
    .execute(db)
    .await?;
    Ok(())
}

/// The key `module_id` should use in a guild: its own, or the shared one while the ClickUp
/// module is enabled.
pub async fn resolve(
    state: &AppState,
    guild_id: u64,
    module_id: &str,
) -> Result<Result<Credential, TokenProblem>> {
    let owner = if use_shared(&state.db, guild_id, module_id).await? {
        if !modules::is_enabled(&state.db, guild_id, SHARED).await? {
            return Ok(Err(TokenProblem::SharedModuleDisabled));
        }
        SHARED
    } else {
        module_id
    };
    Ok(load(&state.db, &state.vault, guild_id, owner)
        .await?
        .map(|token| Credential {
            owner: owner.to_owned(),
            token,
        }))
}

/// Records the outcome of a ClickUp call made with `cred` for `module_id`: the key's last
/// success or error, and the module's `api_ok` / `api_err` counters. Returns `result`.
/// Bookkeeping failures are only logged.
pub async fn track<T>(
    db: &SqlitePool,
    guild_id: u64,
    module_id: &str,
    cred: &Credential,
    result: Result<T, ClickUpError>,
) -> Result<T, ClickUpError> {
    let outcome = async {
        match &result {
            Ok(_) => {
                sqlx::query(
                    "UPDATE clickup_tokens SET last_ok_at = ? WHERE guild_id = ? AND owner = ?",
                )
                .bind(now())
                .bind(sf(guild_id))
                .bind(cred.owner.clone())
                .execute(db)
                .await?;
                bump_counter(db, guild_id, module_id, "api_ok").await
            }
            Err(e) => {
                let mut msg = e.to_string();
                msg.truncate(msg.floor_char_boundary(MAX_ERROR_LEN));
                sqlx::query(
                    "UPDATE clickup_tokens SET last_error = ?, last_error_at = ?
                     WHERE guild_id = ? AND owner = ?",
                )
                .bind(msg)
                .bind(now())
                .bind(sf(guild_id))
                .bind(cred.owner.clone())
                .execute(db)
                .await?;
                bump_counter(db, guild_id, module_id, "api_err").await
            }
        }
    };
    if let Err(e) = outcome.await {
        tracing::error!(
            guild_id,
            module = module_id,
            "could not record ClickUp call: {e:#}"
        );
    }
    if let Err(e) = &result {
        tracing::warn!(guild_id, module = module_id, "ClickUp request failed: {e}");
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::modules::{counter, set_enabled};
    use crate::test_util;

    const KEY: &str = "pk_123_ABCDEFGHIJ";

    fn key() -> SecretString {
        SecretString::new(KEY)
    }

    #[test]
    fn format_check() {
        assert_eq!(
            check_format("  pk_123_ABCDEFGHIJ \n").unwrap().expose(),
            KEY
        );
        for bad in [
            "",
            "pk_1",
            "sk_123_ABCDEFGHIJ",
            "pk_123 ABCDEFGHIJ",
            "pk_ł23_ABCDEFGHIJ",
        ] {
            assert!(check_format(bad).is_none(), "{bad}");
        }
        assert!(check_format(&format!("pk_{}", "a".repeat(300))).is_none());
    }

    #[tokio::test]
    async fn store_info_load_delete() {
        let state = test_util::state().await;
        let (db, vault) = (&state.db, &state.vault);
        assert!(info(db, 1, SHARED).await.unwrap().is_none());
        assert_eq!(
            load(db, vault, 1, SHARED).await.unwrap(),
            Err(TokenProblem::NotSet)
        );

        store(db, vault, 1, SHARED, &key(), "Test Owner", "root")
            .await
            .unwrap();
        let i = info(db, 1, SHARED).await.unwrap().unwrap();
        assert_eq!(i.hint, "GHIJ");
        assert_eq!(i.clickup_user, "Test Owner");
        assert_eq!(i.updated_by.as_deref(), Some("root"));
        assert_eq!(
            load(db, vault, 1, SHARED).await.unwrap().unwrap().expose(),
            KEY
        );

        let sealed: Vec<u8> = sqlx::query_scalar("SELECT sealed FROM clickup_tokens")
            .fetch_one(db)
            .await
            .unwrap();
        assert!(!sealed.windows(KEY.len()).any(|w| w == KEY.as_bytes()));

        assert!(delete(db, 1, SHARED).await.unwrap());
        assert!(!delete(db, 1, SHARED).await.unwrap());
        assert_eq!(
            load(db, vault, 1, SHARED).await.unwrap(),
            Err(TokenProblem::NotSet)
        );
    }

    #[tokio::test]
    async fn copied_rows_and_changed_vault_keys_do_not_open() {
        let mut state = test_util::state().await;
        let db = state.db.clone();
        store(&db, &state.vault, 1, SHARED, &key(), "u", "root")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO clickup_tokens (guild_id, owner, sealed, hint, clickup_user, updated_at)
             SELECT 2, owner, sealed, hint, clickup_user, updated_at FROM clickup_tokens",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(
            load(&db, &state.vault, 2, SHARED).await.unwrap(),
            Err(TokenProblem::Unreadable)
        );

        state.vault = Arc::new(Vault::ephemeral());
        assert_eq!(
            load(&db, &state.vault, 1, SHARED).await.unwrap(),
            Err(TokenProblem::Unreadable)
        );
    }

    #[tokio::test]
    async fn resolve_shared_or_own() {
        let state = test_util::state().await;
        let (db, vault) = (&state.db, &state.vault);

        // Modules default to the shared key, which needs the ClickUp module enabled.
        assert!(use_shared(db, 1, "calendar").await.unwrap());
        assert_eq!(
            resolve(&state, 1, "calendar").await.unwrap().unwrap_err(),
            TokenProblem::SharedModuleDisabled
        );
        set_enabled(db, 1, SHARED, true).await.unwrap();
        assert_eq!(
            resolve(&state, 1, "calendar").await.unwrap().unwrap_err(),
            TokenProblem::NotSet
        );
        store(db, vault, 1, SHARED, &key(), "u", "root")
            .await
            .unwrap();
        let cred = resolve(&state, 1, "calendar").await.unwrap().unwrap();
        assert_eq!(cred.owner, SHARED);

        // Switching to its own key ignores the shared one.
        set_use_shared(db, 1, "calendar", false).await.unwrap();
        assert_eq!(
            resolve(&state, 1, "calendar").await.unwrap().unwrap_err(),
            TokenProblem::NotSet
        );
        let own = SecretString::new("pk_9_OWNKEYOWNKEY");
        store(db, vault, 1, "calendar", &own, "u2", "root")
            .await
            .unwrap();
        let cred = resolve(&state, 1, "calendar").await.unwrap().unwrap();
        assert_eq!(
            (cred.owner.as_str(), cred.token.expose()),
            ("calendar", own.expose())
        );

        // The ClickUp module itself always uses its own row, enabled or not.
        set_enabled(db, 1, SHARED, false).await.unwrap();
        assert!(!use_shared(db, 1, SHARED).await.unwrap());
        assert_eq!(
            resolve(&state, 1, SHARED).await.unwrap().unwrap().owner,
            SHARED
        );
    }

    #[tokio::test]
    async fn track_records_status_and_counters() {
        let state = test_util::state().await;
        let db = &state.db;
        store(db, &state.vault, 1, SHARED, &key(), "u", "root")
            .await
            .unwrap();
        let cred = Credential {
            owner: SHARED.into(),
            token: key(),
        };

        assert_eq!(track(db, 1, "calendar", &cred, Ok(5)).await, Ok(5));
        let err: Result<(), _> = Err(ClickUpError::Unauthorized);
        assert_eq!(track(db, 1, "calendar", &cred, err.clone()).await, err);

        let i = info(db, 1, SHARED).await.unwrap().unwrap();
        assert!(i.last_ok_at.is_some());
        assert_eq!(
            i.last_error.as_deref(),
            Some("ClickUp rejected the API key")
        );
        assert_eq!(counter(db, Some(1), "calendar", "api_ok").await.unwrap(), 1);
        assert_eq!(
            counter(db, Some(1), "calendar", "api_err").await.unwrap(),
            1
        );

        // Saving the key again clears the old error.
        store(db, &state.vault, 1, SHARED, &key(), "u", "root")
            .await
            .unwrap();
        assert!(
            info(db, 1, SHARED)
                .await
                .unwrap()
                .unwrap()
                .last_error
                .is_none()
        );
    }
}
