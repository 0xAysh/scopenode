---
title: "refactor: Scope refinement — remove webhooks, cut non-core planned features, add export"
type: refactor
status: completed
date: 2026-04-21
origin: docs/brainstorms/2026-04-21-scope-refinement-requirements.md
---

# refactor: Scope refinement — remove webhooks, cut non-core planned features, add export

## Overview

scopenode's core promise is: no RPC provider, no full node, free blockchain data, low
resources. Several features have drifted in or been planned that don't serve that promise
— webhooks (already wired), Python bindings, Parquet export, Rhai scripting, SQL views in
config, and a `scopenode init` wizard. This plan removes the one feature already in code
(webhooks), deletes the Phase 5 doc and trims Phase 4, and fills a real gap: a
`scopenode export` command that dumps all matched events to stdout as CSV or JSON with no
row cap.

## Problem Frame

`scopenode query` is limited to 20 rows by default and has no way to dump a full event set
without REST pagination or direct SQLite access — this is a gap in the "get the data" core
use case. The webhook system (just wired in the previous commit) pushes events to external
URLs, which is a different product concern from local data access. Planning documents
describe Python bindings, Parquet, Rhai scripting, SQL views in config, and a
`scopenode init` wizard — none serve the primary user and none are yet implemented in code.
(see origin: `docs/brainstorms/2026-04-21-scope-refinement-requirements.md`)

## Requirements Trace

- R1. Delete `webhook.rs` and all webhook dispatch call sites.
- R2. Remove `webhook`, `webhook_secret`, `webhook_events` from `ContractConfig`; update all struct literal sites.
- R3. Remove `hmac`, `sha2`, `wiremock` from Cargo — they are exclusively used by webhooks.
- R4–R8. Remove Python bindings, Parquet, SQL views, `scopenode init` wizard, and webhooks from `docs/phases/PHASE_04_developer.md`; delete `docs/phases/PHASE_05_computed_state.md`.
- R9. Add `scopenode export --format csv/json` that streams all matching events with no row cap.
- R10–R11. Scrub `VISION.md` and `README.md` of all references to the cut features.

## Scope Boundaries

- `eth_subscribe` WebSocket stays — it is part of the JSON-RPC :8545 drop-in promise (see origin doc).
- SSE streaming stays — already implemented, minimal overhead.
- `--topic0` filter on `scopenode query` is not added here; export already includes it.
- `query_events` (the narrower DB method used by `scopenode query`) is not migrated in this plan; the two parallel query paths remain and consolidation is deferred.
- No changes to `scopenode-rpc` crate — it has no webhook references.

### Deferred to Separate Tasks

- Consolidating `query_events` and `query_events_for_filter` into one method: separate cleanup task.

## Context & Research

### Relevant Code and Patterns

- `crates/scopenode-core/src/webhook.rs` — 577 lines, all webhook logic; this file is deleted.
- `crates/scopenode-core/src/lib.rs` line 28 — `pub mod webhook;` to remove.
- `crates/scopenode-core/src/config.rs` lines 118–131 — three `ContractConfig` fields to remove; lines 158–165 — validation loop referencing `webhook_events`; lines 412–449 — two config tests that parse webhook fields; line 26 — `use std::collections::HashMap` (remove if no longer used).
- `crates/scopenode-core/src/pipeline.rs` lines 612–614 — `webhook: None, webhook_secret: None, webhook_events: HashMap::new()` in `test_config` struct literal.
- `crates/scopenode-core/src/live.rs` lines 302–304 — same three fields in `live_config` struct literal.
- `crates/scopenode/src/commands/sync.rs` lines 30–31 — webhook import; lines 97, 150 — `tokio::spawn(WebhookDispatcher::new(...).run())` calls. Broadcast channel itself (`broadcast::channel`) stays — still used for REST/SSE.
- `crates/scopenode-storage/src/db.rs` `query_events_for_filter` — full filter surface (contract, event_name, topic0, from_block, to_block, limit, offset), uses `fetch_all` internally; streaming requires switching to `fetch`.
- `crates/scopenode/src/commands/query.rs` — direct pattern for `export.rs`: `Db::open`, filter args, JSON re-parse of `decoded` field from stored string to avoid double-escaping.
- `crates/scopenode/src/cli.rs` — `Command` enum; `Export` goes alongside `Query`.
- `crates/scopenode/src/commands/mod.rs` — add `pub mod export;`.

### Institutional Learnings

No `docs/solutions/` knowledge base exists yet. No prior art to carry forward.

### External References

None needed — all patterns exist in the codebase.

## Key Technical Decisions

