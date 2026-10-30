//! Events read from the configured ClickUp list.

use std::sync::Arc;
use std::time::Duration;

use super::{CalendarConfig, ID};
use crate::clickup::ClickUpError;
use crate::clickup::client::{Task, TaskQuery, User};
use crate::keys::{self, KeyProblem, Service};
use crate::state::AppState;

pub const MAX_PAGES: u32 = 5;
/// Always fetch at least this far ahead, so the dashboard's upcoming list is covered too.
pub const MIN_HORIZON_SECS: i64 = 7 * 86400;
/// Reuse window for the dashboard.
pub const MAX_AGE: Duration = Duration::from_secs(5 * 60);
/// Reuse window for posting: events created or moved shortly before their reminder must
/// still be seen, so the bot reads ClickUp about once a minute while a rule needs it.
pub const FRESH: Duration = Duration::from_secs(55);
/// Events that started this long ago are still read, so the dashboard can show them.
pub const LOOKBACK_SECS: i64 = 12 * 3600;

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub task_id: String,
    pub name: String,
    pub url: String,
    /// When the event happens (unix seconds): the due date, or the start date in start mode.
    pub at: i64,
    pub start: Option<i64>,
    pub due: Option<i64>,
    pub assignees: Vec<User>,
    pub description: String,
    pub status: String,
    pub tags: Vec<String>,
    pub priority: Option<String>,
    pub list: String,
}

impl Event {
    /// None for tasks without the date the calendar uses.
    pub fn from_task(task: &Task, start_mode: bool) -> Option<Self> {
        let due = task.due_date.map(|ms| ms.div_euclid(1000));
        let start = task.start_date.map(|ms| ms.div_euclid(1000));
        let at = if start_mode { start.or(due) } else { due }?;
        Some(Self {
            task_id: task.id.clone(),
            name: task.name.trim().to_owned(),
            url: task.url.clone(),
            at,
            start,
            due,
            assignees: task.assignees.clone(),
            description: task.text_content.trim().to_owned(),
            status: task
                .status
                .as_ref()
                .map(|s| s.status.clone())
                .unwrap_or_default(),
            tags: task.tags.iter().map(|t| t.name().to_owned()).collect(),
            priority: task.priority.as_ref().map(|p| p.priority.clone()),
            list: task
                .list
                .as_ref()
                .map(|l| l.name.clone())
                .unwrap_or_default(),
        })
    }

    /// A made-up event at `at` for previews and tests when the list has nothing upcoming.
    pub fn sample(at: i64) -> Self {
        Self {
            task_id: "sample".into(),
            name: "Weekly sync".into(),
            url: "https://app.clickup.com/".into(),
            at,
            start: None,
            due: Some(at),
            assignees: Vec::new(),
            description: "Agenda: status updates, blockers, next steps.".into(),
            status: "to do".into(),
            tags: vec!["meeting".into()],
            priority: None,
            list: "Calendar".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FetchError {
    NoList,
    Token(KeyProblem),
    Api(ClickUpError),
}

impl FetchError {
    pub fn message(&self) -> String {
        match self {
            FetchError::NoList => "No ClickUp list is selected.".into(),
            FetchError::Token(p) => p.message(Service::ClickUp).into(),
            FetchError::Api(e) => e.to_string(),
        }
    }

    /// Flash code for dashboard redirects.
    pub fn code(&self) -> &'static str {
        match self {
            FetchError::NoList => "calendar_no_list",
            FetchError::Token(KeyProblem::NotSet) => "token_missing",
            FetchError::Token(KeyProblem::Unreadable) => "token_unreadable",
            FetchError::Token(KeyProblem::SharedModuleDisabled) => "clickup_disabled",
            FetchError::Api(ClickUpError::Unauthorized) => "token_check_failed",
            FetchError::Api(ClickUpError::NotFound) => "list_not_found",
            FetchError::Api(_) => "clickup_unreachable",
        }
    }
}

/// Events of one fetch, sorted by time then name.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub events: Vec<Event>,
    pub to: i64,
    /// More tasks matched than [`MAX_PAGES`] pages hold.
    pub truncated: bool,
}

impl Fetched {
    /// Events with `from < at <= to`.
    pub fn between(&self, from: i64, to: i64) -> Vec<Event> {
        self.events
            .iter()
            .filter(|e| e.at > from && e.at <= to)
            .cloned()
            .collect()
    }
}

/// Reads events happening between `from` and `to` (unix seconds) from ClickUp.
pub async fn fetch(
    state: &AppState,
    project_id: i64,
    cfg: &CalendarConfig,
    from: i64,
    to: i64,
) -> anyhow::Result<Result<Fetched, FetchError>> {
    let Some(list_id) = cfg.list_id.as_deref() else {
        return Ok(Err(FetchError::NoList));
    };
    let cred = match keys::resolve(state, project_id, Service::ClickUp, ID).await? {
        Ok(c) => c,
        Err(p) => return Ok(Err(FetchError::Token(p))),
    };
    let start_mode = cfg.start_mode();
    // ClickUp cannot filter by start date and drops tasks without a due date under a due
    // date filter, so start mode reads the open tasks of the list and filters below.
    let due_filter = !start_mode;
    let query = TaskQuery {
        due_after_ms: due_filter.then(|| from * 1000 - 1),
        due_before_ms: due_filter.then(|| to * 1000 + 1),
        include_closed: cfg.include_closed,
        subtasks: cfg.include_subtasks,
        tags: cfg.tags(),
        markdown: false,
    };
    let result = state
        .clickup
        .tasks(&cred.token, list_id, &query, MAX_PAGES)
        .await;
    let (tasks, truncated) = match keys::track(&state.db, project_id, ID, &cred, result).await {
        Ok(r) => r,
        Err(e) => return Ok(Err(FetchError::Api(e))),
    };
    let mut events: Vec<Event> = tasks
        .iter()
        .filter_map(|t| Event::from_task(t, start_mode))
        .filter(|e| e.at >= from && e.at <= to)
        .collect();
    events.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.name.cmp(&b.name)));
    Ok(Ok(Fetched {
        events,
        to,
        truncated,
    }))
}

