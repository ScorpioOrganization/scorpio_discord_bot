use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use serenity::all::GatewayIntents;

use super::events::{self, FetchError};
use super::reactions::{self, EmojiOption};
use super::render::{HEADER_PLACEHOLDERS, ITEM_PLACEHOLDERS};
use super::runner::{self, DiscordOutbox, TestOutcome};
use super::schedule::{self, DAY_NAMES};
use super::{
    CalendarConfig, ID, Kind, Layout, LogRow, MAX_RULES, MAX_TEMPLATE_LEN, Rule, count_rules,
    delete_rule, insert_rule, list_rules, load_config, load_rule, parse_tags, recent_log,
    save_config, update_rule, was_sent,
};
use crate::auth::extract::CurrentUser;
use crate::bot::info::{self, ChannelOption, GuildSummary, RoleOption};
use crate::clickup::ClickUpError;
use crate::clickup::tokens::{self, Credential};
use crate::clickup::web::{TokenView, token_routes, token_view};
use crate::db::now;
use crate::error::{AppError, AppResult};
use crate::modules::{self, clickup, counter, text};
use crate::state::AppState;
use crate::web::{MsgQuery, PageCtx, fmt_ts, redirect_msg, require_guild};

const UPCOMING_SECS: i64 = 7 * 86400;
const UPCOMING_MAX: usize = 20;
const CATALOG_MAX_AGE: Duration = Duration::from_secs(10 * 60);
const CATALOG_MAX_TEAMS: usize = 3;
const CATALOG_MAX_SPACES: usize = 20;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/guilds/{gid}/modules/calendar",
            get(page).post(save_source),
        )
        .route("/guilds/{gid}/modules/calendar/refresh", post(refresh))
        .route("/guilds/{gid}/modules/calendar/rules", post(create))
        .route("/guilds/{gid}/modules/calendar/rules/new", get(new_rule))
        .route(
            "/guilds/{gid}/modules/calendar/rules/{rid}",
            get(edit_rule).post(save_rule),
        )
        .route(
            "/guilds/{gid}/modules/calendar/rules/{rid}/delete",
            post(remove_rule),
        )
        .route(
            "/guilds/{gid}/modules/calendar/rules/{rid}/test",
            post(test_rule),
        )
        .merge(token_routes(ID))
}

fn base_path(gid: u64) -> String {
    format!("/guilds/{gid}/modules/{ID}")
}

fn rule_path(gid: u64, rid: i64) -> String {
    format!("{}/rules/{rid}", base_path(gid))
}

fn channels(state: &AppState, gid: u64) -> Vec<ChannelOption> {
    state
        .discord()
        .map(|d| info::text_channels(&d.cache, gid))
        .unwrap_or_default()
}

fn guild_roles(state: &AppState, gid: u64) -> Vec<RoleOption> {
    state
        .discord()
        .map(|d| info::roles(&d.cache, gid))
        .unwrap_or_default()
}

fn guild_emojis(state: &AppState, gid: u64) -> Vec<EmojiOption> {
    state
        .discord()
        .map(|d| reactions::guild_emojis(&d.cache, gid))
        .unwrap_or_default()
}

impl LogRow {
    fn time(&self) -> String {
        fmt_ts(self.created_at)
    }

    fn timestamp(&self) -> i64 {
        self.created_at
    }

    fn error_text(&self) -> &str {
        self.error.as_deref().unwrap_or("")
    }

    fn kind_label(&self) -> &str {
        match self.kind.as_str() {
            "digest" => "Digest",
            "reminder" => "Reminder",
            "missed" => "Missed digest",
            other => other,
        }
    }
}

// ---------------------------------------------------------------- list catalog

/// Lists the key can see, as (group label, [(list id, list name)]).
type Catalog = Vec<(String, Vec<(String, String)>)>;