- **Webhook removal is a single atomic change**: ContractConfig struct change + call-site cleanup must land together or the build breaks. The three locations (config.rs, pipeline.rs test helper, live.rs test helper) are touched in the same unit.
- **True streaming for export (Option B)**: Add a new `stream_events_for_filter` method to `db.rs` using sqlx `fetch` (row-by-row cursor) rather than pagination over `fetch_all`. This avoids loading any result set into memory — important for contracts with millions of events.
- **Hand-written CSV, no new dep**: `StoredEvent` has a fixed schema; only `decoded` (JSON object) and `raw_topics` (JSON array) need quoting. Adding a `csv` crate for six fields would be unnecessary. JSON output follows the same `decoded`-re-parse pattern as `query.rs` but writes a streaming JSON array (`[` → rows with commas → `]`).
- **No new Cargo dependencies for export**: `serde_json` already present; `tokio-stream` already available transitively (used by REST/SSE). `futures::StreamExt` via the existing `tokio-stream` dep suffices for driving the sqlx stream.

## Implementation Units

- [x] **Unit 1: Remove webhook system from Rust code and Cargo**

  **Goal:** Delete all webhook code and its exclusive dependencies so the project compiles cleanly without webhooks.

  **Requirements:** R1, R2, R3

  **Dependencies:** None

  **Files:**
  - Delete: `crates/scopenode-core/src/webhook.rs`
  - Modify: `crates/scopenode-core/src/lib.rs` (remove `pub mod webhook;`)
  - Modify: `crates/scopenode-core/src/config.rs` (remove three fields, validation loop, two tests, `use std::collections::HashMap`)
  - Modify: `crates/scopenode-core/src/pipeline.rs` (remove three field lines from `test_config`)
  - Modify: `crates/scopenode-core/src/live.rs` (remove three field lines from `live_config`)
  - Modify: `crates/scopenode/src/commands/sync.rs` (remove import + two `tokio::spawn` calls)
  - Modify: `Cargo.toml` (workspace root — remove `hmac`, `sha2` from `[workspace.dependencies]`)
  - Modify: `crates/scopenode-core/Cargo.toml` (remove `hmac`, `sha2` from deps; `wiremock` from dev-deps)

  **Approach:**
  - Delete `webhook.rs` first to make the compiler surface every remaining reference.
  - In `config.rs`, after removing the three fields, check that `url::Url` import is still needed (yes — `NodeConfig.consensus_rpc: Vec<Url>`); check that `std::collections::HashMap` import is still needed (no — remove it).
  - In `sync.rs`, the broadcast channel (`broadcast::channel::<StoredEvent>(1024)`) and `broadcast_tx` stay because `start_rest_server` and `LiveSyncer` still consume them.
  - Run `cargo check` after this unit to confirm zero compilation errors before continuing.

  **Patterns to follow:**
  - `crates/scopenode-core/src/pipeline.rs` lines 601–617 (`test_config` struct shape after removal)

  **Test scenarios:**
  - Happy path: `cargo build --workspace` succeeds with no webhook-related errors or warnings
  - Happy path: `cargo test --workspace` passes (webhook tests were inside the deleted file, so the test count decreases but no failures)
  - Edge case: confirm `HMAC`, `sha2`, and `wiremock` no longer appear in `cargo tree` output

  **Verification:**
  - `cargo build --workspace` exits 0 with no warnings (`#![deny(warnings)]` is on)
  - `cargo test --workspace` exits 0
  - `grep -r "webhook\|WebhookDispatcher\|hmac\|sha2\|wiremock" crates/ --include="*.rs"` returns nothing

---

- [x] **Unit 2: Add streaming query method to storage layer**

  **Goal:** Give the export command a row-by-row DB cursor so it can write events to stdout without loading the entire result set into memory.

  **Requirements:** R9 (prerequisite)

  **Dependencies:** None — `scopenode-storage` has no webhook references and can be developed and tested independently.

  **Files:**
  - Modify: `crates/scopenode-storage/src/db.rs`
  - Modify: `crates/scopenode-storage/Cargo.toml` (add `futures-core = "0.3"` to express the `Stream` return type; `futures-core` is a direct dep of `sqlx` but must be declared explicitly to use it in a public return type)
  - Test: `crates/scopenode-storage/src/db.rs` (inline tests)

  **Approach:**
  - Add `stream_events_for_filter` with the same filter signature as `query_events_for_filter` (contract, event_name, topic0, from_block, to_block) but without `limit`/`offset` parameters — export streams the full result.
  - Build the same dynamic `WHERE` clause as `query_events_for_filter`; end with `ORDER BY block_number ASC, log_index ASC`.
  - Use sqlx's `.fetch(&self.pool)` instead of `.fetch_all(...)` — returns a `futures::Stream<Item = Result<StoredEvent, sqlx::Error>>`.
  - Return type: `impl futures::Stream<Item = Result<StoredEvent, DbError>> + '_` (lifetime tied to the pool borrow). The caller drives the stream with `StreamExt::next()`.
  - `tokio-stream` / `futures::StreamExt` are already available transitively — no new dep.

  **Patterns to follow:**
  - `query_events_for_filter` in `crates/scopenode-storage/src/db.rs` — duplicate the WHERE-clause builder, replace the terminal `.fetch_all()` with `.fetch()`

  **Test scenarios:**
  - Happy path: stream over a fixture DB with 3 events returns all 3 rows in `block_number ASC, log_index ASC` order
  - Happy path: stream with `contract` filter returns only matching rows
  - Happy path: stream with `topic0` filter returns only matching rows
  - Edge case: stream over empty result set completes immediately with zero rows (no panic)
  - Edge case: `reorged = 1` events are excluded from the stream

  **Verification:**
  - `cargo test -p scopenode-storage` exits 0
  - `crates/scopenode-storage/Cargo.toml` lists `futures-core` as a direct dep

