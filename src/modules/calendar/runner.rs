//! Posting due digests and reminders, called once a minute per guild by the scheduler.

use std::collections::HashSet;

use anyhow::Result;
use async_trait::async_trait;
use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateEmbed, CreateMessage, Http, Timestamp, UserId,
};

use super::events::{self, Event, FetchError};
use super::render::{self, Ctx, OutMessage};
use super::schedule::{self, Digest, GRACE_SECS};
use super::{
    ID, Kind, LogEntry, Rule, list_rules, load_config, log, mark_sent, set_last_run, was_sent,
};
use crate::bot::members::guild_members;
use crate::clickup::client::User;
use crate::clickup::mapping::Resolver;
use crate::modules::{bump_counter, clickup};
use crate::state::AppState;

/// Upper bound per rule and tick, so a backlog cannot flood a channel at once.
pub const MAX_REMINDERS_PER_TICK: usize = 10;

#[async_trait]
pub trait Outbox: Send + Sync {
    /// Posts `msg` to a channel and adds its reactions in order. `Ok` carries a reaction
    /// error: the message itself made it, so the post does not count as failed.
    async fn post(&self, channel_id: u64, msg: &OutMessage) -> Result<Option<String>, String>;
}

pub struct DiscordOutbox<'a> {
    http: &'a Http,
}

impl<'a> DiscordOutbox<'a> {
    pub fn new(http: &'a Http) -> Self {
        Self { http }
    }
}

fn short(e: impl ToString) -> String {
    let mut s = e.to_string();
    s.truncate(s.floor_char_boundary(300));
    s
}

#[async_trait]
impl Outbox for DiscordOutbox<'_> {
    async fn post(&self, channel_id: u64, msg: &OutMessage) -> Result<Option<String>, String> {
        // Only the listed users may be pinged, never roles, @everyone or @here.
        let mentions =
            CreateAllowedMentions::new().users(msg.mention_users.iter().map(|id| UserId::new(*id)));
        let mut create = CreateMessage::new().allowed_mentions(mentions);
        if !msg.content.is_empty() {
            create = create.content(msg.content.clone());
        }
        if let Some(e) = &msg.embed {
            let mut embed = CreateEmbed::new()
                .description(e.description.clone())
                .color(e.color);
            if let Some(title) = &e.title {
                embed = embed.title(title.clone());
            }
            if let Some(url) = &e.url {
                embed = embed.url(url.clone());
            }
            if let Some(ts) = e
                .timestamp
                .and_then(|s| Timestamp::from_unix_timestamp(s).ok())
            {
                embed = embed.timestamp(ts);
            }
            create = create.embed(embed);
        }
        let sent = ChannelId::new(channel_id)
            .send_message(self.http, create)
            .await
            .map_err(short)?;
        let mut reaction_error = None;
        for r in &msg.reactions {
            if let Err(e) = sent.react(self.http, r.to_reaction_type()).await {
                reaction_error.get_or_insert_with(|| short(e));
            }
        }
        Ok(reaction_error)
    }
}

#[derive(Default)]
struct Outcome {
    posted: usize,
    failed: usize,
    error: Option<String>,
}

async fn post_all(outbox: &dyn Outbox, channel_id: u64, msgs: &[OutMessage]) -> Outcome {
    let mut out = Outcome::default();
    for msg in msgs {
        match outbox.post(channel_id, msg).await {
            Ok(reaction_error) => {
                out.posted += 1;
                if let Some(e) = reaction_error {
                    out.error
                        .get_or_insert(format!("Posted, but adding a reaction failed: {e}"));
                }
            }
            Err(e) => {
                out.failed += 1;
                out.error.get_or_insert(e);
            }
        }
    }
    out
}

async fn record(
    state: &AppState,
    guild_id: u64,
    rule: &Rule,
    events: usize,
    out: Outcome,
    is_test: bool,
) -> Result<()> {
    let entry = LogEntry {
        rule,
        kind: rule.kind.as_str(),
        events,
        success: out.failed == 0,
        error: out.error,
        is_test,
    };
    log(&state.db, guild_id, entry).await?;
    if is_test {
        return Ok(());
    }
    // A digest counts once however many messages it took, a reminder per message.
    let (key, sent) = match rule.kind {
        Kind::Digest => ("digests", out.posted.min(1)),
        Kind::Reminder => ("reminders", out.posted),
    };
    for _ in 0..sent {
        bump_counter(&state.db, guild_id, ID, key).await?;
    }
    for _ in 0..out.failed {
        bump_counter(&state.db, guild_id, ID, "failed").await?;
    }
    Ok(())
}

