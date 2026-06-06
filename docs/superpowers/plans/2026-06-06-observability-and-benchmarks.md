# Observability and Benchmarks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add structured human/JSON logs, bounded Prometheus metrics at `/metrics`, and local Criterion benchmarks for scopenode's sync and serving paths.

**Architecture:** Add a dependency-light `scopenode-observability` workspace crate that owns the Prometheus registry, bounded label enums, timers, and encoding. Core, storage, RPC, and CLI layers record at their existing operation boundaries; the binary remains responsible for subscriber initialization and output format. Criterion benches live with the crate that owns each public operation, while real ERA1/ERE inputs remain opt-in through environment variables.

**Tech Stack:** Rust 2021, `tracing`, `tracing-subscriber` JSON formatting, `prometheus` 0.14, Axum middleware, jsonrpsee methods, Criterion 0.8, Tokio, SQLite/sqlx.

---

## File Map

**Create**

- `crates/scopenode-observability/Cargo.toml` - independent metrics crate manifest.
- `crates/scopenode-observability/src/lib.rs` - registry, metric handles, bounded labels, timers, guards, and Prometheus encoding.
- `crates/scopenode/src/logging.rs` - CLI subscriber construction for human and JSON output.
- `crates/scopenode-core/benches/archive_pipeline.rs` - fixture and opt-in real archive benchmarks.
- `crates/scopenode-storage/benches/storage.rs` - SQLite insert and query benchmarks.

**Modify**

- `Cargo.toml` - add the workspace crate and shared dependencies.
- `crates/scopenode/Cargo.toml` - consume observability and enable logging test support.
- `crates/scopenode/src/cli.rs` - add `LogFormat`.
- `crates/scopenode/src/main.rs` - initialize logging and metrics once.
- `crates/scopenode/src/commands/sync.rs` - sync command lifecycle logs and metrics.
- `crates/scopenode-core/Cargo.toml` - observability dependency and Criterion bench target.
- `crates/scopenode-core/src/era_pipeline.rs` - archive, block, bloom, verification, and decode instrumentation.
- `crates/scopenode-storage/Cargo.toml` - observability dependency and Criterion bench target.
- `crates/scopenode-storage/src/db.rs` - batch storage metrics and spans.
- `crates/scopenode-rpc/Cargo.toml` - observability dependency and Tower middleware support.
- `crates/scopenode-rpc/src/query_front_door.rs` - shared query timing and outcomes.
- `crates/scopenode-rpc/src/rest.rs` - `/metrics` and REST request instrumentation.
- `crates/scopenode-rpc/src/server.rs` - JSON-RPC request instrumentation.
- `README.md` - logging, metrics, and benchmark usage.

**Do not overwrite**

- `crates/scopenode-core/src/e2store.rs`
- `crates/scopenode-core/src/era1_codec.rs`
- `crates/scopenode-core/src/era1_reader.rs`
- `crates/scopenode-core/src/source.rs`

These files contain pre-existing uncommitted ERE support. Read and preserve their
current state. The archive benchmark should use their public APIs without
refactoring or reverting them.

### Task 1: Add The Metrics Foundation

**Files:**
- Create: `crates/scopenode-observability/Cargo.toml`
- Create: `crates/scopenode-observability/src/lib.rs`
- Modify: `Cargo.toml`

- [ ] **Step 1: Write registry and bounded-label tests**

