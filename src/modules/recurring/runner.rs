//! One run for a guild: reads the tasks of every rule's tag from ClickUp, then moves or
//! copies the ones due to recur, or only logs them in dry run mode. `run` takes `now`, so
//! tests drive it with fixed times against `MockClickUp`.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use jiff::tz::TimeZone;

use super::plan::{self, Plan, Planned};
use super::{
    CREATE_LOOKBACK_SECS, ID, LogEntry, MAX_WRITES_PER_RUN, Mode, RecurringConfig, Rule, Trigger,
    done_markers, list_rules, load_config, log, mark_done, schedule, set_last_run, try_lock,
    unlock, unmark_done,
};
use crate::clickup::ClickUpError;
use crate::clickup::client::{NewTask, Priority, Task, TaskQuery, TaskUpdate};
use crate::keys::{self, Credential, KeyProblem, Service};
use crate::modules::bump_counter;
use crate::state::AppState;

/// Pages of 100 tasks read per rule.
const MAX_PAGES: u32 = 5;
const STATUS_MAX_AGE: Duration = Duration::from_secs(10 * 60);
/// The dashboard preview reuses tasks read this recently.
pub const PREVIEW_MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// No new task is started after this, well inside the scheduler's 50 s tick timeout, so a
/// run is not cancelled halfway through a change.
const WRITE_BUDGET: Duration = Duration::from_secs(30);

/// Why nothing could be read from ClickUp.
#[derive(Debug, Clone, PartialEq)]
pub enum Problem {
    NoWorkspace,
    NoRules,
    Token(KeyProblem),
    Api(ClickUpError),
}

impl Problem {
    pub fn message(&self) -> String {
        match self {
            Problem::NoWorkspace => "No ClickUp workspace is selected.".into(),
            Problem::NoRules => "There is no active rule.".into(),
            Problem::Token(p) => p.message(Service::ClickUp).into(),
            Problem::Api(e) => e.to_string(),
        }
    }

