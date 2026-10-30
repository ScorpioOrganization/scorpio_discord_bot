use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;

use super::runner::{self, Outcome, Problem};
use super::{
    CHECK_MINUTES, ID, LogRow, MAX_RULES, MAX_WRITES_PER_RUN, Mode, RecurringConfig, Rule, Trigger,
    Unit, count_rules, delete_rule, insert_rule, list_rules, load_config, load_rule, recent_log,
    save_config, schedule, tag_taken, update_rule,
};
use crate::auth::extract::CurrentUser;
use crate::clickup::ClickUpError;
use crate::clickup::client::Team;
use crate::db::now;
use crate::error::{AppError, AppResult};
use crate::keys::web::{KeyView, key_routes, key_view};
use crate::keys::{self, Credential, Service};
use crate::modules::calendar::schedule::{fmt_local, tz_name, tz_names};
use crate::modules::clickup::web::{invalidate_tags, workspace_tags};
use crate::modules::{self, clickup, counter};
use crate::projects::Project;
use crate::state::AppState;
use crate::web::{MsgQuery, PageCtx, fmt_ts, redirect_msg, require_project};

const TEAMS_MAX_AGE: Duration = Duration::from_secs(10 * 60);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/projects/{pid}/modules/recurring",
            get(page).post(save_settings),
        )
        .route("/projects/{pid}/modules/recurring/refresh", post(refresh))
        .route("/projects/{pid}/modules/recurring/run", post(run_now))
        .route("/projects/{pid}/modules/recurring/rules", post(create))
        .route("/projects/{pid}/modules/recurring/rules/new", get(new_rule))
        .route(
            "/projects/{pid}/modules/recurring/rules/{rid}",
            get(edit_rule).post(save_rule),
        )
        .route(
            "/projects/{pid}/modules/recurring/rules/{rid}/delete",
            post(remove_rule),
        )
        .merge(key_routes(Service::ClickUp, ID))
}

fn base_path(pid: i64) -> String {
    format!("/projects/{pid}/modules/{ID}")
}

fn rule_path(pid: i64, rid: i64) -> String {
    format!("{}/rules/{rid}", base_path(pid))
}

/// Workspaces the key can access, cached by the key itself.
async fn fetch_teams(
    state: &AppState,
    pid: i64,
    cred: &Credential,
) -> Result<Arc<Vec<Team>>, ClickUpError> {
    let fingerprint = &crate::auth::hash_token(cred.token.expose())[..16];
    let key = format!("recurring:teams:{pid}:{fingerprint}");
    if let Some(hit) = state.cache.get::<Vec<Team>>(&key, TEAMS_MAX_AGE) {
        return Ok(hit);
    }
    let result = state.clickup.teams(&cred.token).await;
    let teams = keys::track(&state.db, pid, ID, cred, result).await?;
    Ok(state.cache.insert(key, teams))
}

// ---------------------------------------------------------------- main page

struct TeamOption {
    id: String,
    name: String,
    selected: bool,
}

struct RuleRow {
    id: i64,
    name: String,
    tag: String,
    interval: String,
    trigger: &'static str,
    mode: &'static str,
    enabled: bool,
}

struct PreviewRow {
    title: String,
    url: String,
    rule: String,
    action: &'static str,
    status: String,
    old_due: i64,
    old_text: String,
    new_due: i64,
    new_text: String,
}

struct LogView {
    ts: i64,
    time: String,
    rule_name: String,
    task_name: String,
    task_url: String,
    old_due: i64,
    old_text: String,
    new_due: i64,
    new_text: String,
    /// `dry`, `failed`, `moved` or `copied`.
    result: &'static str,
    error: String,
}

impl LogView {
    fn new(row: LogRow, tz: &jiff::tz::TimeZone) -> Self {
        let result = match (row.dry_run, row.success, row.mode.as_str()) {
            (true, _, _) => "dry",
            (false, false, _) => "failed",
            (false, true, "create") => "copied",
            (false, true, _) => "moved",
        };
        Self {
            ts: row.created_at,
            time: fmt_ts(row.created_at),
            old_text: fmt_local(row.old_due, tz),
            new_text: fmt_local(row.new_due, tz),
            old_due: row.old_due,
            new_due: row.new_due,
            rule_name: row.rule_name,
            task_name: row.task_name,
            task_url: row.task_url,
            result,
            error: row.error.unwrap_or_default(),
        }
    }
}

