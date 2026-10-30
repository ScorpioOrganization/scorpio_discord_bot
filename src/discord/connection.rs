//! Which bot and server a module uses in a project. A Discord key (bot token) is saved with
//! the server it works in; modules use the Discord module's pair unless they have their own.

use std::sync::Arc;

use anyhow::Result;
use serenity::all::GuildId;
use sqlx::{SqliteExecutor, SqlitePool};

use super::client_key;
use super::hub::{DiscordHandle, Status};
use super::info::{self, GuildSummary};
use crate::db::{now, sf};
use crate::keys::{self, KeyProblem, Service};
use crate::modules::{self, Module};
use crate::projects;
use crate::state::AppState;

/// The bot token and server a module resolves to, whether or not the bot is connected.
#[derive(Debug, Clone)]
pub struct Binding {
    /// [`client_key`] of the token.
    pub key: String,
    pub guild_id: Option<u64>,
}

/// A connected bot and the server a module works in.
#[derive(Clone)]
pub struct Conn {
    pub handle: DiscordHandle,
    pub guild_id: u64,
}

impl Conn {
    pub fn guild(&self) -> Option<GuildSummary> {
        info::guild(&self.handle.cache, self.guild_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscordProblem {
    Key(KeyProblem),
    NoServer,
    /// The bot is not connected; the error says why, when known.
    Offline(Option<String>),
    Connecting,
    NotInServer,
}

impl DiscordProblem {
    pub fn message(&self) -> String {
        match self {
            DiscordProblem::Key(p) => p.message(Service::Discord).into(),
            DiscordProblem::NoServer => "No Discord server is selected for this bot.".into(),
            DiscordProblem::Offline(None) => "The Discord bot is not connected.".into(),
            DiscordProblem::Offline(Some(e)) => format!("The Discord bot is not connected. {e}"),
            DiscordProblem::Connecting => {
                "The Discord bot is connecting. Reload in a few seconds.".into()
            }
            DiscordProblem::NotInServer => "The bot is not a member of the selected Discord \
                                            server. Invite it, or pick another server."
                .into(),
        }
    }
}

/// The server saved for a key, as (id, name).
pub async fn server(
    db: &SqlitePool,
    project_id: i64,
    owner: &str,
) -> Result<Option<(u64, String)>> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT guild_id, guild_name FROM discord_servers WHERE project_id = ? AND owner = ?",
    )
    .bind(project_id)
    .bind(owner.to_owned())
    .fetch_optional(db)
    .await?;
    Ok(row.map(|(id, name)| (id as u64, name)))
}

pub async fn set_server(
    db: impl SqliteExecutor<'_>,
    project_id: i64,
    owner: &str,
    guild_id: u64,
    guild_name: &str,
    by: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO discord_servers (project_id, owner, guild_id, guild_name, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (project_id, owner) DO UPDATE SET
             guild_id = excluded.guild_id,
             guild_name = excluded.guild_name,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by",
    )
    .bind(project_id)
    .bind(owner.to_owned())
    .bind(sf(guild_id))
    .bind(guild_name.to_owned())
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

/// Keeps stored server names current when the bot sees a server.
pub async fn name_server(db: &SqlitePool, guild_id: u64, name: &str) -> Result<()> {
    sqlx::query("UPDATE discord_servers SET guild_name = ? WHERE guild_id = ? AND guild_name != ?")
        .bind(name.to_owned())
        .bind(sf(guild_id))
        .bind(name.to_owned())
        .execute(db)
        .await?;
    Ok(())
}

/// Names of other projects with a key working in `guild_id`.
pub async fn other_projects_on(
    db: &SqlitePool,
    guild_id: u64,
    project_id: i64,
) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT p.name FROM discord_servers s JOIN projects p ON p.id = s.project_id
         WHERE s.guild_id = ? AND s.project_id != ? ORDER BY p.name COLLATE NOCASE",
    )
    .bind(sf(guild_id))
    .bind(project_id)
    .fetch_all(db)
    .await?)
}

/// Fills in the account name of saved tokens that belong to the bot with this key and do not
/// have one yet, like the token imported from `DISCORD_TOKEN`.
pub async fn name_bot(state: &AppState, key: &str, name: &str) -> Result<()> {
    let unnamed: Vec<(i64, String)> =
        sqlx::query_as("SELECT project_id, owner FROM api_keys WHERE service = ? AND account = ''")
            .bind(Service::Discord.id())
            .fetch_all(&state.db)
            .await?;
    for (pid, owner) in unnamed {
        if let Ok(token) =
            keys::load(&state.db, &state.vault, pid, Service::Discord, &owner).await?
            && client_key(&token) == key
        {
            keys::set_account(&state.db, pid, Service::Discord, &owner, name).await?;
        }
    }
    Ok(())
}