async fn catalog(
    state: &AppState,
    gid: u64,
    cred: &Credential,
) -> Result<Arc<Catalog>, ClickUpError> {
    // Keyed by the key itself, so a replaced key never shows the old account's lists.
    let fingerprint = &crate::auth::hash_token(cred.token.expose())[..16];
    let key = format!("calendar:catalog:{gid}:{fingerprint}");
    if let Some(hit) = state.cache.get::<Catalog>(&key, CATALOG_MAX_AGE) {
        return Ok(hit);
    }
    let (db, client, token) = (&state.db, &state.clickup, &cred.token);

    let teams = tokens::track(db, gid, ID, cred, client.teams(token).await).await?;
    let many_teams = teams.len() > 1;
    let mut groups: Catalog = Vec::new();
    let mut spaces_seen = 0;
    for team in teams.iter().take(CATALOG_MAX_TEAMS) {
        let spaces = tokens::track(db, gid, ID, cred, client.spaces(token, &team.id).await).await?;
        for space in spaces {
            if spaces_seen >= CATALOG_MAX_SPACES {
                break;
            }
            spaces_seen += 1;
            let prefix = if many_teams {
                format!("{} / {}", team.name, space.name)
            } else {
                space.name.clone()
            };
            let loose = tokens::track(
                db,
                gid,
                ID,
                cred,
                client.folderless_lists(token, &space.id).await,
            )
            .await?;
            if !loose.is_empty() {
                groups.push((
                    prefix.clone(),
                    loose.into_iter().map(|l| (l.id, l.name)).collect(),
                ));
            }
            let folders =
                tokens::track(db, gid, ID, cred, client.folders(token, &space.id).await).await?;
            for folder in folders.into_iter().filter(|f| !f.lists.is_empty()) {
                groups.push((
                    format!("{prefix} / {}", folder.name),
                    folder.lists.into_iter().map(|l| (l.id, l.name)).collect(),
                ));
            }
        }
    }
    Ok(state.cache.insert(key, groups))
}

fn invalidate_catalog(state: &AppState, gid: u64) {
    state
        .cache
        .remove_prefix(&format!("calendar:catalog:{gid}:"));
}

// ---------------------------------------------------------------- main page

struct ListOption {
    id: String,
    name: String,
    selected: bool,
}

struct ListGroup {
    label: String,
    lists: Vec<ListOption>,
}

struct UpcomingRow {
    at: i64,
    time: String,
    title: String,
    url: String,
    people: String,
    /// The event already started (shown for the last 12 hours).
    passed: bool,
    /// 04:00 sharp, which is how ClickUp stores a date without a time.
    maybe_no_time: bool,
    /// One line per active reminder rule: when it reminds, or why it will not.
    reminders: Vec<String>,
}

struct RuleRow {
    id: i64,
    name: String,
    kind: &'static str,
    enabled: bool,
    channel: String,
    schedule: String,
    next_run: Option<i64>,
}

impl RuleRow {
    fn next_ts(&self) -> i64 {
        self.next_run.unwrap_or_default()
    }

    fn next_text(&self) -> String {
        self.next_run.map(fmt_ts).unwrap_or_default()
    }
}

#[derive(Template)]
#[template(path = "modules/calendar.html")]
struct CalendarPage {
    page: PageCtx,
    guild: GuildSummary,
    enabled: bool,
    warnings: Vec<String>,
    token: TokenView,
    cfg: CalendarConfig,
    catalog: Vec<ListGroup>,
    catalog_error: Option<String>,
    /// The selected list is not among the catalog options (or there is no catalog).
    list_outside_catalog: bool,
    tz_names: &'static [String],
    upcoming: Vec<UpcomingRow>,
    show_reminders: bool,
    upcoming_error: Option<String>,
    truncated: bool,
    rules: Vec<RuleRow>,
    can_add_rule: bool,
    digests: i64,
    reminders: i64,
    failed: i64,
    missed: i64,
    api_err: i64,
    recent: Vec<LogRow>,
}

impl CalendarPage {
    fn start_mode(&self) -> bool {
        self.cfg.start_mode()
    }

    fn list_id(&self) -> &str {
        self.cfg.list_id.as_deref().unwrap_or_default()
    }

    fn list_name(&self) -> &str {
        self.cfg.list_name.as_deref().unwrap_or_default()
    }
}