#[derive(Template)]
#[template(path = "modules/recurring.html")]
struct RecurringPage {
    page: PageCtx,
    project: Project,
    enabled: bool,
    warnings: Vec<String>,
    keys: Vec<KeyView>,
    cfg: RecurringConfig,
    teams: Vec<TeamOption>,
    team_error: Option<String>,
    /// The saved workspace is not among the options (or the key cannot list them).
    team_outside_list: bool,
    tz_names: &'static [String],
    check_min: u32,
    check_max: u32,
    rules: Vec<RuleRow>,
    can_add_rule: bool,
    preview: Vec<PreviewRow>,
    preview_error: Option<String>,
    deferred: usize,
    truncated: bool,
    max_writes: usize,
    last_run: String,
    last_run_ts: i64,
    next_run: String,
    next_run_ts: i64,
    moved: i64,
    copied: i64,
    failed: i64,
    api_err: i64,
    recent: Vec<LogView>,
}

impl RecurringPage {
    fn team_id(&self) -> &str {
        self.cfg.team_id.as_deref().unwrap_or_default()
    }

    fn team_name(&self) -> &str {
        self.cfg.team_name.as_deref().unwrap_or_default()
    }
}

async fn page(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let project = require_project(&state, pid).await?;
    let enabled = modules::is_enabled(&state.db, pid, ID).await?;
    let cfg = load_config(&state.db, pid).await?;
    let rules = list_rules(&state.db, pid).await?;
    let tz = cfg.tz();
    let now = now();

    let mut warnings = Vec::new();
    if !enabled {
        warnings.push(
            "This module is disabled, so scheduled runs do not happen. Run now still works."
                .to_owned(),
        );
    }
    if cfg.team_id.is_none() {
        warnings.push("No ClickUp workspace is saved yet, so nothing is read.".to_owned());
    }
    if cfg.dry_run {
        warnings.push(
            "Dry run is on: nothing is changed in ClickUp, the activity log shows what would \
             change. Turn it off in Settings once the preview looks right."
                .to_owned(),
        );
    }
    if !rules.iter().any(|r| r.enabled) {
        warnings.push("There is no active rule yet.".to_owned());
    }

    let mut teams = Vec::new();
    let mut team_error = None;
    if let Ok(cred) = keys::resolve(&state, pid, Service::ClickUp, ID).await? {
        match fetch_teams(&state, pid, &cred).await {
            Ok(list) => {
                // Until a workspace is saved, offer the ClickUp module's one, or the only one.
                let default = match &cfg.team_id {
                    Some(id) => Some(id.clone()),
                    None => clickup::load_config(&state.db, pid)
                        .await?
                        .team_id
                        .or_else(|| (list.len() == 1).then(|| list[0].id.clone())),
                };
                teams = list
                    .iter()
                    .map(|t| TeamOption {
                        selected: default.as_deref() == Some(t.id.as_str()),
                        id: t.id.clone(),
                        name: t.name.clone(),
                    })
                    .collect();
            }
            Err(e) => team_error = Some(e.to_string()),
        }
    }
    let team_outside_list = cfg
        .team_id
        .as_deref()
        .is_some_and(|id| !teams.iter().any(|t| t.id == id));

    let mut preview = Vec::new();
    let mut preview_error = None;
    let (mut deferred, mut truncated) = (0, false);
    match runner::preview(&state, pid, now).await? {
        Ok((plan, cut)) => {
            deferred = plan.deferred;
            truncated = cut;
            preview = plan
                .items
                .into_iter()
                .map(|p| PreviewRow {
                    action: p.rule.mode.label(),
                    status: p.rule.status_text().to_owned(),
                    rule: p.rule.name,
                    old_due: p.old_due / 1000,
                    old_text: fmt_local(p.old_due / 1000, &tz),
                    new_due: p.new_due / 1000,
                    new_text: fmt_local(p.new_due / 1000, &tz),
                    title: p.task.name,
                    url: p.task.url,
                })
                .collect();
        }
        Err(Problem::NoWorkspace | Problem::NoRules) => {}
        Err(p) => preview_error = Some(p.message()),
    }

    let rule_rows = rules
        .iter()
        .map(|r| RuleRow {
            id: r.id,
            name: r.name.clone(),
            tag: r.tag.clone(),
            interval: r.interval_text(),
            trigger: r.trigger.label(),
            mode: r.mode.label(),
            enabled: r.enabled,
        })
        .collect();

    let stat = |key| counter(&state.db, Some(pid), ID, key);
    let next_run_at = cfg.next_run_at().unwrap_or(now).max(now);
    let page = RecurringPage {
        page: PageCtx::new(&user, "projects", &msg),
        project,
        enabled,
        warnings,
        keys: vec![key_view(&state, pid, Service::ClickUp, ID).await?],
        teams,
        team_error,
        team_outside_list,
        tz_names: tz_names(),
        check_min: *CHECK_MINUTES.start(),
        check_max: *CHECK_MINUTES.end(),
        rules: rule_rows,
        can_add_rule: (rules.len() as i64) < MAX_RULES,
        preview,
        preview_error,
        deferred,
        truncated,
        max_writes: MAX_WRITES_PER_RUN,
        last_run: cfg
            .last_run_at
            .map(fmt_ts)
            .unwrap_or_else(|| "Never".into()),
        last_run_ts: cfg.last_run_at.unwrap_or_default(),
        next_run: fmt_ts(next_run_at),
        next_run_ts: next_run_at,
        moved: stat("moved").await?,
        copied: stat("copied").await?,
        failed: stat("failed").await?,
        api_err: stat("api_err").await?,
        recent: recent_log(&state.db, pid, 15)
            .await?
            .into_iter()
            .map(|r| LogView::new(r, &tz))
            .collect(),
        cfg,
    };
    Ok(Html(page.render()?))
}

