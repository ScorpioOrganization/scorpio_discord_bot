use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::response::{Html, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;
use serenity::all::GatewayIntents;

use super::{
    ID, MAX_MAPPINGS, MAX_TAG_ROLES, explicit_mappings, load_config, mapping_rows, normalize_tag,
    save_workspace, set_mapping, set_tag_role, tag_role_rows,
};
use crate::auth::extract::CurrentUser;
use crate::bot::info::{self, GuildSummary, RoleOption};
use crate::bot::members::{MemberInfo, guild_members};
use crate::clickup::ClickUpError;
use crate::clickup::client::{Team, User};
use crate::clickup::mapping::{Explicit, Resolution, Resolver};
use crate::clickup::tokens::{self, Credential, TokenProblem};
use crate::clickup::web::{TokenView, token_routes, token_view};
use crate::error::{AppError, AppResult};
use crate::modules::{self, counter};
use crate::state::AppState;
use crate::web::{MsgQuery, PageCtx, redirect_msg, require_guild};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/guilds/{gid}/modules/clickup", get(page))
        .route("/guilds/{gid}/modules/clickup/test", post(test_key))
        .route(
            "/guilds/{gid}/modules/clickup/workspace",
            post(choose_workspace),
        )
        .route("/guilds/{gid}/modules/clickup/mapping", post(save_mapping))
        .route(
            "/guilds/{gid}/modules/clickup/tag-roles",
            post(save_tag_roles),
        )
        .route(
            "/guilds/{gid}/modules/clickup/members/refresh",
            post(refresh_members),
        )
        .merge(token_routes(ID))
}

const TAGS_MAX_AGE: Duration = Duration::from_secs(10 * 60);
const TAGS_MAX_SPACES: usize = 20;

fn base_path(gid: u64) -> String {
    format!("/guilds/{gid}/modules/{ID}")
}

fn guild_roles(state: &AppState, gid: u64) -> Vec<RoleOption> {
    state
        .discord()
        .map(|d| info::roles(&d.cache, gid))
        .unwrap_or_default()
}

struct TeamOption {
    id: String,
    name: String,
    selected: bool,
}

struct PersonRow {
    id: i64,
    name: String,
    email: String,
    /// Current explicit value for the input: empty (automatic), `-` or a username / ID.
    input: String,
    status: String,
    /// Badge class for the status.
    badge: &'static str,
}

struct MemberOption {
    value: String,
    label: String,
}

struct TagRow {
    tag: String,
    /// The mapped role, 0 for none.
    role_id: u64,
    /// Mapped to a role that no longer exists; saving the form removes the mapping.
    deleted: bool,
}

#[derive(Template)]
#[template(path = "modules/clickup.html")]
struct ClickUpPage {
    page: PageCtx,
    guild: GuildSummary,
    enabled: bool,
    warnings: Vec<String>,
    token: TokenView,
    has_key: bool,
    teams: Vec<TeamOption>,
    team_error: Option<String>,
    workspace: Option<String>,
    people: Vec<PersonRow>,
    member_options: Vec<MemberOption>,
    member_count: usize,
    tag_rows: Vec<TagRow>,
    tag_error: Option<String>,
    roles: Vec<RoleOption>,
    api_ok: i64,
    api_err: i64,
}

/// The shared key, with the dashboard redirect to show when there is none.
async fn credential(state: &AppState, gid: u64) -> AppResult<Result<Credential, &'static str>> {
    Ok(tokens::resolve(state, gid, ID).await?.map_err(|p| match p {
        TokenProblem::Unreadable => "token_unreadable",
        _ => "token_missing",
    }))
}

async fn fetch_teams(
    state: &AppState,
    gid: u64,
    cred: &Credential,
) -> Result<Vec<Team>, ClickUpError> {
    let result = state.clickup.teams(&cred.token).await;
    tokens::track(&state.db, gid, ID, cred, result).await
}

