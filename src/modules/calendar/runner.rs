//! Posting due digests and reminders, called once a minute per project by the scheduler.

use std::collections::HashSet;

use anyhow::Result;
use async_trait::async_trait;
use serenity::all::{
    ChannelId, CreateAllowedMentions, CreateEmbed, CreateMessage, Http, RoleId, Timestamp, UserId,
};

use super::events::{self, Event, FetchError};
use super::render::{self, Ctx, OutMessage};
use super::schedule::{self, Digest, GRACE_SECS};
use super::{
    ID, Kind, LogEntry, Rule, list_rules, load_config, log, mark_sent, set_last_run, was_sent,
};
use crate::clickup::client::User;
use crate::clickup::mapping::Resolver;
use crate::discord::connection::{Conn, connection};
use crate::discord::info;
use crate::discord::members::guild_members;
use crate::modules::{bump_counter, clickup};
use crate::state::AppState;

/// Upper bound per rule and tick, so a backlog cannot flood a channel at once.
pub const MAX_REMINDERS_PER_TICK: usize = 10;
/// Messages shown in a dashboard preview.
const PREVIEW_MESSAGES: usize = 2;

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
        // Only the listed users and roles may be pinged, never @everyone or @here.
        let mentions = CreateAllowedMentions::new()
            .users(msg.mention_users.iter().map(|id| UserId::new(*id)))
            .roles(msg.mention_roles.iter().map(|id| RoleId::new(*id)));
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
    project_id: i64,
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
    log(&state.db, project_id, entry).await?;
    if is_test {
        return Ok(());
    }
    // A digest counts once however many messages it took, a reminder per message.
    let (key, sent) = match rule.kind {
        Kind::Digest => ("digests", out.posted.min(1)),
        Kind::Reminder => ("reminders", out.posted),
    };
    for _ in 0..sent {
        bump_counter(&state.db, project_id, ID, key).await?;
    }
    for _ in 0..out.failed {
        bump_counter(&state.db, project_id, ID, "failed").await?;
    }
    Ok(())
}

/// Matches the assignees of `events` to members of the server of `conn` (explicit mappings
/// first).
async fn resolver_for(
    state: &AppState,
    project_id: i64,
    conn: Option<&Conn>,
    events: &[Event],
) -> Result<Resolver> {
    let mut seen = HashSet::new();
    let people: Vec<User> = events
        .iter()
        .flat_map(|e| e.assignees.iter())
        .filter(|p| seen.insert(p.id))
        .cloned()
        .collect();
    clickup::resolver(state, project_id, conn, &people).await
}

/// The events of a digest window, `from <= at < to`.
fn in_window(events: &[Event], from: i64, to: i64) -> Vec<Event> {
    events
        .iter()
        .filter(|e| (from..to).contains(&e.at))
        .cloned()
        .collect()
}

/// Posts every digest and reminder that is due at `now`.
pub async fn run(state: &AppState, project_id: i64, now: i64, outbox: &dyn Outbox) -> Result<()> {
    let cfg = load_config(&state.db, project_id).await?;
    if cfg.list_id.is_none() {
        return Ok(());
    }
    let rules: Vec<Rule> = list_rules(&state.db, project_id)
        .await?
        .into_iter()
        .filter(|r| r.enabled && r.channel_id.is_some())
        .collect();
    if rules.is_empty() {
        return Ok(());
    }

    let mut due_digests = Vec::new();
    for rule in rules.iter().filter(|r| r.kind == Kind::Digest) {
        let occurrence = schedule::latest_occurrence(now, rule.days, rule.time_of_day, &rule.tz());
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
                log(&state.db, project_id, entry).await?;
                bump_counter(&state.db, project_id, ID, "missed").await?;
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
    let fetched = match events::cached(state, project_id, &cfg, now, horizon, events::FRESH).await?
    {
        Ok(f) => f,
        Err(e) => {
            // Digests stay pending and are retried next minute until the grace runs out.
            // API errors are already logged and shown on the key card by `keys::track`.
            tracing::debug!(
                project_id,
                "calendar could not read ClickUp: {}",
                e.message()
            );
            return Ok(());
        }
    };
    let upcoming = fetched.between(now, i64::MAX);
    let conn = connection(state, project_id, ID).await?.ok();
    let resolver = resolver_for(state, project_id, conn.as_ref(), &upcoming).await?;
    let tag_roles = clickup::tag_roles(state, project_id, conn.as_ref()).await?;
    let list_name = cfg.list_name.clone().unwrap_or_default();
    let ctx = Ctx {
        resolver: &resolver,
        tag_roles: &tag_roles,
        list_name: &list_name,
    };

    for (rule, occurrence) in due_digests {
        // Counted from the scheduled time, so a late post lists the same events.
        let (from, to) = rule.digest_window(occurrence);
        let window = in_window(&upcoming, from, to);
        let msgs = render::digest_messages(rule, &window, from, &ctx);
        let channel = rule.channel_id.unwrap_or_default();
        let out = post_all(outbox, channel, &msgs).await;
        // Marked handled even when Discord refused, so a broken channel is not retried
        // every minute. The failure is in the log.
        set_last_run(&state.db, rule.id, occurrence).await?;
        tracing::info!(
            project_id,
            rule = %rule.name,
            events = window.len(),
            posted = out.posted,
            failed = out.failed,
            "calendar digest"
        );
        record(state, project_id, rule, window.len(), out, false).await?;
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
            tracing::info!(
                project_id,
                rule = %rule.name,
                task = %ev.task_id,
                ok = out.failed == 0,
                "calendar reminder"
            );
            record(state, project_id, rule, 1, out, false).await?;
            handled += 1;
        }
    }
    Ok(())
}