#[derive(Deserialize)]
struct SettingsForm {
    #[serde(default)]
    team_id: String,
    timezone: String,
    check_minutes: String,
    dry_run: Option<String>,
}

async fn save_settings(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
    Form(form): Form<SettingsForm>,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let back = base_path(pid);
    let current = load_config(&state.db, pid).await?;

    let Some(timezone) = tz_name(&form.timezone) else {
        return Ok(redirect_msg(&back, "timezone_invalid"));
    };
    let Some(check_minutes) = form
        .check_minutes
        .trim()
        .parse()
        .ok()
        .filter(|m| CHECK_MINUTES.contains(m))
    else {
        return Ok(redirect_msg(&back, "recurring_interval"));
    };

    let team_id = form.team_id.trim();
    let team = if team_id.is_empty() {
        None
    } else if current.team_id.as_deref() == Some(team_id) {
        current.team_id.clone().zip(current.team_name.clone())
    } else {
        let cred = match keys::resolve(&state, pid, Service::ClickUp, ID).await? {
            Ok(c) => c,
            Err(p) => return Ok(redirect_msg(&back, Problem::Token(p).code())),
        };
        let teams = match fetch_teams(&state, pid, &cred).await {
            Ok(t) => t,
            Err(e) => return Ok(redirect_msg(&back, Problem::Api(e).code())),
        };
        let Some(team) = teams.iter().find(|t| t.id == team_id) else {
            return Ok(redirect_msg(&back, "workspace_invalid"));
        };
        Some((team.id.clone(), team.name.clone()))
    };

    let (team_id, team_name) = team.unzip();
    let cfg = RecurringConfig {
        team_id,
        team_name,
        timezone,
        check_minutes,
        dry_run: form.dry_run.is_some(),
        last_run_at: current.last_run_at,
    };
    save_config(&state.db, pid, &cfg, &user.username).await?;
    runner::invalidate_preview(&state, pid);
    tracing::info!(
        user = %user.username, project_id = pid, team = ?cfg.team_id, dry_run = cfg.dry_run,
        "recurring settings saved"
    );
    Ok(redirect_msg(&back, "saved"))
}

async fn refresh(
    State(state): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(pid): Path<i64>,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    runner::invalidate_preview(&state, pid);
    for prefix in ["recurring:teams", "recurring:status"] {
        state.cache.remove_prefix(&format!("{prefix}:{pid}:"));
    }
    invalidate_tags(&state, pid);
    Ok(redirect_msg(&base_path(pid), "refreshed"))
}

async fn run_now(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let outcome = runner::run(&state, pid, now(), true).await?;
    tracing::info!(user = %user.username, project_id = pid, ?outcome, "recurring run started from the dashboard");
    let code = match outcome {
        Outcome::Done(r) if r.dry_run => "recurring_dry_ran",
        Outcome::Done(r) if r.failed > 0 => "recurring_ran_failed",
        Outcome::Done(_) | Outcome::NotDue => "recurring_ran",
        Outcome::Busy => "recurring_busy",
        Outcome::Problem(p) => p.code(),
    };
    Ok(redirect_msg(&base_path(pid), code))
}

// ---------------------------------------------------------------- rule editor

struct UnitOption {
    value: &'static str,
    label: String,
    selected: bool,
}

#[derive(Template)]
#[template(path = "modules/recurring_rule.html")]
struct RulePage {
    page: PageCtx,
    project: Project,
    rule: Rule,
    is_new: bool,
    units: Vec<UnitOption>,
    on_close: bool,
    copies: bool,
    tags: Vec<String>,
    tags_error: Option<String>,
    example: String,
    warnings: Vec<String>,
}

