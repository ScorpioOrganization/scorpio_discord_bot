//! Read only views of the serenity cache used by the dashboard.

use serenity::all::{Cache, ChannelType, ConnectionStage, GuildId, Permissions};

use super::connection::Conn;
use super::hub::Status;
use crate::state::AppState;

pub struct GuildSummary {
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
    /// Adding reactions also needs the message history permission.
    pub can_react: bool,
    /// Needed to ping roles that are not mentionable.
    pub can_mention_all: bool,
    pub selected: bool,
}

pub struct RoleOption {
    pub id: u64,
    pub name: String,
    /// Anyone may ping it; otherwise the bot needs the Mention All Roles permission.
    pub mentionable: bool,
}

pub struct BotStatus {
    pub online: bool,
    pub stage: &'static str,
    pub latency_ms: Option<u128>,
    pub name: Option<String>,
    pub avatar: Option<String>,
    /// Servers the bot is in.
    pub guild_count: usize,
    /// Why the connection is down, when it stopped.
    pub error: Option<String>,
    /// Developer portal names of privileged intents modules need but the bot runs without.
    pub missing_intents: Vec<&'static str>,
}

pub fn guild(cache: &Cache, guild_id: u64) -> Option<GuildSummary> {
    if guild_id == 0 {
        return None;
    }
    let g = cache.guild(GuildId::new(guild_id))?;
    Some(GuildSummary {
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
                    can_react: has(Permissions::ADD_REACTIONS | Permissions::READ_MESSAGE_HISTORY),
                    can_mention_all: has(Permissions::MENTION_EVERYONE),
                    selected: false,
                },
            )
        })
        .collect();
    channels.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
    channels.into_iter().map(|(_, c)| c).collect()
}

/// Text channels of a connection's server to pick from. While the bot cannot be reached the
/// saved channel stays in the list, so saving other settings does not clear it.
pub fn channel_choices(conn: Option<&Conn>, saved: Option<u64>) -> Vec<ChannelOption> {
    let mut channels = match conn {
        Some(c) => text_channels(&c.handle.cache, c.guild_id),
        None => match saved {
            Some(id) => vec![ChannelOption {
                id,
                name: format!("{id} (saved, bot not connected)"),
                can_send: true,
                can_embed: true,
                can_react: true,
                can_mention_all: true,
                selected: false,
            }],
            None => Vec::new(),
        },
    };
    for c in &mut channels {
        c.selected = Some(c.id) == saved;
    }
    channels
}

/// Roles of a guild, highest first, without `@everyone`.
pub fn roles(cache: &Cache, guild_id: u64) -> Vec<RoleOption> {
    if guild_id == 0 {
        return Vec::new();
    }
    let Some(g) = cache.guild(GuildId::new(guild_id)) else {
        return Vec::new();
    };
    let mut roles: Vec<_> = g
        .roles
        .values()
        .filter(|r| r.id.get() != guild_id)
        .map(|r| {
            (
                r.position,
                RoleOption {
                    id: r.id.get(),
                    name: r.name.clone(),
                    mentionable: r.mentionable,
                },
            )
        })
        .collect();
    roles.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    roles.into_iter().map(|(_, r)| r).collect()
}

/// Status of one bot connection, None when it was never started.
pub async fn status(state: &AppState, client: Option<&Status>) -> BotStatus {
    let handle = match client {
        Some(Status::Running(h)) => h,
        other => {
            let error = match other {
                Some(Status::Failed { error, .. }) => Some(error.clone()),
                _ => None,
            };
            return BotStatus {
                online: false,
                stage: if error.is_some() {
                    "Stopped"
                } else {
                    "Not connected"
                },
                latency_ms: None,
                name: None,
                avatar: None,
                guild_count: 0,
                error,
                missing_intents: Vec::new(),
            };
        }
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
    let wanted = super::wanted_intents(&state.modules);

    BotStatus {
        online: stage == ConnectionStage::Connected,
        stage: match stage {
            ConnectionStage::Connected => "Online",
            ConnectionStage::Disconnected => "Offline",
            ConnectionStage::Resuming => "Resuming",
            _ => "Connecting",
        },
        latency_ms: latency.map(|d| d.as_millis()),
        name,
        avatar,
        guild_count: handle.cache.guild_count(),
        error: None,
        missing_intents: super::privileged_names(wanted.difference(handle.intents)),
    }
}
