-- Projects replace Discord servers as the top level container. Every server that has any
-- data becomes a project, and Discord becomes a module holding a bot token and a server.
-- `guild_id` columns become `project_id`. Real Discord ids are far above the project ids
-- created here, so rewriting key columns in place never collides with a row not yet
-- rewritten.

CREATE TABLE projects (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT    NOT NULL,
    -- Discord server this project was created from by this migration. Used once to import
    -- DISCORD_TOKEN and to redirect old /guilds/ links.
    legacy_guild_id INTEGER UNIQUE,
    -- The name is a placeholder until the bot sees the server's real name
    auto_name       INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    updated_by      TEXT
);

INSERT INTO projects (name, legacy_guild_id, auto_name, created_at, updated_at)
SELECT 'Discord server ' || g, g, 1,
       CAST(strftime('%s', 'now') AS INTEGER), CAST(strftime('%s', 'now') AS INTEGER)
FROM (
    SELECT guild_id AS g FROM guild_modules
    UNION SELECT guild_id FROM greeting_config
    UNION SELECT guild_id FROM greeting_log
    UNION SELECT guild_id FROM module_stats
    UNION SELECT guild_id FROM clickup_tokens
    UNION SELECT guild_id FROM clickup_token_source
    UNION SELECT guild_id FROM clickup_config
    UNION SELECT guild_id FROM clickup_user_map
    UNION SELECT guild_id FROM clickup_tag_roles
    UNION SELECT guild_id FROM calendar_config
    UNION SELECT guild_id FROM calendar_rules
    UNION SELECT guild_id FROM calendar_log
    UNION SELECT guild_id FROM recurring_config
    UNION SELECT guild_id FROM recurring_rules
    UNION SELECT guild_id FROM recurring_done
    UNION SELECT guild_id FROM recurring_log
)
ORDER BY g;

-- Generic module switch, now per project
ALTER TABLE guild_modules RENAME TO project_modules;
ALTER TABLE project_modules RENAME COLUMN guild_id TO project_id;
UPDATE project_modules SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = project_modules.project_id);

ALTER TABLE module_stats RENAME COLUMN guild_id TO project_id;
UPDATE module_stats SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = module_stats.project_id);

-- Greeting
ALTER TABLE greeting_config RENAME COLUMN guild_id TO project_id;
UPDATE greeting_config SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = greeting_config.project_id);

ALTER TABLE greeting_log RENAME COLUMN guild_id TO project_id;
UPDATE greeting_log SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = greeting_log.project_id);
DROP INDEX idx_greeting_log_guild;
CREATE INDEX idx_greeting_log_project ON greeting_log(project_id, created_at);

-- ClickUp
ALTER TABLE clickup_config RENAME COLUMN guild_id TO project_id;
UPDATE clickup_config SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = clickup_config.project_id);

ALTER TABLE clickup_user_map RENAME COLUMN guild_id TO project_id;
UPDATE clickup_user_map SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = clickup_user_map.project_id);

ALTER TABLE clickup_tag_roles RENAME COLUMN guild_id TO project_id;
UPDATE clickup_tag_roles SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = clickup_tag_roles.project_id);

-- Calendar. Renamed in place: rebuilding calendar_rules would cascade into calendar_sent.
ALTER TABLE calendar_config RENAME COLUMN guild_id TO project_id;
UPDATE calendar_config SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = calendar_config.project_id);

ALTER TABLE calendar_rules RENAME COLUMN guild_id TO project_id;
UPDATE calendar_rules SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = calendar_rules.project_id);
DROP INDEX idx_calendar_rules_guild;
CREATE INDEX idx_calendar_rules_project ON calendar_rules(project_id);

ALTER TABLE calendar_log RENAME COLUMN guild_id TO project_id;
UPDATE calendar_log SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = calendar_log.project_id);
DROP INDEX idx_calendar_log_guild;
CREATE INDEX idx_calendar_log_project ON calendar_log(project_id, created_at);

-- Recurring tasks
ALTER TABLE recurring_config RENAME COLUMN guild_id TO project_id;
UPDATE recurring_config SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = recurring_config.project_id);

ALTER TABLE recurring_rules RENAME COLUMN guild_id TO project_id;
UPDATE recurring_rules SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = recurring_rules.project_id);

ALTER TABLE recurring_done RENAME COLUMN guild_id TO project_id;
UPDATE recurring_done SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = recurring_done.project_id);

ALTER TABLE recurring_log RENAME COLUMN guild_id TO project_id;
UPDATE recurring_log SET project_id =
    (SELECT p.id FROM projects p WHERE p.legacy_guild_id = recurring_log.project_id);
DROP INDEX idx_recurring_log_guild;
CREATE INDEX idx_recurring_log_project ON recurring_log(project_id, created_at);

-- API keys of every service, sealed with secrets::Vault (never stored in plain text).
-- `owner` is the module holding the shared key of the service (`clickup`, `discord`) or
-- the id of a module that uses its own key.
CREATE TABLE api_keys (
    project_id      INTEGER NOT NULL,
    service         TEXT    NOT NULL,
    owner           TEXT    NOT NULL,
    sealed          BLOB    NOT NULL,
    hint            TEXT    NOT NULL,
    -- Account the key belongs to, e.g. the ClickUp user or the Discord bot name
    account         TEXT    NOT NULL,
    last_ok_at      INTEGER,
    last_error      TEXT,
    last_error_at   INTEGER,
    updated_at      INTEGER NOT NULL,
    updated_by      TEXT,
    -- Set for keys sealed while they belonged to a Discord server. They open with that
    -- server's context until the startup upgrade seals them for the project.
    legacy_guild_id INTEGER,
    PRIMARY KEY (project_id, service, owner)
);

INSERT INTO api_keys (project_id, service, owner, sealed, hint, account, last_ok_at, last_error,
                      last_error_at, updated_at, updated_by, legacy_guild_id)
SELECT p.id, 'clickup', t.owner, t.sealed, t.hint, t.clickup_user, t.last_ok_at, t.last_error,
       t.last_error_at, t.updated_at, t.updated_by, t.guild_id
FROM clickup_tokens t JOIN projects p ON p.legacy_guild_id = t.guild_id;
DROP TABLE clickup_tokens;

-- Whether a module uses the shared key of a service (default when no row exists) or its own.
CREATE TABLE api_key_source (
    project_id  INTEGER NOT NULL,
    service     TEXT    NOT NULL,
    owner       TEXT    NOT NULL,
    use_shared  INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (project_id, service, owner)
);

INSERT INTO api_key_source (project_id, service, owner, use_shared)
SELECT p.id, 'clickup', s.owner, s.use_shared
FROM clickup_token_source s JOIN projects p ON p.legacy_guild_id = s.guild_id;
DROP TABLE clickup_token_source;

-- Discord server a Discord key works in, per key (same owner as in api_keys)
CREATE TABLE discord_servers (
    project_id  INTEGER NOT NULL,
    owner       TEXT    NOT NULL,
    guild_id    INTEGER NOT NULL,
    guild_name  TEXT    NOT NULL DEFAULT '',
    updated_at  INTEGER NOT NULL,
    updated_by  TEXT,
    PRIMARY KEY (project_id, owner)
);

-- One-off facts about the installation, e.g. that DISCORD_TOKEN was imported
CREATE TABLE app_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