/// Matches the assignees of `events` to members (explicit mappings first).
async fn resolver_for(state: &AppState, guild_id: u64, events: &[Event]) -> Result<Resolver> {
    let mut seen = HashSet::new();
    let people: Vec<User> = events
        .iter()
        .flat_map(|e| e.assignees.iter())
        .filter(|p| seen.insert(p.id))
        .cloned()
        .collect();
    clickup::resolver(state, guild_id, &people).await
}

/// Posts every digest and reminder that is due at `now`.
pub async fn run(state: &AppState, guild_id: u64, now: i64, outbox: &dyn Outbox) -> Result<()> {
    let cfg = load_config(&state.db, guild_id).await?;
    if cfg.list_id.is_none() {
        return Ok(());
    }
    let rules: Vec<Rule> = list_rules(&state.db, guild_id)
        .await?
        .into_iter()
        .filter(|r| r.enabled && r.channel_id.is_some())
        .collect();
    if rules.is_empty() {
        return Ok(());
    }
    let tz = cfg.tz();

    let mut due_digests = Vec::new();
    for rule in rules.iter().filter(|r| r.kind == Kind::Digest) {
        let occurrence = schedule::latest_occurrence(now, rule.days, rule.time_of_day, &tz);
        match schedule::digest_decision(now, rule.last_run_at, occurrence) {
            Digest::Run(at) => due_digests.push((rule, at)),
            Digest::Skip(at) => {
                set_last_run(&state.db, rule.id, at).await?;
                let error = format!(
                    "Not posted within {} hours of the scheduled time. The bot was offline or \
                     ClickUp could not be read.",
                    GRACE_SECS / 3600
                );
                let entry = LogEntry {
                    rule,
                    kind: "missed",
                    events: 0,
                    success: false,
                    error: Some(error),
                    is_test: false,
                };
                log(&state.db, guild_id, entry).await?;
                bump_counter(&state.db, guild_id, ID, "missed").await?;
            }
            Digest::Wait => {}
        }
    }
    let reminders: Vec<&Rule> = rules.iter().filter(|r| r.kind == Kind::Reminder).collect();
    if due_digests.is_empty() && reminders.is_empty() {
        return Ok(());
    }

    let horizon = rules
        .iter()
        .map(Rule::horizon_secs)
        .max()
        .unwrap_or_default();
    let max_age = if due_digests.is_empty() {
        events::MAX_AGE
    } else {
        events::FRESH
    };
    let fetched = match events::cached(state, guild_id, &cfg, now, horizon, max_age).await? {
        Ok(f) => f,
        Err(e) => {
            // Digests stay pending and are retried next minute until the grace runs out.
            // API errors are already logged and shown on the key card by `tokens::track`.
            tracing::debug!(guild_id, "calendar could not read ClickUp: {}", e.message());
            return Ok(());
        }
    };
    let upcoming = fetched.between(now, i64::MAX);
    let resolver = resolver_for(state, guild_id, &upcoming).await?;
    let list_name = cfg.list_name.clone().unwrap_or_default();
    let ctx = Ctx {
        resolver: &resolver,
        list_name: &list_name,
        now,
    };

    for (rule, occurrence) in due_digests {
        let until = now + i64::from(rule.lookahead_hours) * 3600;
        let window: Vec<Event> = upcoming.iter().filter(|e| e.at <= until).cloned().collect();
        let msgs = render::digest_messages(rule, &window, &ctx);
        let channel = rule.channel_id.unwrap_or_default();
        let out = post_all(outbox, channel, &msgs).await;
        // Marked handled even when Discord refused, so a broken channel is not retried
        // every minute. The failure is in the log.
        set_last_run(&state.db, rule.id, occurrence).await?;
        record(state, guild_id, rule, window.len(), out, false).await?;
    }

    for rule in reminders {
        let lead = i64::from(rule.lead_minutes) * 60;
        let channel = rule.channel_id.unwrap_or_default();
        let mut handled = 0;
        for ev in upcoming
            .iter()
            .filter(|e| schedule::reminder_due(now, e.at, lead))
        {
            if handled >= MAX_REMINDERS_PER_TICK {
                break;
            }
            if was_sent(&state.db, rule.id, &ev.task_id, ev.at).await? {
                continue;
            }
            let msg = render::event_message(rule, ev, &ctx);
            let out = post_all(outbox, channel, std::slice::from_ref(&msg)).await;
            mark_sent(&state.db, rule.id, &ev.task_id, ev.at).await?;
            record(state, guild_id, rule, 1, out, false).await?;
            handled += 1;
        }
    }
    Ok(())
}

