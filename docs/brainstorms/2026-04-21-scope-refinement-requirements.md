---
date: 2026-04-21
topic: scope-refinement
---

# Scope Refinement — Core Only

## Problem Frame

scopenode's core promise is narrow and strong: **no RPC provider, no full node, free blockchain data, low resources.** As phases were planned, features crept in that serve different users (data scientists, ML engineers, app developers wanting push delivery). These don't serve the core user — a solo dev or small team who wants specific contract events without Infura, Alchemy, or a 1TB node. Carrying them raises maintenance cost and blurs the product identity.

This document defines what stays, what is removed from the codebase, and what is removed from planned phases.

---

## Requirements

**Remove from codebase (already built)**

- R1. Delete `crates/scopenode-core/src/webhook.rs` (577 lines including wiremock-based tests) and all webhook dispatch calls in `pipeline.rs` and `live.rs`.
- R2. Remove `webhook`, `webhook_secret`, and `webhook_events` fields from `ContractConfig` in `config.rs`. Also update all struct literal sites in `pipeline.rs` and `live.rs` test helpers that set `webhook: None` — these will fail to compile when the fields are gone.
- R3. Remove `hmac`, `sha2`, and `wiremock` from `Cargo.toml` — all three are exclusively used by the webhook system.

**Remove from planned phases (never build)**

- R4. Remove Python bindings section from `docs/phases/PHASE_04_developer.md`. No `crates/scopenode-py`, no PyO3, no maturin/pip packaging.
- R5. Remove Parquet export from `docs/phases/PHASE_04_developer.md`. `scopenode export` keeps `--format csv` and `--format json` only. Users who need Parquet can convert JSON with DuckDB.
- R6. Remove SQL views from `docs/phases/PHASE_04_developer.md`. No `[[contracts.views]]` config blocks. Users query SQLite directly or via `GET /events`.
- R7. Remove `scopenode init` interactive wizard from `docs/phases/PHASE_04_developer.md`. Config is already simple TOML; the wizard adds UX complexity that isn't part of the core data pipeline.
- R8. Delete `docs/phases/PHASE_05_computed_state.md`. The Rhai scripting engine (computed state, per-event handlers, handler tables) is not built and will not be built.
- R10. Update `VISION.md` to remove all mentions of: webhooks (TOML example and description), `scopenode init` wizard (CLI table and description), and Parquet export (CLI table and data science framing). These sections describe features that are now out of scope.
- R11. Update `README.md` to remove the `CSV/JSON/Parquet export` checkbox and any webhook or `scopenode init` mentions.

**Add to core (genuine gap)**

- R9. Add `scopenode export` CLI command with `--format csv` and `--format json`. The current `scopenode query` caps at 20 rows; there is no way to dump a full event set without REST pagination or direct SQLite access. Export streams output — no full load into memory (requires switching `db.rs` from `fetch_all` to a row-by-row cursor). Accepts the same filters as `GET /events`: contract, event name, block range, and `--topic0`. Route through `query_events_for_filter` (already supports the full filter surface including topic0 and offset) rather than the narrower `query_events` used by the `query` command.

---

## Success Criteria

- `webhook.rs` is deleted; no webhook-related fields remain in config or types.
- Phase 4 doc describes only: REST API, SSE, `eth_subscribe`, CSV/JSON export, and `--topic0` filter.
- Phase 5 doc is deleted.
- `scopenode export --format csv --event Swap` produces a valid CSV with all rows (no cap).
- `scopenode export --format json | wc -l` grows as blocks are indexed — output streams, does not buffer.
- `VISION.md` and `README.md` contain no mentions of webhooks, `scopenode init` wizard, Parquet, Python bindings, Rhai, or SQL views.

---

## Scope Boundaries

- Webhooks are out entirely — REST + SSE covers all "get the data" use cases without push delivery.
- Python bindings are out — callers can query the REST API or SQLite directly.
- Parquet is out — DuckDB can convert JSON in one line.
- Rhai scripting is out — raw decoded events are the product; aggregation is the caller's job.
- SQL views are out — `GET /events` with query params and direct SQLite access are sufficient.
- `scopenode init` wizard is out — TOML config is the interface.
- `eth_subscribe` WebSocket stays — it is part of the JSON-RPC :8545 drop-in promise and costs nothing (jsonrpsee already supports WebSocket).

---

## Key Decisions

- **Webhooks cut**: Push-to-external is not the user's goal. "Get data locally" is. SSE at `GET /stream/events` is enough for live consumption.
- **Export kept (CSV/JSON only)**: Getting data out of scopenode is core. Parquet is a niche format that adds a heavy dependency; CSV and JSON cover the use case.
- **Rhai and Phase 5 deleted entirely**: Computed state is powerful but turns scopenode into a subgraph engine. That is a different product. Raw events are the right boundary.
- **`scopenode init` dropped**: Adds a dependency (`dialoguer`) for a wizard that wraps 10 lines of TOML. Cost exceeds value.

---

## Next Steps

-> `/ce:plan` for structured implementation planning

