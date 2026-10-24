//! ClickUp API v2 client.
//!
//! Almost everything here reads. The only writes are [`ClickUpClient::update_task`] and
//! [`ClickUpClient::create_task`], used by the recurring tasks module alone; every other
//! module only reads ClickUp.

use std::sync::Arc;
use std::time::Duration;

use reqwest::Method;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use serde::de::{DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};

use crate::secrets::SecretString;

pub const API_BASE: &str = "https://api.clickup.com/api/v2";
const TIMEOUT: Duration = Duration::from_secs(15);
/// ClickUp returns at most 100 tasks per page.
pub const TASKS_PER_PAGE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClickUpError {
    #[error("ClickUp rejected the API key")]
    Unauthorized,
    #[error("not found in ClickUp, or the API key has no access to it")]
    NotFound,
    #[error("ClickUp rate limit reached, try again in a minute")]
    RateLimited,
    /// HTTP 400 with ClickUp's explanation, like "Status does not exist".
    #[error("ClickUp refused the request: {0}")]
    Rejected(String),
    #[error("ClickUp answered with HTTP {0}")]
    Status(u16),
    #[error("could not reach ClickUp: {0}")]
    Network(String),
    #[error("unexpected response from ClickUp: {0}")]
    Decode(String),
}

#[derive(Clone)]
pub struct ClickUpClient {
    http: reqwest::Client,
    base: Arc<str>,
}

impl Default for ClickUpClient {
    fn default() -> Self {
        Self::with_base_url(API_BASE)
    }
}

