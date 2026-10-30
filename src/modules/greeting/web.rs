use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use serenity::all::GatewayIntents;

use super::template::{MAX_TEMPLATE_LEN, PLACEHOLDERS, Vars};
use super::{
    GreetingConfig, GreetingStats, ID, LogRow, load_config, load_stats, log_attempt, save_config,
    send,
};
use crate::auth::extract::CurrentUser;
use crate::discord::connection::connection;
use crate::discord::info::{ChannelOption, channel_choices};
use crate::error::{AppError, AppResult};
use crate::keys::Service;
use crate::keys::web::{KeyView, key_routes, key_view};
use crate::modules::{self, text};
use crate::projects::Project;
use crate::state::AppState;
use crate::web::{MsgQuery, PageCtx, fmt_ts, redirect_msg, require_project};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/projects/{pid}/modules/greeting", get(page).post(save))
        .route("/projects/{pid}/modules/greeting/test", post(send_test))
        .merge(key_routes(Service::Discord, ID))
}

fn base_path(pid: i64) -> String {
    format!("/projects/{pid}/modules/{ID}")
}

impl LogRow {
    fn time(&self) -> String {
        fmt_ts(self.created_at)
    }

    fn timestamp(&self) -> i64 {
        self.created_at
    }

    fn error_text(&self) -> &str {
        self.error.as_deref().unwrap_or("")
    }
}

#[derive(Template)]
#[template(path = "modules/greeting.html")]
struct GreetingPage {
    page: PageCtx,
    project: Project,
    enabled: bool,
    keys: Vec<KeyView>,
    /// Name and member count of the Discord server, for the preview.
    server_name: String,
    member_count: u64,
    channels: Vec<ChannelOption>,
    template: String,
    use_embed: bool,
    embed_color: String,
    greet_bots: bool,
    placeholders: &'static [(&'static str, &'static str)],
    max_len: usize,
    warnings: Vec<&'static str>,
    stats: GreetingStats,
    bot_name: String,
}

async fn page(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let project = require_project(&state, pid).await?;
    let cfg = load_config(&state.db, pid).await?;
    let enabled = modules::is_enabled(&state.db, pid, ID).await?;
    let conn = connection(&state, pid, ID).await?.ok();

    let selected = cfg.channel_id.map(|c| c as u64);
    let channels = channel_choices(conn.as_ref(), selected);

    let mut warnings = Vec::new();
    if let Some(conn) = &conn {
        if !conn.handle.intents.contains(GatewayIntents::GUILD_MEMBERS) {
            warnings.push(
                "Server Members Intent is disabled in the Discord developer portal, so the bot \
                 does not see members joining and no greetings are sent. Enable it on the Bot \
                 tab of your application and reconnect the bot.",
            );
        }
        match channels.iter().find(|c| c.selected) {
            None if selected.is_some() => {
                warnings.push("The configured channel no longer exists. Pick a new one.")
            }
            None => {
                warnings.push("No channel selected yet. Greetings are not sent until you pick one.")
            }
            Some(c) if !c.can_send => {
                warnings.push("The bot cannot view or send messages in the selected channel.")
            }
            Some(c) if cfg.use_embed && !c.can_embed => warnings
                .push("Embed mode is on but the bot lacks the Embed Links permission there."),
            Some(_) => {}
        }
    }

    let guild = conn.as_ref().and_then(|c| c.guild());
    let bot_name = conn
        .as_ref()
        .map(|c| c.handle.cache.current_user().name.clone())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Scorpio".into());

    let page = GreetingPage {
        page: PageCtx::new(&user, "projects", &msg),
        project,
        enabled,
        keys: vec![key_view(&state, pid, Service::Discord, ID).await?],
        server_name: guild
            .as_ref()
            .map(|g| g.name.clone())
            .unwrap_or_else(|| "your server".into()),
        member_count: guild.as_ref().map(|g| g.member_count).unwrap_or_default(),
        channels,
        template: cfg.template,
        use_embed: cfg.use_embed,
        embed_color: format!("#{:06x}", cfg.embed_color & 0xff_ffff),
        greet_bots: cfg.greet_bots,
        placeholders: PLACEHOLDERS,
        max_len: MAX_TEMPLATE_LEN,
        warnings,
        stats: load_stats(&state.db, pid).await?,
        bot_name,
    };
    Ok(Html(page.render()?))
}

#[derive(Deserialize)]
struct SaveForm {
    channel_id: String,
    template: String,
    embed_color: String,
    use_embed: Option<String>,
    greet_bots: Option<String>,
}

async fn save(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
    Form(form): Form<SaveForm>,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let back = base_path(pid);
    let saved = load_config(&state.db, pid)
        .await?
        .channel_id
        .map(|c| c as u64);

    let channel_id = match form.channel_id.trim() {
        "" => None,
        raw => {
            let id: u64 = raw.parse().map_err(|_| AppError::BadRequest)?;
            let conn = connection(&state, pid, ID).await?.ok();
            if !channel_choices(conn.as_ref(), saved)
                .iter()
                .any(|c| c.id == id)
            {
                return Ok(redirect_msg(&back, "invalid_channel"));
            }
            Some(id as i64)
        }
    };

    let template = form.template.replace("\r\n", "\n");
    let template = template.trim();
    if template.is_empty() {
        return Ok(redirect_msg(&back, "template_empty"));
    }
    if template.chars().count() > MAX_TEMPLATE_LEN {
        return Ok(redirect_msg(&back, "template_long"));
    }

    let Some(embed_color) = text::parse_hex_color(&form.embed_color) else {
        return Ok(redirect_msg(&back, "invalid_color"));
    };

    let cfg = GreetingConfig {
        channel_id,
        template: template.to_owned(),
        use_embed: form.use_embed.is_some(),
        embed_color: i64::from(embed_color),
        greet_bots: form.greet_bots.is_some(),
    };
    save_config(&state.db, pid, &cfg, &user.username).await?;
    tracing::info!(user = %user.username, project_id = pid, "greeting settings saved");
    Ok(redirect_msg(&back, "saved"))
}

/// Sends the configured greeting using the bot itself as the sample member. Nobody is pinged.
async fn send_test(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let back = base_path(pid);
    let Ok(conn) = connection(&state, pid, ID).await? else {
        return Ok(redirect_msg(&back, "bot_offline"));
    };
    let cfg = load_config(&state.db, pid).await?;
    let Some(channel_id) = cfg.channel_id else {
        return Ok(redirect_msg(&back, "no_channel"));
    };
    let guild = conn.guild().ok_or(AppError::NotFound)?;

    let (vars, avatar) = {
        let me = conn.handle.cache.current_user();
        let vars = Vars {
            user_id: me.id.get(),
            username: me.name.clone(),
            display_name: me.global_name.clone().unwrap_or_else(|| me.name.clone()),
            server: guild.name,
            member_count: guild.member_count,
        };
        (vars, me.face())
    };

    let result = send(
        &conn.handle.http,
        &cfg,
        channel_id as u64,
        &vars,
        None,
        avatar,
    )
    .await;
    log_attempt(&state.db, pid, &vars, &result, true).await?;
    tracing::info!(user = %user.username, project_id = pid, ok = result.is_ok(), "greeting test sent");
    Ok(redirect_msg(
        &back,
        if result.is_ok() {
            "test_sent"
        } else {
            "test_failed"
        },
    ))
}
