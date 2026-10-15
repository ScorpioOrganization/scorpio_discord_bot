//! Read only ClickUp API v2 client.
//!
//! Only GET requests exist here on purpose: modules built on this client can read ClickUp
//! but have no way to change anything in it.

use std::sync::Arc;
use std::time::Duration;

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
        let mut auth =
            HeaderValue::from_str(token.expose()).map_err(|_| ClickUpError::Unauthorized)?;
        auth.set_sensitive(true);

        let res = self
            .http
            .get(format!("{}{path}", self.base))
            .header(AUTHORIZATION, auth)
            .query(query)
            .send()
            .await
            .map_err(|e| ClickUpError::Network(e.without_url().to_string()))?;

        match res.status().as_u16() {
            200..=299 => {}
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

    pub async fn list(&self, token: &SecretString, list_id: &str) -> Result<List, ClickUpError> {
        self.get(token, &format!("/list/{}", seg(list_id)), &[])
            .await
    }

    pub async fn tasks_page(
        &self,
        token: &SecretString,
        list_id: &str,
        query: &TaskQuery,
        page: u32,
    ) -> Result<TaskPage, ClickUpError> {
        let mut params = vec![
            ("page", page.to_string()),
            ("order_by", "due_date".into()),
            ("subtasks", query.subtasks.to_string()),
            ("include_closed", query.include_closed.to_string()),
        ];
        if let Some(ms) = query.due_after_ms {
            params.push(("due_date_gt", ms.to_string()));
        }
        if let Some(ms) = query.due_before_ms {
            params.push(("due_date_lt", ms.to_string()));
        }
        for tag in &query.tags {
            params.push(("tags[]", tag.clone()));
        }
        self.get(token, &format!("/list/{}/task", seg(list_id)), &params)
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
        let mut tasks = Vec::new();
        for page in 0..max_pages {
            let p = self.tasks_page(token, list_id, query, page).await?;
            let full = p.tasks.len() >= TASKS_PER_PAGE;
            tasks.extend(p.tasks);
            if p.last_page.unwrap_or(!full) {
                return Ok((tasks, false));
            }
        }
        Ok((tasks, true))
    }
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
}

#[derive(Debug, Clone, Deserialize)]
pub struct Status {
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Priority {
    #[serde(default, deserialize_with = "null_default")]
    pub priority: String,
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
}