/// Tag names (normalized) of every space in a workspace, sorted. API calls count for
/// `module_id`.
pub(crate) async fn workspace_tags(
    state: &AppState,
    gid: u64,
    module_id: &str,
    cred: &Credential,
    team_id: &str,
) -> Result<Arc<Vec<String>>, ClickUpError> {
    // Keyed by the key itself, so a replaced key never shows the old account's tags.
    let fingerprint = &crate::auth::hash_token(cred.token.expose())[..16];
    let key = format!("clickup:tags:{gid}:{team_id}:{fingerprint}");
    if let Some(hit) = state.cache.get::<Vec<String>>(&key, TAGS_MAX_AGE) {
        return Ok(hit);
    }
    let (db, client, token) = (&state.db, &state.clickup, &cred.token);
    let spaces = tokens::track(
        db,
        gid,
        module_id,
        cred,
        client.spaces(token, team_id).await,
    )
    .await?;
    let mut names = HashSet::new();
    for space in spaces.iter().take(TAGS_MAX_SPACES) {
        let result = client.space_tags(token, &space.id).await;
        let tags = tokens::track(db, gid, module_id, cred, result).await?;
        names.extend(tags.iter().filter_map(|t| normalize_tag(t.name())));
    }
    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    Ok(state.cache.insert(key, names))
}

pub(crate) fn invalidate_tags(state: &AppState, gid: u64) {
    state.cache.remove_prefix(&format!("clickup:tags:{gid}:"));
}

async fn page(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Query(msg): Query<MsgQuery>,
) -> AppResult<Html<String>> {
    let guild = require_guild(&state, gid)?;
    let enabled = modules::is_enabled(&state.db, gid, ID).await?;
    let token = token_view(&state, gid, ID).await?;
    let cfg = load_config(&state.db, gid).await?;

    let mut warnings = Vec::new();
    if !enabled {
        warnings.push(
            "This module is disabled, so other modules cannot use the shared API key. \
             Member mappings below still apply."
                .to_owned(),
        );
    }
    if !state
        .missing_intents(GatewayIntents::GUILD_MEMBERS)
        .is_empty()
    {
        warnings.push(
            "Server Members Intent is disabled in the Discord developer portal, so the member \
             list may be incomplete and automatic matching can miss people. Enable it on the \
             Bot tab of your application and restart the bot."
                .to_owned(),
        );
    }

    let mut teams = Vec::new();
    let mut team_error = None;
    let cred = credential(&state, gid).await?;
    if let Ok(cred) = &cred {
        match fetch_teams(&state, gid, cred).await {
            Ok(t) => teams = t,
            Err(e) => team_error = Some(e.to_string()),
        }
    }
    let selected = cfg
        .team_id
        .as_deref()
        .and_then(|id| teams.iter().find(|t| t.id == id));
    if cfg.team_id.is_some() && selected.is_none() && team_error.is_none() && cred.is_ok() {
        warnings.push(
            "The selected workspace is no longer accessible with this API key. Pick another one."
                .to_owned(),
        );
    }

    let workspace_people: Vec<User> = selected
        .map(|t| t.members.iter().map(|m| m.user.clone()).collect())
        .unwrap_or_default();
    let members = guild_members(&state, gid).await;
    let people = person_rows(&state, gid, workspace_people, &members).await?;

    let roles = guild_roles(&state, gid);
    let mut tag_error = None;
    let mut known_tags = Vec::new();
    if let (Ok(cred), Some(team)) = (&cred, selected) {
        match workspace_tags(&state, gid, ID, cred, &team.id).await {
            Ok(tags) => known_tags = tags.to_vec(),
            Err(e) => tag_error = Some(e.to_string()),
        }
    }
    let tag_rows = tag_rows(&state, gid, known_tags, &roles).await?;

    let page = ClickUpPage {
        page: PageCtx::new(&user, "guilds", &msg),
        guild,
        enabled,
        warnings,
        has_key: cred.is_ok(),
        teams: teams
            .iter()
            .map(|t| TeamOption {
                id: t.id.clone(),
                name: t.name.clone(),
                selected: Some(&t.id) == cfg.team_id.as_ref(),
            })
            .collect(),
        team_error,
        workspace: cfg.team_name,
        people,
        member_options: members
            .iter()
            .map(|m| MemberOption {
                value: m.username.clone(),
                label: m.display().to_owned(),
            })
            .collect(),
        member_count: members.len(),
        tag_rows,
        tag_error,
        roles,
        token,
        api_ok: counter(&state.db, Some(gid), ID, "api_ok").await?,
        api_err: counter(&state.db, Some(gid), ID, "api_err").await?,
    };
    Ok(Html(page.render()?))
}

