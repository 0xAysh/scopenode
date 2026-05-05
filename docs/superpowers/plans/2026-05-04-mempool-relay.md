# Mempool Relay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Participate in Ethereum transaction gossip from boot so devp2p peers have a reason to stay connected during historical sync.

**Architecture:** Add `reth-transaction-pool` dependency. In `DevP2PNetwork::start()`, wire an `mpsc` channel into `NetworkManager` via `set_transactions()` before spawning it, then build a `Pool<MockTransactionValidator, CoinbaseTipOrdering, NoopBlobStore>` and spawn `TransactionsManager` — reth handles all gossip protocol details automatically.

**Tech Stack:** `reth-transaction-pool` v1.11.3 (same git tag as existing reth deps), `reth-network::transactions::TransactionsManager`, `tokio::sync::mpsc`

---

### Task 1: Add reth-transaction-pool dependency

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `crates/scopenode-core/Cargo.toml`

- [ ] **Step 1: Add to workspace**

In `Cargo.toml`, find the `[workspace.dependencies]` reth block and add:

```toml
reth-transaction-pool  = { git = "https://github.com/paradigmxyz/reth", tag = "v1.11.3", default-features = false }
```

Place it after the existing `reth-storage-api` line to keep the reth group together.

- [ ] **Step 2: Add to crate**

In `crates/scopenode-core/Cargo.toml`, add after `reth-storage-api`:

```toml
reth-transaction-pool  = { workspace = true }
```

- [ ] **Step 3: Verify it resolves**

```bash
cargo fetch
```

Expected: exits 0, no error. If it errors on a feature flag conflict, add `features = []` to the workspace entry.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/scopenode-core/Cargo.toml Cargo.lock
git commit -m "deps(core): add reth-transaction-pool for mempool relay"
```

---

### Task 2: Wire pool and TransactionsManager into DevP2PNetwork

**Files:**
- Modify: `crates/scopenode-core/src/network.rs`

- [ ] **Step 1: Add imports**

At the top of `crates/scopenode-core/src/network.rs`, find the `use tokio::sync::{oneshot, RwLock};` line and replace it with:

```rust
use tokio::sync::{mpsc, oneshot, RwLock};
```

Then add a new import block after the existing reth imports:

```rust
use reth_network::transactions::{TransactionsManager, TransactionsManagerConfig};
use reth_transaction_pool::{
    blobstore::NoopBlobStore,
    noop::MockTransactionValidator,
    ordering::CoinbaseTipOrdering,
    EthPooledTransaction, Pool, PoolConfig, SubPoolLimit,
};
```

- [ ] **Step 2: Add type alias**

After the existing `use` block and before `pub enum ReceiptFetchResult`, add:

```rust
/// Transaction pool type used for the mempool relay.
///
/// Accepts all transactions without validation (invalid ones are rejected by
/// receiving peers). Bounded to ~5k transactions total across sub-pools.
type RelayPool = Pool<
    MockTransactionValidator<EthPooledTransaction>,
    CoinbaseTipOrdering<EthPooledTransaction>,
    NoopBlobStore,
>;
```

- [ ] **Step 3: Add pool field to DevP2PNetwork**

Find the `pub struct DevP2PNetwork` definition and add the `pool` field:

```rust
pub struct DevP2PNetwork {
    #[allow(dead_code)]
    handle: NetworkHandle<EthNetworkPrimitives>,
    peers: Arc<RwLock<HashMap<reth_network_peers::PeerId, PeerSession>>>,
    blacklisted: Arc<RwLock<HashSet<reth_network_peers::PeerId>>>,
    /// Relay pool — held alive for the process lifetime so TransactionsManager
    /// can clone it. Never queried directly by sync logic.
    #[allow(dead_code)]
    pool: RelayPool,
}
```

- [ ] **Step 4: Wire set_transactions before spawning NetworkManager**

In `DevP2PNetwork::start()`, find:

```rust
let manager = NetworkManager::new(network_config)
    .await
    .map_err(|e| NetworkError::Boot(e.to_string()))?;

