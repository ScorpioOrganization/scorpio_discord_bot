mod pages;

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
use crate::bot::info::{self, GuildSummary};
use crate::error::AppError;
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
        .route("/guilds/{gid}", get(pages::guild_page))
        .route(
            "/guilds/{gid}/modules/{mid}/toggle",
            post(pages::toggle_module),
        )
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
            "bot_offline" => (false, "The bot is not connected to Discord."),
            "invalid_channel" => (false, "Pick a text channel from this server."),
            "template_empty" => (false, "The message cannot be empty."),
            "template_long" => (false, "The message is too long."),
            "invalid_color" => (false, "Embed color must look like #7f8cff."),
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

/// Looks up a guild the bot is a member of. Everything guild related 404s otherwise.
pub fn require_guild(state: &AppState, guild_id: u64) -> Result<GuildSummary, AppError> {
    state
        .discord()
        .and_then(|d| info::guild(&d.cache, guild_id))
        .ok_or(AppError::NotFound)
}

pub fn redirect_msg(path: &str, code: &str) -> Response {
    axum::response::Redirect::to(&format!("{path}?msg={code}")).into_response()
}
