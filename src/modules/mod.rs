pub mod calendar;
pub mod clickup;
pub mod discord;
pub mod greeting;
pub mod recurring;
pub mod text;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use serenity::all::{Context, GatewayIntents, Member};
use sqlx::{SqliteExecutor, SqlitePool};

use crate::keys::Service;
use crate::state::AppState;

/// A feature that can be switched on or off per project from the dashboard.
///
/// Adding a module: implement this trait in a new folder under `src/modules`, then add it
/// to [`registry`]. The bot handler, the scheduler and the dashboard pick it up
/// automatically.
#[async_trait]
pub trait Module: Send + Sync + 'static {
    /// Stable identifier used in URLs and the database.
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;

    /// Services the module needs a key for. Each gets a key card (shared key or the module's
    /// own) and is listed on the project page. Modules using Discord get its events and keep
    /// their bot connected while enabled.
    fn services(&self) -> &'static [Service] {
        &[]
    }

    /// A few key numbers shown on the module card of the project page.
    async fn summary(&self, state: &AppState, project_id: i64) -> Result<Vec<StatItem>>;

    /// Dashboard routes. Paths must start with `/projects/{pid}/modules/<id>`.
    fn routes(&self) -> Router<AppState>;

    /// Discord gateway intents the module's event hooks rely on, on top of `GUILDS`.
    fn intents(&self) -> GatewayIntents {
        GatewayIntents::empty()
    }

    /// Someone joined the Discord server this module's bot works in.
    async fn on_member_join(
        &self,
        _ctx: &Context,
        _state: &AppState,
        _project_id: i64,
        _member: &Member,
    ) -> Result<()> {
        Ok(())
    }

    /// Whether [`on_tick`](Self::on_tick) should be called. Off by default.
    fn ticks(&self) -> bool {
        false
    }

    /// Periodic work, called by the scheduler about once a minute for every project the
    /// module is enabled in.
    async fn on_tick(&self, _state: &AppState, _project_id: i64) -> Result<()> {
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
        Arc::new(discord::DiscordModule),
        Arc::new(clickup::ClickUpModule),
        Arc::new(greeting::GreetingModule),
        Arc::new(calendar::CalendarModule),
        Arc::new(recurring::RecurringModule),
    ]
}

pub async fn is_enabled(db: &SqlitePool, project_id: i64, module_id: &str) -> Result<bool> {
    let enabled: Option<bool> = sqlx::query_scalar(
        "SELECT enabled FROM project_modules WHERE project_id = ? AND module_id = ?",
    )
    .bind(project_id)
    .bind(module_id.to_owned())
    .fetch_optional(db)
    .await?;
    Ok(enabled.unwrap_or(false))
}

pub async fn set_enabled(
    db: impl SqliteExecutor<'_>,
    project_id: i64,
    module_id: &str,
    enabled: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO project_modules (project_id, module_id, enabled) VALUES (?, ?, ?)
         ON CONFLICT (project_id, module_id) DO UPDATE SET enabled = excluded.enabled",
    )
    .bind(project_id)
    .bind(module_id.to_owned())
    .bind(enabled)
    .execute(db)
    .await?;
    Ok(())
}

/// Projects a module is switched on in.
pub async fn enabled_projects(db: &SqlitePool, module_id: &str) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT m.project_id FROM project_modules m JOIN projects p ON p.id = m.project_id
         WHERE m.module_id = ? AND m.enabled = 1 ORDER BY m.project_id",
    )
    .bind(module_id.to_owned())
    .fetch_all(db)
    .await?)
}

pub async fn enabled_count(db: &SqlitePool, project_id: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM project_modules WHERE project_id = ? AND enabled = 1",
    )
    .bind(project_id)
    .fetch_one(db)
    .await?)
}

/// Increments an all-time counter for a module in a project.
pub async fn bump_counter(
    db: &SqlitePool,
    project_id: i64,
    module_id: &str,
    key: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO module_stats (project_id, module_id, key, value) VALUES (?, ?, ?, 1)
         ON CONFLICT (project_id, module_id, key) DO UPDATE SET value = value + 1",
    )
    .bind(project_id)
    .bind(module_id.to_owned())
    .bind(key.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

/// Reads a counter for one project, or the sum over all projects when `project_id` is None.
pub async fn counter(
    db: &SqlitePool,
    project_id: Option<i64>,
    module_id: &str,
    key: &str,
) -> Result<i64> {
    let value: Option<i64> =
        match project_id {
            Some(pid) => sqlx::query_scalar(
                "SELECT value FROM module_stats WHERE project_id = ? AND module_id = ? AND key = ?",
            )
            .bind(pid)
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
    use crate::projects;
    use crate::test_util;

    #[tokio::test]
    async fn modules_off_by_default_and_toggle_per_project() {
        let db = test_util::pool().await;
        let [a, b, c] = test_util::projects(&db, ["A", "B", "C"]).await;
        assert!(!is_enabled(&db, a, "greeting").await.unwrap());
        assert_eq!(enabled_count(&db, a).await.unwrap(), 0);

        set_enabled(&db, a, "greeting", true).await.unwrap();
        set_enabled(&db, a, "other", true).await.unwrap();
        assert!(is_enabled(&db, a, "greeting").await.unwrap());
        assert!(!is_enabled(&db, b, "greeting").await.unwrap());
        assert_eq!(enabled_count(&db, a).await.unwrap(), 2);

        set_enabled(&db, c, "greeting", true).await.unwrap();
        assert_eq!(enabled_projects(&db, "greeting").await.unwrap(), [a, c]);

        set_enabled(&db, a, "greeting", false).await.unwrap();
        assert!(!is_enabled(&db, a, "greeting").await.unwrap());
        assert_eq!(enabled_count(&db, a).await.unwrap(), 1);
        assert_eq!(enabled_projects(&db, "greeting").await.unwrap(), [c]);
        assert_eq!(enabled_projects(&db, "other").await.unwrap(), [a]);

        // Rows of a deleted project are never listed.
        sqlx::query("DELETE FROM projects WHERE id = ?")
            .bind(c)
            .execute(&db)
            .await
            .unwrap();
        assert!(enabled_projects(&db, "greeting").await.unwrap().is_empty());
        assert!(projects::get(&db, c).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn counters_per_project_and_total() {
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

    #[test]
    fn module_ids_are_unique_and_services_have_their_module() {
        let modules = registry();
        let mut ids: Vec<_> = modules.iter().map(|m| m.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), modules.len());
        for service in [Service::ClickUp, Service::Discord] {
            assert!(ids.contains(&service.shared_owner()));
        }
    }
}
