CREATE TABLE IF NOT EXISTS sources (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    kind            TEXT NOT NULL,
    network         TEXT,
    path            TEXT NOT NULL,
    first_seen_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    last_scanned_at INTEGER NOT NULL,
    UNIQUE (kind, path)
);

CREATE TABLE IF NOT EXISTS source_files (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id        INTEGER NOT NULL,
    format           TEXT NOT NULL,
    path             TEXT NOT NULL,
    filename         TEXT NOT NULL,
    network          TEXT NOT NULL,
    epoch            INTEGER,
    file_hash        TEXT,
    size_bytes       INTEGER NOT NULL,
    modified_at      INTEGER,
    sha256           TEXT NOT NULL,
    checksum_status  TEXT NOT NULL,
    expected_sha256  TEXT,
    last_seen_at     INTEGER NOT NULL,
    FOREIGN KEY (source_id) REFERENCES sources(id) ON DELETE CASCADE,
    UNIQUE (source_id, path)
);

CREATE TABLE IF NOT EXISTS source_ranges (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source_file_id  INTEGER NOT NULL,
    from_block      INTEGER NOT NULL,
    to_block        INTEGER NOT NULL,
    completeness    TEXT NOT NULL,
    FOREIGN KEY (source_file_id) REFERENCES source_files(id) ON DELETE CASCADE,
    UNIQUE (source_file_id, from_block, to_block)
);

CREATE INDEX IF NOT EXISTS idx_sources_kind_path
    ON sources(kind, path);

CREATE INDEX IF NOT EXISTS idx_source_files_source
    ON source_files(source_id);

CREATE INDEX IF NOT EXISTS idx_source_ranges_coverage
    ON source_ranges(from_block, to_block);