/// Messages of a rule's next post with the events known now: a digest of the window of its
/// next scheduled time (posted even when empty), or a reminder for the next event. When
/// ClickUp cannot be read, a sample event is used and the error is returned next to the
/// messages.
pub async fn sample_messages(
    state: &AppState,
    project_id: i64,
    rule: &Rule,
    now: i64,
) -> Result<(Vec<OutMessage>, Option<FetchError>)> {
    let cfg = load_config(&state.db, project_id).await?;
    let (from, to) = match rule.kind {
        Kind::Digest => rule.digest_window(
            schedule::next_occurrence(now, rule.days, rule.time_of_day, &rule.tz()).unwrap_or(now),
        ),
        Kind::Reminder => (now, now + rule.horizon_secs()),
    };
    // Inside the window of a digest, so the preview is not empty.
    let sample = Event::sample(match rule.kind {
        Kind::Digest => from + (to - from) / 2,
        Kind::Reminder => now + 3600,
    });
    let (upcoming, error) =
        match events::cached(state, project_id, &cfg, now, to - now, events::MAX_AGE).await? {
            Ok(f) => (f.between(now, i64::MAX), None),
            Err(e) => (vec![sample.clone()], Some(e)),
        };
    let conn = connection(state, project_id, ID).await?.ok();
    let resolver = resolver_for(state, project_id, conn.as_ref(), &upcoming).await?;
    let tag_roles = clickup::tag_roles(state, project_id, conn.as_ref()).await?;
    let list_name = cfg.list_name.clone().unwrap_or_else(|| "Calendar".into());
    let ctx = Ctx {
        resolver: &resolver,
        tag_roles: &tag_roles,
        list_name: &list_name,
    };
    let msgs = match rule.kind {
        Kind::Digest => {
            let window = in_window(&upcoming, from, to);
            let mut always = rule.clone();
            always.post_when_empty = true;
            render::digest_messages(&always, &window, from, &ctx)
        }
        Kind::Reminder => {
            let ev = upcoming.into_iter().next().unwrap_or(sample);
            vec![render::event_message(rule, &ev, &ctx)]
        }
    };
    Ok((msgs, error))
}

fn message_text(msg: &OutMessage) -> String {
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
    text
}

/// Readable text of the first messages of a rule's next post, for the dashboard. Digest
/// times are shown in the rule's time zone, reminder times in the server default.
pub async fn preview(state: &AppState, project_id: i64, rule: &Rule, now: i64) -> Result<String> {
    let (msgs, _) = sample_messages(state, project_id, rule, now).await?;
    if msgs.is_empty() {
        return Ok(String::new());
    }
    let shown = msgs.len().min(PREVIEW_MESSAGES);
    let text = msgs[..shown]
        .iter()
        .map(message_text)
        .collect::<Vec<_>>()
        .join("\n\n--- next message ---\n\n");
    let tz = match rule.kind {
        Kind::Digest => rule.tz(),
        Kind::Reminder => load_config(&state.db, project_id).await?.tz(),
    };
    let conn = connection(state, project_id, ID).await?.ok();
    let members = guild_members(state, conn.as_ref()).await;
    let roles = conn
        .as_ref()
        .map(|c| info::roles(&c.handle.cache, c.guild_id))
        .unwrap_or_default();
    let more = if msgs.len() > shown {
        format!("\n\n(and {} more messages)", msgs.len() - shown)
    } else {
        String::new()
    };
    Ok(render::humanize(&text, &tz, &members, &roles, now) + &more)
}

