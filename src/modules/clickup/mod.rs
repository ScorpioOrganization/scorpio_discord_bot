//! The ClickUp module: keeps the API key other ClickUp based modules can share, the selected
//! workspace, and explicit ClickUp person to Discord member mappings. It has no bot events of
//! its own; switching it off only stops other modules from using the shared key.

mod web;

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use async_trait::async_trait;
use axum::Router;
use serenity::all::GatewayIntents;
use sqlx::SqlitePool;

use super::{Module, StatItem};
use crate::bot::info::roles;
use crate::bot::members::guild_members;
use crate::clickup::client::User;
use crate::clickup::mapping::{Explicit, Resolver};
use crate::clickup::tokens::{self, TokenProblem};
use crate::db::{now, sf};
use crate::state::AppState;

pub const ID: &str = tokens::SHARED;
/// Upper bound for mapping rows saved in one request.
pub const MAX_MAPPINGS: usize = 500;
const MAX_NAME_LEN: usize = 100;
/// Upper bound for tag to role mappings per guild.
pub const MAX_TAG_ROLES: i64 = 200;

pub struct ClickUpModule;

#[async_trait]
impl Module for ClickUpModule {
    fn id(&self) -> &'static str {
        ID
    }

    fn name(&self) -> &'static str {
        "ClickUp"
    }

    fn description(&self) -> &'static str {
        "Stores the ClickUp API key other modules can share and maps ClickUp people to server \
         members."
    }

    async fn summary(&self, state: &AppState, guild_id: u64) -> Result<Vec<StatItem>> {
        let key = match tokens::info(&state.db, guild_id, ID).await? {
            None => "Not set".to_owned(),
            Some(info) => match tokens::load(&state.db, &state.vault, guild_id, ID).await? {
                Err(TokenProblem::Unreadable) => "Unreadable, set it again".to_owned(),
                _ => info.clickup_user,
            },
        };
        let cfg = load_config(&state.db, guild_id).await?;
        let explicit = explicit_mappings(&state.db, guild_id).await?;
        let mapped = explicit
            .values()
            .filter(|e| matches!(e, Explicit::Member(_)))
            .count();
        Ok(vec![
            StatItem::new("API key", key),
            StatItem::new(
                "Workspace",
                cfg.team_name.unwrap_or_else(|| "Not selected".into()),
            ),
            StatItem::new("Mapped by hand", mapped),
        ])
    }

    fn routes(&self) -> Router<AppState> {
        web::routes()
    }

    fn intents(&self) -> GatewayIntents {
        // Listing members for name matching needs this privileged intent.
        GatewayIntents::GUILD_MEMBERS
    }
}

#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct ClickUpConfig {
    pub team_id: Option<String>,
    pub team_name: Option<String>,
}

pub async fn load_config(db: &SqlitePool, guild_id: u64) -> Result<ClickUpConfig> {
    let cfg = sqlx::query_as("SELECT team_id, team_name FROM clickup_config WHERE guild_id = ?")
        .bind(sf(guild_id))
        .fetch_optional(db)
        .await?;
    Ok(cfg.unwrap_or_default())
}

