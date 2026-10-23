//! Calendar module: reads meetings and events from a ClickUp list (read only) and posts
//! scheduled digests and per-event reminders to Discord channels.

pub mod events;
pub mod reactions;
pub mod render;
pub mod runner;
pub mod schedule;
mod web;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use jiff::tz::TimeZone;
use sqlx::SqlitePool;

use self::reactions::Reaction;
use super::{Module, StatItem, counter};
use crate::db::{now, sf};
use crate::state::{AppState, DiscordHandle};

pub const ID: &str = "calendar";
pub const MAX_RULES: i64 = 20;
pub const MAX_TEMPLATE_LEN: usize = 1500;
/// Upper bound of a digest's window offset, 7 days.
pub const MAX_OFFSET_MINUTES: u32 = 10080;
/// How long individual log rows are kept. Totals live in `module_stats`.
pub const LOG_RETENTION_SECS: i64 = 90 * 24 * 60 * 60;
/// Sent-reminder markers are kept this long after the event.
pub const SENT_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;
pub const DEFAULT_COLOR: u32 = 0x7f8cff;

pub const DEFAULT_DIGEST_HEADER: &str = "**Upcoming events** ({count})";
pub const DEFAULT_DIGEST_ITEM: &str = "**{link}**\n{when} ({relative})\n{assignees}";
pub const DEFAULT_EMPTY: &str = "No upcoming events.";
pub const DEFAULT_REMINDER_ITEM: &str =
    "Reminder: **{link}** starts {relative} ({time})\n{assignees}\n{description}";

pub struct CalendarModule;

#[async_trait]
impl Module for CalendarModule {
    fn id(&self) -> &'static str {
        ID
    }

    fn name(&self) -> &'static str {
        "Calendar"
    }

    fn description(&self) -> &'static str {
        "Posts agendas and reminders of meetings and events from a ClickUp list."
    }

    async fn summary(&self, state: &AppState, guild_id: u64) -> Result<Vec<StatItem>> {
        let cfg = load_config(&state.db, guild_id).await?;
        let active = list_rules(&state.db, guild_id)
            .await?
            .iter()
            .filter(|r| r.enabled)
            .count();
        Ok(vec![
            StatItem::new(
                "ClickUp list",
                cfg.list_name.unwrap_or_else(|| "Not set".into()),
            ),
            StatItem::new("Active rules", active),
            StatItem::new("Posts sent", posts_sent(&state.db, Some(guild_id)).await?),
        ])
    }

    fn routes(&self) -> Router<AppState> {
        web::routes()
    }

    fn ticks(&self) -> bool {
        true
    }

    async fn on_tick(
        &self,
        state: &AppState,
        discord: &DiscordHandle,
        guild_id: u64,
    ) -> Result<()> {
        let outbox = runner::DiscordOutbox::new(&discord.http);
        runner::run(state, guild_id, now(), &outbox).await
    }
}

/// Digest and reminder messages sent, for one guild or all of them.
pub async fn posts_sent(db: &SqlitePool, guild_id: Option<u64>) -> Result<i64> {
    Ok(
        counter(db, guild_id, ID, "digests").await?
            + counter(db, guild_id, ID, "reminders").await?,
    )
}

// ---------------------------------------------------------------- source config

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CalendarConfig {
    pub list_id: Option<String>,
    pub list_name: Option<String>,
    /// `due`, or `start` (falling back to the due date).
    pub time_field: String,
    pub timezone: String,
    /// Comma separated tag names; tasks need one of them. Empty means all tasks.
    pub tag_filter: String,
    pub include_subtasks: bool,
    pub include_closed: bool,
}

impl Default for CalendarConfig {
    fn default() -> Self {
        Self {
            list_id: None,
            list_name: None,
            time_field: "due".into(),
            timezone: "UTC".into(),
            tag_filter: String::new(),
            include_subtasks: false,
            include_closed: false,
        }
    }
}

impl CalendarConfig {
    pub fn tz(&self) -> TimeZone {
        schedule::parse_tz(&self.timezone).unwrap_or(TimeZone::UTC)
    }

    pub fn start_mode(&self) -> bool {
        self.time_field == "start"
    }

    pub fn tags(&self) -> Vec<String> {
        parse_tags(&self.tag_filter).unwrap_or_default()
    }
}

