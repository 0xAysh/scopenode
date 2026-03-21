# Phase 3 — Live Sync + Reliability

## Goal

scopenode runs indefinitely. After historical sync completes it watches new
blocks in real time, verified by the Helios beacon light client. Reorgs are
detected and handled without data loss. The TUI is rebuilt in ratatui for a
rich, real-time operator view.

---

## What changes

| | Phase 2 | Phase 3 |
|---|---|---|
| Live blocks | Not supported (`to_block` required) | Automatic after historical sync |
| Header verification (live) | N/A | Helios beacon — BLS-verified by sync committee |
| Reorgs | Not handled | Detected via `parent_hash`; events soft-deleted |
| TUI | indicatif progress bars | ratatui — multi-panel, real-time |
| Operations | Manual | `scopenode snapshot/restore/doctor/retry` |

---

## What we build

### Helios beacon light client

Provides cryptographically verified headers for live sync. Helios uses the
Altair light client protocol:
- Bootstraps from a recent finalized checkpoint
- Verifies each new header via BLS signature of 512-validator sync committee
- 512 validators must collude to deceive us

Multiple `consensus_rpc` endpoints configured — all must agree on sync
committee updates before accepting. Disagreement halts live sync and alerts.

```toml
[node]
consensus_rpc = [
    "https://www.lightclientdata.org",
    "https://sync-mainnet.beaconcha.in",
]
```

### Live sync

When `to_block` is absent from config: historical sync runs, then live sync
starts automatically. Each new Helios-verified header goes through the same
bloom → receipt fetch → MPT verify → decode → store pipeline as historical.

A `broadcast` channel fans live events to all consumers (REST SSE, webhooks
in Phase 4).

### Reorg handling

Rolling buffer of last 64 block hashes. On each new block:
1. Check `new_block.parent_hash == our_tip_hash`
2. Mismatch → walk back via `parent_hash` to common ancestor
3. Mark orphaned events `reorged = 1` — never hard-delete
4. Re-process winning chain blocks through the pipeline

Post-Merge finality: after ~64 blocks (~12.8 min), the beacon chain
cryptographically finalizes the block. Reorgs beyond that depth are
impossible without breaking PoS security.

### ratatui TUI

Replaces indicatif bars with a full-screen terminal UI:

```
┌─ scopenode ──────────────────────────────────────────────────────────┐
│ Mode: LIVE  Block: 19,842,301  Speed: 142 blocks/s  Peers: 12       │
├─────────────────────────────┬────────────────────────────────────────┤
│ Contracts                   │ Recent Events                          │
│                             │                                        │
│ Uniswap V3 ETH/USDC         │ 19842301  Swap  -1.2 ETH / +3841 USDC │
│   Swap          12,431 evts │ 19842298  Swap  +0.5 ETH / -1920 USDC │
│   Mint             482 evts │ 19842291  Mint  tick -200..200         │
│                             │ 19842280  Swap  -4.0 ETH / +12203 USDC│
├─────────────────────────────┴────────────────────────────────────────┤
│ Peers  ████████████░░░░░░░░  12/25    Reorgs: 0   Retries: 3        │
│ Sync   ████████████████████  historical complete — live              │
└──────────────────────────────────────────────────────────────────────┘
```

Key bindings: `q` quit · `p` peer list · `r` recent events · `l` logs

### New commands

- `scopenode snapshot [--label <name>]` — copy DB to `data_dir/snapshots/`
- `scopenode restore [--label <name>]` — restore from snapshot (auto-snaps current first)
- `scopenode doctor` — peers connected, beacon head, DB integrity, pending_retry count
- `scopenode retry` — re-fetches all `pending_retry` blocks through the full pipeline

---

## Definition of done

- [ ] Live sync starts automatically when `to_block` not set in config
- [ ] Live headers verified by Helios (BLS sync committee — not trusted from peer)
- [ ] Multiple `consensus_rpc` must agree; disagreement halts live sync with clear error
- [ ] Reorg detected (parent_hash mismatch): orphaned events marked `reorged = 1`
- [ ] `eth_getLogs` excludes reorged events by default
- [ ] ratatui TUI: mode, block, speed, peer count, per-contract event totals, recent events
- [ ] TUI key bindings work: quit, peer list, logs
- [ ] `scopenode snapshot` / `scopenode restore` work; restore auto-snaps before overwriting
- [ ] `scopenode doctor` reports: peer count, beacon head, DB counts, pending_retry count
- [ ] `scopenode retry` clears all `pending_retry` blocks on success
- [ ] Unit tests: reorg detection at depth 1/5/64, live syncer broadcast fan-out
- [ ] Integration test: live sync processes 10 real blocks after historical completes

## New dependencies

```toml
helios-client = { version = "=0.8", features = ["ethereum"] }
ratatui       = "0.28"
crossterm     = "0.28"    # ratatui backend
```