/// Workspace tags plus every mapped tag, sorted.
async fn tag_rows(
    state: &AppState,
    gid: u64,
    known: Vec<String>,
    roles: &[RoleOption],
) -> AppResult<Vec<TagRow>> {
    let mapped: HashMap<String, u64> = tag_role_rows(&state.db, gid)
        .await?
        .into_iter()
        .map(|r| (r.tag, r.role_id as u64))
        .collect();
    let mut tags: Vec<String> = known;
    tags.extend(mapped.keys().cloned());
    tags.sort();
    tags.dedup();
    Ok(tags
        .into_iter()
        .map(|tag| {
            let role_id = mapped.get(&tag).copied().unwrap_or_default();
            let deleted = role_id != 0 && !roles.iter().any(|r| r.id == role_id);
            TagRow {
                tag,
                role_id: if deleted { 0 } else { role_id },
                deleted,
            }
        })
        .collect())
}

/// Workspace members plus anyone with a saved mapping, with their current match.
async fn person_rows(
    state: &AppState,
    gid: u64,
    mut people: Vec<User>,
    members: &[MemberInfo],
) -> AppResult<Vec<PersonRow>> {
    let rows = mapping_rows(&state.db, gid).await?;
    let known: HashSet<i64> = people.iter().map(|p| p.id).collect();
    for row in &rows {
        if !known.contains(&row.clickup_user_id) {
            people.push(User {
                id: row.clickup_user_id,
                username: row.clickup_name.clone(),
                email: String::new(),
                initials: String::new(),
            });
        }
    }
    people.sort_by_cached_key(|p| p.display().to_lowercase());

    let explicit: HashMap<i64, Explicit> = rows
        .iter()
        .map(|r| (r.clickup_user_id, r.explicit()))
        .collect();
    let resolver = Resolver::new(&people, members, explicit.clone());
    let by_id: HashMap<u64, &MemberInfo> = members.iter().map(|m| (m.id, m)).collect();
    let name = |id: u64| {
        by_id
            .get(&id)
            .map(|m| m.display().to_owned())
            .unwrap_or_else(|| format!("unknown member {id}"))
    };

    Ok(people
        .iter()
        .map(|p| {
            let (status, badge) = match resolver.resolve(p) {
                Resolution::Explicit(id) => (name(id), "badge-on"),
                Resolution::Auto { member, kind } => (
                    format!("{} (auto, {})", name(member), kind.label()),
                    "badge-on",
                ),
                Resolution::Never => ("Never mapped".to_owned(), "badge-muted"),
                Resolution::Ambiguous(ids) => {
                    let names: Vec<String> = ids.into_iter().map(name).collect();
                    (format!("Unclear: {}", names.join(", ")), "badge-warn")
                }
                Resolution::Unmatched => ("No match".to_owned(), "badge-off"),
            };
            let input = match explicit.get(&p.id) {
                None => String::new(),
                Some(Explicit::Never) => "-".to_owned(),
                Some(Explicit::Member(id)) => by_id
                    .get(id)
                    .map(|m| m.username.clone())
                    .unwrap_or_else(|| id.to_string()),
            };
            PersonRow {
                id: p.id,
                name: p.display().to_owned(),
                email: p.email.clone(),
                input,
                status,
                badge,
            }
        })
        .collect())
}

async fn test_key(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let back = base_path(gid);
    let cred = match credential(&state, gid).await? {
        Ok(c) => c,
        Err(code) => return Ok(redirect_msg(&back, code)),
    };
    let result = state.clickup.user(&cred.token).await;
    let ok = tokens::track(&state.db, gid, ID, &cred, result)
        .await
        .is_ok();
    tracing::info!(user = %user.username, guild_id = gid, ok, "ClickUp key checked");
    Ok(redirect_msg(
        &back,
        if ok { "token_ok" } else { "token_check_failed" },
    ))
}

#[derive(Deserialize)]
struct WorkspaceForm {
    team_id: String,
}

