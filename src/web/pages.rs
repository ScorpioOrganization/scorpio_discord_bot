use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use askama::Template;
use axum::Form;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use serde::Deserialize;

use super::{MsgQuery, PageCtx, fmt_duration, fmt_ts, redirect_msg, require_guild};
use crate::auth::extract::{CurrentUser, RootUser, client_ip};
use crate::auth::{self, SESSION_COOKIE, SESSION_TTL_SECS};
use crate::bot::info::{self, BotStatus, GuildSummary};
use crate::db::now;
use crate::error::{AppError, AppResult};
use crate::modules::{self, StatItem};
use crate::state::AppState;

// ---------------------------------------------------------------- login

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    error: Option<&'static str>,
}

pub async fn login_page(user: Option<CurrentUser>) -> AppResult<Response> {
    if user.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    Ok(Html(LoginPage { error: None }.render()?).into_response())
}

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

pub async fn login_submit(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, peer, state.config.trust_proxy);
    let username = form.username.trim();

    if let Some(wait) = state.limiter.check(&ip, username) {
        tracing::warn!(%ip, username, "login blocked by rate limit");
        let minutes = wait.as_secs().div_ceil(60);
        let error = if minutes <= 1 {
            "Too many failed attempts. Try again in a minute."
        } else {
            "Too many failed attempts. Try again in a few minutes."
        };
        return login_error(StatusCode::TOO_MANY_REQUESTS, error);
    }

    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE username = ?")
            .bind(username.to_owned())
            .fetch_optional(&state.db)
            .await?;
    let (user_id, hash) = match row {
        Some((id, hash)) => (Some(id), Some(hash)),
        None => (None, None),
    };
    let valid = auth::verify_password(form.password, hash).await;

    let Some(user_id) = user_id.filter(|_| valid) else {
        state.limiter.record_failure(&ip, username);
        tracing::warn!(%ip, username, "failed login");
        return login_error(StatusCode::UNAUTHORIZED, "Invalid username or password.");
    };

    state.limiter.record_success(&ip, username);
    let token = auth::create_session(&state.db, user_id).await?;
    tracing::info!(%ip, username, "user logged in");

    let cookie = Cookie::build((SESSION_COOKIE, token))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.cookie_secure)
        .path("/")
        .max_age(time::Duration::seconds(SESSION_TTL_SECS));
    Ok((jar.add(cookie), Redirect::to("/")).into_response())
}

fn login_error(status: StatusCode, error: &'static str) -> AppResult<Response> {
    Ok((status, Html(LoginPage { error: Some(error) }.render()?)).into_response())
}

pub async fn logout(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    jar: CookieJar,
) -> AppResult<Response> {
    auth::delete_session(&state.db, &user.token_hash).await?;
    let removal = Cookie::build(SESSION_COOKIE).path("/");
    Ok((jar.remove(removal), Redirect::to("/login")).into_response())
}

// ---------------------------------------------------------------- overview

struct GuildRow {
    guild: GuildSummary,
    modules_enabled: i64,
}

#[derive(Template)]
#[template(path = "overview.html")]
struct OverviewPage {
    page: PageCtx,
    bot: BotStatus,
    token_configured: bool,
    uptime: String,
    guild_count: usize,
    member_total: u64,
    member_joins: u64,
    events_handled: u64,
    greetings_total: i64,
    module_count: usize,
    guilds: Vec<GuildRow>,
}

pub async fn overview(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let discord = state.discord();
    let bot = info::status(discord).await;
    let guilds = discord.map(|d| info::guilds(&d.cache)).unwrap_or_default();

    let mut rows = Vec::with_capacity(guilds.len());
    for guild in guilds {
        let modules_enabled = modules::enabled_count(&state.db, guild.id).await?;
        rows.push(GuildRow {
            guild,
            modules_enabled,
        });
    }

    let stats = &state.stats;
    let page = OverviewPage {
        page: PageCtx::new(&user, "overview", &msg),
        bot,
        token_configured: discord.is_some(),
        uptime: fmt_duration(stats.started_at.elapsed().as_secs()),
        guild_count: rows.len(),
        member_total: rows.iter().map(|r| r.guild.member_count).sum(),
        member_joins: stats.member_joins.load(Ordering::Relaxed),
        events_handled: stats.events_handled.load(Ordering::Relaxed),
        greetings_total: modules::counter(&state.db, None, modules::greeting::ID, "sent").await?,
        module_count: state.modules.len(),
        guilds: rows,
    };
    Ok(Html(page.render()?))
}

