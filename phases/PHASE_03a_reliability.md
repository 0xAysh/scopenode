# Phase 3a — Reliability (Production-Grade Core)

## Goal

Make scopenode production-grade. After this phase, live sync runs indefinitely,
reorgs are handled correctly, and operators have the tools to diagnose and fix
issues. This phase focuses exclusively on core reliability — no new APIs or
convenience features.

**What "done" looks like:** scopenode syncs historically, transitions to live
mode, handles a reorg without losing data, and `scopenode doctor` confirms
everything is healthy.

---

## Staging environment

The `--data-dir` / `SCOPENODE_DATA_DIR` setup from Phase 1 continues to apply.
Add a live-sync entry to `config.test.toml` for testing:

```toml
# config.test.toml — live sync test contract (low volume)
[[contracts]]
name = "Uniswap V3 ETH/USDC (live test)"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events = ["Swap"]
from_block = 17000000
# no to_block = live sync
```

To test reorg handling safely against staging: snapshot before the test,
inject a synthetic reorg via the DB (flip a block hash), verify detection
and recovery, then restore.

```bash
cp ~/.scopenode-staging/scopenode.db ~/.scopenode-staging/pre-reorg-test.db.snap
# ... run reorg test ...
cp ~/.scopenode-staging/pre-reorg-test.db.snap ~/.scopenode-staging/scopenode.db
```

---

## What we build

1. **Live sync** — watch new blocks after historical sync completes
2. **Reorg handling** — detect chain splits, soft-delete affected events
3. **`scopenode snapshot` / `scopenode restore`** — proper CLI for DB snapshots
4. **`scopenode doctor`** — diagnose peers, beacon health, DB integrity
5. **`scopenode retry`** — re-fetch all `pending_retry` blocks
6. **Full progress TUI refinement** — peer count, events found, speed, source breakdown

---

## Concepts to understand deeply

### Chain reorganizations

A reorg happens when two valid blocks are produced at the same height and one
branch eventually wins. Example:

```
... → block 100 → block 101A → block 102A  ← our stored chain
                ↘ block 101B → block 102B → block 103B  ← new longer chain
```

Post-Merge reorg safety:
- After 1 epoch (~6.4 min, ~32 blocks): very unlikely to reorg
- After 2 epochs (~12.8 min, ~64 blocks): **cryptographically finalized** by
  the beacon chain. Cannot be reorged without breaking PoS security.

We keep a rolling buffer of the last 64 block hashes. On each new block:
1. Check `new_block.parent_hash == our_tip_hash`
2. If not: reorg detected. Walk back via `parent_hash` until common ancestor.
3. Mark events from orphaned blocks as `reorged = 1` (never hard-delete).
4. Re-process the winning chain's blocks.

---

## Implementation

### Live sync

```rust
// crates/scopenode-core/src/sync/live.rs

pub struct LiveSyncer {
    beacon: Arc<BeaconHeaderSource>,
    pipeline: Arc<Pipeline>,
    contracts: Vec<ContractConfig>,
    live_events_tx: broadcast::Sender<LiveEvent>,
}

impl LiveSyncer {
    pub async fn start(self) -> LiveSyncHandle {
        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = tokio::spawn(self.run(stop_rx));
        LiveSyncHandle { handle, stop: stop_tx }
    }

    async fn run(mut self, mut stop: oneshot::Receiver<()>) {
        let mut new_heads = self.beacon.subscribe_new_heads().await;
        let mut reorg = ReorgDetector::new();

        loop {
            tokio::select! {
                Some(header) = new_heads.recv() => {
                    if let Err(e) = self.process(&mut reorg, header).await {
                        tracing::error!(err = %e, "Live sync error");
                    }
                }
                _ = &mut stop => {
                    tracing::info!("Live sync stopped");
                    break;
                }
            }
        }
    }

    async fn process(&self, reorg: &mut ReorgDetector, header: ScopeHeader)
        -> Result<()>
    {
        match reorg.check(&header, &self.pipeline.db).await? {
            ReorgStatus::Ok => self.process_block(&header).await?,
            ReorgStatus::Reorg { orphaned, new_chain } => {
                let hashes: Vec<_> = orphaned.iter().map(|h| format!("{h:?}")).collect();
                let n = self.pipeline.db.events().mark_reorged(&hashes).await?;
                tracing::warn!(orphaned = orphaned.len(), reorged_events = n, "Reorg handled");

                for block in new_chain {
                    self.process_block(&block).await?;
                }
            }
        }
        Ok(())
    }

    async fn process_block(&self, header: &ScopeHeader) -> Result<()> {
        for contract in &self.contracts {
            let targets = self.pipeline.targets_for(contract);
            if !targets.iter().any(|t| t.matches(&header.logs_bloom)) {
                continue;
            }

            let fetch = self.pipeline.fetch_and_verify(header).await?;
            let decoded = self.pipeline.decode(&fetch, contract).await?;
            self.pipeline.db.events().insert_events(&decoded, fetch.source).await?;

            for event in &decoded {
                let _ = self.live_events_tx.send(LiveEvent::from(event));
            }
        }
        Ok(())
    }
}
```

