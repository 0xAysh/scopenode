# Efficiency Improvements Requirements

Created: 2026-05-21
Status: completed

Current implementation note: the result-cap behavior, topic lookup index, ABI
decoder precompilation, and single-pass multi-contract archive scan described
here are present in the current codebase. This document records the original
requirements; use `README.md` and `ONBOARDING.md` for current behavior.

## Problem frame

scopenode's current ERA1-backed indexer works, but several paths do repeated work or carry avoidable overhead. The user wants the previously identified efficiency improvements implemented so the project is easier to trust, faster to query, and more scalable for multiple contracts over the same ERA1 files.

## In scope

- Keep existing CLI and API behavior compatible unless the current behavior is clearly a bug.
- Make `cargo clippy --workspace --all-targets -- -D warnings` clean.
- Preserve REST and JSON-RPC result caps while distinguishing exactly-at-cap from over-cap result sets.
- Add SQLite support for the JSON-RPC hot path: contract + topic0 + block range.
- Remove the streaming-query memory leak.
- Avoid repeated per-log ABI type parsing during event decode.
- Replace per-contract ERA1 scanning with a single-pass multi-contract path for sync.
- Keep tests as the safety net for each behavior-bearing change.

## Out of scope

- Changing the product model away from local ERA1 files.
- Adding live devp2p, reorg handling, Sourcify fetching, or new public APIs.
- Rewriting storage format beyond indexes and safe query improvements.
- Aggressive dependency-feature trimming unless it is low-risk after runtime work is complete.

## Success criteria

- `cargo test --workspace` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- Query cap semantics are covered by tests.
- Streaming query code no longer leaks generated SQL strings.
- ABI decoder compiles reusable per-event decode metadata once per decoder.
- Sync scans overlapping ERA1 files once per `sync` run while indexing all configured contracts.
- Existing ERA1, storage, REST, and RPC tests remain green.

## Key decisions

- Prioritize correctness-preserving efficiency over speculative features.
- Keep public shape stable: `sync`, `serve`, `status`, REST, and JSON-RPC should not require users to change configs or clients.
- Treat multi-contract single-pass sync as the main architectural change; the rest are contained hardening/optimization tasks.