---

- [x] **Unit 3: Add `scopenode export` CLI command**

  **Goal:** A new `scopenode export` subcommand that streams all matching events to stdout as CSV or JSON, with no row cap.

  **Requirements:** R9

  **Dependencies:** Unit 2

  **Files:**
  - Modify: `crates/scopenode/src/cli.rs` (add `Export` variant to `Command`)
  - Modify: `crates/scopenode/src/commands/mod.rs` (add `pub mod export;`)
  - Create: `crates/scopenode/src/commands/export.rs`
  - Modify: `crates/scopenode/src/main.rs` (dispatch `Command::Export`)

  **Approach:**
  - `Export` variant mirrors `Query` args plus `--topic0` and `--format` (default `csv`). No `--limit` or `--output` (export always writes everything to stdout).
  - Dispatch in `main.rs` follows the `resolve_data_dir_no_config` + `Db::open` + `run(args, db)` pattern of the existing `Query` command.
  - In `export.rs`, call `db.stream_events_for_filter(...)`, drive with `while let Some(row) = stream.next().await`.
  - **JSON format**: write `[` once; for each row, write a separator (empty string for the first row, `,` for every subsequent row) followed by a JSON object. Re-parse `decoded` from its stored string to `serde_json::Value` to avoid double-escaping — same pattern as `query.rs`. After the final row write `]`. This first-element guard avoids the invalid trailing-comma produced by a naive per-row `{...},` loop. Flush stdout after the closing bracket. All 12 fields are included in each JSON object: `contract`, `event_name`, `topic0`, `block_number`, `block_hash`, `tx_hash`, `tx_index`, `log_index`, `raw_topics` (re-parsed from stored JSON string), `raw_data`, `decoded` (re-parsed), `source`. Note: `query.rs` only emits 6 fields — export intentionally emits all 12 for full fidelity.
  - **CSV format**: write a header line (`contract,event_name,topic0,block_number,block_hash,tx_hash,tx_index,log_index,raw_topics,raw_data,decoded,source`), then for each row write comma-separated values. Quote any field that contains a comma, double-quote, or newline by wrapping in `"` and escaping internal `"` as `""`. In practice, `decoded` (JSON object) and `raw_topics` (JSON array) always need quoting; others are safe but quoting them unconditionally is simpler and correct.
  - Write directly to `std::io::stdout()` (synchronous, fine for a CLI command). No `indicatif` progress bar — this is intended for piping.
  - If `--quiet` is set on the parent CLI, still write data output (quiet only suppresses progress/log output).

  **Patterns to follow:**
  - `crates/scopenode/src/commands/query.rs` — filter argument handling, `Db::open`, JSON `decoded` re-parse
  - `crates/scopenode/src/cli.rs` `Query` variant — arg shape

  **Test scenarios:**
  - Happy path: `export --format csv` with 2 events writes correct header + 2 data rows to stdout
  - Happy path: `export --format json` with 2 events writes a valid JSON array `[{...},{...}]`
  - Happy path: `export --contract 0x... --event Swap --from-block 100 --to-block 200` filters correctly (delegates to `stream_events_for_filter`, verified by row count)
  - Happy path: `export --topic0 0xabc...` returns correct rows matching that topic0
  - Edge case: `export` with no matching events writes `[]` for JSON and only the header for CSV (no crash)
  - Edge case: `decoded` field contains double-quotes (JSON string values with `"`) — CSV output correctly escapes them as `""`
  - Edge case: `--format parquet` or unknown format returns a clear error message, not a panic

  **Verification:**
  - `cargo test -p scopenode` exits 0
  - `scopenode export --format csv | head -1` shows the expected CSV header
  - `scopenode export --format json | python3 -m json.tool` parses without error
  - `scopenode export --format csv | wc -l` equals `(total event count) + 1` (header line)

