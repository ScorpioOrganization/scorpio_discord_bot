//! Projects: the top level container. Every module is switched on, configured and counted
//! per project, and every API key belongs to one.

use anyhow::Result;
use sqlx::SqlitePool;

use crate::db::{now, sf};

pub const MAX_PROJECTS: i64 = 100;
pub const MAX_NAME_LEN: usize = 60;

/// Every table with a `project_id` column. Deleting a project clears them all; a test checks
/// the list against the schema, so a new table cannot be forgotten.
pub const TABLES: &[&str] = &[
    "project_modules",
    "module_stats",
    "api_keys",
    "api_key_source",
    "discord_servers",
    "greeting_config",
    "greeting_log",
    "clickup_config",
    "clickup_user_map",
    "clickup_tag_roles",
    "calendar_config",
    // calendar_sent rows go with their rule (ON DELETE CASCADE)
    "calendar_rules",
    "calendar_log",
    "recurring_config",
    "recurring_rules",
    "recurring_done",
    "recurring_log",
];

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Project {
    pub id: i64,
    pub name: String,
}

/// Trimmed name, or None when empty or too long.
pub fn normalize_name(raw: &str) -> Option<String> {
    let name = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!name.is_empty() && name.chars().count() <= MAX_NAME_LEN).then_some(name)
}

pub async fn get(db: &SqlitePool, id: i64) -> Result<Option<Project>> {
    Ok(sqlx::query_as("SELECT id, name FROM projects WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?)
}

pub async fn list(db: &SqlitePool) -> Result<Vec<Project>> {
    Ok(
        sqlx::query_as("SELECT id, name FROM projects ORDER BY name COLLATE NOCASE, id")
            .fetch_all(db)
            .await?,
    )
}

pub async fn ids(db: &SqlitePool) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar("SELECT id FROM projects ORDER BY id")
        .fetch_all(db)
        .await?)
}

#[cfg(test)]
pub async fn count(db: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COUNT(*) FROM projects")
        .fetch_one(db)
        .await?)
}

/// Creates a project with an already normalized name. None when the limit is reached.
pub async fn create(db: &SqlitePool, name: &str, by: &str) -> Result<Option<i64>> {
    let res = sqlx::query(
        "INSERT INTO projects (name, created_at, updated_at, updated_by)
         SELECT ?, ?, ?, ? WHERE (SELECT COUNT(*) FROM projects) < ?",
    )
    .bind(name.to_owned())
    .bind(now())
    .bind(now())
    .bind(by.to_owned())
    .bind(MAX_PROJECTS)
    .execute(db)
    .await?;
    Ok((res.rows_affected() == 1).then(|| res.last_insert_rowid()))
}

