use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

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

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("failed to run migrations")?;

    Ok(pool)
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
