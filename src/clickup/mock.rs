//! In-process fake of the ClickUp API for tests. Serves a small fixed workspace, filters
//! and pages tasks like ClickUp does, and records every request it receives. The only
//! writes it accepts are `PUT /task/{id}` and `POST /list/{id}/task`, which change the
//! stored tasks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
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
    pub body: String,
}

impl Recorded {
    /// The request body as JSON, Null when there is none.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

#[derive(Default)]
struct Inner {
    requests: Vec<Recorded>,
    tasks: HashMap<String, Vec<Value>>,
    members: Vec<Value>,
    fail: Option<u16>,
    fail_writes: Option<u16>,
    created: u32,
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

    /// The stored tasks of a list, including changes made through the API.
    pub fn tasks(&self, list_id: &str) -> Vec<Value> {
        self.lock().tasks.get(list_id).cloned().unwrap_or_default()
    }

    /// Requests other than GET.
    pub fn writes(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method != "GET")
            .collect()
    }

    /// Makes every request fail with this status until reset with None.
    pub fn fail_with(&self, status: Option<u16>) {
        self.lock().fail = status;
    }

    /// Like [`fail_with`](Self::fail_with), for writes only: reads keep working.
    pub fn fail_writes_with(&self, status: Option<u16>) {
        self.lock().fail_writes = status;
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

/// Statuses of both lists, in board order.
pub fn statuses() -> Value {
    json!([
        {"status": "to do", "color": "#d3d3d3", "type": "open", "orderindex": 0},
        {"status": "in progress", "color": "#4194f6", "type": "custom", "orderindex": 1},
        {"status": "review", "color": "#a875ff", "type": "done", "orderindex": 2},
        {"status": "complete", "color": "#6bc950", "type": "closed", "orderindex": 3}
    ])
}

fn status_named(name: &str) -> Option<Value> {
    statuses()
        .as_array()?
        .iter()
        .find(|s| s["status"].as_str() == Some(name))
        .cloned()
}

async fn handle(
    State(inner): State<Arc<Mutex<Inner>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
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
        body: String::from_utf8_lossy(&body).into_owned(),
    });

    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let write = match (&method, segments.as_slice()) {
        (&Method::GET, _) => false,
        (&Method::PUT, ["task", _]) => true,
        (&Method::POST, ["list", id, "task"]) if [LIST_ID, OTHER_LIST_ID].contains(id) => true,
        _ => return (StatusCode::METHOD_NOT_ALLOWED, "read only").into_response(),
    };
    if let Some(status) = inner.fail.or(inner.fail_writes.filter(|_| write)) {
        let status = StatusCode::from_u16(status).unwrap();
        return (status, axum::Json(json!({"err": "mock failure"}))).into_response();
    }
    if auth.as_deref() != Some(TOKEN) {
        let body = json!({"err": "Token invalid", "ECODE": "OAUTH_025"});
        return (StatusCode::UNAUTHORIZED, axum::Json(body)).into_response();
    }

    if write {
        let Ok(body) = serde_json::from_slice::<Value>(&body) else {
            return rejected("Invalid JSON", "JSON_001");
        };
        return match segments.as_slice() {
            ["task", id] => update_task(&mut inner, id, &body),
            ["list", id, "task"] => create_task(&mut inner, id, &body),
            _ => unreachable!("checked above"),
        };
    }

