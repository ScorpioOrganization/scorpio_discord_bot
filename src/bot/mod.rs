pub mod info;

use std::sync::atomic::Ordering;

use anyhow::{Context as _, Result};
use serenity::all::{Client, Context, EventHandler, GatewayIntents, Member, Ready};
use serenity::async_trait;

use crate::modules;
use crate::state::AppState;

/// Gateway intents the bot needs. GUILD_MEMBERS is privileged and must be enabled in the
/// Discord developer portal ("Server Members Intent").
pub fn intents() -> GatewayIntents {
    GatewayIntents::GUILDS | GatewayIntents::GUILD_MEMBERS
}

pub async fn build_client(token: &str, state: AppState) -> Result<Client> {
    Client::builder(token, intents())
        .event_handler(Handler { state })
        .await
        .context("failed to create Discord client")
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