// ---------------------------------------------------------------- guild

struct ModuleCard {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    enabled: bool,
    stats: Vec<StatItem>,
}

#[derive(Template)]
#[template(path = "guild.html")]
struct GuildPage {
    page: PageCtx,
    guild: GuildSummary,
    modules: Vec<ModuleCard>,
}

pub async fn guild_page(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let guild = require_guild(&state, gid)?;
    let mut cards = Vec::with_capacity(state.modules.len());
    for m in state.modules.iter() {
        cards.push(ModuleCard {
            id: m.id(),
            name: m.name(),
            description: m.description(),
            enabled: modules::is_enabled(&state.db, gid, m.id()).await?,
            stats: m.summary(&state, gid).await?,
        });
    }
    let page = GuildPage {
        page: PageCtx::new(&user, "guilds", &msg),
        guild,
        modules: cards,
    };
    Ok(Html(page.render()?))
}

#[derive(Deserialize)]
pub struct ToggleForm {
    enabled: String,
    back: Option<String>,
}

pub async fn toggle_module(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((gid, mid)): Path<(u64, String)>,
    Form(form): Form<ToggleForm>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let module = state.module(&mid).ok_or(AppError::NotFound)?;
    let enabled = form.enabled == "1";
    modules::set_enabled(&state.db, gid, module.id(), enabled).await?;
    tracing::info!(user = %user.username, guild_id = gid, module = module.id(), enabled, "module toggled");

    // Only two fixed targets, never an arbitrary redirect.
    let back = match form.back.as_deref() {
        Some("module") => format!("/guilds/{gid}/modules/{}", module.id()),
        _ => format!("/guilds/{gid}"),
    };
    Ok(redirect_msg(
        &back,
        if enabled { "enabled" } else { "disabled" },
    ))
}

// ---------------------------------------------------------------- account

#[derive(Template)]
#[template(path = "account.html")]
struct AccountPage {
    page: PageCtx,
    min_len: usize,
}

pub async fn account_page(
    CurrentUser(user): CurrentUser,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let page = AccountPage {
        page: PageCtx::new(&user, "account", &msg),
        min_len: auth::MIN_PASSWORD_LEN,
    };
    Ok(Html(page.render()?))
}

#[derive(Deserialize)]
pub struct ChangePasswordForm {
    current: String,
    new: String,
    confirm: String,
}

pub async fn account_password(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Form(form): Form<ChangePasswordForm>,
) -> AppResult<Response> {
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users WHERE id = ?")
        .bind(user.user_id)
        .fetch_one(&state.db)
        .await?;
    if !auth::verify_password(form.current, Some(hash)).await {
        return Ok(redirect_msg("/account", "pw_wrong"));
    }
    if let Err(code) = auth::validate_password(&form.new, &form.confirm) {
        return Ok(redirect_msg("/account", code));
    }

    set_password(&state, user.user_id, form.new).await?;
    // Keep the current session, sign out everywhere else.
    sqlx::query("DELETE FROM sessions WHERE user_id = ? AND token_hash != ?")
        .bind(user.user_id)
        .bind(user.token_hash.clone())
        .execute(&state.db)
        .await?;
    tracing::info!(user = %user.username, "password changed");
    Ok(redirect_msg("/account", "pw_changed"))
}