### Reorg detector

```rust
// crates/scopenode-core/src/sync/reorg.rs

const FINALITY_DEPTH: usize = 64;

pub struct ReorgDetector {
    /// Rolling buffer of (block_number, block_hash) for last 64 blocks
    recent: VecDeque<(u64, B256)>,
}

impl ReorgDetector {
    pub async fn check(&mut self, new: &ScopeHeader, db: &Db)
        -> Result<ReorgStatus>
    {
        if let Some(&(_, tip_hash)) = self.recent.back() {
            if new.parent_hash != tip_hash {
                let orphaned: Vec<B256> = self.recent.iter()
                    .rev()
                    .take_while(|(_, h)| *h != new.parent_hash)
                    .map(|(_, h)| *h)
                    .collect();

                let new_chain = db.headers()
                    .get_chain_from(new.parent_hash, orphaned.len())
                    .await?;

                let orphan_count = orphaned.len();
                for _ in 0..orphan_count { self.recent.pop_back(); }
                self.push(new);

                return Ok(ReorgStatus::Reorg { orphaned, new_chain });
            }
        }

        self.push(new);
        Ok(ReorgStatus::Ok)
    }

    fn push(&mut self, h: &ScopeHeader) {
        if self.recent.len() >= FINALITY_DEPTH {
            self.recent.pop_front();
        }
        self.recent.push_back((h.number, h.hash));
    }
}

pub enum ReorgStatus {
    Ok,
    Reorg { orphaned: Vec<B256>, new_chain: Vec<ScopeHeader> },
}
```

### `scopenode snapshot` / `scopenode restore` commands

Promote the manual `cp` workflow from Phase 1 into proper CLI commands.
These are thin wrappers — no special format, just a timestamped copy of
the SQLite file.

```rust
// crates/scopenode/src/commands/snapshot.rs

pub async fn snapshot(data_dir: &Path, label: Option<String>) -> Result<()> {
    let db_path = data_dir.join("scopenode.db");
    if !db_path.exists() {
        return Err(anyhow::anyhow!("No database at {}", db_path.display()));
    }

    let label = label.unwrap_or_else(|| {
        chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
    });
    let snap_path = data_dir.join(format!("scopenode.{label}.snap"));

    std::fs::copy(&db_path, &snap_path)?;
    println!("Snapshot saved: {}", snap_path.display());
    Ok(())
}

pub async fn restore(data_dir: &Path, label: Option<String>) -> Result<()> {
    let snaps = list_snapshots(data_dir)?;

    let snap_path = if let Some(label) = label {
        data_dir.join(format!("scopenode.{label}.snap"))
    } else {
        // No label — show list and prompt
        if snaps.is_empty() {
            println!("No snapshots found in {}", data_dir.display());
            return Ok(());
        }
        println!("Available snapshots:");
        for (i, s) in snaps.iter().enumerate() {
            println!("  [{i}] {}", s.display());
        }
        // prompt user to pick one
        let idx: usize = dialoguer::Select::new()
            .with_prompt("Restore which snapshot?")
            .items(&snaps.iter().map(|p| p.display().to_string()).collect::<Vec<_>>())
            .interact()?;
        snaps[idx].clone()
    };

    if !snap_path.exists() {
        return Err(anyhow::anyhow!("Snapshot not found: {}", snap_path.display()));
    }

    let db_path = data_dir.join("scopenode.db");
    // Auto-snapshot current DB before overwriting
    if db_path.exists() {
        let auto = data_dir.join("scopenode.before_restore.snap");
        std::fs::copy(&db_path, &auto)?;
        println!("Auto-snapshot of current DB: {}", auto.display());
    }

    std::fs::copy(&snap_path, &db_path)?;
    println!("Restored from: {}", snap_path.display());
    Ok(())
}

fn list_snapshots(data_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut snaps: Vec<_> = std::fs::read_dir(data_dir)?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".snap"))
        .map(|e| e.path())
        .collect();
    snaps.sort();
    Ok(snaps)
}
```

