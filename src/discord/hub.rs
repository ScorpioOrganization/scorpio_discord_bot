//! Running Discord bot connections: one gateway client per distinct bot token that an
//! enabled module uses. [`DiscordHub::reconcile`] compares the tokens in use with the
//! running clients and starts or stops clients to match.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serenity::all::{Cache, GatewayIntents, Http, ShardManager};

use super::{StartError, TOKEN_REJECTED, build_client, client_key, describe_stop};
use crate::keys::{self, Service};
use crate::modules::{self, discord::ID as DISCORD};
use crate::projects;
use crate::secrets::SecretString;
use crate::state::AppState;

/// Most bots connected at once, so a pile of different tokens cannot exhaust the host.
pub const MAX_CLIENTS: usize = 10;
/// Failed connections are tried again after this long, unless the token was rejected.
pub const RETRY_AFTER: Duration = Duration::from_secs(15 * 60);
/// How often the connections are compared with the settings.
pub const RECONCILE_EVERY: Duration = Duration::from_secs(60);
pub const TOO_MANY: &str = "Not connected: at most 10 different Discord bots can be connected at \
                            once. Use fewer bot tokens.";

/// What dashboard pages and modules use of a running client.
#[derive(Clone)]
pub struct DiscordHandle {
    pub http: Arc<Http>,
    pub cache: Arc<Cache>,
    pub shard_manager: Arc<ShardManager>,
    /// Intents the gateway connection was opened with.
    pub intents: GatewayIntents,
    /// Set once the server list in the cache is complete.
    ready: Arc<AtomicBool>,
}

