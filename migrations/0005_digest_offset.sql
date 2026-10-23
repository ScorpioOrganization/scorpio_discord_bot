-- Digest rules can start their event window a number of minutes after the scheduled time
-- (posting at 20:00 with 240 minutes and 24 hours lists the events of the next day)
ALTER TABLE calendar_rules ADD COLUMN offset_minutes INTEGER NOT NULL DEFAULT 0;
