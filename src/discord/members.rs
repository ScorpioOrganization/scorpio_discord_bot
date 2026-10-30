//! Snapshot of a guild's members, used to match names from other services to Discord users.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serenity::all::{Cache, GuildId, Http, Member};

use super::connection::Conn;
use crate::state::AppState;

/// How long a fetched member list is reused.
pub const TTL: Duration = Duration::from_secs(10 * 60);
const PAGE: u64 = 1000;
const MAX_MEMBERS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub id: u64,
    pub username: String,
    pub global_name: Option<String>,
    pub nick: Option<String>,
}

impl MemberInfo {
    /// Server nickname, else global display name, else username.
    pub fn display(&self) -> &str {
        self.nick
            .as_deref()
            .or(self.global_name.as_deref())
            .unwrap_or(&self.username)
    }

    fn from_member(m: &Member) -> Self {
        Self {
            id: m.user.id.get(),
            username: m.user.name.clone(),
            global_name: m.user.global_name.clone(),
            nick: m.nick.clone(),
        }
    }
}

type Entry = (Instant, Arc<Vec<MemberInfo>>);

#[derive(Default)]
pub struct MemberCache {
    inner: Mutex<HashMap<u64, Entry>>,
}

impl MemberCache {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Entry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The cached list, if it is younger than [`TTL`].
    pub fn get(&self, guild_id: u64) -> Option<Arc<Vec<MemberInfo>>> {
        self.lock()
            .get(&guild_id)
            .filter(|(at, _)| at.elapsed() < TTL)
            .map(|(_, members)| members.clone())
    }

    /// Stores a list sorted by display name and returns it.
    pub fn insert(&self, guild_id: u64, mut members: Vec<MemberInfo>) -> Arc<Vec<MemberInfo>> {
        members.sort_by_cached_key(|m| m.display().to_lowercase());
        let members = Arc::new(members);
        self.lock()
            .insert(guild_id, (Instant::now(), members.clone()));
        members
    }

    pub fn invalidate(&self, guild_id: u64) {
        self.lock().remove(&guild_id);
    }

    /// Drops expired entries.
    pub fn prune(&self) {
        self.lock().retain(|_, (at, _)| at.elapsed() < TTL);
    }
}

/// Human members of the server of a connection. Fetched over HTTP (needs the Server Members
/// Intent) and cached; when that fails the members in the gateway cache are used. Empty
/// without a connection.
pub async fn guild_members(state: &AppState, conn: Option<&Conn>) -> Arc<Vec<MemberInfo>> {
    let Some(conn) = conn else {
        return Arc::default();
    };
    let guild_id = conn.guild_id;
    if let Some(members) = state.members.get(guild_id) {
        return members;
    }
    let members = match fetch(&conn.handle.http, guild_id).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                guild_id,
                "could not list members, using the gateway cache: {e}"
            );
            from_cache(&conn.handle.cache, guild_id)
        }
    };
    state.members.insert(guild_id, members)
}

async fn fetch(http: &Http, guild_id: u64) -> anyhow::Result<Vec<MemberInfo>> {
    let guild = GuildId::new(guild_id);
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = guild.members(http, Some(PAGE), after).await?;
        after = page.last().map(|m| m.user.id);
        out.extend(
            page.iter()
                .filter(|m| !m.user.bot)
                .map(MemberInfo::from_member),
        );
        if (page.len() as u64) < PAGE || after.is_none() || out.len() >= MAX_MEMBERS {
            return Ok(out);
        }
    }
}

pub fn from_cache(cache: &Cache, guild_id: u64) -> Vec<MemberInfo> {
    if guild_id == 0 {
        return Vec::new();
    }
    cache
        .guild(GuildId::new(guild_id))
        .map(|g| {
            g.members
                .values()
                .filter(|m| !m.user.bot)
                .map(MemberInfo::from_member)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn member(id: u64, username: &str, global: Option<&str>, nick: Option<&str>) -> MemberInfo {
        MemberInfo {
            id,
            username: username.into(),
            global_name: global.map(Into::into),
            nick: nick.map(Into::into),
        }
    }

    #[test]
    fn display_prefers_nick_then_global_name() {
        assert_eq!(member(1, "u", Some("G"), Some("N")).display(), "N");
        assert_eq!(member(1, "u", Some("G"), None).display(), "G");
        assert_eq!(member(1, "u", None, None).display(), "u");
    }

    #[test]
    fn cache_sorts_and_invalidates() {
        let cache = MemberCache::default();
        assert!(cache.get(1).is_none());
        let stored = cache.insert(
            1,
            vec![member(1, "zed", None, None), member(2, "Amy", None, None)],
        );
        assert_eq!(stored[0].username, "Amy");
        assert_eq!(cache.get(1).unwrap().len(), 2);
        assert!(cache.get(2).is_none());
        cache.prune();
        assert!(cache.get(1).is_some());
        cache.invalidate(1);
        assert!(cache.get(1).is_none());
    }

    #[tokio::test]
    async fn members_of_the_connection_server() {
        let state = crate::test_util::state().await;
        assert!(guild_members(&state, None).await.is_empty());

        let pid = crate::projects::create(&state.db, "A", "root")
            .await
            .unwrap()
            .unwrap();
        crate::test_util::connect_fake_discord(&state, pid).await;
        let conn = crate::discord::connection::connection(&state, pid, "discord")
            .await
            .unwrap()
            .unwrap();
        let members = guild_members(&state, Some(&conn)).await;
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].display(), "Anna Nowak");
    }
}