/// Up to 5 comma separated tag names of at most 50 characters each.
pub fn parse_tags(raw: &str) -> Option<Vec<String>> {
    let tags: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect();
    (tags.len() <= 5 && tags.iter().all(|t| t.chars().count() <= 50)).then_some(tags)
}

pub async fn load_config(db: &SqlitePool, guild_id: u64) -> Result<CalendarConfig> {
    let cfg = sqlx::query_as(
        "SELECT list_id, list_name, time_field, timezone, tag_filter, include_subtasks,
                include_closed
         FROM calendar_config WHERE guild_id = ?",
    )
    .bind(sf(guild_id))
    .fetch_optional(db)
    .await?;
    Ok(cfg.unwrap_or_default())
}

pub async fn save_config(
    db: &SqlitePool,
    guild_id: u64,
    cfg: &CalendarConfig,
    by: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO calendar_config
             (guild_id, list_id, list_name, time_field, timezone, tag_filter, include_subtasks,
              include_closed, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (guild_id) DO UPDATE SET
             list_id = excluded.list_id,
             list_name = excluded.list_name,
             time_field = excluded.time_field,
             timezone = excluded.timezone,
             tag_filter = excluded.tag_filter,
             include_subtasks = excluded.include_subtasks,
             include_closed = excluded.include_closed,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by",
    )
    .bind(sf(guild_id))
    .bind(cfg.list_id.clone())
    .bind(cfg.list_name.clone())
    .bind(cfg.time_field.clone())
    .bind(cfg.timezone.clone())
    .bind(cfg.tag_filter.clone())
    .bind(cfg.include_subtasks)
    .bind(cfg.include_closed)
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- rules

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Digest,
    Reminder,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Digest => "digest",
            Kind::Reminder => "reminder",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "digest" => Some(Kind::Digest),
            "reminder" => Some(Kind::Reminder),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Digest => "Digest",
            Kind::Reminder => "Reminder",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// One message listing every event.
    Combined,
    /// One message per event.
    PerEvent,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    pub kind: Kind,
    pub channel_id: Option<u64>,
    /// Weekdays, bit 0 = Monday.
    pub days: u8,
    /// Minutes after midnight in `timezone`.
    pub time_of_day: u16,
    /// IANA time zone of the digest schedule.
    pub timezone: String,
    pub lookahead_hours: u32,
    /// Minutes after the scheduled time where a digest's event window starts.
    pub offset_minutes: u32,
    pub lead_minutes: u32,
    pub layout: Layout,
    pub max_events: u32,
    pub post_when_empty: bool,
    pub header_template: String,
    pub item_template: String,
    pub empty_template: String,
    pub mention_assignees: bool,
    /// Ping roles mapped from task tags (the `{roles}` placeholder).
    pub mention_roles: bool,
    pub use_embed: bool,
    pub embed_color: u32,
    pub description_max: u32,
    pub reactions: Vec<Reaction>,
    pub last_run_at: Option<i64>,
}

impl Rule {
    pub fn new(kind: Kind) -> Self {
        let (name, item) = match kind {
            Kind::Digest => ("Daily agenda", DEFAULT_DIGEST_ITEM),
            Kind::Reminder => ("Event reminder", DEFAULT_REMINDER_ITEM),
        };
        Self {
            id: 0,
            name: name.into(),
            enabled: true,
            kind,
            channel_id: None,
            days: schedule::ALL_DAYS,
            time_of_day: 9 * 60,
            timezone: "UTC".into(),
            lookahead_hours: 24,
            offset_minutes: 0,
            lead_minutes: 15,
            layout: Layout::Combined,
            max_events: 15,
            post_when_empty: false,
            header_template: DEFAULT_DIGEST_HEADER.into(),
            item_template: item.into(),
            empty_template: DEFAULT_EMPTY.into(),
            mention_assignees: false,
            mention_roles: false,
            use_embed: false,
            embed_color: DEFAULT_COLOR,
            description_max: 200,
            reactions: Vec::new(),
            last_run_at: None,
        }
    }

    /// The digest schedule's time zone, UTC if the stored name is unknown.
    pub fn tz(&self) -> TimeZone {
        schedule::parse_tz(&self.timezone).unwrap_or(TimeZone::UTC)
    }

