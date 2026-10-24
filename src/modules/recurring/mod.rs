//! Recurring tasks module: stands in for ClickUp's own recurrence when it does not run
//! (for example once a Free plan workspace is out of automations).
//!
//! ClickUp's API does not expose recurrence settings, so the rules live here: each maps a
//! ClickUp tag to an interval. Tagged tasks that are done (or past due, per rule) move to
//! their next date, or get a copy at the next date. This is the only module that writes
//! to ClickUp.

pub mod plan;
pub mod runner;
pub mod schedule;
mod web;

use std::collections::HashSet;

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use jiff::tz::TimeZone;
use sqlx::SqlitePool;

use super::{Module, StatItem, counter};
use crate::db::{now, sf};
use crate::modules::calendar::schedule::parse_tz;
use crate::state::{AppState, DiscordHandle};
use crate::web::fmt_ts;

pub const ID: &str = "recurring";
pub const MAX_RULES: i64 = 20;
/// Most tasks changed in one run, so a wrong tag cannot rewrite a whole workspace at once.
pub const MAX_WRITES_PER_RUN: usize = 25;
/// Copy mode only looks at tasks due this recently, so a new rule never copies old history.
pub const CREATE_LOOKBACK_SECS: i64 = 60 * 86400;
/// How long individual log rows are kept. Totals live in `module_stats`.
pub const LOG_RETENTION_SECS: i64 = 90 * 86400;
/// Handled occurrences are remembered longer than copy mode looks back.
pub const DONE_RETENTION_SECS: i64 = CREATE_LOOKBACK_SECS + 30 * 86400;
pub const CHECK_MINUTES: std::ops::RangeInclusive<u32> = 5..=1440;
/// A run lock older than this is from a crashed run and is ignored.
const STALE_LOCK_SECS: i64 = 10 * 60;

pub struct RecurringModule;

