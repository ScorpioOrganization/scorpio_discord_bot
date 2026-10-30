//! Dashboard pieces for API keys, shared by every module that uses a service.
//!
//! A module merges [`key_routes`] for each service it uses into its router, puts the views
//! from [`key_view`] into a `keys` vector of its page struct and includes
//! `templates/keys/cards.html`.

use axum::Form;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::post;
use serde::Deserialize;

use super::{Service, use_shared};
use crate::auth::SessionUser;
use crate::auth::extract::CurrentUser;
use crate::clickup::ClickUpError;
use crate::discord::connection;
use crate::discord::hub::DiscordHub;
use crate::discord::{self as dc, DiscordApiError};
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::web::{fmt_ts, redirect_msg, require_project};

/// Key card data. Never contains the key itself.
pub struct KeyView {
    pub service: &'static str,
    pub key_name: &'static str,
    /// Form target, `/projects/{pid}/modules/<owner>/keys/<service>`.
    pub action: String,
    /// The module may use the shared key instead of its own.
    pub shared_option: bool,
    /// Page of the module holding the shared key.
    pub shared_path: String,
    pub use_shared: bool,
    pub own: Option<KeyStatus>,
    pub shared: Option<KeyStatus>,
    /// Why the module cannot use the service right now.
    pub problem: Option<String>,
    /// Set when a module changes data in the service with this key, instead of only reading.
    pub write_note: Option<&'static str>,
    pub placeholder: &'static str,
    /// Where to get a key and how it is used.
    pub help: &'static str,
    /// Discord only: the server the bot works in.
    pub server: Option<ServerView>,
}

impl KeyView {
    /// "ClickUp API key", "Discord bot token".
    pub fn title(&self) -> String {
        format!("{} {}", self.service, self.key_name)
    }
}

pub struct KeyStatus {
    pub account: String,
    pub hint: String,
    pub readable: bool,
    pub updated_at: i64,
    pub updated_by: String,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    pub last_error_at: Option<i64>,
}

impl KeyStatus {
    pub fn updated(&self) -> String {
        fmt_ts(self.updated_at)
    }

    pub fn last_ok(&self) -> String {
        self.last_ok_at
            .map(fmt_ts)
            .unwrap_or_else(|| "Never".into())
    }

    pub fn last_ok_ts(&self) -> i64 {
        self.last_ok_at.unwrap_or_default()
    }

    /// The last error, unless a later request succeeded.
    pub fn current_error(&self) -> Option<&str> {
        match (self.last_error_at, self.last_ok_at) {
            (Some(err), Some(ok)) if ok > err => None,
            _ => self.last_error.as_deref(),
        }
    }

    pub fn last_error_time(&self) -> String {
        self.last_error_at.map(fmt_ts).unwrap_or_default()
    }

    pub fn last_error_ts(&self) -> i64 {
        self.last_error_at.unwrap_or_default()
    }
}

pub struct ServerView {
    /// The server can be changed here: the card shows the module's own token, or it is the
    /// Discord module's card.
    pub editable: bool,
    /// Selected server id, 0 for none.
    pub selected_id: u64,
    pub selected_name: String,
    pub options: Vec<ServerOption>,
    /// Why the server list could not be loaded.
    pub error: Option<String>,
    /// Other projects working in the selected server.
    pub also_used_by: Vec<String>,
}

impl ServerView {
    /// A server is selected but the bot's server list does not have it.
    pub fn selected_elsewhere(&self) -> bool {
        self.selected_id != 0 && !self.options.iter().any(|o| o.selected)
    }
}

pub struct ServerOption {
    pub id: u64,
    pub name: String,
    pub selected: bool,
}

async fn key_status(
    state: &AppState,
    pid: i64,
    service: Service,
    owner: &str,
) -> anyhow::Result<Option<KeyStatus>> {
    let Some(info) = super::info(&state.db, pid, service, owner).await? else {
        return Ok(None);
    };
    let readable = super::load(&state.db, &state.vault, pid, service, owner)
        .await?
        .is_ok();
    Ok(Some(KeyStatus {
        account: info.account,
        hint: info.hint,
        readable,
        updated_at: info.updated_at,
        updated_by: info.updated_by.unwrap_or_default(),
        last_ok_at: info.last_ok_at,
        last_error: info.last_error,
        last_error_at: info.last_error_at,
    }))
}