    /// Flash code for dashboard redirects.
    pub fn code(&self) -> &'static str {
        match self {
            Problem::NoWorkspace => "recurring_no_workspace",
            Problem::NoRules => "recurring_no_rules",
            Problem::Token(KeyProblem::NotSet) => "token_missing",
            Problem::Token(KeyProblem::Unreadable) => "token_unreadable",
            Problem::Token(KeyProblem::SharedModuleDisabled) => "clickup_disabled",
            Problem::Api(ClickUpError::Unauthorized) => "token_check_failed",
            Problem::Api(_) => "clickup_unreachable",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The check interval has not passed yet.
    NotDue,
    /// Another run of this guild is in progress.
    Busy,
    Problem(Problem),
    Done(Report),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub dry_run: bool,
    /// Tasks moved or copied (in a dry run: that would have been).
    pub changed: usize,
    pub failed: usize,
    /// Left for a later run because of the per run cap.
    pub deferred: usize,
    /// A tag had more tasks than one run reads.
    pub truncated: bool,
}

/// The tasks of each enabled rule, oldest rule first.
pub struct Fetched {
    pub rules: Vec<(Rule, Vec<Task>)>,
    pub truncated: bool,
}

/// Runs the module for a guild, unless the check interval has not passed (`force` skips
/// that check, for "Run now"). A run that hits a problem still counts as a run, so a broken
/// key is not retried every minute.
pub async fn run(state: &AppState, project_id: i64, now: i64, force: bool) -> Result<Outcome> {
    let cfg = load_config(&state.db, project_id).await?;
    let Some(team_id) = cfg.team_id.clone() else {
        return Ok(Outcome::Problem(Problem::NoWorkspace));
    };
    let rules = active_rules(state, project_id).await?;
    if rules.is_empty() {
        return Ok(Outcome::Problem(Problem::NoRules));
    }
    if !force && cfg.next_run_at().is_some_and(|at| at > now) {
        return Ok(Outcome::NotDue);
    }
    if !try_lock(&state.db, project_id, now).await? {
        return Ok(Outcome::Busy);
    }
    let outcome = locked_run(state, project_id, now, &cfg, &team_id, &rules).await;
    set_last_run(&state.db, project_id, now).await?;
    unlock(&state.db, project_id).await?;
    invalidate_preview(state, project_id);
    let outcome = outcome?;
    match &outcome {
        Outcome::Problem(p) => tracing::debug!(project_id, problem = ?p, "recurring run skipped"),
        Outcome::Done(r) if r.changed + r.failed > 0 => {
            tracing::info!(project_id, report = ?r, "recurring run done");
        }
        _ => {}
    }
    Ok(outcome)
}

async fn active_rules(state: &AppState, project_id: i64) -> Result<Vec<Rule>> {
    Ok(list_rules(&state.db, project_id)
        .await?
        .into_iter()
        .filter(|r| r.enabled)
        .collect())
}

async fn locked_run(
    state: &AppState,
    project_id: i64,
    now: i64,
    cfg: &RecurringConfig,
    team_id: &str,
    rules: &[Rule],
) -> Result<Outcome> {
    let cred = match keys::resolve(state, project_id, Service::ClickUp, ID).await? {
        Ok(c) => c,
        Err(p) => return Ok(Outcome::Problem(Problem::Token(p))),
    };
    let now_ms = now * 1000;
    let fetched = match fetch(state, project_id, &cred, team_id, rules, now_ms).await {
        Ok(f) => f,
        Err(e) => return Ok(Outcome::Problem(Problem::Api(e))),
    };
    let tz = cfg.tz();
    let done = done_markers(&state.db, project_id).await?;
    let plan = plan::plan(&fetched.rules, now_ms, &tz, &done, MAX_WRITES_PER_RUN);
    let mut report = Report {
        dry_run: cfg.dry_run,
        deferred: plan.deferred,
        truncated: fetched.truncated,
        ..Default::default()
    };

    let started = std::time::Instant::now();
    for (i, item) in plan.items.iter().enumerate() {
        if started.elapsed() > WRITE_BUDGET {
            report.deferred += plan.items.len() - i;
            break;
        }
        // A copy is recorded before it is made: should the run be cut off right after
        // ClickUp created it, the next run must not make a second one.
        let claim = !cfg.dry_run && item.rule.mode == Mode::Create;
        if claim {
            mark_done(
                &state.db,
                project_id,
                &item.task.id,
                item.old_due,
                item.rule.id,
                None,
            )
            .await?;
        }
        let result = if cfg.dry_run {
            Ok(None)
        } else {
            apply(state, project_id, &cred, &tz, item).await
        };
        let entry = LogEntry {
            rule: &item.rule,
            task_id: &item.task.id,
            task_name: &item.task.name,
            task_url: &item.task.url,
            old_due: item.old_due / 1000,
            new_due: item.new_due / 1000,
            dry_run: cfg.dry_run,
            error: result.as_ref().err().map(ToString::to_string),
        };
        let logged = log(&state.db, project_id, entry).await?;
        match result {
            Ok(new_task) => {
                report.changed += 1;
                if !cfg.dry_run {
                    let task_id = &item.task.id;
                    let rule_id = item.rule.id;
                    mark_done(
                        &state.db,
                        project_id,
                        task_id,
                        item.old_due,
                        rule_id,
                        new_task.as_deref(),
                    )
                    .await?;
                    let key = match item.rule.mode {
                        Mode::Update => "moved",
                        Mode::Create => "copied",
                    };
                    bump_counter(&state.db, project_id, ID, key).await?;
                }
            }
            Err(e) => {
                report.failed += 1;
                // Only when ClickUp surely made no copy: after a timeout or a server error
                // it may have, and a missing copy is better than a second one.
                let refused = matches!(
                    e,
                    ClickUpError::Rejected(_)
                        | ClickUpError::Unauthorized
                        | ClickUpError::NotFound
                        | ClickUpError::RateLimited
                );
                if claim && refused {
                    unmark_done(&state.db, project_id, &item.task.id, item.old_due).await?;
                }
                // A failure that repeats every run is counted once, like it is logged once.
                if logged {
                    bump_counter(&state.db, project_id, ID, "failed").await?;
                }
                // The remaining tasks would fail the same way, leave them for the next run.
                if matches!(
                    e,
                    ClickUpError::Unauthorized
                        | ClickUpError::RateLimited
                        | ClickUpError::Network(_)
                ) {
                    break;
                }
            }
        }
    }
    Ok(Outcome::Done(report))
}

/// Reads the tasks of each rule's tag in the workspace.
async fn fetch(
    state: &AppState,
    project_id: i64,
    cred: &Credential,
    team_id: &str,
    rules: &[Rule],
    now_ms: i64,
) -> Result<Fetched, ClickUpError> {
    let mut fetched = Fetched {
        rules: Vec::with_capacity(rules.len()),
        truncated: false,
    };
    for rule in rules {
        let query = TaskQuery {
            // Copies are only made for recent occurrences, see `CREATE_LOOKBACK_SECS`.
            due_after_ms: (rule.mode == Mode::Create)
                .then(|| now_ms - CREATE_LOOKBACK_SECS * 1000 - 1),
            due_before_ms: (rule.trigger == Trigger::Schedule).then_some(now_ms + 1),
            include_closed: true,
            subtasks: true,
            tags: vec![rule.tag.clone()],
            markdown: rule.mode == Mode::Create,
        };
        let result = state
            .clickup
            .team_tasks(&cred.token, team_id, &query, MAX_PAGES)
            .await;
        let (tasks, cut) = keys::track(&state.db, project_id, ID, cred, result).await?;
        fetched.truncated |= cut;
        fetched.rules.push((rule.clone(), tasks));
    }
    Ok(fetched)
}

/// Moves or copies one task. Returns the id of the copy.
async fn apply(
    state: &AppState,
    project_id: i64,
    cred: &Credential,
    tz: &TimeZone,
    item: &Planned,
) -> Result<Option<String>, ClickUpError> {
    let list_id = item
        .task
        .list
        .as_ref()
        .map(|l| l.id.clone())
        .ok_or_else(|| ClickUpError::Decode("the task has no list".into()))?;
    let status = match item.rule.reopen_status.as_str() {
        "" => open_status(state, project_id, cred, &list_id).await?,
        name => name.to_owned(),
    };
    let client = &state.clickup;
    match item.rule.mode {
        Mode::Update => {
            let update = TaskUpdate {
                status: Some(status),
                due_date: Some(item.new_due),
                start_date: item.new_start,
            };
            let result = client
                .update_task(&cred.token, &item.task.id, &update)
                .await;
            keys::track(&state.db, project_id, ID, cred, result).await?;
            Ok(None)
        }
        Mode::Create => {
            let task = new_task(item, status, tz);
            let result = client.create_task(&cred.token, &list_id, &task).await;
            let created = keys::track(&state.db, project_id, ID, cred, result).await?;
            Ok(Some(created.id))
        }
    }
}

/// The copy of a task at its next date. Checklists, subtasks, custom fields and
/// attachments are not part of it.
fn new_task(item: &Planned, status: String, tz: &TimeZone) -> NewTask {
    let t = &item.task;
    let description = if t.markdown_description.trim().is_empty() {
        &t.text_content
    } else {
        &t.markdown_description
    };
    NewTask {
        name: t.name.clone(),
        markdown_content: description.clone(),
        assignees: t.assignees.iter().map(|u| u.id).collect(),
        tags: t.tags.iter().map(|tag| tag.name().to_owned()).collect(),
        status: Some(status),
        priority: t.priority.as_ref().and_then(Priority::level),
        due_date: Some(item.new_due),
        due_date_time: !schedule::looks_dateless(item.new_due, tz),
        start_date: item.new_start,
        start_date_time: item
            .new_start
            .is_some_and(|s| !schedule::looks_dateless(s, tz)),
        parent: t.parent.clone(),
    }
}

/// The first open status of a list, which ClickUp's own recurrence also returns to.
async fn open_status(
    state: &AppState,
    project_id: i64,
    cred: &Credential,
    list_id: &str,
) -> Result<String, ClickUpError> {
    let key = format!("recurring:status:{project_id}:{list_id}");
    if let Some(hit) = state.cache.get::<String>(&key, STATUS_MAX_AGE) {
        return Ok((*hit).clone());
    }
    let result = state.clickup.list(&cred.token, list_id).await;
    let list = keys::track(&state.db, project_id, ID, cred, result).await?;
    let status = list
        .statuses
        .iter()
        .find(|s| s.kind == "open")
        .or(list.statuses.first())
        .map(|s| s.status.clone())
        .ok_or_else(|| ClickUpError::Decode("the list has no statuses".into()))?;
    Ok((*state.cache.insert(key, status)).clone())
}

// ---------------------------------------------------------------- dashboard preview

/// What the next run would do, from tasks read at most [`PREVIEW_MAX_AGE`] ago. Never
/// changes anything in ClickUp.
pub async fn preview(
    state: &AppState,
    project_id: i64,
    now: i64,
) -> Result<Result<(Plan, bool), Problem>> {
    let cfg = load_config(&state.db, project_id).await?;
    let Some(team_id) = cfg.team_id.clone() else {
        return Ok(Err(Problem::NoWorkspace));
    };
    let rules = active_rules(state, project_id).await?;
    if rules.is_empty() {
        return Ok(Err(Problem::NoRules));
    }
    let cred = match keys::resolve(state, project_id, Service::ClickUp, ID).await? {
        Ok(c) => c,
        Err(p) => return Ok(Err(Problem::Token(p))),
    };
    // Keyed by the key itself, so a replaced key never shows the old account's tasks.
    let fingerprint = &crate::auth::hash_token(cred.token.expose())[..16];
    let key = format!("recurring:preview:{project_id}:{fingerprint}");
    let now_ms = now * 1000;
    let fetched = match state.cache.get::<Fetched>(&key, PREVIEW_MAX_AGE) {
        Some(hit) => hit,
        None => match fetch(state, project_id, &cred, &team_id, &rules, now_ms).await {
            Ok(f) => state.cache.insert(key, f),
            Err(e) => return Ok(Err(Problem::Api(e))),
        },
    };
    let done: HashSet<_> = done_markers(&state.db, project_id).await?;
    let plan = plan::plan(&fetched.rules, now_ms, &cfg.tz(), &done, MAX_WRITES_PER_RUN);
    Ok(Ok((plan, fetched.truncated)))
}

/// Drops the tasks the preview read, after settings, rules or tasks changed.
pub fn invalidate_preview(state: &AppState, project_id: i64) {
    state
        .cache
        .remove_prefix(&format!("recurring:preview:{project_id}:"));
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::clickup::mock::{self, MockClickUp};
    use crate::modules::recurring::{
        Unit, insert_rule, recent_log, save_config, set_last_run, update_rule,
    };
    use crate::modules::{counter, set_enabled};
    use crate::secrets::SecretString;
    use crate::test_util::{self, PID};

    const DAY: i64 = 86_400;
    // Thursday 2026-10-15 09:00:00 UTC.
    const NOW: i64 = 1_792_054_800;

    struct Setup {
        state: AppState,
        mock: MockClickUp,
    }

    /// Shared ClickUp key, the mock's workspace, live mode, UTC.
    async fn setup() -> Setup {
        let mut state = test_util::state().await;
        let mock = MockClickUp::start().await;
        state.clickup = mock.client();
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
        let cfg = RecurringConfig {
            team_id: Some(mock::TEAM_ID.into()),
            team_name: Some("Scorpio Team".into()),
            dry_run: false,
            ..Default::default()
        };
        save_config(db, PID, &cfg, "root").await.unwrap();
        Setup { state, mock }
    }

    async fn add_rule(s: &Setup, edit: impl FnOnce(&mut Rule)) -> Rule {
        let mut rule = Rule {
            name: "Weekly".into(),
            tag: "weekly".into(),
            ..Rule::new()
        };
        edit(&mut rule);
        rule.id = insert_rule(&s.state.db, PID, &rule, "root").await.unwrap();
        rule
    }

    /// A task tagged `weekly`, due `due` (unix seconds), in a status of `kind`.
    fn tagged(id: &str, due: i64, status: &str, kind: &str) -> Value {
        let mut t = mock::task(id, &format!("Task {id}"), Some(due * 1000));
        t["status"] = json!({"status": status, "color": "#000", "type": kind});
        t["tags"] = json!([{"name": "weekly"}]);
        t
    }

    fn report(outcome: Outcome) -> Report {
        match outcome {
            Outcome::Done(r) => r,
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn stat(s: &Setup, key: &str) -> i64 {
        counter(&s.state.db, Some(PID), ID, key).await.unwrap()
    }

    #[tokio::test]
    async fn moves_done_tasks_and_reopens_them() {
        let s = setup().await;
        add_rule(&s, |_| {}).await;
        let mut task = tagged("t1", NOW - DAY, "complete", "closed");
        task["start_date"] = json!(((NOW - 2 * DAY) * 1000).to_string());
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![
                task,
                tagged("open", NOW - DAY, "to do", "open"),
                tagged("review", NOW - 3 * DAY, "review", "done"),
            ],
        );

        let r = report(run(&s.state, PID, NOW, false).await.unwrap());
        assert_eq!((r.changed, r.failed, r.dry_run), (2, 0, false));

        let fetch = &s.mock.requests()[0];
        assert_eq!(fetch.path, format!("/team/{}/task", mock::TEAM_ID));
        for part in ["tags%5B%5D=weekly", "include_closed=true", "subtasks=true"] {
            assert!(fetch.query.contains(part), "{}", fetch.query);
        }
        assert!(
            !fetch.query.contains("due_date_gt") && !fetch.query.contains("due_date_lt"),
            "close rules read every due date"
        );

        let writes = s.mock.writes();
        assert_eq!(writes.len(), 2);
        assert!(writes.iter().all(|w| w.method == "PUT"));
        let body = |path: &str| writes.iter().find(|w| w.path == path).unwrap().json();
        assert_eq!(
            body("/task/t1"),
            json!({
                "status": "to do",
                "due_date": (NOW + 6 * DAY) * 1000,
                "start_date": (NOW + 5 * DAY) * 1000
            })
        );
        let review = body("/task/review");
        assert_eq!(review["due_date"], json!((NOW + 4 * DAY) * 1000));
        assert!(review.get("start_date").is_none(), "no start date to move");
        // The list's statuses were read once for both tasks.
        let list_reads = s
            .mock
            .requests()
            .iter()
            .filter(|r| r.path == format!("/list/{}", mock::LIST_ID))
            .count();
        assert_eq!(list_reads, 1);

        let stored = s.mock.tasks(mock::LIST_ID);
        assert_eq!(stored[0]["status"]["status"], "to do");
        assert_eq!(stored[0]["due_date"], ((NOW + 6 * DAY) * 1000).to_string());
        assert_eq!(stat(&s, "moved").await, 2);
        let log = recent_log(&s.state.db, PID, 10).await.unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.iter().all(|l| l.success && !l.dry_run));
        let t1 = log.iter().find(|l| l.task_name == "Task t1").unwrap();
        assert_eq!((t1.old_due, t1.new_due), (NOW - DAY, NOW + 6 * DAY));

        // Reopened tasks do not match any more.
        let r = report(run(&s.state, PID, NOW + 60, true).await.unwrap());
        assert_eq!(r.changed, 0);
        assert_eq!(s.mock.writes().len(), 2);
    }

    #[tokio::test]
    async fn dry_run_only_logs_once() {
        let s = setup().await;
        let mut cfg = load_config(&s.state.db, PID).await.unwrap();
        cfg.dry_run = true;
        save_config(&s.state.db, PID, &cfg, "root").await.unwrap();
        add_rule(&s, |_| {}).await;
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![tagged("t1", NOW - DAY, "complete", "closed")],
        );

        for at in [NOW, NOW + 3600] {
            let r = report(run(&s.state, PID, at, true).await.unwrap());
            assert_eq!((r.changed, r.dry_run), (1, true));
        }
        assert!(s.mock.writes().is_empty());
        assert!(s.mock.requests().iter().all(|r| r.method == "GET"));
        let log = recent_log(&s.state.db, PID, 10).await.unwrap();
        assert_eq!(log.len(), 1, "the same occurrence is logged once");
        assert!(log[0].dry_run);
        assert_eq!(stat(&s, "moved").await, 0);
        assert!(done_markers(&s.state.db, PID).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn copies_due_tasks_once() {
        let s = setup().await;
        add_rule(&s, |r| {
            r.trigger = Trigger::Schedule;
            r.mode = Mode::Create;
        })
        .await;
        let mut task = tagged("t1", NOW - 3600, "in progress", "custom");
        task["assignees"] = json!([{"id": 3, "username": "Anna Nowak", "email": "anna@x.io"}]);
        task["priority"] = json!({"id": "2", "priority": "high", "color": "#f00"});
        task["markdown_description"] = json!("**Agenda**");
        task["parent"] = json!("p1");
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![task, tagged("old", NOW - 61 * DAY, "to do", "open")],
        );

        let r = report(run(&s.state, PID, NOW, false).await.unwrap());
        assert_eq!(r.changed, 1);
        let fetch = &s.mock.requests()[0];
        let lookback = (NOW - super::CREATE_LOOKBACK_SECS) * 1000 - 1;
        for part in [
            format!("due_date_gt={lookback}"),
            format!("due_date_lt={}", NOW * 1000 + 1),
            "include_markdown_description=true".to_owned(),
        ] {
            assert!(fetch.query.contains(&part), "{}", fetch.query);
        }

        let writes = s.mock.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            (writes[0].method.as_str(), writes[0].path.as_str()),
            ("POST", format!("/list/{}/task", mock::LIST_ID).as_str())
        );
        assert_eq!(
            writes[0].json(),
            json!({
                "name": "Task t1",
                "markdown_content": "**Agenda**",
                "assignees": [3],
                "tags": ["weekly"],
                "status": "to do",
                "priority": 2,
                "due_date": (NOW - 3600 + 7 * DAY) * 1000,
                "due_date_time": true,
                "start_date_time": false,
                "parent": "p1"
            })
        );
        let stored = s.mock.tasks(mock::LIST_ID);
        assert_eq!(stored.len(), 3);
        assert_eq!(
            stored[0]["status"]["status"], "in progress",
            "the original stays"
        );
        let markers = done_markers(&s.state.db, PID).await.unwrap();
        assert!(markers.contains(&("t1".to_owned(), (NOW - 3600) * 1000)));
        assert_eq!(stat(&s, "copied").await, 1);

        // The original is still overdue, but this occurrence was handled.
        let r = report(run(&s.state, PID, NOW + 600, true).await.unwrap());
        assert_eq!(r.changed, 0);
        assert_eq!(s.mock.writes().len(), 1);
    }