Start `crates/scopenode-observability/src/lib.rs` with tests that define the
public contract before the implementation:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_sync_and_request_metrics() {
        let metrics = Metrics::new().unwrap();
        metrics.record_sync_run(Outcome::Success, 1.25);
        let request = metrics.start_request(Transport::Rest, Endpoint::Events);
        request.finish(Outcome::Empty);
        let text = metrics.encode().unwrap();

        assert!(text.contains(
            "scopenode_sync_runs_total{outcome=\"success\"} 1"
        ));
        assert!(text.contains(
            "scopenode_requests_total{endpoint=\"events\",outcome=\"empty\",transport=\"rest\"} 1"
        ));
    }

    #[test]
    fn labels_are_closed_enums() {
        assert_eq!(ArchiveFormat::Era1.as_str(), "era1");
        assert_eq!(ArchiveFormat::Ere.as_str(), "ere");
        assert_eq!(ErrorCategory::Storage.as_str(), "storage");
        assert_eq!(Endpoint::Abi.as_str(), "abi");
    }

    #[test]
    fn in_flight_guard_balances_gauge() {
        let metrics = Metrics::new().unwrap();
        {
            let _guard = metrics.start_request(Transport::Rest, Endpoint::Status);
            let text = metrics.encode().unwrap();
            assert!(text.contains(
                "scopenode_requests_in_flight{endpoint=\"status\",transport=\"rest\"} 1"
            ));
        }
        let text = metrics.encode().unwrap();
        assert!(text.contains(
            "scopenode_requests_in_flight{endpoint=\"status\",transport=\"rest\"} 0"
        ));
    }
}
```

- [ ] **Step 2: Run the new crate test to verify it fails**

Run:

```bash
cargo test -p scopenode-observability
```

Expected: FAIL because the workspace member and metrics implementation do not
exist yet.

- [ ] **Step 3: Add the workspace dependencies and crate manifest**

In the root `Cargo.toml`, add the member and dependencies:

```toml
[workspace]
members = [
    "crates/scopenode",
    "crates/scopenode-core",
    "crates/scopenode-observability",
    "crates/scopenode-storage",
    "crates/scopenode-rpc",
]

[workspace.dependencies]
prometheus = "0.14"
criterion = { version = "0.8", features = ["async_tokio"] }
scopenode-observability = { path = "crates/scopenode-observability" }
```

Create `crates/scopenode-observability/Cargo.toml`:

```toml
[package]
name = "scopenode-observability"
version = "0.1.0"
edition = "2021"

[dependencies]
prometheus = { workspace = true }
```

- [ ] **Step 4: Implement the registry and closed labels**

Implement `Metrics` around a private `prometheus::Registry`. Register every
metric from the approved design in `Metrics::new()`. Use:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Success,
    Empty,
    NotIndexed,
    MissingCoverage,
    Capped,
    Unsupported,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transport {
    Rest,
    Rpc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endpoint {
    Events,
    Status,
    Contracts,
    Abi,
    Metrics,
    GetLogs,
    BlockNumber,
    ChainId,
    PeerCount,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveFormat {
    Era1,
    Ere,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorCategory {
    Source,
    Decode,
    Verify,
    Storage,
    Query,
    Unsupported,
    Encode,
    Internal,
}
```

Each enum gets a non-publicly-extensible `as_str() -> &'static str`. Expose
narrow recording methods rather than metric vectors:

```rust
pub fn record_sync_run(&self, outcome: Outcome, seconds: f64);
pub fn record_source_scan(&self, seconds: f64);
pub fn record_archive_file(
    &self,
    format: ArchiveFormat,
    outcome: Outcome,
    seconds: f64,
);
pub fn record_block_processed(&self, format: ArchiveFormat);
pub fn record_block_failed(
    &self,
    format: ArchiveFormat,
    category: ErrorCategory,
);
pub fn add_bloom_matches(&self, count: u64);
pub fn record_receipt_verification(&self, outcome: Outcome, seconds: f64);
pub fn add_events_decoded(&self, count: u64);
pub fn record_store_batch(&self, outcome: Outcome, size: usize, seconds: f64);
pub fn add_events_stored(&self, count: u64);
pub fn record_query(&self, outcome: Outcome, rows: usize, seconds: f64);
pub fn record_query_error(&self, category: ErrorCategory);
pub fn start_request(
    &self,
    transport: Transport,
    endpoint: Endpoint,
) -> RequestTimer<'_>;
pub fn encode(&self) -> Result<String, prometheus::Error>;
```

Use custom histogram buckets suitable for seconds:

```rust
const LATENCY_BUCKETS: &[f64] = &[
    0.000_1, 0.000_5, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 30.0,
];
```

Add a global instance:

```rust
static GLOBAL: std::sync::OnceLock<Metrics> = std::sync::OnceLock::new();

pub fn global() -> &'static Metrics {
    GLOBAL.get_or_init(|| Metrics::new().expect("metrics descriptors are valid"))
}
```