    /// How far ahead this rule needs events, in seconds.
    pub fn horizon_secs(&self) -> i64 {
        match self.kind {
            Kind::Digest => {
                i64::from(self.offset_minutes) * 60 + i64::from(self.lookahead_hours) * 3600
            }
            Kind::Reminder => i64::from(self.lead_minutes) * 60 + 600,
        }
    }

    /// The events a digest posted for the scheduled time `anchor` lists: `start <= at < end`.
    pub fn digest_window(&self, anchor: i64) -> (i64, i64) {
        let start = anchor + i64::from(self.offset_minutes) * 60;
        (start, start + i64::from(self.lookahead_hours) * 3600)
    }

    /// Short description for the rules table.
    pub fn schedule_text(&self) -> String {
        match self.kind {
            Kind::Digest => {
                let mut text = format!(
                    "{} at {} {}, events of the next {} h",
                    schedule::describe_days(self.days),
                    schedule::fmt_minute(self.time_of_day),
                    self.timezone,
                    self.lookahead_hours
                );
                if self.offset_minutes > 0 {
                    text.push_str(&format!(
                        ", starting {} after",
                        schedule::describe_lead(self.offset_minutes)
                    ));
                }
                text
            }
            Kind::Reminder => format!(
                "{} before each event",
                schedule::describe_lead(self.lead_minutes)
            ),
        }
    }
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: i64,
    name: String,
    enabled: bool,
    kind: String,
    channel_id: Option<i64>,
    days: i64,
    time_of_day: i64,
    timezone: String,
    lookahead_hours: i64,
    offset_minutes: i64,
    lead_minutes: i64,
    layout: String,
    max_events: i64,
    post_when_empty: bool,
    header_template: String,
    item_template: String,
    empty_template: String,
    mention_assignees: bool,
    mention_roles: bool,
    use_embed: bool,
    embed_color: i64,
    description_max: i64,
    reactions: String,
    last_run_at: Option<i64>,
}

impl From<RuleRow> for Rule {
    fn from(r: RuleRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            enabled: r.enabled,
            kind: Kind::parse(&r.kind).unwrap_or(Kind::Digest),
            channel_id: r.channel_id.map(|c| c as u64),
            days: (r.days & i64::from(schedule::ALL_DAYS)) as u8,
            time_of_day: r.time_of_day.clamp(0, 1439) as u16,
            timezone: r.timezone,
            lookahead_hours: r.lookahead_hours.clamp(1, 336) as u32,
            offset_minutes: r.offset_minutes.clamp(0, i64::from(MAX_OFFSET_MINUTES)) as u32,
            lead_minutes: r.lead_minutes.clamp(1, 10080) as u32,
            layout: if r.layout == "per_event" {
                Layout::PerEvent
            } else {
                Layout::Combined
            },
            max_events: r.max_events.clamp(1, 50) as u32,
            post_when_empty: r.post_when_empty,
            header_template: r.header_template,
            item_template: r.item_template,
            empty_template: r.empty_template,
            mention_assignees: r.mention_assignees,
            mention_roles: r.mention_roles,
            use_embed: r.use_embed,
            embed_color: (r.embed_color & 0xff_ffff) as u32,
            description_max: r.description_max.clamp(0, 1000) as u32,
            reactions: reactions::from_storage(&r.reactions),
            last_run_at: r.last_run_at,
        }
    }
}

const RULE_COLUMNS: &str = "id, name, enabled, kind, channel_id, days, time_of_day, timezone, \
    lookahead_hours, offset_minutes, lead_minutes, layout, max_events, post_when_empty, header_template, \
    item_template, empty_template, mention_assignees, mention_roles, use_embed, embed_color, \
    description_max, reactions, last_run_at";

pub async fn list_rules(db: &SqlitePool, guild_id: u64) -> Result<Vec<Rule>> {
    let rows: Vec<RuleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RULE_COLUMNS} FROM calendar_rules WHERE guild_id = ?
         ORDER BY kind, name COLLATE NOCASE, id"
    )))
    .bind(sf(guild_id))
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Rule::from).collect())
}

/// A rule of this guild. Ids of other guilds' rules return None.
pub async fn load_rule(db: &SqlitePool, guild_id: u64, rule_id: i64) -> Result<Option<Rule>> {
    let row: Option<RuleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RULE_COLUMNS} FROM calendar_rules WHERE guild_id = ? AND id = ?"
    )))
    .bind(sf(guild_id))
    .bind(rule_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(Rule::from))
}