/// Selects a workspace, or clears the selection with None.
pub async fn save_workspace(
    db: &SqlitePool,
    guild_id: u64,
    team: Option<(&str, &str)>,
    by: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO clickup_config (guild_id, team_id, team_name, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (guild_id) DO UPDATE SET
             team_id = excluded.team_id,
             team_name = excluded.team_name,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by",
    )
    .bind(sf(guild_id))
    .bind(team.map(|(id, _)| id.to_owned()))
    .bind(team.map(|(_, name)| name.to_owned()))
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MappingRow {
    pub clickup_user_id: i64,
    pub clickup_name: String,
    pub discord_user_id: Option<i64>,
}

impl MappingRow {
    pub fn explicit(&self) -> Explicit {
        match self.discord_user_id {
            Some(id) => Explicit::Member(id as u64),
            None => Explicit::Never,
        }
    }
}

pub async fn mapping_rows(db: &SqlitePool, guild_id: u64) -> Result<Vec<MappingRow>> {
    Ok(sqlx::query_as(
        "SELECT clickup_user_id, clickup_name, discord_user_id FROM clickup_user_map
         WHERE guild_id = ? ORDER BY clickup_name COLLATE NOCASE",
    )
    .bind(sf(guild_id))
    .fetch_all(db)
    .await?)
}

pub async fn explicit_mappings(db: &SqlitePool, guild_id: u64) -> Result<HashMap<i64, Explicit>> {
    Ok(mapping_rows(db, guild_id)
        .await?
        .into_iter()
        .map(|r| (r.clickup_user_id, r.explicit()))
        .collect())
}

/// Sets or, with None, removes (back to automatic) the mapping of one ClickUp person.
pub async fn set_mapping(
    db: &SqlitePool,
    guild_id: u64,
    clickup_user_id: i64,
    clickup_name: &str,
    target: Option<Explicit>,
    by: &str,
) -> Result<()> {
    let Some(target) = target else {
        sqlx::query("DELETE FROM clickup_user_map WHERE guild_id = ? AND clickup_user_id = ?")
            .bind(sf(guild_id))
            .bind(clickup_user_id)
            .execute(db)
            .await?;
        return Ok(());
    };
    let discord_id = match target {
        Explicit::Member(id) => Some(sf(id)),
        Explicit::Never => None,
    };
    let name: String = clickup_name.chars().take(MAX_NAME_LEN).collect();
    sqlx::query(
        "INSERT INTO clickup_user_map
             (guild_id, clickup_user_id, clickup_name, discord_user_id, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (guild_id, clickup_user_id) DO UPDATE SET
             clickup_name = excluded.clickup_name,
             discord_user_id = excluded.discord_user_id,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by",
    )
    .bind(sf(guild_id))
    .bind(clickup_user_id)
    .bind(name)
    .bind(discord_id)
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

/// A ClickUp tag as stored for role mappings: trimmed and lowercase, since ClickUp tags are
/// matched case insensitively. None when empty or too long.
pub fn normalize_tag(raw: &str) -> Option<String> {
    let tag = raw.trim().to_lowercase();
    (!tag.is_empty() && tag.chars().count() <= MAX_NAME_LEN).then_some(tag)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TagRoleRow {
    pub tag: String,
    pub role_id: i64,
}

pub async fn tag_role_rows(db: &SqlitePool, guild_id: u64) -> Result<Vec<TagRoleRow>> {
    Ok(
        sqlx::query_as(
            "SELECT tag, role_id FROM clickup_tag_roles WHERE guild_id = ? ORDER BY tag",
        )
        .bind(sf(guild_id))
        .fetch_all(db)
        .await?,
    )
}

/// Maps `tag` (normalized by the caller) to a role, or removes its mapping with None.
pub async fn set_tag_role(
    db: &SqlitePool,
    guild_id: u64,
    tag: &str,
    role_id: Option<u64>,
    by: &str,
) -> Result<()> {
    let Some(role_id) = role_id else {
        sqlx::query("DELETE FROM clickup_tag_roles WHERE guild_id = ? AND tag = ?")
            .bind(sf(guild_id))
            .bind(tag.to_owned())
            .execute(db)
            .await?;
        return Ok(());
    };
    sqlx::query(
        "INSERT INTO clickup_tag_roles (guild_id, tag, role_id, updated_at, updated_by)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT (guild_id, tag) DO UPDATE SET
             role_id = excluded.role_id,
             updated_at = excluded.updated_at,
             updated_by = excluded.updated_by",
    )
    .bind(sf(guild_id))
    .bind(tag.to_owned())
    .bind(sf(role_id))
    .bind(now())
    .bind(by.to_owned())
    .execute(db)
    .await?;
    Ok(())
}

/// Lowercase tag to role id for a guild. Roles deleted in Discord are left out while the
/// bot can see the guild.
pub async fn tag_roles(state: &AppState, guild_id: u64) -> Result<HashMap<String, u64>> {
    let rows = tag_role_rows(&state.db, guild_id).await?;
    let existing: Option<HashSet<u64>> = state.discord().and_then(|d| {
        d.cache.guild(serenity::all::GuildId::new(guild_id))?;
        Some(
            roles(&d.cache, guild_id)
                .into_iter()
                .map(|r| r.id)
                .collect(),
        )
    });
    Ok(rows
        .into_iter()
        .map(|r| (r.tag, r.role_id as u64))
        .filter(|(_, id)| existing.as_ref().is_none_or(|ids| ids.contains(id)))
        .collect())
}

/// Name matcher for a guild: explicit mappings plus automatic matching against the current
/// member list. `people` (usually the workspace members) are matched one-to-one up front;
/// anyone else, like a task assignee from another workspace, is matched on demand.
pub async fn resolver(state: &AppState, guild_id: u64, people: &[User]) -> Result<Resolver> {
    let explicit = explicit_mappings(&state.db, guild_id).await?;
    let members = guild_members(state, guild_id).await;
    Ok(Resolver::new(people, &members, explicit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clickup::mapping::Resolution;
    use crate::test_util;

    #[tokio::test]
    async fn workspace_and_mappings_round_trip() {
        let db = test_util::pool().await;
        assert!(load_config(&db, 1).await.unwrap().team_id.is_none());
        save_workspace(&db, 1, Some(("100", "Team")), "root")
            .await
            .unwrap();
        let cfg = load_config(&db, 1).await.unwrap();
        assert_eq!(cfg.team_id.as_deref(), Some("100"));
        assert_eq!(cfg.team_name.as_deref(), Some("Team"));
        save_workspace(&db, 1, None, "root").await.unwrap();
        assert!(load_config(&db, 1).await.unwrap().team_id.is_none());

        let big = u64::MAX - 1;
        set_mapping(&db, 1, 5, "Anna", Some(Explicit::Member(big)), "root")
            .await
            .unwrap();
        set_mapping(&db, 1, 6, &"x".repeat(500), Some(Explicit::Never), "root")
            .await
            .unwrap();
        set_mapping(&db, 2, 5, "Anna", Some(Explicit::Never), "root")
            .await
            .unwrap();
        let map = explicit_mappings(&db, 1).await.unwrap();
        assert_eq!(map[&5], Explicit::Member(big));
        assert_eq!(map[&6], Explicit::Never);
        assert_eq!(
            mapping_rows(&db, 1).await.unwrap()[1].clickup_name.len(),
            MAX_NAME_LEN
        );

        set_mapping(&db, 1, 5, "Anna", None, "root").await.unwrap();
        assert!(!explicit_mappings(&db, 1).await.unwrap().contains_key(&5));
        assert_eq!(explicit_mappings(&db, 2).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn tag_roles_round_trip() {
        assert_eq!(normalize_tag("  Team Sync ").as_deref(), Some("team sync"));
        assert!(normalize_tag("   ").is_none());
        assert!(normalize_tag(&"x".repeat(101)).is_none());

        let state = test_util::state().await;
        let db = &state.db;
        let big = u64::MAX - 1;
        set_tag_role(db, 1, "meeting", Some(5), "root")
            .await
            .unwrap();
        set_tag_role(db, 1, "meeting", Some(big), "root")
            .await
            .unwrap();
        set_tag_role(db, 1, "team", Some(6), "root").await.unwrap();
        set_tag_role(db, 2, "meeting", Some(7), "root")
            .await
            .unwrap();
        let map = tag_roles(&state, 1).await.unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["meeting"], big);

        set_tag_role(db, 1, "team", None, "root").await.unwrap();
        let rows = tag_role_rows(db, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(tag_role_rows(db, 2).await.unwrap()[0].role_id, 7);
    }

    #[tokio::test]
    async fn tag_roles_skip_deleted_roles() {
        let state = test_util::state().await;
        test_util::connect_fake_discord(&state);
        let gid = test_util::GUILD_ID;
        set_tag_role(&state.db, gid, "meeting", Some(test_util::ROLE_ID), "root")
            .await
            .unwrap();
        set_tag_role(&state.db, gid, "gone", Some(12345), "root")
            .await
            .unwrap();
        let map = tag_roles(&state, gid).await.unwrap();
        assert_eq!(map.len(), 1);
        assert_eq!(map["meeting"], test_util::ROLE_ID);
    }

    #[tokio::test]
    async fn resolver_combines_explicit_and_members() {
        let state = test_util::state().await;
        test_util::connect_fake_discord(&state);
        let anna = User {
            id: 3,
            username: "Anna Nowak".into(),
            email: "anna@example.com".into(),
            initials: String::new(),
        };
        let r = resolver(&state, test_util::GUILD_ID, std::slice::from_ref(&anna))
            .await
            .unwrap();
        assert_eq!(r.resolve(&anna).member(), Some(test_util::MEMBER_ID));

        set_mapping(
            &state.db,
            test_util::GUILD_ID,
            3,
            "Anna Nowak",
            Some(Explicit::Never),
            "root",
        )
        .await
        .unwrap();
        let r = resolver(&state, test_util::GUILD_ID, std::slice::from_ref(&anna))
            .await
            .unwrap();
        assert_eq!(r.resolve(&anna), Resolution::Never);
    }
}