`RequestTimer` owns an `Instant`, increments the matching in-flight gauge when
created, and exposes `pub fn finish(mut self, outcome: Outcome)`. `finish`
records count and duration, marks the timer finished, then decrements the gauge.
Its `Drop` implementation records `Outcome::Error` and decrements the gauge if
the caller exits without finishing.

- [ ] **Step 5: Run tests and formatting**

Run:

```bash
cargo fmt --all --check
cargo test -p scopenode-observability
cargo clippy -p scopenode-observability --all-targets -- -D warnings
```

Expected: all commands PASS.

- [ ] **Step 6: Commit the foundation**

```bash
git add Cargo.toml Cargo.lock crates/scopenode-observability
git commit -m "Add bounded observability metrics"
```

### Task 2: Add Human And JSON Logging Configuration

**Files:**
- Create: `crates/scopenode/src/logging.rs`
- Modify: `Cargo.toml`
- Modify: `crates/scopenode/Cargo.toml`
- Modify: `crates/scopenode/src/cli.rs`
- Modify: `crates/scopenode/src/main.rs`

- [ ] **Step 1: Write CLI parsing tests**

Add to `crates/scopenode/src/cli.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_human_logs() {
        let cli = Cli::try_parse_from(["scopenode", "status"]).unwrap();
        assert_eq!(cli.log_format, LogFormat::Human);
    }

    #[test]
    fn parses_json_logs_and_verbosity() {
        let cli = Cli::try_parse_from([
            "scopenode",
            "--log-format",
            "json",
            "-vv",
            "status",
        ])
        .unwrap();
        assert_eq!(cli.log_format, LogFormat::Json);
        assert_eq!(cli.verbose, 2);
    }
}
```

- [ ] **Step 2: Run the CLI tests to verify they fail**

Run:

```bash
cargo test -p scopenode cli::tests
```

Expected: FAIL because `LogFormat` and `log_format` do not exist.

- [ ] **Step 3: Add the CLI option**

Use Clap's closed value enum:

```rust
use clap::{ArgAction, Parser, Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum LogFormat {
    #[default]
    Human,
    Json,
}
```

Add to `Cli`:

```rust
/// Log output format
#[arg(long, global = true, value_enum, default_value_t = LogFormat::Human)]
pub log_format: LogFormat,
```

- [ ] **Step 4: Implement subscriber construction**

Enable JSON formatting:

```toml
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

Create `crates/scopenode/src/logging.rs` with:

```rust
pub fn init(format: LogFormat, verbose: u8) {
    let default_level = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(default_level));

    match format {
        LogFormat::Human => fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_writer(std::io::stderr)
            .init(),
        LogFormat::Json => fmt()
            .json()
            .with_env_filter(filter)
            .with_target(true)
            .with_writer(std::io::stderr)
            .init(),
    }
}
```

In `main.rs`, add `mod logging;`, remove direct subscriber imports, call
`logging::init(cli.log_format, cli.verbose)`, and initialize
`scopenode_observability::global()` before dispatching the command.

- [ ] **Step 5: Run focused and workspace checks**

Run:

```bash
cargo test -p scopenode cli::tests
cargo check --workspace
cargo clippy -p scopenode --all-targets -- -D warnings
```

Expected: all commands PASS.

- [ ] **Step 6: Manually verify both output formats**

Run against a deliberately missing config so initialization logs and errors are
short:

```bash
cargo run -q -p scopenode -- -v --log-format human status --config /tmp/scopenode-missing.toml 2>&1
cargo run -q -p scopenode -- -v --log-format json status --config /tmp/scopenode-missing.toml 2>&1
```

Expected: the first output is readable text; every tracing line in the second is
valid JSON. Both commands exit non-zero because the config is missing.

- [ ] **Step 7: Commit logging**

```bash
git add Cargo.toml Cargo.lock crates/scopenode/Cargo.toml crates/scopenode/src/cli.rs crates/scopenode/src/logging.rs crates/scopenode/src/main.rs
git commit -m "Add selectable structured log output"
```

### Task 3: Instrument The Sync Pipeline

**Files:**
- Modify: `crates/scopenode-core/Cargo.toml`
- Modify: `crates/scopenode-core/src/era_pipeline.rs`
- Modify: `crates/scopenode-storage/Cargo.toml`
- Modify: `crates/scopenode-storage/src/db.rs`
- Modify: `crates/scopenode/Cargo.toml`
- Modify: `crates/scopenode/src/commands/sync.rs`
- Test: `crates/scopenode-core/tests/era1_pipeline_test.rs`

- [ ] **Step 1: Add a failing recovery-metrics test**

In `crates/scopenode-core/tests/era1_pipeline_test.rs`, add a test that snapshots
the global Prometheus text, runs the existing corrupt-source or failing-store
fixture path, then verifies the relevant counter increased:

```rust
fn metric_value(text: &str, prefix: &str) -> f64 {
    text.lines()
        .find(|line| line.starts_with(prefix))
        .and_then(|line| line.rsplit_once(' '))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0.0)
}