/// Messages a rule would post right now: a digest of the current window (posted even when
/// empty), or a reminder for the next event. When ClickUp cannot be read, a sample event is
/// used and the error is returned next to the messages.
pub async fn sample_messages(
    state: &AppState,
    guild_id: u64,
    rule: &Rule,
    now: i64,
) -> Result<(Vec<OutMessage>, Option<FetchError>)> {
    let cfg = load_config(&state.db, guild_id).await?;
    let (upcoming, error) = match events::cached(
        state,
        guild_id,
        &cfg,
        now,
        rule.horizon_secs(),
        events::MAX_AGE,
    )
    .await?
    {
        Ok(f) => (f.between(now, i64::MAX), None),
        Err(e) => (vec![Event::sample(now)], Some(e)),
    };
    let resolver = resolver_for(state, guild_id, &upcoming).await?;
    let list_name = cfg.list_name.clone().unwrap_or_else(|| "Calendar".into());
    let ctx = Ctx {
        resolver: &resolver,
        list_name: &list_name,
        now,
    };
    let msgs = match rule.kind {
        Kind::Digest => {
            let until = now + i64::from(rule.lookahead_hours) * 3600;
            let window: Vec<Event> = upcoming.into_iter().filter(|e| e.at <= until).collect();
            let mut always = rule.clone();
            always.post_when_empty = true;
            render::digest_messages(&always, &window, &ctx)
        }
        Kind::Reminder => {
            let ev = upcoming
                .into_iter()
                .next()
                .unwrap_or_else(|| Event::sample(now));
            vec![render::event_message(rule, &ev, &ctx)]
        }
    };
    Ok((msgs, error))
}

/// Readable text of the first message a rule would post, for the dashboard.
pub async fn preview(state: &AppState, guild_id: u64, rule: &Rule, now: i64) -> Result<String> {
    let (msgs, _) = sample_messages(state, guild_id, rule, now).await?;
    let Some(msg) = msgs.first() else {
        return Ok(String::new());
    };
    let mut text = msg.content.clone();
    if let Some(e) = &msg.embed {
        for part in [e.title.as_deref(), Some(e.description.as_str())]
            .into_iter()
            .flatten()
        {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(part);
        }
    }
    let cfg = load_config(&state.db, guild_id).await?;
    let members = guild_members(state, guild_id).await;
    let more = if msgs.len() > 1 {
        format!("\n\n(and {} more messages)", msgs.len() - 1)
    } else {
        String::new()
    };
    Ok(render::humanize(&text, &cfg.tz(), &members, now) + &more)
}

#[derive(Debug, PartialEq)]
pub enum TestOutcome {
    Sent,
    Failed,
    Fetch(FetchError),
}

/// Posts what the rule would post now, without pings, counting it or changing any schedule
/// state.
pub async fn send_test(
    state: &AppState,
    guild_id: u64,
    rule: &Rule,
    outbox: &dyn Outbox,
    now: i64,
) -> Result<TestOutcome> {
    let (mut msgs, error) = sample_messages(state, guild_id, rule, now).await?;
    if let Some(e) = error {
        return Ok(TestOutcome::Fetch(e));
    }
    // Tests never ping anyone; mentions still render as names.
    for msg in &mut msgs {
        msg.mention_users.clear();
    }
    let out = post_all(outbox, rule.channel_id.unwrap_or_default(), &msgs).await;
    let ok = out.failed == 0;
    record(state, guild_id, rule, msgs.len(), out, true).await?;
    Ok(if ok {
        TestOutcome::Sent
    } else {
        TestOutcome::Failed
    })
}