pub async fn count_rules(db: &SqlitePool, guild_id: u64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM calendar_rules WHERE guild_id = ?")
            .bind(sf(guild_id))
            .fetch_one(db)
            .await?,
    )
}

/// Inserts a rule and returns its id. `last_run_at` starts at `now`, so a digest time
/// earlier today does not fire right after saving.
pub async fn insert_rule(db: &SqlitePool, guild_id: u64, rule: &Rule, by: &str) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO calendar_rules
             (guild_id, name, enabled, kind, channel_id, days, time_of_day, timezone,
              lookahead_hours, offset_minutes, lead_minutes, layout, max_events,
              post_when_empty, header_template, item_template, empty_template,
              mention_assignees, mention_roles, use_embed, embed_color, description_max,
              reactions, last_run_at, created_at, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sf(guild_id))
    .bind(rule.name.clone())
    .bind(rule.enabled)
    .bind(rule.kind.as_str())
    .bind(rule.channel_id.map(sf))
    .bind(i64::from(rule.days))
    .bind(i64::from(rule.time_of_day))
    .bind(rule.timezone.clone())
    .bind(i64::from(rule.lookahead_hours))
    .bind(i64::from(rule.offset_minutes))
    .bind(i64::from(rule.lead_minutes))
    .bind(layout_str(rule.layout))
    .bind(i64::from(rule.max_events))
    .bind(rule.post_when_empty)
    .bind(rule.header_template.clone())
    .bind(rule.item_template.clone())
    .bind(rule.empty_template.clone())
    .bind(rule.mention_assignees)
    .bind(rule.mention_roles)
    .bind(rule.use_embed)
    .bind(i64::from(rule.embed_color))
    .bind(i64::from(rule.description_max))
    .bind(reactions::to_storage(&rule.reactions))
    .bind(now())
    .bind(now())
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Saves every editable field of `rule` (matched by id and guild). Resets `last_run_at` to
/// now, like [`insert_rule`].
pub async fn update_rule(db: &SqlitePool, guild_id: u64, rule: &Rule, by: &str) -> Result<()> {
    sqlx::query(
        "UPDATE calendar_rules SET
             name = ?, enabled = ?, channel_id = ?, days = ?, time_of_day = ?, timezone = ?,
             lookahead_hours = ?, offset_minutes = ?, lead_minutes = ?, layout = ?,
             max_events = ?,
             post_when_empty = ?, header_template = ?, item_template = ?, empty_template = ?,
             mention_assignees = ?, mention_roles = ?, use_embed = ?, embed_color = ?,
             description_max = ?, reactions = ?, last_run_at = ?, updated_at = ?,
             updated_by = ?
         WHERE guild_id = ? AND id = ?",
    )
    .bind(rule.name.clone())
    .bind(rule.enabled)
    .bind(rule.channel_id.map(sf))
    .bind(i64::from(rule.days))
    .bind(i64::from(rule.time_of_day))
    .bind(rule.timezone.clone())
    .bind(i64::from(rule.lookahead_hours))
    .bind(i64::from(rule.offset_minutes))
    .bind(i64::from(rule.lead_minutes))
    .bind(layout_str(rule.layout))
    .bind(i64::from(rule.max_events))
    .bind(rule.post_when_empty)
    .bind(rule.header_template.clone())
    .bind(rule.item_template.clone())
    .bind(rule.empty_template.clone())
    .bind(rule.mention_assignees)
    .bind(rule.mention_roles)
    .bind(rule.use_embed)
    .bind(i64::from(rule.embed_color))
    .bind(i64::from(rule.description_max))
    .bind(reactions::to_storage(&rule.reactions))
    .bind(now())
    .bind(now())
    .bind(by.to_owned())
    .bind(sf(guild_id))
    .bind(rule.id)
    .execute(db)
    .await?;
    Ok(())
}

fn layout_str(layout: Layout) -> &'static str {
    match layout {
        Layout::Combined => "combined",
        Layout::PerEvent => "per_event",
    }
}