#[tokio::test]
async fn failed_store_records_storage_metrics_without_recording_coverage() {
    let before = scopenode_observability::global().encode().unwrap();
    // Reuse FailingStoreSink and the existing synthetic source setup.
    let summary = run_era1_scopes(
        &source,
        &[contract],
        &resolver,
        &sink,
        &NullReporter,
    )
        .await
        .unwrap();
    let after = scopenode_observability::global().encode().unwrap();

    assert!(
        metric_value(
            &after,
            "scopenode_store_batches_total{outcome=\"error\"}"
        ) > metric_value(
            &before,
            "scopenode_store_batches_total{outcome=\"error\"}"
        )
    );
    assert!(sink.covered_ranges().await.is_empty());
    assert_eq!(summary.store_failures, 1);
}
```

Adapt only the fixture setup names to the helpers already present in that file;
do not duplicate the binary ERA fixture builders.

- [ ] **Step 2: Run the focused test to verify it fails**

Run:

```bash
cargo test -p scopenode-core --test era1_pipeline_test failed_store_records_storage_metrics_without_recording_coverage
```

Expected: FAIL because core does not yet depend on or record observability
metrics.

- [ ] **Step 3: Instrument block and file processing**

Add `scopenode-observability` dependencies to core, storage, and the binary.

In `era_pipeline.rs`:

- add a public `PipelineRunSummary` with `files_processed`, `blocks_processed`,
  `source_failures`, `store_failures`, `events_decoded`, and `events_stored`;
- return `Result<PipelineRunSummary, CoreError>` from `run_era1_scope` and
  `run_era1_scopes`;
- create an `archive_file` span with path and `format`;
- derive `ArchiveFormat` from the manifest format or extension;
- time each file from iterator creation through completion;
- increment processed blocks after successful `Era1BlockFacts` decoding;
- count failed block reads as `ErrorCategory::Decode`;
- add bloom match count before verification;
- time receipt verification and record `success` or `error`;
- time event extraction and add the decoded-event count;
- time `sink.store`, recording batch size, outcome, and stored-event count;
- retain every current skip/coverage behavior.

Use `std::time::Instant` and `tracing::debug_span!`:

```rust
let file_started = Instant::now();
let file_span = tracing::debug_span!(
    "archive_file",
    path = %file.path().display(),
    format = format.as_str(),
);
let _entered = file_span.enter();
```

At per-block `trace` level, log only identity and counts:

```rust
tracing::trace!(
    block = facts.block_number,
    receipts = facts.receipts.len(),
    tx_hashes = facts.tx_hashes.len(),
    "Decoded archive block facts"
);
```

- [ ] **Step 4: Instrument SQLite batches**

In `Db::insert_events`, add a `store_batch` debug span containing only
`event_count` and emit a completion `debug!` containing elapsed time. Avoid
double counting: the pipeline sink boundary owns Prometheus store batch metrics
because it observes every `EventSink`, including synthetic and future
non-SQLite sinks.

Use a single result block so every error path records duration:

```rust
let started = std::time::Instant::now();
let span = tracing::debug_span!("store_batch", event_count = events.len());
let _entered = span.enter();
```

Place those lines immediately before `let mut tx = self.pool.begin()`. Keep the
transaction and chunk loop unchanged, then place this immediately before
`Ok(())`:

```rust
tracing::debug!(
    elapsed_seconds = started.elapsed().as_secs_f64(),
    "Stored event batch"
);
```

- [ ] **Step 5: Instrument command lifecycle**

In `commands/sync.rs`, wrap source scanning and the full run:

```rust
let started = Instant::now();
let span = tracing::info_span!(
    "sync_run",
    from_block = *plan.block_range.start(),
    to_block = *plan.block_range.end(),
    contracts = plan.contracts.len(),
);
let _entered = span.enter();
```

Record source scan duration immediately after `Era1Source::scan`. Use the
returned `PipelineRunSummary` for the final `info!` summary. A summary with
non-zero source or store failures records `Outcome::Error`; a clean summary
records `Outcome::Success`. On a returned `Err`, log context and record
`Outcome::Error` before returning the same error.

- [ ] **Step 6: Run pipeline and storage tests**

Run:

```bash
cargo test -p scopenode-core --test era1_pipeline_test
cargo test -p scopenode-core era_pipeline
cargo test -p scopenode-storage
cargo clippy -p scopenode-core -p scopenode-storage -p scopenode --all-targets -- -D warnings
```

Expected: all commands PASS and existing coverage recovery tests remain
unchanged.

- [ ] **Step 7: Commit sync instrumentation**

```bash
git add Cargo.lock crates/scopenode-core/Cargo.toml crates/scopenode-core/src/era_pipeline.rs crates/scopenode-core/tests/era1_pipeline_test.rs crates/scopenode-storage/Cargo.toml crates/scopenode-storage/src/db.rs crates/scopenode/Cargo.toml crates/scopenode/src/commands/sync.rs
git commit -m "Instrument archive sync performance"
```

### Task 4: Expose Metrics And Instrument REST

**Files:**
- Modify: `crates/scopenode-rpc/Cargo.toml`
- Modify: `crates/scopenode-rpc/src/query_front_door.rs`
- Modify: `crates/scopenode-rpc/src/rest.rs`
- Test: `crates/scopenode-rpc/tests/parity.rs`

- [ ] **Step 1: Write failing `/metrics` and query-outcome tests**

Add a router test in `rest.rs`:

```rust
#[tokio::test]
async fn metrics_endpoint_exposes_prometheus_text() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let (file, path) = tmp.into_parts();
    drop(file);
    let db = Db::open(path.to_path_buf()).await.unwrap();
    let response = build_rest_router(db)
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[CONTENT_TYPE],
        "text/plain; version=0.0.4"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("scopenode_requests_total"));
}
```

Add to `query_front_door.rs` a before/after assertion that the existing capped
query test increments:

```text
scopenode_query_duration_seconds_count{outcome="capped"}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test -p scopenode-rpc metrics_endpoint_exposes_prometheus_text
cargo test -p scopenode-rpc query_front_door_maps_capped_event_query
```

Expected: the metrics route test FAILS with 404 and the query metric assertion
FAILS.

- [ ] **Step 3: Instrument the shared query front door**

Time `execute_event_query` once. Map every return path to a bounded outcome:

```rust
fn response_outcome(response: &EventQueryResponse) -> (Outcome, usize) {
    match response {
        EventQueryResponse::NotIndexed => (Outcome::NotIndexed, 0),
        EventQueryResponse::MissingCoverage => (Outcome::MissingCoverage, 0),
        EventQueryResponse::TooManyResults { .. } => (Outcome::Capped, 0),
        EventQueryResponse::Empty => (Outcome::Empty, 0),
        EventQueryResponse::Results(rows) => (Outcome::Success, rows.len()),
    }
}
```

Record unsupported planning errors as `Outcome::Unsupported` and storage errors
as `Outcome::Error` plus `ErrorCategory::Query`. Add a `debug_span!` named
`event_query` containing only bounded plan facts: whether address/topic/ranges
are present, limit, and offset. Do not log their values.

- [ ] **Step 4: Add `/metrics`**

Add:

```rust
async fn get_metrics() -> impl IntoResponse {
    match scopenode_observability::global().encode() {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4",
            )],
            body,
        )
            .into_response(),
        Err(error) => {
            tracing::error!(err = %error, "Failed to encode Prometheus metrics");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "failed to encode metrics",
            )
                .into_response()
        }
    }
}
```

Add `.route("/metrics", get(get_metrics))` to the single shared router builder.
Refactor `start_rest_server` to call `build_rest_router(db)` so test and
production routes cannot drift.

- [ ] **Step 5: Add REST request observations**

At the start of each handler, create a request timer with its closed endpoint.
Finish it immediately before every known return with the domain outcome. For
`/events`, preserve `success`, `empty`, `not_indexed`, `missing_coverage`,
`capped`, `unsupported`, and `error`; the other handlers use `success` or
`error`.

```rust
let request = scopenode_observability::global()
    .start_request(Transport::Rest, Endpoint::Events);
