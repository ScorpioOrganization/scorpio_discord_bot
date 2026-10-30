use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use sqlx::SqlitePool;

use crate::auth::limiter::LoginLimiter;
use crate::cache::TtlCache;
use crate::clickup::ClickUpClient;
use crate::config::Config;
use crate::discord::hub::DiscordHub;
use crate::discord::members::MemberCache;
use crate::modules::Module;
use crate::secrets::Vault;

#[derive(Clone)]
pub struct AppState {
    pub db: SqlitePool,
    pub config: Arc<Config>,
    pub modules: Arc<Vec<Arc<dyn Module>>>,
    pub stats: Arc<BotStats>,
    pub limiter: Arc<LoginLimiter>,
    /// Encrypts API keys stored in the database.
    pub vault: Arc<Vault>,
    pub clickup: ClickUpClient,
    /// Running Discord bots, one per bot token in use.
    pub discord: Arc<DiscordHub>,
    /// Recently fetched Discord server member lists.
    pub members: Arc<MemberCache>,
    /// Module data fetched from other services, e.g. ClickUp tasks.
    pub cache: Arc<TtlCache>,
}

impl AppState {
    pub fn new(
        db: SqlitePool,
        config: Config,
        modules: Vec<Arc<dyn Module>>,
        vault: Vault,
    ) -> Self {
        Self {
            db,
            config: Arc::new(config),
            modules: Arc::new(modules),
            stats: Arc::new(BotStats::default()),
            limiter: Arc::new(LoginLimiter::default()),
            vault: Arc::new(vault),
            clickup: ClickUpClient::default(),
            discord: Arc::new(DiscordHub::default()),
            members: Arc::new(MemberCache::default()),
            cache: Arc::new(TtlCache::default()),
        }
    }

    pub fn module(&self, id: &str) -> Option<&Arc<dyn Module>> {
        self.modules.iter().find(|m| m.id() == id)
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
