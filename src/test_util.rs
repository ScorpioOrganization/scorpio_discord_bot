//! Shared helpers for tests: an in-memory database and a driver for the dashboard router.

use std::net::SocketAddr;
use std::str::FromStr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower::ServiceExt;

use crate::auth;
use crate::config::Config;
use crate::modules;
use crate::state::AppState;

pub const ROOT_PW: &str = "root-password-123";
pub const IP: &str = "127.0.0.1";

/// Fresh migrated database. Every `:memory:` connection is its own database, so the pool
/// holds exactly one connection and never recycles it.
pub async fn pool() -> SqlitePool {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

/// Dashboard only state, like running without DISCORD_TOKEN.
pub async fn state() -> AppState {
    let config = Config {
        discord_token: None,
        database_url: "sqlite::memory:".into(),
        web_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        cookie_secure: false,
        trust_proxy: false,
        root_password: None,
    };
    AppState::new(pool().await, config, modules::registry())
}

pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl TestResponse {
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }

    pub fn location(&self) -> &str {
        self.header(header::LOCATION.as_str())
    }

    /// Asserts a redirect and returns its target.
    pub fn redirect(&self) -> &str {
        assert_eq!(self.status, StatusCode::SEE_OTHER, "body: {}", self.body);
        self.location()
    }
}

/// The dashboard router with a root account, driven without a socket.
pub struct TestApp {
    pub state: AppState,
    router: Router,
}

impl TestApp {
    pub async fn new() -> Self {
        let state = state().await;
        auth::bootstrap_root(&state.db, Some(ROOT_PW))
            .await
            .unwrap();
        let router = crate::web::router(state.clone());
        Self { state, router }
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        form: &[(&str, &str)],
        ip: &str,
    ) -> TestResponse {
        let mut req = Request::builder().method(method.clone()).uri(path);
        if let Some(cookie) = cookie {
            req = req.header(header::COOKIE, cookie);
        }
        let body = if method == Method::GET {
            Body::empty()
        } else {
            req = req.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            Body::from(serde_urlencoded::to_string(form).unwrap())
        };
        let mut req = req.body(body).unwrap();
        let peer = SocketAddr::new(ip.parse().unwrap(), 40000);
        req.extensions_mut().insert(ConnectInfo(peer));

        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        TestResponse {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    pub async fn get(&self, path: &str, cookie: Option<&str>) -> TestResponse {
        self.request(Method::GET, path, cookie, &[], IP).await
    }

    pub async fn post(
        &self,
        path: &str,
        cookie: Option<&str>,
        form: &[(&str, &str)],
    ) -> TestResponse {
        self.request(Method::POST, path, cookie, form, IP).await
    }

    pub async fn login_from(&self, ip: &str, username: &str, password: &str) -> TestResponse {
        let form = [("username", username), ("password", password)];
        self.request(Method::POST, "/login", None, &form, ip).await
    }

    /// Logs in and returns the `name=value` pair to send back as Cookie header.
    pub async fn login(&self, username: &str, password: &str) -> String {
        let res = self.login_from(IP, username, password).await;
        assert_eq!(res.redirect(), "/");
        let set_cookie = res.header(header::SET_COOKIE.as_str());
        let pair = set_cookie.split(';').next().unwrap();
        assert!(pair.starts_with(&format!("{}=", auth::SESSION_COOKIE)));
        pair.to_owned()
    }

    pub async fn login_root(&self) -> String {
        self.login(auth::ROOT_USERNAME, ROOT_PW).await
    }

    /// CSRF token of a session, read from the hidden form field like a browser would.
    pub async fn csrf(&self, cookie: &str) -> String {
        let res = self.get("/account", Some(cookie)).await;
        assert_eq!(res.status, StatusCode::OK);
        let marker = r#"name="_csrf" value=""#;
        let start = res.body.find(marker).expect("csrf field") + marker.len();
        let len = res.body[start..].find('"').unwrap();
        res.body[start..start + len].to_owned()
    }

    /// Creates a dashboard account through the users page.
    pub async fn create_user(&self, root_cookie: &str, username: &str, password: &str) {
        let csrf = self.csrf(root_cookie).await;
        let form = [
            ("_csrf", csrf.as_str()),
            ("username", username),
            ("password", password),
            ("confirm", password),
        ];
        let res = self.post("/users", Some(root_cookie), &form).await;
        assert_eq!(res.redirect(), "/users?msg=user_created");
    }

    pub async fn user_id(&self, username: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM users WHERE username = ?")
            .bind(username.to_owned())
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }
}
