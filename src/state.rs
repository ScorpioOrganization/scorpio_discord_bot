use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serenity::all::{Cache, GatewayIntents, Http, ShardManager};
use sqlx::SqlitePool;

use crate::auth::limiter::LoginLimiter;
use crate::config::Config;
use crate::modules::Module;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<Config>,
    pub modules: Arc<Vec<Arc<dyn Module>>>,
    pub stats: Arc<BotStats>,
    pub limiter: Arc<LoginLimiter>,
    /// Set once the serenity client is built. Stays empty when no token is configured.
    pub discord: Arc<OnceLock<DiscordHandle>>,
    /// Why the Discord connection is down, shown on the dashboard. None while healthy.
    bot_error: Arc<Mutex<Option<String>>>,
}

#[derive(Clone)]
pub struct DiscordHandle {
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
    pub shard_manager: Arc<ShardManager>,
    /// Intents the gateway connection was opened with.
    pub intents: GatewayIntents,
}

impl AppState {
    pub fn new(db: SqlitePool, config: Config, modules: Vec<Arc<dyn Module>>) -> Self {
        Self {
            db,
            config: Arc::new(config),
            modules: Arc::new(modules),
            stats: Arc::new(BotStats::default()),
            limiter: Arc::new(LoginLimiter::default()),
            discord: Arc::new(OnceLock::new()),
            bot_error: Arc::new(Mutex::new(None)),
        }
    }

    pub fn discord(&self) -> Option<&DiscordHandle> {
        self.discord.get()
    }

    pub fn module(&self, id: &str) -> Option<&Arc<dyn Module>> {
        self.modules.iter().find(|m| m.id() == id)
    }

    /// Intents that `wanted` asks for but the running connection does not have.
    /// Empty when the bot is not connected at all.
    pub fn missing_intents(&self, wanted: GatewayIntents) -> GatewayIntents {
        self.discord()
            .map(|d| wanted.difference(d.intents))
            .unwrap_or_else(GatewayIntents::empty)
    }

    pub fn bot_error(&self) -> Option<String> {
        self.bot_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_bot_error(&self, error: impl Into<String>) {
        *self.bot_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.into());
    }
}

/// Runtime counters shown on the dashboard. Reset on restart.
pub struct BotStats {
    pub started_at: Instant,
    pub events_handled: AtomicU64,
    pub member_joins: AtomicU64,
}

impl Default for BotStats {
    fn default() -> Self {
        Self {
            started_at: Instant::now(),
            events_handled: AtomicU64::new(0),
            member_joins: AtomicU64::new(0),
        }
    }
}

impl BotStats {
    pub fn record_event(&self) {
        self.events_handled.fetch_add(1, Ordering::Relaxed);
    }
}