CLI additions:

```rust
// Add to Command enum in cli.rs

/// Save a snapshot of the current database
Snapshot {
    /// Optional label (default: timestamp)
    #[arg(long)]
    label: Option<String>,
},
/// Restore database from a snapshot
Restore {
    /// Snapshot label to restore (omit to pick interactively)
    #[arg(long)]
    label: Option<String>,
},
```

---

### `scopenode doctor` command

```rust
// crates/scopenode/src/commands/doctor.rs

use console::style;

pub async fn run() -> Result<()> {
    println!("Portal Network connectivity");
    match test_portal_connectivity().await {
        Ok((discovered, responsive, test_ms)) => {
            println!("  Peers discovered:   {discovered}");
            println!("  Peers responsive:   {responsive} / {discovered}");
            println!("  Receipt fetch test: {} ({test_ms}ms)",
                style("✓").green());
        }
        Err(e) => println!("  {} {e}", style("✗").red()),
    }

    println!("\nBeacon chain");
    match test_beacon_connectivity().await {
        Ok((period, validators, block)) => {
            println!("  Sync committee:     period {period}, {validators}/512 validators signing");
            println!("  Latest header:      block {block}");
        }
        Err(e) => println!("  {} {e}", style("✗").red()),
    }

    println!("\nDatabase");
    let db = Db::open_default().await?;
    let path = db.path();
    let size = db.size_bytes().await?;
    let pending_retry = db.count_pending_retry().await?;
    let integrity = db.check_integrity().await?;
    println!("  Path:       {}", path.display());
    println!("  Size:       {}", format_bytes(size));
    println!("  Integrity:  {}", if integrity { style("✓").green() } else { style("✗ FAIL").red() });
    if pending_retry > 0 {
        println!("  Pending retry: {pending_retry} blocks (run `scopenode retry`)");
    }

    println!("\nERA1 archives");
    match check_era1_dir().await {
        Some((dir, count, range)) => {
            println!("  Directory:  {}", dir.display());
            println!("  Files:      {count} archives covering blocks {range}");
        }
        None => println!("  Not configured (set era1_dir in config)"),
    }

    println!("\nNetwork");
    println!("  Portal bootstrap:  {}", check_icon(test_portal_bootstrap().await.is_ok()));
    println!("  Etherscan API:     {}", check_icon(test_etherscan().await.is_ok()));

    Ok(())
}

fn check_icon(ok: bool) -> impl std::fmt::Display {
    if ok { style("✓ connected").green() } else { style("✗ failed").red() }
}
```

### `scopenode retry` command

```rust
// crates/scopenode/src/commands/retry.rs

pub async fn run() -> Result<()> {
    let db = Db::open_default().await?;
    let pending = db.get_pending_retry_blocks().await?;

    if pending.is_empty() {
        println!("No blocks pending retry.");
        return Ok(());
    }

    println!("Retrying {} blocks...\n", pending.len());

    let fetcher = ReceiptFetcher::from_config(&config)?;
    let mut succeeded = 0;
    let mut failed = 0;

    for (block_num, block_hash, contract) in &pending {
        match fetcher.fetch(*block_num, *block_hash).await {
            Ok((receipts, source)) => {
                let header = db.get_header(*block_num).await?
                    .ok_or_else(|| anyhow::anyhow!("Header {} missing", block_num))?;

                if verify_receipts(&receipts, header.receipts_root, *block_num).is_ok() {
                    let decoded = decoder.extract_and_decode(&receipts, *block_num, *block_hash);
                    db.insert_events(&decoded, source.as_str()).await?;
                    db.mark_fetched(*block_num, contract).await?;
                    succeeded += 1;
                } else {
                    tracing::warn!(block = block_num, "Verification still failing");
                    failed += 1;
                }
            }
            Err(e) => {
                tracing::warn!(block = block_num, err = %e, "Retry failed");
                failed += 1;
            }
        }
    }

    println!("\nRetry complete: {succeeded} succeeded, {failed} still pending.");
    Ok(())
}
```