/// "A task due Thu 2026-10-15 09:00 CEST moves to Thu 2026-10-22 09:00 CEST."
fn example(rule: &Rule, cfg: &RecurringConfig, now: i64) -> String {
    let tz = cfg.tz();
    let sample = jiff::Timestamp::from_second(now)
        .ok()
        .map(|t| t.to_zoned(tz.clone()).date())
        .and_then(|d| d.at(9, 0, 0, 0).to_zoned(tz.clone()).ok())
        .map(|z| z.timestamp().as_millisecond());
    let next = sample.and_then(|due| schedule::shift(due, 1, rule.every, rule.unit, &tz));
    let (Some(due), Some(next)) = (sample, next) else {
        return String::new();
    };
    let verb = match rule.mode {
        Mode::Update => "moves to",
        Mode::Create => "gets a copy due",
    };
    let when = match rule.trigger {
        Trigger::Close => "once it is done",
        Trigger::Schedule => "once that time passes",
    };
    format!(
        "A task due {} {verb} {}, {when}.",
        fmt_local(due / 1000, &tz),
        fmt_local(next / 1000, &tz)
    )
}

async fn rule_page(
    state: &AppState,
    user: &crate::auth::SessionUser,
    msg: &MsgQuery,
    pid: i64,
    rule: Rule,
    is_new: bool,
) -> AppResult<Html<String>> {
    let project = require_project(state, pid).await?;
    let cfg = load_config(&state.db, pid).await?;

    let mut warnings = Vec::new();
    if is_new && count_rules(&state.db, pid).await? >= MAX_RULES {
        warnings.push(format!(
            "This server already has {MAX_RULES} rules, the maximum."
        ));
    }

    let mut tags = Vec::new();
    let mut tags_error = None;
    if let (Some(team), Ok(cred)) = (
        &cfg.team_id,
        keys::resolve(state, pid, Service::ClickUp, ID).await?,
    ) {
        match workspace_tags(state, pid, ID, &cred, team).await {
            Ok(list) => tags = (*list).clone(),
            Err(e) => tags_error = Some(e.to_string()),
        }
    }

    let page = RulePage {
        page: PageCtx::new(user, "projects", msg),
        project,
        is_new,
        units: Unit::ALL
            .into_iter()
            .map(|u| UnitOption {
                value: u.as_str(),
                label: u.label(2),
                selected: u == rule.unit,
            })
            .collect(),
        on_close: rule.trigger == Trigger::Close,
        copies: rule.mode == Mode::Create,
        tags,
        tags_error,
        example: example(&rule, &cfg, now()),
        warnings,
        rule,
    };
    Ok(Html(page.render()?))
}

async fn new_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    require_project(&state, pid).await?;
    rule_page(&state, &user, &msg, pid, Rule::new(), true).await
}

async fn edit_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((pid, rid)): Path<(i64, i64)>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    require_project(&state, pid).await?;
    let rule = load_rule(&state.db, pid, rid)
        .await?
        .ok_or(AppError::NotFound)?;
    rule_page(&state, &user, &msg, pid, rule, false).await
}

/// Applies a submitted rule form to `rule`. Returns a flash code when something is invalid.
fn parse_rule_form(fields: &[(String, String)], mut rule: Rule) -> Result<Rule, &'static str> {
    let get = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim())
            .unwrap_or_default()
    };
    let checked = |key: &str| fields.iter().any(|(k, _)| k == key);

    let name = get("name");
    if name.is_empty() || name.chars().count() > 60 {
        return Err("rule_name");
    }
    rule.name = name.to_owned();
    rule.enabled = checked("enabled");
    rule.tag = clickup::normalize_tag(get("tag"))
        .filter(|t| !t.contains(','))
        .ok_or("rule_tag")?;
    rule.every = get("every")
        .parse()
        .ok()
        .filter(|n| (1..=365).contains(n))
        .ok_or("rule_every")?;
    rule.unit = Unit::parse(get("unit")).ok_or("rule_invalid")?;
    rule.trigger = Trigger::parse(get("trigger")).ok_or("rule_invalid")?;
    rule.mode = Mode::parse(get("mode")).ok_or("rule_invalid")?;
    let status = get("reopen_status");
    if status.chars().count() > 60 {
        return Err("rule_status");
    }
    rule.reopen_status = status.to_owned();
    rule.shift_start = checked("shift_start");
    Ok(rule)
}

