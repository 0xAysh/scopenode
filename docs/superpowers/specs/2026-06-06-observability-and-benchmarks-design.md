# Observability and Benchmarks Design

## Goal

Add enough observability to diagnose correctness and performance problems in both
the archive sync pipeline and the serving path without making normal output
noisy or introducing high-cardinality metrics.

The feature has three parts:

1. structured `tracing` logs and spans;
2. Prometheus metrics exposed by the existing REST server at `GET /metrics`;
3. local Criterion benchmarks using repository fixtures and optional real
   ERA1/ERE archives.

Persistent log files, an OpenTelemetry collector, and CI benchmark execution are
out of scope.

## Logging

The CLI keeps human-readable terminal logs as the default. A global
`--log-format json` option selects newline-delimited JSON logs. The existing
verbosity mapping remains:

| Flag | Default filter |
|---|---|
| no `-v` | `warn` |
| `-v` | `info` |
| `-vv` | `debug` |
| `-vvv` or more | `trace` |

`RUST_LOG` continues to override the default filter. Logs are written to stderr,
leaving command output and machine-readable stdout usable independently.

Instrumentation levels are:

- `info`: command and server lifecycle, completed sync summaries, and material
  failures;
- `debug`: selected archive files, per-file summaries, storage batches, query
  plans and outcomes;
- `trace`: per-block and other high-volume diagnostic details.

Spans carry useful diagnostic fields such as command, archive format, file path,
block range, block number, contract address, endpoint, result count, and elapsed
time. ABI contents, decoded event payloads, and complete request filters are not
logged.

The primary spans are:

- `sync_run`
- `source_scan`
- `archive_file`
- `block_pipeline`
- `store_batch`
- `event_query`
- `http_request`
- `rpc_request`

Failures record the operation, a bounded error category, relevant span context,
and elapsed time. Logging failures must not change command, pipeline, or request
behavior.

## Metrics

The REST router exposes Prometheus text format at `GET /metrics` on the existing
REST bind address and port. Metrics are process-local and reset on restart. The
endpoint requires no database access and remains available when the database has
no indexed events.

A small observability module owns metric registration and recording. Core,
storage, and RPC code call narrow recording functions instead of depending on
the Prometheus implementation directly. Recording is infallible from caller
code: duplicate registration is prevented during initialization, and metric
updates cannot fail sync or requests.

### Label Policy

Allowed labels are bounded enums:

- `operation`
- `format`
- `endpoint`
- `transport`
- `outcome`
- `error_category`

Paths, block numbers, contract addresses, event names, topic hashes, and raw
error strings are forbidden as labels. They may appear in tracing spans.

### Sync Metrics

- `scopenode_sync_runs_total{outcome}`
- `scopenode_sync_duration_seconds`
- `scopenode_source_scan_duration_seconds`
- `scopenode_archive_files_total{format,outcome}`
- `scopenode_archive_file_duration_seconds{format}`
- `scopenode_blocks_processed_total{format}`
- `scopenode_blocks_failed_total{format,error_category}`
- `scopenode_bloom_matches_total`
- `scopenode_receipt_verifications_total{outcome}`
- `scopenode_receipt_verification_duration_seconds`
- `scopenode_events_decoded_total`
- `scopenode_events_stored_total`
- `scopenode_store_batches_total{outcome}`
- `scopenode_store_batch_size`
- `scopenode_store_duration_seconds`

Archive reading currently decodes a block tuple into block facts as one
operation. Metrics initially measure that real boundary rather than inventing
separate header, body, and receipt timers that the current APIs cannot measure
accurately. Bloom, verification, decode, and storage are recorded separately
where the pipeline already has clear boundaries.

### Serving Metrics

- `scopenode_requests_total{transport,endpoint,outcome}`
- `scopenode_request_duration_seconds{transport,endpoint,outcome}`
- `scopenode_requests_in_flight{transport,endpoint}`
- `scopenode_query_rows`
- `scopenode_query_duration_seconds{outcome}`
- `scopenode_query_errors_total{error_category}`

REST endpoints use route templates such as `/events` and `/abi/:address`, never
raw paths. JSON-RPC uses bounded method names implemented by scopenode. Outcomes
include `success`, `empty`, `not_indexed`, `missing_coverage`, `capped`,
`unsupported`, and `error`.

