//! In-process fake of the Discord HTTP API for tests. Serenity's `HttpBuilder::proxy` sends
//! every request here instead of `https://discord.com`. Knows two bot tokens, lists the
//! servers of the first one, accepts messages and reactions and records every request.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::test_util::{DISCORD_TOKEN, GUILD_ID, GUILD_NAME, OTHER_DISCORD_TOKEN};

pub const BOT_ID: u64 = 9_300_000_000_000_000_100;
pub const BOT_NAME: &str = "Scorpio Test Bot";
pub const OTHER_BOT_ID: u64 = 9_300_000_000_000_000_101;
pub const OTHER_BOT_NAME: &str = "Other Bot";
pub const OTHER_GUILD_ID: u64 = 9_300_000_000_000_000_200;
pub const OTHER_GUILD_NAME: &str = "Zeta Server";

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub auth: Option<String>,
    pub body: String,
}

impl Recorded {
    /// The request body as JSON, Null when there is none.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Default)]
struct Inner {
    requests: Vec<Recorded>,
    fail: Option<u16>,
    sent: u64,
}

#[derive(Clone)]
pub struct MockDiscord {
    pub url: String,
    inner: Arc<Mutex<Inner>>,
}

impl MockDiscord {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner::default()));
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url: format!("http://{addr}"),
            inner,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.lock().requests.clone()
    }

    /// Messages posted, as recorded requests.
    pub fn messages(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/messages"))
            .collect()
    }

    /// Makes every request fail with this status until reset with None.
    pub fn fail_with(&self, status: Option<u16>) {
        self.lock().fail = status;
    }
}

fn bot_of(auth: Option<&str>) -> Option<(u64, &'static str)> {
    match auth?.strip_prefix("Bot ")? {
        t if t == DISCORD_TOKEN => Some((BOT_ID, BOT_NAME)),
        t if t == OTHER_DISCORD_TOKEN => Some((OTHER_BOT_ID, OTHER_BOT_NAME)),
        _ => None,
    }
}

fn user(id: u64, name: &str) -> Value {
    json!({
        "id": id.to_string(), "username": name, "global_name": null, "discriminator": "0",
        "avatar": null, "bot": true
    })
}

fn error(status: StatusCode) -> Response {
    let body = json!({"message": "mock error", "code": 0});
    (status, axum::Json(body)).into_response()
}

async fn handle(
    State(inner): State<Arc<Mutex<Inner>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let path = uri.path().trim_start_matches("/api/v10").to_owned();
    let (fail, n) = {
        let mut inner = inner.lock().unwrap();
        inner.requests.push(Recorded {
            method: method.to_string(),
            path: path.clone(),
            auth: auth.clone(),
            body: String::from_utf8_lossy(&body).into_owned(),
        });
        if method == Method::POST {
            inner.sent += 1;
        }
        (inner.fail, inner.sent)
    };
    if let Some(status) = fail {
        return error(StatusCode::from_u16(status).unwrap());
    }
    let Some((bot_id, bot_name)) = bot_of(auth.as_deref()) else {
        return error(StatusCode::UNAUTHORIZED);
    };

    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let json = match (method.as_str(), segments.as_slice()) {
        ("GET", ["users", "@me"]) => user(bot_id, bot_name),
        ("GET", ["users", "@me", "guilds"]) => {
            let guild = |id: u64, name: &str| {
                json!({"id": id.to_string(), "name": name, "icon": null, "owner": false,
                       "permissions": "0", "features": []})
            };
            if bot_id == BOT_ID {
                json!([
                    guild(OTHER_GUILD_ID, OTHER_GUILD_NAME),
                    guild(GUILD_ID, GUILD_NAME)
                ])
            } else {
                json!([guild(GUILD_ID, GUILD_NAME)])
            }
        }
        ("POST", ["channels", channel, "messages"]) => {
            let sent: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            json!({
                "id": (1_000 + n).to_string(),
                "channel_id": channel,
                "author": user(bot_id, bot_name),
                "content": sent.get("content").cloned().unwrap_or(json!("")),
                "timestamp": "2026-01-01T00:00:00.000000+00:00",
                "edited_timestamp": null,
                "tts": false,
                "mention_everyone": false,
                "mentions": [],
                "mention_roles": [],
                "attachments": [],
                "embeds": [],
                "pinned": false,
                "type": 0
            })
        }
        ("PUT", ["channels", _, "messages", _, "reactions", _, "@me"]) => {
            return StatusCode::NO_CONTENT.into_response();
        }
        _ => return error(StatusCode::NOT_FOUND),
    };
    axum::Json(json).into_response()
}
