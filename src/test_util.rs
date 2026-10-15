//! Shared helpers for tests: an in-memory database and a driver for the dashboard router.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use serenity::all::{
    Cache, GatewayIntents, GuildCreateEvent, HttpBuilder, ShardManager, ShardManagerOptions,
};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower::ServiceExt;

use crate::auth;
use crate::bot::members;
use crate::clickup::mock::MockClickUp;
use crate::config::Config;
use crate::modules::{self, Module};
use crate::secrets::Vault;
use crate::state::{AppState, DiscordHandle};

pub const ROOT_PW: &str = "root-password-123";
pub const IP: &str = "127.0.0.1";
/// The guild the fake Discord connection knows about. Above `i64::MAX` on purpose.
pub const GUILD_ID: u64 = 9_300_000_000_000_000_001;
pub const GUILD_NAME: &str = "Scorpio Test";
pub const CHANNEL_ID: u64 = 9_300_000_000_000_000_002;
pub const OTHER_CHANNEL_ID: u64 = 9_300_000_000_000_000_003;
pub const VOICE_CHANNEL_ID: u64 = 9_300_000_000_000_000_004;
pub const EMOJI_ID: u64 = 9_300_000_000_000_000_005;
pub const MEMBER_ID: u64 = 9_300_000_000_000_000_010;

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

pub fn config() -> Config {
    Config {
        discord_token: None,
        database_url: "sqlite::memory:".into(),
        web_bind: SocketAddr::from(([127, 0, 0, 1], 0)),
        cookie_secure: false,
        trust_proxy: false,
        root_password: None,
        secrets_key: None,
    }
}

/// Dashboard only state, like running without DISCORD_TOKEN.
pub async fn state() -> AppState {
    state_with(modules::registry()).await
}

pub async fn state_with(modules: Vec<Arc<dyn Module>>) -> AppState {
    AppState::new(pool().await, config(), modules, Vault::ephemeral())
}

/// Pretends the bot is connected and member of one guild ([`GUILD_ID`]) with two text
/// channels, a voice channel, a custom emoji and one member. Nothing talks to Discord: the
/// cache is filled from a GUILD_CREATE payload and the shard manager never starts a shard.
/// Any handler that calls the Discord HTTP API would fail, so tests avoid those paths.
pub fn connect_fake_discord(state: &AppState) {
    let cache = Arc::new(Cache::new());
    let mut event: GuildCreateEvent =
        serde_json::from_value(guild_json()).expect("guild fixture matches serenity's model");
    cache.update(&mut event);

    // Point the HTTP client at a closed local port: a handler that calls Discord by
    // mistake fails fast instead of reaching discord.com.
    let http = Arc::new(
        HttpBuilder::new("test-token")
            .proxy("http://127.0.0.1:9")
            .ratelimiter_disabled(true)
            .build(),
    );
    let (shard_manager, _) = ShardManager::new(ShardManagerOptions {
        data: Default::default(),
        event_handlers: Vec::new(),
        raw_event_handlers: Vec::new(),
        shard_index: 0,
        shard_init: 0,
        shard_total: 1,
        ws_url: Default::default(),
        cache: cache.clone(),
        http: http.clone(),
        intents: GatewayIntents::all(),
        presence: None,
    });
    // Member lists come from HTTP in production; serve the fixture from the cache instead.
    state
        .members
        .insert(GUILD_ID, members::from_cache(&cache, GUILD_ID));
    let handle = DiscordHandle {
        http,
        cache,
        shard_manager,
        intents: GatewayIntents::all(),
    };
    assert!(state.discord.set(handle).is_ok(), "already connected");
}

fn guild_json() -> serde_json::Value {
    let channel = |id: u64, kind: u8, name: &str, position: u16| {
        serde_json::json!({
            "id": id.to_string(), "type": kind, "name": name, "position": position,
            "guild_id": GUILD_ID.to_string(), "permission_overwrites": [], "nsfw": false
        })
    };
    serde_json::json!({
        "id": GUILD_ID.to_string(),
        "name": GUILD_NAME,
        "icon": null,
        "owner_id": "1",
        "default_message_notifications": 0,
        "explicit_content_filter": 0,
        "verification_level": 0,
        "mfa_level": 0,
        "nsfw_level": 0,
        "premium_tier": 0,
        "system_channel_flags": 0,
        "preferred_locale": "en-US",
        "features": [],
        "premium_progress_bar_enabled": false,
        "roles": [{
            "id": GUILD_ID.to_string(), "name": "@everyone", "color": 0, "hoist": false,
            "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
            "managed": false, "mentionable": false, "permissions": "0", "position": 0, "flags": 0
        }],
        "emojis": [{
            "id": EMOJI_ID.to_string(), "name": "party", "animated": false, "available": true,
            "managed": false, "require_colons": true, "roles": []
        }],
        "stickers": [],
        "joined_at": "2026-01-01T00:00:00.000000+00:00",
        "large": false,
        "unavailable": false,
        "member_count": 2,
        "voice_states": [],
        "members": [{
            "user": {"id": MEMBER_ID.to_string(), "username": "anna.nowak", "global_name": "Anna Nowak", "discriminator": "0", "avatar": null},
            "nick": null, "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
            "deaf": false, "mute": false, "flags": 0
        }],
        "channels": [
            channel(CHANNEL_ID, 0, "events", 0),
            channel(OTHER_CHANNEL_ID, 0, "general", 1),
            channel(VOICE_CHANNEL_ID, 2, "Voice", 2),
        ],
        "threads": [],
        "presences": [],
        "stage_instances": [],
        "guild_scheduled_events": []
    })
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
    /// Fake ClickUp API the app talks to.
    pub clickup: MockClickUp,
    router: Router,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::from_state(state().await).await
    }

    /// Like [`new`](Self::new), with the bot connected to the fake guild [`GUILD_ID`].
    pub async fn with_guild() -> Self {
        let app = Self::new().await;
        connect_fake_discord(&app.state);
        app
    }

    async fn from_state(mut state: AppState) -> Self {
        let clickup = MockClickUp::start().await;
        state.clickup = clickup.client();
        auth::bootstrap_root(&state.db, Some(ROOT_PW))
            .await
            .unwrap();
        let router = crate::web::router(state.clone());
        Self {
            state,
            clickup,
            router,
        }
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
