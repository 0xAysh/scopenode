-- Add covering index for the contract+fetched query pattern used by
-- get_unfetched_candidates. Without this, SQLite scans the whole table.
CREATE INDEX IF NOT EXISTS idx_bloom_contract ON bloom_candidates(contract, fetched);

-- Recreate the events table with a semantically correct UNIQUE constraint.
-- The old UNIQUE (tx_hash, log_index) relied on a synthetic tx_hash derived from
-- block_hash; two blocks with B256::ZERO hash (common in tests) could collide.
-- The new UNIQUE (block_number, tx_index, log_index) is the canonical identity
-- of a log: its position in the block, independent of any synthetic hash.
-- SQLite does not support ALTER TABLE ... DROP CONSTRAINT, so we recreate.
CREATE TABLE events_new (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    contract        TEXT NOT NULL,
    event_name      TEXT NOT NULL,
    topic0          TEXT NOT NULL,
    block_number    INTEGER NOT NULL,
    block_hash      TEXT NOT NULL,
    tx_hash         TEXT NOT NULL,
    tx_index        INTEGER NOT NULL,
    log_index       INTEGER NOT NULL,
    raw_topics      TEXT NOT NULL,
    raw_data        TEXT NOT NULL,
    decoded         TEXT NOT NULL,
    source          TEXT NOT NULL,
    reorged         INTEGER NOT NULL DEFAULT 0,
    UNIQUE (block_number, tx_index, log_index)
);

INSERT INTO events_new
    SELECT id, contract, event_name, topic0, block_number, block_hash,
           tx_hash, tx_index, log_index, raw_topics, raw_data, decoded,
           source, reorged
    FROM events;

DROP TABLE events;
ALTER TABLE events_new RENAME TO events;

-- Recreate the lookup index that was on the original table.
CREATE INDEX IF NOT EXISTS idx_events_lookup
    ON events(contract, event_name, block_number) WHERE reorged = 0;
