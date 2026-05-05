# Mempool Relay — Requirements

**Date:** 2026-05-04
**Status:** Ready for planning

---

## Problem

scopenode uses `NoopProvider`, which responds to all peer data requests with empty
responses. devp2p peers penalise nodes that don't reciprocate and disconnect within
seconds of the ETH Status handshake. This causes the `peers` HashMap to stay empty
during historical sync, making header and receipt fetching fail repeatedly.

The fix: participate in Ethereum transaction gossip as a relay node. We have no
historical data to offer, but we can relay mempool transactions — giving peers a
reason to maintain connections with us.

---

## Goal

Keep devp2p peers connected long enough for historical sync (headers + receipts)
to succeed. Mempool participation is purely instrumental — it is not exposed to
users via JSON-RPC or any other interface.

---

## Approach

Use reth's `TransactionsManager` with a thin `TransactionPool` implementation
(`BoundedTxPool`) backed by an LRU cache. `TransactionsManager` handles all
gossip protocol details natively — receiving `NewPooledTransactionHashes`
announcements, fetching full transactions from peers, and announcing back.

---

## Requirements

### BoundedTxPool (`crates/scopenode-core/src/txpool.rs`)

- New file implementing reth's `TransactionPool` trait
- Internal storage: `Arc<RwLock<LruCache<TxHash, Arc<PooledTransactionsElement>>>>` with capacity 5000
- LRU eviction: oldest transactions dropped automatically when capacity is reached
- No transaction validation — accept all incoming transactions; invalid ones will
  be rejected by receiving peers downstream
- No persistence — pool is in-memory only, cleared on restart
- Trait implementation split into three tiers:
  - **Real:** `add_transactions`, `contains_transaction`, `get_transactions_by_hashes`,
    `pooled_transaction_hashes_max`
  - **Events:** `new_transactions_listener`, `transaction_event_listener` — backed
    by broadcast channel so `TransactionsManager` can subscribe and announce new txs
  - **Stubs:** all block-building methods (`best_transactions`, `pending_transactions`,
    blob methods, etc.) — return empty iterators, empty vecs, or `None`; never panic
- Must implement `Clone` (required by `TransactionPool`)

### TransactionsManager integration (`crates/scopenode-core/src/network.rs`)

- `DevP2PNetwork` gains one new field: `pool: BoundedTxPool`
- In `DevP2PNetwork::start()`, after spawning `NetworkManager`:
  1. Build `BoundedTxPool` (capacity 5000)
  2. Build `TransactionsManager` using pool + `NetworkHandle` + `TransactionsManagerConfig::default()`
  3. `tokio::spawn(transactions_manager)` — runs for the process lifetime
- Pool and `TransactionsManager` must be booted **before** `wait_for_peers` — so
  gossip participation begins the moment the first peer completes the ETH handshake
- All existing sync logic (`wait_for_peers`, `get_headers`, `get_receipts_for_blocks`,
  `peers` HashMap) is unchanged

### Dependencies

- Add `reth-transaction-pool` to `crates/scopenode-core/Cargo.toml`

---

## Out of Scope

- Exposing mempool data via JSON-RPC (`eth_getTransactionByHash`, pending tx queries)
- Transaction submission (`eth_sendRawTransaction`)
- Blob transaction support
- Transaction validation or ordering
- Persistent mempool across restarts
- Mempool size metrics or monitoring

---

## Success Criteria

- Peers remain in the `peers` HashMap for the duration of historical sync
- `sn sync` completes header and receipt fetching without "Header chunk failed" errors
  on a network where peers are available
- No user-visible change in behaviour or output
