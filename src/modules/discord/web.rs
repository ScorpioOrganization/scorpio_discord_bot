use askama::Template;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::response::Html;
use axum::routing::get;

use super::ID;
use crate::auth::extract::CurrentUser;
use crate::discord::connection;
use crate::discord::info::{self, BotStatus};
use crate::error::AppResult;
use crate::keys::web::{KeyView, key_routes, key_view};
use crate::keys::{self, Service};
use crate::modules;
use crate::projects::Project;
use crate::state::AppState;
use crate::web::{MsgQuery, PageCtx, require_project};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/projects/{pid}/modules/discord", get(page))
        .merge(key_routes(Service::Discord, ID))
}

#[derive(Template)]
#[template(path = "modules/discord.html")]
struct DiscordPage {
    page: PageCtx,
    project: Project,
    enabled: bool,
    keys: Vec<KeyView>,
    /// Status of the bot of the saved token, None without one.
    bot: Option<BotStatus>,
    /// Enabled modules that use this bot.
    users: Vec<&'static str>,
    warnings: Vec<String>,
}

async fn page(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let project = require_project(&state, pid).await?;
    let enabled = modules::is_enabled(&state.db, pid, ID).await?;

    let bot = match connection::binding(&state, pid, ID).await? {
        Ok(b) => Some(info::status(&state, state.discord.status(&b.key).as_ref()).await),
        Err(_) => None,
    };

    let mut users = Vec::new();
    for m in state.modules.iter() {
        if m.services().contains(&Service::Discord)
            && modules::is_enabled(&state.db, pid, m.id()).await?
            && keys::use_shared(&state.db, pid, Service::Discord, m.id()).await?
        {
            users.push(m.name());
        }
    }

    let mut warnings = Vec::new();
    if !enabled {
        warnings.push(
            "This module is disabled, so its bot is not connected and other modules cannot use \
             it. Modules with their own bot token keep working."
                .to_owned(),
        );
    }
    if let Some(bot) = &bot
        && !bot.missing_intents.is_empty()
    {
        warnings.push(format!(
            "{} disabled in the Discord developer portal, so the bot runs without it and \
             modules that depend on it do not work. Enable it on the Bot tab of your \
             application, then reconnect the bot.",
            bot.missing_intents.join(", ")
        ));
    }

    let page = DiscordPage {
        page: PageCtx::new(&user, "projects", &msg),
        project,
        enabled,
        keys: vec![key_view(&state, pid, Service::Discord, ID).await?],
        bot,
        users,
        warnings,
    };
    Ok(Html(page.render()?))
}
