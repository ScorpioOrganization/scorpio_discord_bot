-- Dashboard accounts
CREATE TABLE users (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    username        TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    password_hash   TEXT    NOT NULL,
    is_root         INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    last_login_at   INTEGER
);

-- Only the SHA-256 of the session token is stored
CREATE TABLE sessions (
    token_hash  TEXT    PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    csrf_token  TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);
CREATE INDEX idx_sessions_user ON sessions(user_id);

-- Generic per guild module on/off switch
CREATE TABLE guild_modules (
    guild_id    INTEGER NOT NULL,
    module_id   TEXT    NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (guild_id, module_id)
);

-- Greeting module
CREATE TABLE greeting_config (
    guild_id    INTEGER PRIMARY KEY,
    channel_id  INTEGER,
    template    TEXT    NOT NULL,
    use_embed   INTEGER NOT NULL DEFAULT 0,
    embed_color INTEGER NOT NULL DEFAULT 8359167,
    greet_bots  INTEGER NOT NULL DEFAULT 0,
    updated_at  INTEGER NOT NULL,
    updated_by  TEXT
);

CREATE TABLE greeting_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id    INTEGER NOT NULL,
    user_id     INTEGER NOT NULL,
    user_name   TEXT    NOT NULL,
    success     INTEGER NOT NULL,
    error       TEXT,
    is_test     INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_greeting_log_guild ON greeting_log(guild_id, created_at);

-- Generic all-time counters for module statistics
CREATE TABLE module_stats (
    guild_id    INTEGER NOT NULL,
    module_id   TEXT    NOT NULL,
    key         TEXT    NOT NULL,
    value       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (guild_id, module_id, key)
);
