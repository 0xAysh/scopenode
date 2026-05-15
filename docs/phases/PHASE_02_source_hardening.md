> **Deprecated (2026-05-14):** This phase document is superseded by the architectural reset. See `docs/brainstorms/2026-05-14-architectural-reset-requirements.md`.

# Phase 2 - Source Hardening, Verification, and Tooling

## Goal

Make the EraE-backed MVP difficult to misuse.

Phase 1 proves the vertical slice. Phase 2 hardens source handling, verification
policy, ABI ergonomics, retry behavior, and operator tooling so scopenode can be
trusted for real local indexing workflows.

This phase is not about returning to devp2p as the historical data source.
Historical indexing still comes from local EraE files.

## What changes

| Area | Phase 1 | Phase 2 |
|---|---|---|
| Source coverage | Basic manifest | Persistent manifest with integrity/status details |
| Incomplete ranges | Fail loudly | Fail loudly with precise missing chunks/ranges |
| Verification | Header continuity + receipt roots | Stronger source, header, body, receipt, and transaction-root checks |
| ABI setup | Sourcify or local override | Better validation, proxy support, event discovery |
| Retry/debug | Minimal | Explicit retry and diagnosis commands |
| Query safety | In-scope checks | Coverage-aware errors with actionable status output |

## What we build

### Persistent source manifest

scopenode records what it found in the local EraE directory:

```text
source id
source path
file/chunk identity
covered block ranges
chain id or chain identity where available
integrity status
first indexed at
last scanned at
```

The manifest lets `status`, `doctor`, and future incremental indexing explain
which local history source produced the indexed data.

### Stronger verification policy

Phase 2 expands verification beyond the minimum needed for the MVP:

- Verify parent continuity across chunk boundaries.
- Verify block hash/header encoding where available.
- Verify receipt roots for all candidate blocks.
- Verify transaction roots when bodies are read.
- Record verification failures separately from missing coverage.
- Keep enough source metadata to explain whether a failure is source corruption,
  unsupported format, or an implementation bug.

### Coverage-aware status

`scopenode status` should answer:

```text
What sources are known?
Which block ranges are covered?
Which scopes are fully indexed?
Which scopes are partial or failed?
Which blocks failed verification?
What can the local JSON-RPC safely answer?
```

### Config validation

`scopenode validate config.toml` checks before indexing:

- EraE source path exists and is readable.
- Source appears to match the configured chain.
- Requested block ranges are covered or missing ranges are reported.
- Contract addresses are valid.
- ABI source is available.
- Requested event names exist in the ABI.
- `to_block >= from_block`.

Validation should not mutate indexing state.

### ABI and proxy ergonomics

Add tooling that helps users write correct scopes:

```bash
scopenode abi 0x<address>
scopenode validate config.toml
```

Proxy handling should support explicit implementation addresses first. Automatic
proxy detection can be added when the required state/source access is available,
but it is not allowed to introduce a hosted RPC dependency into the happy path.

### Debug and retry commands

Add operational commands for failed local indexing:

```bash
scopenode retry config.toml
scopenode doctor
```

`retry` re-runs failed verification/decode work after source or config fixes.
`doctor` reports source coverage, database integrity, failed blocks, ABI cache
state, and local serving readiness.

## Definition of done

- [ ] Source manifest persists source identity, block coverage, and scan status.
- [ ] Missing coverage errors name the exact missing block ranges.
- [ ] Verification failures are distinct from missing data and decode failures.
- [ ] Header continuity is checked across source chunk boundaries.
- [ ] Transaction roots are verified when bodies are read.
- [ ] `scopenode status` reports source coverage and scope index completeness.
- [ ] `scopenode validate` checks source path, coverage, ABI, event names, and ranges without mutating state.
- [ ] `scopenode abi 0x...` lists events with signatures and topic0 hashes.
- [ ] Explicit proxy implementation config is validated.
- [ ] `scopenode doctor` diagnoses source, DB, ABI, and failed-block state.
- [ ] `scopenode retry` can reprocess failed blocks after source/config fixes.
- [ ] Tests cover missing chunks, corrupt receipts, bad ABIs, unknown events, and coverage-aware query errors.
