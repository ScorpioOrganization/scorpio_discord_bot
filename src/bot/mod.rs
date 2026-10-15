pub mod info;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context as _, Result, bail};
use serenity::all::{
    ApplicationFlags, Client, ClientBuilder, Context, EventHandler, GatewayError, GatewayIntents,
    Http, Member, Ready,
};
use serenity::async_trait;

use crate::modules::{self, Module};
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

const TOKEN_REJECTED: &str = "DISCORD_TOKEN was rejected by Discord. Copy a new token from the Bot \
                              tab of the developer portal.";

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

/// Builds the client with every wanted intent the application is allowed to use. Privileged
/// intents that are switched off in the developer portal are left out, so the bot still comes
/// online instead of being disconnected with "Disallowed intent(s)".
pub async fn build_client(token: &str, state: AppState) -> Result<(Client, GatewayIntents)> {
    let http = Http::new(token);
    let wanted = wanted_intents(&state.modules);

    let intents = match http.get_current_application_info().await {
        Ok(app) => {
            let denied = denied_intents(wanted, app.flags.unwrap_or(ApplicationFlags::empty()));
            if !denied.is_empty() {
                tracing::warn!(
                    "{} disabled in the Discord developer portal. Connecting without it, so some \
                     modules will not work. Enable it on the Bot tab of your application and \
                     restart the bot.",
                    privileged_names(denied).join(", ")
                );
            }
            wanted.difference(denied)
        }
        Err(serenity::Error::Http(e)) if e.status_code().map(|s| s.as_u16()) == Some(401) => {
            bail!(TOKEN_REJECTED)
        }
        Err(e) => {
            tracing::warn!("could not check which privileged intents are enabled: {e}");
            wanted
        }
    };

    let client = ClientBuilder::new_with_http(http, intents)
        .event_handler(Handler { state })
        .await
        .context("failed to create Discord client")?;
    Ok((client, intents))
}

/// Human readable reason for the client stopping, shown in the log and on the dashboard.
pub fn describe_stop(error: &serenity::Error) -> String {
    match error {
        serenity::Error::Gateway(GatewayError::DisallowedGatewayIntents) => {
            "Discord refused the connection because a privileged intent is disabled. Enable \
             Server Members Intent on the Bot tab of the developer portal and restart the bot."
                .into()
        }
        serenity::Error::Gateway(GatewayError::InvalidAuthentication) => TOKEN_REJECTED.into(),
        other => format!("Discord client stopped: {other}"),
    }
}

struct Handler {
    state: AppState,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!(
            "connected to Discord as {} ({} servers)",
            ready.user.name,
            ready.guilds.len()
        );
    }

    async fn guild_member_addition(&self, ctx: Context, member: Member) {
        let stats = &self.state.stats;
        stats.record_event();
        stats.member_joins.fetch_add(1, Ordering::Relaxed);

        let guild_id = member.guild_id.get();
        for module in self.state.modules.iter() {
            match modules::is_enabled(&self.state.db, guild_id, module.id()).await {
                Ok(true) => {
                    if let Err(e) = module.on_member_join(&ctx, &self.state, &member).await {
                        tracing::error!(
                            module = module.id(),
                            guild_id,
                            "member join handler failed: {e:#}"
                        );
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::error!(module = module.id(), "could not read module state: {e:#}")
                }
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
}
