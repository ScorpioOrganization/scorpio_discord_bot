//! Per project API keys of the services modules talk to (ClickUp, Discord).
//!
//! Keys are stored sealed by [`Vault`] and are write-only for the dashboard: after saving,
//! only the account name and the last four characters are ever shown. Every service has a
//! module owning its shared key ([`Service::shared_owner`]); any other module can either use
//! that one or keep its own key under its module id.

pub mod web;

use std::fmt::Display;

use anyhow::Result;
use sqlx::{SqliteExecutor, SqlitePool};

use crate::db::now;
use crate::modules::{self, bump_counter};
use crate::secrets::{SecretString, Vault};
use crate::state::AppState;

const MAX_ERROR_LEN: usize = 300;

/// An external service a module needs a key for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Service {
    ClickUp,
    Discord,
}

impl Service {
    /// Stored in the `service` column.
    pub fn id(self) -> &'static str {
        match self {
            Service::ClickUp => "clickup",
            Service::Discord => "discord",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Service::ClickUp => "ClickUp",
            Service::Discord => "Discord",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "clickup" => Some(Service::ClickUp),
            "discord" => Some(Service::Discord),
            _ => None,
        }
    }

    /// Id of the module that holds the shared key.
    pub fn shared_owner(self) -> &'static str {
        match self {
            Service::ClickUp => crate::modules::clickup::ID,
            Service::Discord => crate::modules::discord::ID,
        }
    }

    /// What the service calls its key.
    pub fn key_name(self) -> &'static str {
        match self {
            Service::ClickUp => "API key",
            Service::Discord => "bot token",
        }
    }

    /// Trims a pasted key and checks it looks like a key of this service.
    pub fn check_format(self, raw: &str) -> Option<SecretString> {
        let key = raw.trim();
        let ok = match self {
            Service::ClickUp => {
                key.starts_with("pk_")
                    && (10..=200).contains(&key.len())
                    && key.chars().all(|c| c.is_ascii_graphic())
            }
            Service::Discord => {
                let key = key.strip_prefix("Bot ").unwrap_or(key).trim();
                return ((50..=200).contains(&key.len())
                    && key.split('.').count() == 3
                    && key.split('.').all(|part| !part.is_empty())
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
                .then(|| SecretString::new(key));
            }
        };
        ok.then(|| SecretString::new(key))
    }
}

/// Authenticated data for the sealed key, so a key copied to another row does not open.
fn context(project_id: i64, service: Service, owner: &str) -> String {
    format!("key:{}:{project_id}:{owner}", service.id())
}

/// Context of keys sealed while they belonged to a Discord server, before projects. Only
/// ClickUp keys existed then.
pub fn legacy_context(guild_id: u64, owner: &str) -> String {
    format!("clickup:{guild_id}:{owner}")
}

/// Everything about a stored key except the key itself.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct KeyInfo {
    pub hint: String,
    pub account: String,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
    pub updated_at: i64,
    pub updated_by: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProblem {
    NotSet,
    /// The module uses the shared key but the module holding it is switched off.
    SharedModuleDisabled,
    /// Stored, but cannot be decrypted, e.g. after `SECRETS_KEY` changed.
    Unreadable,
}

impl KeyProblem {
    pub fn message(self, service: Service) -> &'static str {
        match (self, service) {
            (KeyProblem::NotSet, Service::ClickUp) => "No ClickUp API key is set.",
            (KeyProblem::NotSet, Service::Discord) => "No Discord bot token is set.",
            (KeyProblem::SharedModuleDisabled, Service::ClickUp) => {
                "Set to use the shared key, but the ClickUp module is disabled in this project."
            }
            (KeyProblem::SharedModuleDisabled, Service::Discord) => {
                "Set to use the shared bot, but the Discord module is disabled in this project."
            }
            (KeyProblem::Unreadable, Service::ClickUp) => {
                "The saved ClickUp API key cannot be decrypted (was SECRETS_KEY or secret.key \
                 changed?). Set the key again."
            }
            (KeyProblem::Unreadable, Service::Discord) => {
                "The saved Discord bot token cannot be decrypted (was SECRETS_KEY or secret.key \
                 changed?). Set the token again."
            }
        }
    }
}

/// A usable key and the row it came from.
#[derive(Debug, Clone)]
pub struct Credential {
    pub service: Service,
    pub owner: String,
    pub token: SecretString,
}