```

On a successful result:

```rust
request.finish(Outcome::Success);
return Ok(Json(EventsResponse { events, count }));
```

On a known failure:

```rust
request.finish(Outcome::MissingCoverage);
return Err((
    axum::http::StatusCode::BAD_REQUEST,
    MISSING_COVERAGE_MESSAGE.into(),
));
```

Apply the same explicit pattern to each match arm. The timer's drop fallback
handles an unexpected `?` early return as `error`.

- [ ] **Step 6: Run RPC tests**

Run:

```bash
cargo test -p scopenode-rpc
cargo clippy -p scopenode-rpc --all-targets -- -D warnings
```

Expected: all tests PASS.

- [ ] **Step 7: Commit REST and query metrics**

```bash
git add Cargo.lock crates/scopenode-rpc/Cargo.toml crates/scopenode-rpc/src/query_front_door.rs crates/scopenode-rpc/src/rest.rs crates/scopenode-rpc/tests/parity.rs
git commit -m "Expose bounded REST and query metrics"
```

### Task 5: Instrument JSON-RPC Methods

**Files:**
- Modify: `crates/scopenode-rpc/src/server.rs`
- Test: `crates/scopenode-rpc/src/server.rs`

- [ ] **Step 1: Write a failing method metric test**

Add a Tokio test that creates a temporary DB, calls
`EthApiServer::block_number(&api)`, and compares the before/after global metric
count for:

```text
scopenode_requests_total{endpoint="block_number",outcome="success",transport="rpc"}
```

Use the same `metric_value` helper pattern from Task 3.

- [ ] **Step 2: Run the focused test to verify it fails**

Run:

```bash
cargo test -p scopenode-rpc server::tests::block_number_records_rpc_metrics
```

Expected: FAIL because JSON-RPC methods do not record metrics.

- [ ] **Step 3: Add a method guard helper**

In `server.rs`, add a private helper that starts an `rpc_request` span and calls
`global().start_request(Transport::Rpc, endpoint)`. Each method must call
`finish` exactly once on known return paths; the timer's drop fallback records
unexpected early returns as `error`.

Map methods to closed endpoints:

```rust
peer_count   -> Endpoint::PeerCount
get_logs     -> Endpoint::GetLogs
block_number -> Endpoint::BlockNumber
chain_id     -> Endpoint::ChainId
```

For `get_logs`, use the shared query response to distinguish `success`, `empty`,
`not_indexed`, `missing_coverage`, `capped`, `unsupported`, and `error`. Keep
client-visible JSON-RPC codes and messages unchanged.

- [ ] **Step 4: Run server and parity tests**

Run:

```bash
cargo test -p scopenode-rpc server
cargo test -p scopenode-rpc --test parity
cargo clippy -p scopenode-rpc --all-targets -- -D warnings
```

Expected: all commands PASS.

- [ ] **Step 5: Commit JSON-RPC instrumentation**

```bash
git add crates/scopenode-rpc/src/server.rs
git commit -m "Instrument JSON-RPC request performance"
```

### Task 6: Add Core Fixture And Real-Archive Benchmarks

**Files:**
- Modify: `crates/scopenode-core/Cargo.toml`
- Create: `crates/scopenode-core/benches/archive_pipeline.rs`

- [ ] **Step 1: Add the bench target and compile a skeleton**

Add:

```toml
[dev-dependencies]
criterion = { workspace = true }