pub async fn delete_rule(db: &SqlitePool, guild_id: u64, rule_id: i64) -> Result<bool> {
    let res = sqlx::query("DELETE FROM calendar_rules WHERE guild_id = ? AND id = ?")
        .bind(sf(guild_id))
        .bind(rule_id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn set_last_run(db: &SqlitePool, rule_id: i64, at: i64) -> Result<()> {
    sqlx::query("UPDATE calendar_rules SET last_run_at = ? WHERE id = ?")
        .bind(at)
        .bind(rule_id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn was_sent(db: &SqlitePool, rule_id: i64, task_id: &str, event_at: i64) -> Result<bool> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM calendar_sent WHERE rule_id = ? AND task_id = ? AND event_at = ?",
    )
    .bind(rule_id)
    .bind(task_id.to_owned())
    .bind(event_at)
    .fetch_optional(db)
    .await?;
    Ok(found.is_some())
}

pub async fn mark_sent(db: &SqlitePool, rule_id: i64, task_id: &str, event_at: i64) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO calendar_sent (rule_id, task_id, event_at, sent_at)
         VALUES (?, ?, ?, ?)",
    )
    .bind(rule_id)
    .bind(task_id.to_owned())
    .bind(event_at)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- log and stats

pub struct LogEntry<'a> {
    pub rule: &'a Rule,
    /// `digest`, `reminder` or `missed`.
    pub kind: &'a str,
    pub events: usize,
    pub error: Option<String>,
    pub success: bool,
    pub is_test: bool,
}