#[async_trait]
impl Module for RecurringModule {
    fn id(&self) -> &'static str {
        ID
    }

    fn name(&self) -> &'static str {
        "Recurring tasks"
    }

    fn description(&self) -> &'static str {
        "Moves tagged ClickUp tasks to their next date when ClickUp's own recurrence does not."
    }

    async fn summary(&self, state: &AppState, guild_id: u64) -> Result<Vec<StatItem>> {
        let cfg = load_config(&state.db, guild_id).await?;
        let active = list_rules(&state.db, guild_id)
            .await?
            .iter()
            .filter(|r| r.enabled)
            .count();
        Ok(vec![
            StatItem::new("Active rules", active),
            StatItem::new("Tasks recurred", recurred(&state.db, Some(guild_id)).await?),
            StatItem::new("Mode", if cfg.dry_run { "Dry run" } else { "Live" }),
            StatItem::new(
                "Last run",
                cfg.last_run_at
                    .map(fmt_ts)
                    .unwrap_or_else(|| "Never".into()),
            ),
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
        _discord: &DiscordHandle,
        guild_id: u64,
    ) -> Result<()> {
        runner::run(state, guild_id, now(), false).await?;
        Ok(())
    }
}

/// Tasks moved or copied, for one guild or all of them.
pub async fn recurred(db: &SqlitePool, guild_id: Option<u64>) -> Result<i64> {
    Ok(counter(db, guild_id, ID, "moved").await? + counter(db, guild_id, ID, "copied").await?)
}

// ---------------------------------------------------------------- config

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecurringConfig {
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub timezone: String,
    pub check_minutes: u32,
    pub dry_run: bool,
    pub last_run_at: Option<i64>,
}

impl Default for RecurringConfig {
    fn default() -> Self {
        Self {
            team_id: None,
            team_name: None,
            timezone: "UTC".into(),
            check_minutes: 15,
            dry_run: true,
            last_run_at: None,
        }
    }
}

impl RecurringConfig {
    pub fn tz(&self) -> TimeZone {
        parse_tz(&self.timezone).unwrap_or(TimeZone::UTC)
    }

    /// When the next scheduled run happens, at the earliest.
    pub fn next_run_at(&self) -> Option<i64> {
        self.last_run_at
            .map(|at| at + i64::from(self.check_minutes) * 60)
    }
}

#[derive(sqlx::FromRow)]
struct ConfigRow {
    team_id: Option<String>,
    team_name: Option<String>,
    timezone: String,
    check_minutes: i64,
    dry_run: bool,
    last_run_at: Option<i64>,
}

pub async fn load_config(db: &SqlitePool, guild_id: u64) -> Result<RecurringConfig> {
    let row: Option<ConfigRow> = sqlx::query_as(
        "SELECT team_id, team_name, timezone, check_minutes, dry_run, last_run_at
         FROM recurring_config WHERE guild_id = ?",
    )
    .bind(sf(guild_id))
    .fetch_optional(db)
    .await?;
    Ok(row
        .map(|r| RecurringConfig {
            team_id: r.team_id,
            team_name: r.team_name,
            timezone: r.timezone,
            check_minutes: u32::try_from(r.check_minutes).unwrap_or(15),
            dry_run: r.dry_run,
            last_run_at: r.last_run_at,
        })
        .unwrap_or_default())
}

/// Saves the settings. Leaves `last_run_at` alone.
pub async fn save_config(
    db: &SqlitePool,
    guild_id: u64,
    cfg: &RecurringConfig,
    by: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO recurring_config
             (guild_id, team_id, team_name, timezone, check_minutes, dry_run, updated_at,
              updated_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (guild_id) DO UPDATE SET
             team_id = excluded.team_id,
             team_name = excluded.team_name,
             timezone = excluded.timezone,
             check_minutes = excluded.check_minutes,
             dry_run = excluded.dry_run,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by",
    )
    .bind(sf(guild_id))
    .bind(cfg.team_id.clone())
    .bind(cfg.team_name.clone())
    .bind(cfg.timezone.clone())
    .bind(i64::from(cfg.check_minutes))
    .bind(cfg.dry_run)
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn set_last_run(db: &SqlitePool, guild_id: u64, at: i64) -> Result<()> {
    sqlx::query("UPDATE recurring_config SET last_run_at = ? WHERE guild_id = ?")
        .bind(at)
        .bind(sf(guild_id))
        .execute(db)
        .await?;
    Ok(())
}

/// Takes the guild's run lock. False when another run holds it.
pub async fn try_lock(db: &SqlitePool, guild_id: u64, at: i64) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE recurring_config SET running_since = ?
         WHERE guild_id = ? AND (running_since IS NULL OR running_since < ?)",
    )
    .bind(at)
    .bind(sf(guild_id))
    .bind(at - STALE_LOCK_SECS)
    .execute(db)
    .await?;
    Ok(res.rows_affected() == 1)
}

