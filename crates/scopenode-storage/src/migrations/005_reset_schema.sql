-- Migration 005: Drop devp2p-only tables and clean up the events schema.
--
-- Tables dropped: bloom_candidates, sync_cursor, source_ranges, source_files,
-- sources, headers. All were written only by the devp2p/daemon pipeline.
--
-- events.reorged column dropped (requires drop+recreate of the index that
-- references it). contracts.impl_address dropped (Sourcify proxy feature removed).

DROP TABLE IF EXISTS bloom_candidates;
DROP TABLE IF EXISTS sync_cursor;
DROP TABLE IF EXISTS source_ranges;
DROP TABLE IF EXISTS source_files;
DROP TABLE IF EXISTS sources;
DROP TABLE IF EXISTS headers;

-- Drop the partial index that references reorged BEFORE dropping the column.
-- SQLite rejects DROP COLUMN on a column referenced by any index.
DROP INDEX IF EXISTS idx_events_lookup;

-- Drop the reorged column (requires SQLite 3.35+, same as migration 002).
ALTER TABLE events DROP COLUMN reorged;

-- Recreate the lookup index without the reorged predicate.
CREATE INDEX IF NOT EXISTS idx_events_lookup
    ON events(contract, event_name, block_number);

-- Drop impl_address from contracts (Sourcify proxy feature removed).
ALTER TABLE contracts DROP COLUMN impl_address;
