mod pages;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use time::OffsetDateTime;
use time::macros::format_description;
use tower_http::trace::TraceLayer;

use crate::auth::SessionUser;
use crate::auth::extract::session_middleware;
use crate::error::AppError;
use crate::projects::{self, Project};
use crate::state::AppState;

const APP_CSS: &str = include_str!("../../static/app.css");
const APP_JS: &str = include_str!("../../static/app.js");
const FAVICON: &str = include_str!("../../static/favicon.svg");

const CSP: &str = "default-src 'self'; img-src 'self' https://cdn.discordapp.com data:; \
    style-src 'self'; script-src 'self'; object-src 'none'; base-uri 'none'; \
    form-action 'self'; frame-ancestors 'none'";

pub async fn serve(
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let addr = state.config.web_bind;
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind dashboard to {addr}"))?;
    tracing::info!("dashboard listening on http://{addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}

pub fn router(state: AppState) -> Router {
    let mut app = Router::new()
        .route("/", get(pages::overview))
        .route("/login", get(pages::login_page).post(pages::login_submit))
        .route("/logout", post(pages::logout))
        .route("/projects", post(pages::project_create))
        .route("/projects/{pid}", get(pages::project_page))
        .route("/projects/{pid}/rename", post(pages::project_rename))
        .route("/projects/{pid}/delete", post(pages::project_delete))
        .route(
            "/projects/{pid}/modules/{mid}/toggle",
            post(pages::toggle_module),
        )
        // Links from before projects, when everything was per Discord server.
        .route("/guilds/{gid}", get(pages::legacy_guild))
        .route("/guilds/{gid}/{*rest}", get(pages::legacy_guild_path))
        .route("/account", get(pages::account_page))
        .route("/account/password", post(pages::account_password))
        .route("/users", get(pages::users_page).post(pages::user_create))
        .route("/users/{id}/password", post(pages::user_reset_password))
        .route("/users/{id}/delete", post(pages::user_delete));

    for module in state.modules.iter() {
        app = app.merge(module.routes());
    }

    app.route(
        "/static/app.css",
        get(|| static_file(APP_CSS, "text/css; charset=utf-8")),
    )
    .route(
        "/static/app.js",
        get(|| static_file(APP_JS, "text/javascript; charset=utf-8")),
    )
    .route(
        "/favicon.svg",
        get(|| static_file(FAVICON, "image/svg+xml")),
    )
    .route("/healthz", get(|| async { "ok" }))
    .fallback(|| async { AppError::NotFound })
    .layer(from_fn_with_state(state.clone(), session_middleware))
    .layer(from_fn(security_headers))
    .layer(TraceLayer::new_for_http())
    .with_state(state)
}

async fn static_file(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body,
    )
        .into_response()
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}

/// Data every dashboard page needs for the layout.
pub struct PageCtx {
    pub username: String,
    pub is_root: bool,
    pub csrf: String,
    pub nav: &'static str,
    pub flash: Option<Flash>,
    pub version: &'static str,
}

impl PageCtx {
    pub fn new(user: &SessionUser, nav: &'static str, msg: &MsgQuery) -> Self {
        Self {
            username: user.username.clone(),
            is_root: user.is_root,
            csrf: user.csrf.clone(),
            nav,
            flash: msg.msg.as_deref().and_then(Flash::from_code),
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

#[derive(Deserialize, Default)]
pub struct MsgQuery {
    pub msg: Option<String>,
}

/// Messages shown after a redirect. Only fixed codes are accepted, nothing user supplied
/// is ever reflected into the page.
pub struct Flash {
    pub ok: bool,
    pub text: &'static str,
}

impl Flash {
    fn from_code(code: &str) -> Option<Self> {
        let (ok, text) = match code {
            "saved" => (true, "Settings saved."),
            "enabled" => (true, "Module enabled."),
            "disabled" => (true, "Module disabled."),
            "test_sent" => (true, "Test message sent. Check the channel in Discord."),
            "test_failed" => (
                false,
                "Test message failed. See the error in recent activity below.",
            ),
            "no_channel" => (
                false,
                "Pick a channel and save before sending a test message.",
            ),
            "bot_offline" => (
                false,
                "This module cannot reach Discord right now. See the Discord bot token card.",
            ),
            "invalid_channel" => (false, "Pick a text channel from the Discord server."),
            "template_empty" => (false, "The message cannot be empty."),
            "template_long" => (false, "The message is too long."),
            "invalid_color" => (false, "Embed color must look like #7f8cff."),
            "token_saved" => (true, "API key checked with ClickUp and saved."),
            "token_removed" => (true, "Key removed."),
            "token_invalid" => (
                false,
                "That does not look like a ClickUp personal API key, which starts with pk_.",
            ),
            "token_rejected" => (false, "ClickUp rejected this API key. Nothing was saved."),
            "clickup_unreachable" => (
                false,
                "Could not reach ClickUp. Nothing was changed, try again in a moment.",
            ),
            "token_ok" => (true, "ClickUp accepted the saved API key."),
            "token_check_failed" => (
                false,
                "ClickUp did not accept the saved API key. See the error on the key card.",
            ),
            "token_missing" => (false, "Save a ClickUp API key first."),
            "token_unreadable" => (
                false,
                "The saved API key cannot be decrypted. Save the key again.",
            ),
            "source_saved" => (true, "Key choice saved."),
            "discord_token_saved" => (true, "Bot token checked with Discord and saved."),
            "discord_token_invalid" => (
                false,
                "That does not look like a Discord bot token. Copy it from the Bot tab of the \
                 developer portal.",
            ),
            "discord_token_rejected" => {
                (false, "Discord rejected this bot token. Nothing was saved.")
            }
            "discord_unreachable" => (
                false,
                "Could not reach Discord. Nothing was changed, try again in a moment.",
            ),
            "discord_token_missing" => (false, "Save a Discord bot token first."),
            "server_saved" => (true, "Discord server saved."),
            "server_invalid" => (false, "Pick a server the bot is a member of."),
            "reconnecting" => (
                true,
                "Reconnecting the bot. Reload the page in a few seconds.",
            ),
            "project_created" => (true, "Project created."),
            "project_renamed" => (true, "Project renamed."),
            "project_deleted" => (true, "Project deleted."),
            "project_name" => (false, "The project name must be 1 to 60 characters."),
            "too_many_projects" => (false, "The maximum number of projects is reached."),
            "workspace_saved" => (true, "Workspace saved."),
            "workspace_invalid" => (false, "Pick a workspace this API key can access."),
            "mapping_saved" => (true, "Mappings saved."),
            "mapping_unknown" => (
                false,
                "A Discord member was not found or the name is not unique. Use a username or \
                 user ID. Nothing was saved.",
            ),
            "members_refreshed" => (true, "Server members reloaded."),
            "tag_roles_saved" => (true, "Tag roles saved."),
            "role_unknown" => (
                false,
                "Pick a role from the Discord server. Nothing was saved.",
            ),
            "tag_invalid" => (
                false,
                "Tags must be 1 to 100 characters. Nothing was saved.",
            ),
            "too_many_tag_roles" => (
                false,
                "This project has the maximum number of tag roles. Nothing was saved.",
            ),
            "clickup_disabled" => (
                false,
                "This module uses the shared ClickUp key, but the ClickUp module is disabled. \
                 Enable it or save a separate key.",
            ),
            "list_not_found" => (
                false,
                "That ClickUp list was not found, or the API key cannot see it.",
            ),
            "timezone_invalid" => (false, "Unknown time zone. Use a name like Europe/Warsaw."),
            "tags_invalid" => (false, "Use at most 5 tags of up to 50 characters each."),
            "calendar_no_list" => (false, "Select a ClickUp list first."),
            "refreshed" => (true, "Reloaded from ClickUp."),
            "rule_created" => (true, "Rule created."),
            "rule_deleted" => (true, "Rule deleted."),
            "too_many_rules" => (false, "This project has the maximum number of rules."),
            "rule_name" => (false, "The rule name must be 1 to 60 characters."),
            "rule_no_days" => (false, "Pick at least one day."),
            "rule_time" => (false, "The time must look like 09:00."),
            "rule_lookahead" => (false, "Look ahead between 1 and 336 hours."),
            "rule_offset" => (
                false,
                "Start the window between 0 and 10080 minutes (7 days) after the post.",
            ),
            "rule_lead" => (
                false,
                "Remind between 1 minute and 7 days (10080 minutes) before.",
            ),
            "rule_max_events" => (false, "Show between 1 and 50 events."),
            "rule_description_max" => (false, "The description length must be 0 to 1000."),
            "reactions_invalid" => (
                false,
                "Reactions must be up to 5 emojis, custom ones from the Discord server only.",
            ),
            "recurring_no_workspace" => (false, "Select and save a ClickUp workspace first."),
            "recurring_no_rules" => (false, "Add an active rule first."),
            "recurring_interval" => (false, "Check every 5 to 1440 minutes."),
            "recurring_ran" => (true, "Run finished. See the result in recent activity."),
            "recurring_ran_failed" => (
                false,
                "Run finished, but some tasks could not be changed. See recent activity.",
            ),
            "recurring_dry_ran" => (
                true,
                "Dry run finished, nothing was changed in ClickUp. See recent activity.",
            ),
            "recurring_busy" => (false, "A run is already in progress, try again shortly."),
            "rule_tag" => (
                false,
                "The tag must be 1 to 100 characters, without commas.",
            ),
            "rule_tag_taken" => (false, "Another rule already uses this tag."),
            "rule_every" => (false, "Repeat every 1 to 365 days, weeks, months or years."),
            "rule_invalid" => (
                false,
                "Pick an interval, trigger and action from the lists.",
            ),
            "rule_status" => (false, "The status name must be at most 60 characters."),
            "pw_changed" => (
                true,
                "Password changed. Your other sessions were signed out.",
            ),
            "pw_wrong" => (false, "Current password is incorrect."),
            "pw_mismatch" => (false, "The passwords do not match."),
            "pw_short" => (false, "Password must be at least 10 characters."),
            "pw_long" => (false, "Password must be at most 256 characters."),
            "user_created" => (true, "Account created."),
            "user_exists" => (false, "An account with that name already exists."),
            "user_invalid" => (
                false,
                "Username must be 3 to 32 characters: letters, digits, _ . -",
            ),
            "user_deleted" => (true, "Account deleted."),
            "user_pw_reset" => (true, "Password reset. That user was signed out everywhere."),
            "root_protected" => (
                false,
                "The root account cannot be changed here. Use the Account page.",
            ),
            _ => return None,
        };
        Some(Self { ok, text })
    }
}

pub fn fmt_ts(ts: i64) -> String {
    OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| {
            t.format(format_description!(
                "[year]-[month]-[day] [hour]:[minute] UTC"
            ))
            .ok()
        })
        .unwrap_or_else(|| "-".into())
}

pub fn fmt_duration(secs: u64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h {m}m"),
    }
}

/// Looks up a project. Everything under `/projects/{pid}` 404s for unknown ones.
pub async fn require_project(state: &AppState, project_id: i64) -> Result<Project, AppError> {
    projects::get(&state.db, project_id)
        .await?
        .ok_or(AppError::NotFound)
}

pub fn redirect_msg(path: &str, code: &str) -> Response {
    axum::response::Redirect::to(&format!("{path}?msg={code}")).into_response()
}
