-- Recurring tasks module: moves tagged ClickUp tasks to their next occurrence, standing in
-- for ClickUp's own recurrence when it does not run.
CREATE TABLE recurring_config (
    guild_id      INTEGER PRIMARY KEY,
    -- ClickUp workspace searched for tagged tasks
    team_id       TEXT,
    team_name     TEXT,
    timezone      TEXT    NOT NULL DEFAULT 'UTC',
    check_minutes INTEGER NOT NULL DEFAULT 15,
    -- Log what would change without changing anything in ClickUp
    dry_run       INTEGER NOT NULL DEFAULT 1,
    last_run_at   INTEGER,
    -- Set while a run is in progress, so a scheduled run and "Run now" never overlap
    running_since INTEGER,
    updated_at    INTEGER NOT NULL,
    updated_by    TEXT
);

-- One rule per tag: tasks with `tag` recur every `every` `unit`s. `trigger_on` is `close`
-- (when the task is done or closed) or `schedule` (when the due date passed). `mode` is
-- `update` (move the task itself) or `create` (add a copy at the next date).
CREATE TABLE recurring_rules (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id      INTEGER NOT NULL,
    name          TEXT    NOT NULL,
    enabled       INTEGER NOT NULL DEFAULT 1,
    tag           TEXT    NOT NULL,
    every         INTEGER NOT NULL DEFAULT 1 CHECK (every BETWEEN 1 AND 365),
    unit          TEXT    NOT NULL DEFAULT 'week' CHECK (unit IN ('day', 'week', 'month', 'year')),
    trigger_on    TEXT    NOT NULL DEFAULT 'close' CHECK (trigger_on IN ('close', 'schedule')),
    mode          TEXT    NOT NULL DEFAULT 'update' CHECK (mode IN ('update', 'create')),
    -- Status after recurring; empty means the first open status of the task's list
    reopen_status TEXT    NOT NULL DEFAULT '',
    shift_start   INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    updated_by    TEXT,
    UNIQUE (guild_id, tag)
);

-- Occurrences already handled, so a task is never moved or copied twice for the same due
-- date. `due_ms` is the due date before recurring, in ClickUp's milliseconds.
CREATE TABLE recurring_done (
    guild_id    INTEGER NOT NULL,
    task_id     TEXT    NOT NULL,
    due_ms      INTEGER NOT NULL,
    rule_id     INTEGER,
    new_task_id TEXT,
    done_at     INTEGER NOT NULL,
    PRIMARY KEY (guild_id, task_id, due_ms)
);

CREATE TABLE recurring_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    guild_id   INTEGER NOT NULL,
    rule_id    INTEGER,
    rule_name  TEXT    NOT NULL,
    task_id    TEXT    NOT NULL,
    task_name  TEXT    NOT NULL,
    task_url   TEXT    NOT NULL DEFAULT '',
    -- Unix seconds
    old_due    INTEGER NOT NULL,
    new_due    INTEGER NOT NULL,
    mode       TEXT    NOT NULL,
    dry_run    INTEGER NOT NULL DEFAULT 0,
    success    INTEGER NOT NULL,
    error      TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_recurring_log_guild ON recurring_log(guild_id, created_at);