let handle = manager.handle().clone();
```

Replace with:

```rust
let mut manager = NetworkManager::new(network_config)
    .await
    .map_err(|e| NetworkError::Boot(e.to_string()))?;

// Wire the tx gossip channel BEFORE spawning NetworkManager.
// NetworkManager routes all incoming tx events (NewPooledTransactionHashes,
// GetPooledTransactions) to this channel. Without it, tx gossip is silently dropped
// and we appear as a non-participating node to peers.
let (tx_events_tx, tx_events_rx) = mpsc::unbounded_channel();
manager.set_transactions(tx_events_tx);

let handle = manager.handle().clone();
```

- [ ] **Step 5: Build pool and spawn TransactionsManager**

In `DevP2PNetwork::start()`, find `tokio::spawn(manager);` and after it add:

```rust
// Build relay pool: accepts all transactions without validation,
// tip-based ordering, no blob sidecar storage.
// Cap: ~2k pending + 1.5k basefee + 1.5k queued ≈ 5k total transactions.
let pool = Pool::new(
    MockTransactionValidator::default(),
    CoinbaseTipOrdering::default(),
    NoopBlobStore::default(),
    PoolConfig {
        pending_limit: SubPoolLimit { max_txs: 2000, max_size: 20 * 1024 * 1024 },
        basefee_limit: SubPoolLimit { max_txs: 1500, max_size: 15 * 1024 * 1024 },
        queued_limit:  SubPoolLimit { max_txs: 1500, max_size: 15 * 1024 * 1024 },
        ..PoolConfig::default()
    },
);

// TransactionsManager owns all tx gossip protocol logic:
// - Receives NewPooledTransactionHashes from peers
// - Fetches full transactions via GetPooledTransactions
// - Stores in pool and announces to other peers
// Runs for the process lifetime alongside NetworkManager.
let tx_manager = TransactionsManager::new(
    handle.clone(),
    pool.clone(),
    tx_events_rx,
    TransactionsManagerConfig::default(),
);
tokio::spawn(tx_manager);
```

- [ ] **Step 6: Store pool in the returned struct**

Find the `Ok(Self {` block at the end of `start()` and add the `pool` field:

```rust
Ok(Self {
    handle,
    peers,
    blacklisted: Arc::new(RwLock::new(HashSet::new())),
    pool,
})
```

- [ ] **Step 7: Build**

```bash
cargo build 2>&1 | tail -20
```

Expected: `Finished` with no errors. Common issues:
- If `SubPoolLimit` isn't in scope: try `reth_transaction_pool::pool::txpool::SubPoolLimit` or check what `PoolConfig` fields expect — may need `reth_transaction_pool::PoolConfig` default fields inspected
- If `TransactionsManager` path is wrong: try `reth_network::TransactionsManager` (some reth versions re-export at the crate root)
- If `MockTransactionValidator` isn't `Default`: use `MockTransactionValidator::no_propagate_local()` or `MockTransactionValidator { propagate_local: true, return_invalid: false, _marker: Default::default() }` — but note `_marker` is private, so use `default()` or the public constructor

- [ ] **Step 8: Run tests**

```bash
cargo test 2>&1 | tail -30
```

Expected: all existing tests pass. No new tests needed — the relay is a background behaviour with no observable interface to unit-test directly.

- [ ] **Step 9: Smoke test**

```bash
cargo build --release && ./target/release/sn sync config.test.toml
```

Watch for:
- `peers: N` where N > 0 in the status line (peers staying connected)
- Stage 1/3 header progress bar advancing past 0
- No "Header chunk fetch failed" within the first 90 seconds

- [ ] **Step 10: Commit**

```bash
git add crates/scopenode-core/src/network.rs
git commit -m "feat(network): relay mempool txs to maintain peer connections during sync"
```