fn cache_prefix(project_id: i64) -> String {
    format!("calendar:events:{project_id}:")
}

/// Changing any of these settings must not reuse events fetched with the old ones.
fn cache_key(project_id: i64, cfg: &CalendarConfig) -> String {
    format!(
        "{}{}|{}|{}|{}|{}",
        cache_prefix(project_id),
        cfg.list_id.as_deref().unwrap_or_default(),
        cfg.time_field,
        cfg.tag_filter,
        cfg.include_subtasks,
        cfg.include_closed
    )
}

/// Events from now until at least `now + horizon`, reusing a fetch younger than `max_age`.
pub async fn cached(
    state: &AppState,
    project_id: i64,
    cfg: &CalendarConfig,
    now: i64,
    horizon: i64,
    max_age: Duration,
) -> anyhow::Result<Result<Arc<Fetched>, FetchError>> {
    let key = cache_key(project_id, cfg);
    let horizon = horizon.max(MIN_HORIZON_SECS);
    if let Some(hit) = state.cache.get::<Fetched>(&key, max_age)
        && hit.to >= now + horizon
    {
        return Ok(Ok(hit));
    }
    // Fetch a little further than needed so the next minutes are still covered.
    let fetched = fetch(
        state,
        project_id,
        cfg,
        now - LOOKBACK_SECS,
        now + horizon + 3600,
    )
    .await?;
    Ok(fetched.map(|f| state.cache.insert(key, f)))
}

