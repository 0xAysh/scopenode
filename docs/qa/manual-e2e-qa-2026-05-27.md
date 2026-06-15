# Manual end-to-end QA checklist

Original date: 2026-05-27  
Updated for current code: 2026-06-07  
Status: reusable manual checklist; prior branch-specific results were removed

## Scope

Verify the current local ERA1/ERE event-indexing product:

- config validation and dry-run planning;
- local or Sourcify ABI resolution;
- ERA1 and ERE discovery;
- verified, idempotent event indexing;
- coverage-aware queries;
- `status`;
- JSON-RPC and REST serving.

## Setup

Use disposable state and an archive range with known logs:

```bash
rm -rf .qa-scopenode
mkdir -p .qa-scopenode
cargo build --workspace
```

Create `.qa-scopenode/config.toml` with:

- `data_dir = ".qa-scopenode/data"`;
- a real local archive directory containing `.era1` or `.ere` files;
- a contract and inclusive range covered by those files;
- either a valid `abi_override` or a Sourcify-verified address.

## Automated baseline

```bash
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Do not begin manual QA with a failing baseline.

## 1. CLI and configuration

```bash
cargo run -p scopenode -- --help
cargo run -p scopenode -- sync --help
cargo run -p scopenode -- serve --help
cargo run -p scopenode -- status --help
cargo run -p scopenode -- sync \
  --config .qa-scopenode/config.toml \
  --dry-run
```

Verify:

- only `sync`, `serve`, and `status` are advertised;
- dry run prints the archive source and every contract range;
- `to_block < from_block`, empty events, mixed wildcard/named events, and
  unknown fields fail clearly;
- integer and `K`/`M` block forms parse;
- `events = ["*"]` alone succeeds.

## 2. ABI resolution

Exercise independently:

1. valid local `abi_override`;
2. no override for a Sourcify-verified contract;
3. invalid override falling back to Sourcify;
4. proxy `impl_address`;
5. no usable local or remote ABI.

Verify the successful ABI is cached under the configured contract address and a
second sync can use the cache. Failure should recommend a usable
`abi_override`.

## 3. Archive discovery

Use ranges that select:

- one `.era1` file;
- one `.ere` file;
- multiple overlapping files;
- no files.

Verify unrelated files are ignored, selected manifests report the correct
format/range, malformed supported archives fail explicitly, and ERE profiles
without usable receipts are rejected or skipped according to current source
policy.

## 4. Sync and persistence

```bash
cargo run -p scopenode -- sync \
  --config .qa-scopenode/config.toml

cargo run -p scopenode -- status \
  --config .qa-scopenode/config.toml
```

Inspect SQLite when useful:

```bash
sqlite3 .qa-scopenode/data/scopenode.db \
  'select count(*) from contracts;'
sqlite3 .qa-scopenode/data/scopenode.db \
  'select count(*) from events;'
sqlite3 .qa-scopenode/data/scopenode.db \
  'select contract,from_block,to_block,source from covered_ranges;'
```

Verify:

- matching logs are decoded and stored in block/log order;
- receipt-root verification runs before event acceptance;
- a clean scope writes coverage;
- incomplete archive reads, verification failures, decode failures, or store
  failures do not write successful coverage;
- repeating sync does not duplicate events.

## 5. Status

Test an empty database and an indexed database.

Verify:

- DB path and size are correct;
- contract/event counts match SQLite;
- event breakdowns are correct;
- displayed block range reflects stored events;
- JSON-RPC and REST URLs use configured ports.

Remember: status currently derives its block range from events, so a covered
eventless range is not represented there.

## 6. Serve and JSON-RPC

```bash
cargo run -p scopenode -- serve \
  --config .qa-scopenode/config.toml
```

In another terminal, verify:

- `eth_chainId` returns `0x1`;
- `net_peerCount` returns `0x0`;
- `eth_blockNumber` returns the latest stored-event block or `0x0`;
- `eth_getLogs` returns correctly projected logs for one indexed address;
- missing address returns `-32602` (Invalid params) with a message naming the field;
- multiple addresses returns `-32002`;
- topic0 OR and topics beyond topic0 are rejected;
- an unindexed contract returns `-32000`;
- an uncovered explicit range returns `-32001`;
- more than 10,000 rows returns `-32005`;
- exactly 10,000 rows is allowed.

## 7. REST

Verify:

```text
GET /status
GET /contracts
GET /abi/:address
GET /events?contract=...&event=...&topic0=...&fromBlock=...&toBlock=...
```

Check:

- limit defaults to 100 and is clamped to 10,000;
- offset pagination works: `limit=100&offset=100` returns the next page;
- covered empty queries return an empty list;
- unindexed contracts return an empty list, intentionally differing from RPC;
- uncovered explicit ranges return HTTP 400;
- queries that would materialise more than 10,000 rows return HTTP 400 (the 10k hard cap, not the user-supplied limit);
- missing ABIs return HTTP 404.

## 8. Operational failures

Verify clear errors for:

- missing config;
- unwritable data directory;
- missing archive directory;
- malformed archive;
- occupied RPC or REST port;
- Sourcify timeout/404 without a valid local ABI.

## Result log

| Area | Result | Evidence | Notes |
|---|---|---|---|
| Automated baseline | Pending | | |
| CLI/config | Pending | | |
| ABI resolution | Pending | | |
| Archive discovery | Pending | | |
| Sync/persistence | Pending | | |
| Coverage failure semantics | Pending | | |
| Status | Pending | | |
| JSON-RPC | Pending | | |
| REST | Pending | | |
| Operational failures | Pending | | |
