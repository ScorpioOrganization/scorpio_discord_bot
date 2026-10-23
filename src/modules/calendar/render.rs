//! Turning events into Discord messages.
//!
//! Times are Discord timestamp tags (`<t:...>`), so every reader sees their own time zone.
//! Names and titles are markdown escaped. Matched assignees appear as `<@id>` mentions and
//! roles mapped from task tags as `<@&id>`; they only ping when the rule allows it (see
//! [`OutMessage::mention_users`] and [`OutMessage::mention_roles`]).

use std::collections::{HashMap, HashSet};

use jiff::tz::TimeZone;

use super::events::Event;
use super::reactions::Reaction;
use super::schedule::fmt_local_as;
use super::{Layout, Rule};
use crate::bot::info::RoleOption;
use crate::bot::members::MemberInfo;
use crate::clickup::mapping::Resolver;
use crate::modules::text::{self, escape_markdown, truncate_chars};
use crate::web::fmt_duration;

pub const CONTENT_MAX: usize = 2000;
pub const EMBED_DESCRIPTION_MAX: usize = 4096;
pub const EMBED_TITLE_MAX: usize = 256;
const MAX_MENTIONS: usize = 100;

pub const ITEM_PLACEHOLDERS: &[(&str, &str)] = &[
    ("{title}", "Task name"),
    ("{link}", "Task name linking to ClickUp"),
    ("{url}", "Link to the task"),
    ("{when}", "Date and time"),
    ("{date}", "Date"),
    ("{time}", "Time"),
    ("{relative}", "Relative time, like \"in 2 hours\""),
    ("{start}", "Start date and time, if set"),
    ("{due}", "Due date and time, if set"),
    (
        "{assignees}",
        "Assignees, as mentions when matched to members",
    ),
    ("{description}", "Task description, shortened"),
    ("{status}", "Status"),
    ("{tags}", "Tags"),
    ("{roles}", "Roles mapped from the tags, as mentions"),
    ("{priority}", "Priority"),
    ("{list}", "ClickUp list name"),
];

pub const HEADER_PLACEHOLDERS: &[(&str, &str)] = &[
    ("{count}", "Number of events"),
    ("{list}", "ClickUp list name"),
    ("{date}", "Date the listed events start from"),
    ("{roles}", "Roles of the listed events, as mentions"),
];

pub struct Ctx<'a> {
    pub resolver: &'a Resolver,
    /// Lowercase ClickUp tag to Discord role.
    pub tag_roles: &'a HashMap<String, u64>,
    pub list_name: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutEmbed {
    pub title: Option<String>,
    pub url: Option<String>,
    pub description: String,
    pub color: u32,
    pub timestamp: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutMessage {
    pub content: String,
    pub embed: Option<OutEmbed>,
    /// The only users this message may ping. Empty unless the rule mentions assignees.
    pub mention_users: Vec<u64>,
    /// The only roles this message may ping. Empty unless the rule mentions roles.
    pub mention_roles: Vec<u64>,
    pub reactions: Vec<Reaction>,
}

/// Users and roles that appear as mentions in a rendered text.
#[derive(Debug, Default)]
struct Mentions {
    users: Vec<u64>,
    roles: Vec<u64>,
}

impl Mentions {
    fn extend(&mut self, other: &Mentions) {
        self.users.extend_from_slice(&other.users);
        self.roles.extend_from_slice(&other.roles);
    }
}

fn stamp(ts: i64, style: char) -> String {
    format!("<t:{ts}:{style}>")
}

/// The task URL if it is a plain https link that cannot break the message markup.
fn safe_url(url: &str) -> Option<&str> {
    let ok = url.starts_with("https://")
        && url.len() <= 500
        && !url
            .chars()
            .any(|c| c.is_whitespace() || "<>()[]".contains(c));
    ok.then_some(url)
}

/// Renders line by line. Lines with a placeholder that come out blank are dropped, so an
/// event without assignees or description does not leave empty lines behind.
fn render_lines(template: &str, mut replace: impl FnMut(&str) -> Option<String>) -> String {
    let mut lines = Vec::new();
    for line in template.split('\n') {
        let out = text::render(line, &mut replace);
        if out.trim().is_empty() && line.contains('{') {
            continue;
        }
        lines.push(out);
    }
    lines.join("\n").trim_end().to_owned()
}

fn description(text: &str, max: u32) -> String {
    let text = text.trim();
    let max = max as usize;
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_owned();
    }
    format!("{}...", truncate_chars(text.to_owned(), max).trim_end())
}