async fn create(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pid): Path<i64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    if count_rules(&state.db, pid).await? >= MAX_RULES {
        return Ok(redirect_msg(&base_path(pid), "too_many_rules"));
    }
    let back = |code: &str| {
        Redirect::to(&format!("{}/rules/new?msg={code}", base_path(pid))).into_response()
    };
    let rule = match parse_rule_form(&fields, Rule::new()) {
        Ok(r) => r,
        Err(code) => return Ok(back(code)),
    };
    if tag_taken(&state.db, pid, &rule.tag, 0).await? {
        return Ok(back("rule_tag_taken"));
    }
    let id = insert_rule(&state.db, pid, &rule, &user.username).await?;
    runner::invalidate_preview(&state, pid);
    tracing::info!(user = %user.username, project_id = pid, rule = id, tag = %rule.tag, "recurring rule created");
    Ok(redirect_msg(&rule_path(pid, id), "rule_created"))
}

async fn save_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((pid, rid)): Path<(i64, i64)>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    let current = load_rule(&state.db, pid, rid)
        .await?
        .ok_or(AppError::NotFound)?;
    let back = rule_path(pid, rid);
    let rule = match parse_rule_form(&fields, current) {
        Ok(r) => r,
        Err(code) => return Ok(redirect_msg(&back, code)),
    };
    if tag_taken(&state.db, pid, &rule.tag, rid).await? {
        return Ok(redirect_msg(&back, "rule_tag_taken"));
    }
    update_rule(&state.db, pid, &rule, &user.username).await?;
    runner::invalidate_preview(&state, pid);
    tracing::info!(user = %user.username, project_id = pid, rule = rid, "recurring rule saved");
    Ok(redirect_msg(&back, "saved"))
}

async fn remove_rule(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((pid, rid)): Path<(i64, i64)>,
) -> AppResult<Response> {
    require_project(&state, pid).await?;
    if !delete_rule(&state.db, pid, rid).await? {
        return Err(AppError::NotFound);
    }
    runner::invalidate_preview(&state, pid);
    tracing::info!(user = %user.username, project_id = pid, rule = rid, "recurring rule deleted");
    Ok(redirect_msg(&base_path(pid), "rule_deleted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn valid() -> Vec<(&'static str, &'static str)> {
        vec![
            ("name", " Weekly sync "),
            ("enabled", "on"),
            ("tag", " Weekly "),
            ("every", "2"),
            ("unit", "week"),
            ("trigger", "schedule"),
            ("mode", "create"),
            ("reopen_status", " to do "),
        ]
    }

    #[test]
    fn rule_form_is_validated() {
        let rule = parse_rule_form(&fields(&valid()), Rule::new()).unwrap();
        assert_eq!(rule.name, "Weekly sync");
        assert_eq!(rule.tag, "weekly");
        assert_eq!((rule.every, rule.unit), (2, Unit::Week));
        assert_eq!((rule.trigger, rule.mode), (Trigger::Schedule, Mode::Create));
        assert_eq!(rule.reopen_status, "to do");
        assert!(rule.enabled && !rule.shift_start);

        let long = "x".repeat(61);
        let cases = [
            ("name", "", "rule_name"),
            ("name", long.as_str(), "rule_name"),
            ("tag", "  ", "rule_tag"),
            ("tag", "a,b", "rule_tag"),
            ("every", "0", "rule_every"),
            ("every", "366", "rule_every"),
            ("every", "two", "rule_every"),
            ("unit", "fortnight", "rule_invalid"),
            ("trigger", "always", "rule_invalid"),
            ("mode", "delete", "rule_invalid"),
            ("reopen_status", long.as_str(), "rule_status"),
        ];
        for (key, value, code) in cases {
            let mut form = valid();
            form.retain(|(k, _)| *k != key);
            form.push((key, value));
            assert_eq!(
                parse_rule_form(&fields(&form), Rule::new()).unwrap_err(),
                code,
                "{key}={value}"
            );
        }
    }

    #[test]
    fn examples_describe_the_rule() {
        let cfg = RecurringConfig::default();
        // Thursday 2026-10-15 12:00 UTC.
        let now = 1_792_065_600;
        let mut rule = Rule::new();
        assert_eq!(
            example(&rule, &cfg, now),
            "A task due 2026-10-15 09:00 UTC moves to 2026-10-22 09:00 UTC, once it is done."
        );
        rule.mode = Mode::Create;
        rule.trigger = Trigger::Schedule;
        rule.unit = Unit::Month;
        assert_eq!(
            example(&rule, &cfg, now),
            "A task due 2026-10-15 09:00 UTC gets a copy due 2026-11-15 09:00 UTC, once that \
             time passes."
        );
    }
}
