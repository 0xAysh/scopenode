# scopenode Onboarding Guide

This guide is for learning the current codebase, Rust, and the Ethereum concepts
needed to change it safely. Do not begin by reading every file top to bottom.
Follow one end-to-end data path, run it, and only then widen your understanding.

## 1. Build a correct mental model

scopenode is not a custom Ethereum peer or a full node. It is a bounded archive
indexer:

```text
local ERA1/ERE archives
  → block facts
  → scope bloom check
  → receipt-root verification
  → ABI event decoding
  → SQLite events + coverage
  → JSON-RPC / REST queries
```

There are two main flows:

### Write path

```text
main.rs
  → cli.rs
  → commands/sync.rs
  → RuntimeContext::load
  → SyncPlan::from_config
  → Era1Source::scan
  → AbiResolver::resolve_events
  → run_era1_scopes
  → BlockPipeline::process_block
  → DbEventSink
  → SQLite
```

### Read path

```text
JSON-RPC filter or REST query
  → FilterPlan
  → execute_event_query
  → Db::query_events
  → EventQueryOutcome
  → projection
  → transport response
```

If you can explain those two flows without looking at the diagram, you understand
the architecture well enough to begin making local changes.

## 2. Run the system before reading deeply

```bash
cargo test --workspace
cargo run -p scopenode -- --help
cargo run -p scopenode -- sync --help
cargo run -p scopenode -- status --help
```

Then create a disposable config and run:

```bash
cargo run -p scopenode -- sync --config config.toml --dry-run
```

The dry run teaches three important boundaries without requiring a full archive
scan: config loading, path resolution, and scope planning.

## 3. Learn only the Rust needed for the next module

Do not pause the project to “learn Rust first.” Learn these concepts in this
order while reading the named files:

| Rust concept | Why it matters here | First file |
|---|---|---|
| structs, enums, `Option`, `Result` | Configuration and explicit query outcomes | `crates/scopenode-core/src/config.rs` |
| modules and `pub` visibility | The workspace is split by responsibility | each crate's `lib.rs` |
| ownership and borrowing | Archive facts and decoded events move through the pipeline | `era_pipeline.rs` |
| iterators | Archive files and block facts are streamed | `source.rs`, `era1_reader.rs` |
| traits | Storage, ABI fetching, progress, and verification are replaceable seams | `abi_resolution.rs`, `era_pipeline.rs` |
| `async`/`.await` | SQLite, HTTP ABI fetches, and servers are asynchronous | `commands/sync.rs`, `query.rs` |
| `Arc<dyn Trait>` | Runtime-selected shared adapters | `abi_resolution.rs` |
| error propagation with `?` | Failures remain typed until command boundaries | throughout |

For each unfamiliar construct:

1. predict what the function owns and returns;
2. use rust-analyzer “go to definition”;
3. change or add one test;
4. run only that crate's test;
5. explain the compiler error before fixing it.

The compiler is part of the curriculum.

## 4. Ethereum concepts in dependency order

Learn the concepts in the order the pipeline uses them.

### Log and event

A contract event is stored in a transaction receipt as a log:

- `address`: emitting contract;
- `topics[0]`: usually the hash of the event signature;
- additional topics: indexed event arguments;
- `data`: ABI-encoded non-indexed arguments.

Read `crates/scopenode-core/src/abi.rs` after you understand this shape.

### ABI and topic0

The ABI describes event names and parameter types. For a normal event,
`topic0 = keccak256("EventName(type1,type2,...)")`. scopenode compiles resolved
event definitions once, then uses them for bloom checks and log decoding.

Read `abi_resolution.rs` before `abi.rs`: resolution decides *which definitions
exist*; decoding decides *how matching bytes become values*.

### Block header and logs bloom

Each block header contains a probabilistic `logs_bloom`. A negative check proves
the block cannot contain a configured address/topic combination. A positive
check only means “possibly”; receipts must still be inspected.

Read `headers.rs`, then the scope-matching part of `era_pipeline.rs`.

### Receipt and receipt trie root

A receipt contains transaction outcome data and emitted logs. The block header's
`receipts_root` commits to the ordered receipt set. scopenode reconstructs the
receipt trie and compares roots before accepting logs from a candidate block.

Read `receipts.rs`, then `MerkleVerifier` in `era_pipeline.rs`.

### ERA1, ERE, E2Store, and RLP

ERA1 and ERE are execution-history archive formats built from typed E2Store
entries. scopenode:

- discovers files and their block ranges in `source.rs`;
- parses low-level entries and indexes in `e2store.rs`;
- decompresses and decodes payloads in `era1_codec.rs`;
- assembles public `Era1BlockFacts` in `era1_reader.rs`.

Despite the historical `Era1*` names, the reader dispatches both `.era1` and
`.ere` files.

### Coverage

No matching rows can mean either “nothing happened” or “we never processed that
range.” `covered_ranges` lets the query layer distinguish these cases. Coverage
is withheld if archive reading, verification, decoding, or storage makes the
scope incomplete.