async fn choose_workspace(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Form(form): Form<WorkspaceForm>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let back = base_path(gid);
    let team_id = form.team_id.trim();
    if team_id.is_empty() {
        save_workspace(&state.db, gid, None, &user.username).await?;
        return Ok(redirect_msg(&back, "workspace_saved"));
    }
    let cred = match credential(&state, gid).await? {
        Ok(c) => c,
        Err(code) => return Ok(redirect_msg(&back, code)),
    };
    let Ok(teams) = fetch_teams(&state, gid, &cred).await else {
        return Ok(redirect_msg(&back, "clickup_unreachable"));
    };
    let Some(team) = teams.iter().find(|t| t.id == team_id) else {
        return Ok(redirect_msg(&back, "workspace_invalid"));
    };
    save_workspace(&state.db, gid, Some((&team.id, &team.name)), &user.username).await?;
    invalidate_tags(&state, gid);
    tracing::info!(user = %user.username, guild_id = gid, team = %team.id, "ClickUp workspace selected");
    Ok(redirect_msg(&back, "workspace_saved"))
}

/// Parses one mapping input. Ok(None) means automatic matching.
fn parse_target(raw: &str, members: &[MemberInfo]) -> Option<Option<Explicit>> {
    let value = raw.trim();
    if value.is_empty() {
        return Some(None);
    }
    if value == "-" {
        return Some(Some(Explicit::Never));
    }
    let id_text = value
        .strip_prefix("<@")
        .and_then(|v| v.strip_suffix('>'))
        .map(|v| v.trim_start_matches('!'))
        .unwrap_or(value);
    if let Ok(id) = id_text.parse::<u64>() {
        // IDs are accepted as typed, the member list may be incomplete.
        return (id > 0).then_some(Some(Explicit::Member(id)));
    }

    let wanted = value.strip_prefix('@').unwrap_or(value).to_lowercase();
    if let Some(m) = members.iter().find(|m| m.username.to_lowercase() == wanted) {
        return Some(Some(Explicit::Member(m.id)));
    }
    let by_name: Vec<&MemberInfo> = members
        .iter()
        .filter(|m| {
            [m.nick.as_deref(), m.global_name.as_deref()]
                .into_iter()
                .flatten()
                .any(|n| n.to_lowercase() == wanted)
        })
        .collect();
    match by_name.as_slice() {
        [m] => Some(Some(Explicit::Member(m.id))),
        _ => None,
    }
}

/// Saves every row of the mapping table at once. If any row names an unknown member,
/// nothing is saved.
async fn save_mapping(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let back = base_path(gid);
    let names: HashMap<&str, &str> = fields
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("name_")?, v.as_str())))
        .collect();
    let inputs: Vec<(i64, &str)> = fields
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("map_")?, v.as_str())))
        .map(|(id, v)| {
            id.parse()
                .map(|id| (id, v))
                .map_err(|_| AppError::BadRequest)
        })
        .collect::<Result<_, _>>()?;
    if inputs.len() > MAX_MAPPINGS {
        return Err(AppError::BadRequest);
    }

    let members = guild_members(&state, gid).await;
    let mut changes = Vec::new();
    let current = explicit_mappings(&state.db, gid).await?;
    for (id, raw) in inputs {
        let Some(target) = parse_target(raw, &members) else {
            return Ok(redirect_msg(&back, "mapping_unknown"));
        };
        if current.get(&id).copied() != target {
            let name = names
                .get(id.to_string().as_str())
                .copied()
                .unwrap_or_default();
            let name = if name.trim().is_empty() {
                id.to_string()
            } else {
                name.trim().to_owned()
            };
            changes.push((id, name, target));
        }
    }
    for (id, name, target) in &changes {
        set_mapping(&state.db, gid, *id, name, *target, &user.username).await?;
    }
    tracing::info!(user = %user.username, guild_id = gid, changed = changes.len(), "ClickUp mappings saved");
    Ok(redirect_msg(&back, "mapping_saved"))
}

async fn refresh_members(
    State(state): State<AppState>,
    CurrentUser(_user): CurrentUser,
    Path(gid): Path<u64>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    state.members.invalidate(gid);
    invalidate_tags(&state, gid);
    Ok(redirect_msg(&base_path(gid), "members_refreshed"))
}

