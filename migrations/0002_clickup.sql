-- ClickUp API keys, sealed with secrets::Vault (never stored in plain text). `owner` is
-- 'clickup' for the key shared through the ClickUp module, or the id of a module that
-- uses its own key.
CREATE TABLE clickup_tokens (
    guild_id        INTEGER NOT NULL,
    owner           TEXT    NOT NULL,
    sealed          BLOB    NOT NULL,
    hint            TEXT    NOT NULL,
    clickup_user    TEXT    NOT NULL,
    last_ok_at      INTEGER,
    last_error      TEXT,
    last_error_at   INTEGER,
    updated_at      INTEGER NOT NULL,
    updated_by      TEXT,
    PRIMARY KEY (guild_id, owner)
);

-- Whether a module uses the shared key (default when no row exists) or its own.
CREATE TABLE clickup_token_source (
    guild_id    INTEGER NOT NULL,
    owner       TEXT    NOT NULL,
    use_shared  INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (guild_id, owner)
);

-- ClickUp module: selected workspace
CREATE TABLE clickup_config (
    guild_id    INTEGER PRIMARY KEY,
    team_id     TEXT,
    team_name   TEXT,
    updated_at  INTEGER NOT NULL,
    updated_by  TEXT
);

-- Explicit ClickUp person -> Discord member mappings. People without a row are matched
-- automatically; a NULL discord_user_id means "never map this person".
CREATE TABLE clickup_user_map (
    guild_id        INTEGER NOT NULL,
    clickup_user_id INTEGER NOT NULL,
    clickup_name    TEXT    NOT NULL,
    discord_user_id INTEGER,
    updated_at      INTEGER NOT NULL,
    updated_by      TEXT,
    PRIMARY KEY (guild_id, clickup_user_id)
);