Read `coverage.rs`, then `query.rs`, then the completion logic at the end of
`run_era1_scopes`.

## 5. Recommended code-reading sequence

### Pass A: product shell

1. `crates/scopenode/src/cli.rs`
2. `crates/scopenode/src/main.rs`
3. `crates/scopenode/src/runtime.rs`
4. `crates/scopenode-core/src/config.rs`
5. `crates/scopenode/src/sync_plan.rs`

Goal: explain how a TOML file becomes validated runtime state and contract
scopes.

### Pass B: one sync

1. `crates/scopenode/src/commands/sync.rs`
2. `crates/scopenode-core/src/abi_resolution.rs`
3. `crates/scopenode/src/sourcify.rs`
4. `crates/scopenode-core/src/source.rs`
5. `crates/scopenode-core/src/era_pipeline.rs`
6. `crates/scopenode-storage/src/sink.rs`

Goal: trace one configured contract from config to stored rows and recorded
coverage.

### Pass C: archive internals

1. `e2store.rs`
2. `era1_codec.rs`
3. `era1_reader.rs`
4. `headers.rs`
5. `receipts.rs`
6. `abi.rs`

Goal: explain exactly which bytes are trusted, decoded, verified, and matched.

### Pass D: one query

1. `crates/scopenode-rpc/src/filter_plan.rs`
2. `crates/scopenode-rpc/src/query_front_door.rs`
3. `crates/scopenode-storage/src/query.rs`
4. `crates/scopenode-storage/src/db.rs`
5. `crates/scopenode-rpc/src/projection.rs`
6. `server.rs` and `rest.rs`

Goal: explain why not-indexed, empty, missing-coverage, capped, and successful
results are distinct.

### Pass E: tests as executable documentation

Read tests next to each module, then:

- `crates/scopenode-core/tests/era1_pipeline_test.rs`
- `crates/scopenode-core/tests/era1_manifest_fixture.rs`
- `crates/scopenode-rpc/tests/parity.rs`

These reveal the intended cross-crate contracts more reliably than comments.

## 6. Responsibility map

| Module | Owns | Must not own |
|---|---|---|
| CLI/runtime | command parsing, config/data-dir startup | archive or query policy |
| sync plan | immutable scope/range planning | I/O |
| source | archive discovery, manifests, file selection | ABI or SQLite |
| reader/codec/e2store | archive byte interpretation | contract selection |
| ABI resolution | cache/local/remote lookup order | receipt traversal |
| event decoder | topic matching and ABI decoding | network fetching |
| pipeline | orchestration and completion semantics | SQL details |
| storage | persistence, coverage, query outcomes | transport error codes |
| query front door | shared query outcome translation | SQL construction |
| RPC/REST adapters | input normalization and response shape | domain policy duplication |

When a change appears to cross several rows, stop and identify whether a missing
interface is causing policy to leak.

## 7. Practical learning exercises

Do these in order. Each is small enough to finish and forces you through a real
architectural seam.

1. Add a config validation test for one invalid value and improve its error.
2. Add a filename/profile test in `source.rs`.
3. Add an ABI event-decoding test with one indexed and one non-indexed field.
4. Add a storage query test for one `EventQueryOutcome`.
5. Add equivalent JSON-RPC and REST assertions in `tests/parity.rs`.
6. Trace and document why a failed block prevents coverage from being written.

Avoid beginning with archive codecs or receipt trie verification. They are
important, but they combine binary formats, Ethereum encoding, and Rust
ownership—the steepest possible starting point.

## 8. How to make changes safely

Use this loop:

```text
state the invariant
  → locate its owning module
  → write the smallest failing test
  → implement locally
  → run focused tests
  → run workspace tests + clippy
  → update the relevant living doc
```

Useful commands:

```bash
cargo test -p scopenode-core <test-name>
cargo test -p scopenode-storage <test-name>
cargo test -p scopenode-rpc <test-name>
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

## 9. Known sharp edges

- Names still use `Era1` even where behavior supports ERE.
- `source.rs`, `db.rs`, and `abi.rs` remain large and deserve careful,
  evidence-driven refactoring rather than cosmetic splitting.
- `eth_getLogs` supports only one address and one topic0 value.
- REST and JSON-RPC intentionally differ for unindexed contracts.
- ABI cache entries contain normalized event definitions, not necessarily the
  original full contract ABI document.
- `status` reports the range of stored events; an eventless covered range is not
  visible there.
- the schema migration history contains removed devp2p-era tables; the active
  schema after all migrations is much smaller.

## 10. Your first ownership milestone

You are ready to work independently when you can:

1. draw the write and read paths from memory;
2. explain the trust boundary between archive bytes, header commitments, and
   decoded events;
3. name the module that owns any proposed behavior change;
4. predict which tests should fail before editing;
5. explain when coverage is and is not recorded.

That is a better target than memorizing every file.
