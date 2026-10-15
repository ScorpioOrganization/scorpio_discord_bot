pub mod template;
mod web;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use serenity::all::{
    ChannelId, Context, CreateAllowedMentions, CreateEmbed, CreateMessage, GatewayIntents, Http,
    Member, UserId,
};
use sqlx::SqlitePool;

use self::template::{DEFAULT_TEMPLATE, Vars};
use super::{Module, StatItem, bump_counter, counter};
use crate::db::{now, sf};
use crate::state::AppState;

pub const ID: &str = "greeting";
/// Muted indigo, matches the dashboard accent.
pub const DEFAULT_EMBED_COLOR: u32 = 0x7f8cff;
/// How long individual greeting attempts are kept. Totals live in `module_stats`.
pub const LOG_RETENTION_SECS: i64 = 90 * 24 * 60 * 60;

pub struct GreetingModule;

#[async_trait]
impl Module for GreetingModule {
    fn id(&self) -> &'static str {
        ID
    }

    fn name(&self) -> &'static str {
        "Greeting"
    }

    fn description(&self) -> &'static str {
        "Posts a customizable welcome message when someone joins the server."
    }

    async fn summary(&self, state: &AppState, guild_id: u64) -> Result<Vec<StatItem>> {
        let cfg = load_config(&state.db, guild_id).await?;
        let channel = match (cfg.channel_id, state.discord()) {
            (None, _) => "Not set".to_owned(),
            (Some(id), Some(d)) => d
                .cache
                .guild(serenity::all::GuildId::new(guild_id))
                .and_then(|g| {
                    g.channels
                        .get(&ChannelId::new(id as u64))
                        .map(|c| format!("#{}", c.name))
                })
                .unwrap_or_else(|| "Unknown channel".into()),
            (Some(_), None) => "Set".to_owned(),
        };
        Ok(vec![
            StatItem::new("Channel", channel),
            StatItem::new(
                "Greetings sent",
                counter(&state.db, Some(guild_id), ID, "sent").await?,
            ),
            StatItem::new(
                "Last 7 days",
                count_since(&state.db, guild_id, 7 * 86400).await?,
            ),
        ])
    }

    fn routes(&self) -> Router<AppState> {
        web::routes()
    }

    fn intents(&self) -> GatewayIntents {
        // Member join events are only delivered with this privileged intent.
        GatewayIntents::GUILD_MEMBERS
    }

    async fn on_member_join(&self, ctx: &Context, state: &AppState, member: &Member) -> Result<()> {
        let guild_id = member.guild_id.get();
        let cfg = load_config(&state.db, guild_id).await?;

        if member.user.bot && !cfg.greet_bots {
            return Ok(());
        }
        let Some(channel_id) = cfg.channel_id else {
            tracing::debug!(guild_id, "greeting enabled but no channel configured");
            return Ok(());
        };

        let (server, member_count) = ctx
            .cache
            .guild(member.guild_id)
            .map(|g| (g.name.clone(), g.member_count))
            .unwrap_or_default();
        let vars = Vars {
            user_id: member.user.id.get(),
            username: member.user.name.clone(),
            display_name: member.display_name().to_owned(),
            server,
            member_count,
        };

        let result = send(
            &ctx.http,
            &cfg,
            channel_id as u64,
            &vars,
            Some(member.user.id),
            member.face(),
        )
        .await;
        log_attempt(&state.db, guild_id, &vars, &result, false).await
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct GreetingConfig {
    pub channel_id: Option<i64>,
    pub template: String,
    pub use_embed: bool,
    pub embed_color: i64,
    pub greet_bots: bool,
}

impl Default for GreetingConfig {
    fn default() -> Self {
        Self {
            channel_id: None,
            template: DEFAULT_TEMPLATE.to_owned(),
            use_embed: false,
            embed_color: i64::from(DEFAULT_EMBED_COLOR),
            greet_bots: false,
        }
    }
}

pub async fn load_config(db: &SqlitePool, guild_id: u64) -> Result<GreetingConfig> {
    let cfg = sqlx::query_as(
        "SELECT channel_id, template, use_embed, embed_color, greet_bots
         FROM greeting_config WHERE guild_id = ?",
    )
    .bind(sf(guild_id))
    .fetch_optional(db)
    .await?;
    Ok(cfg.unwrap_or_default())
}

pub async fn save_config(
    db: &SqlitePool,
    guild_id: u64,
    cfg: &GreetingConfig,
    by: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO greeting_config
             (guild_id, channel_id, template, use_embed, embed_color, greet_bots, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (guild_id) DO UPDATE SET
             channel_id = excluded.channel_id,
             template = excluded.template,
             use_embed = excluded.use_embed,
             embed_color = excluded.embed_color,
             greet_bots = excluded.greet_bots,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by",
    )
    .bind(sf(guild_id))
    .bind(cfg.channel_id)
    .bind(cfg.template.clone())
    .bind(cfg.use_embed)
    .bind(cfg.embed_color)
    .bind(cfg.greet_bots)
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

/// Posts the greeting. Only `ping` may be mentioned, so names or templates containing
/// `@everyone` or role mentions can never ping anyone else.
async fn send(
    http: &Http,
    cfg: &GreetingConfig,
    channel_id: u64,
    vars: &Vars,
    ping: Option<UserId>,
    avatar: String,
) -> Result<(), String> {
    let text = template::render(&cfg.template, vars);
    let mentions = match ping {
        Some(user) => CreateAllowedMentions::new().users([user]),
        None => CreateAllowedMentions::new(),
    };
    let mut msg = CreateMessage::new().allowed_mentions(mentions);

    if cfg.use_embed {
        // Mentions inside embeds do not notify, so the mention also goes into the content.
        if cfg.template.contains("{user}") {
            msg = msg.content(format!("<@{}>", vars.user_id));
        }
        let embed = CreateEmbed::new()
            .description(text)
            .color(cfg.embed_color as u32)
            .thumbnail(avatar);
        msg = msg.embed(embed);
    } else {
        msg = msg.content(text);
    }

    ChannelId::new(channel_id)
        .send_message(http, msg)
        .await
        .map(|_| ())
        .map_err(|e| {
            let mut err = e.to_string();
            err.truncate(err.floor_char_boundary(300));
            err
        })
}

async fn log_attempt(
    db: &SqlitePool,
    guild_id: u64,
    vars: &Vars,
    result: &Result<(), String>,
    is_test: bool,
) -> Result<()> {
    if let Err(e) = result {
        tracing::warn!(guild_id, "greeting failed: {e}");
    }
    sqlx::query(
        "INSERT INTO greeting_log (guild_id, user_id, user_name, success, error, is_test, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sf(guild_id))
    .bind(sf(vars.user_id))
    .bind(vars.display_name.clone())
    .bind(result.is_ok())
    .bind(result.as_ref().err().cloned())
    .bind(is_test)
    .bind(now())
    .execute(db)
    .await?;

    if !is_test {
        let key = if result.is_ok() { "sent" } else { "failed" };
        bump_counter(db, guild_id, ID, key).await?;
    }
    Ok(())
}

async fn count_since(db: &SqlitePool, guild_id: u64, secs: i64) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM greeting_log
         WHERE guild_id = ? AND success = 1 AND is_test = 0 AND created_at >= ?",
    )
    .bind(sf(guild_id))
    .bind(now() - secs)
    .fetch_one(db)
    .await?)
}