/// Problems with a rule's channel, or None when it is fine.
fn channel_warning(rule: &Rule, channels: &[ChannelOption]) -> Option<String> {
    let name = &rule.name;
    let Some(id) = rule.channel_id else {
        return Some(format!("Rule \"{name}\" has no channel and posts nothing."));
    };
    let Some(c) = channels.iter().find(|c| c.id == id) else {
        return Some(format!("The channel of rule \"{name}\" no longer exists."));
    };
    if !c.can_send {
        return Some(format!(
            "The bot cannot post in #{} (rule \"{name}\").",
            c.name
        ));
    }
    if rule.use_embed && !c.can_embed {
        return Some(format!(
            "The bot lacks the Embed Links permission in #{} (rule \"{name}\").",
            c.name
        ));
    }
    if !rule.reactions.is_empty() && !c.can_react {
        return Some(format!(
            "The bot cannot add reactions in #{} (rule \"{name}\"): it needs Add Reactions and \
             Read Message History.",
            c.name
        ));
    }
    None
}

/// Problems with a rule's role pings, or None when it is fine or the rule does not ping roles.
fn role_warning(
    rule: &Rule,
    channels: &[ChannelOption],
    roles: &[RoleOption],
    tag_roles: &HashMap<String, u64>,
) -> Option<String> {
    if !rule.mention_roles {
        return None;
    }
    let name = &rule.name;
    let uses_roles = rule.item_template.contains("{roles}")
        || (rule.kind == Kind::Digest && rule.header_template.contains("{roles}"));
    if !uses_roles {
        return Some(format!(
            "Rule \"{name}\" pings roles, but its messages have no {{roles}} placeholder."
        ));
    }
    if tag_roles.is_empty() {
        return Some(format!(
            "Rule \"{name}\" pings roles, but no ClickUp tag is mapped to a role yet. Map tags \
             on the ClickUp page."
        ));
    }
    let channel = rule
        .channel_id
        .and_then(|id| channels.iter().find(|c| c.id == id))?;
    if channel.can_mention_all {
        return None;
    }
    let silent: Vec<String> = roles
        .iter()
        .filter(|r| !r.mentionable && tag_roles.values().any(|id| *id == r.id))
        .map(|r| format!("@{}", r.name))
        .collect();
    if silent.is_empty() {
        return None;
    }
    Some(format!(
        "The bot lacks the Mention All Roles permission in #{}, so rule \"{name}\" cannot ping \
         roles that are not mentionable: {}.",
        channel.name,
        silent.join(", ")
    ))
}