#[cfg(test)]
pub mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::clickup::mock::{self, MockClickUp};
    use crate::clickup::tokens;
    use crate::modules::calendar::{
        CalendarConfig, Layout, insert_rule, load_rule, reactions, recent_log, save_config,
    };
    use crate::modules::{counter, set_enabled};
    use crate::secrets::SecretString;
    use crate::test_util::{self, CHANNEL_ID, GUILD_ID, MEMBER_ID};

    /// Remembers posts instead of sending them. Fails every post while `fail` is set.
    #[derive(Default)]
    pub struct RecordingOutbox {
        pub posts: Mutex<Vec<(u64, OutMessage)>>,
        pub fail: Mutex<Option<String>>,
    }

    #[async_trait]
    impl Outbox for RecordingOutbox {
        async fn post(&self, channel_id: u64, msg: &OutMessage) -> Result<Option<String>, String> {
            if let Some(e) = self.fail.lock().unwrap().clone() {
                return Err(e);
            }
            self.posts.lock().unwrap().push((channel_id, msg.clone()));
            Ok(None)
        }
    }

    impl RecordingOutbox {
        fn take(&self) -> Vec<(u64, OutMessage)> {
            std::mem::take(&mut *self.posts.lock().unwrap())
        }
    }

    // Thursday 2026-10-15 09:00:00 UTC.
    const NINE: i64 = 1_792_054_800;

    struct Setup {
        state: AppState,
        mock: MockClickUp,
        outbox: RecordingOutbox,
    }

    /// Connected fake guild, shared ClickUp key, list selected, UTC.
    async fn setup() -> Setup {
        let mut state = test_util::state().await;
        let mock = MockClickUp::start().await;
        state.clickup = mock.client();
        test_util::connect_fake_discord(&state);
        let db = &state.db;
        set_enabled(db, GUILD_ID, tokens::SHARED, true)
            .await
            .unwrap();
        let key = SecretString::new(mock::TOKEN);
        tokens::store(
            db,
            &state.vault,
            GUILD_ID,
            tokens::SHARED,
            &key,
            "Owner",
            "root",
        )
        .await
        .unwrap();
        let cfg = CalendarConfig {
            list_id: Some(mock::LIST_ID.into()),
            list_name: Some("Events / Team / Calendar".into()),
            ..Default::default()
        };
        save_config(db, GUILD_ID, &cfg, "root").await.unwrap();
        Setup {
            state,
            mock,
            outbox: RecordingOutbox::default(),
        }
    }

    fn task_at(id: &str, name: &str, at: i64) -> serde_json::Value {
        let mut t = mock::task(id, name, Some(at * 1000));
        t["assignees"] = json!([{"id": 3, "username": "Anna Nowak", "email": "anna@x.io"}]);
        t
    }

    async fn add_rule(s: &Setup, rule: &Rule) -> Rule {
        let id = insert_rule(&s.state.db, GUILD_ID, rule, "root")
            .await
            .unwrap();
        // Rules are created "now"; move the start back so the test clock decides.
        sqlx::query("UPDATE calendar_rules SET last_run_at = ? WHERE id = ?")
            .bind(rule.last_run_at)
            .bind(id)
            .execute(&s.state.db)
            .await
            .unwrap();
        load_rule(&s.state.db, GUILD_ID, id).await.unwrap().unwrap()
    }

    fn digest() -> Rule {
        let mut r = Rule::new(Kind::Digest);
        r.channel_id = Some(CHANNEL_ID);
        r.time_of_day = 9 * 60;
        r.lookahead_hours = 12;
        r.item_template = "{title} {assignees}".into();
        r.reactions = reactions::parse("✅ ❌").unwrap();
        r
    }

    fn reminder() -> Rule {
        let mut r = Rule::new(Kind::Reminder);
        r.channel_id = Some(CHANNEL_ID);
        r.lead_minutes = 15;
        r.item_template = "{title} {assignees}".into();
        r
    }

    async fn count(s: &Setup, key: &str) -> i64 {
        counter(&s.state.db, Some(GUILD_ID), ID, key).await.unwrap()
    }

    #[tokio::test]
    async fn digest_posts_once_per_occurrence() {
        let s = setup().await;
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![
                task_at("b", "Retro", NINE + 3 * 3600),
                task_at("a", "Standup", NINE + 1800),
                task_at("late", "Tonight", NINE + 20 * 3600),
                task_at("next", "Tomorrow", NINE + 86400 + 3600),
            ],
        );
        add_rule(&s, &digest()).await;

        run(&s.state, GUILD_ID, NINE - 60, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty(), "before 09:00");

        run(&s.state, GUILD_ID, NINE + 30, &s.outbox).await.unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 1);
        let (channel, msg) = &posts[0];
        assert_eq!(*channel, CHANNEL_ID);
        assert!(
            msg.content.starts_with("**Upcoming events** (2)"),
            "{}",
            msg.content
        );
        let standup = msg.content.find("Standup").unwrap();
        assert!(
            standup < msg.content.find("Retro").unwrap(),
            "sorted by time"
        );
        assert!(!msg.content.contains("Tonight"), "outside the 12 h window");
        assert!(
            msg.content.contains(&format!("<@{MEMBER_ID}>")),
            "Anna is matched"
        );
        assert!(msg.mention_users.is_empty(), "but not pinged");
        assert_eq!(msg.reactions.len(), 2);

        run(&s.state, GUILD_ID, NINE + 90, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty(), "same occurrence");
        assert_eq!(count(&s, "digests").await, 1);

        // Next day at 09:00 it posts again.
        run(&s.state, GUILD_ID, NINE + 86400, &s.outbox)
            .await
            .unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].1.content.contains("Tomorrow"));
        assert!(s.mock.requests().iter().all(|r| r.method == "GET"));
    }

    #[tokio::test]
    async fn late_digests_are_skipped_and_logged() {
        let s = setup().await;
        let mut rule = digest();
        rule.last_run_at = Some(NINE - 86400);
        let rule = add_rule(&s, &rule).await;

        run(&s.state, GUILD_ID, NINE + GRACE_SECS + 60, &s.outbox)
            .await
            .unwrap();
        assert!(s.outbox.take().is_empty());
        assert_eq!(count(&s, "missed").await, 1);
        let rule = load_rule(&s.state.db, GUILD_ID, rule.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rule.last_run_at, Some(NINE));
        let log = recent_log(&s.state.db, GUILD_ID, 5).await.unwrap();
        assert_eq!(log[0].kind, "missed");
        assert!(!log[0].success);
        // No ClickUp call was needed for that.
        assert!(s.mock.requests().is_empty());
    }

    #[tokio::test]
    async fn per_event_layout_mentions_and_empty_digests() {
        let s = setup().await;
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![
                task_at("a", "Standup", NINE + 1800),
                task_at("b", "Retro", NINE + 3600),
            ],
        );
        let mut rule = digest();
        rule.layout = Layout::PerEvent;
        rule.mention_assignees = true;
        add_rule(&s, &rule).await;
        let mut empty = digest();
        empty.name = "Empty".into();
        empty.time_of_day = 23 * 60;
        empty.lookahead_hours = 1;
        empty.post_when_empty = true;
        add_rule(&s, &empty).await;

        run(&s.state, GUILD_ID, NINE + 30, &s.outbox).await.unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 2);
        assert!(posts.iter().all(|(_, m)| m.mention_users == [MEMBER_ID]));

        // 23:00, nothing within the next hour: the empty message is posted.
        run(&s.state, GUILD_ID, NINE + 14 * 3600, &s.outbox)
            .await
            .unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].1.content, "No upcoming events.");
    }

    #[tokio::test]
    async fn reminders_fire_once_inside_the_window() {
        let s = setup().await;
        s.mock
            .set_tasks(mock::LIST_ID, vec![task_at("a", "Standup", NINE + 1800)]);
        add_rule(&s, &reminder()).await;

        // 20 minutes before: too early. 15 minutes before: due.
        run(&s.state, GUILD_ID, NINE + 600, &s.outbox)
            .await
            .unwrap();
        assert!(s.outbox.take().is_empty());
        run(&s.state, GUILD_ID, NINE + 900, &s.outbox)
            .await
            .unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].1.content.starts_with("Standup"));
        run(&s.state, GUILD_ID, NINE + 960, &s.outbox)
            .await
            .unwrap();
        assert!(s.outbox.take().is_empty(), "only once");
        assert_eq!(count(&s, "reminders").await, 1);

        // The meeting moves to 10:00: a new reminder before the new time.
        s.mock
            .set_tasks(mock::LIST_ID, vec![task_at("a", "Standup", NINE + 3600)]);
        crate::modules::calendar::events::invalidate(&s.state, GUILD_ID);
        run(&s.state, GUILD_ID, NINE + 2700, &s.outbox)
            .await
            .unwrap();
        assert_eq!(s.outbox.take().len(), 1);

        // Started events are never reminded.
        run(&s.state, GUILD_ID, NINE + 3700, &s.outbox)
            .await
            .unwrap();
        assert!(s.outbox.take().is_empty());
    }

    #[tokio::test]
    async fn disabled_rules_missing_channels_and_lists_do_nothing() {
        let s = setup().await;
        s.mock
            .set_tasks(mock::LIST_ID, vec![task_at("a", "Standup", NINE + 600)]);
        let mut off = reminder();
        off.enabled = false;
        add_rule(&s, &off).await;
        let mut nowhere = reminder();
        nowhere.channel_id = None;
        add_rule(&s, &nowhere).await;
        run(&s.state, GUILD_ID, NINE, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty());
        assert!(
            s.mock.requests().is_empty(),
            "nothing to do, no ClickUp call"
        );

        add_rule(&s, &reminder()).await;
        save_config(&s.state.db, GUILD_ID, &CalendarConfig::default(), "root")
            .await
            .unwrap();
        run(&s.state, GUILD_ID, NINE, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty(), "no list selected");
    }

    #[tokio::test]
    async fn failures_are_counted_and_logged() {
        let s = setup().await;
        s.mock
            .set_tasks(mock::LIST_ID, vec![task_at("a", "Standup", NINE + 600)]);
        add_rule(&s, &reminder()).await;

        // ClickUp down: nothing posted, nothing marked, the API error is counted.
        s.mock.fail_with(Some(503));
        run(&s.state, GUILD_ID, NINE, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty());
        assert_eq!(count(&s, "api_err").await, 1);
        s.mock.fail_with(None);

        // Discord refuses: counted as failed and not retried.
        *s.outbox.fail.lock().unwrap() = Some("Missing Permissions".into());
        run(&s.state, GUILD_ID, NINE + 60, &s.outbox).await.unwrap();
        *s.outbox.fail.lock().unwrap() = None;
        run(&s.state, GUILD_ID, NINE + 120, &s.outbox)
            .await
            .unwrap();
        assert!(s.outbox.take().is_empty());
        assert_eq!(count(&s, "failed").await, 1);
        let log = recent_log(&s.state.db, GUILD_ID, 5).await.unwrap();
        assert_eq!(log[0].error.as_deref(), Some("Missing Permissions"));
    }

    #[tokio::test]
    async fn tests_and_previews_change_nothing() {
        let s = setup().await;
        s.mock
            .set_tasks(mock::LIST_ID, vec![task_at("a", "Standup", NINE + 1800)]);
        let rule = add_rule(&s, &reminder()).await;

        let outcome = send_test(&s.state, GUILD_ID, &rule, &s.outbox, NINE)
            .await
            .unwrap();
        assert_eq!(outcome, TestOutcome::Sent);
        assert_eq!(s.outbox.take().len(), 1);
        assert_eq!(count(&s, "reminders").await, 0);
        assert!(recent_log(&s.state.db, GUILD_ID, 1).await.unwrap()[0].is_test);
        assert!(
            !was_sent(&s.state.db, rule.id, "a", NINE + 1800)
                .await
                .unwrap()
        );

        let text = preview(&s.state, GUILD_ID, &rule, NINE).await.unwrap();
        assert!(text.starts_with("Standup @Anna Nowak"), "{text}");

        // Without a list the preview falls back to a sample, the test reports the problem.
        save_config(&s.state.db, GUILD_ID, &CalendarConfig::default(), "root")
            .await
            .unwrap();
        let text = preview(&s.state, GUILD_ID, &rule, NINE).await.unwrap();
        assert!(text.starts_with("Weekly sync"), "{text}");
        let outcome = send_test(&s.state, GUILD_ID, &rule, &s.outbox, NINE)
            .await
            .unwrap();
        assert_eq!(outcome, TestOutcome::Fetch(FetchError::NoList));
        assert!(s.outbox.take().is_empty());
    }
}
