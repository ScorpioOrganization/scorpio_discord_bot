-- ClickUp tag -> Discord role mappings, shared by ClickUp based modules. `tag` is stored
-- lowercase; ClickUp tags are matched case insensitively.
CREATE TABLE clickup_tag_roles (
    guild_id    INTEGER NOT NULL,
    tag         TEXT    NOT NULL,
    role_id     INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    updated_by  TEXT,
    PRIMARY KEY (guild_id, tag)
);

-- Digest rules keep their own time zone, starting from the server wide setting
ALTER TABLE calendar_rules ADD COLUMN timezone TEXT NOT NULL DEFAULT 'UTC';
UPDATE calendar_rules SET timezone = COALESCE(
    (SELECT c.timezone FROM calendar_config c WHERE c.guild_id = calendar_rules.guild_id),
    'UTC'
);

-- Whether roles mapped from task tags may be pinged
ALTER TABLE calendar_rules ADD COLUMN mention_roles INTEGER NOT NULL DEFAULT 0;