    let params: Vec<(String, String)> = serde_urlencoded::from_str(&query).unwrap_or_default();
    let body = match segments.as_slice() {
        ["user"] => {
            json!({"user": {"id": 1, "username": "Test Owner", "email": "owner@example.com"}})
        }
        ["team"] => json!({"teams": [{
            "id": TEAM_ID, "name": "Scorpio Team", "color": "#000", "avatar": null,
            "members": inner.members
        }]}),
        ["team", TEAM_ID, "space"] => json!({"spaces": [{"id": SPACE_ID, "name": "Events"}]}),
        ["team", TEAM_ID, "task"] => {
            let mut lists: Vec<_> = inner.tasks.iter().collect();
            lists.sort_by_key(|(id, _)| id.as_str());
            let tasks = lists.into_iter().flat_map(|(_, t)| t.clone()).collect();
            task_page(tasks, &params)
        }
        ["space", SPACE_ID, "folder"] => json!({"folders": [{
            "id": FOLDER_ID, "name": "Team",
            "lists": [{"id": LIST_ID, "name": "Calendar"}]
        }]}),
        ["space", SPACE_ID, "tag"] => json!({"tags": [
            {"name": "meeting", "tag_fg": "#fff", "tag_bg": "#000"},
            {"name": "Team Sync", "tag_fg": "#fff", "tag_bg": "#000"}
        ]}),
        ["space", SPACE_ID, "list"] => json!({"lists": [{"id": OTHER_LIST_ID, "name": "Misc"}]}),
        ["list", LIST_ID] => json!({
            "id": LIST_ID, "name": "Calendar",
            "folder": {"id": FOLDER_ID, "name": "Team"},
            "space": {"id": SPACE_ID, "name": "Events"},
            "statuses": statuses()
        }),
        ["list", OTHER_LIST_ID] => json!({
            "id": OTHER_LIST_ID, "name": "Misc", "folder": null,
            "space": {"id": SPACE_ID, "name": "Events"},
            "statuses": statuses()
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

fn rejected(err: &str, ecode: &str) -> Response {
    let body = json!({"err": err, "ECODE": ecode});
    (StatusCode::BAD_REQUEST, axum::Json(body)).into_response()
}

fn millis(v: &Value) -> Value {
    v.as_i64().map_or(Value::Null, |ms| json!(ms.to_string()))
}

/// `PUT /task/{id}`: status and dates, like ClickUp.
fn update_task(inner: &mut Inner, id: &str, body: &Value) -> Response {
    let status = match body["status"].as_str() {
        None => None,
        Some(name) => match status_named(name) {
            Some(s) => Some(s),
            None => return rejected("Status does not exist", "ITEM_085"),
        },
    };
    let Some(task) = inner
        .tasks
        .values_mut()
        .flatten()
        .find(|t| t["id"].as_str() == Some(id))
    else {
        let body = json!({"err": "Task not found", "ECODE": "ITEM_013"});
        return (StatusCode::NOT_FOUND, axum::Json(body)).into_response();
    };
    if let Some(s) = status {
        task["status"] = s;
    }
    for field in ["due_date", "start_date"] {
        if body.get(field).is_some() {
            task[field] = millis(&body[field]);
        }
    }
    axum::Json(task.clone()).into_response()
}

/// `POST /list/{id}/task`, storing the new task in the list.
fn create_task(inner: &mut Inner, list_id: &str, body: &Value) -> Response {
    let Some(name) = body["name"].as_str().filter(|n| !n.trim().is_empty()) else {
        return rejected("Task name invalid", "INPUT_003");
    };
    let status = match body["status"].as_str() {
        None => statuses()[0].clone(),
        Some(s) => match status_named(s) {
            Some(s) => s,
            None => return rejected("Status does not exist", "ITEM_085"),
        },
    };
    inner.created += 1;
    let id = format!("new{}", inner.created);
    let ids: Vec<i64> = body["assignees"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    let assignees: Vec<Value> = inner
        .members
        .iter()
        .map(|m| m["user"].clone())
        .filter(|u| u["id"].as_i64().is_some_and(|i| ids.contains(&i)))
        .collect();
    let tags: Vec<Value> = body["tags"]
        .as_array()
        .map(|a| a.iter().map(|t| json!({"name": t})).collect())
        .unwrap_or_default();
    let list_name = if list_id == LIST_ID {
        "Calendar"
    } else {
        "Misc"
    };
    let task = json!({
        "id": id,
        "name": name,
        "text_content": body["markdown_content"].as_str().unwrap_or_default(),
        "status": status,
        "due_date": millis(&body["due_date"]),
        "start_date": millis(&body["start_date"]),
        "assignees": assignees,
        "tags": tags,
        "url": format!("https://app.clickup.com/t/{id}"),
        "list": {"id": list_id, "name": list_name},
        "priority": body["priority"].as_i64().and_then(|p| {
            let name = ["urgent", "high", "normal", "low"].get(usize::try_from(p - 1).ok()?)?;
            Some(json!({"id": p.to_string(), "priority": name}))
        }),
        "parent": body["parent"].clone()
    });
    inner
        .tasks
        .entry(list_id.to_owned())
        .or_default()
        .push(task.clone());
    axum::Json(task).into_response()
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