/// The token and server `module_id` uses in a project, see [`keys::resolve`].
pub async fn binding(
    state: &AppState,
    project_id: i64,
    module_id: &str,
) -> Result<Result<Binding, DiscordProblem>> {
    let cred = match keys::resolve(state, project_id, Service::Discord, module_id).await? {
        Ok(c) => c,
        Err(p) => return Ok(Err(DiscordProblem::Key(p))),
    };
    let guild_id = server(&state.db, project_id, &cred.owner)
        .await?
        .map(|(id, _)| id);
    Ok(Ok(Binding {
        key: client_key(&cred.token),
        guild_id,
    }))
}

/// The connected bot and server `module_id` uses in a project.
pub async fn connection(
    state: &AppState,
    project_id: i64,
    module_id: &str,
) -> Result<Result<Conn, DiscordProblem>> {
    let binding = match binding(state, project_id, module_id).await? {
        Ok(b) => b,
        Err(p) => return Ok(Err(p)),
    };
    Ok(connect(state, &binding))
}

/// The running client and server of a binding.
pub fn connect(state: &AppState, binding: &Binding) -> Result<Conn, DiscordProblem> {
    let Some(guild_id) = binding.guild_id else {
        return Err(DiscordProblem::NoServer);
    };
    let handle = match state.discord.status(&binding.key) {
        None => return Err(DiscordProblem::Offline(None)),
        Some(Status::Failed { error, .. }) => return Err(DiscordProblem::Offline(Some(error))),
        Some(Status::Running(h)) => h,
    };
    if handle.cache.guild(GuildId::new(guild_id.max(1))).is_none() {
        return Err(if handle.is_ready() {
            DiscordProblem::NotInServer
        } else {
            DiscordProblem::Connecting
        });
    }
    Ok(Conn { handle, guild_id })
}

