CREATE TABLE IF NOT EXISTS covered_ranges (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    contract     TEXT NOT NULL,
    from_block   INTEGER NOT NULL,
    to_block     INTEGER NOT NULL,
    source       TEXT NOT NULL DEFAULT 'era1',
    recorded_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (contract, from_block, to_block, source)
);

CREATE INDEX IF NOT EXISTS idx_covered_ranges_contract_range
    ON covered_ranges(contract, from_block, to_block);