[[bench]]
name = "archive_pipeline"
harness = false
```

Create a benchmark skeleton with:

```rust
use criterion::{criterion_group, criterion_main, Criterion};

fn archive_pipeline(c: &mut Criterion) {
    c.bench_function("fixture/source_scan", |b| {
        b.iter(|| {
            scopenode_core::source::Era1Source::scan(
                "../../fixtures/era1/mainnet",
                None,
                0,
                8191,
            )
            .unwrap()
        })
    });
}

criterion_group!(benches, archive_pipeline);
criterion_main!(benches);
```

- [ ] **Step 2: Compile the benchmark**

Run:

```bash
cargo bench -p scopenode-core --bench archive_pipeline --no-run
```

Expected: PASS. If the relative fixture path is resolved from the workspace
instead of the crate directory, replace it with
`PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/...")`.

- [ ] **Step 3: Add deterministic benchmark cases**

Add cases for:

- source scanning over block range `0..=8191`;
- decoding the first block from
  `fixtures/era1/mainnet/mainnet-00000-5ec1ffb8.era1`;
- receipt verification using the decoded first block;
- event extraction using a synthetic `Transfer(address,address,uint256)` ABI and
  a synthetic receipt/log, built once outside the timed closure.

Use `iter_batched` when setup clones mutable data and `Throughput::Elements(1)`
for single-block cases. Use `std::hint::black_box`, not the deprecated Criterion
re-export.