/// Enabled modules, per project, that receive events of the bot with `key` in `guild_id`.
pub async fn subscribers(
    state: &AppState,
    key: &str,
    guild_id: u64,
) -> Result<Vec<(i64, Arc<dyn Module>)>> {
    let mut out = Vec::new();
    for pid in projects::ids(&state.db).await? {
        for module in state.modules.iter() {
            if !module.services().contains(&Service::Discord)
                || !modules::is_enabled(&state.db, pid, module.id()).await?
            {
                continue;
            }
            if let Ok(b) = binding(state, pid, module.id()).await?
                && b.key == key
                && b.guild_id == Some(guild_id)
            {
                out.push((pid, module.clone()));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::hub::DiscordHandle;
    use crate::modules::set_enabled;
    use crate::secrets::SecretString;
    use crate::test_util::{self, DISCORD_TOKEN, GUILD_ID, OTHER_DISCORD_TOKEN};

    async fn project(state: &AppState, name: &str) -> i64 {
        projects::create(&state.db, name, "root")
            .await
            .unwrap()
            .unwrap()
    }

    async fn token(state: &AppState, pid: i64, owner: &str, token: &str) {
        keys::store(
            &state.db,
            &state.vault,
            pid,
            Service::Discord,
            owner,
            &SecretString::new(token),
            "",
            "root",
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connection_explains_what_is_missing() {
        let state = test_util::state().await;
        let pid = project(&state, "A").await;
        let problem = |state: AppState| async move {
            connection(&state, pid, "greeting").await.unwrap().err()
        };
        assert_eq!(
            problem(state.clone()).await,
            Some(DiscordProblem::Key(KeyProblem::SharedModuleDisabled))
        );
        set_enabled(&state.db, pid, "discord", true).await.unwrap();
        assert_eq!(
            problem(state.clone()).await,
            Some(DiscordProblem::Key(KeyProblem::NotSet))
        );
        token(&state, pid, "discord", DISCORD_TOKEN).await;
        assert_eq!(problem(state.clone()).await, Some(DiscordProblem::NoServer));
        set_server(&state.db, pid, "discord", GUILD_ID, "Test", "root")
            .await
            .unwrap();
        assert_eq!(
            problem(state.clone()).await,
            Some(DiscordProblem::Offline(None))
        );

        // A connected bot that is not in the selected server.
        test_util::connect_fake_discord(&state, pid).await;
        set_server(&state.db, pid, "discord", GUILD_ID + 1, "Gone", "root")
            .await
            .unwrap();
        assert_eq!(
            problem(state.clone()).await,
            Some(DiscordProblem::NotInServer)
        );

        set_server(&state.db, pid, "discord", GUILD_ID, "Test", "root")
            .await
            .unwrap();
        let conn = connection(&state, pid, "greeting").await.unwrap().unwrap();
        assert_eq!(conn.guild_id, GUILD_ID);
        assert_eq!(conn.guild().unwrap().name, test_util::GUILD_NAME);
    }

    #[tokio::test]
    async fn connecting_bots_are_not_reported_missing_from_servers() {
        let state = test_util::state().await;
        let pid = project(&state, "A").await;
        test_util::connect_fake_discord(&state, pid).await;
        let key = client_key(&SecretString::new(DISCORD_TOKEN));
        let h = state.discord.handle(&key).unwrap();
        let fresh = DiscordHandle::new(
            h.http.clone(),
            Arc::new(serenity::all::Cache::new()),
            h.shard_manager.clone(),
            h.intents,
        );
        state.discord.insert_running(&key, fresh);
        assert_eq!(
            connection(&state, pid, "greeting").await.unwrap().err(),
            Some(DiscordProblem::Connecting)
        );
        state.discord.mark_ready(&key);
        assert_eq!(
            connection(&state, pid, "greeting").await.unwrap().err(),
            Some(DiscordProblem::NotInServer)
        );
    }

    #[tokio::test]
    async fn events_reach_enabled_modules_of_the_right_bot_and_server() {
        let state = test_util::state().await;
        let db = &state.db;
        let a = project(&state, "A").await;
        let b = project(&state, "B").await;
        let c = project(&state, "C").await;
        let shared = client_key(&SecretString::new(DISCORD_TOKEN));
        let other = client_key(&SecretString::new(OTHER_DISCORD_TOKEN));
        for pid in [a, b, c] {
            set_enabled(db, pid, "discord", true).await.unwrap();
            token(&state, pid, "discord", DISCORD_TOKEN).await;
        }
        // A and B share a server; C is elsewhere.
        set_server(db, a, "discord", GUILD_ID, "S", "root")
            .await
            .unwrap();
        set_server(db, b, "discord", GUILD_ID, "S", "root")
            .await
            .unwrap();
        set_server(db, c, "discord", GUILD_ID + 1, "T", "root")
            .await
            .unwrap();
        set_enabled(db, a, "greeting", true).await.unwrap();
        set_enabled(db, c, "greeting", true).await.unwrap();
        set_enabled(db, b, "calendar", true).await.unwrap();

        let ids = |state: AppState, key: String, guild: u64| async move {
            subscribers(&state, &key, guild)
                .await
                .unwrap()
                .into_iter()
                .map(|(pid, m)| (pid, m.id()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(state.clone(), shared.clone(), GUILD_ID).await,
            [(a, "greeting"), (b, "calendar")]
        );
        assert_eq!(
            ids(state.clone(), shared.clone(), GUILD_ID + 1).await,
            [(c, "greeting")]
        );
        assert!(ids(state.clone(), other.clone(), GUILD_ID).await.is_empty());

        // With its own bot, A's greeting only hears that bot.
        keys::set_use_shared(db, a, Service::Discord, "greeting", false)
            .await
            .unwrap();
        token(&state, a, "greeting", OTHER_DISCORD_TOKEN).await;
        set_server(db, a, "greeting", GUILD_ID, "S", "root")
            .await
            .unwrap();
        assert_eq!(
            ids(state.clone(), shared.clone(), GUILD_ID).await,
            [(b, "calendar")]
        );
        assert_eq!(ids(state.clone(), other, GUILD_ID).await, [(a, "greeting")]);

        // Disabled modules hear nothing.
        set_enabled(db, b, "calendar", false).await.unwrap();
        assert!(ids(state.clone(), shared, GUILD_ID).await.is_empty());
    }

    #[tokio::test]
    async fn bot_and_server_names_are_filled_in() {
        let state = test_util::state().await;
        let db = &state.db;
        let a = project(&state, "A").await;
        token(&state, a, "discord", DISCORD_TOKEN).await;
        token(&state, a, "greeting", OTHER_DISCORD_TOKEN).await;
        name_bot(
            &state,
            &client_key(&SecretString::new(DISCORD_TOKEN)),
            "Scorpio",
        )
        .await
        .unwrap();
        let account = |owner: &'static str| {
            let db = db.clone();
            async move {
                keys::info(&db, a, Service::Discord, owner)
                    .await
                    .unwrap()
                    .unwrap()
                    .account
            }
        };
        assert_eq!(account("discord").await, "Scorpio");
        assert_eq!(account("greeting").await, "");

        set_server(db, a, "discord", GUILD_ID, "Old name", "root")
            .await
            .unwrap();
        name_server(db, GUILD_ID, "New name").await.unwrap();
        assert_eq!(
            server(db, a, "discord").await.unwrap(),
            Some((GUILD_ID, "New name".to_owned()))
        );

        let b = project(&state, "B").await;
        set_server(db, b, "discord", GUILD_ID, "New name", "root")
            .await
            .unwrap();
        assert_eq!(other_projects_on(db, GUILD_ID, a).await.unwrap(), ["B"]);
        assert!(
            other_projects_on(db, GUILD_ID + 1, a)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
