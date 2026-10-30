//! Shared helpers for tests: an in-memory database and a driver for the dashboard router.

use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use serenity::all::{Cache, GatewayIntents, GuildCreateEvent, ShardManager, ShardManagerOptions};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tower::ServiceExt;

use crate::auth;
use crate::clickup::mock::MockClickUp;
use crate::config::Config;
use crate::db::MIGRATOR;
use crate::discord::connection::set_server;
use crate::discord::hub::{DiscordHandle, DiscordHub};
use crate::discord::members;
use crate::discord::mock::{self as discord_mock, MockDiscord};
use crate::keys::{self, Service};
use crate::modules::{self, Module};
use crate::projects;
use crate::secrets::{SecretString, Vault};
use crate::state::AppState;

pub const ROOT_PW: &str = "root-password-123";
pub const IP: &str = "127.0.0.1";
/// Bot tokens the fake Discord API accepts. Pass `Service::check_format`, but are not
/// shaped like real ones, so secret scanners (GitHub push protection) leave them alone.
pub const DISCORD_TOKEN: &str =
    "fake-discord-bot-token-one.for-tests-only.not-a-real-secret-000001";
pub const OTHER_DISCORD_TOKEN: &str =
    "fake-discord-bot-token-two.for-tests-only.not-a-real-secret-000002";
pub const PROJECT_NAME: &str = "Test project";
/// Project id unit tests use. In a fresh database the project of a `TestApp` gets it too.
pub const PID: i64 = 1;
/// The guild the fake Discord connection knows about. Above `i64::MAX` on purpose.
pub const GUILD_ID: u64 = 9_300_000_000_000_000_001;
pub const GUILD_NAME: &str = "Scorpio Test";
pub const CHANNEL_ID: u64 = 9_300_000_000_000_000_002;
pub const OTHER_CHANNEL_ID: u64 = 9_300_000_000_000_000_003;
pub const VOICE_CHANNEL_ID: u64 = 9_300_000_000_000_000_004;
pub const EMOJI_ID: u64 = 9_300_000_000_000_000_005;
pub const MEMBER_ID: u64 = 9_300_000_000_000_000_010;
/// A mentionable role.
pub const ROLE_ID: u64 = 9_300_000_000_000_000_020;
/// A role that is not mentionable.
pub const LEADS_ROLE_ID: u64 = 9_300_000_000_000_000_021;

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
    MIGRATOR.run(&pool).await.unwrap();
    pool
}

/// Creates projects with these names and returns their ids.
pub async fn projects<const N: usize>(db: &SqlitePool, names: [&str; N]) -> [i64; N] {
    let mut ids = [0; N];
    for (id, name) in ids.iter_mut().zip(names) {
        *id = projects::create(db, name, "root").await.unwrap().unwrap();
    }
    ids
}

/// One row in every table that belongs to a project, plus a sent reminder marker.
pub async fn fill_project(db: &SqlitePool, pid: i64) {
    let statements = [
        "INSERT INTO project_modules (project_id, module_id, enabled) VALUES (?1, 'greeting', 1)",
        "INSERT INTO module_stats (project_id, module_id, key, value) VALUES (?1, 'greeting', 'sent', 3)",
        "INSERT INTO api_keys (project_id, service, owner, sealed, hint, account, updated_at)
         VALUES (?1, 'clickup', 'clickup', x'00', 'abcd', 'u', 0)",
        "INSERT INTO api_key_source (project_id, service, owner, use_shared)
         VALUES (?1, 'clickup', 'calendar', 0)",
        "INSERT INTO discord_servers (project_id, owner, guild_id, guild_name, updated_at)
         VALUES (?1, 'discord', 5, 'S', 0)",
        "INSERT INTO greeting_config (project_id, template, updated_at) VALUES (?1, 'Hi', 0)",
        "INSERT INTO greeting_log (project_id, user_id, user_name, success, created_at)
         VALUES (?1, 1, 'u', 1, 0)",
        "INSERT INTO clickup_config (project_id, updated_at) VALUES (?1, 0)",
        "INSERT INTO clickup_user_map (project_id, clickup_user_id, clickup_name, updated_at)
         VALUES (?1, 1, 'u', 0)",
        "INSERT INTO clickup_tag_roles (project_id, tag, role_id, updated_at) VALUES (?1, 't', 1, 0)",
        "INSERT INTO calendar_config (project_id, updated_at) VALUES (?1, 0)",
        "INSERT INTO calendar_rules (project_id, name, kind, header_template, item_template,
                                     empty_template, created_at, updated_at)
         VALUES (?1, 'r', 'reminder', '', '', '', 0, 0)",
        "INSERT INTO calendar_sent (rule_id, task_id, event_at, sent_at)
         VALUES ((SELECT MAX(id) FROM calendar_rules WHERE project_id = ?1), 't', 0, 0)",
        "INSERT INTO calendar_log (project_id, rule_name, kind, success, created_at)
         VALUES (?1, 'r', 'digest', 1, 0)",
        "INSERT INTO recurring_config (project_id, updated_at) VALUES (?1, 0)",
        "INSERT INTO recurring_rules (project_id, name, tag, created_at, updated_at)
         VALUES (?1, 'r', 't', 0, 0)",
        "INSERT INTO recurring_done (project_id, task_id, due_ms, done_at) VALUES (?1, 't', 0, 0)",
        "INSERT INTO recurring_log (project_id, rule_name, task_id, task_name, old_due, new_due,
                                    mode, success, created_at)
         VALUES (?1, 'r', 't', 't', 0, 0, 'update', 1, 0)",
    ];
    for sql in statements {
        sqlx::query(sql).bind(pid).execute(db).await.unwrap();
    }
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