impl ClickUpClient {
    pub fn with_base_url(base: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("scorpio-discord-bot/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("static reqwest configuration is valid");
        Self {
            http,
            base: base.trim_end_matches('/').into(),
        }
    }

    async fn get<T: DeserializeOwned>(
        &self,
        token: &SecretString,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, ClickUpError> {
        self.send(Method::GET, token, path, query, None::<&()>)
            .await
    }

    async fn send<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        method: Method,
        token: &SecretString,
        path: &str,
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> Result<T, ClickUpError> {
        let mut auth =
            HeaderValue::from_str(token.expose()).map_err(|_| ClickUpError::Unauthorized)?;
        auth.set_sensitive(true);

        let mut req = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header(AUTHORIZATION, auth)
            .query(query);
        if let Some(body) = body {
            req = req.json(body);
        }
        let res = req
            .send()
            .await
            .map_err(|e| ClickUpError::Network(e.without_url().to_string()))?;

        match res.status().as_u16() {
            200..=299 => {}
            400 => {
                let body = res.bytes().await.unwrap_or_default();
                return Err(ClickUpError::Rejected(rejection(&body)));
            }
            401 => return Err(ClickUpError::Unauthorized),
            403 | 404 => return Err(ClickUpError::NotFound),
            429 => return Err(ClickUpError::RateLimited),
            other => return Err(ClickUpError::Status(other)),
        }
        let body = res
            .bytes()
            .await
            .map_err(|e| ClickUpError::Network(e.without_url().to_string()))?;
        serde_json::from_slice(&body).map_err(|e| ClickUpError::Decode(e.to_string()))
    }

    /// The account the token belongs to. Also the cheapest way to validate a token.
    pub async fn user(&self, token: &SecretString) -> Result<User, ClickUpError> {
        #[derive(Deserialize)]
        struct Wrap {
            user: User,
        }
        let w: Wrap = self.get(token, "/user", &[]).await?;
        Ok(w.user)
    }

    /// Workspaces ("teams" in the API) the token can access, with their members.
    pub async fn teams(&self, token: &SecretString) -> Result<Vec<Team>, ClickUpError> {
        #[derive(Deserialize)]
        struct Wrap {
            teams: Vec<Team>,
        }
        let w: Wrap = self.get(token, "/team", &[]).await?;
        Ok(w.teams)
    }

    pub async fn spaces(
        &self,
        token: &SecretString,
        team_id: &str,
    ) -> Result<Vec<Named>, ClickUpError> {
        #[derive(Deserialize)]
        struct Wrap {
            spaces: Vec<Named>,
        }
        let path = format!("/team/{}/space", seg(team_id));
        let w: Wrap = self
            .get(token, &path, &[("archived", "false".into())])
            .await?;
        Ok(w.spaces)
    }

    /// Folders of a space, each with its lists.
    pub async fn folders(
        &self,
        token: &SecretString,
        space_id: &str,
    ) -> Result<Vec<Folder>, ClickUpError> {
        #[derive(Deserialize)]
        struct Wrap {
            folders: Vec<Folder>,
        }
        let path = format!("/space/{}/folder", seg(space_id));
        let w: Wrap = self
            .get(token, &path, &[("archived", "false".into())])
            .await?;
        Ok(w.folders)
    }

    /// Lists directly in a space, outside any folder.
    pub async fn folderless_lists(
        &self,
        token: &SecretString,
        space_id: &str,
    ) -> Result<Vec<Named>, ClickUpError> {
        #[derive(Deserialize)]
        struct Wrap {
            lists: Vec<Named>,
        }
        let path = format!("/space/{}/list", seg(space_id));
        let w: Wrap = self
            .get(token, &path, &[("archived", "false".into())])
            .await?;
        Ok(w.lists)
    }

    /// Tags defined in a space.
    pub async fn space_tags(
        &self,
        token: &SecretString,
        space_id: &str,
    ) -> Result<Vec<Tag>, ClickUpError> {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default, deserialize_with = "null_default")]
            tags: Vec<Tag>,
        }
        let path = format!("/space/{}/tag", seg(space_id));
        let w: Wrap = self.get(token, &path, &[]).await?;
        Ok(w.tags)
    }

    pub async fn list(&self, token: &SecretString, list_id: &str) -> Result<List, ClickUpError> {
        self.get(token, &format!("/list/{}", seg(list_id)), &[])
            .await
    }

    /// All tasks matching `query`, following pages until ClickUp reports the last one or
    /// `max_pages` were read. The flag is true when tasks were left out because of the cap.
    pub async fn tasks(
        &self,
        token: &SecretString,
        list_id: &str,
        query: &TaskQuery,
        max_pages: u32,
    ) -> Result<(Vec<Task>, bool), ClickUpError> {
        let path = format!("/list/{}/task", seg(list_id));
        self.all_pages(token, &path, query, max_pages).await
    }

    /// Like [`tasks`](Self::tasks), across every list of a workspace the key can see.
    pub async fn team_tasks(
        &self,
        token: &SecretString,
        team_id: &str,
        query: &TaskQuery,
        max_pages: u32,
    ) -> Result<(Vec<Task>, bool), ClickUpError> {
        let path = format!("/team/{}/task", seg(team_id));
        self.all_pages(token, &path, query, max_pages).await
    }

    async fn all_pages(
        &self,
        token: &SecretString,
        path: &str,
        query: &TaskQuery,
        max_pages: u32,
    ) -> Result<(Vec<Task>, bool), ClickUpError> {
        let mut tasks = Vec::new();
        for page in 0..max_pages {
            let p: TaskPage = self.get(token, path, &query.params(page)).await?;
            let full = p.tasks.len() >= TASKS_PER_PAGE;
            tasks.extend(p.tasks);
            if p.last_page.unwrap_or(!full) {
                return Ok((tasks, false));
            }
        }
        Ok((tasks, true))
    }

    /// Changes the fields set in `update`. Write call, only for the recurring tasks module.
    pub async fn update_task(
        &self,
        token: &SecretString,
        task_id: &str,
        update: &TaskUpdate,
    ) -> Result<(), ClickUpError> {
        let path = format!("/task/{}", seg(task_id));
        let _: serde::de::IgnoredAny = self
            .send(Method::PUT, token, &path, &[], Some(update))
            .await?;
        Ok(())
    }

    /// Creates a task in a list. Write call, only for the recurring tasks module.
    pub async fn create_task(
        &self,
        token: &SecretString,
        list_id: &str,
        task: &NewTask,
    ) -> Result<Task, ClickUpError> {
        let path = format!("/list/{}/task", seg(list_id));
        self.send(Method::POST, token, &path, &[], Some(task)).await
    }
}