pub async fn unlock(db: &SqlitePool, guild_id: u64) -> Result<()> {
    sqlx::query("UPDATE recurring_config SET running_since = NULL WHERE guild_id = ?")
        .bind(sf(guild_id))
        .execute(db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- rules

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    pub const ALL: [Unit; 4] = [Unit::Day, Unit::Week, Unit::Month, Unit::Year];

    pub fn as_str(self) -> &'static str {
        match self {
            Unit::Day => "day",
            Unit::Week => "week",
            Unit::Month => "month",
            Unit::Year => "year",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|u| u.as_str() == raw)
    }

    /// "day" or "days".
    pub fn label(self, every: u16) -> String {
        if every == 1 {
            self.as_str().to_owned()
        } else {
            format!("{}s", self.as_str())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The task is in a done or closed status.
    Close,
    /// The due date passed, whatever the status.
    Schedule,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::Close => "close",
            Trigger::Schedule => "schedule",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "close" => Some(Trigger::Close),
            "schedule" => Some(Trigger::Schedule),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Trigger::Close => "When done",
            Trigger::Schedule => "When due",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Move the task itself to the next date.
    Update,
    /// Leave the task and create a copy at the next date.
    Create,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Update => "update",
            Mode::Create => "create",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "update" => Some(Mode::Update),
            "create" => Some(Mode::Create),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Update => "Move task",
            Mode::Create => "Copy task",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub enabled: bool,
    /// Lowercase, see `modules::clickup::normalize_tag`.
    pub tag: String,
    pub every: u16,
    pub unit: Unit,
    pub trigger: Trigger,
    pub mode: Mode,
    /// Status after recurring. Empty means the first open status of the task's list.
    pub reopen_status: String,
    pub shift_start: bool,
}

impl Rule {
    pub fn new() -> Self {
        Self {
            id: 0,
            name: String::new(),
            enabled: true,
            tag: String::new(),
            every: 1,
            unit: Unit::Week,
            trigger: Trigger::Close,
            mode: Mode::Update,
            reopen_status: String::new(),
            shift_start: true,
        }
    }

    /// "Every week", "Every 2 months".
    pub fn interval_text(&self) -> String {
        match self.every {
            1 => format!("Every {}", self.unit.label(1)),
            n => format!("Every {n} {}", self.unit.label(n)),
        }
    }

    pub fn status_text(&self) -> &str {
        if self.reopen_status.is_empty() {
            "first open status of the list"
        } else {
            &self.reopen_status
        }
    }
}

impl Default for Rule {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: i64,
    name: String,
    enabled: bool,
    tag: String,
    every: i64,
    unit: String,
    trigger_on: String,
    mode: String,
    reopen_status: String,
    shift_start: bool,
}

impl From<RuleRow> for Rule {
    fn from(r: RuleRow) -> Self {
        Self {
            id: r.id,
            name: r.name,
            enabled: r.enabled,
            tag: r.tag,
            every: u16::try_from(r.every).unwrap_or(1),
            unit: Unit::parse(&r.unit).unwrap_or(Unit::Week),
            trigger: Trigger::parse(&r.trigger_on).unwrap_or(Trigger::Close),
            mode: Mode::parse(&r.mode).unwrap_or(Mode::Update),
            reopen_status: r.reopen_status,
            shift_start: r.shift_start,
        }
    }
}

const RULE_COLUMNS: &str =
    "id, name, enabled, tag, every, unit, trigger_on, mode, reopen_status, shift_start";

/// Rules of a guild, oldest first: when a task has the tags of several rules, the oldest
/// rule decides.
pub async fn list_rules(db: &SqlitePool, guild_id: u64) -> Result<Vec<Rule>> {
    let rows: Vec<RuleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RULE_COLUMNS} FROM recurring_rules WHERE guild_id = ? ORDER BY id"
    )))
    .bind(sf(guild_id))
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(Rule::from).collect())
}

/// A rule of this guild. Ids of other guilds' rules return None.
pub async fn load_rule(db: &SqlitePool, guild_id: u64, rule_id: i64) -> Result<Option<Rule>> {
    let row: Option<RuleRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {RULE_COLUMNS} FROM recurring_rules WHERE guild_id = ? AND id = ?"
    )))
    .bind(sf(guild_id))
    .bind(rule_id)
    .fetch_optional(db)
    .await?;
    Ok(row.map(Rule::from))
}

pub async fn count_rules(db: &SqlitePool, guild_id: u64) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM recurring_rules WHERE guild_id = ?")
            .bind(sf(guild_id))
            .fetch_one(db)
            .await?,
    )
}

/// Another rule of the guild already uses `tag`.
pub async fn tag_taken(db: &SqlitePool, guild_id: u64, tag: &str, except_id: i64) -> Result<bool> {
    let found: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM recurring_rules WHERE guild_id = ? AND tag = ? AND id != ?",
    )
    .bind(sf(guild_id))
    .bind(tag.to_owned())
    .bind(except_id)
    .fetch_optional(db)
    .await?;
    Ok(found.is_some())
}