async fn set_password(state: &AppState, user_id: i64, password: String) -> AppResult<()> {
    let hash = auth::hash_password(password).await?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(user_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- users (root only)

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i64,
    username: String,
    is_root: bool,
    created_at: i64,
    last_login_at: Option<i64>,
}

impl UserRow {
    fn created(&self) -> String {
        fmt_ts(self.created_at)
    }

    fn last_login(&self) -> String {
        self.last_login_at
            .map(fmt_ts)
            .unwrap_or_else(|| "Never".into())
    }
}

#[derive(Template)]
#[template(path = "users.html")]
struct UsersPage {
    page: PageCtx,
    users: Vec<UserRow>,
    min_len: usize,
}

pub async fn users_page(
    State(state): State<AppState>,
    RootUser(user): RootUser,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let users = sqlx::query_as(
        "SELECT id, username, is_root, created_at, last_login_at FROM users
         ORDER BY is_root DESC, username COLLATE NOCASE",
    )
    .fetch_all(&state.db)
    .await?;
    let page = UsersPage {
        page: PageCtx::new(&user, "users", &msg),
        users,
        min_len: auth::MIN_PASSWORD_LEN,
    };
    Ok(Html(page.render()?))
}

#[derive(Deserialize)]
pub struct CreateUserForm {
    username: String,
    password: String,
    confirm: String,
}

pub async fn user_create(
    State(state): State<AppState>,
    RootUser(root): RootUser,
    Form(form): Form<CreateUserForm>,
) -> AppResult<Response> {
    let username = form.username.trim();
    if let Err(code) = auth::validate_username(username)
        .and_then(|_| auth::validate_password(&form.password, &form.confirm))
    {
        return Ok(redirect_msg("/users", code));
    }

    let hash = auth::hash_password(form.password).await?;
    let res = sqlx::query(
        "INSERT INTO users (username, password_hash, is_root, created_at) VALUES (?, ?, 0, ?)",
    )
    .bind(username.to_owned())
    .bind(hash)
    .bind(now())
    .execute(&state.db)
    .await;

    match res {
        Ok(_) => {
            tracing::info!(by = %root.username, username, "account created");
            Ok(redirect_msg("/users", "user_created"))
        }
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            Ok(redirect_msg("/users", "user_exists"))
        }
        Err(e) => Err(e.into()),
    }
}

/// Fetches a non-root user for modification, or returns the redirect to show instead.
async fn target_user(state: &AppState, id: i64) -> AppResult<Result<String, Response>> {
    let row: Option<(String, bool)> =
        sqlx::query_as("SELECT username, is_root FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
    match row {
        None => Err(AppError::NotFound),
        Some((_, true)) => Ok(Err(redirect_msg("/users", "root_protected"))),
        Some((name, false)) => Ok(Ok(name)),
    }
}

#[derive(Deserialize)]
pub struct ResetPasswordForm {
    password: String,
    confirm: String,
}

pub async fn user_reset_password(
    State(state): State<AppState>,
    RootUser(root): RootUser,
    Path(id): Path<i64>,
    Form(form): Form<ResetPasswordForm>,
) -> AppResult<Response> {
    let name = match target_user(&state, id).await? {
        Ok(name) => name,
        Err(redirect) => return Ok(redirect),
    };
    if let Err(code) = auth::validate_password(&form.password, &form.confirm) {
        return Ok(redirect_msg("/users", code));
    }
    set_password(&state, id, form.password).await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(id)
        .execute(&state.db)
        .await?;
    tracing::info!(by = %root.username, username = %name, "password reset");
    Ok(redirect_msg("/users", "user_pw_reset"))
}

pub async fn user_delete(
    State(state): State<AppState>,
    RootUser(root): RootUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let name = match target_user(&state, id).await? {
        Ok(name) => name,
        Err(redirect) => return Ok(redirect),
    };
    sqlx::query("DELETE FROM users WHERE id = ? AND is_root = 0")
        .bind(id)
        .execute(&state.db)
        .await?;
    tracing::info!(by = %root.username, username = %name, "account deleted");
    Ok(redirect_msg("/users", "user_deleted"))
}