async fn page(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let guild = require_guild(&state, gid)?;
    let enabled = modules::is_enabled(&state.db, gid, ID).await?;
    let cfg = load_config(&state.db, gid).await?;
    let rules = list_rules(&state.db, gid).await?;
    let channel_list = channels(&state, gid);
    let tz = cfg.tz();
    let now = now();

    let mut warnings = Vec::new();
    if !enabled {
        warnings.push("This module is disabled, nothing is posted until you enable it.".into());
    }
    if cfg.list_id.is_none() {
        warnings.push("No ClickUp list is selected yet, so nothing is posted.".into());
    }
    let roles = guild_roles(&state, gid);
    let tag_roles = clickup::tag_roles(&state, gid).await?;
    for rule in rules.iter().filter(|r| r.enabled) {
        warnings.extend(channel_warning(rule, &channel_list));
        warnings.extend(role_warning(rule, &channel_list, &roles, &tag_roles));
    }
    if rules.iter().any(|r| r.enabled && r.mention_assignees)
        && !state
            .missing_intents(GatewayIntents::GUILD_MEMBERS)
            .is_empty()
    {
        warnings.push(
            "Server Members Intent is disabled, so assignees may not be matched to members and \
             mentioned."
                .into(),
        );
    }

    let mut catalog_groups = Vec::new();
    let mut catalog_error = None;
    if let Ok(cred) = tokens::resolve(&state, gid, ID).await? {
        match catalog(&state, gid, &cred).await {
            Ok(groups) => {
                catalog_groups = groups
                    .iter()
                    .map(|(label, lists)| ListGroup {
                        label: label.clone(),
                        lists: lists
                            .iter()
                            .map(|(id, name)| ListOption {
                                id: id.clone(),
                                name: name.clone(),
                                selected: cfg.list_id.as_deref() == Some(id.as_str()),
                            })
                            .collect(),
                    })
                    .collect();
            }
            Err(e) => catalog_error = Some(e.to_string()),
        }
    }
    let list_outside_catalog = cfg.list_id.is_some()
        && !catalog_groups
            .iter()
            .any(|g| g.lists.iter().any(|l| l.selected));

    let reminder_rules: Vec<&Rule> = rules
        .iter()
        .filter(|r| r.enabled && r.kind == Kind::Reminder && r.channel_id.is_some())
        .collect();
    let mut upcoming = Vec::new();
    let mut upcoming_error = None;
    let mut truncated = false;
    if cfg.list_id.is_some() {
        match events::cached(&state, gid, &cfg, now, UPCOMING_SECS, events::MAX_AGE).await? {
            Ok(fetched) => {
                truncated = fetched.truncated;
                let list: Vec<_> = fetched
                    .between(now - events::LOOKBACK_SECS, now + UPCOMING_SECS)
                    .into_iter()
                    .take(UPCOMING_MAX)
                    .collect();
                let people: Vec<_> = list.iter().flat_map(|e| e.assignees.clone()).collect();
                let resolver = clickup::resolver(&state, gid, &people).await?;
                let members = crate::bot::members::guild_members(&state, gid).await;
                let names: HashMap<u64, &str> =
                    members.iter().map(|m| (m.id, m.display())).collect();
                for e in list {
                    let passed = e.at <= now;
                    let mut reminders = Vec::new();
                    for rule in &reminder_rules {
                        let remind_at = e.at - i64::from(rule.lead_minutes) * 60;
                        let status = if was_sent(&state.db, rule.id, &e.task_id, e.at).await? {
                            "sent".to_owned()
                        } else if passed {
                            "not sent, the bot did not see it before it started".to_owned()
                        } else if remind_at <= now {
                            "due now, posted within a minute".to_owned()
                        } else {
                            format!("at {}", schedule::fmt_local(remind_at, &tz))
                        };
                        reminders.push(format!("{}: {status}", rule.name));
                    }
                    upcoming.push(UpcomingRow {
                        at: e.at,
                        time: schedule::fmt_local(e.at, &tz),
                        people: e
                            .assignees
                            .iter()
                            .map(|p| match resolver.resolve(p).member() {
                                Some(id) => {
                                    format!("@{}", names.get(&id).copied().unwrap_or("member"))
                                }
                                None => format!("{} (no member)", p.display()),
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        passed,
                        maybe_no_time: schedule::fmt_local_as(e.at, &tz, "%H:%M:%S") == "04:00:00",
                        reminders,
                        title: e.name,
                        url: e.url,
                    });
                }
            }
            Err(e) => upcoming_error = Some(e.message()),
        }
    }
    let show_reminders = !reminder_rules.is_empty();

    let rule_rows = rules
        .iter()
        .map(|r| RuleRow {
            id: r.id,
            name: r.name.clone(),
            kind: r.kind.label(),
            enabled: r.enabled,
            channel: match r.channel_id {
                None => "Not set".into(),
                Some(id) => channel_list
                    .iter()
                    .find(|c| c.id == id)
                    .map(|c| format!("#{}", c.name))
                    .unwrap_or_else(|| "Missing channel".into()),
            },
            schedule: r.schedule_text(),
            next_run: match r.kind {
                Kind::Digest if r.enabled => {
                    schedule::next_occurrence(now, r.days, r.time_of_day, &r.tz())
                }
                _ => None,
            },
        })
        .collect();

    let stat = |key| counter(&state.db, Some(gid), ID, key);
    let page = CalendarPage {
        page: PageCtx::new(&user, "guilds", &msg),
        guild,
        enabled,
        warnings,
        token: token_view(&state, gid, ID).await?,
        catalog: catalog_groups,
        catalog_error,
        list_outside_catalog,
        tz_names: schedule::tz_names(),
        upcoming,
        show_reminders,
        upcoming_error,
        truncated,
        rules: rule_rows,
        can_add_rule: (rules.len() as i64) < MAX_RULES,
        digests: stat("digests").await?,
        reminders: stat("reminders").await?,
        failed: stat("failed").await?,
        missed: stat("missed").await?,
        api_err: stat("api_err").await?,
        recent: recent_log(&state.db, gid, 10).await?,
        cfg,
    };
    Ok(Html(page.render()?))
}

#[derive(Deserialize)]
struct SourceForm {
    #[serde(default)]
    list_select: String,
    #[serde(default)]
    list_manual: String,
    time_field: String,
    timezone: String,
    #[serde(default)]
    tag_filter: String,
    include_subtasks: Option<String>,
    include_closed: Option<String>,
}

/// A list ID typed by hand: the ID itself, or a ClickUp list URL containing `/li/<id>`.
fn manual_list_id(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let id = match raw.find("/li/") {
        Some(i) => raw[i + 4..]
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default(),
        None => raw,
    };
    let ok = (1..=40).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| id.to_owned())
}

async fn save_source(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Form(form): Form<SourceForm>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let back = base_path(gid);
    let current = load_config(&state.db, gid).await?;

    let time_field = match form.time_field.as_str() {
        "due" | "start" => form.time_field.clone(),
        _ => return Err(AppError::BadRequest),
    };
    let Some(timezone) = schedule::tz_name(&form.timezone) else {
        return Ok(redirect_msg(&back, "timezone_invalid"));
    };
    let Some(tags) = parse_tags(&form.tag_filter) else {
        return Ok(redirect_msg(&back, "tags_invalid"));
    };

    let list_input = if form.list_manual.trim().is_empty() {
        form.list_select.trim().to_owned()
    } else {
        match manual_list_id(&form.list_manual) {
            Some(id) => id,
            None => return Ok(redirect_msg(&back, "list_not_found")),
        }
    };
    let (list_id, list_name) = if list_input.is_empty() {
        (None, None)
    } else if current.list_id.as_deref() == Some(list_input.as_str()) {
        (current.list_id.clone(), current.list_name.clone())
    } else {
        let cred = match tokens::resolve(&state, gid, ID).await? {
            Ok(c) => c,
            Err(p) => return Ok(redirect_msg(&back, FetchError::Token(p).code())),
        };
        let result = state.clickup.list(&cred.token, &list_input).await;
        let list = match tokens::track(&state.db, gid, ID, &cred, result).await {
            Ok(list) => list,
            Err(e) => return Ok(redirect_msg(&back, FetchError::Api(e).code())),
        };
        let name = [
            list.space.map(|s| s.name),
            list.folder.map(|f| f.name),
            Some(list.name),
        ]
        .into_iter()
        .flatten()
        .filter(|n| !n.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" / ");
        (Some(list.id), Some(name))
    };

    let cfg = CalendarConfig {
        list_id,
        list_name,
        time_field,
        timezone,
        tag_filter: tags.join(", "),
        include_subtasks: form.include_subtasks.is_some(),
        include_closed: form.include_closed.is_some(),
    };
    save_config(&state.db, gid, &cfg, &user.username).await?;
    events::invalidate(&state, gid);
    tracing::info!(user = %user.username, guild_id = gid, list = ?cfg.list_id, "calendar source saved");
    Ok(redirect_msg(&back, "saved"))
}

async fn refresh(
    State(state): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(gid): Path<u64>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    events::invalidate(&state, gid);
    invalidate_catalog(&state, gid);
    state.members.invalidate(gid);
    Ok(redirect_msg(&base_path(gid), "refreshed"))
}

// ---------------------------------------------------------------- rule editor

struct DayBox {
    value: usize,
    label: &'static str,
    checked: bool,
}

#[derive(Template)]
#[template(path = "modules/calendar_rule.html")]
struct RulePage {
    page: PageCtx,
    guild: GuildSummary,
    rule: Rule,
    is_new: bool,
    is_digest: bool,
    per_event: bool,
    channels: Vec<ChannelOption>,
    days: Vec<DayBox>,
    time: String,
    color: String,
    reactions: String,
    item_placeholders: &'static [(&'static str, &'static str)],
    header_placeholders: &'static [(&'static str, &'static str)],
    emojis: Vec<EmojiOption>,
    quick_reactions: &'static [&'static str],
    preview: String,
    /// The zone preview times are shown in: the rule's for digests, the default otherwise.
    timezone: String,
    tz_names: &'static [String],
    warnings: Vec<String>,
    max_len: usize,
}

