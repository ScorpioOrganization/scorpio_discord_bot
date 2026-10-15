pub mod greeting;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use serenity::all::{Context, Member};
use sqlx::SqlitePool;

use crate::db::sf;
use crate::state::AppState;

/// A bot feature that can be switched on or off per guild from the dashboard.
///
/// Adding a module: implement this trait in a new folder under `src/modules`, then add it
/// to [`registry`]. The bot handler and the dashboard pick it up automatically.
#[async_trait]
pub trait Module: Send + Sync + 'static {
    /// Stable identifier used in URLs and the database.
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// A few key numbers shown on the module card of the guild page.
    async fn summary(&self, state: &AppState, guild_id: u64) -> Result<Vec<StatItem>>;

    /// Dashboard routes. Paths must start with `/guilds/{gid}/modules/<id>`.
    fn routes(&self) -> Router<AppState>;

    async fn on_member_join(
        &self,
        _ctx: &Context,
        _state: &AppState,
        _member: &Member,
    ) -> Result<()> {
        Ok(())
    }
}

pub struct StatItem {
    pub label: &'static str,
    pub value: String,
}

impl StatItem {
    pub fn new(label: &'static str, value: impl ToString) -> Self {
        Self {
            label,
            value: value.to_string(),
        }
    }
}

pub fn registry() -> Vec<Arc<dyn Module>> {
    vec![Arc::new(greeting::GreetingModule)]
}

pub async fn is_enabled(db: &SqlitePool, guild_id: u64, module_id: &str) -> Result<bool> {
    let enabled: Option<bool> = sqlx::query_scalar(
        "SELECT enabled FROM guild_modules WHERE guild_id = ? AND module_id = ?",
    )
    .bind(sf(guild_id))
    .bind(module_id.to_owned())
    .fetch_optional(db)
    .await?;
    Ok(enabled.unwrap_or(false))
}

pub async fn set_enabled(
    db: &SqlitePool,
    guild_id: u64,
    module_id: &str,
    enabled: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO guild_modules (guild_id, module_id, enabled) VALUES (?, ?, ?)
         ON CONFLICT (guild_id, module_id) DO UPDATE SET enabled = excluded.enabled",
    )
    .bind(sf(guild_id))
    .bind(module_id.to_owned())
    .bind(enabled)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn enabled_count(db: &SqlitePool, guild_id: u64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM guild_modules WHERE guild_id = ? AND enabled = 1")
            .bind(sf(guild_id))
            .fetch_one(db)
            .await?,
    )
}

/// Increments an all-time counter for a module in a guild.
pub async fn bump_counter(
    db: &SqlitePool,
    guild_id: u64,
    module_id: &str,
    key: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO module_stats (guild_id, module_id, key, value) VALUES (?, ?, ?, 1)
         ON CONFLICT (guild_id, module_id, key) DO UPDATE SET value = value + 1",
    )
    .bind(sf(guild_id))
    .bind(module_id.to_owned())
    .bind(key.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

/// Reads a counter for one guild, or the sum over all guilds when `guild_id` is None.
pub async fn counter(
    db: &SqlitePool,
    guild_id: Option<u64>,
    module_id: &str,
    key: &str,
) -> Result<i64> {
    let value: Option<i64> =
        match guild_id {
            Some(gid) => sqlx::query_scalar(
                "SELECT value FROM module_stats WHERE guild_id = ? AND module_id = ? AND key = ?",
            )
            .bind(sf(gid))
            .bind(module_id.to_owned())
            .bind(key.to_owned())
            .fetch_optional(db)
            .await?,
            None => {
                sqlx::query_scalar(
                    "SELECT SUM(value) FROM module_stats WHERE module_id = ? AND key = ?",
                )
                .bind(module_id.to_owned())
                .bind(key.to_owned())
                .fetch_one(db)
                .await?
            }
        };
    Ok(value.unwrap_or(0))
}
