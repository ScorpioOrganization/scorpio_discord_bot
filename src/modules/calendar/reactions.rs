//! Reactions the bot adds to its calendar messages, e.g. ✅ ❌ for attendance.

use serenity::all::{Cache, EmojiId, GuildId, ReactionType};

pub const MAX_REACTIONS: usize = 5;
const MAX_UNICODE_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction {
    Unicode(String),
    Custom {
        id: u64,
        name: String,
        animated: bool,
    },
}

impl Reaction {
    pub fn to_reaction_type(&self) -> ReactionType {
        match self {
            Reaction::Unicode(s) => ReactionType::Unicode(s.clone()),
            Reaction::Custom { id, name, animated } => ReactionType::Custom {
                animated: *animated,
                id: EmojiId::new(*id),
                name: Some(name.clone()),
            },
        }
    }

    /// How the reaction is typed and stored: the emoji itself, or `<:name:id>`.
    pub fn code(&self) -> String {
        match self {
            Reaction::Unicode(s) => s.clone(),
            Reaction::Custom { id, name, animated } => {
                format!("<{}:{name}:{id}>", if *animated { "a" } else { "" })
            }
        }
    }
}

fn parse_custom(token: &str) -> Option<Reaction> {
    let inner = token.strip_prefix('<')?.strip_suffix('>')?;
    let mut parts = inner.split(':');
    let (flag, name, id) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || !matches!(flag, "" | "a") {
        return None;
    }
    let name_ok = (2..=32).contains(&name.len())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let id: u64 = id.parse().ok().filter(|id| *id > 0)?;
    name_ok.then(|| Reaction::Custom {
        id,
        name: name.to_owned(),
        animated: flag == "a",
    })
}

/// Rough check that a token is an emoji and not a word: no ASCII letters or markup, at
/// least one non-ASCII character, short. Discord has the final say when reacting.
fn is_unicode_emoji(token: &str) -> bool {
    token.len() <= MAX_UNICODE_BYTES
        && !token.is_ascii()
        && !token
            .chars()
            .any(|c| c.is_ascii_alphabetic() || c.is_whitespace() || "<>:@`*_~|\\".contains(c))
}

/// Parses up to [`MAX_REACTIONS`] reactions separated by spaces or commas. Duplicates are
/// dropped. None when anything is not a reaction.
pub fn parse(raw: &str) -> Option<Vec<Reaction>> {
    let mut out: Vec<Reaction> = Vec::new();
    for token in raw
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
    {
        let reaction = if token.starts_with('<') {
            parse_custom(token)?
        } else if is_unicode_emoji(token) {
            Reaction::Unicode(token.to_owned())
        } else {
            return None;
        };
        if !out.contains(&reaction) {
            out.push(reaction);
        }
    }
    (out.len() <= MAX_REACTIONS).then_some(out)
}