impl DiscordHandle {
    pub fn new(
        http: Arc<Http>,
        cache: Arc<Cache>,
        shard_manager: Arc<ShardManager>,
        intents: GatewayIntents,
    ) -> Self {
        Self {
            http,
            cache,
            shard_manager,
            intents,
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub enum Status {
    Running(DiscordHandle),
    Failed {
        error: String,
        /// None: not before the token changes or someone presses Reconnect.
        retry_at: Option<Instant>,
    },
}

struct Entry {
    /// Distinguishes a client from an older one with the same token.
    id: u64,
    status: Status,
}

/// Snapshot of a client for planning, see [`plan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    Running,
    Failed { retry_at: Option<Instant> },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub start: Vec<String>,
    pub stop: Vec<String>,
    /// Wanted but over [`MAX_CLIENTS`].
    pub over_limit: Vec<String>,
}

/// Decides which clients to start and stop. `desired` is in priority order; running clients
/// keep their slot so a new token never disconnects a working bot.
pub fn plan(known: &HashMap<String, Known>, desired: &[String], now: Instant, max: usize) -> Plan {
    let wanted: HashSet<&String> = desired.iter().collect();
    let mut stop: Vec<String> = known
        .keys()
        .filter(|k| !wanted.contains(k))
        .cloned()
        .collect();
    stop.sort();

    let mut allowed: Vec<&String> = desired
        .iter()
        .filter(|k| known.get(*k) == Some(&Known::Running))
        .collect();
    for key in desired {
        if allowed.len() >= max {
            break;
        }
        if !allowed.contains(&key) {
            allowed.push(key);
        }
    }
    let start = allowed
        .iter()
        .filter(|k| match known.get(**k) {
            None => true,
            Some(Known::Running) => false,
            Some(Known::Failed { retry_at }) => retry_at.is_some_and(|at| at <= now),
        })
        .map(|k| (*k).clone())
        .collect();
    let over_limit = desired
        .iter()
        .filter(|k| !allowed.contains(k))
        .cloned()
        .collect();
    Plan {
        start,
        stop,
        over_limit,
    }
}

/// Tokens that should be connected, in project order without duplicates: the Discord
/// module's token where that module is enabled, and the token every enabled module that uses
/// Discord resolves to.
pub async fn desired(state: &AppState) -> Result<Vec<(String, SecretString)>> {
    let mut out: Vec<(String, SecretString)> = Vec::new();
    let mut push = |token: SecretString| {
        let key = client_key(&token);
        if !out.iter().any(|(k, _)| *k == key) {
            out.push((key, token));
        }
    };
    for pid in projects::ids(&state.db).await? {
        for module in state.modules.iter() {
            let uses_discord =
                module.id() == DISCORD || module.services().contains(&Service::Discord);
            if !uses_discord || !modules::is_enabled(&state.db, pid, module.id()).await? {
                continue;
            }
            if let Ok(cred) = keys::resolve(state, pid, Service::Discord, module.id()).await? {
                push(cred.token);
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// Clients started, or that would have been started when the hub is not live.
    pub started: Vec<String>,
    pub stopped: Vec<String>,
}

pub struct DiscordHub {
    clients: RwLock<HashMap<String, Entry>>,
    next_id: AtomicU64,
    reconcile_lock: tokio::sync::Mutex<()>,
    /// Replaces `https://discord.com` in HTTP calls, for tests.
    proxy: Option<String>,
    /// Whether clients really connect. Off in tests, which insert fake clients instead.
    live: bool,
}

impl Default for DiscordHub {
    fn default() -> Self {
        Self::new(None, true)
    }
}

impl DiscordHub {
    pub fn new(proxy: Option<String>, live: bool) -> Self {
        Self {
            clients: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            reconcile_lock: tokio::sync::Mutex::new(()),
            proxy,
            live,
        }
    }

    pub fn proxy(&self) -> Option<&str> {
        self.proxy.as_deref()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Entry>> {
        self.clients.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Entry>> {
        self.clients.write().unwrap_or_else(|e| e.into_inner())
    }

    pub fn status(&self, key: &str) -> Option<Status> {
        self.read().get(key).map(|e| e.status.clone())
    }

    /// The running client of a token.
    #[cfg(test)]
    pub fn handle(&self, key: &str) -> Option<DiscordHandle> {
        match self.status(key)? {
            Status::Running(h) => Some(h),
            Status::Failed { .. } => None,
        }
    }

    /// Every client, running or failed, sorted by key.
    pub fn all(&self) -> Vec<(String, Status)> {
        let mut list: Vec<_> = self
            .read()
            .iter()
            .map(|(k, e)| (k.clone(), e.status.clone()))
            .collect();
        list.sort_by(|a, b| a.0.cmp(&b.0));
        list
    }

    #[cfg(test)]
    pub fn running_count(&self) -> usize {
        self.read()
            .values()
            .filter(|e| matches!(e.status, Status::Running(_)))
            .count()
    }

    /// Registers a running client and returns its id. Replaces (and stops) an older one.
    pub fn insert_running(&self, key: &str, handle: DiscordHandle) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let old = self.write().insert(
            key.to_owned(),
            Entry {
                id,
                status: Status::Running(handle),
            },
        );
        stop_entry(old);
        id
    }

    fn insert_failed(&self, key: &str, error: String, retry_at: Option<Instant>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let old = self.write().insert(
            key.to_owned(),
            Entry {
                id,
                status: Status::Failed { error, retry_at },
            },
        );
        stop_entry(old);
    }

    /// Records that client `id` stopped, unless it was replaced or removed meanwhile.
    fn client_stopped(&self, key: &str, id: u64, error: String, retry: bool) {
        let mut clients = self.write();
        if let Some(entry) = clients.get_mut(key)
            && entry.id == id
        {
            entry.status = Status::Failed {
                error,
                retry_at: retry.then(|| Instant::now() + RETRY_AFTER),
            };
        }
    }

    pub fn mark_ready(&self, key: &str) {
        if let Some(Status::Running(h)) = self.read().get(key).map(|e| &e.status) {
            h.ready.store(true, Ordering::Relaxed);
        }
    }

    /// Stops and forgets a client, so the next reconcile connects it from scratch.
    pub fn forget(&self, key: &str) {
        let old = self.write().remove(key);
        stop_entry(old);
    }

    /// Starts and stops clients to match the tokens in use. Never runs twice at once.
    pub async fn reconcile(&self, state: &AppState) -> Result<Reconciled> {
        let _guard = self.reconcile_lock.lock().await;
        let desired = desired(state).await?;
        let keys: Vec<String> = desired.iter().map(|(k, _)| k.clone()).collect();
        let known: HashMap<String, Known> = self
            .read()
            .iter()
            .map(|(k, e)| {
                let known = match &e.status {
                    Status::Running(_) => Known::Running,
                    Status::Failed { retry_at, .. } => Known::Failed {
                        retry_at: *retry_at,
                    },
                };
                (k.clone(), known)
            })
            .collect();
        let plan = plan(&known, &keys, Instant::now(), MAX_CLIENTS);

        for key in &plan.stop {
            tracing::info!("disconnecting a Discord bot that no module uses any more");
            self.forget(key);
        }
        for key in &plan.over_limit {
            if !matches!(known.get(key), Some(Known::Failed { .. })) {
                tracing::warn!("{TOO_MANY}");
            }
            self.insert_failed(key, TOO_MANY.into(), Some(Instant::now()));
        }
        if self.live {
            for key in &plan.start {
                if let Some((_, token)) = desired.iter().find(|(k, _)| k == key) {
                    self.start(state, key, token).await;
                }
            }
        }
        Ok(Reconciled {
            started: plan.start,
            stopped: plan.stop,
        })
    }

    /// Keeps the connections in line with the settings: at start, then every
    /// [`RECONCILE_EVERY`]. Separate from module ticks, so a slow Discord never delays them.
    pub async fn run(state: AppState) {
        let mut interval = tokio::time::interval(RECONCILE_EVERY);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if let Err(e) = state.discord.reconcile(&state).await {
                tracing::error!("could not update Discord connections: {e:#}");
            }
        }
    }

    /// Runs [`reconcile`](Self::reconcile) in the background, e.g. after a dashboard change.
    pub fn reconcile_soon(state: &AppState) {
        if !state.discord.live {
            return;
        }
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = state.discord.reconcile(&state).await {
                tracing::error!("could not update Discord connections: {e:#}");
            }
        });
    }

    async fn start(&self, state: &AppState, key: &str, token: &SecretString) {
        match build_client(state, token, key).await {
            Ok((mut client, handle)) => {
                let id = self.insert_running(key, handle);
                let (state, key) = (state.clone(), key.to_owned());
                tokio::spawn(async move {
                    if let Err(e) = client.start().await {
                        let (reason, retry) = describe_stop(&e);
                        tracing::error!("{reason}");
                        state.discord.client_stopped(&key, id, reason, retry);
                    }
                });
            }
            Err(StartError::Rejected) => {
                tracing::error!("{TOKEN_REJECTED}");
                self.insert_failed(key, TOKEN_REJECTED.into(), None);
            }
            Err(StartError::Failed(e)) => {
                tracing::error!("Discord bot not started: {e:#}");
                self.insert_failed(key, format!("{e:#}"), Some(Instant::now() + RETRY_AFTER));
            }
        }
    }

    /// Disconnects every bot, on shutdown.
    pub async fn shutdown_all(&self) {
        let entries: Vec<Entry> = self.write().drain().map(|(_, e)| e).collect();
        for entry in entries {
            if let Status::Running(h) = entry.status {
                h.shard_manager.shutdown_all().await;
            }
        }
    }
}

fn stop_entry(entry: Option<Entry>) {
    if let Some(Entry {
        status: Status::Running(h),
        ..
    }) = entry
    {
        tokio::spawn(async move { h.shard_manager.shutdown_all().await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::store;
    use crate::modules::set_enabled;
    use crate::test_util::{self, DISCORD_TOKEN, OTHER_DISCORD_TOKEN};

    fn k(s: &str) -> String {
        s.to_owned()
    }

    fn known(entries: &[(&str, Known)]) -> HashMap<String, Known> {
        entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn plan_starts_missing_and_stops_unused() {
        let now = Instant::now();
        let p = plan(
            &known(&[("a", Known::Running), ("old", Known::Running)]),
            &[k("a"), k("b")],
            now,
            10,
        );
        assert_eq!(p.start, [k("b")]);
        assert_eq!(p.stop, [k("old")]);
        assert!(p.over_limit.is_empty());
    }

    #[test]
    fn plan_retries_failed_clients_only_when_due() {
        let now = Instant::now();
        let later = now + Duration::from_secs(60);
        let p = plan(
            &known(&[
                ("rejected", Known::Failed { retry_at: None }),
                (
                    "due",
                    Known::Failed {
                        retry_at: Some(now),
                    },
                ),
                (
                    "later",
                    Known::Failed {
                        retry_at: Some(later),
                    },
                ),
            ]),
            &[k("rejected"), k("due"), k("later")],
            now,
            10,
        );
        assert_eq!(p.start, [k("due")]);
        assert!(p.stop.is_empty());
    }

    #[test]
    fn plan_keeps_running_clients_within_the_limit() {
        let now = Instant::now();
        let p = plan(
            &known(&[("c", Known::Running)]),
            &[k("a"), k("b"), k("c")],
            now,
            2,
        );
        // c keeps its slot even though a and b come first.
        assert_eq!(p.start, [k("a")]);
        assert_eq!(p.over_limit, [k("b")]);
        assert!(p.stop.is_empty());
    }

    async fn discord_key(state: &AppState, pid: i64, owner: &str, token: &str) {
        store(
            &state.db,
            &state.vault,
            pid,
            Service::Discord,
            owner,
            &SecretString::new(token),
            "bot",
            "root",
        )
        .await
        .unwrap();
    }

    fn key_of(token: &str) -> String {
        client_key(&SecretString::new(token))
    }

    #[tokio::test]
    async fn desired_follows_enabled_modules_and_their_keys() {
        let state = test_util::state().await;
        let db = &state.db;
        let a = projects::create(db, "A", "root").await.unwrap().unwrap();
        let b = projects::create(db, "B", "root").await.unwrap().unwrap();
        let keys = |state: AppState| async move {
            desired(&state)
                .await
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };

        // A stored token alone connects nothing.
        discord_key(&state, a, DISCORD, DISCORD_TOKEN).await;
        assert!(keys(state.clone()).await.is_empty());

        set_enabled(db, a, DISCORD, true).await.unwrap();
        assert_eq!(keys(state.clone()).await, [key_of(DISCORD_TOKEN)]);

        // The same token in another project is one client.
        discord_key(&state, b, DISCORD, DISCORD_TOKEN).await;
        set_enabled(db, b, DISCORD, true).await.unwrap();
        assert_eq!(keys(state.clone()).await, [key_of(DISCORD_TOKEN)]);

        // A module with its own token adds a client only while enabled.
        keys::set_use_shared(db, b, Service::Discord, "greeting", false)
            .await
            .unwrap();
        discord_key(&state, b, "greeting", OTHER_DISCORD_TOKEN).await;
        assert_eq!(keys(state.clone()).await, [key_of(DISCORD_TOKEN)]);
        set_enabled(db, b, "greeting", true).await.unwrap();
        assert_eq!(
            keys(state.clone()).await,
            [key_of(DISCORD_TOKEN), key_of(OTHER_DISCORD_TOKEN)]
        );

        // Modules on the shared token add nothing when the Discord module is off.
        set_enabled(db, a, DISCORD, false).await.unwrap();
        set_enabled(db, a, "greeting", true).await.unwrap();
        set_enabled(db, b, DISCORD, false).await.unwrap();
        assert_eq!(keys(state.clone()).await, [key_of(OTHER_DISCORD_TOKEN)]);

        // Unreadable tokens are skipped.
        sqlx::query("UPDATE api_keys SET sealed = x'00' WHERE owner = 'greeting'")
            .execute(db)
            .await
            .unwrap();
        assert!(keys(state.clone()).await.is_empty());
    }

    #[tokio::test]
    async fn reconcile_stops_unused_clients_and_reports_new_ones() {
        let state = test_util::state().await;
        let db = &state.db;
        let pid = projects::create(db, "A", "root").await.unwrap().unwrap();
        test_util::connect_fake_discord(&state, pid).await;
        let running = key_of(DISCORD_TOKEN);
        assert!(state.discord.handle(&running).is_some());

        // In use: nothing changes.
        let r = state.discord.reconcile(&state).await.unwrap();
        assert_eq!(r, Reconciled::default());

        // Another token gets wanted; the hub is not live, so it is only reported.
        keys::set_use_shared(db, pid, Service::Discord, "greeting", false)
            .await
            .unwrap();
        discord_key(&state, pid, "greeting", OTHER_DISCORD_TOKEN).await;
        set_enabled(db, pid, "greeting", true).await.unwrap();
        let r = state.discord.reconcile(&state).await.unwrap();
        assert_eq!(r.started, [key_of(OTHER_DISCORD_TOKEN)]);

        // Switching the Discord module off disconnects its bot.
        set_enabled(db, pid, DISCORD, false).await.unwrap();
        let r = state.discord.reconcile(&state).await.unwrap();
        assert_eq!(r.stopped, std::slice::from_ref(&running));
        assert!(state.discord.handle(&running).is_none());
        assert_eq!(state.discord.running_count(), 0);
    }

    #[tokio::test]
    async fn stopped_clients_are_recorded_unless_replaced() {
        let state = test_util::state().await;
        let pid = projects::create(&state.db, "A", "root")
            .await
            .unwrap()
            .unwrap();
        test_util::connect_fake_discord(&state, pid).await;
        let key = key_of(DISCORD_TOKEN);
        let handle = state.discord.handle(&key).unwrap();
        assert!(handle.is_ready());

        let old = state.discord.insert_running(&key, handle.clone());
        let new = state.discord.insert_running(&key, handle);
        state
            .discord
            .client_stopped(&key, old, "old client".into(), true);
        assert!(
            state.discord.handle(&key).is_some(),
            "a replaced client is ignored"
        );

        state
            .discord
            .client_stopped(&key, new, TOKEN_REJECTED.into(), false);
        match state.discord.status(&key) {
            Some(Status::Failed { error, retry_at }) => {
                assert_eq!(error, TOKEN_REJECTED);
                assert!(retry_at.is_none());
            }
            _ => panic!("expected a failed client"),
        }
        state.discord.forget(&key);
        assert!(state.discord.status(&key).is_none());
    }
}