/// Saves the tag to role table plus the "other tag" row. An empty role removes a mapping.
/// If any row is invalid, nothing is saved.
async fn save_tag_roles(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(gid): Path<u64>,
    Form(fields): Form<Vec<(String, String)>>,
) -> AppResult<Response> {
    require_guild(&state, gid)?;
    let back = base_path(gid);
    let get = |key: &str| {
        fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default()
    };
    let mut inputs: Vec<(&str, &str)> = fields
        .iter()
        .filter_map(|(k, v)| {
            let i = k.strip_prefix("tag_")?;
            Some((v.as_str(), get(&format!("role_{i}"))))
        })
        .collect();
    if !get("new_tag").trim().is_empty() {
        inputs.push((get("new_tag"), get("new_role")));
    }
    if inputs.len() > MAX_MAPPINGS {
        return Err(AppError::BadRequest);
    }

    let known: HashSet<u64> = guild_roles(&state, gid).iter().map(|r| r.id).collect();
    let mut wanted: HashMap<String, Option<u64>> = HashMap::new();
    for (tag_raw, role_raw) in inputs {
        let Some(tag) = normalize_tag(tag_raw) else {
            return Ok(redirect_msg(&back, "tag_invalid"));
        };
        let role = match role_raw.trim() {
            "" => None,
            raw => match raw.parse::<u64>() {
                Ok(id) if known.contains(&id) => Some(id),
                _ => return Ok(redirect_msg(&back, "role_unknown")),
            },
        };
        wanted.insert(tag, role);
    }

    let current: HashMap<String, u64> = tag_role_rows(&state.db, gid)
        .await?
        .into_iter()
        .map(|r| (r.tag, r.role_id as u64))
        .collect();
    let changes: Vec<(String, Option<u64>)> = wanted
        .into_iter()
        .filter(|(tag, role)| current.get(tag).copied() != *role)
        .collect();
    let after = current.len() as i64
        + changes
            .iter()
            .map(|(tag, role)| match (current.contains_key(tag), role) {
                (false, Some(_)) => 1,
                (true, None) => -1,
                _ => 0,
            })
            .sum::<i64>();
    if after > MAX_TAG_ROLES {
        return Ok(redirect_msg(&back, "too_many_tag_roles"));
    }
    for (tag, role) in &changes {
        set_tag_role(&state.db, gid, tag, *role, &user.username).await?;
    }
    tracing::info!(user = %user.username, guild_id = gid, changed = changes.len(), "ClickUp tag roles saved");
    Ok(redirect_msg(&back, "tag_roles_saved"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u64, username: &str, global: Option<&str>, nick: Option<&str>) -> MemberInfo {
        MemberInfo {
            id,
            username: username.into(),
            global_name: global.map(Into::into),
            nick: nick.map(Into::into),
        }
    }

    #[test]
    fn mapping_inputs() {
        let members = [
            member(10, "anna.nowak", Some("Anna Nowak"), None),
            member(20, "jan", Some("Jan"), Some("Boss")),
            member(30, "jan2", Some("Jan"), None),
            member(40, "kim1", Some("Kim"), None),
            member(50, "kim2", None, Some("kim")),
        ];
        assert_eq!(parse_target("  ", &members), Some(None));
        assert_eq!(parse_target("-", &members), Some(Some(Explicit::Never)));
        assert_eq!(
            parse_target("123", &members),
            Some(Some(Explicit::Member(123)))
        );
        assert_eq!(
            parse_target("<@!123>", &members),
            Some(Some(Explicit::Member(123)))
        );
        assert_eq!(
            parse_target("<@123>", &members),
            Some(Some(Explicit::Member(123)))
        );
        assert_eq!(
            parse_target("@Anna.Nowak", &members),
            Some(Some(Explicit::Member(10)))
        );
        assert_eq!(
            parse_target("anna nowak", &members),
            Some(Some(Explicit::Member(10)))
        );
        assert_eq!(
            parse_target("boss", &members),
            Some(Some(Explicit::Member(20)))
        );
        // Username wins over display names, display names must be unique.
        assert_eq!(
            parse_target("jan", &members),
            Some(Some(Explicit::Member(20)))
        );
        assert_eq!(
            parse_target("Jan ", &members),
            Some(Some(Explicit::Member(20)))
        );
        assert_eq!(
            parse_target("Kim", &members),
            None,
            "two members are called Kim"
        );
        assert_eq!(parse_target("nobody", &members), None);
        assert_eq!(parse_target("0", &members), None);
    }
}
