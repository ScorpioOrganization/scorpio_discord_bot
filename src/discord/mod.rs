//! Discord integration shared by every module that talks to Discord: the running bot
//! connections ([`hub`]), which bot and server a module uses ([`connection`]) and read only
//! views of a server for the dashboard ([`info`], [`members`]). Bot tokens are keys of
//! [`Service::Discord`](crate::keys::Service) in [`crate::keys`].

pub mod connection;
pub mod hub;
pub mod info;
pub mod members;
#[cfg(test)]
pub mod mock;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serenity::all::{
    ApplicationFlags, Client, ClientBuilder, Context, EventHandler, GatewayError, GatewayIntents,
    Guild, Http, HttpBuilder, Member, Ready,
};
use serenity::async_trait;

use self::hub::DiscordHandle;
use crate::modules::Module;
use crate::projects;
use crate::secrets::SecretString;
use crate::state::AppState;

/// Privileged intents, the application flags Discord sets when they are switched on, and their
/// name on the Bot tab of the developer portal.
const PRIVILEGED: [(GatewayIntents, ApplicationFlags, &str); 3] = [
    (
        GatewayIntents::GUILD_MEMBERS,
        ApplicationFlags::GATEWAY_GUILD_MEMBERS
            .union(ApplicationFlags::GATEWAY_GUILD_MEMBERS_LIMITED),
        "Server Members Intent",
    ),
    (
        GatewayIntents::GUILD_PRESENCES,
        ApplicationFlags::GATEWAY_PRESENCE.union(ApplicationFlags::GATEWAY_PRESENCE_LIMITED),
        "Presence Intent",
    ),
    (
        GatewayIntents::MESSAGE_CONTENT,
        ApplicationFlags::GATEWAY_MESSAGE_CONTENT
            .union(ApplicationFlags::GATEWAY_MESSAGE_CONTENT_LIMITED),
        "Message Content Intent",
    ),
];

pub const TOKEN_REJECTED: &str = "Discord rejected the bot token. Copy a new token from the Bot \
                                  tab of the developer portal and save it again.";
/// How long a server list fetched for the dashboard is reused.
const GUILDS_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// Upper bound for servers listed in the server picker.
const MAX_GUILDS: u64 = 200;
/// Serenity's HTTP client has no timeout of its own; Discord calls never wait longer.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Gateway intents the bot would like: `GUILDS` plus whatever the modules need.
pub fn wanted_intents(modules: &[Arc<dyn Module>]) -> GatewayIntents {
    modules
        .iter()
        .fold(GatewayIntents::GUILDS, |acc, m| acc | m.intents())
}

/// Developer portal names of the privileged intents in `intents`.
pub fn privileged_names(intents: GatewayIntents) -> Vec<&'static str> {
    PRIVILEGED
        .iter()
        .filter(|(intent, _, _)| intents.contains(*intent))
        .map(|(_, _, name)| *name)
        .collect()
}

/// Privileged intents in `wanted` that the application flags do not allow.
fn denied_intents(wanted: GatewayIntents, flags: ApplicationFlags) -> GatewayIntents {
    PRIVILEGED
        .iter()
        .filter(|(intent, allowed_by, _)| {
            wanted.contains(*intent) && !flags.intersects(*allowed_by)
        })
        .fold(GatewayIntents::empty(), |acc, (intent, _, _)| acc | *intent)
}

/// Identifies a bot connection: the hash of its token. Only kept in memory.
pub fn client_key(token: &SecretString) -> String {
    crate::auth::hash_token(token.expose())
}

/// HTTP client for a token. `proxy` replaces `https://discord.com`, so tests can point it at
/// a fake API.
pub fn http(proxy: Option<&str>, token: &SecretString) -> Http {
    let mut builder = HttpBuilder::new(token.expose());
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy).ratelimiter_disabled(true);
    }
    builder.build()
}

/// Error from a Discord HTTP call made for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscordApiError {
    #[error("Discord rejected the bot token")]
    Unauthorized,
    #[error("Discord request failed: {0}")]
    Failed(String),
}

/// Runs a Discord HTTP call with [`HTTP_TIMEOUT`].
async fn timed<T>(
    call: impl Future<Output = Result<T, serenity::Error>>,
) -> Result<T, DiscordApiError> {
    match tokio::time::timeout(HTTP_TIMEOUT, call).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(DiscordApiError::Failed(
            "Discord did not answer in time".into(),
        )),
    }
}

