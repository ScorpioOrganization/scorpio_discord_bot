//! Data upgrades that SQL migrations cannot do, run on every start right after them: keys
//! sealed for a Discord server get sealed for their project, and the bot token from
//! `DISCORD_TOKEN` moves into the Discord module of the projects created from servers. Every
//! step is idempotent and only logs errors, so the dashboard always comes up.

use anyhow::Result;
use sqlx::SqlitePool;

use crate::db::now;
use crate::discord::connection::set_server;
use crate::keys::{self, Service};
use crate::modules::{self, discord::ID as DISCORD};
use crate::secrets::SecretString;
use crate::state::AppState;

/// `app_meta` key set once `DISCORD_TOKEN` was imported.
pub const DISCORD_IMPORTED: &str = "discord_env_imported";
/// Name of the project created for an imported token when no server had any settings.
pub const MAIN_PROJECT: &str = "Main";

pub async fn run(state: &AppState) {
    match keys::reseal_legacy(&state.db, &state.vault).await {
        Ok(0) => {}
        Ok(n) => tracing::info!("sealed {n} saved API keys for their project"),
        Err(e) => tracing::error!("could not upgrade saved API keys: {e:#}"),
    }
    match import_discord_token(state).await {
        Ok(Import::NoToken) => {}
        Ok(Import::AlreadyDone) => tracing::warn!(
            "DISCORD_TOKEN is set but no longer used: bot tokens are managed in the Discord \
             module of each project. Remove it from the environment."
        ),
        Ok(Import::Done(projects)) => tracing::warn!(
            "moved DISCORD_TOKEN into the Discord module of {projects} project(s). The token is \
             managed on the dashboard now; remove DISCORD_TOKEN from the environment."
        ),
        Err(e) => tracing::error!("could not import DISCORD_TOKEN, will retry on restart: {e:#}"),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Import {
    NoToken,
    AlreadyDone,
    /// Number of projects that received the token.
    Done(usize),
}

pub async fn meta(db: &SqlitePool, key: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT value FROM app_meta WHERE key = ?")
            .bind(key.to_owned())
            .fetch_optional(db)
            .await?,
    )
}

/// Stores `DISCORD_TOKEN` as the Discord module's token, with the server it came from and
/// the module enabled, in every project migrated from a server that has no token yet. With
/// no such project a project named [`MAIN_PROJECT`] holds it. Happens once: afterwards the
/// dashboard is the only place for bot tokens, so a removed token stays removed.
pub async fn import_discord_token(state: &AppState) -> Result<Import> {
    let Some(raw) = state.config.discord_token.as_deref() else {
        return Ok(Import::NoToken);
    };
    let db = &state.db;
    if meta(db, DISCORD_IMPORTED).await?.is_some() {
        return Ok(Import::AlreadyDone);
    }
    // The token worked before, so keep it even if it looks unusual.
    let token = Service::Discord
        .check_format(raw)
        .unwrap_or_else(|| SecretString::new(raw.trim()));

    let mut tx = db.begin().await?;
    let legacy: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT id, legacy_guild_id, name FROM projects WHERE legacy_guild_id IS NOT NULL
         AND NOT EXISTS (SELECT 1 FROM api_keys k
                         WHERE k.project_id = projects.id AND k.service = ? AND k.owner = ?)
         ORDER BY id",
    )
    .bind(Service::Discord.id())
    .bind(DISCORD)
    .fetch_all(&mut *tx)
    .await?;
    let any_legacy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM projects WHERE legacy_guild_id IS NOT NULL)",
    )
    .fetch_one(&mut *tx)
    .await?;

    let mut targets: Vec<(i64, Option<u64>)> = legacy
        .iter()
        .map(|(id, guild, _)| (*id, Some(*guild as u64)))
        .collect();
    if !any_legacy {
        let id = sqlx::query(
            "INSERT INTO projects (name, created_at, updated_at, updated_by)
             VALUES (?, ?, ?, 'DISCORD_TOKEN')",
        )
        .bind(MAIN_PROJECT)
        .bind(now())
        .bind(now())
        .execute(&mut *tx)
        .await?
        .last_insert_rowid();
        targets.push((id, None));
    }

    for (pid, guild) in &targets {
        // The bot name is filled in once it connects.
        keys::store(
            &mut *tx,
            &state.vault,
            *pid,
            Service::Discord,
            DISCORD,
            &token,
            "",
            "DISCORD_TOKEN",
        )
        .await?;
        if let Some(guild) = guild {
            let name = legacy
                .iter()
                .find(|(id, _, _)| id == pid)
                .map(|(_, _, name)| name.as_str())
                .unwrap_or_default();
            set_server(&mut *tx, *pid, DISCORD, *guild, name, "DISCORD_TOKEN").await?;
        }
        modules::set_enabled(&mut *tx, *pid, DISCORD, true).await?;
    }
    sqlx::query("INSERT INTO app_meta (key, value) VALUES (?, ?)")
        .bind(DISCORD_IMPORTED)
        .bind(now().to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Import::Done(targets.len()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use axum::http::StatusCode;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use super::*;
    use crate::db::MIGRATOR;
    use crate::discord::connection;
    use crate::keys::legacy_context;
    use crate::projects;
    use crate::secrets::Vault;
    use crate::test_util::{self, TestApp};

    const SEED: &str = include_str!("../ci/legacy_v6_seed.sql");
    const HQ: i64 = 1187654321098765432;
    /// Above i64::MAX as u64, stored as this bit pattern.
    const BIG: i64 = -5446744073709551615;
    const LEFT: i64 = 1187654321098765999;
    const CLICKUP_KEY: &str = "pk_1_LEGACYKEYVALUE";
    const RECURRING_KEY: &str = "pk_2_OWNRECURRINGKEY";

    /// Tables that had a `guild_id` column, with the name they have after the migration.
    const GUILD_TABLES: [(&str, &str); 16] = [
        ("guild_modules", "project_modules"),
        ("greeting_config", "greeting_config"),
        ("greeting_log", "greeting_log"),
        ("module_stats", "module_stats"),
        ("clickup_tokens", "api_keys"),
        ("clickup_token_source", "api_key_source"),
        ("clickup_config", "clickup_config"),
        ("clickup_user_map", "clickup_user_map"),
        ("clickup_tag_roles", "clickup_tag_roles"),
        ("calendar_config", "calendar_config"),
        ("calendar_rules", "calendar_rules"),
        ("calendar_log", "calendar_log"),
        ("recurring_config", "recurring_config"),
        ("recurring_rules", "recurring_rules"),
        ("recurring_done", "recurring_done"),
        ("recurring_log", "recurring_log"),
    ];

    async fn memory_pool() -> SqlitePool {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .foreign_keys(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await
            .unwrap()
    }

    /// Fills a database at version 6 with the shared fixture plus ClickUp keys sealed the
    /// old way, bound to their server.
    async fn seed_v6(db: &SqlitePool, vault: &Vault) {
        sqlx::raw_sql(SEED).execute(db).await.unwrap();
        for (guild, owner, key) in [
            (HQ, "clickup", CLICKUP_KEY),
            (BIG, "recurring", RECURRING_KEY),
        ] {
            sqlx::query(
                "INSERT INTO clickup_tokens (guild_id, owner, sealed, hint, clickup_user,
                                             last_ok_at, updated_at, updated_by)
                 VALUES (?, ?, ?, ?, 'Test Owner', 1760000000, 1760000000, 'root')",
            )
            .bind(guild)
            .bind(owner)
            .bind(vault.seal(key, &legacy_context(guild as u64, owner)))
            .bind(keys::hint(key))
            .execute(db)
            .await
            .unwrap();
        }
    }

    /// Rows per server of every guild keyed table.
    async fn rows_per_guild(db: &SqlitePool, migrated: bool) -> BTreeMap<String, Vec<(i64, i64)>> {
        let mut out = BTreeMap::new();
        for (old, new) in GUILD_TABLES {
            let sql = if migrated {
                format!(
                    "SELECT p.legacy_guild_id, COUNT(*) FROM {new} t
                     JOIN projects p ON p.id = t.project_id
                     GROUP BY p.legacy_guild_id ORDER BY 1"
                )
            } else {
                format!("SELECT guild_id, COUNT(*) FROM {old} GROUP BY guild_id ORDER BY 1")
            };
            let rows: Vec<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                .fetch_all(db)
                .await
                .unwrap();
            out.insert(old.to_owned(), rows);
        }
        out
    }

    async fn project_of(db: &SqlitePool, guild: i64) -> i64 {
        projects::by_legacy_guild(db, guild as u64)
            .await
            .unwrap()
            .unwrap()
    }

    async fn migrated_state(token: Option<&str>) -> AppState {
        let db = memory_pool().await;
        MIGRATOR.run_to(6, &db).await.unwrap();
        let vault = Vault::ephemeral();
        seed_v6(&db, &vault).await;
        MIGRATOR.run(&db).await.unwrap();
        let mut config = test_util::config();
        config.discord_token = token.map(str::to_owned);
        AppState::new(db, config, modules::registry(), vault)
    }

    #[tokio::test]
    async fn every_row_moves_to_the_project_of_its_server() {
        let db = memory_pool().await;
        MIGRATOR.run_to(6, &db).await.unwrap();
        let vault = Vault::ephemeral();
        seed_v6(&db, &vault).await;
        let before = rows_per_guild(&db, false).await;
        assert!(
            before.values().all(|rows| !rows.is_empty()),
            "fixture covers every table"
        );
        let sent_before: Vec<(i64, String, i64, i64)> = sqlx::query_as(
            "SELECT rule_id, task_id, event_at, sent_at FROM calendar_sent ORDER BY 2",
        )
        .fetch_all(&db)
        .await
        .unwrap();

        MIGRATOR.run(&db).await.unwrap();

        // One project per server, in id order, with a placeholder name.
        let list: Vec<(i64, Option<i64>, String, bool)> =
            sqlx::query_as("SELECT id, legacy_guild_id, name, auto_name FROM projects ORDER BY id")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(
            list.iter().map(|p| p.1).collect::<Vec<_>>(),
            [Some(BIG), Some(HQ), Some(LEFT)]
        );
        assert!(
            list.iter()
                .all(|p| p.3 && p.2.starts_with("Discord server "))
        );

        assert_eq!(rows_per_guild(&db, true).await, before);
        let sent_after: Vec<(i64, String, i64, i64)> = sqlx::query_as(
            "SELECT rule_id, task_id, event_at, sent_at FROM calendar_sent ORDER BY 2",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(sent_after, sent_before, "reminders are not sent twice");

        // Spot checks of moved values.
        let hq = project_of(&db, HQ).await;
        let big = project_of(&db, BIG).await;
        let (template, channel): (String, Option<i64>) =
            sqlx::query_as("SELECT template, channel_id FROM greeting_config WHERE project_id = ?")
                .bind(hq)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(template, "Welcome {user} to {server}!");
        assert_eq!(channel, Some(1187654321098765433));
        assert!(modules::is_enabled(&db, hq, "calendar").await.unwrap());
        assert!(!modules::is_enabled(&db, big, "greeting").await.unwrap());
        assert!(modules::is_enabled(&db, big, "recurring").await.unwrap());
        assert_eq!(
            modules::counter(&db, Some(hq), "greeting", "sent")
                .await
                .unwrap(),
            41
        );
        let offset: i64 = sqlx::query_scalar(
            "SELECT offset_minutes FROM calendar_rules WHERE project_id = ? AND kind = 'digest'",
        )
        .bind(hq)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(offset, 240);
        assert!(
            !keys::use_shared(&db, big, Service::ClickUp, "recurring")
                .await
                .unwrap()
        );
        assert!(
            keys::use_shared(&db, hq, Service::ClickUp, "calendar")
                .await
                .unwrap()
        );

        // Keys moved, still bound to their server until resealed.
        let legacy: Vec<(i64, String, Option<i64>, String)> = sqlx::query_as(
            "SELECT project_id, owner, legacy_guild_id, account FROM api_keys ORDER BY project_id",
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            legacy,
            [
                (
                    big,
                    "recurring".to_owned(),
                    Some(BIG),
                    "Test Owner".to_owned()
                ),
                (hq, "clickup".to_owned(), Some(HQ), "Test Owner".to_owned()),
            ]
        );

        // Old tables and indexes are gone, the schema is consistent.
        let names: Vec<String> = sqlx::query_scalar("SELECT name FROM sqlite_master ORDER BY name")
            .fetch_all(&db)
            .await
            .unwrap();
        for gone in ["guild_modules", "clickup_tokens", "clickup_token_source"] {
            assert!(!names.contains(&gone.to_owned()), "{gone}");
        }
        assert!(!names.iter().any(|n| n.ends_with("_guild")), "{names:?}");
        for index in [
            "idx_greeting_log_project",
            "idx_calendar_rules_project",
            "idx_calendar_log_project",
            "idx_recurring_log_project",
        ] {
            assert!(names.contains(&index.to_owned()), "{index}");
        }
        let no_guild_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master m JOIN pragma_table_info(m.name) c
             WHERE m.type = 'table' AND m.name != 'discord_servers' AND c.name = 'guild_id'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(no_guild_columns, 0);
        let fk_problems: Vec<(String,)> = sqlx::query_as("PRAGMA foreign_key_check")
            .fetch_all(&db)
            .await
            .unwrap();
        assert!(fk_problems.is_empty(), "{fk_problems:?}");
        let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(integrity, "ok");

        // The project table list matches what was migrated.
        for (_, new) in GUILD_TABLES {
            assert!(projects::TABLES.contains(&new), "{new}");
        }
    }

    #[tokio::test]
    async fn empty_databases_migrate() {
        // A fresh install.
        let db = test_util::pool().await;
        assert_eq!(projects::count(&db).await.unwrap(), 0);

        // Version 6 without any rows.
        let db = memory_pool().await;
        MIGRATOR.run_to(6, &db).await.unwrap();
        MIGRATOR.run(&db).await.unwrap();
        assert_eq!(projects::count(&db).await.unwrap(), 0);
        let id = projects::create(&db, "New", "root").await.unwrap().unwrap();
        assert_eq!(id, 1);
    }

    #[tokio::test]
    async fn legacy_keys_are_resealed_once() {
        let state = migrated_state(None).await;
        let db = &state.db;
        let hq = project_of(db, HQ).await;
        let big = project_of(db, BIG).await;
        let load = |pid: i64, owner: &'static str| {
            let state = state.clone();
            async move {
                keys::load(&state.db, &state.vault, pid, Service::ClickUp, owner)
                    .await
                    .unwrap()
                    .unwrap()
                    .expose()
                    .to_owned()
            }
        };
        // Readable before the upgrade step ran.
        assert_eq!(load(hq, "clickup").await, CLICKUP_KEY);

        run(&state).await;
        assert_eq!(load(hq, "clickup").await, CLICKUP_KEY);
        assert_eq!(load(big, "recurring").await, RECURRING_KEY);
        let left: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE legacy_guild_id IS NOT NULL")
                .fetch_one(db)
                .await
                .unwrap();
        assert_eq!(left, 0);
        assert_eq!(keys::reseal_legacy(db, &state.vault).await.unwrap(), 0);

        // ClickUp keeps working through the resolved credential.
        let cred = keys::resolve(&state, big, Service::ClickUp, "recurring")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cred.token.expose(), RECURRING_KEY);
    }

    #[tokio::test]
    async fn discord_token_moves_into_every_server_project_once() {
        let state = migrated_state(Some(test_util::DISCORD_TOKEN)).await;
        let db = &state.db;
        run(&state).await;
        assert!(meta(db, DISCORD_IMPORTED).await.unwrap().is_some());

        for guild in [HQ, BIG, LEFT] {
            let pid = project_of(db, guild).await;
            assert!(
                modules::is_enabled(db, pid, DISCORD).await.unwrap(),
                "{guild}"
            );
            let b = connection::binding(&state, pid, DISCORD)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                b.key,
                crate::discord::client_key(&SecretString::new(test_util::DISCORD_TOKEN))
            );
            assert_eq!(b.guild_id, Some(guild as u64));
            let info = keys::info(db, pid, Service::Discord, DISCORD)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(info.account, "");
            assert_eq!(info.updated_by.as_deref(), Some("DISCORD_TOKEN"));
        }

        // Modules on the shared bot use it right away, like before the upgrade.
        let hq = project_of(db, HQ).await;
        let greeting = connection::binding(&state, hq, "greeting")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(greeting.guild_id, Some(HQ as u64));
        let wanted = crate::discord::hub::desired(&state).await.unwrap();
        assert_eq!(wanted.len(), 1, "one bot for every project");

        // A removed token stays removed.
        keys::delete(db, hq, Service::Discord, DISCORD)
            .await
            .unwrap();
        assert_eq!(
            import_discord_token(&state).await.unwrap(),
            Import::AlreadyDone
        );
        assert!(
            keys::info(db, hq, Service::Discord, DISCORD)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(projects::count(db).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn token_added_later_fills_only_projects_without_one() {
        let mut state = migrated_state(None).await;
        run(&state).await;
        let db = state.db.clone();
        assert!(meta(&db, DISCORD_IMPORTED).await.unwrap().is_none());

        // Someone set a token by hand in one project meanwhile.
        let hq = project_of(&db, HQ).await;
        keys::store(
            &db,
            &state.vault,
            hq,
            Service::Discord,
            DISCORD,
            &SecretString::new(test_util::OTHER_DISCORD_TOKEN),
            "Other",
            "root",
        )
        .await
        .unwrap();

        let mut config = (*state.config).clone();
        config.discord_token = Some(format!("Bot {}\n", test_util::DISCORD_TOKEN));
        state.config = std::sync::Arc::new(config);
        assert_eq!(import_discord_token(&state).await.unwrap(), Import::Done(2));
        let token_of = |pid: i64| {
            let state = state.clone();
            async move {
                keys::load(&state.db, &state.vault, pid, Service::Discord, DISCORD)
                    .await
                    .unwrap()
                    .unwrap()
                    .expose()
                    .to_owned()
            }
        };
        assert_eq!(token_of(hq).await, test_util::OTHER_DISCORD_TOKEN);
        assert_eq!(
            token_of(project_of(&db, BIG).await).await,
            test_util::DISCORD_TOKEN
        );
    }

    #[tokio::test]
    async fn token_without_server_projects_gets_a_main_project() {
        let mut state = test_util::state().await;
        let mut config = test_util::config();
        config.discord_token = Some(test_util::DISCORD_TOKEN.into());
        state.config = std::sync::Arc::new(config);
        assert_eq!(import_discord_token(&state).await.unwrap(), Import::Done(1));
        let list = projects::list(&state.db).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, MAIN_PROJECT);
        let b = connection::binding(&state, list[0].id, DISCORD)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(b.guild_id, None);
        assert!(
            modules::is_enabled(&state.db, list[0].id, DISCORD)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn upgraded_file_database_serves_the_old_settings() {
        let dir = std::env::temp_dir().join(format!(
            "scorpio-test-upgrade-{}-{}",
            std::process::id(),
            crate::auth::random_token()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let url = format!("sqlite://{}/scorpio.db?mode=rwc", dir.display());
        let mut config = test_util::config();
        config.database_url = url.clone();
        config.secrets_key = Some("ab".repeat(32));
        config.discord_token = Some(test_util::DISCORD_TOKEN.into());

        // Version 6 as the previous release left it.
        {
            let db = SqlitePool::connect(&url).await.unwrap();
            MIGRATOR.run_to(6, &db).await.unwrap();
            seed_v6(&db, &Vault::load(&config).unwrap()).await;
            db.close().await;
        }

        // The same steps as `main`.
        let db = crate::db::connect(&url).await.unwrap();
        let state = crate::prepare(config, db).await.unwrap();
        assert!(dir.join("scorpio.db.pre-v7.bak").exists());
        let hq = project_of(&state.db, HQ).await;

        let app = TestApp::from_state(state).await;
        // The fake bot only knows the test server.
        test_util::connect_fake_discord(&app.state, hq).await;
        let cookie = app.login_root().await;

        let page = app.get(&format!("/projects/{hq}"), Some(&cookie)).await;
        assert_eq!(page.status, StatusCode::OK);
        for module in [
            "Discord",
            "Greeting",
            "ClickUp",
            "Calendar",
            "Recurring tasks",
        ] {
            assert!(page.body.contains(module), "{module}");
        }
        let greeting = app
            .get(&format!("/projects/{hq}/modules/greeting"), Some(&cookie))
            .await;
        assert_eq!(greeting.status, StatusCode::OK);
        assert!(greeting.body.contains("Welcome {user} to {server}!"));

        let calendar = app
            .get(&format!("/projects/{hq}/modules/calendar"), Some(&cookie))
            .await;
        assert_eq!(calendar.status, StatusCode::OK);
        assert!(calendar.body.contains("Daily digest"));

        // Old links lead to the project.
        let res = app
            .get(&format!("/guilds/{HQ}/modules/calendar"), Some(&cookie))
            .await;
        assert_eq!(res.redirect(), format!("/projects/{hq}/modules/calendar"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