const QUICK_REACTIONS: &[&str] = &["✅", "❌", "❓", "👍", "🎉", "👀"];

async fn rule_page(
    state: &AppState,
    user: &crate::auth::SessionUser,
    msg: &MsgQuery,
    gid: u64,
    rule: Rule,
    is_new: bool,
) -> AppResult<Html<String>> {
    let guild = require_guild(state, gid)?;
    let cfg = load_config(&state.db, gid).await?;
    let mut channel_list = channels(state, gid);
    for c in &mut channel_list {
        c.selected = Some(c.id) == rule.channel_id;
    }

    let mut warnings = Vec::new();
    if cfg.list_id.is_none() {
        warnings.push("No ClickUp list is selected, the preview uses a sample event.".to_owned());
    }
    if !is_new {
        warnings.extend(channel_warning(&rule, &channel_list));
        let tag_roles = clickup::tag_roles(state, gid).await?;
        warnings.extend(role_warning(
            &rule,
            &channel_list,
            &guild_roles(state, gid),
            &tag_roles,
        ));
    }
    if is_new && count_rules(&state.db, gid).await? >= MAX_RULES {
        warnings.push(format!(
            "This server already has {MAX_RULES} rules, the maximum."
        ));
    }

    let preview = runner::preview(state, gid, &rule, now()).await?;
    let page = RulePage {
        page: PageCtx::new(user, "guilds", msg),
        guild,
        is_new,
        is_digest: rule.kind == Kind::Digest,
        per_event: rule.layout == Layout::PerEvent,
        channels: channel_list,
        days: DAY_NAMES
            .iter()
            .enumerate()
            .map(|(i, label)| DayBox {
                value: i,
                label,
                checked: rule.days & (1 << i) != 0,
            })
            .collect(),
        time: schedule::fmt_minute(rule.time_of_day),
        color: format!("#{:06x}", rule.embed_color & 0xff_ffff),
        reactions: reactions::to_storage(&rule.reactions),
        item_placeholders: ITEM_PLACEHOLDERS,
        header_placeholders: HEADER_PLACEHOLDERS,
        emojis: guild_emojis(state, gid),
        quick_reactions: QUICK_REACTIONS,
        preview,
        timezone: match rule.kind {
            Kind::Digest => rule.timezone.clone(),
            Kind::Reminder => cfg.timezone,
        },
        tz_names: schedule::tz_names(),
        warnings,
        max_len: MAX_TEMPLATE_LEN,
        rule,
    };
    Ok(Html(page.render()?))
}