/// Whether a module writes to ClickUp with the key of `owner`, as a note for the key card.
async fn write_note(
    state: &AppState,
    pid: i64,
    owner: &str,
) -> anyhow::Result<Option<&'static str>> {
    let writer = crate::modules::recurring::ID;
    if owner == writer {
        return Ok(Some(
            "This module changes tasks in ClickUp with this key: it moves their dates and \
             status, or creates copies.",
        ));
    }
    let shared_writes = owner == Service::ClickUp.shared_owner()
        && crate::modules::is_enabled(&state.db, pid, writer).await?
        && use_shared(&state.db, pid, Service::ClickUp, writer).await?;
    Ok(shared_writes.then_some(
        "The Recurring tasks module uses this key to change tasks in ClickUp: it moves their \
         dates and status, or creates copies. Other modules only read.",
    ))
}

async fn server_view(
    state: &AppState,
    pid: i64,
    owner: &str,
    use_shared: bool,
) -> anyhow::Result<ServerView> {
    let shared_owner = Service::Discord.shared_owner();
    let from = if use_shared { shared_owner } else { owner };
    let editable = from == owner;
    let selected = connection::server(&state.db, pid, from).await?;
    let (selected_id, selected_name) = selected.unwrap_or((0, String::new()));

    let mut options = Vec::new();
    let mut error = None;
    if editable
        && let Ok(token) =
            super::load(&state.db, &state.vault, pid, Service::Discord, owner).await?
    {
        match dc::guilds(state, &token).await {
            Ok(list) => {
                options = list
                    .iter()
                    .map(|g| ServerOption {
                        id: g.id,
                        name: g.name.clone(),
                        selected: g.id == selected_id,
                    })
                    .collect()
            }
            Err(e) => error = Some(e.to_string()),
        }
    }
    let also_used_by = if selected_id == 0 {
        Vec::new()
    } else {
        connection::other_projects_on(&state.db, selected_id, pid).await?
    };
    Ok(ServerView {
        editable,
        selected_id,
        selected_name,
        options,
        error,
        also_used_by,
    })
}

pub async fn key_view(
    state: &AppState,
    pid: i64,
    service: Service,
    owner: &str,
) -> anyhow::Result<KeyView> {
    let shared_owner = service.shared_owner();
    let shared_option = owner != shared_owner;
    let shared = if shared_option {
        key_status(state, pid, service, shared_owner).await?
    } else {
        None
    };
    let use_shared = use_shared(&state.db, pid, service, owner).await?;
    let (problem, write_note, server) = match service {
        Service::ClickUp => (
            super::resolve(state, pid, service, owner)
                .await?
                .err()
                .map(|p| p.message(service).to_owned()),
            write_note(state, pid, owner).await?,
            None,
        ),
        Service::Discord => (
            connection::connection(state, pid, owner)
                .await?
                .err()
                .map(|p| p.message()),
            None,
            Some(server_view(state, pid, owner, use_shared).await?),
        ),
    };
    let (placeholder, help) = match service {
        Service::ClickUp => (
            "pk_...",
            "In ClickUp: avatar, Settings, Apps, API Token. The key is checked with ClickUp, \
             stored encrypted and never shown again.",
        ),
        Service::Discord => (
            "Bot token",
            "In the Discord developer portal: your application, Bot, Reset Token. The token is \
             checked with Discord, stored encrypted and never shown again. Turn on Server \
             Members Intent on the same tab.",
        ),
    };
    Ok(KeyView {
        service: service.name(),
        key_name: service.key_name(),
        action: format!("{}/keys/{}", module_path(pid, owner), service.id()),
        shared_option,
        shared_path: module_path(pid, shared_owner),
        use_shared,
        own: key_status(state, pid, service, owner).await?,
        shared,
        problem,
        write_note,
        placeholder,
        help,
        server,
    })
}

fn module_path(pid: i64, owner: &str) -> String {
    format!("/projects/{pid}/modules/{owner}")
}

