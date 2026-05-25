-- Add block timestamp to events.
-- Re-sync required to populate non-zero values (existing rows default to 0).
ALTER TABLE events ADD COLUMN timestamp INTEGER NOT NULL DEFAULT 0;