    #[tokio::test]
    async fn dateless_copies_stay_dateless() {
        let s = setup().await;
        add_rule(&s, |r| {
            r.trigger = Trigger::Schedule;
            r.mode = Mode::Create;
            r.unit = Unit::Day;
        })
        .await;
        // 04:00 UTC: a ClickUp date without a time.
        let four = NOW - 5 * 3600;
        s.mock
            .set_tasks(mock::LIST_ID, vec![tagged("t1", four, "to do", "open")]);
        report(run(&s.state, PID, NOW, true).await.unwrap());
        let body = s.mock.writes()[0].json();
        assert_eq!(body["due_date"], json!((four + DAY) * 1000));
        assert_eq!(body["due_date_time"], json!(false));
    }

    #[tokio::test]
    async fn runs_wait_for_the_interval_and_the_lock() {
        let s = setup().await;
        add_rule(&s, |_| {}).await;
        let db = &s.state.db;
        set_last_run(db, PID, NOW - 60).await.unwrap();
        assert_eq!(
            run(&s.state, PID, NOW, false).await.unwrap(),
            Outcome::NotDue
        );
        assert!(
            s.mock.requests().is_empty(),
            "no ClickUp call before the interval"
        );

        assert!(try_lock(db, PID, NOW).await.unwrap());
        assert_eq!(run(&s.state, PID, NOW, true).await.unwrap(), Outcome::Busy);
        unlock(db, PID).await.unwrap();

        let r = report(run(&s.state, PID, NOW + 15 * 60, false).await.unwrap());
        assert_eq!(r.changed, 0);
        let cfg = load_config(db, PID).await.unwrap();
        assert_eq!(cfg.last_run_at, Some(NOW + 15 * 60));
        assert!(try_lock(db, PID, NOW + 15 * 60).await.unwrap(), "released");
    }

