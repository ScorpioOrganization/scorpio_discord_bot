pub mod calendar;
pub mod clickup;
pub mod greeting;
pub mod recurring;
pub mod text;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use serenity::all::{Context, GatewayIntents, Member};
use sqlx::SqlitePool;

use crate::db::sf;
use crate::state::{AppState, DiscordHandle};

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

    /// Gateway intents the module's event hooks rely on, on top of `GUILDS`.
    fn intents(&self) -> GatewayIntents {
        GatewayIntents::empty()
    }

    async fn on_member_join(
        &self,
        _ctx: &Context,
        _state: &AppState,
        _member: &Member,
    ) -> Result<()> {
        Ok(())
    }

    /// Whether [`on_tick`](Self::on_tick) should be called. Off by default.
    fn ticks(&self) -> bool {
        false
    }

    /// Periodic work, called by `bot::scheduler` about once a minute for every guild the
    /// module is enabled in, while the bot is connected.
    async fn on_tick(
        &self,
        _state: &AppState,
        _discord: &DiscordHandle,
        _guild_id: u64,
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
    vec![
        Arc::new(greeting::GreetingModule),
        Arc::new(clickup::ClickUpModule),
        Arc::new(calendar::CalendarModule),
        Arc::new(recurring::RecurringModule),
    ]
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

/// Guilds a module is switched on in, whether or not the bot is still a member.
pub async fn enabled_guilds(db: &SqlitePool, module_id: &str) -> Result<Vec<u64>> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT guild_id FROM guild_modules WHERE module_id = ? AND enabled = 1 ORDER BY guild_id",
    )
    .bind(module_id.to_owned())
    .fetch_all(db)
    .await?;
    Ok(ids.into_iter().map(|id| id as u64).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util;

    #[tokio::test]
    async fn modules_off_by_default_and_toggle_per_guild() {
        let db = test_util::pool().await;
        assert!(!is_enabled(&db, 1, "greeting").await.unwrap());
        assert_eq!(enabled_count(&db, 1).await.unwrap(), 0);

        set_enabled(&db, 1, "greeting", true).await.unwrap();
        set_enabled(&db, 1, "other", true).await.unwrap();
        assert!(is_enabled(&db, 1, "greeting").await.unwrap());
        assert!(!is_enabled(&db, 2, "greeting").await.unwrap());
        assert_eq!(enabled_count(&db, 1).await.unwrap(), 2);

        set_enabled(&db, 3, "greeting", true).await.unwrap();
        assert_eq!(enabled_guilds(&db, "greeting").await.unwrap(), [1, 3]);

        set_enabled(&db, 1, "greeting", false).await.unwrap();
        assert!(!is_enabled(&db, 1, "greeting").await.unwrap());
        assert_eq!(enabled_count(&db, 1).await.unwrap(), 1);
        assert_eq!(enabled_guilds(&db, "greeting").await.unwrap(), [3]);
        assert_eq!(enabled_guilds(&db, "other").await.unwrap(), [1]);
    }

    #[tokio::test]
    async fn counters_per_guild_and_total() {
        let db = test_util::pool().await;
        assert_eq!(counter(&db, Some(1), "m", "sent").await.unwrap(), 0);
        assert_eq!(counter(&db, None, "m", "sent").await.unwrap(), 0);

        for _ in 0..3 {
            bump_counter(&db, 1, "m", "sent").await.unwrap();
        }
        bump_counter(&db, 2, "m", "sent").await.unwrap();
        bump_counter(&db, 2, "m", "failed").await.unwrap();
        bump_counter(&db, 2, "other", "sent").await.unwrap();

        assert_eq!(counter(&db, Some(1), "m", "sent").await.unwrap(), 3);
        assert_eq!(counter(&db, Some(2), "m", "sent").await.unwrap(), 1);
        assert_eq!(counter(&db, None, "m", "sent").await.unwrap(), 4);
        assert_eq!(counter(&db, None, "m", "failed").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn snowflakes_above_i64_max_round_trip() {
        let db = test_util::pool().await;
        let gid = u64::MAX - 5;
        set_enabled(&db, gid, "greeting", true).await.unwrap();
        bump_counter(&db, gid, "greeting", "sent").await.unwrap();
        assert!(is_enabled(&db, gid, "greeting").await.unwrap());
        assert!(!is_enabled(&db, gid - 1, "greeting").await.unwrap());
        assert_eq!(enabled_guilds(&db, "greeting").await.unwrap(), [gid]);
        assert_eq!(
            counter(&db, Some(gid), "greeting", "sent").await.unwrap(),
            1
        );
    }
}