#[derive(Deserialize)]
struct NewRuleQuery {
    kind: Option<String>,
    #[serde(flatten)]
    msg: MsgQuery,
}

async fn new_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Query(q): Query<NewRuleQuery>,
) -> AppResult<Html<String>> {
    require_guild(&state, gid)?;
    let kind = Kind::parse(q.kind.as_deref().unwrap_or("digest")).ok_or(AppError::BadRequest)?;
    let mut rule = Rule::new(kind);
    // The browser's zone replaces this when scripts run, see `data-detect-tz`.
    rule.timezone = load_config(&state.db, gid).await?.timezone;
    rule_page(&state, &user, &q.msg, gid, rule, true).await
}

async fn edit_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((gid, rid)): Path<(u64, i64)>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    require_guild(&state, gid)?;
    let rule = load_rule(&state.db, gid, rid)
        .await?
        .ok_or(AppError::NotFound)?;
    rule_page(&state, &user, &msg, gid, rule, false).await
}

fn template_field(raw: Option<&str>) -> Result<String, &'static str> {
    let value = raw.unwrap_or_default().replace("\r\n", "\n");
    let value = value.trim();
    if value.is_empty() {
        return Err("template_empty");
    }
    if value.chars().count() > MAX_TEMPLATE_LEN {
        return Err("template_long");
    }
    Ok(value.to_owned())
}

/// Like [`template_field`], but empty is allowed and means "none".
fn optional_template_field(raw: Option<&str>) -> Result<String, &'static str> {
    match template_field(raw) {
        Err("template_empty") => Ok(String::new()),
        other => other,
    }
}

fn number<T: std::str::FromStr + PartialOrd>(
    raw: Option<&str>,
    range: std::ops::RangeInclusive<T>,
    code: &'static str,
) -> Result<T, &'static str> {
    raw.unwrap_or_default()
        .trim()
        .parse()
        .ok()
        .filter(|v| range.contains(v))
        .ok_or(code)
}