    #[tokio::test]
    async fn problems_make_no_writes() {
        let s = setup().await;
        let db = &s.state.db;
        assert_eq!(
            run(&s.state, PID, NOW, true).await.unwrap(),
            Outcome::Problem(Problem::NoRules)
        );
        let mut rule = add_rule(&s, |_| {}).await;

        set_enabled(db, PID, "clickup", false).await.unwrap();
        assert_eq!(
            run(&s.state, PID, NOW, true).await.unwrap(),
            Outcome::Problem(Problem::Token(KeyProblem::SharedModuleDisabled))
        );
        assert!(s.mock.requests().is_empty());
        set_enabled(db, PID, "clickup", true).await.unwrap();

        s.mock.fail_with(Some(429));
        assert_eq!(
            run(&s.state, PID, NOW, true).await.unwrap(),
            Outcome::Problem(Problem::Api(ClickUpError::RateLimited))
        );
        s.mock.fail_with(None);
        assert!(s.mock.writes().is_empty());
        assert_eq!(stat(&s, "api_err").await, 1);

        rule.enabled = false;
        update_rule(db, PID, &rule, "root").await.unwrap();
        assert_eq!(
            run(&s.state, PID, NOW, true).await.unwrap(),
            Outcome::Problem(Problem::NoRules)
        );
        let mut cfg = load_config(db, PID).await.unwrap();
        cfg.team_id = None;
        save_config(db, PID, &cfg, "root").await.unwrap();
        assert_eq!(
            run(&s.state, PID, NOW, true).await.unwrap(),
            Outcome::Problem(Problem::NoWorkspace)
        );
    }