- [ ] **Step 4: Add opt-in real archive cases**

Read:

```rust
let real_archive = std::env::var_os("SCOPENODE_BENCH_ARCHIVE");
let real_dir = std::env::var_os("SCOPENODE_BENCH_ERA_DIR");
```

Register real cases only when the corresponding variable exists. For an archive
file, benchmark opening and decoding a bounded number of blocks, default 64,
configured by `SCOPENODE_BENCH_BLOCKS`. For a directory, read optional
`SCOPENODE_BENCH_FROM_BLOCK` and `SCOPENODE_BENCH_TO_BLOCK`; otherwise use
`0..=8191`. Print one concise `eprintln!` when a real case is skipped.

Do not infer that every real archive is ERA1: call the current public
`iter_era1_block_facts` API, which already dispatches ERA1/ERE by extension.

- [ ] **Step 5: Run a short benchmark smoke test**

Run:

```bash
cargo bench -p scopenode-core --bench archive_pipeline -- --sample-size 10 --measurement-time 1
```

Expected: deterministic cases complete; real cases are skipped unless their
environment variables are set.

- [ ] **Step 6: Commit core benchmarks**

```bash
git add Cargo.lock crates/scopenode-core/Cargo.toml crates/scopenode-core/benches/archive_pipeline.rs
git commit -m "Benchmark archive pipeline stages"
```

### Task 7: Add SQLite Storage And Query Benchmarks

**Files:**
- Modify: `crates/scopenode-storage/Cargo.toml`
- Create: `crates/scopenode-storage/benches/storage.rs`

- [ ] **Step 1: Add the bench target**

Add:

```toml
[dev-dependencies]
criterion = { workspace = true }

[[bench]]
name = "storage"
harness = false
```

- [ ] **Step 2: Write the async benchmark harness**

Use `Criterion::to_async(tokio::runtime::Runtime)` and create fresh temporary
databases outside timed query iterations. Build deterministic `StoredEvent`
vectors for sizes `1`, `100`, and `500`.

Benchmark names:

```text
storage/insert/1
storage/insert/100
storage/insert/500
query/empty
query/selective
query/capped
```

For insert benchmarks, use `iter_batched_async` so each iteration receives a
fresh DB and does not measure `INSERT OR IGNORE` against duplicate rows. For
queries, seed once and repeatedly call public `Db::query_events`.

- [ ] **Step 3: Compile the benchmark**

Run:

```bash
cargo bench -p scopenode-storage --bench storage --no-run
```

Expected: PASS.

- [ ] **Step 4: Run a short benchmark smoke test**

Run:

```bash
cargo bench -p scopenode-storage --bench storage -- --sample-size 10 --measurement-time 1
```

Expected: all six benchmark groups complete.

- [ ] **Step 5: Commit storage benchmarks**