/// Last four characters, shown to recognize a key.
pub fn hint(key: &str) -> String {
    key.chars()
        .skip(key.chars().count().saturating_sub(4))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub async fn store(
    db: impl SqliteExecutor<'_>,
    vault: &Vault,
    project_id: i64,
    service: Service,
    owner: &str,
    token: &SecretString,
    account: &str,
    by: &str,
) -> Result<()> {
    let key = token.expose();
    let sealed = vault.seal(key, &context(project_id, service, owner));
    sqlx::query(
        "INSERT INTO api_keys
             (project_id, service, owner, sealed, hint, account, last_ok_at, last_error,
              last_error_at, updated_at, updated_by, legacy_guild_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?, NULL)
         ON CONFLICT (project_id, service, owner) DO UPDATE SET
             sealed = excluded.sealed,
             hint = excluded.hint,
             account = excluded.account,
             last_ok_at = excluded.last_ok_at,
             last_error = NULL,
             last_error_at = NULL,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by,
             legacy_guild_id = NULL",
    )
    .bind(project_id)
    .bind(service.id())
    .bind(owner.to_owned())
    .bind(sealed)
    .bind(hint(key))
    .bind(account.to_owned())
    .bind(now())
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn info(
    db: &SqlitePool,
    project_id: i64,
    service: Service,
    owner: &str,
) -> Result<Option<KeyInfo>> {
    Ok(sqlx::query_as(
        "SELECT hint, account, last_ok_at, last_error, last_error_at, updated_at, updated_by
         FROM api_keys WHERE project_id = ? AND service = ? AND owner = ?",
    )
    .bind(project_id)
    .bind(service.id())
    .bind(owner.to_owned())
    .fetch_optional(db)
    .await?)
}

pub async fn delete(
    db: &SqlitePool,
    project_id: i64,
    service: Service,
    owner: &str,
) -> Result<bool> {
    let res =
        sqlx::query("DELETE FROM api_keys WHERE project_id = ? AND service = ? AND owner = ?")
            .bind(project_id)
            .bind(service.id())
            .bind(owner.to_owned())
            .execute(db)
            .await?;
    Ok(res.rows_affected() > 0)
}

/// Updates the account name shown for a key, e.g. once a bot reports its name.
pub async fn set_account(
    db: &SqlitePool,
    project_id: i64,
    service: Service,
    owner: &str,
    account: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE api_keys SET account = ? WHERE project_id = ? AND service = ? AND owner = ?",
    )
    .bind(account.to_owned())
    .bind(project_id)
    .bind(service.id())
    .bind(owner.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

/// The key stored for exactly this owner.
pub async fn load(
    db: &SqlitePool,
    vault: &Vault,
    project_id: i64,
    service: Service,
    owner: &str,
) -> Result<Result<SecretString, KeyProblem>> {
    let row: Option<(Vec<u8>, Option<i64>)> = sqlx::query_as(
        "SELECT sealed, legacy_guild_id FROM api_keys
         WHERE project_id = ? AND service = ? AND owner = ?",
    )
    .bind(project_id)
    .bind(service.id())
    .bind(owner.to_owned())
    .fetch_optional(db)
    .await?;
    let Some((sealed, legacy_guild)) = row else {
        return Ok(Err(KeyProblem::NotSet));
    };
    let ctx = match legacy_guild {
        // Not sealed for the project yet, see `upgrade::reseal_legacy_keys`.
        Some(guild) => legacy_context(guild as u64, owner),
        None => context(project_id, service, owner),
    };
    Ok(vault
        .open(&sealed, &ctx)
        .map(SecretString::new)
        .ok_or(KeyProblem::Unreadable))
}

/// Seals keys stored before projects existed for their project. Keys that do not open
/// (the vault key changed) are left as they are and tried again on the next start. Returns
/// how many keys were sealed again.
pub async fn reseal_legacy(db: &SqlitePool, vault: &Vault) -> Result<usize> {
    let rows: Vec<(i64, String, String, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT project_id, service, owner, sealed, legacy_guild_id FROM api_keys
         WHERE legacy_guild_id IS NOT NULL",
    )
    .fetch_all(db)
    .await?;
    let mut done = 0;
    for (project_id, service_id, owner, sealed, guild) in rows {
        let Some(service) = Service::from_id(&service_id) else {
            continue;
        };
        let Some(key) = vault.open(&sealed, &legacy_context(guild as u64, &owner)) else {
            tracing::warn!(
                project_id,
                owner,
                "a saved {} key cannot be decrypted, so it was not upgraded",
                service.name()
            );
            continue;
        };
        let resealed = vault.seal(&key, &context(project_id, service, &owner));
        sqlx::query(
            "UPDATE api_keys SET sealed = ?, legacy_guild_id = NULL
             WHERE project_id = ? AND service = ? AND owner = ? AND legacy_guild_id = ?",
        )
        .bind(resealed)
        .bind(project_id)
        .bind(service.id())
        .bind(owner)
        .bind(guild)
        .execute(db)
        .await?;
        done += 1;
    }
    Ok(done)
}

/// Whether a module uses the shared key. True unless it was switched to its own key.
pub async fn use_shared(
    db: &SqlitePool,
    project_id: i64,
    service: Service,
    owner: &str,
) -> Result<bool> {
    if owner == service.shared_owner() {
        return Ok(false);
    }
    let value: Option<bool> = sqlx::query_scalar(
        "SELECT use_shared FROM api_key_source WHERE project_id = ? AND service = ? AND owner = ?",
    )
    .bind(project_id)
    .bind(service.id())
    .bind(owner.to_owned())
    .fetch_optional(db)
    .await?;
    Ok(value.unwrap_or(true))
}

pub async fn set_use_shared(
    db: &SqlitePool,
    project_id: i64,
    service: Service,
    owner: &str,
    use_shared: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO api_key_source (project_id, service, owner, use_shared) VALUES (?, ?, ?, ?)
         ON CONFLICT (project_id, service, owner) DO UPDATE SET use_shared = excluded.use_shared",
    )
    .bind(project_id)
    .bind(service.id())
    .bind(owner.to_owned())
    .bind(use_shared)
    .execute(db)
    .await?;
    Ok(())
}

/// The key `module_id` should use in a project: its own, or the shared one while the module
/// holding it is enabled.
pub async fn resolve(
    state: &AppState,
    project_id: i64,
    service: Service,
    module_id: &str,
) -> Result<Result<Credential, KeyProblem>> {
    let owner = if use_shared(&state.db, project_id, service, module_id).await? {
        if !modules::is_enabled(&state.db, project_id, service.shared_owner()).await? {
            return Ok(Err(KeyProblem::SharedModuleDisabled));
        }
        service.shared_owner()
    } else {
        module_id
    };
    Ok(load(&state.db, &state.vault, project_id, service, owner)
        .await?
        .map(|token| Credential {
            service,
            owner: owner.to_owned(),
            token,
        }))
}

/// Records the outcome of a call made with `cred` for `module_id`: the key's last success or
/// error, and the module's `api_ok` / `api_err` counters. Returns `result`. Bookkeeping
/// failures are only logged.
pub async fn track<T, E: Display>(
    db: &SqlitePool,
    project_id: i64,
    module_id: &str,
    cred: &Credential,
    result: Result<T, E>,
) -> Result<T, E> {
    let outcome = async {
        match &result {
            Ok(_) => {
                sqlx::query(
                    "UPDATE api_keys SET last_ok_at = ?
                     WHERE project_id = ? AND service = ? AND owner = ?",
                )
                .bind(now())
                .bind(project_id)
                .bind(cred.service.id())
                .bind(cred.owner.clone())
                .execute(db)
                .await?;
                bump_counter(db, project_id, module_id, "api_ok").await
            }
            Err(e) => {
                let mut msg = e.to_string();
                msg.truncate(msg.floor_char_boundary(MAX_ERROR_LEN));
                sqlx::query(
                    "UPDATE api_keys SET last_error = ?, last_error_at = ?
                     WHERE project_id = ? AND service = ? AND owner = ?",
                )
                .bind(msg)
                .bind(now())
                .bind(project_id)
                .bind(cred.service.id())
                .bind(cred.owner.clone())
                .execute(db)
                .await?;
                bump_counter(db, project_id, module_id, "api_err").await
            }
        }
    };
    if let Err(e) = outcome.await {
        tracing::error!(
            project_id,
            module = module_id,
            "could not record {} call: {e:#}",
            cred.service.name()
        );
    }
    if let Err(e) = &result {
        tracing::warn!(
            project_id,
            module = module_id,
            "{} request failed: {e}",
            cred.service.name()
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::clickup::ClickUpError;
    use crate::modules::{counter, set_enabled};
    use crate::test_util;

    const KEY: &str = "pk_123_ABCDEFGHIJ";
    const CLICKUP: Service = Service::ClickUp;
    const SHARED: &str = "clickup";

    fn key() -> SecretString {
        SecretString::new(KEY)
    }

    #[test]
    fn clickup_format_check() {
        assert_eq!(
            CLICKUP
                .check_format("  pk_123_ABCDEFGHIJ \n")
                .unwrap()
                .expose(),
            KEY
        );
        for bad in [
            "",
            "pk_1",
            "sk_123_ABCDEFGHIJ",
            "pk_123 ABCDEFGHIJ",
            "pk_ł23_ABCDEFGHIJ",
        ] {
            assert!(CLICKUP.check_format(bad).is_none(), "{bad}");
        }
        assert!(
            CLICKUP
                .check_format(&format!("pk_{}", "a".repeat(300)))
                .is_none()
        );
    }

    #[test]
    fn discord_format_check() {
        let token = test_util::DISCORD_TOKEN;
        let d = Service::Discord;
        assert_eq!(d.check_format(token).unwrap().expose(), token);
        assert_eq!(
            d.check_format(&format!("  Bot {token}\n"))
                .unwrap()
                .expose(),
            token
        );
        let bad = [
            String::new(),
            "abc.def.ghi".into(),
            "pk_123_ABCDEFGHIJ".into(),
            format!("{token}.extra"),
            token.replace('.', ""),
            token.replacen('.', "..", 1),
            format!("{token}!"),
            format!("{token}é"),
            "a.b.".repeat(60),
        ];
        for bad in bad {
            assert!(d.check_format(&bad).is_none(), "{bad}");
        }
    }

    #[tokio::test]
    async fn store_info_load_delete() {
        let state = test_util::state().await;
        let (db, vault) = (&state.db, &state.vault);
        assert!(info(db, 1, CLICKUP, SHARED).await.unwrap().is_none());
        assert_eq!(
            load(db, vault, 1, CLICKUP, SHARED).await.unwrap(),
            Err(KeyProblem::NotSet)
        );

        store(db, vault, 1, CLICKUP, SHARED, &key(), "Test Owner", "root")
            .await
            .unwrap();
        let i = info(db, 1, CLICKUP, SHARED).await.unwrap().unwrap();
        assert_eq!(i.hint, "GHIJ");
        assert_eq!(i.account, "Test Owner");
        assert_eq!(i.updated_by.as_deref(), Some("root"));
        assert_eq!(
            load(db, vault, 1, CLICKUP, SHARED)
                .await
                .unwrap()
                .unwrap()
                .expose(),
            KEY
        );

        let sealed: Vec<u8> = sqlx::query_scalar("SELECT sealed FROM api_keys")
            .fetch_one(db)
            .await
            .unwrap();
        assert!(!sealed.windows(KEY.len()).any(|w| w == KEY.as_bytes()));

        set_account(db, 1, CLICKUP, SHARED, "Renamed")
            .await
            .unwrap();
        assert_eq!(
            info(db, 1, CLICKUP, SHARED).await.unwrap().unwrap().account,
            "Renamed"
        );

        assert!(delete(db, 1, CLICKUP, SHARED).await.unwrap());
        assert!(!delete(db, 1, CLICKUP, SHARED).await.unwrap());
        assert_eq!(
            load(db, vault, 1, CLICKUP, SHARED).await.unwrap(),
            Err(KeyProblem::NotSet)
        );
    }

    #[tokio::test]
    async fn copied_rows_other_services_and_changed_vault_keys_do_not_open() {
        let mut state = test_util::state().await;
        let db = state.db.clone();
        store(&db, &state.vault, 1, CLICKUP, SHARED, &key(), "u", "root")
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO api_keys (project_id, service, owner, sealed, hint, account, updated_at)
             SELECT 2, service, owner, sealed, hint, account, updated_at FROM api_keys
             UNION ALL
             SELECT 1, 'discord', 'discord', sealed, hint, account, updated_at FROM api_keys",
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(
            load(&db, &state.vault, 2, CLICKUP, SHARED).await.unwrap(),
            Err(KeyProblem::Unreadable)
        );
        assert_eq!(
            load(&db, &state.vault, 1, Service::Discord, "discord")
                .await
                .unwrap(),
            Err(KeyProblem::Unreadable)
        );

        state.vault = Arc::new(Vault::ephemeral());
        assert_eq!(
            load(&db, &state.vault, 1, CLICKUP, SHARED).await.unwrap(),
            Err(KeyProblem::Unreadable)
        );
    }

    #[tokio::test]
    async fn legacy_keys_open_until_resealed() {
        let state = test_util::state().await;
        let (db, vault) = (&state.db, &state.vault);
        let guild = u64::MAX - 7;
        let sealed = vault.seal(KEY, &legacy_context(guild, SHARED));
        sqlx::query(
            "INSERT INTO api_keys (project_id, service, owner, sealed, hint, account, updated_at,
                                   legacy_guild_id)
             VALUES (3, 'clickup', 'clickup', ?, 'GHIJ', 'u', 0, ?)",
        )
        .bind(sealed)
        .bind(guild as i64)
        .execute(db)
        .await
        .unwrap();

        assert_eq!(
            load(db, vault, 3, CLICKUP, SHARED)
                .await
                .unwrap()
                .unwrap()
                .expose(),
            KEY
        );
        // A different vault cannot upgrade it: left alone, no error.
        assert_eq!(reseal_legacy(db, &Vault::ephemeral()).await.unwrap(), 0);
        assert_eq!(reseal_legacy(db, vault).await.unwrap(), 1);
        assert_eq!(reseal_legacy(db, vault).await.unwrap(), 0);
        let legacy: Option<i64> = sqlx::query_scalar("SELECT legacy_guild_id FROM api_keys")
            .fetch_one(db)
            .await
            .unwrap();
        assert_eq!(legacy, None);
        assert_eq!(
            load(db, vault, 3, CLICKUP, SHARED)
                .await
                .unwrap()
                .unwrap()
                .expose(),
            KEY
        );
        // Now bound to the project, not to the old server.
        let sealed: Vec<u8> = sqlx::query_scalar("SELECT sealed FROM api_keys")
            .fetch_one(db)
            .await
            .unwrap();
        assert!(
            vault
                .open(&sealed, &legacy_context(guild, SHARED))
                .is_none()
        );
    }

    #[tokio::test]
    async fn resolve_shared_or_own() {
        let state = test_util::state().await;
        let (db, vault) = (&state.db, &state.vault);

        // Modules default to the shared key, which needs the ClickUp module enabled.
        assert!(use_shared(db, 1, CLICKUP, "calendar").await.unwrap());
        assert_eq!(
            resolve(&state, 1, CLICKUP, "calendar")
                .await
                .unwrap()
                .unwrap_err(),
            KeyProblem::SharedModuleDisabled
        );
        set_enabled(db, 1, SHARED, true).await.unwrap();
        assert_eq!(
            resolve(&state, 1, CLICKUP, "calendar")
                .await
                .unwrap()
                .unwrap_err(),
            KeyProblem::NotSet
        );
        store(db, vault, 1, CLICKUP, SHARED, &key(), "u", "root")
            .await
            .unwrap();
        let cred = resolve(&state, 1, CLICKUP, "calendar")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cred.owner, SHARED);

        // The Discord key of the same module is independent.
        assert_eq!(
            resolve(&state, 1, Service::Discord, "calendar")
                .await
                .unwrap()
                .unwrap_err(),
            KeyProblem::SharedModuleDisabled
        );

        // Switching to its own key ignores the shared one.
        set_use_shared(db, 1, CLICKUP, "calendar", false)
            .await
            .unwrap();
        assert_eq!(
            resolve(&state, 1, CLICKUP, "calendar")
                .await
                .unwrap()
                .unwrap_err(),
            KeyProblem::NotSet
        );
        assert!(
            use_shared(db, 1, Service::Discord, "calendar")
                .await
                .unwrap()
        );
        let own = SecretString::new("pk_9_OWNKEYOWNKEY");
        store(db, vault, 1, CLICKUP, "calendar", &own, "u2", "root")
            .await
            .unwrap();
        let cred = resolve(&state, 1, CLICKUP, "calendar")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (cred.owner.as_str(), cred.token.expose()),
            ("calendar", own.expose())
        );

        // The ClickUp module itself always uses its own row, enabled or not.
        set_enabled(db, 1, SHARED, false).await.unwrap();
        assert!(!use_shared(db, 1, CLICKUP, SHARED).await.unwrap());
        assert_eq!(
            resolve(&state, 1, CLICKUP, SHARED)
                .await
                .unwrap()
                .unwrap()
                .owner,
            SHARED
        );
    }

    #[tokio::test]
    async fn track_records_status_and_counters() {
        let state = test_util::state().await;
        let db = &state.db;
        store(db, &state.vault, 1, CLICKUP, SHARED, &key(), "u", "root")
            .await
            .unwrap();
        let cred = Credential {
            service: CLICKUP,
            owner: SHARED.into(),
            token: key(),
        };

        assert_eq!(
            track(db, 1, "calendar", &cred, Ok::<_, ClickUpError>(5)).await,
            Ok(5)
        );
        let err: Result<(), _> = Err(ClickUpError::Unauthorized);
        assert_eq!(track(db, 1, "calendar", &cred, err.clone()).await, err);

        let i = info(db, 1, CLICKUP, SHARED).await.unwrap().unwrap();
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
        store(db, &state.vault, 1, CLICKUP, SHARED, &key(), "u", "root")
            .await
            .unwrap();
        assert!(
            info(db, 1, CLICKUP, SHARED)
                .await
                .unwrap()
                .unwrap()
                .last_error
                .is_none()
        );
    }
}