    #[tokio::test]
    async fn rejected_changes_are_logged_once() {
        let s = setup().await;
        add_rule(&s, |r| r.reopen_status = "no such status".into()).await;
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![tagged("t1", NOW - DAY, "complete", "closed")],
        );

        for at in [NOW, NOW + 900] {
            let r = report(run(&s.state, PID, at, true).await.unwrap());
            assert_eq!((r.changed, r.failed), (0, 1));
        }
        let log = recent_log(&s.state.db, PID, 10).await.unwrap();
        assert_eq!(log.len(), 1);
        assert!(!log[0].success);
        let error = log[0].error.as_deref().unwrap();
        assert!(error.contains("Status does not exist"), "{error}");
        assert!(!error.contains(mock::TOKEN));
        assert_eq!(stat(&s, "failed").await, 1);
        assert_eq!(stat(&s, "moved").await, 0);
        assert!(done_markers(&s.state.db, PID).await.unwrap().is_empty());
        let stored = s.mock.tasks(mock::LIST_ID);
        assert_eq!(stored[0]["status"]["status"], "complete");
    }

    #[tokio::test]
    async fn failed_copies_are_retried() {
        let s = setup().await;
        let mut rule = add_rule(&s, |r| {
            r.mode = Mode::Create;
            r.reopen_status = "nope".into();
        })
        .await;
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![tagged("t1", NOW - DAY, "complete", "closed")],
        );
        let r = report(run(&s.state, PID, NOW, true).await.unwrap());
        assert_eq!(r.failed, 1);
        assert!(done_markers(&s.state.db, PID).await.unwrap().is_empty());

        rule.reopen_status.clear();
        update_rule(&s.state.db, PID, &rule, "root").await.unwrap();
        let r = report(run(&s.state, PID, NOW + 60, true).await.unwrap());
        assert_eq!((r.changed, r.failed), (1, 0));
        let new_id: Option<String> = sqlx::query_scalar("SELECT new_task_id FROM recurring_done")
            .fetch_one(&s.state.db)
            .await
            .unwrap();
        assert_eq!(new_id.as_deref(), Some("new1"));
        assert_eq!(s.mock.tasks(mock::LIST_ID).len(), 2);
    }

    #[tokio::test]
    async fn unclear_copy_failures_are_not_retried() {
        let s = setup().await;
        add_rule(&s, |r| r.mode = Mode::Create).await;
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![tagged("t1", NOW - DAY, "complete", "closed")],
        );
        s.mock.fail_writes_with(Some(502));
        let r = report(run(&s.state, PID, NOW, true).await.unwrap());
        assert_eq!(r.failed, 1);
        s.mock.fail_writes_with(None);

        // ClickUp may have made the copy before failing, so it is not tried again.
        let r = report(run(&s.state, PID, NOW + 60, true).await.unwrap());
        assert_eq!((r.changed, r.failed), (0, 0));
        assert_eq!(s.mock.writes().len(), 1);
        let log = recent_log(&s.state.db, PID, 10).await.unwrap();
        assert_eq!(log.len(), 1);
        assert!(!log[0].success);
    }

    #[tokio::test]
    async fn preview_matches_the_run_and_never_writes() {
        let s = setup().await;
        add_rule(&s, |_| {}).await;
        s.mock.set_tasks(
            mock::LIST_ID,
            vec![
                tagged("t1", NOW - DAY, "complete", "closed"),
                tagged("t2", NOW - DAY, "to do", "open"),
            ],
        );
        let (plan, truncated) = preview(&s.state, PID, NOW).await.unwrap().unwrap();
        assert!(!truncated);
        let ids: Vec<&str> = plan.items.iter().map(|p| p.task.id.as_str()).collect();
        assert_eq!(ids, ["t1"]);
        // Cached: a second preview reads nothing.
        let reads = s.mock.requests().len();
        preview(&s.state, PID, NOW).await.unwrap().unwrap();
        assert_eq!(s.mock.requests().len(), reads);
        assert!(s.mock.writes().is_empty());

        report(run(&s.state, PID, NOW, true).await.unwrap());
        let (plan, _) = preview(&s.state, PID, NOW).await.unwrap().unwrap();
        assert!(plan.items.is_empty(), "the run refreshed the preview");
    }
}
