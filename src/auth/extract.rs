use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;

use super::{SESSION_COOKIE, SessionUser, ct_eq, load_session};
use crate::error::AppError;
use crate::state::AppState;

/// Upper bound for buffered form bodies. Dashboard forms are tiny.
const MAX_FORM_BYTES: usize = 64 * 1024;

/// Loads the session for every request and enforces CSRF tokens on every state changing
/// request. The only exception is the login form, which has no session yet.
pub async fn session_middleware(
    State(state): State<AppState>,
    jar: CookieJar,
    mut req: Request,
    next: Next,
) -> Response {
    let session = match jar.get(SESSION_COOKIE) {
        Some(cookie) => match load_session(&state.db, cookie.value()).await {
            Ok(s) => s,
            Err(e) => return AppError::Internal(e).into_response(),
        },
        None => None,
    };

    let safe = matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe && req.uri().path() != "/login" {
        let Some(user) = &session else {
            return Redirect::to("/login").into_response();
        };
        let (parts, body) = req.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, MAX_FORM_BYTES).await else {
            return AppError::BadRequest.into_response();
        };
        let token = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&bytes)
            .ok()
            .and_then(|fields| fields.into_iter().find(|(k, _)| k == "_csrf"))
            .map(|(_, v)| v);
        if !token.is_some_and(|t| ct_eq(&t, &user.csrf)) {
            tracing::warn!(user = %user.username, path = %parts.uri.path(), "rejected request with bad CSRF token");
            return AppError::Forbidden.into_response();
        }
        req = Request::from_parts(parts, Body::from(bytes));
    }

    if let Some(user) = session {
        req.extensions_mut().insert(user);
    }
    next.run(req).await
}

/// Any logged in user. Redirects to the login page otherwise.
pub struct CurrentUser(pub SessionUser);

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<SessionUser>()
            .cloned()
            .map(CurrentUser)
            .ok_or_else(|| Redirect::to("/login").into_response())
    }
}

/// Logged in root user. Other users get 403.
pub struct RootUser(pub SessionUser);

impl<S: Send + Sync> FromRequestParts<S> for RootUser {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let CurrentUser(user) = CurrentUser::from_request_parts(parts, state).await?;
        if user.is_root {
            Ok(RootUser(user))
        } else {
            Err(AppError::Forbidden.into_response())
        }
    }
}

/// Client address used for login throttling. Proxy headers are only trusted when enabled.
/// The last X-Forwarded-For entry is used: it is the one appended by our own reverse proxy,
/// earlier entries can be forged by the client.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr, trust_proxy: bool) -> String {
    if trust_proxy
        && let Some(first) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    {
        return first.to_owned();
    }
    peer.ip().to_string()
}

impl<S: Send + Sync> axum::extract::OptionalFromRequestParts<S> for CurrentUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Option<Self>, Self::Rejection> {
        Ok(parts
            .extensions
            .get::<SessionUser>()
            .cloned()
            .map(CurrentUser))
    }
}