fn assignees(ev: &Event, ctx: &Ctx, mentioned: &mut Vec<u64>) -> String {
    ev.assignees
        .iter()
        .map(|person| match ctx.resolver.resolve(person).member() {
            Some(id) => {
                mentioned.push(id);
                format!("<@{id}>")
            }
            None => escape_markdown(person.display()),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Roles mapped from the tags of `events`, deduplicated, in order of appearance.
fn tag_roles<'e>(events: impl IntoIterator<Item = &'e Event>, ctx: &Ctx) -> Vec<u64> {
    let mut seen = HashSet::new();
    events
        .into_iter()
        .flat_map(|e| e.tags.iter())
        .filter_map(|t| ctx.tag_roles.get(&t.trim().to_lowercase()).copied())
        .filter(|id| seen.insert(*id))
        .collect()
}

fn role_mentions(ids: Vec<u64>, mentioned: &mut Mentions) -> String {
    let text = ids
        .iter()
        .map(|id| format!("<@&{id}>"))
        .collect::<Vec<_>>()
        .join(" ");
    mentioned.roles.extend(ids);
    text
}

fn event_text(
    template: &str,
    ev: &Event,
    rule: &Rule,
    ctx: &Ctx,
    mentioned: &mut Mentions,
) -> String {
    render_lines(template, |token| {
        Some(match token {
            "title" => escape_markdown(&ev.name),
            "link" => match safe_url(&ev.url) {
                Some(url) => format!("[{}](<{url}>)", escape_markdown(&ev.name)),
                None => escape_markdown(&ev.name),
            },
            "url" => safe_url(&ev.url)
                .map(|u| format!("<{u}>"))
                .unwrap_or_default(),
            "when" => stamp(ev.at, 'F'),
            "date" => stamp(ev.at, 'D'),
            "time" => stamp(ev.at, 't'),
            "relative" => stamp(ev.at, 'R'),
            "start" => ev.start.map(|t| stamp(t, 'f')).unwrap_or_default(),
            "due" => ev.due.map(|t| stamp(t, 'f')).unwrap_or_default(),
            "assignees" => assignees(ev, ctx, &mut mentioned.users),
            "roles" => role_mentions(tag_roles([ev], ctx), mentioned),
            "description" => description(&ev.description, rule.description_max),
            "status" => escape_markdown(&ev.status),
            "tags" => escape_markdown(&ev.tags.join(", ")),
            "priority" => escape_markdown(ev.priority.as_deref().unwrap_or_default()),
            "list" => escape_markdown(if ev.list.is_empty() {
                ctx.list_name
            } else {
                &ev.list
            }),
            _ => return None,
        })
    })
}

/// Header and empty message text. `count` is the number of events in the window, `listed`
/// the ones the digest shows, `from` the start of the window.
fn header_text(
    template: &str,
    count: usize,
    listed: &[Event],
    from: i64,
    ctx: &Ctx,
    mentioned: &mut Mentions,
) -> String {
    render_lines(template, |token| {
        Some(match token {
            "count" => count.to_string(),
            "list" => escape_markdown(ctx.list_name),
            "date" => stamp(from, 'D'),
            "roles" => role_mentions(tag_roles(listed, ctx), mentioned),
            _ => return None,
        })
    })
}

/// Messages for a digest of `events` (already limited to the rule's window, which starts at
/// `from`).
pub fn digest_messages(rule: &Rule, events: &[Event], from: i64, ctx: &Ctx) -> Vec<OutMessage> {
    if events.is_empty() {
        if !rule.post_when_empty {
            return Vec::new();
        }
        let mut mentioned = Mentions::default();
        let text = header_text(&rule.empty_template, 0, &[], from, ctx, &mut mentioned);
        return vec![finish(rule, text, None, mentioned)];
    }
    let listed = &events[..events.len().min(rule.max_events as usize)];
    match rule.layout {
        Layout::PerEvent => {
            let mut msgs = Vec::with_capacity(listed.len() + 1);
            // The header goes first as its own message, without the reactions meant for
            // events. An empty header template means no header.
            let mut mentioned = Mentions::default();
            let header = header_text(
                &rule.header_template,
                events.len(),
                listed,
                from,
                ctx,
                &mut mentioned,
            );
            if !header.trim().is_empty() {
                let mut msg = finish(rule, header, None, mentioned);
                msg.reactions.clear();
                msgs.push(msg);
            }
            msgs.extend(listed.iter().map(|e| event_message(rule, e, ctx)));
            msgs
        }
        Layout::Combined => vec![combined(rule, events, from, ctx)],
    }
}

/// One message about one event: reminders and per-event digests.
pub fn event_message(rule: &Rule, ev: &Event, ctx: &Ctx) -> OutMessage {
    let mut mentioned = Mentions::default();
    let text = event_text(&rule.item_template, ev, rule, ctx, &mut mentioned);
    finish(rule, text, Some(ev), mentioned)
}

fn more_line(n: usize) -> String {
    format!("...and {n} more.")
}

fn combined(rule: &Rule, events: &[Event], from: i64, ctx: &Ctx) -> OutMessage {
    let limit = if rule.use_embed {
        EMBED_DESCRIPTION_MAX
    } else {
        CONTENT_MAX
    };
    let shown = events.len().min(rule.max_events as usize);
    let items: Vec<(String, Mentions)> = events[..shown]
        .iter()
        .map(|e| {
            let mut m = Mentions::default();
            (event_text(&rule.item_template, e, rule, ctx, &mut m), m)
        })
        .collect();
    let sep = if items.iter().all(|(t, _)| !t.contains('\n')) {
        "\n"
    } else {
        "\n\n"
    };

    let mut mentioned = Mentions::default();
    let mut body = header_text(
        &rule.header_template,
        events.len(),
        &events[..shown],
        from,
        ctx,
        &mut mentioned,
    );
    let mut included = 0;
    for (i, (item, m)) in items.iter().enumerate() {
        let left_after = events.len() - (i + 1);
        let reserve = if left_after > 0 {
            sep.len() + more_line(events.len() - i).len()
        } else {
            0
        };
        let joined = body.chars().count() + sep.len() + item.chars().count();
        if joined + reserve > limit {
            break;
        }
        if !body.is_empty() {
            body.push_str(sep);
        }
        body.push_str(item);
        mentioned.extend(m);
        included += 1;
    }
    let omitted = events.len() - included;
    if omitted > 0 {
        body.push_str(sep);
        body.push_str(&more_line(omitted));
    }
    finish(rule, body, None, mentioned)
}

fn dedupe(ids: &mut Vec<u64>) {
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(*id));
    ids.truncate(MAX_MENTIONS);
}

fn finish(rule: &Rule, text: String, ev: Option<&Event>, mentioned: Mentions) -> OutMessage {
    let Mentions {
        mut users,
        mut roles,
    } = mentioned;
    dedupe(&mut users);
    dedupe(&mut roles);
    let mention_users = if rule.mention_assignees {
        users
    } else {
        Vec::new()
    };
    let mention_roles = if rule.mention_roles {
        roles
    } else {
        Vec::new()
    };
    let reactions = rule.reactions.clone();

    if rule.use_embed {
        // Mentions inside embeds never notify, so pings go into the content.
        let content = mention_users
            .iter()
            .map(|id| format!("<@{id}>"))
            .chain(mention_roles.iter().map(|id| format!("<@&{id}>")))
            .collect::<Vec<_>>()
            .join(" ");
        let title = ev
            .map(|e| truncate_chars(e.name.clone(), EMBED_TITLE_MAX))
            .filter(|t| !t.is_empty());
        let mut description = truncate_chars(text, EMBED_DESCRIPTION_MAX);
        if description.trim().is_empty() && title.is_none() {
            description = "(empty message)".into();
        }
        let embed = OutEmbed {
            url: title
                .as_ref()
                .and(ev.and_then(|e| safe_url(&e.url)))
                .map(str::to_owned),
            title,
            description,
            color: rule.embed_color,
            timestamp: ev.map(|e| e.at),
        };
        return OutMessage {
            content,
            embed: Some(embed),
            mention_users,
            mention_roles,
            reactions,
        };
    }

    let content = if text.trim().is_empty() {
        ev.map(|e| escape_markdown(&e.name))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "(empty message)".into())
    } else {
        text
    };
    OutMessage {
        content: truncate_chars(content, CONTENT_MAX),
        embed: None,
        mention_users,
        mention_roles,
        reactions,
    }
}