pub fn to_storage(reactions: &[Reaction]) -> String {
    reactions
        .iter()
        .map(Reaction::code)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Stored values were validated on save; anything unreadable is dropped.
pub fn from_storage(raw: &str) -> Vec<Reaction> {
    parse(raw).unwrap_or_default()
}

/// A custom emoji of the guild, for the editor's emoji buttons.
pub struct EmojiOption {
    pub code: String,
    pub name: String,
    pub url: String,
}

pub fn guild_emojis(cache: &Cache, guild_id: u64) -> Vec<EmojiOption> {
    if guild_id == 0 {
        return Vec::new();
    }
    let Some(guild) = cache.guild(GuildId::new(guild_id)) else {
        return Vec::new();
    };
    let mut list: Vec<EmojiOption> = guild
        .emojis
        .values()
        .filter(|e| e.available)
        .map(|e| EmojiOption {
            code: Reaction::Custom {
                id: e.id.get(),
                name: e.name.clone(),
                animated: e.animated,
            }
            .code(),
            name: e.name.clone(),
            url: e.url(),
        })
        .collect();
    list.sort_by_cached_key(|e| e.name.to_lowercase());
    list
}

impl Reaction {
    /// A custom emoji as an editor option, None for Unicode emojis.
    pub fn as_emoji_option(&self) -> Option<EmojiOption> {
        match self {
            Reaction::Custom { id, name, animated } => Some(EmojiOption {
                code: self.code(),
                name: name.clone(),
                url: format!(
                    "https://cdn.discordapp.com/emojis/{id}.{}",
                    if *animated { "gif" } else { "png" }
                ),
            }),
            Reaction::Unicode(_) => None,
        }
    }
}

/// Custom emojis that are not (or no longer) in the guild.
pub fn unknown_custom(reactions: &[Reaction], available: &[EmojiOption]) -> bool {
    reactions.iter().any(|r| match r {
        Reaction::Custom { .. } => !available.iter().any(|e| e.code == r.code()),
        Reaction::Unicode(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_custom_and_animated() {
        let parsed = parse("✅, ❌  1️⃣ <:party:123> <a:dance_2:456>").unwrap();
        assert_eq!(
            parsed,
            [
                Reaction::Unicode("✅".into()),
                Reaction::Unicode("❌".into()),
                Reaction::Unicode("1️⃣".into()),
                Reaction::Custom {
                    id: 123,
                    name: "party".into(),
                    animated: false
                },
                Reaction::Custom {
                    id: 456,
                    name: "dance_2".into(),
                    animated: true
                },
            ]
        );
        assert_eq!(to_storage(&parsed), "✅ ❌ 1️⃣ <:party:123> <a:dance_2:456>");
        assert_eq!(from_storage(&to_storage(&parsed)), parsed);
        assert_eq!(parse("  ").unwrap(), []);
        assert_eq!(parse("✅ ✅").unwrap().len(), 1);
    }

    #[test]
    fn junk_and_limits() {
        for bad in [
            "yes",
            "✅yes",
            ":thumbsup:",
            "<:party:abc>",
            "<:p:123>",
            "<b:party:123>",
            "<:party:0>",
            "<:party:1:2>",
            "@everyone",
            "123",
            "✅ ❌ 👍 🎉 ❓ 👀",
        ] {
            assert!(parse(bad).is_none(), "{bad}");
        }
        assert!(parse(&"🎉".repeat(10)).is_none(), "too long for one emoji");
    }

    #[test]
    fn custom_emojis_must_belong_to_the_guild() {
        let available = [EmojiOption {
            code: "<:party:123>".into(),
            name: "party".into(),
            url: String::new(),
        }];
        assert!(!unknown_custom(
            &parse("✅ <:party:123>").unwrap(),
            &available
        ));
        assert!(unknown_custom(&parse("<:other:9>").unwrap(), &available));
        assert!(unknown_custom(&parse("<a:party:123>").unwrap(), &available));
    }

    #[test]
    fn saved_custom_reactions_become_options() {
        let saved = parse("✅ <:party:123> <a:dance:456>").unwrap();
        let options: Vec<EmojiOption> =
            saved.iter().filter_map(Reaction::as_emoji_option).collect();
        assert_eq!(options.len(), 2);
        assert!(!unknown_custom(&saved, &options));
        assert_eq!(options[0].url, "https://cdn.discordapp.com/emojis/123.png");
        assert_eq!(options[1].url, "https://cdn.discordapp.com/emojis/456.gif");
    }

    #[tokio::test]
    async fn guild_emojis_from_the_cache() {
        use crate::test_util::{EMOJI_ID, GUILD_ID, connect_fake_discord, state};
        let state = state().await;
        connect_fake_discord(&state, 1).await;
        let conn = crate::discord::connection::connection(&state, 1, "calendar")
            .await
            .unwrap()
            .unwrap();
        let cache = &conn.handle.cache;
        let emojis = guild_emojis(cache, GUILD_ID);
        assert_eq!(emojis.len(), 1);
        assert_eq!(emojis[0].code, format!("<:party:{EMOJI_ID}>"));
        assert!(
            emojis[0]
                .url
                .starts_with("https://cdn.discordapp.com/emojis/")
        );
        assert!(guild_emojis(cache, 1).is_empty());
    }
}