pub async fn prune_log(db: &SqlitePool) -> Result<u64> {
    let res = sqlx::query("DELETE FROM greeting_log WHERE created_at < ?")
        .bind(now() - LOG_RETENTION_SECS)
        .execute(db)
        .await?;
    Ok(res.rows_affected())
}

#[derive(sqlx::FromRow)]
pub struct LogRow {
    pub user_name: String,
    pub success: bool,
    pub error: Option<String>,
    pub is_test: bool,
    pub created_at: i64,
}

pub struct GreetingStats {
    pub sent_total: i64,
    pub failed_total: i64,
    pub sent_24h: i64,
    pub sent_7d: i64,
    pub last_error: Option<LogRow>,
    pub recent: Vec<LogRow>,
}

pub async fn load_stats(db: &SqlitePool, guild_id: u64) -> Result<GreetingStats> {
    let last_error = sqlx::query_as(
        "SELECT user_name, success, error, is_test, created_at FROM greeting_log
         WHERE guild_id = ? AND success = 0 ORDER BY id DESC LIMIT 1",
    )
    .bind(sf(guild_id))
    .fetch_optional(db)
    .await?;
    let recent = sqlx::query_as(
        "SELECT user_name, success, error, is_test, created_at FROM greeting_log
         WHERE guild_id = ? ORDER BY id DESC LIMIT 10",
    )
    .bind(sf(guild_id))
    .fetch_all(db)
    .await?;

    Ok(GreetingStats {
        sent_total: counter(db, Some(guild_id), ID, "sent").await?,
        failed_total: counter(db, Some(guild_id), ID, "failed").await?,
        sent_24h: count_since(db, guild_id, 86400).await?,
        sent_7d: count_since(db, guild_id, 7 * 86400).await?,
        last_error,
        recent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util;

    fn vars() -> Vars {
        Vars {
            user_id: 42,
            username: "new_guy".into(),
            display_name: "New Guy".into(),
            server: "Scorpio".into(),
            member_count: 7,
        }
    }

    #[tokio::test]
    async fn config_defaults_then_upserts() {
        let db = test_util::pool().await;
        let cfg = load_config(&db, 1).await.unwrap();
        assert_eq!(cfg.template, DEFAULT_TEMPLATE);
        assert_eq!(cfg.channel_id, None);
        assert_eq!(cfg.embed_color, i64::from(DEFAULT_EMBED_COLOR));

        let mut cfg = GreetingConfig {
            channel_id: Some(sf(u64::MAX - 1)),
            template: "Hi {user}".into(),
            use_embed: true,
            embed_color: 0x123456,
            greet_bots: true,
        };
        save_config(&db, 1, &cfg, "root").await.unwrap();
        cfg.template = "Hello {user}".into();
        save_config(&db, 1, &cfg, "alice").await.unwrap();

        let loaded = load_config(&db, 1).await.unwrap();
        assert_eq!(loaded.channel_id, cfg.channel_id);
        assert_eq!(loaded.template, "Hello {user}");
        assert!(loaded.use_embed && loaded.greet_bots);
        assert_eq!(loaded.embed_color, 0x123456);
        assert_eq!(
            load_config(&db, 2).await.unwrap().template,
            DEFAULT_TEMPLATE
        );
    }

    #[tokio::test]
    async fn attempts_feed_stats_but_tests_do_not_count() {
        let db = test_util::pool().await;
        let ok = Ok(());
        let failed = Err("Missing Permissions".to_owned());
        log_attempt(&db, 1, &vars(), &ok, false).await.unwrap();
        log_attempt(&db, 1, &vars(), &ok, false).await.unwrap();
        log_attempt(&db, 1, &vars(), &failed, false).await.unwrap();
        log_attempt(&db, 1, &vars(), &ok, true).await.unwrap();
        log_attempt(&db, 2, &vars(), &ok, false).await.unwrap();

        let stats = load_stats(&db, 1).await.unwrap();
        assert_eq!(stats.sent_total, 2);
        assert_eq!(stats.failed_total, 1);
        assert_eq!(stats.sent_24h, 2);
        assert_eq!(stats.sent_7d, 2);
        assert_eq!(stats.recent.len(), 4);
        assert!(stats.recent[0].is_test);
        let last_error = stats.last_error.unwrap();
        assert_eq!(last_error.error.as_deref(), Some("Missing Permissions"));
        assert_eq!(last_error.user_name, "New Guy");
    }

    #[tokio::test]
    async fn prune_log_keeps_recent_rows_and_totals() {
        let db = test_util::pool().await;
        log_attempt(&db, 1, &vars(), &Ok(()), false).await.unwrap();
        for age in [LOG_RETENTION_SECS + 60, 30 * 86400] {
            sqlx::query(
                "INSERT INTO greeting_log (guild_id, user_id, user_name, success, created_at)
                 VALUES (1, 42, 'old', 1, ?)",
            )
            .bind(now() - age)
            .execute(&db)
            .await
            .unwrap();
        }

        assert_eq!(prune_log(&db).await.unwrap(), 1);
        assert_eq!(prune_log(&db).await.unwrap(), 0);
        let stats = load_stats(&db, 1).await.unwrap();
        assert_eq!(stats.recent.len(), 2);
        assert_eq!(stats.sent_total, 1);
    }
}