pub async fn log(db: &SqlitePool, guild_id: u64, entry: LogEntry<'_>) -> Result<()> {
    let error = entry.error.map(|mut e| {
        e.truncate(e.floor_char_boundary(300));
        e
    });
    sqlx::query(
        "INSERT INTO calendar_log
             (guild_id, rule_id, rule_name, kind, events, success, error, is_test, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sf(guild_id))
    .bind(entry.rule.id)
    .bind(entry.rule.name.clone())
    .bind(entry.kind.to_owned())
    .bind(entry.events as i64)
    .bind(entry.success)
    .bind(error)
    .bind(entry.is_test)
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
pub struct LogRow {
    pub rule_name: String,
    pub kind: String,
    pub events: i64,
    pub success: bool,
    pub error: Option<String>,
    pub is_test: bool,
    pub created_at: i64,
}

pub async fn recent_log(db: &SqlitePool, guild_id: u64, limit: i64) -> Result<Vec<LogRow>> {
    Ok(sqlx::query_as(
        "SELECT rule_name, kind, events, success, error, is_test, created_at FROM calendar_log
         WHERE guild_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(sf(guild_id))
    .bind(limit)
    .fetch_all(db)
    .await?)
}

pub async fn prune(db: &SqlitePool) -> Result<u64> {
    let log = sqlx::query("DELETE FROM calendar_log WHERE created_at < ?")
        .bind(now() - LOG_RETENTION_SECS)
        .execute(db)
        .await?;
    let sent = sqlx::query("DELETE FROM calendar_sent WHERE event_at < ?")
        .bind(now() - SENT_RETENTION_SECS)
        .execute(db)
        .await?;
    Ok(log.rows_affected() + sent.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util;

    #[test]
    fn tags() {
        assert_eq!(
            parse_tags(" meeting, event ,,").unwrap(),
            ["meeting", "event"]
        );
        assert_eq!(parse_tags("").unwrap(), Vec::<String>::new());
        assert!(parse_tags("a,b,c,d,e,f").is_none());
        assert!(parse_tags(&"x".repeat(51)).is_none());
    }

    #[test]
    fn digest_windows() {
        let mut rule = Rule::new(Kind::Digest);
        rule.time_of_day = 20 * 60;
        rule.lookahead_hours = 24;
        assert_eq!(rule.digest_window(1000), (1000, 1000 + 86400));
        assert_eq!(rule.horizon_secs(), 86400);
        assert_eq!(
            rule.schedule_text(),
            "Every day at 20:00 UTC, events of the next 24 h"
        );

        rule.offset_minutes = 240;
        assert_eq!(
            rule.digest_window(1000),
            (1000 + 14400, 1000 + 14400 + 86400)
        );
        assert_eq!(rule.horizon_secs(), 14400 + 86400);
        assert_eq!(
            rule.schedule_text(),
            "Every day at 20:00 UTC, events of the next 24 h, starting 4 hours after"
        );
    }

    #[tokio::test]
    async fn config_defaults_and_round_trip() {
        let db = test_util::pool().await;
        let cfg = load_config(&db, 1).await.unwrap();
        assert_eq!(cfg.time_field, "due");
        assert_eq!(cfg.timezone, "UTC");
        assert!(cfg.list_id.is_none());

        let cfg = CalendarConfig {
            list_id: Some("400".into()),
            list_name: Some("Events / Calendar".into()),
            time_field: "start".into(),
            timezone: "Europe/Warsaw".into(),
            tag_filter: "meeting".into(),
            include_subtasks: true,
            include_closed: true,
        };
        save_config(&db, 1, &cfg, "root").await.unwrap();
        let loaded = load_config(&db, 1).await.unwrap();
        assert!(loaded.start_mode());
        assert_eq!(loaded.tags(), ["meeting"]);
        assert_eq!(loaded.list_id.as_deref(), Some("400"));
        assert!(loaded.include_subtasks && loaded.include_closed);
        assert_eq!(loaded.tz().iana_name(), Some("Europe/Warsaw"),);
    }

    #[tokio::test]
    async fn rules_are_scoped_to_their_guild() {
        let db = test_util::pool().await;
        let mut rule = Rule::new(Kind::Reminder);
        rule.channel_id = Some(u64::MAX - 3);
        rule.reactions = reactions::parse("✅ <:party:5>").unwrap();
        let id = insert_rule(&db, 1, &rule, "root").await.unwrap();
        let other = insert_rule(&db, 2, &Rule::new(Kind::Digest), "root")
            .await
            .unwrap();

        let loaded = load_rule(&db, 1, id).await.unwrap().unwrap();
        assert_eq!(loaded.kind, Kind::Reminder);
        assert_eq!(loaded.channel_id, Some(u64::MAX - 3));
        assert_eq!(loaded.reactions, rule.reactions);
        assert!(loaded.last_run_at.is_some());
        assert!(load_rule(&db, 1, other).await.unwrap().is_none());
        assert_eq!(count_rules(&db, 1).await.unwrap(), 1);

        assert_eq!(loaded.timezone, "UTC");
        assert!(!loaded.mention_roles);

        let mut edited = loaded.clone();
        edited.name = "Standup ping".into();
        edited.lead_minutes = 60;
        edited.offset_minutes = 240;
        edited.timezone = "Europe/Warsaw".into();
        edited.mention_roles = true;
        update_rule(&db, 1, &edited, "alice").await.unwrap();
        // Editing through the wrong guild changes nothing.
        edited.name = "hijacked".into();
        update_rule(&db, 2, &edited, "mallory").await.unwrap();
        let loaded = load_rule(&db, 1, id).await.unwrap().unwrap();
        assert_eq!(loaded.name, "Standup ping");
        assert_eq!(loaded.lead_minutes, 60);
        assert_eq!(loaded.offset_minutes, 240);
        assert_eq!(loaded.tz().iana_name(), Some("Europe/Warsaw"));
        assert!(loaded.mention_roles);

        mark_sent(&db, id, "t1", 100).await.unwrap();
        mark_sent(&db, id, "t1", 100).await.unwrap();
        assert!(was_sent(&db, id, "t1", 100).await.unwrap());
        assert!(!was_sent(&db, id, "t1", 200).await.unwrap());

        assert!(!delete_rule(&db, 2, id).await.unwrap());
        assert!(delete_rule(&db, 1, id).await.unwrap());
        assert!(
            !was_sent(&db, id, "t1", 100).await.unwrap(),
            "sent rows cascade"
        );
    }

    #[tokio::test]
    async fn prune_drops_old_log_and_sent_rows() {
        let db = test_util::pool().await;
        let rule = Rule::new(Kind::Reminder);
        let id = insert_rule(&db, 1, &rule, "root").await.unwrap();
        let future = now() + 100;
        mark_sent(&db, id, "old", now() - SENT_RETENTION_SECS - 10)
            .await
            .unwrap();
        mark_sent(&db, id, "new", future).await.unwrap();
        let entry = LogEntry {
            rule: &rule,
            kind: "reminder",
            events: 1,
            error: None,
            success: true,
            is_test: false,
        };
        log(&db, 1, entry).await.unwrap();
        sqlx::query("UPDATE calendar_log SET created_at = ?")
            .bind(now() - LOG_RETENTION_SECS - 10)
            .execute(&db)
            .await
            .unwrap();
        assert_eq!(prune(&db).await.unwrap(), 2);
        assert!(was_sent(&db, id, "new", future).await.unwrap());
        assert!(recent_log(&db, 1, 10).await.unwrap().is_empty());
    }
}
