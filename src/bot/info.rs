//! Read only views of the serenity cache used by the dashboard.

use serenity::all::{Cache, ChannelType, ConnectionStage, GuildId, Permissions};

use crate::state::AppState;

pub struct GuildSummary {
    pub id: u64,
    pub name: String,
    pub icon: Option<String>,
    pub member_count: u64,
}

impl GuildSummary {
    /// Up to two letters shown when the guild has no icon.
    pub fn initials(&self) -> String {
        self.name
            .split_whitespace()
            .filter_map(|w| w.chars().next())
            .take(2)
            .collect::<String>()
            .to_uppercase()
    }
}

pub struct ChannelOption {
    pub id: u64,
    pub name: String,
    /// False only when we know the bot cannot view or post in the channel.
    pub can_send: bool,
    pub can_embed: bool,
    pub selected: bool,
}

pub struct BotStatus {
    pub online: bool,
    pub stage: &'static str,
    pub latency_ms: Option<u128>,
    pub name: Option<String>,
    pub avatar: Option<String>,
    /// Why the connection is down, when it stopped for good.
    pub error: Option<String>,
    /// Developer portal names of privileged intents modules need but the bot runs without.
    pub missing_intents: Vec<&'static str>,
}

pub fn guilds(cache: &Cache) -> Vec<GuildSummary> {
    let mut list: Vec<_> = cache
        .guilds()
        .into_iter()
        .filter_map(|id| guild(cache, id.get()))
        .collect();
    list.sort_by_key(|g| g.name.to_lowercase());
    list
}

pub fn guild(cache: &Cache, guild_id: u64) -> Option<GuildSummary> {
    if guild_id == 0 {
        return None;
    }
    let g = cache.guild(GuildId::new(guild_id))?;
    Some(GuildSummary {
        id: guild_id,
        name: g.name.clone(),
        icon: g.icon_url(),
        member_count: g.member_count,
    })
}

/// Text and announcement channels of a guild, in sidebar order.
pub fn text_channels(cache: &Cache, guild_id: u64) -> Vec<ChannelOption> {
    if guild_id == 0 {
        return Vec::new();
    }
    let bot_id = cache.current_user().id;
    let Some(g) = cache.guild(GuildId::new(guild_id)) else {
        return Vec::new();
    };
    let bot_member = g.members.get(&bot_id);

    let mut channels: Vec<_> = g
        .channels
        .values()
        .filter(|c| matches!(c.kind, ChannelType::Text | ChannelType::News))
        .map(|c| {
            let perms = bot_member.map(|m| g.user_permissions_in(c, m));
            let has = |p: Permissions| perms.is_none_or(|perms| perms.contains(p));
            (
                c.position,
                ChannelOption {
                    id: c.id.get(),
                    name: c.name.clone(),
                    can_send: has(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES),
                    can_embed: has(Permissions::EMBED_LINKS),
                    selected: false,
                },
            )
        })
        .collect();
    channels.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    channels.into_iter().map(|(_, c)| c).collect()
}

pub async fn status(state: &AppState) -> BotStatus {
    let error = state.bot_error();
    let Some(handle) = state.discord() else {
        return BotStatus {
            online: false,
            stage: if error.is_some() {
                "Stopped"
            } else {
                "Not configured"
            },
            latency_ms: None,
            name: None,
            avatar: None,
            error,
            missing_intents: Vec::new(),
        };
    };

    let (stage, latency) = {
        let runners = handle.shard_manager.runners.lock().await;
        match runners.values().next() {
            Some(info) => (info.stage, info.latency),
            None => (ConnectionStage::Disconnected, None),
        }
    };
    let (name, avatar) = {
        let user = handle.cache.current_user();
        if user.id.get() == 0 || user.name.is_empty() {
            (None, None)
        } else {
            (Some(user.name.clone()), Some(user.face()))
        }
    };

    BotStatus {
        online: stage == ConnectionStage::Connected,
        stage: match stage {
            _ if error.is_some() => "Stopped",
            ConnectionStage::Connected => "Online",
            ConnectionStage::Disconnected => "Offline",
            ConnectionStage::Resuming => "Resuming",
            _ => "Connecting",
        },
        latency_ms: latency.map(|d| d.as_millis()),
        name,
        avatar,
        error,
        missing_intents: super::privileged_names(
            state.missing_intents(super::wanted_intents(&state.modules)),
        ),
    }
}
