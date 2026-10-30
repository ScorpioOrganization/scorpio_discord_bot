use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

pub async fn connect(url: &str) -> Result<SqlitePool> {
    ensure_parent_dir(url);

    let options = SqliteConnectOptions::from_str(url)
        .with_context(|| format!("invalid DATABASE_URL: {url}"))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .context("failed to open database")?;

    if let Some(path) = sqlite_path(url) {
        match backup_before_migrations(&pool, &path).await {
            Ok(Some(backup)) => tracing::warn!(
                "database copied to {} before migrating it. To roll back, run the previous \
                 version with this copy in place of the database.",
                backup.display()
            ),
            Ok(None) => {}
            // Migrations run in a transaction and leave the database as it was if they fail,
            // so a missing copy is no reason to keep the dashboard down.
            Err(e) => tracing::error!("could not copy the database before migrating it: {e:#}"),
        }
    }
    MIGRATOR
        .run(&pool)
        .await
        .context("failed to run migrations")?;

    Ok(pool)
}

/// Copies the database file next to itself (`<file>.pre-v<N>.bak`, N the newest migration)
/// when migrations are about to change an existing database. Returns the copy's path, or
/// None when nothing is pending or the database is new.
async fn backup_before_migrations(pool: &SqlitePool, db_path: &Path) -> Result<Option<PathBuf>> {
    let tracked: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await?;
    if tracked == 0 {
        return Ok(None);
    }
    let applied: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = 1")
            .fetch_all(pool)
            .await?;
    let pending: Vec<i64> = MIGRATOR
        .iter()
        .map(|m| m.version)
        .filter(|v| !applied.contains(v))
        .collect();
    let Some(newest) = pending.iter().max() else {
        return Ok(None);
    };

    let file_name = db_path
        .file_name()
        .context("database path has no file name")?
        .to_string_lossy();
    let backup = db_path.with_file_name(format!("{file_name}.pre-v{newest}.bak"));
    if backup.exists() {
        // An earlier start already copied the database and then failed to migrate it.
        return Ok(Some(backup));
    }
    let target = backup
        .to_str()
        .context("database path is not valid UTF-8")?
        .to_owned();
    if let Err(e) = sqlx::query("VACUUM INTO ?")
        .bind(target)
        .execute(pool)
        .await
    {
        let _ = std::fs::remove_file(&backup);
        return Err(e.into());
    }
    Ok(Some(backup))
}

/// Creates the directory holding the sqlite file so first start works out of the box.
fn ensure_parent_dir(url: &str) {
    if let Some(parent) = data_dir(url) {
        let _ = std::fs::create_dir_all(parent);
    }
}

/// Path of the sqlite file in a `DATABASE_URL`, or None for an in-memory database.
pub fn sqlite_path(url: &str) -> Option<PathBuf> {
    let path = url
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:")
        .split('?')
        .next()
        .unwrap_or_default();
    if path.is_empty() || path == ":memory:" {
        return None;
    }
    Some(PathBuf::from(path))
}

/// Directory holding the sqlite file, where other data files (like the secrets key) live too.
/// `.` when the file has no directory part, None for an in-memory database.
pub fn data_dir(url: &str) -> Option<PathBuf> {
    let path = sqlite_path(url)?;
    Some(match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    })
}

/// Current unix time in seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Discord snowflakes are u64, sqlite integers are i64. The bit pattern is kept as is.
pub fn sf(id: u64) -> i64 {
    id as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!(
            "scorpio-test-{name}-{}-{}",
            std::process::id(),
            crate::auth::random_token()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}/scorpio.db?mode=rwc", dir.display());
        (dir, url)
    }

    async fn migrate_to(url: &str, version: i64) {
        let pool = SqlitePool::connect(url).await.unwrap();
        MIGRATOR.run_to(version, &pool).await.unwrap();
        pool.close().await;
    }

    #[tokio::test]
    async fn pending_migrations_back_up_the_database_first() {
        let (dir, url) = temp_db("backup");
        migrate_to(&url, 6).await;
        let newest = MIGRATOR.iter().map(|m| m.version).max().unwrap();
        let backup = dir.join(format!("scorpio.db.pre-v{newest}.bak"));

        let pool = connect(&url).await.unwrap();
        pool.close().await;
        let copy = SqlitePool::connect(&format!("sqlite://{}", backup.display()))
            .await
            .unwrap();
        let version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&copy)
            .await
            .unwrap();
        assert_eq!(version, 6, "the copy holds the database before migrating");
        let guild_table: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'guild_modules'")
                .fetch_one(&copy)
                .await
                .unwrap();
        assert_eq!(guild_table, 1);
        copy.close().await;

        // Nothing pending on the next start: no new copy, the old one is left alone.
        std::fs::remove_file(&backup).unwrap();
        let pool = connect(&url).await.unwrap();
        pool.close().await;
        assert!(!backup.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn new_databases_are_not_backed_up() {
        let (dir, url) = temp_db("fresh");
        let pool = connect(&url).await.unwrap();
        pool.close().await;
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".bak"))
            .collect();
        assert!(files.is_empty(), "{files:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn paths_from_database_urls() {
        let dir = |url: &str| data_dir(url).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(
            dir("sqlite://data/scorpio.db?mode=rwc").as_deref(),
            Some("data")
        );
        assert_eq!(dir("sqlite:///data/scorpio.db").as_deref(), Some("/data"));
        assert_eq!(dir("sqlite:scorpio.db").as_deref(), Some("."));
        assert_eq!(dir("sqlite::memory:"), None);
        assert_eq!(dir("sqlite://:memory:"), None);
    }
}