#[derive(Debug, PartialEq)]
pub enum TestOutcome {
    Sent,
    Failed,
    Fetch(FetchError),
}

/// Posts what the rule's next post would look like (see [`sample_messages`]), without pings,
/// counting it or changing any schedule state.
pub async fn send_test(
    state: &AppState,
    project_id: i64,
    rule: &Rule,
    outbox: &dyn Outbox,
    now: i64,
) -> Result<TestOutcome> {
    let (mut msgs, error) = sample_messages(state, project_id, rule, now).await?;
    if let Some(e) = error {
        return Ok(TestOutcome::Fetch(e));
    }
    // Tests never ping anyone; mentions still render as names.
    for msg in &mut msgs {
        msg.mention_users.clear();
        msg.mention_roles.clear();
    }
    let out = post_all(outbox, rule.channel_id.unwrap_or_default(), &msgs).await;
    let ok = out.failed == 0;
    record(state, project_id, rule, msgs.len(), out, true).await?;
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
    use crate::keys::{self, Service};
    use crate::modules::calendar::{
        CalendarConfig, Layout, insert_rule, load_rule, reactions, recent_log, save_config,
    };
    use crate::modules::clickup::set_tag_role;
    use crate::modules::{counter, set_enabled};
    use crate::secrets::SecretString;
    use crate::test_util::{self, CHANNEL_ID, MEMBER_ID, PID, ROLE_ID};

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

    /// Connected fake Discord server, shared ClickUp key, list selected, UTC.
    async fn setup() -> Setup {
        let mut state = test_util::state().await;
        let mock = MockClickUp::start().await;
        state.clickup = mock.client();
        test_util::connect_fake_discord(&state, PID).await;
        let db = &state.db;
        set_enabled(db, PID, "clickup", true).await.unwrap();
        let key = SecretString::new(mock::TOKEN);
        keys::store(
            db,
            &state.vault,
            PID,
            Service::ClickUp,
            "clickup",
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
        save_config(db, PID, &cfg, "root").await.unwrap();
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
        let id = insert_rule(&s.state.db, PID, rule, "root").await.unwrap();
        // Rules are created "now"; move the start back so the test clock decides.
        sqlx::query("UPDATE calendar_rules SET last_run_at = ? WHERE id = ?")
            .bind(rule.last_run_at)
            .bind(id)
            .execute(&s.state.db)
            .await
            .unwrap();
        load_rule(&s.state.db, PID, id).await.unwrap().unwrap()
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
        counter(&s.state.db, Some(PID), ID, key).await.unwrap()
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

        run(&s.state, PID, NINE - 60, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty(), "before 09:00");

        run(&s.state, PID, NINE + 30, &s.outbox).await.unwrap();
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

        run(&s.state, PID, NINE + 90, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty(), "same occurrence");
        assert_eq!(count(&s, "digests").await, 1);

        // Next day at 09:00 it posts again.
        run(&s.state, PID, NINE + 86400, &s.outbox).await.unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].1.content.contains("Tomorrow"));
        assert!(s.mock.requests().iter().all(|r| r.method == "GET"));
    }

    #[tokio::test]
    async fn digests_follow_the_rule_time_zone() {
        let s = setup().await;
        s.mock
            .set_tasks(mock::LIST_ID, vec![task_at("a", "Standup", NINE + 1800)]);
        // 09:00 in Warsaw is 07:00 UTC in October (CEST).
        let mut rule = digest();
        rule.timezone = "Europe/Warsaw".into();
        add_rule(&s, &rule).await;

        run(&s.state, PID, NINE - 7200 - 60, &s.outbox)
            .await
            .unwrap();
        assert!(s.outbox.take().is_empty(), "before 09:00 Warsaw");
        run(&s.state, PID, NINE - 7200 + 30, &s.outbox)
            .await
            .unwrap();
        assert_eq!(s.outbox.take().len(), 1);
        run(&s.state, PID, NINE + 30, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty(), "09:00 UTC is not the schedule");
    }

    #[tokio::test]
    async fn digest_offset_lists_only_the_shifted_window() {
        let s = setup().await;
        let midnight = NINE + 15 * 3600;
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![
                task_at("a", "Tonight", NINE + 12 * 3600),
                task_at("b", "Midnight", midnight),
                task_at("c", "Afternoon", NINE + 30 * 3600),
                task_at("d", "Saturday", midnight + 86400),
            ],
        );
        // Posted at 20:00, lists the next day: 00:00 Friday up to 00:00 Saturday.
        let mut rule = digest();
        rule.time_of_day = 20 * 60;
        rule.offset_minutes = 240;
        rule.lookahead_hours = 24;
        rule.header_template = "**{date}** ({count})".into();
        let rule = add_rule(&s, &rule).await;

        // The preview at 14:00 already shows the window of the 20:00 post.
        let text = preview(&s.state, PID, &rule, NINE + 5 * 3600)
            .await
            .unwrap();
        assert!(text.starts_with("**2026-10-16** (2)"), "{text}");
        assert!(text.contains("Midnight") && text.contains("Afternoon"));
        assert!(!text.contains("Tonight") && !text.contains("Saturday"));

        run(&s.state, PID, NINE + 11 * 3600 + 30, &s.outbox)
            .await
            .unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 1);
        let msg = &posts[0].1.content;
        assert!(
            msg.starts_with(&format!("**<t:{midnight}:D>** (2)")),
            "{msg}"
        );
        let first = msg.find("Midnight").unwrap();
        assert!(first < msg.find("Afternoon").unwrap());
        assert!(!msg.contains("Tonight"), "before the window");
        assert!(!msg.contains("Saturday"), "the window end is excluded");
    }

    #[tokio::test]
    async fn roles_from_tags_are_pinged_only_when_enabled() {
        let s = setup().await;
        let mut task = task_at("a", "Standup", NINE + 600);
        task["tags"] = json!([{"name": "Meeting"}, {"name": "unmapped"}]);
        s.mock.set_tasks(mock::LIST_ID, vec![task]);
        set_tag_role(&s.state.db, PID, "meeting", Some(ROLE_ID), "root")
            .await
            .unwrap();
        let mut rule = reminder();
        rule.item_template = "{title} {roles}".into();
        rule.mention_roles = true;
        let rule = add_rule(&s, &rule).await;

        run(&s.state, PID, NINE, &s.outbox).await.unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 1);
        let msg = &posts[0].1;
        assert_eq!(msg.content, format!("Standup <@&{ROLE_ID}>"));
        assert_eq!(msg.mention_roles, [ROLE_ID]);
        assert!(msg.mention_users.is_empty());

        // Test messages never ping, the preview shows role names.
        send_test(&s.state, PID, &rule, &s.outbox, NINE)
            .await
            .unwrap();
        let posts = s.outbox.take();
        assert!(posts[0].1.mention_roles.is_empty());
        assert!(posts[0].1.content.contains(&format!("<@&{ROLE_ID}>")));
        let text = preview(&s.state, PID, &rule, NINE).await.unwrap();
        assert_eq!(text, "Standup @Team");
    }

    #[tokio::test]
    async fn late_digests_are_skipped_and_logged() {
        let s = setup().await;
        let mut rule = digest();
        rule.last_run_at = Some(NINE - 86400);
        let rule = add_rule(&s, &rule).await;

        run(&s.state, PID, NINE + GRACE_SECS + 60, &s.outbox)
            .await
            .unwrap();
        assert!(s.outbox.take().is_empty());
        assert_eq!(count(&s, "missed").await, 1);
        let rule = load_rule(&s.state.db, PID, rule.id).await.unwrap().unwrap();
        assert_eq!(rule.last_run_at, Some(NINE));
        let log = recent_log(&s.state.db, PID, 5).await.unwrap();
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

        run(&s.state, PID, NINE + 30, &s.outbox).await.unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 3, "the header, then one message per event");
        let header = &posts[0].1;
        assert_eq!(header.content, "**Upcoming events** (2)");
        assert!(header.reactions.is_empty());
        assert!(posts[1].1.content.starts_with("Standup"));
        assert!(
            posts[1..]
                .iter()
                .all(|(_, m)| m.mention_users == [MEMBER_ID])
        );
        assert!(posts[1..].iter().all(|(_, m)| m.reactions.len() == 2));

        // 23:00, nothing within the next hour: the empty message is posted.
        run(&s.state, PID, NINE + 14 * 3600, &s.outbox)
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
        run(&s.state, PID, NINE + 600, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty());
        run(&s.state, PID, NINE + 900, &s.outbox).await.unwrap();
        let posts = s.outbox.take();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].1.content.starts_with("Standup"));
        run(&s.state, PID, NINE + 960, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty(), "only once");
        assert_eq!(count(&s, "reminders").await, 1);

        // The meeting moves to 10:00: a new reminder before the new time.
        s.mock
            .set_tasks(mock::LIST_ID, vec![task_at("a", "Standup", NINE + 3600)]);
        crate::modules::calendar::events::invalidate(&s.state, PID);
        run(&s.state, PID, NINE + 2700, &s.outbox).await.unwrap();
        assert_eq!(s.outbox.take().len(), 1);

        // Started events are never reminded.
        run(&s.state, PID, NINE + 3700, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty());
    }

    #[tokio::test]
    async fn tasks_added_after_the_last_read_are_seen_within_a_minute() {
        let s = setup().await;
        add_rule(&s, &reminder()).await;
        run(&s.state, PID, NINE, &s.outbox).await.unwrap();
        let reads = s.mock.requests().len();

        // Someone adds a meeting starting in 5 minutes right after the bot read the list.
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![task_at("new", "Quick sync", NINE + 360)],
        );
        run(&s.state, PID, NINE + 30, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty(), "the last read is still fresh");
        assert_eq!(s.mock.requests().len(), reads);

        // A minute later the bot reads again and reminds.
        s.state.cache.backdate(std::time::Duration::from_secs(60));
        run(&s.state, PID, NINE + 60, &s.outbox).await.unwrap();
        assert_eq!(s.outbox.take().len(), 1);
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
        run(&s.state, PID, NINE, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty());
        assert!(
            s.mock.requests().is_empty(),
            "nothing to do, no ClickUp call"
        );

        add_rule(&s, &reminder()).await;
        save_config(&s.state.db, PID, &CalendarConfig::default(), "root")
            .await
            .unwrap();
        run(&s.state, PID, NINE, &s.outbox).await.unwrap();
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
        run(&s.state, PID, NINE, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty());
        assert_eq!(count(&s, "api_err").await, 1);
        s.mock.fail_with(None);

        // Discord refuses: counted as failed and not retried.
        *s.outbox.fail.lock().unwrap() = Some("Missing Permissions".into());
        run(&s.state, PID, NINE + 60, &s.outbox).await.unwrap();
        *s.outbox.fail.lock().unwrap() = None;
        run(&s.state, PID, NINE + 120, &s.outbox).await.unwrap();
        assert!(s.outbox.take().is_empty());
        assert_eq!(count(&s, "failed").await, 1);
        let log = recent_log(&s.state.db, PID, 5).await.unwrap();
        assert_eq!(log[0].error.as_deref(), Some("Missing Permissions"));
    }

    #[tokio::test]
    async fn tests_and_previews_change_nothing() {
        let s = setup().await;
        s.mock
            .set_tasks(mock::LIST_ID, vec![task_at("a", "Standup", NINE + 1800)]);
        let rule = add_rule(&s, &reminder()).await;

        let outcome = send_test(&s.state, PID, &rule, &s.outbox, NINE)
            .await
            .unwrap();
        assert_eq!(outcome, TestOutcome::Sent);
        assert_eq!(s.outbox.take().len(), 1);
        assert_eq!(count(&s, "reminders").await, 0);
        assert!(recent_log(&s.state.db, PID, 1).await.unwrap()[0].is_test);
        assert!(
            !was_sent(&s.state.db, rule.id, "a", NINE + 1800)
                .await
                .unwrap()
        );

        let text = preview(&s.state, PID, &rule, NINE).await.unwrap();
        assert!(text.starts_with("Standup @Anna Nowak"), "{text}");

        // Without a list the preview falls back to a sample, the test reports the problem.
        save_config(&s.state.db, PID, &CalendarConfig::default(), "root")
            .await
            .unwrap();
        let text = preview(&s.state, PID, &rule, NINE).await.unwrap();
        assert!(text.starts_with("Weekly sync"), "{text}");
        let outcome = send_test(&s.state, PID, &rule, &s.outbox, NINE)
            .await
            .unwrap();
        assert_eq!(outcome, TestOutcome::Fetch(FetchError::NoList));
        assert!(s.outbox.take().is_empty());
    }
}
