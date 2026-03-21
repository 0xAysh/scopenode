# Phase 2 — Hardened P2P + Tooling

## Goal

Make the devp2p layer production-grade and add operator tooling. After this
phase, bad peers are automatically retried and blacklisted, proxy contracts
resolve without manual config, and `scopenode validate` catches problems
before a sync ever runs.

**Trust model (unchanged):** verification IS the trust mechanism. A peer
cannot lie — if its data fails MPT verification, it gets blacklisted and
the next peer is tried. No multi-peer agreement, no fallback RPC, no
alternative data sources. All data comes from devp2p peers.

---

## What changes

| | Phase 1 | Phase 2 |
|---|---|---|
| Bad peer handling | Logs warning, continues | Blacklisted; next peer tried automatically |
| Peer pool | Fixed set from discv4 | Rolling pool — stale/bad peers rotated out |
| Proxy contracts | Manual `abi_override` required | EIP-1967 auto-detected via devp2p |
| Pre-flight check | None | `scopenode validate config.toml` |

---

## What we build

### Peer manager

Maintains a pool of connected peers. On MPT verification failure or
unresponsive peer:
1. Blacklist the peer for the session (don't reconnect)
2. Pull a fresh peer from the discovery pool
3. Retry the request transparently

Peers are scored by response rate and verification pass rate. Lowest-scoring
peers are rotated out as better peers are discovered.

No new deps required — `reth-network` already exposes the peer management
APIs used in Phase 1.

### EIP-1967 proxy detection

At sync setup, check the implementation slot:
```
slot = keccak256("eip1967.proxy.implementation") - 1
     = 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc
```
Fetch via `eth_getStorageAt` from a devp2p peer using the ETH wire protocol.
If non-zero → use implementation address for Sourcify ABI lookup. Result is
cached in SQLite after first resolution.

If detection fails (peer doesn't serve it): warn and require `abi_override`.

### `scopenode validate config.toml`

Checks before syncing (no devp2p connection needed):
- Contract on Sourcify (or `abi_override` set and readable)
- All listed event names exist in the ABI
- `impl_address`, if set, resolves to an ABI on Sourcify

Prints a per-contract ✓/✗ report. Safe to run without modifying any state.
Exits with code 1 if any check fails — usable in CI.

### `scopenode abi 0x<address>`

Fetches from Sourcify, prints every event with its full signature and
topic0 hash. Useful for building configs and debugging.

```
Event: Swap
  Signature : Swap(address,address,int256,int256,uint160,uint128,int24)
  Topic0    : 0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67
  Parameters:
    address       sender      [indexed]
    address       recipient   [indexed]
    int256        amount0
    int256        amount1
    uint160       sqrtPriceX96
    uint128       liquidity
    int24         tick
```

---

## Definition of done

- [x] MPT verification failure → peer blacklisted immediately, next peer tried automatically
- [x] Blacklisted peers skipped for all subsequent requests in the session
- [ ] Sync never stalls due to a single bad peer (needs integration test)
- [x] `impl_address` in config → ABI fetched from implementation address on Sourcify
- [x] Contract not on Sourcify + no `abi_override` → clear error with exact fix instructions
- [x] `scopenode validate config.toml` — per-contract report: ABI ✓, events ✓, proxy info
- [x] `scopenode abi 0x...` — lists all events with signatures and topic0 hashes
- [ ] Unit tests: peer blacklist logic, Sourcify parse edge cases
- [ ] Integration test: sync recovers after a peer returns bad receipts
