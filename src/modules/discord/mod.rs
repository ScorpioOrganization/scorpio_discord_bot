//! The Discord module: holds the bot token, and the server that bot works in, which other
//! modules can share. Switching it off disconnects the shared bot, so modules using it stop
//! until it is back on or they get their own token.

mod web;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;

use super::{Module, StatItem};
use crate::discord::connection::{self, DiscordProblem};
use crate::keys::{self, KeyProblem, Service};
use crate::state::AppState;

pub const ID: &str = "discord";

pub struct DiscordModule;

#[async_trait]
impl Module for DiscordModule {
    fn id(&self) -> &'static str {
        ID
    }

    fn name(&self) -> &'static str {
        "Discord"
    }

    fn description(&self) -> &'static str {
        "Connects a Discord bot other modules can share, in the server of your choice."
    }

    async fn summary(&self, state: &AppState, project_id: i64) -> Result<Vec<StatItem>> {
        let bot = match keys::info(&state.db, project_id, Service::Discord, ID).await? {
            None => "Not set".to_owned(),
            Some(info) if info.account.is_empty() => "Not seen yet".to_owned(),
            Some(info) => info.account,
        };
        let server = connection::server(&state.db, project_id, ID)
            .await?
            .map(|(_, name)| name)
            .unwrap_or_else(|| "Not selected".into());
        let status = match connection::connection(state, project_id, ID).await? {
            Ok(_) => "Connected",
            Err(DiscordProblem::Key(KeyProblem::Unreadable)) => "Token unreadable",
            Err(DiscordProblem::Key(_)) => "No token",
            Err(DiscordProblem::NoServer) => "No server",
            Err(DiscordProblem::Connecting) => "Connecting",
            Err(DiscordProblem::NotInServer) => "Not in the server",
            Err(DiscordProblem::Offline(_)) => "Not connected",
        };
        Ok(vec![
            StatItem::new("Bot", bot),
            StatItem::new("Server", server),
            StatItem::new("Status", status),
        ])
    }

    fn routes(&self) -> Router<AppState> {
        web::routes()
    }
}