impl From<serenity::Error> for DiscordApiError {
    fn from(e: serenity::Error) -> Self {
        if let serenity::Error::Http(http) = &e
            && http.status_code().map(|s| s.as_u16()) == Some(401)
        {
            return DiscordApiError::Unauthorized;
        }
        let mut msg = e.to_string();
        msg.truncate(msg.floor_char_boundary(200));
        DiscordApiError::Failed(msg)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotUser {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildOption {
    pub id: u64,
    pub name: String,
}

/// The bot a token belongs to. Used to check a token before saving it.
pub async fn current_user(
    state: &AppState,
    token: &SecretString,
) -> Result<BotUser, DiscordApiError> {
    let user = timed(http(state.discord.proxy(), token).get_current_user()).await?;
    Ok(BotUser {
        id: user.id.get(),
        name: user.name.clone(),
    })
}

/// Servers the bot of `token` is a member of, sorted by name. Cached for a few minutes.
pub async fn guilds(
    state: &AppState,
    token: &SecretString,
) -> Result<Arc<Vec<GuildOption>>, DiscordApiError> {
    let key = guilds_cache_key(token);
    if let Some(hit) = state.cache.get::<Vec<GuildOption>>(&key, GUILDS_MAX_AGE) {
        return Ok(hit);
    }
    let list = timed(http(state.discord.proxy(), token).get_guilds(None, Some(MAX_GUILDS))).await?;
    let mut list: Vec<GuildOption> = list
        .into_iter()
        .map(|g| GuildOption {
            id: g.id.get(),
            name: g.name,
        })
        .collect();
    list.sort_by_cached_key(|g| g.name.to_lowercase());
    Ok(state.cache.insert(key, list))
}

pub fn invalidate_guilds(state: &AppState, token: &SecretString) {
    state.cache.remove_prefix(&guilds_cache_key(token));
}

fn guilds_cache_key(token: &SecretString) -> String {
    format!("discord:guilds:{}", &client_key(token)[..16])
}

pub enum StartError {
    /// The token is not valid any more. Not retried until it changes.
    Rejected,
    Failed(anyhow::Error),
}

/// Builds a client with every wanted intent the application is allowed to use. Privileged
/// intents that are switched off in the developer portal are left out, so the bot still comes
/// online instead of being disconnected with "Disallowed intent(s)".
pub async fn build_client(
    state: &AppState,
    token: &SecretString,
    key: &str,
) -> Result<(Client, DiscordHandle), StartError> {
    let http = http(state.discord.proxy(), token);
    let wanted = wanted_intents(&state.modules);

    let info = tokio::time::timeout(HTTP_TIMEOUT, http.get_current_application_info()).await;
    let intents = match info {
        Err(_) => {
            tracing::warn!("Discord did not say in time which privileged intents are enabled");
            wanted
        }
        Ok(Ok(app)) => {
            let denied = denied_intents(wanted, app.flags.unwrap_or(ApplicationFlags::empty()));
            if !denied.is_empty() {
                tracing::warn!(
                    "{} disabled in the Discord developer portal for bot {}. Connecting \
                     without it, so some modules will not work. Enable it on the Bot tab of \
                     your application and reconnect the bot.",
                    privileged_names(denied).join(", "),
                    app.name
                );
            }
            wanted.difference(denied)
        }
        Ok(Err(serenity::Error::Http(e))) if e.status_code().map(|s| s.as_u16()) == Some(401) => {
            return Err(StartError::Rejected);
        }
        Ok(Err(e)) => {
            tracing::warn!("could not check which privileged intents are enabled: {e}");
            wanted
        }
    };

    let client = ClientBuilder::new_with_http(http, intents)
        .event_handler(Handler {
            state: state.clone(),
            key: key.to_owned(),
        })
        .await
        .map_err(|e| {
            StartError::Failed(anyhow::Error::new(e).context("failed to create Discord client"))
        })?;
    let handle = DiscordHandle::new(
        client.http.clone(),
        client.cache.clone(),
        client.shard_manager.clone(),
        intents,
    );
    Ok((client, handle))
}

/// Human readable reason for a client stopping, shown in the log and on the dashboard, and
/// whether it is worth trying again later.
pub fn describe_stop(error: &serenity::Error) -> (String, bool) {
    match error {
        serenity::Error::Gateway(GatewayError::DisallowedGatewayIntents) => (
            "Discord refused the connection because a privileged intent is disabled. Enable \
             Server Members Intent on the Bot tab of the developer portal and reconnect the bot."
                .into(),
            true,
        ),
        serenity::Error::Gateway(GatewayError::InvalidAuthentication) => {
            (TOKEN_REJECTED.into(), false)
        }
        other => (format!("Discord client stopped: {other}"), true),
    }
}

struct Handler {
    state: AppState,
    /// [`client_key`] of the token this client runs with.
    key: String,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!(
            "connected to Discord as {} ({} servers)",
            ready.user.name,
            ready.guilds.len()
        );
        if ready.guilds.is_empty() {
            // No GUILD_CREATE follows, so the cache is complete already.
            self.state.discord.mark_ready(&self.key);
        }
        if let Err(e) = connection::name_bot(&self.state, &self.key, &ready.user.name).await {
            tracing::error!("could not record the bot name: {e:#}");
        }
    }

    async fn cache_ready(&self, _ctx: Context, _guilds: Vec<serenity::all::GuildId>) {
        self.state.discord.mark_ready(&self.key);
    }

    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        let db = &self.state.db;
        let id = guild.id.get();
        if let Err(e) = projects::name_from_guild(db, id, &guild.name).await {
            tracing::error!("could not name the project of a server: {e:#}");
        }
        if let Err(e) = connection::name_server(db, id, &guild.name).await {
            tracing::error!("could not record a server name: {e:#}");
        }
    }

    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        let stats = &self.state.stats;
        stats.record_event();
        stats.member_joins.fetch_add(1, Ordering::Relaxed);

        let guild_id = member.guild_id.get();
        let targets = match connection::subscribers(&self.state, &self.key, guild_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(guild_id, "could not find modules for a member join: {e:#}");
                return;
            }
        };
        for (project_id, module) in targets {
            if let Err(e) = module
                .on_member_join(&ctx, &self.state, project_id, &member)
                .await
            {
                tracing::error!(
                    module = module.id(),
                    project_id,
                    "member join handler failed: {e:#}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WANTED: GatewayIntents = GatewayIntents::GUILDS.union(GatewayIntents::GUILD_MEMBERS);

    #[test]
    fn members_intent_off_is_dropped() {
        let denied = denied_intents(WANTED, ApplicationFlags::empty());
        assert_eq!(denied, GatewayIntents::GUILD_MEMBERS);
        assert_eq!(WANTED.difference(denied), GatewayIntents::GUILDS);
        assert_eq!(privileged_names(denied), ["Server Members Intent"]);
    }

    #[test]
    fn members_intent_on_is_kept() {
        for flag in [
            ApplicationFlags::GATEWAY_GUILD_MEMBERS,
            ApplicationFlags::GATEWAY_GUILD_MEMBERS_LIMITED,
        ] {
            assert!(denied_intents(WANTED, flag).is_empty());
        }
    }

    #[test]
    fn unrelated_flags_do_not_allow_members() {
        let flags = ApplicationFlags::GATEWAY_PRESENCE | ApplicationFlags::GATEWAY_MESSAGE_CONTENT;
        assert_eq!(denied_intents(WANTED, flags), GatewayIntents::GUILD_MEMBERS);
    }

    #[test]
    fn non_privileged_intents_are_never_denied() {
        assert!(denied_intents(GatewayIntents::GUILDS, ApplicationFlags::empty()).is_empty());
    }

    #[test]
    fn client_keys_differ_per_token_and_hide_it() {
        let a = SecretString::new("a.b.c");
        let b = SecretString::new("a.b.d");
        assert_eq!(client_key(&a), client_key(&a.clone()));
        assert_ne!(client_key(&a), client_key(&b));
        assert!(!client_key(&a).contains("a.b.c"));
    }

    #[tokio::test]
    async fn token_check_and_server_list_use_the_api() {
        let app = crate::test_util::TestApp::new().await;
        let state = &app.state;
        let token = SecretString::new(crate::test_util::DISCORD_TOKEN);
        let bot = current_user(state, &token).await.unwrap();
        assert_eq!(bot.id, mock::BOT_ID);
        assert_eq!(bot.name, mock::BOT_NAME);

        let list = guilds(state, &token).await.unwrap();
        assert_eq!(
            list.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
            [crate::test_util::GUILD_NAME, mock::OTHER_GUILD_NAME]
        );
        // Cached: a failing API does not matter until invalidated.
        app.discord.fail_with(Some(500));
        assert_eq!(guilds(state, &token).await.unwrap().len(), 2);
        invalidate_guilds(state, &token);
        assert!(matches!(
            guilds(state, &token).await,
            Err(DiscordApiError::Failed(_))
        ));
        app.discord.fail_with(None);

        let wrong = SecretString::new("wrong.token.value");
        assert_eq!(
            current_user(state, &wrong).await,
            Err(DiscordApiError::Unauthorized)
        );
    }
}