---

## `scopenode status` output (updated for 3a)

```
Contracts indexed:
  Uniswap V3 ETH/USDC (0x8ad599c3...)
    Events: Swap (12,439)  Mint (844)  Burn (601)
    Range:  block 17000000 → 17555000 (fully synced)
    Live:   watching (last block: 19001234, 3s ago)
    DB:     142 MB

  Aave V3 (0x87870Bca3F...)
    Events: Supply (3,211)  Withdraw (1,844)
    Range:  block 16291127 → live
    Status: syncing... (78% — ETA 42min)

Sources: 38,412 blocks from Portal (94%)  |  2,100 from ERA1 (5%)  |  419 from RPC (1%)
Node:  localhost:8545 (JSON-RPC)
Peers: 6 Portal Network peers
DB:    ~/.scopenode/data.db (286 MB)
```

---

## Full progress TUI

```
scopenode — Uniswap V3 ETH/USDC (0x8ad599c3...)

Stage 1/3  Header sync      ████████████████████  555,000 / 555,000   done
Stage 2/3  Bloom scan       ████████████████████  555,000 / 555,000   done (48,231 hits)
Stage 3/3  Receipt fetch    ████████░░░░░░░░░░░░   21,847 / 41,000    53%

Peers: 5 active  |  Speed: 14.2 receipts/sec  |  ETA: 1h 23m
Sources: Portal 18,200 (83%)  |  ERA1 3,400 (16%)  |  RPC 247 (1%)
Events found so far: 12,439 Swap  |  844 Mint  |  601 Burn
Failed blocks (pending retry): 3
```

---

## Dependency additions

```toml
# Add to workspace (Phase 3a only)
console = "0.15"        # styled terminal output (doctor, retry)
```

---

## Tests

```
Unit:
  - ReorgDetector detects single-block reorg correctly
  - ReorgDetector: no reorg on normal chain extension
  - ReorgDetector: handles multi-block reorg (3 blocks deep)
  - ReorgDetector: buffer never exceeds FINALITY_DEPTH
  - LiveEvent serializes to valid JSON

Integration (--ignored):
  - Live sync receives and processes at least 1 new block
  - Reorg: insert events for block A, mark reorged, verify hidden from queries
  - scopenode retry re-fetches pending blocks
  - scopenode doctor completes without error on healthy setup
```

---

## Definition of done

- [ ] Live sync runs indefinitely in background after historical sync
- [ ] Reorg detected and handled: orphaned events marked `reorged = 1`, hidden
  from all query interfaces
- [ ] `scopenode snapshot` saves a timestamped copy of the DB
- [ ] `scopenode restore` restores from a snapshot (auto-snapshots current DB first)
- [ ] `scopenode doctor` shows portal, beacon, ERA1, DB, and network status
- [ ] `scopenode retry` re-fetches all `pending_retry` blocks
- [ ] Progress TUI shows source breakdown (Portal / ERA1 / RPC)
- [ ] All previous phase tests still pass

---

## What you learn in this phase

**Reorgs:** Why they happen, post-Merge finality guarantees, the parent_hash
chain, common ancestor algorithm, why soft-delete is the right pattern.

**Async patterns:** `tokio::select!` for shutdown signals, `oneshot` channel
for one-time stop, structured concurrency in long-running background tasks,
`broadcast` channel for fan-out to multiple consumers.

**Production operations:** The `doctor` command as a debugging affordance —
giving operators visibility into peer health, DB integrity, and data source
coverage. Retry semantics and idempotent re-processing.