/// Makes message text readable on the dashboard: timestamp tags become local times in `tz`
/// and user and role mentions become `@name`.
pub fn humanize(
    text: &str,
    tz: &TimeZone,
    members: &[MemberInfo],
    roles: &[RoleOption],
    now: i64,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if let Some(end) = tail.find('>')
            && let Some(readable) = humanize_tag(&tail[1..end], tz, members, roles, now)
        {
            out.push_str(&readable);
            rest = &tail[end + 1..];
            continue;
        }
        out.push('<');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

fn humanize_tag(
    inner: &str,
    tz: &TimeZone,
    members: &[MemberInfo],
    roles: &[RoleOption],
    now: i64,
) -> Option<String> {
    if let Some(t) = inner.strip_prefix("t:") {
        let (ts, style) = t.split_once(':').unwrap_or((t, "f"));
        let ts: i64 = ts.parse().ok()?;
        return Some(match style {
            "R" if ts >= now => format!("in {}", fmt_duration((ts - now) as u64)),
            "R" => format!("{} ago", fmt_duration((now - ts) as u64)),
            "D" | "d" => fmt_local_as(ts, tz, "%Y-%m-%d"),
            "t" | "T" => fmt_local_as(ts, tz, "%H:%M"),
            _ => fmt_local_as(ts, tz, "%Y-%m-%d %H:%M %Z"),
        });
    }
    if let Some(id) = inner.strip_prefix("@&") {
        let id: u64 = id.parse().ok()?;
        let name = roles
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| "unknown role".into());
        return Some(format!("@{name}"));
    }
    let id: u64 = inner
        .strip_prefix('@')?
        .trim_start_matches('!')
        .parse()
        .ok()?;
    let name = members
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.display().to_owned())
        .unwrap_or_else(|| "unknown member".into());
    Some(format!("@{name}"))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::clickup::client::User;
    use crate::modules::calendar::{Kind, reactions};

    const NOW: i64 = 1_800_000_000;

    fn anna() -> User {
        User {
            id: 3,
            username: "Anna Nowak".into(),
            email: "anna@x.io".into(),
            initials: String::new(),
        }
    }

    fn stranger() -> User {
        User {
            id: 4,
            username: "Bob_the *Builder*".into(),
            email: String::new(),
            initials: String::new(),
        }
    }

    fn members() -> Vec<MemberInfo> {
        vec![MemberInfo {
            id: 77,
            username: "anna.nowak".into(),
            global_name: Some("Anna Nowak".into()),
            nick: None,
        }]
    }

    fn resolver() -> Resolver {
        Resolver::new(&[anna()], &members(), HashMap::new())
    }

    fn event(id: &str, name: &str, at: i64) -> Event {
        Event {
            task_id: id.into(),
            name: name.into(),
            url: format!("https://app.clickup.com/t/{id}"),
            at,
            start: None,
            due: Some(at),
            assignees: vec![anna(), stranger()],
            description: "Line one\nLine two".into(),
            status: "to do".into(),
            tags: vec!["meeting".into(), "team".into()],
            priority: Some("high".into()),
            list: "Calendar".into(),
        }
    }

    const MEETING_ROLE: u64 = 500;
    const TEAM_ROLE: u64 = 501;

    fn tag_map() -> HashMap<String, u64> {
        HashMap::from([
            ("meeting".to_owned(), MEETING_ROLE),
            ("team".to_owned(), TEAM_ROLE),
        ])
    }

    fn with<R>(f: impl FnOnce(&Ctx) -> R) -> R {
        let r = resolver();
        let tags = tag_map();
        f(&Ctx {
            resolver: &r,
            tag_roles: &tags,
            list_name: "Events / Calendar",
        })
    }

    #[test]
    fn every_placeholder() {
        let mut rule = Rule::new(Kind::Reminder);
        rule.item_template = "{title}|{link}|{url}|{when}|{date}|{time}|{relative}|{start}|{due}\
            |{assignees}|{status}|{tags}|{roles}|{priority}|{list}|{nope}"
            .into();
        let ev = event("abc", "Plan [Q4] *now*", NOW + 60);
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        let t = NOW + 60;
        assert_eq!(
            msg.content,
            format!(
                "Plan \\[Q4\\] \\*now\\*|[Plan \\[Q4\\] \\*now\\*](<https://app.clickup.com/t/abc>)\
                 |<https://app.clickup.com/t/abc>|<t:{t}:F>|<t:{t}:D>|<t:{t}:t>|<t:{t}:R>||<t:{t}:f>\
                 |<@77>, Bob\\_the \\*Builder\\*|to do|meeting, team|<@&500> <@&501>|high|Calendar\
                 |{{nope}}"
            )
        );
        assert!(msg.mention_users.is_empty(), "mentions are off by default");
        assert!(
            msg.mention_roles.is_empty(),
            "role pings are off by default"
        );
        assert!(msg.embed.is_none());
    }

    #[test]
    fn blank_lines_are_dropped_and_descriptions_cut() {
        let mut rule = Rule::new(Kind::Reminder);
        rule.item_template =
            "**{title}**\n{start}\n{assignees}\nStatic line\n\n{description}".into();
        rule.description_max = 6;
        let mut ev = event("a", "Sync", NOW + 60);
        ev.assignees.clear();
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.content, "**Sync**\nStatic line\n\nLine o...");

        rule.description_max = 0;
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.content, "**Sync**\nStatic line");
    }

    #[test]
    fn mentions_only_for_matched_people_and_only_when_enabled() {
        let mut rule = Rule::new(Kind::Reminder);
        rule.item_template = "{assignees}".into();
        rule.mention_assignees = true;
        rule.reactions = reactions::parse("✅ ❌").unwrap();
        let ev = event("a", "Sync", NOW + 60);
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.mention_users, [77]);
        assert_eq!(msg.reactions.len(), 2);

        rule.use_embed = true;
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.content, "<@77>", "pings go outside the embed");
        let embed = msg.embed.unwrap();
        assert_eq!(embed.title.as_deref(), Some("Sync"));
        assert_eq!(embed.url.as_deref(), Some("https://app.clickup.com/t/a"));
        assert_eq!(embed.timestamp, Some(NOW + 60));
        assert_eq!(embed.color, rule.embed_color);
    }

    #[test]
    fn roles_come_from_mapped_tags() {
        let mut rule = Rule::new(Kind::Reminder);
        rule.item_template = "{title}\n{roles}".into();
        let mut ev = event("a", "Sync", NOW + 60);
        ev.tags = vec!["Meeting".into(), "other".into(), " meeting ".into()];
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(
            msg.content, "Sync\n<@&500>",
            "case insensitive, deduplicated"
        );
        assert!(msg.mention_roles.is_empty());

        rule.mention_roles = true;
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.mention_roles, [MEETING_ROLE]);
        assert!(msg.mention_users.is_empty(), "assignee pings stay separate");

        // Without a mapped tag the line disappears and nothing is pinged.
        ev.tags = vec!["other".into()];
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.content, "Sync");
        assert!(msg.mention_roles.is_empty());

        // Roles only ping where the placeholder is used.
        ev.tags = vec!["team".into()];
        rule.item_template = "{title}".into();
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert!(msg.mention_roles.is_empty());

        rule.item_template = "{title} {roles} {assignees}".into();
        rule.mention_assignees = true;
        rule.use_embed = true;
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.content, "<@77> <@&501>", "pings go outside the embed");
        assert_eq!(msg.mention_roles, [TEAM_ROLE]);
    }

    #[test]
    fn per_event_digests_start_with_the_header() {
        let mut rule = Rule::new(Kind::Digest);
        rule.layout = Layout::PerEvent;
        rule.header_template = "**{count} events** {roles}".into();
        rule.item_template = "{title}".into();
        rule.mention_roles = true;
        rule.reactions = reactions::parse("✅").unwrap();
        let mut events: Vec<Event> = (0..2)
            .map(|i| event(&i.to_string(), &format!("E{i}"), NOW + i))
            .collect();
        events[0].tags = vec!["meeting".into()];
        events[1].tags = vec!["team".into(), "meeting".into()];
        let msgs = with(|ctx| digest_messages(&rule, &events, NOW, ctx));
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].content, "**2 events** <@&500> <@&501>");
        assert_eq!(msgs[0].mention_roles, [MEETING_ROLE, TEAM_ROLE]);
        assert!(msgs[0].reactions.is_empty(), "reactions are for events");
        assert_eq!(msgs[1].content, "E0");
        assert_eq!(msgs[2].reactions.len(), 1);
        assert!(msgs[1].mention_roles.is_empty(), "no {{roles}} in the item");

        // An empty header means no header message, in both layouts.
        rule.header_template = String::new();
        let msgs = with(|ctx| digest_messages(&rule, &events, NOW, ctx));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content, "E0");
        rule.layout = Layout::Combined;
        let msgs = with(|ctx| digest_messages(&rule, &events, NOW, ctx));
        assert_eq!(msgs[0].content, "E0\nE1");

        // A header whose placeholders are all empty is not sent either.
        rule.layout = Layout::PerEvent;
        rule.header_template = "{roles}".into();
        events.iter_mut().for_each(|e| e.tags.clear());
        let msgs = with(|ctx| digest_messages(&rule, &events, NOW, ctx));
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn unsafe_urls_are_not_linked() {
        let mut rule = Rule::new(Kind::Reminder);
        rule.item_template = "{link} {url}".into();
        let mut ev = event("a", "Sync", NOW);
        ev.url = "javascript:alert(1)".into();
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.content, "Sync");
        ev.url = "https://x.io/a>b".into();
        let msg = with(|ctx| event_message(&rule, &ev, ctx));
        assert_eq!(msg.content, "Sync");
    }

    #[test]
    fn combined_digest_and_limits() {
        let mut rule = Rule::new(Kind::Digest);
        rule.header_template = "**{count} on {list}**".into();
        rule.item_template = "{title} {time}".into();
        let events: Vec<Event> = (0..3)
            .map(|i| event(&i.to_string(), &format!("E{i}"), NOW + i))
            .collect();
        let msgs = with(|ctx| digest_messages(&rule, &events, NOW, ctx));
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0].content,
            format!(
                "**3 on Events / Calendar**\nE0 <t:{}:t>\nE1 <t:{}:t>\nE2 <t:{}:t>",
                NOW,
                NOW + 1,
                NOW + 2
            )
        );

        // Multi-line items get a blank line between them.
        rule.item_template = "{title}\n{time}".into();
        let msgs = with(|ctx| digest_messages(&rule, &events[..2], NOW, ctx));
        assert!(msgs[0].content.contains("\n\nE1"));

        // max_events and the message size limit both end with a "more" line.
        rule.item_template = "{title} {description}".into();
        rule.max_events = 2;
        let msgs = with(|ctx| digest_messages(&rule, &events, NOW, ctx));
        assert!(msgs[0].content.ends_with("...and 1 more."));
        assert!(!msgs[0].content.contains("E2"));

        rule.max_events = 50;
        rule.description_max = 1000;
        let long: Vec<Event> = (0..40)
            .map(|i| {
                let mut e = event(&i.to_string(), &format!("E{i}"), NOW + i);
                e.description = "x".repeat(300);
                e
            })
            .collect();
        let msgs = with(|ctx| digest_messages(&rule, &long, NOW, ctx));
        let content = &msgs[0].content;
        assert!(content.chars().count() <= CONTENT_MAX);
        assert!(content.ends_with("more."), "{content}");
        rule.use_embed = true;
        let msgs = with(|ctx| digest_messages(&rule, &long, NOW, ctx));
        let embed = msgs[0].embed.as_ref().unwrap();
        assert!(embed.description.chars().count() <= EMBED_DESCRIPTION_MAX);
        assert!(embed.description.chars().count() > CONTENT_MAX);
        assert!(embed.title.is_none());
    }

    #[test]
    fn per_event_and_empty_digests() {
        let mut rule = Rule::new(Kind::Digest);
        rule.layout = Layout::PerEvent;
        rule.max_events = 2;
        let events: Vec<Event> = (0..3)
            .map(|i| event(&i.to_string(), "E", NOW + i))
            .collect();
        assert_eq!(
            with(|ctx| digest_messages(&rule, &events, NOW, ctx)).len(),
            3,
            "the header and max_events events"
        );

        assert!(with(|ctx| digest_messages(&rule, &[], NOW, ctx)).is_empty());
        rule.post_when_empty = true;
        rule.empty_template = "Nothing on {list} ({count})".into();
        let msgs = with(|ctx| digest_messages(&rule, &[], NOW, ctx));
        assert_eq!(msgs[0].content, "Nothing on Events / Calendar (0)");

        // {date} is the day the window starts, not the day of posting.
        rule.empty_template = "Nothing on {date}".into();
        let tomorrow = NOW + 86400;
        let msgs = with(|ctx| digest_messages(&rule, &[], tomorrow, ctx));
        assert_eq!(msgs[0].content, format!("Nothing on <t:{tomorrow}:D>"));
    }

    #[test]
    fn humanize_for_previews() {
        let tz = TimeZone::UTC;
        let roles = [RoleOption {
            id: MEETING_ROLE,
            name: "Team".into(),
            mentionable: true,
        }];
        let text = format!(
            "<t:{NOW}:F> <t:{NOW}:D> <t:{NOW}:t> <t:{}:R> <@77> <@!5> <@&500> <@&9> \
             <https://x.io> a<b",
            NOW + 7200
        );
        assert_eq!(
            humanize(&text, &tz, &members(), &roles, NOW),
            "2027-01-15 08:00 UTC 2027-01-15 08:00 in 2h 0m @Anna Nowak @unknown member @Team \
             @unknown role <https://x.io> a<b"
        );
    }
}
