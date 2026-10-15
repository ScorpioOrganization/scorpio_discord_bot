//! Dashboard pieces for ClickUp API keys, shared by every module that reads ClickUp.
//!
//! A module adds [`token_routes`] to its router, puts a [`TokenView`] named `token` into its
//! page struct and includes `templates/clickup/token_card.html`.

use axum::Form;
use axum::Router;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::post;
use serde::Deserialize;

use super::ClickUpError;
use super::tokens::{self, SHARED};
use crate::auth::SessionUser;
use crate::auth::extract::CurrentUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::web::{fmt_ts, redirect_msg, require_guild};

/// Key card data. Never contains the key itself.
pub struct TokenView {
    /// Form target, `/guilds/{gid}/modules/<owner>/token`.
    pub action: String,
    /// The module may use the shared key instead of its own.
    pub shared_option: bool,
    pub use_shared: bool,
    pub own: Option<KeyStatus>,
    pub shared: Option<KeyStatus>,
    /// Why the module cannot use ClickUp right now.
    pub problem: Option<&'static str>,
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

async fn key_status(
    state: &AppState,
    guild_id: u64,
    owner: &str,
) -> anyhow::Result<Option<KeyStatus>> {
    let Some(info) = tokens::info(&state.db, guild_id, owner).await? else {
        return Ok(None);
    };
    let readable = tokens::load(&state.db, &state.vault, guild_id, owner)
        .await?
        .is_ok();
    Ok(Some(KeyStatus {
        account: info.clickup_user,
        hint: info.hint,
        readable,
        updated_at: info.updated_at,
        updated_by: info.updated_by.unwrap_or_default(),
        last_ok_at: info.last_ok_at,
        last_error: info.last_error,
        last_error_at: info.last_error_at,
    }))
}

pub async fn token_view(state: &AppState, guild_id: u64, owner: &str) -> anyhow::Result<TokenView> {
    let shared_option = owner != SHARED;
    let shared = if shared_option {
        key_status(state, guild_id, SHARED).await?
    } else {
        None
    };
    Ok(TokenView {
        action: format!("{}/token", module_path(guild_id, owner)),
        shared_option,
        use_shared: tokens::use_shared(&state.db, guild_id, owner).await?,
        own: key_status(state, guild_id, owner).await?,
        shared,
        problem: tokens::resolve(state, guild_id, owner)
            .await?
            .err()
            .map(|p| p.message()),
    })
}

fn module_path(guild_id: u64, owner: &str) -> String {
    format!("/guilds/{guild_id}/modules/{owner}")
}

/// `POST .../token` (save), `.../token/delete` and, for modules other than the ClickUp
/// module, `.../token/source` (shared or own key).
pub fn token_routes(owner: &'static str) -> Router<AppState> {
    let base = format!("/guilds/{{gid}}/modules/{owner}/token");
    let mut router = Router::new()
        .route(
            &base,
            post(
                move |State(state): State<AppState>,
                      CurrentUser(user): CurrentUser,
                      Path(gid): Path<u64>,
                      Form(form): Form<TokenForm>| {
                    save_token(owner, state, user, gid, form)
                },
            ),
        )
        .route(
            &format!("{base}/delete"),
            post(
                move |State(state): State<AppState>,
                      CurrentUser(user): CurrentUser,
                      Path(gid): Path<u64>| {
                    delete_token(owner, state, user, gid)
                },
            ),
        );
    if owner != SHARED {
        router = router.route(
            &format!("{base}/source"),
            post(
                move |State(state): State<AppState>,
                      CurrentUser(user): CurrentUser,
                      Path(gid): Path<u64>,
                      Form(form): Form<SourceForm>| {
                    save_source(owner, state, user, gid, form)
                },
            ),
        );
    }
    router
}

#[derive(Deserialize)]
pub struct TokenForm {
    token: String,
}

/// Checks the key with ClickUp before storing it. A key ClickUp does not accept is never
/// saved, and the key is never logged or echoed back.
async fn save_token(
    owner: &'static str,
    state: AppState,
    user: SessionUser,
    gid: u64,
    form: TokenForm,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let back = module_path(gid, owner);
    let Some(token) = tokens::check_format(&form.token) else {
        return Ok(redirect_msg(&back, "token_invalid"));
    };
    let account = match state.clickup.user(&token).await {
        Ok(account) => account.display().to_owned(),
        Err(ClickUpError::Unauthorized) => return Ok(redirect_msg(&back, "token_rejected")),
        Err(e) => {
            tracing::warn!(guild_id = gid, owner, "could not verify ClickUp key: {e}");
            return Ok(redirect_msg(&back, "clickup_unreachable"));
        }
    };
    tokens::store(
        &state.db,
        &state.vault,
        gid,
        owner,
        &token,
        &account,
        &user.username,
    )
    .await?;
    if owner != SHARED {
        // Saving a separate key means the module should use it.
        tokens::set_use_shared(&state.db, gid, owner, false).await?;
    }
    tracing::info!(user = %user.username, guild_id = gid, owner, "ClickUp API key saved");
    Ok(redirect_msg(&back, "token_saved"))
}

async fn delete_token(
    owner: &'static str,
    state: AppState,
    user: SessionUser,
    gid: u64,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    tokens::delete(&state.db, gid, owner).await?;
    tracing::info!(user = %user.username, guild_id = gid, owner, "ClickUp API key removed");
    Ok(redirect_msg(&module_path(gid, owner), "token_removed"))
}

#[derive(Deserialize)]
pub struct SourceForm {
    source: String,
}

async fn save_source(
    owner: &'static str,
    state: AppState,
    user: SessionUser,
    gid: u64,
    form: SourceForm,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let use_shared = match form.source.as_str() {
        "shared" => true,
        "own" => false,
        _ => return Err(AppError::BadRequest),
    };
    tokens::set_use_shared(&state.db, gid, owner, use_shared).await?;
    tracing::info!(user = %user.username, guild_id = gid, owner, use_shared, "ClickUp key source saved");
    Ok(redirect_msg(&module_path(gid, owner), "source_saved"))
}
