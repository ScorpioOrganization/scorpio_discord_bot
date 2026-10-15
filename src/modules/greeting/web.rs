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
use crate::bot::info::{self, ChannelOption, GuildSummary};
use crate::error::{AppError, AppResult};
use crate::modules::{self, text};
use crate::state::AppState;
use crate::web::{MsgQuery, PageCtx, fmt_ts, redirect_msg, require_guild};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/guilds/{gid}/modules/greeting", get(page).post(save))
        .route("/guilds/{gid}/modules/greeting/test", post(send_test))
}

fn base_path(gid: u64) -> String {
    format!("/guilds/{gid}/modules/{ID}")
}

fn channels(state: &AppState, gid: u64) -> Vec<ChannelOption> {
    state
        .discord()
        .map(|d| info::text_channels(&d.cache, gid))
        .unwrap_or_default()
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
    guild: GuildSummary,
    enabled: bool,
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
    Path(gid): Path<u64>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let guild = require_guild(&state, gid)?;
    let cfg = load_config(&state.db, gid).await?;
    let enabled = modules::is_enabled(&state.db, gid, ID).await?;

    let mut channels = channels(&state, gid);
    let selected = cfg.channel_id.map(|c| c as u64);
    for c in &mut channels {
        c.selected = Some(c.id) == selected;
    }

    let mut warnings = Vec::new();
    if !state
        .missing_intents(GatewayIntents::GUILD_MEMBERS)
        .is_empty()
    {
        warnings.push(
            "Server Members Intent is disabled in the Discord developer portal, so the bot does not \
             see members joining and no greetings are sent. Enable it on the Bot tab of your \
             application and restart the bot.",
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
        Some(c) if cfg.use_embed && !c.can_embed => {
            warnings.push("Embed mode is on but the bot lacks the Embed Links permission there.")
        }
        Some(_) => {}
    }

    let bot_name = state
        .discord()
        .map(|d| d.cache.current_user().name.clone())
        .unwrap_or_else(|| "Scorpio".into());

    let page = GreetingPage {
        page: PageCtx::new(&user, "guilds", &msg),
        guild,
        enabled,
        channels,
        template: cfg.template,
        use_embed: cfg.use_embed,
        embed_color: format!("#{:06x}", cfg.embed_color & 0xff_ffff),
        greet_bots: cfg.greet_bots,
        placeholders: PLACEHOLDERS,
        max_len: MAX_TEMPLATE_LEN,
        warnings,
        stats: load_stats(&state.db, gid).await?,
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
    Path(gid): Path<u64>,
    Form(form): Form<SaveForm>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let back = base_path(gid);

    let channel_id = match form.channel_id.trim() {
        "" => None,
        raw => {
            let id: u64 = raw.parse().map_err(|_| AppError::BadRequest)?;
            if !channels(&state, gid).iter().any(|c| c.id == id) {
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
    save_config(&state.db, gid, &cfg, &user.username).await?;
    tracing::info!(user = %user.username, guild_id = gid, "greeting settings saved");
    Ok(redirect_msg(&back, "saved"))
}

/// Sends the configured greeting using the bot itself as the sample member. Nobody is pinged.
async fn send_test(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
) -> AppResult<Response> {
    let guild = require_guild(&state, gid)?;
    let back = base_path(gid);
    let Some(discord) = state.discord() else {
        return Ok(redirect_msg(&back, "bot_offline"));
    };
    let cfg = load_config(&state.db, gid).await?;
    let Some(channel_id) = cfg.channel_id else {
        return Ok(redirect_msg(&back, "no_channel"));
    };

    let (vars, avatar) = {
        let me = discord.cache.current_user();
        let vars = Vars {
            user_id: me.id.get(),
            username: me.name.clone(),
            display_name: me.global_name.clone().unwrap_or_else(|| me.name.clone()),
            server: guild.name,
            member_count: guild.member_count,
        };
        (vars, me.face())
    };

    let result = send(&discord.http, &cfg, channel_id as u64, &vars, None, avatar).await;
    log_attempt(&state.db, gid, &vars, &result, true).await?;
    tracing::info!(user = %user.username, guild_id = gid, ok = result.is_ok(), "greeting test sent");
    Ok(redirect_msg(
        &back,
        if result.is_ok() {
            "test_sent"
        } else {
            "test_failed"
        },
    ))
}