Request timing starts at the transport boundary and includes query planning,
SQLite work, and response projection. Query timing separately measures the
shared event-query front door so REST and JSON-RPC can be compared.

## Instrumentation Flow

### Sync

1. The CLI starts a `sync_run` span and records total duration and outcome.
2. Source discovery records scan duration and selected file count.
3. Each archive file gets an `archive_file` span and a completion summary.
4. Each successfully decoded block increments the processed counter.
5. Bloom matches, receipt verification, event decoding, and store batches record
   their own counts and durations at existing code boundaries.
6. Read, verification, and storage failures retain current recovery semantics
   while adding categorized logs and counters.
7. The final log summarizes files, blocks, failures, decoded events, stored
   events, and elapsed time.

### Serving

1. REST middleware and JSON-RPC method implementations create request spans and
   maintain in-flight gauges.
2. The shared event-query front door records query duration and domain outcome.
3. Transport adapters record response outcome and row count after projection.
4. Internal errors retain their current client-facing response while logs carry
   detailed error context.

## Benchmarks

Criterion benchmarks are local-only and are not added to normal CI. Benchmark
code lives in each owning crate's `benches/` directory so it can exercise public
APIs without widening production interfaces solely for benchmarking.

Deterministic repository-fixture benchmarks cover:

- archive source scanning;
- archive block-fact decoding;
- receipt-root verification;
- event extraction and ABI decoding;
- SQLite insertion at representative batch sizes;
- indexed event queries for empty, selective, and capped-result shapes.

Real-data benchmarks are opt-in through environment variables:

- `SCOPENODE_BENCH_ARCHIVE` points to an ERA1 or ERE file;
- `SCOPENODE_BENCH_ERA_DIR` points to an archive directory for source scanning;
- optional block-range variables may narrow expensive real-data runs.

When real-data variables are absent, those benchmark cases report that they were
skipped rather than failing. Real-data results are machine-specific and are not
treated as portable pass/fail thresholds.

Documentation provides commands for:

- running all deterministic benchmarks;
- enabling real archive benchmarks;
- saving and comparing Criterion baselines;
- using release builds and a quiet machine for useful measurements.

## Error Handling

Observability is diagnostic and must not become a new failure mode.

- Metrics initialization happens once during process startup.
- Metric recording functions do not return application errors.
- `/metrics` encoding failures return HTTP 500 and emit an error log without
  affecting other endpoints.
- Instrumentation preserves existing decisions to skip corrupt blocks, suppress
  coverage after incomplete runs, and surface storage/query failures.
- Timings use monotonic elapsed time rather than wall-clock timestamps.

## Testing

Focused tests verify:

- CLI parsing selects human or JSON format while preserving verbosity;
- the metrics registry starts with expected metric families;
- representative recording calls appear in Prometheus output;
- `GET /metrics` returns Prometheus content from the REST router;
- request outcomes and pipeline failures increment the expected bounded labels;
- instrumentation does not change sync output, query outcomes, or recovery
  behavior;
- label values cannot accidentally include addresses, paths, block numbers, or
  arbitrary error messages.

Existing pipeline, storage, REST, and JSON-RPC tests remain the primary
correctness suite. Criterion results are measurements, not correctness tests.

## Documentation

The README will document:

- `--log-format human|json`;
- `RUST_LOG` and verbosity examples;
- the `/metrics` endpoint and metric stability expectations;
- local fixture and real-archive benchmark commands;
- the rule that metrics labels are intentionally low-cardinality.

## Acceptance Criteria

1. Both sync and serving paths emit contextual human-readable logs by default
   and valid JSON logs when requested.
2. `GET /metrics` exposes bounded Prometheus counters, gauges, and histograms for
   sync stages and API requests.
3. A slow or failing archive file, storage batch, or event query can be located
   from logs and correlated with a metric operation/outcome.
4. No metric uses contract addresses, paths, block numbers, event names, topic
   hashes, or raw errors as labels.
5. Fixture benchmarks and opt-in real ERA1/ERE benchmarks run locally with
   documented commands.
6. All existing tests pass, and new observability tests prove that
   instrumentation preserves behavior.
