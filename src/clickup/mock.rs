//! In-process fake of the ClickUp API for tests. Serves a small fixed workspace, filters
//! and pages tasks like ClickUp does, and records every request it receives.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::client::{ClickUpClient, TASKS_PER_PAGE};

pub const TOKEN: &str = "pk_1_TESTTOKENVALUE";
pub const TEAM_ID: &str = "100";
pub const SPACE_ID: &str = "200";
pub const FOLDER_ID: &str = "300";
pub const LIST_ID: &str = "400";
pub const OTHER_LIST_ID: &str = "401";

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub query: String,
    pub auth: Option<String>,
}

#[derive(Default)]
struct Inner {
    requests: Vec<Recorded>,
    tasks: HashMap<String, Vec<Value>>,
    members: Vec<Value>,
    fail: Option<u16>,
}

#[derive(Clone)]
pub struct MockClickUp {
    pub url: String,
    inner: Arc<Mutex<Inner>>,
}

impl MockClickUp {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            members: vec![
                person(1, "Test Owner", "owner@example.com"),
                person(2, "Igor Zawórski", "igor.zaworski@example.com"),
                person(3, "Anna Nowak", "anna@example.com"),
            ],
            ..Default::default()
        }));
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url: format!("http://{addr}"),
            inner,
        }
    }

    pub fn client(&self) -> ClickUpClient {
        ClickUpClient::with_base_url(&self.url)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.lock().requests.clone()
    }

    pub fn set_tasks(&self, list_id: &str, tasks: Vec<Value>) {
        self.lock().tasks.insert(list_id.to_owned(), tasks);
    }

    /// Makes every request fail with this status until reset with None.
    pub fn fail_with(&self, status: Option<u16>) {
        self.lock().fail = status;
    }
}

/// A workspace member as returned by `GET /team`.
pub fn person(id: i64, username: &str, email: &str) -> Value {
    json!({"user": {"id": id, "username": username, "email": email, "initials": "XX"}})
}

/// A task as returned by `GET /list/{id}/task`. `due` is unix milliseconds.
pub fn task(id: &str, name: &str, due: Option<i64>) -> Value {
    json!({
        "id": id,
        "name": name,
        "text_content": format!("Notes for {name}"),
        "status": {"status": "to do", "color": "#d3d3d3", "type": "open"},
        "due_date": due.map(|d| d.to_string()),
        "start_date": null,
        "assignees": [],
        "tags": [],
        "url": format!("https://app.clickup.com/t/{id}"),
        "list": {"id": LIST_ID, "name": "Calendar"},
        "priority": null
    })
}

async fn handle(
    State(inner): State<Arc<Mutex<Inner>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let path = uri.path().to_owned();
    let query = uri.query().unwrap_or_default().to_owned();

    let mut inner = inner.lock().unwrap();
    inner.requests.push(Recorded {
        method: method.to_string(),
        path: path.clone(),
        query: query.clone(),
        auth: auth.clone(),
    });

    if method != Method::GET {
        return (StatusCode::METHOD_NOT_ALLOWED, "read only").into_response();
    }
    if let Some(status) = inner.fail {
        let status = StatusCode::from_u16(status).unwrap();
        return (status, axum::Json(json!({"err": "mock failure"}))).into_response();
    }
    if auth.as_deref() != Some(TOKEN) {
        let body = json!({"err": "Token invalid", "ECODE": "OAUTH_025"});
        return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    }

    let params: Vec<(String, String)> = serde_urlencoded::from_str(&query).unwrap_or_default();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let body = match segments.as_slice() {
        ["user"] => {
            json!({"user": {"id": 1, "username": "Test Owner", "email": "owner@example.com"}})
        }
        ["team"] => json!({"teams": [{
            "id": TEAM_ID, "name": "Scorpio Team", "color": "#000", "avatar": null,
            "members": inner.members
        }]}),
        ["team", TEAM_ID, "space"] => json!({"spaces": [{"id": SPACE_ID, "name": "Events"}]}),
        ["space", SPACE_ID, "folder"] => json!({"folders": [{
            "id": FOLDER_ID, "name": "Team",
            "lists": [{"id": LIST_ID, "name": "Calendar"}]
        }]}),
        ["space", SPACE_ID, "list"] => json!({"lists": [{"id": OTHER_LIST_ID, "name": "Misc"}]}),
        ["list", LIST_ID] => json!({
            "id": LIST_ID, "name": "Calendar",
            "folder": {"id": FOLDER_ID, "name": "Team"},
            "space": {"id": SPACE_ID, "name": "Events"}
        }),
        ["list", OTHER_LIST_ID] => json!({
            "id": OTHER_LIST_ID, "name": "Misc", "folder": null,
            "space": {"id": SPACE_ID, "name": "Events"}
        }),
        ["list", id, "task"] if [LIST_ID, OTHER_LIST_ID].contains(id) => {
            let tasks = inner.tasks.get(*id).cloned().unwrap_or_default();
            task_page(tasks, &params)
        }
        _ => {
            let body = json!({"err": "Not found", "ECODE": "ITEM_015"});
            return (StatusCode::NOT_FOUND, axum::Json(body)).into_response();
        }
    };
    axum::Json(body).into_response()
}

fn task_page(mut tasks: Vec<Value>, params: &[(String, String)]) -> Value {
    let get = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let num = |key: &str| get(key).and_then(|v| v.parse::<i64>().ok());
    let due = |t: &Value| t["due_date"].as_str().and_then(|d| d.parse::<i64>().ok());

    let include_closed = get("include_closed") == Some("true");
    let tags: Vec<&str> = params
        .iter()
        .filter(|(k, _)| k == "tags[]")
        .map(|(_, v)| v.as_str())
        .collect();
    let (gt, lt) = (num("due_date_gt"), num("due_date_lt"));

    tasks.retain(|t| {
        let closed = t["status"]["type"].as_str() == Some("closed");
        let has_tag = tags.is_empty()
            || t["tags"].as_array().is_some_and(|ts| {
                ts.iter()
                    .any(|tag| tags.contains(&tag["name"].as_str().unwrap_or_default()))
            });
        let in_range = match (due(t), gt.is_some() || lt.is_some()) {
            (_, false) => true,
            (None, true) => false,
            (Some(d), true) => gt.is_none_or(|g| d > g) && lt.is_none_or(|l| d < l),
        };
        (include_closed || !closed) && has_tag && in_range
    });
    tasks.sort_by_key(|t| due(t).unwrap_or(i64::MAX));

    let page = num("page").unwrap_or(0).max(0) as usize;
    let start = (page * TASKS_PER_PAGE).min(tasks.len());
    let end = (start + TASKS_PER_PAGE).min(tasks.len());
    json!({"tasks": tasks[start..end].to_vec(), "last_page": end >= tasks.len()})
}
