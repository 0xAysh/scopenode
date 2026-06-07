# Ere Support Implementation Plan

**Status:** completed and retained as a historical implementation plan.

The current source stack discovers `.ere` files, parses dynamic block indexes,
assembles sectioned block tuples, decodes slim receipts, and routes ERA1 and ERE
through the same block-fact pipeline. Unchecked boxes below are historical, not
an active task list.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Teach scopenode to read standard EraE/Ere `.ere` execution-history files while preserving existing `.era1` support.

**Architecture:** Keep scopenode's public pipeline shape intact and add Ere handling at the source/reader boundary. Reuse proven ideas from geth `internal/era/execdb` and reth `crates/era/src/ere`: `.ere` filename profiles, dynamic block index parsing, sectioned entry assembly, and slim receipt decoding.

**Tech Stack:** Rust 2021, existing Alloy consensus/RLP/primitives crates, existing `snap` framed Snappy, existing e2store reader code.

---

### Task 1: Source Discovery

**Files:**
- Modify: `crates/scopenode-core/src/source.rs`
- Test: `crates/scopenode-core/src/source.rs`

- [ ] Add failing tests for parsing `mainnet-00000-4bb7de2e.ere` and `mainnet-00000-4bb7de2e-noproofs.ere`.
- [ ] Implement extension-aware parsing that accepts `.era1` and `.ere`, with `.ere` allowing one or more profile postfixes after the 8-character short block hash.
- [ ] Keep `era_dir` config unchanged and stamp `.ere` manifests as `ere`.

### Task 2: Dynamic Index

**Files:**
- Modify: `crates/scopenode-core/src/e2store.rs`
- Test: `crates/scopenode-core/src/source.rs`

- [ ] Add failing tests for Ere `DynamicBlockIndex` entry type `[0x67, 0x32]`.
- [ ] Parse `starting-number | offsets... | component-count | count` with component count constrained to 2 through 5.
- [ ] Return file-index coverage from `starting-number` through `starting-number + count - 1`.

### Task 3: Ere Block Tuples

**Files:**
- Modify: `crates/scopenode-core/src/e2store.rs`
- Modify: `crates/scopenode-core/src/era1_reader.rs`
- Test: `crates/scopenode-core/src/source.rs`

- [ ] Add failing tests for iterating `.ere` files whose entries are grouped by type: all headers, all bodies, all slim receipts, then dynamic index.
- [ ] Build Ere tuples by reading the file once, bucketing entries by type, validating counts, and assigning block numbers from the dynamic index.
- [ ] Reject or skip `.ere` files that use the `noreceipts` profile because scopenode cannot index events without receipts.

### Task 4: Slim Receipts

**Files:**
- Modify: `crates/scopenode-core/src/era1_codec.rs`
- Test: `crates/scopenode-core/src/source.rs`

- [ ] Add failing tests for decoding `CompressedSlimReceipts` as RLP list of `[tx-type, status, cumulative-gas, logs]`.
- [ ] Reconstruct `ReceiptWithBloom<Receipt<PrimitiveLog>>` from slim receipts by recomputing the bloom from logs.
- [ ] Feed reconstructed `ReceiptEnvelope` values into the existing ABI decoder and receipt-root verifier.

### Task 5: Verification

**Files:**
- Modify as needed in docs and user-facing strings.

- [ ] Run `cargo test -p scopenode-core source::tests`.
- [ ] Run `cargo test -p scopenode-core`.
- [ ] Run `cargo test`.
