CREATE TABLE IF NOT EXISTS headers (
    number          INTEGER PRIMARY KEY,
    hash            TEXT NOT NULL UNIQUE,
    parent_hash     TEXT NOT NULL,
    timestamp       INTEGER NOT NULL,
    receipts_root   TEXT NOT NULL,
    logs_bloom      BLOB NOT NULL,
    gas_used        INTEGER NOT NULL,
    base_fee        INTEGER
);

CREATE TABLE IF NOT EXISTS bloom_candidates (
    block_number     INTEGER NOT NULL,
    block_hash       TEXT NOT NULL,
    contract         TEXT NOT NULL,
    fetched          INTEGER NOT NULL DEFAULT 0,
    pending_retry    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (block_number, contract)
);

CREATE TABLE IF NOT EXISTS events (
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
    UNIQUE (tx_hash, log_index)
);

CREATE INDEX IF NOT EXISTS idx_events_lookup
    ON events(contract, event_name, block_number) WHERE reorged = 0;

CREATE TABLE IF NOT EXISTS sync_cursor (
    contract         TEXT PRIMARY KEY,
    from_block       INTEGER NOT NULL,
    to_block         INTEGER,
    headers_done_to  INTEGER,
    bloom_done_to    INTEGER,
    receipts_done_to INTEGER
);

CREATE TABLE IF NOT EXISTS contracts (
    address          TEXT PRIMARY KEY,
    name             TEXT,
    abi_json         TEXT,
    impl_address     TEXT
);
