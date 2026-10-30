use askama::Template;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,
    #[error("forbidden")]
    Forbidden,
    #[error("bad request")]
    BadRequest,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<askama::Error> for AppError {
    fn from(e: askama::Error) -> Self {
        Self::Internal(e.into())
    }
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorPage {
    code: u16,
    title: &'static str,
    message: &'static str,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, title, message) = match &self {
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "Not found",
                "This page does not exist, or it was deleted.",
            ),
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "Forbidden",
                "You are not allowed to do that. If you submitted a form, reload the page and try again.",
            ),
            Self::BadRequest => (
                StatusCode::BAD_REQUEST,
                "Bad request",
                "The request could not be understood.",
            ),
            Self::Internal(e) => {
                tracing::error!("internal error: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong",
                    "An internal error occurred. Details were written to the server log.",
                )
            }
        };
        error_page(status, title, message)
    }
}

pub fn error_page(status: StatusCode, title: &'static str, message: &'static str) -> Response {
    let page = ErrorPage {
        code: status.as_u16(),
        title,
        message,
    };
    match page.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(_) => (status, title).into_response(),
    }
}