/// Applies a submitted rule form to `base` (a new rule or the stored one). Fields of the
/// other kind are ignored. Returns a flash code when something is invalid.
fn parse_rule_form(
    fields: &[(String, String)],
    mut rule: Rule,
    channel_list: &[ChannelOption],
    emojis: &[EmojiOption],
) -> Result<Rule, &'static str> {
    let get = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let checked = |key: &str| get(key).is_some();

    let name = get("name").unwrap_or_default().trim();
    if name.is_empty() || name.chars().count() > 60 {
        return Err("rule_name");
    }
    rule.name = name.to_owned();
    rule.enabled = checked("enabled");
    rule.channel_id = match get("channel_id").unwrap_or_default().trim() {
        "" => None,
        raw => {
            let id: u64 = raw.parse().map_err(|_| "invalid_channel")?;
            if !channel_list.iter().any(|c| c.id == id) {
                return Err("invalid_channel");
            }
            Some(id)
        }
    };

    match rule.kind {
        Kind::Digest => {
            let mut days = 0u8;
            for (_, v) in fields.iter().filter(|(k, _)| k == "day") {
                let day: u8 = v.parse().map_err(|_| "rule_no_days")?;
                if day > 6 {
                    return Err("rule_no_days");
                }
                days |= 1 << day;
            }
            if days == 0 {
                return Err("rule_no_days");
            }
            rule.days = days;
            rule.time_of_day =
                schedule::parse_hhmm(get("time").unwrap_or_default()).ok_or("rule_time")?;
            rule.timezone =
                schedule::tz_name(get("timezone").unwrap_or_default()).ok_or("timezone_invalid")?;
            rule.lookahead_hours = number(get("lookahead_hours"), 1..=336, "rule_lookahead")?;
            rule.layout = if get("layout") == Some("per_event") {
                Layout::PerEvent
            } else {
                Layout::Combined
            };
            rule.max_events = number(get("max_events"), 1..=50, "rule_max_events")?;
            rule.post_when_empty = checked("post_when_empty");
            rule.header_template = optional_template_field(get("header_template"))?;
            rule.empty_template = template_field(get("empty_template"))?;
        }
        Kind::Reminder => {
            rule.lead_minutes = number(get("lead_minutes"), 1..=10080, "rule_lead")?;
        }
    }

    rule.item_template = template_field(get("item_template"))?;
    rule.mention_assignees = checked("mention_assignees");
    rule.mention_roles = checked("mention_roles");
    rule.use_embed = checked("use_embed");
    rule.embed_color =
        text::parse_hex_color(get("embed_color").unwrap_or_default()).ok_or("invalid_color")?;
    rule.description_max = number(get("description_max"), 0..=1000, "rule_description_max")?;
    let parsed =
        reactions::parse(get("reactions").unwrap_or_default()).ok_or("reactions_invalid")?;
    if reactions::unknown_custom(&parsed, emojis) {
        return Err("reactions_invalid");
    }
    rule.reactions = parsed;
    Ok(rule)
}

async fn create(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let kind_raw = fields
        .iter()
        .find(|(k, _)| k == "kind")
        .map(|(_, v)| v.as_str())
        .unwrap_or_default();
    let kind = Kind::parse(kind_raw).ok_or(AppError::BadRequest)?;
    if count_rules(&state.db, gid).await? >= MAX_RULES {
        return Ok(redirect_msg(&base_path(gid), "too_many_rules"));
    }
    let rule = match parse_rule_form(
        &fields,
        Rule::new(kind),
        &channels(&state, gid),
        &guild_emojis(&state, gid),
    ) {
        Ok(r) => r,
        Err(code) => {
            let to = format!(
                "{}/rules/new?kind={}&msg={code}",
                base_path(gid),
                kind.as_str()
            );
            return Ok(Redirect::to(&to).into_response());
        }
    };
    let id = insert_rule(&state.db, gid, &rule, &user.username).await?;
    tracing::info!(user = %user.username, guild_id = gid, rule = id, "calendar rule created");
    Ok(redirect_msg(&rule_path(gid, id), "rule_created"))
}