pub async fn insert_rule(db: &SqlitePool, guild_id: u64, rule: &Rule, by: &str) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO recurring_rules
             (guild_id, name, enabled, tag, every, unit, trigger_on, mode, reopen_status,
              shift_start, created_at, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sf(guild_id))
    .bind(rule.name.clone())
    .bind(rule.enabled)
    .bind(rule.tag.clone())
    .bind(i64::from(rule.every))
    .bind(rule.unit.as_str())
    .bind(rule.trigger.as_str())
    .bind(rule.mode.as_str())
    .bind(rule.reopen_status.clone())
    .bind(rule.shift_start)
    .bind(now())
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(res.last_insert_rowid())
}

/// Saves every editable field of `rule` (matched by id and guild).
pub async fn update_rule(db: &SqlitePool, guild_id: u64, rule: &Rule, by: &str) -> Result<()> {
    sqlx::query(
        "UPDATE recurring_rules SET
             name = ?, enabled = ?, tag = ?, every = ?, unit = ?, trigger_on = ?, mode = ?,
             reopen_status = ?, shift_start = ?, updated_at = ?, updated_by = ?
         WHERE guild_id = ? AND id = ?",
    )
    .bind(rule.name.clone())
    .bind(rule.enabled)
    .bind(rule.tag.clone())
    .bind(i64::from(rule.every))
    .bind(rule.unit.as_str())
    .bind(rule.trigger.as_str())
    .bind(rule.mode.as_str())
    .bind(rule.reopen_status.clone())
    .bind(rule.shift_start)
    .bind(now())
    .bind(by.to_owned())
    .bind(sf(guild_id))
    .bind(rule.id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn delete_rule(db: &SqlitePool, guild_id: u64, rule_id: i64) -> Result<bool> {
    let res = sqlx::query("DELETE FROM recurring_rules WHERE guild_id = ? AND id = ?")
        .bind(sf(guild_id))
        .bind(rule_id)
        .execute(db)
        .await?;
    Ok(res.rows_affected() > 0)
}

// ---------------------------------------------------------------- handled occurrences

/// (task id, due date in ms) of every occurrence handled recently.
pub async fn done_markers(db: &SqlitePool, guild_id: u64) -> Result<HashSet<(String, i64)>> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT task_id, due_ms FROM recurring_done WHERE guild_id = ?")
            .bind(sf(guild_id))
            .fetch_all(db)
            .await?;
    Ok(rows.into_iter().collect())
}

/// Records a handled occurrence, or adds the copy's id to one recorded before.
pub async fn mark_done(
    db: &SqlitePool,
    guild_id: u64,
    task_id: &str,
    due_ms: i64,
    rule_id: i64,
    new_task_id: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO recurring_done
             (guild_id, task_id, due_ms, rule_id, new_task_id, done_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (guild_id, task_id, due_ms) DO UPDATE SET
             new_task_id = COALESCE(excluded.new_task_id, new_task_id),
             done_at = excluded.done_at",
    )
    .bind(sf(guild_id))
    .bind(task_id.to_owned())
    .bind(due_ms)
    .bind(rule_id)
    .bind(new_task_id.map(str::to_owned))
    .bind(now())
    .execute(db)
    .await?;
    Ok(())
}

pub async fn unmark_done(db: &SqlitePool, guild_id: u64, task_id: &str, due_ms: i64) -> Result<()> {
    sqlx::query("DELETE FROM recurring_done WHERE guild_id = ? AND task_id = ? AND due_ms = ?")
        .bind(sf(guild_id))
        .bind(task_id.to_owned())
        .bind(due_ms)
        .execute(db)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- log

pub struct LogEntry<'a> {
    pub rule: &'a Rule,
    pub task_id: &'a str,
    pub task_name: &'a str,
    pub task_url: &'a str,
    /// Unix seconds.
    pub old_due: i64,
    pub new_due: i64,
    pub dry_run: bool,
    pub error: Option<String>,
}

/// Writes a log row. A dry run or failure that is already logged for the same occurrence
/// is not logged again, so runs every few minutes do not fill the log. Returns whether a
/// row was written.
pub async fn log(db: &SqlitePool, guild_id: u64, entry: LogEntry<'_>) -> Result<bool> {
    let error = entry.error.map(|mut e| {
        e.truncate(e.floor_char_boundary(300));
        e
    });
    let success = error.is_none();
    if entry.dry_run || !success {
        let seen: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM recurring_log
             WHERE guild_id = ? AND task_id = ? AND old_due = ? AND new_due = ? AND dry_run = ?
               AND success = ? AND error IS ?",
        )
        .bind(sf(guild_id))
        .bind(entry.task_id.to_owned())
        .bind(entry.old_due)
        .bind(entry.new_due)
        .bind(entry.dry_run)
        .bind(success)
        .bind(error.clone())
        .fetch_optional(db)
        .await?;
        if seen.is_some() {
            return Ok(false);
        }
    }
    sqlx::query(
        "INSERT INTO recurring_log
             (guild_id, rule_id, rule_name, task_id, task_name, task_url, old_due, new_due,
              mode, dry_run, success, error, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(sf(guild_id))
    .bind(entry.rule.id)
    .bind(entry.rule.name.clone())
    .bind(entry.task_id.to_owned())
    .bind(entry.task_name.to_owned())
    .bind(entry.task_url.to_owned())
    .bind(entry.old_due)
    .bind(entry.new_due)
    .bind(entry.rule.mode.as_str())
    .bind(entry.dry_run)
    .bind(success)
    .bind(error)
    .bind(now())
    .execute(db)
    .await?;
    Ok(true)
}

#[derive(Debug, sqlx::FromRow)]
pub struct LogRow {
    pub rule_name: String,
    pub task_name: String,
    pub task_url: String,
    pub old_due: i64,
    pub new_due: i64,
    pub mode: String,
    pub dry_run: bool,
    pub success: bool,
    pub error: Option<String>,
    pub created_at: i64,
}

pub async fn recent_log(db: &SqlitePool, guild_id: u64, limit: i64) -> Result<Vec<LogRow>> {
    Ok(sqlx::query_as(
        "SELECT rule_name, task_name, task_url, old_due, new_due, mode, dry_run, success, error,
                created_at
         FROM recurring_log WHERE guild_id = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(sf(guild_id))
    .bind(limit)
    .fetch_all(db)
    .await?)
}

pub async fn prune(db: &SqlitePool) -> Result<u64> {
    let log = sqlx::query("DELETE FROM recurring_log WHERE created_at < ?")
        .bind(now() - LOG_RETENTION_SECS)
        .execute(db)
        .await?;
    let done = sqlx::query("DELETE FROM recurring_done WHERE due_ms < ?")
        .bind((now() - DONE_RETENTION_SECS) * 1000)
        .execute(db)
        .await?;
    Ok(log.rows_affected() + done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util;

    #[test]
    fn rule_texts() {
        let mut rule = Rule::new();
        assert_eq!(rule.interval_text(), "Every week");
        rule.every = 3;
        rule.unit = Unit::Month;
        assert_eq!(rule.interval_text(), "Every 3 months");
        assert_eq!(rule.status_text(), "first open status of the list");
        for unit in Unit::ALL {
            assert_eq!(Unit::parse(unit.as_str()), Some(unit));
        }
        assert_eq!(Unit::parse("fortnight"), None);
        assert_eq!(Trigger::parse("schedule"), Some(Trigger::Schedule));
        assert_eq!(Mode::parse("create"), Some(Mode::Create));
        assert_eq!(Mode::parse("delete"), None);
    }

    #[tokio::test]
    async fn config_rules_and_lock_round_trip() {
        let db = test_util::pool().await;
        assert_eq!(
            load_config(&db, 1).await.unwrap(),
            RecurringConfig::default()
        );
        assert!(!try_lock(&db, 1, 100).await.unwrap(), "no config row yet");

        let cfg = RecurringConfig {
            team_id: Some("100".into()),
            team_name: Some("Team".into()),
            timezone: "Europe/Warsaw".into(),
            check_minutes: 30,
            dry_run: false,
            last_run_at: None,
        };
        save_config(&db, 1, &cfg, "root").await.unwrap();
        set_last_run(&db, 1, 500).await.unwrap();
        let loaded = load_config(&db, 1).await.unwrap();
        assert_eq!(loaded.last_run_at, Some(500));
        assert_eq!(loaded.next_run_at(), Some(500 + 1800));
        save_config(&db, 1, &cfg, "root").await.unwrap();
        assert_eq!(load_config(&db, 1).await.unwrap().last_run_at, Some(500));

        assert!(try_lock(&db, 1, 1000).await.unwrap());
        assert!(!try_lock(&db, 1, 1001).await.unwrap(), "held");
        assert!(
            try_lock(&db, 1, 1000 + STALE_LOCK_SECS + 1).await.unwrap(),
            "stale"
        );
        unlock(&db, 1).await.unwrap();
        assert!(try_lock(&db, 1, 5000).await.unwrap());

        let mut rule = Rule::new();
        rule.name = "Weekly".into();
        rule.tag = "weekly".into();
        let id = insert_rule(&db, 1, &rule, "root").await.unwrap();
        assert!(tag_taken(&db, 1, "weekly", 0).await.unwrap());
        assert!(!tag_taken(&db, 1, "weekly", id).await.unwrap());
        assert!(!tag_taken(&db, 2, "weekly", 0).await.unwrap());
        rule.id = id;
        rule.mode = Mode::Create;
        rule.trigger = Trigger::Schedule;
        update_rule(&db, 1, &rule, "root").await.unwrap();
        assert_eq!(load_rule(&db, 1, id).await.unwrap(), Some(rule));
        assert_eq!(load_rule(&db, 2, id).await.unwrap(), None);
        assert!(!delete_rule(&db, 2, id).await.unwrap());
        assert!(delete_rule(&db, 1, id).await.unwrap());
    }

    #[tokio::test]
    async fn log_skips_repeated_dry_runs_and_failures() {
        let db = test_util::pool().await;
        let mut rule = Rule::new();
        rule.id = 1;
        rule.name = "Weekly".into();
        let entry = |dry_run, error: Option<&str>| LogEntry {
            rule: &rule,
            task_id: "t1",
            task_name: "Task",
            task_url: "",
            old_due: 10,
            new_due: 20,
            dry_run,
            error: error.map(str::to_owned),
        };
        assert!(log(&db, 1, entry(true, None)).await.unwrap());
        assert!(!log(&db, 1, entry(true, None)).await.unwrap());
        assert!(log(&db, 1, entry(false, Some("boom"))).await.unwrap());
        assert!(!log(&db, 1, entry(false, Some("boom"))).await.unwrap());
        assert!(log(&db, 1, entry(false, Some("other"))).await.unwrap());
        assert!(log(&db, 1, entry(false, None)).await.unwrap());
        assert!(
            log(&db, 1, entry(false, None)).await.unwrap(),
            "successes always"
        );
        assert_eq!(recent_log(&db, 1, 10).await.unwrap().len(), 5);
    }

    #[tokio::test]
    async fn prune_keeps_recent_rows() {
        let db = test_util::pool().await;
        let recent_ms = now() * 1000;
        let old_ms = (now() - DONE_RETENTION_SECS - 10) * 1000;
        mark_done(&db, 1, "old", old_ms, 1, None).await.unwrap();
        mark_done(&db, 1, "new", recent_ms, 1, Some("copy"))
            .await
            .unwrap();
        assert_eq!(prune(&db).await.unwrap(), 1);
        let markers = done_markers(&db, 1).await.unwrap();
        assert_eq!(markers, HashSet::from([("new".to_owned(), recent_ms)]));
    }
}