/// Flash codes of a service's key routes.
fn code(service: Service, what: &'static str) -> &'static str {
    match (service, what) {
        (Service::ClickUp, "saved") => "token_saved",
        (Service::ClickUp, "invalid") => "token_invalid",
        (Service::ClickUp, "rejected") => "token_rejected",
        (Service::ClickUp, "unreachable") => "clickup_unreachable",
        (Service::Discord, "saved") => "discord_token_saved",
        (Service::Discord, "invalid") => "discord_token_invalid",
        (Service::Discord, "rejected") => "discord_token_rejected",
        (Service::Discord, "unreachable") => "discord_unreachable",
        _ => what,
    }
}

/// `POST .../keys/<service>` (save), `.../delete` and, for modules other than the one
/// holding the shared key, `.../source` (shared or own key). Discord adds `.../server` and
/// `.../reconnect`.
pub fn key_routes(service: Service, owner: &'static str) -> Router<AppState> {
    let base = format!("/projects/{{pid}}/modules/{owner}/keys/{}", service.id());
    let mut router = Router::new()
        .route(
            &base,
            post(
                move |State(state): State<AppState>,
                      CurrentUser(user): CurrentUser,
                      Path(pid): Path<i64>,
                      Form(form): Form<KeyForm>| {
                    save_key(service, owner, state, user, pid, form)
                },
            ),
        )
        .route(
            &format!("{base}/delete"),
            post(
                move |State(state): State<AppState>,
                      CurrentUser(user): CurrentUser,
                      Path(pid): Path<i64>| {
                    delete_key(service, owner, state, user, pid)
                },
            ),
        );
    if owner != service.shared_owner() {
        router = router.route(
            &format!("{base}/source"),
            post(
                move |State(state): State<AppState>,
                      CurrentUser(user): CurrentUser,
                      Path(pid): Path<i64>,
                      Form(form): Form<SourceForm>| {
                    save_source(service, owner, state, user, pid, form)
                },
            ),
        );
    }
    if service == Service::Discord {
        router = router
            .route(
                &format!("{base}/server"),
                post(
                    move |State(state): State<AppState>,
                          CurrentUser(user): CurrentUser,
                          Path(pid): Path<i64>,
                          Form(form): Form<ServerForm>| {
                        save_server(owner, state, user, pid, form)
                    },
                ),
            )
            .route(
                &format!("{base}/reconnect"),
                post(
                    move |State(state): State<AppState>,
                          CurrentUser(user): CurrentUser,
                          Path(pid): Path<i64>| {
                        reconnect(owner, state, user, pid)
                    },
                ),
            );
    }
    router
}

#[derive(Deserialize)]
pub struct KeyForm {
    token: String,
}

/// Checks the key with the service before storing it, and stores the account it belongs
/// to. A key the service does not accept is never saved, and the key is never logged or
/// echoed back.
async fn save_key(
    service: Service,
    owner: &'static str,
    state: AppState,
    user: SessionUser,
    pid: i64,
    form: KeyForm,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let back = module_path(pid, owner);
    let Some(token) = service.check_format(&form.token) else {
        return Ok(redirect_msg(&back, code(service, "invalid")));
    };
    let account = match service {
        Service::ClickUp => match state.clickup.user(&token).await {
            Ok(account) => account.display().to_owned(),
            Err(ClickUpError::Unauthorized) => {
                return Ok(redirect_msg(&back, code(service, "rejected")));
            }
            Err(e) => {
                tracing::warn!(project_id = pid, owner, "could not verify ClickUp key: {e}");
                return Ok(redirect_msg(&back, code(service, "unreachable")));
            }
        },
        Service::Discord => match dc::current_user(&state, &token).await {
            Ok(bot) => bot.name,
            Err(DiscordApiError::Unauthorized) => {
                return Ok(redirect_msg(&back, code(service, "rejected")));
            }
            Err(e) => {
                tracing::warn!(
                    project_id = pid,
                    owner,
                    "could not verify Discord token: {e}"
                );
                return Ok(redirect_msg(&back, code(service, "unreachable")));
            }
        },
    };
    super::store(
        &state.db,
        &state.vault,
        pid,
        service,
        owner,
        &token,
        &account,
        &user.username,
    )
    .await?;
    if owner != service.shared_owner() {
        // Saving a separate key means the module should use it.
        super::set_use_shared(&state.db, pid, service, owner, false).await?;
    }
    if service == Service::Discord {
        // A bot in exactly one server needs no choice.
        if connection::server(&state.db, pid, owner).await?.is_none()
            && let Ok(list) = dc::guilds(&state, &token).await
            && let [only] = list.as_slice()
        {
            connection::set_server(&state.db, pid, owner, only.id, &only.name, &user.username)
                .await?;
        }
        DiscordHub::reconcile_soon(&state);
    }
    tracing::info!(user = %user.username, project_id = pid, owner, "{} key saved", service.name());
    Ok(redirect_msg(&back, code(service, "saved")))
}

async fn delete_key(
    service: Service,
    owner: &'static str,
    state: AppState,
    user: SessionUser,
    pid: i64,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    super::delete(&state.db, pid, service, owner).await?;
    if service == Service::Discord {
        DiscordHub::reconcile_soon(&state);
    }
    tracing::info!(user = %user.username, project_id = pid, owner, "{} key removed", service.name());
    Ok(redirect_msg(&module_path(pid, owner), "token_removed"))
}

#[derive(Deserialize)]
pub struct SourceForm {
    source: String,
}

async fn save_source(
    service: Service,
    owner: &'static str,
    state: AppState,
    user: SessionUser,
    pid: i64,
    form: SourceForm,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let use_shared = match form.source.as_str() {
        "shared" => true,
        "own" => false,
        _ => return Err(AppError::BadRequest),
    };
    super::set_use_shared(&state.db, pid, service, owner, use_shared).await?;
    if service == Service::Discord {
        DiscordHub::reconcile_soon(&state);
    }
    tracing::info!(user = %user.username, project_id = pid, owner, use_shared, "{} key source saved", service.name());
    Ok(redirect_msg(&module_path(pid, owner), "source_saved"))
}

#[derive(Deserialize)]
pub struct ServerForm {
    guild_id: String,
}

/// Selects the server of the module's own bot token. Only servers that bot is in are
/// accepted.
async fn save_server(
    owner: &'static str,
    state: AppState,
    user: SessionUser,
    pid: i64,
    form: ServerForm,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let back = module_path(pid, owner);
    let guild_id: u64 = form
        .guild_id
        .trim()
        .parse()
        .map_err(|_| AppError::BadRequest)?;
    let Ok(token) = super::load(&state.db, &state.vault, pid, Service::Discord, owner).await?
    else {
        return Ok(redirect_msg(&back, "discord_token_missing"));
    };
    let mut list = dc::guilds(&state, &token).await;
    if list
        .as_ref()
        .is_ok_and(|l| !l.iter().any(|g| g.id == guild_id))
    {
        // The bot may have joined since the list was cached.
        dc::invalidate_guilds(&state, &token);
        list = dc::guilds(&state, &token).await;
    }
    let list = match list {
        Ok(list) => list,
        Err(DiscordApiError::Unauthorized) => {
            return Ok(redirect_msg(&back, "discord_token_rejected"));
        }
        Err(_) => return Ok(redirect_msg(&back, "discord_unreachable")),
    };
    let Some(guild) = list.iter().find(|g| g.id == guild_id) else {
        return Ok(redirect_msg(&back, "server_invalid"));
    };
    connection::set_server(&state.db, pid, owner, guild.id, &guild.name, &user.username).await?;
    tracing::info!(user = %user.username, project_id = pid, owner, guild_id, "Discord server selected");
    Ok(redirect_msg(&back, "server_saved"))
}

/// Drops the connection of the module's own bot token and connects it again, e.g. after
/// enabling an intent in the developer portal.
async fn reconnect(
    owner: &'static str,
    state: AppState,
    user: SessionUser,
    pid: i64,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let back = module_path(pid, owner);
    let Ok(token) = super::load(&state.db, &state.vault, pid, Service::Discord, owner).await?
    else {
        return Ok(redirect_msg(&back, "discord_token_missing"));
    };
    state.discord.forget(&dc::client_key(&token));
    DiscordHub::reconcile_soon(&state);
    tracing::info!(user = %user.username, project_id = pid, owner, "Discord bot reconnect requested");
    Ok(redirect_msg(&back, "reconnecting"))
}