/// State without any Discord bot. Its hub never connects for real.
pub async fn state() -> AppState {
    state_with(modules::registry()).await
}

pub async fn state_with(modules: Vec<Arc<dyn Module>>) -> AppState {
    let mut state = AppState::new(pool().await, config(), modules, Vault::ephemeral());
    // Calls that reach Discord fail fast on a closed local port instead of reaching
    // discord.com. `TestApp` points them at a `MockDiscord` instead.
    state.discord = Arc::new(DiscordHub::new(Some("http://127.0.0.1:9".into()), false));
    state
}

/// Gives a project the Discord module (enabled) with [`DISCORD_TOKEN`] working in
/// [`GUILD_ID`], and pretends that bot is connected and member of that one server with two
/// text channels, a voice channel, a custom emoji and one member. The cache is filled from a
/// GUILD_CREATE payload and the shard manager never starts a shard. HTTP calls go wherever
/// the hub's proxy points: a closed port, or the `MockDiscord` of a `TestApp`.
pub async fn connect_fake_discord(state: &AppState, pid: i64) {
    let token = SecretString::new(DISCORD_TOKEN);
    let db = &state.db;
    if keys::info(db, pid, Service::Discord, modules::discord::ID)
        .await
        .unwrap()
        .is_none()
    {
        keys::store(
            db,
            &state.vault,
            pid,
            Service::Discord,
            modules::discord::ID,
            &token,
            discord_mock::BOT_NAME,
            "root",
        )
        .await
        .unwrap();
    }
    set_server(db, pid, modules::discord::ID, GUILD_ID, GUILD_NAME, "root")
        .await
        .unwrap();
    modules::set_enabled(db, pid, modules::discord::ID, true)
        .await
        .unwrap();

    let cache = Arc::new(Cache::new());
    let mut event: GuildCreateEvent =
        serde_json::from_value(guild_json()).expect("guild fixture matches serenity's model");
    cache.update(&mut event);

    let http = Arc::new(crate::discord::http(state.discord.proxy(), &token));
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
    let key = crate::discord::client_key(&token);
    let handle = DiscordHandle::new(http, cache, shard_manager, GatewayIntents::all());
    state.discord.insert_running(&key, handle);
    state.discord.mark_ready(&key);
}

fn guild_json() -> serde_json::Value {
    let channel = |id: u64, kind: u8, name: &str, position: u16| {
        serde_json::json!({
            "id": id.to_string(), "type": kind, "name": name, "position": position,
            "guild_id": GUILD_ID.to_string(), "permission_overwrites": [], "nsfw": false
        })
    };
    let role = |id: u64, name: &str, mentionable: bool, position: u16| {
        serde_json::json!({
            "id": id.to_string(), "name": name, "color": 0, "hoist": false,
            "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
            "managed": false, "mentionable": mentionable, "permissions": "0",
            "position": position, "flags": 0
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
        "roles": [
            role(GUILD_ID, "@everyone", false, 0),
            role(ROLE_ID, "Team", true, 2),
            role(LEADS_ROLE_ID, "Leads", false, 1),
        ],
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

/// The dashboard router with a root account and one project ([`PROJECT_NAME`], id in
/// `pid`), driven without a socket.
pub struct TestApp {
    pub state: AppState,
    /// Fake ClickUp API the app talks to.
    pub clickup: MockClickUp,
    /// Fake Discord HTTP API the app talks to.
    pub discord: MockDiscord,
    /// The project every module test uses.
    pub pid: i64,
    router: Router,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::from_state(state().await).await
    }

    /// Like [`new`](Self::new), with the project's Discord module connected to the fake
    /// server [`GUILD_ID`], see [`connect_fake_discord`].
    pub async fn with_guild() -> Self {
        let app = Self::new().await;
        connect_fake_discord(&app.state, app.pid).await;
        app
    }

    pub async fn from_state(mut state: AppState) -> Self {
        let clickup = MockClickUp::start().await;
        state.clickup = clickup.client();
        let discord = MockDiscord::start().await;
        state.discord = Arc::new(DiscordHub::new(Some(discord.url.clone()), false));
        auth::bootstrap_root(&state.db, Some(ROOT_PW))
            .await
            .unwrap();
        let pid = match projects::list(&state.db).await.unwrap().first() {
            Some(p) => p.id,
            None => projects::create(&state.db, PROJECT_NAME, "root")
                .await
                .unwrap()
                .unwrap(),
        };
        let router = crate::web::router(state.clone());
        Self {
            state,
            clickup,
            discord,
            pid,
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