---

- [x] **Unit 4: Prune phase docs, VISION.md, README.md, and config.example.toml**

  **Goal:** Remove all references to cut features from every documentation file so the docs accurately reflect the planned and actual scope.

  **Requirements:** R4, R5, R6, R7, R8, R10, R11

  **Dependencies:** None (doc-only, no Rust compilation dependency)

  **Files:**
  - Delete: `docs/phases/PHASE_05_computed_state.md`
  - Modify: `docs/phases/PHASE_04_developer.md` (remove webhooks, Python bindings, Parquet, SQL views, `scopenode init` sections and their definition-of-done checkboxes and dependency entries)
  - Modify: `VISION.md` (remove webhook TOML example + description, `scopenode init` CLI table entry and description, Parquet mentions)
  - Modify: `README.md` (remove `CSV/JSON/Parquet export` checkbox, webhook mentions, `scopenode init` mentions)
  - Modify: `config.example.toml` (remove `webhook`, `webhook_secret`, `webhook_events` documented fields)

  **Approach:**
  - In `PHASE_04_developer.md`, keep: REST API, SSE, `eth_subscribe`, `scopenode export` (CSV/JSON, updated from Parquet), `--topic0` filter. Remove: webhooks section, Python bindings section, SQL views section, `scopenode init` section, and all corresponding DoD checkboxes and new-dependencies entries for `dialoguer`, `parquet`, `pyo3`.
  - In `VISION.md`, do a targeted search for `webhook`, `init`, `Parquet` and remove or replace the relevant passages. The core pipeline description stays.
  - In `README.md`, update the feature list to reflect CSV/JSON export (not Parquet).
  - In `config.example.toml`, remove all `webhook`-related field examples and comments.

  **Test scenarios:**
  - Test expectation: none — documentation changes have no automated tests.

  **Verification:**
  - `grep -r "webhook\|parquet\|Parquet\|pyo3\|PyO3\|maturin\|dialoguer\|rhai\|Rhai" docs/ VISION.md README.md config.example.toml` returns nothing
  - `docs/phases/PHASE_05_computed_state.md` no longer exists
  - `PHASE_04_developer.md` retains: REST API, SSE, `eth_subscribe`, `scopenode export` (CSV/JSON), `--topic0` filter

## System-Wide Impact

- **Interaction graph:** `WebhookDispatcher` is spawned inside `sync.rs` as a detached tokio task. Removing it has no effect on any other task — the broadcast channel it subscribed to remains valid.
- **Error propagation:** No webhook-specific error paths remain. The broadcast channel `send` error (lagged receiver) was silently ignored in the webhook subscriber; removing the subscriber means one fewer lagged-receiver warning in logs.
- **State lifecycle risks:** No state owned by the webhook system (no persistent queue, no DB writes). Removal is clean.
- **API surface parity:** `ContractConfig` is the public config type. Removing three fields is a breaking change to any existing `config.toml` that contains webhook fields — those files will fail to parse. The change is intentional; the `config.example.toml` update (Unit 4) removes the examples.
- **Unchanged invariants:** The broadcast channel, `start_rest_server`, `LiveSyncer`, and all JSON-RPC methods are unaffected by webhook removal. `query_events_for_filter` is read-only and unchanged; the new `stream_events_for_filter` adds a method without modifying existing behavior.

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| User has a `config.toml` with `webhook` fields — sync will fail to parse after removal | `config.example.toml` update makes the removal explicit; the error message from the TOML parser names the unknown field |
| `HashMap` or `Url` import removal from `config.rs` breaks unrelated code | Compiler will catch it; verify `url::Url` stays (it's used by `NodeConfig`), only `HashMap` goes |
| sqlx `fetch` stream lifetime constraints complicate the return type in `stream_events_for_filter` | Follow the existing `query_events_for_filter` borrow pattern; the pool reference lifetime is already established |
| stdout write errors during export (e.g., broken pipe from `head`) | Check `io::Result` on each write; exit cleanly on `BrokenPipe` (standard Unix behavior — do not propagate as an error) |

## Sources & References

- **Origin document:** [docs/brainstorms/2026-04-21-scope-refinement-requirements.md](docs/brainstorms/2026-04-21-scope-refinement-requirements.md)
- Webhook code: `crates/scopenode-core/src/webhook.rs`
- Storage query patterns: `crates/scopenode-storage/src/db.rs`
- Export command pattern: `crates/scopenode/src/commands/query.rs`
- CLI command pattern: `crates/scopenode/src/cli.rs`
