-- Calendar module: which ClickUp list is followed and how it is read
CREATE TABLE calendar_config (
    guild_id         INTEGER PRIMARY KEY,
    list_id          TEXT,
    list_name        TEXT,
    time_field       TEXT    NOT NULL DEFAULT 'due' CHECK (time_field IN ('due', 'start')),
    timezone         TEXT    NOT NULL DEFAULT 'UTC',
    tag_filter       TEXT    NOT NULL DEFAULT '',
    include_subtasks INTEGER NOT NULL DEFAULT 0,
    include_closed   INTEGER NOT NULL DEFAULT 0,
    updated_at       INTEGER NOT NULL,
    updated_by       TEXT
);

-- Notification rules. A digest posts on the weekdays in `days` (bit 0 = Monday) at
-- `time_of_day` (minutes after local midnight); a reminder posts `lead_minutes` before
-- each event.
CREATE TABLE calendar_rules (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id          INTEGER NOT NULL,
    name              TEXT    NOT NULL,
    enabled           INTEGER NOT NULL DEFAULT 1,
    kind              TEXT    NOT NULL CHECK (kind IN ('digest', 'reminder')),
    channel_id        INTEGER,
    days              INTEGER NOT NULL DEFAULT 127,
    time_of_day       INTEGER NOT NULL DEFAULT 540,
    lookahead_hours   INTEGER NOT NULL DEFAULT 24,
    lead_minutes      INTEGER NOT NULL DEFAULT 15,
    layout            TEXT    NOT NULL DEFAULT 'combined' CHECK (layout IN ('combined', 'per_event')),
    max_events        INTEGER NOT NULL DEFAULT 15,
    post_when_empty   INTEGER NOT NULL DEFAULT 0,
    header_template   TEXT    NOT NULL,
    item_template     TEXT    NOT NULL,
    empty_template    TEXT    NOT NULL,
    mention_assignees INTEGER NOT NULL DEFAULT 0,
    use_embed         INTEGER NOT NULL DEFAULT 0,
    embed_color       INTEGER NOT NULL DEFAULT 8359167,
    description_max   INTEGER NOT NULL DEFAULT 200,
    reactions         TEXT    NOT NULL DEFAULT '',
    -- Digest: the scheduled time handled last, posted or skipped
    last_run_at       INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    updated_by        TEXT
);
CREATE INDEX idx_calendar_rules_guild ON calendar_rules(guild_id);

-- Reminders already sent, so each event time is announced once per rule
CREATE TABLE calendar_sent (
    rule_id     INTEGER NOT NULL REFERENCES calendar_rules(id) ON DELETE CASCADE,
    task_id     TEXT    NOT NULL,
    event_at    INTEGER NOT NULL,
    sent_at     INTEGER NOT NULL,
    PRIMARY KEY (rule_id, task_id, event_at)
);

CREATE TABLE calendar_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id    INTEGER NOT NULL,
    rule_id     INTEGER,
    rule_name   TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    events      INTEGER NOT NULL DEFAULT 0,
    success     INTEGER NOT NULL,
    error       TEXT,
    is_test     INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);
CREATE INDEX idx_calendar_log_guild ON calendar_log(guild_id, created_at);