/// ClickUp's explanation of a 400 answer, like "Status does not exist (ITEM_085)".
fn rejection(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Answer {
        #[serde(default, deserialize_with = "null_default")]
        err: String,
        #[serde(default, rename = "ECODE", deserialize_with = "null_default")]
        ecode: String,
    }
    let text = match serde_json::from_slice::<Answer>(body) {
        Ok(a) if !a.err.trim().is_empty() && !a.ecode.is_empty() => {
            format!("{} ({})", a.err.trim(), a.ecode)
        }
        Ok(a) if !a.err.trim().is_empty() => a.err.trim().to_owned(),
        _ => "no details given".to_owned(),
    };
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// Keeps ids from the database or forms from changing the request path.
fn seg(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct TaskQuery {
    pub due_after_ms: Option<i64>,
    pub due_before_ms: Option<i64>,
    pub include_closed: bool,
    pub subtasks: bool,
    pub tags: Vec<String>,
    /// Fill [`Task::markdown_description`].
    pub markdown: bool,
}

impl TaskQuery {
    fn params(&self, page: u32) -> Vec<(&'static str, String)> {
        let mut params = vec![
            ("page", page.to_string()),
            ("order_by", "due_date".into()),
            ("subtasks", self.subtasks.to_string()),
            ("include_closed", self.include_closed.to_string()),
        ];
        if let Some(ms) = self.due_after_ms {
            params.push(("due_date_gt", ms.to_string()));
        }
        if let Some(ms) = self.due_before_ms {
            params.push(("due_date_lt", ms.to_string()));
        }
        for tag in &self.tags {
            params.push(("tags[]", tag.clone()));
        }
        if self.markdown {
            params.push(("include_markdown_description", "true".into()));
        }
        params
    }
}

/// Fields [`ClickUpClient::update_task`] changes. None leaves a field as it is, which
/// also keeps whether a date has a time.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TaskUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<i64>,
    /// Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<i64>,
}

/// Body of [`ClickUpClient::create_task`].
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct NewTask {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub markdown_content: String,
    pub assignees: Vec<i64>,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 1 urgent, 2 high, 3 normal, 4 low.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    /// Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<i64>,
    /// The due date has a time of day, not only a date.
    pub due_date_time: bool,
    /// Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<i64>,
    pub start_date_time: bool,
    /// Creates a subtask of this task, which must be in the same list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct User {
    #[serde(deserialize_with = "int_id")]
    pub id: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub username: String,
    #[serde(default, deserialize_with = "null_default")]
    pub email: String,
    #[serde(default, deserialize_with = "null_default")]
    pub initials: String,
}

impl User {
    /// Username, or the email when ClickUp has no name for the account.
    pub fn display(&self) -> &str {
        if self.username.trim().is_empty() {
            &self.email
        } else {
            &self.username
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Team {
    #[serde(deserialize_with = "str_id")]
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub members: Vec<TeamMember>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TeamMember {
    pub user: User,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Named {
    #[serde(deserialize_with = "str_id")]
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Folder {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub lists: Vec<Named>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct List {
    #[serde(deserialize_with = "str_id")]
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default)]
    pub folder: Option<Named>,
    #[serde(default)]
    pub space: Option<Named>,
    /// In board order.
    #[serde(default, deserialize_with = "null_default")]
    pub statuses: Vec<Status>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TaskPage {
    #[serde(default)]
    pub tasks: Vec<Task>,
    #[serde(default)]
    pub last_page: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    #[serde(deserialize_with = "str_id")]
    pub id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    /// Plain text description.
    #[serde(default, deserialize_with = "null_default")]
    pub text_content: String,
    #[serde(default)]
    pub status: Option<Status>,
    /// Unix milliseconds.
    #[serde(default, deserialize_with = "opt_millis")]
    pub due_date: Option<i64>,
    /// Unix milliseconds.
    #[serde(default, deserialize_with = "opt_millis")]
    pub start_date: Option<i64>,
    #[serde(default, deserialize_with = "null_default")]
    pub assignees: Vec<User>,
    #[serde(default, deserialize_with = "null_default")]
    pub tags: Vec<Tag>,
    #[serde(default, deserialize_with = "null_default")]
    pub url: String,
    #[serde(default)]
    pub list: Option<Named>,
    #[serde(default)]
    pub priority: Option<Priority>,
    /// Id of the parent task, for subtasks.
    #[serde(default, deserialize_with = "opt_id")]
    pub parent: Option<String>,
    /// Only filled when asked for with [`TaskQuery::markdown`].
    #[serde(default, deserialize_with = "null_default")]
    pub markdown_description: String,
    #[serde(default, deserialize_with = "null_default")]
    pub archived: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Status {
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    /// `open`, `custom`, `done` or `closed`.
    #[serde(default, rename = "type", deserialize_with = "null_default")]
    pub kind: String,
}

impl Status {
    /// A done or closed status, the ones that count as completed.
    pub fn is_done(&self) -> bool {
        matches!(self.kind.as_str(), "done" | "closed")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Priority {
    #[serde(default, deserialize_with = "null_default")]
    pub priority: String,
}

impl Priority {
    /// The number ClickUp expects when setting a priority.
    pub fn level(&self) -> Option<u8> {
        match self.priority.to_lowercase().as_str() {
            "urgent" => Some(1),
            "high" => Some(2),
            "normal" => Some(3),
            "low" => Some(4),
            _ => None,
        }
    }
}

/// Tags are objects in task responses, but accept bare strings too.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Tag {
    Object { name: String },
    Name(String),
}

impl Tag {
    pub fn name(&self) -> &str {
        match self {
            Tag::Object { name } | Tag::Name(name) => name,
        }
    }
}

/// ClickUp sends some ids as strings and others as numbers.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawId {
    Str(String),
    Int(i64),
}

fn str_id<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match RawId::deserialize(d)? {
        RawId::Str(s) => s,
        RawId::Int(i) => i.to_string(),
    })
}

fn int_id<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    match RawId::deserialize(d)? {
        RawId::Int(i) => Ok(i),
        RawId::Str(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

fn opt_id<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(match Option::<RawId>::deserialize(d)? {
        None => None,
        Some(RawId::Str(s)) => Some(s),
        Some(RawId::Int(i)) => Some(i.to_string()),
    })
}

/// Dates are millisecond strings, sometimes numbers, or null.
fn opt_millis<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    Ok(match Option::<RawId>::deserialize(d)? {
        None => None,
        Some(RawId::Int(i)) => Some(i),
        Some(RawId::Str(s)) => s.trim().parse().ok(),
    })
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clickup::mock::{self, MockClickUp};

    fn token() -> SecretString {
        SecretString::new(mock::TOKEN)
    }

    #[test]
    fn task_json_is_parsed_leniently() {
        let task: Task = serde_json::from_value(serde_json::json!({
            "id": "abc1",
            "name": "Standup",
            "text_content": null,
            "status": {"status": "open", "color": "#d3d3d3"},
            "due_date": "1760000000000",
            "start_date": null,
            "assignees": [{"id": 5, "username": null, "email": "a@b.c"}],
            "tags": [{"name": "meeting", "tag_fg": "#fff"}, "plain"],
            "url": "https://app.clickup.com/t/abc1",
            "priority": null,
            "list": {"id": 400, "name": "Calendar"}
        }))
        .unwrap();
        assert_eq!(task.due_date, Some(1_760_000_000_000));
        assert_eq!(task.start_date, None);
        assert_eq!(task.text_content, "");
        assert_eq!(task.assignees[0].display(), "a@b.c");
        assert_eq!(task.tags[0].name(), "meeting");
        assert_eq!(task.tags[1].name(), "plain");
        assert_eq!(task.list.unwrap().id, "400");
    }

    #[test]
    fn path_segments_are_sanitized() {
        assert_eq!(seg("901234"), "901234");
        assert_eq!(seg("../team/1?x=y"), "team1xy");
        assert_eq!(seg("abc-1_2"), "abc-1_2");
    }

    #[tokio::test]
    async fn reads_user_teams_and_hierarchy() {
        let mock = MockClickUp::start().await;
        let client = mock.client();
        let t = token();

        assert_eq!(client.user(&t).await.unwrap().username, "Test Owner");
        let teams = client.teams(&t).await.unwrap();
        assert_eq!(teams[0].id, mock::TEAM_ID);
        assert!(teams[0].members.len() >= 2);
        let spaces = client.spaces(&t, mock::TEAM_ID).await.unwrap();
        assert_eq!(spaces[0].id, mock::SPACE_ID);
        let folders = client.folders(&t, mock::SPACE_ID).await.unwrap();
        assert_eq!(folders[0].lists[0].id, mock::LIST_ID);
        let lists = client.folderless_lists(&t, mock::SPACE_ID).await.unwrap();
        assert_eq!(lists[0].id, mock::OTHER_LIST_ID);
        let tags = client.space_tags(&t, mock::SPACE_ID).await.unwrap();
        let names: Vec<&str> = tags.iter().map(Tag::name).collect();
        assert_eq!(names, ["meeting", "Team Sync"]);
        let list = client.list(&t, mock::LIST_ID).await.unwrap();
        assert_eq!(list.name, "Calendar");
        assert_eq!(list.folder.unwrap().name, "Team");

        let auth: Vec<_> = mock.requests().into_iter().map(|r| r.auth).collect();
        assert!(auth.iter().all(|a| a.as_deref() == Some(mock::TOKEN)));
    }

    #[tokio::test]
    async fn errors_are_mapped_and_never_contain_the_token() {
        let mock = MockClickUp::start().await;
        let client = mock.client();

        let bad = SecretString::new("pk_wrong_token");
        let err = client.user(&bad).await.unwrap_err();
        assert_eq!(err, ClickUpError::Unauthorized);

        let err = client.list(&token(), "999").await.unwrap_err();
        assert_eq!(err, ClickUpError::NotFound);

        for (status, expected) in [
            (400, ClickUpError::Rejected("mock failure".into())),
            (429, ClickUpError::RateLimited),
            (500, ClickUpError::Status(500)),
        ] {
            mock.fail_with(Some(status));
            let err = client.user(&token()).await.unwrap_err();
            assert_eq!(err, expected);
            assert!(!err.to_string().contains(mock::TOKEN));
        }
        mock.fail_with(None);

        // Header injection attempts never reach the wire.
        let err = client
            .user(&SecretString::new("pk_1\r\nX-Evil: 1"))
            .await
            .unwrap_err();
        assert_eq!(err, ClickUpError::Unauthorized);

        let offline = ClickUpClient::with_base_url("http://127.0.0.1:9");
        match offline.user(&token()).await.unwrap_err() {
            ClickUpError::Network(msg) => assert!(!msg.contains(mock::TOKEN)),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn task_query_filters_and_pages() {
        let mock = MockClickUp::start().await;
        let base = 1_800_000_000_000_i64;
        let mut tasks = Vec::new();
        for i in 0..250 {
            tasks.push(mock::task(
                &format!("t{i}"),
                &format!("Task {i}"),
                Some(base + i * 1000),
            ));
        }
        tasks.push(mock::task("nodue", "No due date", None));
        let mut tagged = mock::task("tagged", "Tagged", Some(base));
        tagged["tags"] = serde_json::json!([{"name": "meeting"}]);
        tasks.push(tagged);
        let mut closed = mock::task("closed", "Closed", Some(base));
        closed["status"] = serde_json::json!({"status": "done", "color": "#000", "type": "closed"});
        tasks.push(closed);
        mock.set_tasks(mock::LIST_ID, tasks);

        let client = mock.client();
        let query = TaskQuery {
            due_after_ms: Some(base - 1),
            due_before_ms: Some(base + 300_000),
            ..Default::default()
        };
        let (all, truncated) = client
            .tasks(&token(), mock::LIST_ID, &query, 5)
            .await
            .unwrap();
        assert!(!truncated);
        assert_eq!(all.len(), 251, "250 + tagged, no undated, no closed");

        let (capped, truncated) = client
            .tasks(&token(), mock::LIST_ID, &query, 2)
            .await
            .unwrap();
        assert!(truncated);
        assert_eq!(capped.len(), 200);

        let tag_query = TaskQuery {
            tags: vec!["meeting".into()],
            include_closed: true,
            ..Default::default()
        };
        let (only, _) = client
            .tasks(&token(), mock::LIST_ID, &tag_query, 5)
            .await
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].id, "tagged");

        let reqs = mock.requests();
        assert!(reqs.iter().all(|r| r.method == "GET"));
        let last = reqs.last().unwrap();
        assert_eq!(last.path, format!("/list/{}/task", mock::LIST_ID));
        assert!(last.query.contains("tags%5B%5D=meeting"), "{}", last.query);
        assert!(last.query.contains("include_closed=true"), "{}", last.query);
        assert!(last.query.contains("order_by=due_date"), "{}", last.query);
    }

    #[test]
    fn rejections_are_short_and_plain() {
        let body = br#"{"err": "Status does not exist", "ECODE": "ITEM_085"}"#;
        assert_eq!(rejection(body), "Status does not exist (ITEM_085)");
        assert_eq!(rejection(br#"{"err": "Bad"}"#), "Bad");
        assert_eq!(rejection(b"<html>"), "no details given");
        let long = format!(r#"{{"err": "a\nb{}"}}"#, "x".repeat(300));
        let text = rejection(long.as_bytes());
        assert_eq!(text.chars().count(), 200);
        assert!(text.starts_with("abx"), "control characters dropped");
    }

    #[tokio::test]
    async fn team_tasks_and_writes() {
        let mock = MockClickUp::start().await;
        let client = mock.client();
        let t = token();
        let mut tagged = mock::task("a", "A", Some(1_000));
        tagged["tags"] = serde_json::json!([{"name": "weekly"}]);
        mock.set_tasks(
            mock::LIST_ID,
            vec![tagged, mock::task("b", "B", Some(2_000))],
        );
        let mut other = mock::task("c", "C", Some(3_000));
        other["tags"] = serde_json::json!([{"name": "weekly"}]);
        mock.set_tasks(mock::OTHER_LIST_ID, vec![other]);

        let query = TaskQuery {
            tags: vec!["weekly".into()],
            markdown: true,
            ..Default::default()
        };
        let (tasks, truncated) = client
            .team_tasks(&t, mock::TEAM_ID, &query, 5)
            .await
            .unwrap();
        assert!(!truncated);
        let ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["a", "c"], "across lists");
        let list = client.list(&t, mock::LIST_ID).await.unwrap();
        assert_eq!(list.statuses[0].status, "to do");
        assert_eq!(list.statuses[0].kind, "open");
        assert!(list.statuses[3].is_done());

        let update = TaskUpdate {
            status: Some("complete".into()),
            due_date: Some(5_000),
            start_date: None,
        };
        client.update_task(&t, "a", &update).await.unwrap();
        let err = client
            .update_task(
                &t,
                "a",
                &TaskUpdate {
                    status: Some("nope".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ClickUpError::Rejected("Status does not exist (ITEM_085)".into())
        );
        let err = client
            .update_task(&t, "missing", &TaskUpdate::default())
            .await;
        assert_eq!(err.unwrap_err(), ClickUpError::NotFound);

        let new = NewTask {
            name: "Copy".into(),
            tags: vec!["weekly".into()],
            assignees: vec![3],
            priority: Some(2),
            due_date: Some(9_000),
            due_date_time: true,
            ..Default::default()
        };
        let created = client.create_task(&t, mock::LIST_ID, &new).await.unwrap();
        assert_eq!(created.name, "Copy");
        assert_eq!(created.due_date, Some(9_000));
        assert_eq!(created.assignees[0].id, 3);
        assert_eq!(created.priority.unwrap().level(), Some(2));

        let reqs = mock.requests();
        let team = &reqs[0];
        assert_eq!(team.path, format!("/team/{}/task", mock::TEAM_ID));
        assert!(
            team.query.contains("include_markdown_description=true"),
            "{}",
            team.query
        );
        let put = reqs.iter().find(|r| r.method == "PUT").unwrap();
        assert_eq!(put.path, "/task/a");
        assert_eq!(
            put.json(),
            serde_json::json!({"status": "complete", "due_date": 5_000}),
            "unset fields are left out, so ClickUp keeps them"
        );
        let post = reqs.iter().find(|r| r.method == "POST").unwrap();
        assert_eq!(post.path, format!("/list/{}/task", mock::LIST_ID));
        assert_eq!(post.json()["due_date_time"], true);
        assert!(post.json().get("parent").is_none());
        assert!(reqs.iter().all(|r| r.auth.as_deref() == Some(mock::TOKEN)));
        assert_eq!(mock.tasks(mock::LIST_ID)[0]["status"]["status"], "complete");
    }
}