pub fn invalidate(state: &AppState, project_id: i64) {
    state.cache.remove_prefix(&cache_prefix(project_id));
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::clickup::mock::{self, MockClickUp};
    use crate::modules::set_enabled;
    use crate::secrets::SecretString;
    use crate::test_util;

    const NOW: i64 = 1_800_000_000;

    fn task(value: serde_json::Value) -> Task {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn task_conversion() {
        let mut t = mock::task("a", " Standup ", Some(NOW * 1000 + 999));
        t["start_date"] = json!((NOW - 600) * 1000);
        t["assignees"] = json!([{"id": 3, "username": "Anna Nowak", "email": "a@x.io"}]);
        t["tags"] = json!([{"name": "meeting"}]);
        t["priority"] = json!({"priority": "high"});
        let t = task(t);

        let due_mode = Event::from_task(&t, false).unwrap();
        assert_eq!(due_mode.at, NOW);
        assert_eq!(due_mode.name, "Standup");
        assert_eq!(due_mode.start, Some(NOW - 600));
        assert_eq!(due_mode.tags, ["meeting"]);
        assert_eq!(due_mode.priority.as_deref(), Some("high"));
        assert_eq!(due_mode.status, "to do");
        assert_eq!(due_mode.list, "Calendar");
        assert_eq!(due_mode.assignees[0].username, "Anna Nowak");
        assert_eq!(Event::from_task(&t, true).unwrap().at, NOW - 600);

        let undated = task(mock::task("b", "Someday", None));
        assert!(Event::from_task(&undated, false).is_none());
        let mut start_only = mock::task("c", "Offsite", None);
        start_only["start_date"] = json!((NOW * 1000).to_string());
        let start_only = task(start_only);
        assert!(Event::from_task(&start_only, false).is_none());
        assert_eq!(Event::from_task(&start_only, true).unwrap().at, NOW);
    }

    /// State with the mock's key saved as the shared key and the ClickUp module enabled.
    async fn setup() -> (AppState, MockClickUp, CalendarConfig) {
        let mut state = test_util::state().await;
        let mock = MockClickUp::start().await;
        state.clickup = mock.client();
        set_enabled(&state.db, 1, "clickup", true).await.unwrap();
        let key = SecretString::new(mock::TOKEN);
        keys::store(
            &state.db,
            &state.vault,
            1,
            Service::ClickUp,
            "clickup",
            &key,
            "Test Owner",
            "root",
        )
        .await
        .unwrap();
        let cfg = CalendarConfig {
            list_id: Some(mock::LIST_ID.into()),
            ..Default::default()
        };
        (state, mock, cfg)
    }

    fn at(secs: i64) -> Option<i64> {
        Some(secs * 1000)
    }

    #[tokio::test]
    async fn fetch_filters_sorts_and_handles_start_mode() {
        let (state, mock, mut cfg) = setup().await;
        let mut long = mock::task("long", "Conference", at(NOW + 20 * 86400));
        long["start_date"] = json!(((NOW + 3600) * 1000).to_string());
        let mut start_only = mock::task("start_only", "Workshop", None);
        start_only["start_date"] = json!(((NOW + 5400) * 1000).to_string());
        mock.set_tasks(
            mock::LIST_ID,
            vec![
                mock::task("later", "B later", at(NOW + 7200)),
                mock::task("soon", "A soon", at(NOW + 3600)),
                mock::task("same", "A same time", at(NOW + 3600)),
                mock::task("past", "Past", at(NOW - 60)),
                mock::task("far", "Far", at(NOW + 30 * 86400)),
                long,
                start_only,
            ],
        );

        let f = fetch(&state, 1, &cfg, NOW, NOW + 86400)
            .await
            .unwrap()
            .unwrap();
        let ids: Vec<&str> = f.events.iter().map(|e| e.task_id.as_str()).collect();
        assert_eq!(ids, ["same", "soon", "later"]);
        assert!(!f.truncated);
        let due_query = mock.requests().last().unwrap().query.clone();
        assert!(due_query.contains("due_date_gt="), "{due_query}");
        assert!(due_query.contains("due_date_lt="), "{due_query}");

        // Start mode finds the conference that starts soon but is due in 20 days, and the
        // workshop without a due date, which a due date filter would hide.
        cfg.time_field = "start".into();
        let f = fetch(&state, 1, &cfg, NOW, NOW + 86400)
            .await
            .unwrap()
            .unwrap();
        assert!(
            f.events
                .iter()
                .any(|e| e.task_id == "long" && e.at == NOW + 3600)
        );
        assert!(
            f.events
                .iter()
                .any(|e| e.task_id == "start_only" && e.at == NOW + 5400)
        );
        let start_query = mock.requests().last().unwrap().query.clone();
        assert!(!start_query.contains("due_date_"), "{start_query}");
        assert_eq!(
            f.between(NOW + 3600, NOW + 86400).len(),
            2,
            "after `from` only: the workshop and the later task"
        );

        assert!(mock.requests().iter().all(|r| r.method == "GET"));
    }

    #[tokio::test]
    async fn cache_reuses_until_settings_change() {
        let (state, mock, mut cfg) = setup().await;
        mock.set_tasks(mock::LIST_ID, vec![mock::task("a", "A", at(NOW + 60))]);

        let first = cached(&state, 1, &cfg, NOW, 3600, MAX_AGE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.events.len(), 1);
        let calls = mock.requests().len();
        cached(&state, 1, &cfg, NOW + 30, 3600, MAX_AGE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mock.requests().len(), calls, "served from the cache");

        // A longer horizon than fetched, a settings change or a refresh fetch again.
        cached(&state, 1, &cfg, NOW, 30 * 86400, MAX_AGE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mock.requests().len(), calls + 1);
        cfg.include_closed = true;
        cached(&state, 1, &cfg, NOW, 3600, MAX_AGE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mock.requests().len(), calls + 2);
        invalidate(&state, 1);
        cached(&state, 1, &cfg, NOW, 3600, MAX_AGE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mock.requests().len(), calls + 3);
        cached(&state, 1, &cfg, NOW, 3600, Duration::ZERO)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mock.requests().len(), calls + 4);
    }

    #[tokio::test]
    async fn errors_without_list_key_or_access() {
        let (state, mock, cfg) = setup().await;
        let no_list = CalendarConfig::default();
        assert_eq!(
            fetch(&state, 1, &no_list, NOW, NOW + 60)
                .await
                .unwrap()
                .unwrap_err(),
            FetchError::NoList
        );
        assert_eq!(
            fetch(&state, 2, &cfg, NOW, NOW + 60)
                .await
                .unwrap()
                .unwrap_err(),
            FetchError::Token(KeyProblem::SharedModuleDisabled)
        );
        let missing = CalendarConfig {
            list_id: Some("999".into()),
            ..Default::default()
        };
        let err = fetch(&state, 1, &missing, NOW, NOW + 60)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(err, FetchError::Api(ClickUpError::NotFound));
        assert_eq!(err.code(), "list_not_found");
        assert_eq!(
            crate::modules::counter(&state.db, Some(1), ID, "api_err")
                .await
                .unwrap(),
            1
        );
        // Failures are not cached.
        mock.fail_with(Some(500));
        assert!(
            cached(&state, 1, &cfg, NOW, 60, MAX_AGE)
                .await
                .unwrap()
                .is_err()
        );
        mock.fail_with(None);
        assert!(
            cached(&state, 1, &cfg, NOW, 60, MAX_AGE)
                .await
                .unwrap()
                .is_ok()
        );
    }
}