```bash
git add Cargo.lock crates/scopenode-storage/Cargo.toml crates/scopenode-storage/benches/storage.rs
git commit -m "Benchmark SQLite event workloads"
```

### Task 8: Document Usage And Verify End To End

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Document logging**

Add CLI examples:

```bash
scopenode -v sync --config config.toml
RUST_LOG=scopenode_core=trace,scopenode_storage=debug scopenode sync
scopenode --log-format json -v serve
```

State that logs use stderr, normal command output uses stdout, and verbosity
defaults to `warn/info/debug/trace`.

- [ ] **Step 2: Document `/metrics` and label policy**

Add:

```bash
curl http://127.0.0.1:8546/metrics
```

Explain that metrics reset at process restart and intentionally exclude
contract addresses, paths, block numbers, event names, topics, and raw errors
from labels.

- [ ] **Step 3: Document local benchmarks**

Add:

```bash
cargo bench -p scopenode-core --bench archive_pipeline
cargo bench -p scopenode-storage --bench storage

SCOPENODE_BENCH_ARCHIVE=/path/to/archive.era1 \
  cargo bench -p scopenode-core --bench archive_pipeline

SCOPENODE_BENCH_ERA_DIR=/path/to/era-directory \
SCOPENODE_BENCH_FROM_BLOCK=12000000 \
SCOPENODE_BENCH_TO_BLOCK=12008191 \
  cargo bench -p scopenode-core --bench archive_pipeline

cargo bench -p scopenode-core --bench archive_pipeline -- --save-baseline before
cargo bench -p scopenode-core --bench archive_pipeline -- --baseline before
```

State explicitly that benchmarks are local-only and are not CI gates.

- [ ] **Step 4: Run full verification**

Run:

```bash
cargo fmt --all --check
cargo check --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo bench -p scopenode-core --bench archive_pipeline --no-run
cargo bench -p scopenode-storage --bench storage --no-run
git diff --check
```

Expected: every command PASS.

- [ ] **Step 5: Run the metrics endpoint manually**

Start the server with a valid local config:

```bash
cargo run -p scopenode -- -v serve --config config.toml
```

In another terminal:

```bash
curl -fsS http://127.0.0.1:8546/status
curl -fsS http://127.0.0.1:8546/metrics | rg 'scopenode_(requests|query)'
```

Expected: `/status` succeeds and `/metrics` contains request counters,
in-flight gauges, and histograms with only bounded labels.

- [ ] **Step 6: Verify JSON logs manually**

Run:

```bash
cargo run -p scopenode -- --log-format json -v status --config config.toml 2>&1 \
  | jq -c .
```

Expected: tracing lines parse as JSON. Plain command output remains on stdout;
when combining streams, exclude those lines or run with stdout redirected.

- [ ] **Step 7: Commit docs**

```bash
git add README.md
git commit -m "Document logs metrics and benchmarks"
```

### Task 9: Final Scope And Regression Review

**Files:**
- Review: all files changed by Tasks 1-8

- [ ] **Step 1: Check metric labels**

Run:

```bash
rg -n 'with_label_values|labels\\(|const_labels' crates
```

Expected: every label value originates from one of the closed enums in
`scopenode-observability`; no path, address, block, event, topic, or error string
is passed to Prometheus.

- [ ] **Step 2: Check sensitive/high-volume logs**

Run:

```bash
rg -n 'tracing::|info!|debug!|trace!|warn!|error!' crates
```

Review each new call. Expected: no ABI JSON, decoded payload, full query filter,
or per-block `debug`/`info` logs.

- [ ] **Step 3: Confirm user ERE work was preserved**

Run:

```bash
git diff main -- crates/scopenode-core/src/e2store.rs crates/scopenode-core/src/era1_codec.rs crates/scopenode-core/src/era1_reader.rs crates/scopenode-core/src/source.rs
```

Expected: ERE support remains present. Any changes to these files must be
limited to conflict resolution explicitly required by the benchmark API; the
preferred result is no observability-plan edits in them.

- [ ] **Step 4: Run final verification once more**

Run:

```bash
cargo fmt --all --check &&
cargo test --workspace &&
cargo clippy --workspace --all-targets -- -D warnings &&
git diff --check
```

Expected: PASS with no warnings.