async fn save_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((gid, rid)): Path<(u64, i64)>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let current = load_rule(&state.db, gid, rid)
        .await?
        .ok_or(AppError::NotFound)?;
    let back = rule_path(gid, rid);
    let rule = match parse_rule_form(
        &fields,
        current,
        &channels(&state, gid),
        &guild_emojis(&state, gid),
    ) {
        Ok(r) => r,
        Err(code) => return Ok(redirect_msg(&back, code)),
    };
    update_rule(&state.db, gid, &rule, &user.username).await?;
    tracing::info!(user = %user.username, guild_id = gid, rule = rid, "calendar rule saved");
    Ok(redirect_msg(&back, "saved"))
}

async fn remove_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((gid, rid)): Path<(u64, i64)>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    if !delete_rule(&state.db, gid, rid).await? {
        return Err(AppError::NotFound);
    }
    tracing::info!(user = %user.username, guild_id = gid, rule = rid, "calendar rule deleted");
    Ok(redirect_msg(&base_path(gid), "rule_deleted"))
}

async fn test_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((gid, rid)): Path<(u64, i64)>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let rule = load_rule(&state.db, gid, rid)
        .await?
        .ok_or(AppError::NotFound)?;
    let back = rule_path(gid, rid);
    let Some(discord) = state.discord() else {
        return Ok(redirect_msg(&back, "bot_offline"));
    };
    if rule.channel_id.is_none() {
        return Ok(redirect_msg(&back, "no_channel"));
    }
    let outbox = DiscordOutbox::new(&discord.http);
    let outcome = runner::send_test(&state, gid, &rule, &outbox, now()).await?;
    tracing::info!(user = %user.username, guild_id = gid, rule = rid, ?outcome, "calendar test sent");
    let code = match outcome {
        TestOutcome::Sent => "test_sent",
        TestOutcome::Failed => "test_failed",
        TestOutcome::Fetch(e) => e.code(),
    };
    Ok(redirect_msg(&back, code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_warnings() {
        let mut rule = Rule::new(Kind::Reminder);
        rule.name = "Ping".into();
        rule.channel_id = Some(1);
        rule.item_template = "{title} {roles}".into();
        let mut channels = vec![ChannelOption {
            id: 1,
            name: "events".into(),
            can_send: true,
            can_embed: true,
            can_react: true,
            can_mention_all: false,
            selected: false,
        }];
        let role = |id, name: &str, mentionable| RoleOption {
            id,
            name: name.into(),
            mentionable,
        };
        let roles = [role(10, "Team", true), role(11, "Leads", false)];
        let mut tags = HashMap::from([("meeting".to_owned(), 10)]);
        assert!(
            role_warning(&rule, &channels, &roles, &tags).is_none(),
            "off"
        );

        rule.mention_roles = true;
        assert!(role_warning(&rule, &channels, &roles, &tags).is_none());
        assert!(
            role_warning(&rule, &channels, &roles, &HashMap::new())
                .unwrap()
                .contains("no ClickUp tag")
        );
        tags.insert("leads".into(), 11);
        let warning = role_warning(&rule, &channels, &roles, &tags).unwrap();
        assert!(warning.contains("#events") && warning.contains("@Leads"));
        assert!(!warning.contains("@Team"));
        channels[0].can_mention_all = true;
        assert!(role_warning(&rule, &channels, &roles, &tags).is_none());

        rule.item_template = "{title}".into();
        assert!(
            role_warning(&rule, &channels, &roles, &tags)
                .unwrap()
                .contains("no {roles} placeholder")
        );
    }

    #[test]
    fn manual_list_ids_and_urls() {
        assert_eq!(manual_list_id(" 901234 ").as_deref(), Some("901234"));
        assert_eq!(
            manual_list_id("https://app.clickup.com/9012/v/li/901234567?pr=1").as_deref(),
            Some("901234567")
        );
        assert_eq!(
            manual_list_id("https://app.clickup.com/9012/v/li/901234567/").as_deref(),
            Some("901234567")
        );
        assert!(manual_list_id("../team").is_none());
        assert!(manual_list_id("https://example.com/x").is_none());
        assert!(manual_list_id(&"9".repeat(41)).is_none());
    }
}
