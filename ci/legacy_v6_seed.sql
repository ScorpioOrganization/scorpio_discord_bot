-- A database as version 6 (before projects) left it: two Discord servers with modules
-- configured and a third the bot left, with only old log rows. Used by the migration tests
-- (src/upgrade.rs) and by the CI upgrade job. API keys are added by the tests, they need
-- the vault.
--   1187654321098765432   "Scorpio HQ", most modules
--   -5446744073709551615  a server id above i64::MAX, stored as its bit pattern
--   1187654321098765999   left server, logs only
-- Log and marker times are relative to now, so the new version's first log cleanup keeps
-- them.

CREATE TEMP TABLE seed_clock AS SELECT CAST(strftime('%s', 'now') AS INTEGER) AS now;

INSERT INTO guild_modules (guild_id, module_id, enabled) VALUES
    (1187654321098765432, 'greeting', 1),
    (1187654321098765432, 'clickup', 1),
    (1187654321098765432, 'calendar', 1),
    (1187654321098765432, 'recurring', 0),
    (-5446744073709551615, 'greeting', 0),
    (-5446744073709551615, 'clickup', 1),
    (-5446744073709551615, 'recurring', 1);

INSERT INTO greeting_config
    (guild_id, channel_id, template, use_embed, embed_color, greet_bots, updated_at, updated_by)
VALUES
    (1187654321098765432, 1187654321098765433, 'Welcome {user} to {server}!', 1, 8359167, 0,
     1760000000, 'root'),
    (-5446744073709551615, NULL, 'Hi {user}', 0, 8359167, 1, 1760000000, 'alice');

INSERT INTO greeting_log (guild_id, user_id, user_name, success, error, is_test, created_at) VALUES
    (1187654321098765432, 1, 'Anna', 1, NULL, 0, (SELECT now - 86400 FROM seed_clock)),
    (1187654321098765432, 2, 'Jan', 0, 'Missing Permissions', 0,
     (SELECT now - 7200 FROM seed_clock)),
    (1187654321098765999, 3, 'Old', 1, NULL, 0, (SELECT now - 20 * 86400 FROM seed_clock));

INSERT INTO module_stats (guild_id, module_id, key, value) VALUES
    (1187654321098765432, 'greeting', 'sent', 41),
    (1187654321098765432, 'calendar', 'digests', 7),
    (-5446744073709551615, 'recurring', 'moved', 3);

INSERT INTO clickup_token_source (guild_id, owner, use_shared) VALUES
    (1187654321098765432, 'calendar', 1),
    (-5446744073709551615, 'recurring', 0);

INSERT INTO clickup_config (guild_id, team_id, team_name, updated_at, updated_by) VALUES
    (1187654321098765432, '100', 'Team', 1760000000, 'root');

INSERT INTO clickup_user_map
    (guild_id, clickup_user_id, clickup_name, discord_user_id, updated_at, updated_by)
VALUES
    (1187654321098765432, 3, 'Anna Nowak', 1187654321098765500, 1760000000, 'root'),
    (1187654321098765432, 4, 'Robot', NULL, 1760000000, 'root');

INSERT INTO clickup_tag_roles (guild_id, tag, role_id, updated_at, updated_by) VALUES
    (1187654321098765432, 'meeting', 1187654321098765600, 1760000000, 'root');

INSERT INTO calendar_config
    (guild_id, list_id, list_name, time_field, timezone, tag_filter, include_subtasks,
     include_closed, updated_at, updated_by)
VALUES
    (1187654321098765432, '400', 'Calendar', 'start', 'Europe/Warsaw', 'meeting', 0, 0,
     1760000000, 'root');

INSERT INTO calendar_rules
    (id, guild_id, name, enabled, kind, channel_id, header_template, item_template,
     empty_template, timezone, offset_minutes, created_at, updated_at)
VALUES
    (1, 1187654321098765432, 'Daily digest', 1, 'digest', 1187654321098765433, '**Upcoming**',
     '{link}', 'Nothing', 'Europe/Warsaw', 240, 1760000000, 1760000000),
    (2, 1187654321098765432, 'Reminder', 1, 'reminder', 1187654321098765433, '', '{link}', '',
     'UTC', 0, 1760000000, 1760000000);

INSERT INTO calendar_sent (rule_id, task_id, event_at, sent_at) VALUES
    (2, 'task1', (SELECT now + 3600 FROM seed_clock), (SELECT now - 600 FROM seed_clock)),
    (2, 'task2', (SELECT now + 7200 FROM seed_clock), (SELECT now - 300 FROM seed_clock));

INSERT INTO calendar_log (guild_id, rule_id, rule_name, kind, events, success, created_at) VALUES
    (1187654321098765432, 1, 'Daily digest', 'digest', 3, 1, (SELECT now - 3600 FROM seed_clock)),
    (1187654321098765999, NULL, 'Gone', 'digest', 0, 1, (SELECT now - 30 * 86400 FROM seed_clock));

INSERT INTO recurring_config
    (guild_id, team_id, team_name, timezone, check_minutes, dry_run, last_run_at, updated_at,
     updated_by)
VALUES
    (-5446744073709551615, '100', 'Team', 'UTC', 15, 0, 1760000000, 1760000000, 'root');

INSERT INTO recurring_rules (id, guild_id, name, tag, every, unit, created_at, updated_at) VALUES
    (1, -5446744073709551615, 'Weekly', 'weekly', 1, 'week', 1760000000, 1760000000);

INSERT INTO recurring_done (guild_id, task_id, due_ms, rule_id, new_task_id, done_at) VALUES
    (-5446744073709551615, 'task9', (SELECT (now - 86400) * 1000 FROM seed_clock), 1, NULL,
     (SELECT now - 600 FROM seed_clock));

INSERT INTO recurring_log
    (guild_id, rule_id, rule_name, task_id, task_name, old_due, new_due, mode, success,
     created_at)
VALUES
    (-5446744073709551615, 1, 'Weekly', 'task9', 'Review', 1760000000, 1760604800, 'update', 1,
     (SELECT now - 600 FROM seed_clock));

DROP TABLE seed_clock;