pub async fn rename(db: &SqlitePool, id: i64, name: &str, by: &str) -> Result<()> {
    sqlx::query(
        "UPDATE projects SET name = ?, auto_name = 0, updated_at = ?, updated_by = ? WHERE id = ?",
    )
    .bind(name.to_owned())
    .bind(now())
    .bind(by.to_owned())
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Gives projects migrated from a Discord server that server's name, until someone renames
/// them.
pub async fn name_from_guild(db: &SqlitePool, guild_id: u64, name: &str) -> Result<()> {
    let Some(name) = normalize_name(name) else {
        return Ok(());
    };
    sqlx::query("UPDATE projects SET name = ? WHERE legacy_guild_id = ? AND auto_name = 1")
        .bind(name)
        .bind(sf(guild_id))
        .execute(db)
        .await?;
    Ok(())
}

/// The project migrated from a Discord server.
pub async fn by_legacy_guild(db: &SqlitePool, guild_id: u64) -> Result<Option<i64>> {
    Ok(
        sqlx::query_scalar("SELECT id FROM projects WHERE legacy_guild_id = ?")
            .bind(sf(guild_id))
            .fetch_optional(db)
            .await?,
    )
}

/// Deletes a project with all its settings, keys, logs and counters.
pub async fn delete(db: &SqlitePool, id: i64) -> Result<bool> {
    let mut tx = db.begin().await?;
    for table in TABLES {
        // Table names come from the constant list above.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE project_id = ?"
        )))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    let res = sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(res.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::test_util;

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(normalize_name("  Team   A \n").as_deref(), Some("Team A"));
        assert!(normalize_name(" \t ").is_none());
        assert!(normalize_name(&"x".repeat(MAX_NAME_LEN)).is_some());
        assert!(normalize_name(&"ż".repeat(MAX_NAME_LEN + 1)).is_none());
    }

    #[tokio::test]
    async fn create_rename_list_and_limit() {
        let db = test_util::pool().await;
        let a = create(&db, "Beta", "root").await.unwrap().unwrap();
        let b = create(&db, "alpha", "root").await.unwrap().unwrap();
        assert_ne!(a, b);
        let names: Vec<String> = list(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["alpha", "Beta"]);

        rename(&db, a, "Gamma", "alice").await.unwrap();
        assert_eq!(get(&db, a).await.unwrap().unwrap().name, "Gamma");
        assert!(get(&db, 999).await.unwrap().is_none());

        for i in 2..MAX_PROJECTS {
            create(&db, &format!("p{i}"), "root")
                .await
                .unwrap()
                .unwrap();
        }
        assert_eq!(count(&db).await.unwrap(), MAX_PROJECTS);
        assert!(create(&db, "one too many", "root").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn guild_names_only_replace_placeholders() {
        let db = test_util::pool().await;
        let guild = u64::MAX - 3;
        sqlx::query(
            "INSERT INTO projects (name, legacy_guild_id, auto_name, created_at, updated_at)
             VALUES ('Discord server x', ?, 1, 0, 0)",
        )
        .bind(sf(guild))
        .execute(&db)
        .await
        .unwrap();
        let id = by_legacy_guild(&db, guild).await.unwrap().unwrap();
        assert!(by_legacy_guild(&db, guild - 1).await.unwrap().is_none());

        name_from_guild(&db, guild, "  Scorpio  HQ ").await.unwrap();
        assert_eq!(get(&db, id).await.unwrap().unwrap().name, "Scorpio HQ");
        name_from_guild(&db, guild, "Renamed server").await.unwrap();
        assert_eq!(get(&db, id).await.unwrap().unwrap().name, "Renamed server");

        rename(&db, id, "Mine", "root").await.unwrap();
        name_from_guild(&db, guild, "Server again").await.unwrap();
        assert_eq!(get(&db, id).await.unwrap().unwrap().name, "Mine");
    }

    #[tokio::test]
    async fn table_list_matches_the_schema() {
        let db = test_util::pool().await;
        let in_schema: BTreeSet<String> = sqlx::query_scalar(
            "SELECT m.name FROM sqlite_master m JOIN pragma_table_info(m.name) c
             WHERE m.type = 'table' AND c.name = 'project_id'",
        )
        .fetch_all(&db)
        .await
        .unwrap()
        .into_iter()
        .collect();
        let listed: BTreeSet<String> = TABLES.iter().map(|t| t.to_string()).collect();
        assert_eq!(in_schema, listed);
    }

    #[tokio::test]
    async fn delete_removes_every_row_of_the_project() {
        let db = test_util::pool().await;
        let keep = create(&db, "Keep", "root").await.unwrap().unwrap();
        let gone = create(&db, "Gone", "root").await.unwrap().unwrap();
        for pid in [keep, gone] {
            test_util::fill_project(&db, pid).await;
        }
        let rows = |pid: i64| {
            let db = db.clone();
            async move {
                let mut total = 0;
                for table in TABLES {
                    let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                        "SELECT COUNT(*) FROM {table} WHERE project_id = ?"
                    )))
                    .bind(pid)
                    .fetch_one(&db)
                    .await
                    .unwrap();
                    assert!(n > 0, "{table} has no row for project {pid}");
                    total += n;
                }
                total
            }
        };
        let before = rows(keep).await;
        rows(gone).await;

        assert!(delete(&db, gone).await.unwrap());
        assert!(!delete(&db, gone).await.unwrap());
        assert!(get(&db, gone).await.unwrap().is_none());
        for table in TABLES {
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) FROM {table} WHERE project_id = ?"
            )))
            .bind(gone)
            .fetch_one(&db)
            .await
            .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        let sent: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM calendar_sent")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(sent, 1, "only the kept project's reminder marker is left");
        assert_eq!(rows(keep).await, before);
    }
}
