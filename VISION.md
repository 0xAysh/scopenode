# scopenode — Vision & Goals

## What this is

A custom Ethereum node that sits between a light node and a full node.

Not a full node (doesn't store all state, doesn't execute every transaction).
Not a light node (doesn't blindly trust peers, doesn't rely on centralized RPCs).

You tell it what you care about. It fetches exactly that from the Ethereum P2P
network, proves it's real using cryptographic verification, and serves it locally
at localhost:8545.

Zero Infura. Zero Alchemy. Zero rate limits. Zero ongoing cost.
Zero trust.

---

## The problem it solves

Developers who need Ethereum data have three options today:

1. Run a full node — 1TB+ storage, weeks to sync, expensive to maintain
2. Use Infura/Alchemy — rate limits, costs money, you trust a third party,
   their data can be wrong, they can go down
3. Use The Graph — complex setup (AssemblyScript subgraphs), costs GRT tokens,
   you trust decentralized indexers, takes hours to index

None of these are good for a solo dev or small team who just needs the events
from one or two contracts to build their thing.

scopenode is the fourth option:

- Specify what contracts and events you want in plain English
- It syncs exactly that from the P2P network, verified trustlessly
- Runs on your laptop
- Takes minutes (for small/niche contracts) to hours (for Uniswap-scale)
- Serves standard JSON-RPC on localhost:8545 — any Ethereum library works
  without code changes

---

## Who it's for

Small projects and solo developers who:
- Want specific blockchain data (DeFi analytics, NFT tracking, DAO tooling, etc.)
- Don't want to set up The Graph or pay for API keys
- Don't want to run a full node
- Want to just run one command and start building

Example users:
- DeFi dev who wants 2 years of Uniswap swap history for an analytics dashboard
- NFT project tracking all mints and transfers for their collection
- DAO building a governance history tool
- Developer building a personal portfolio tracker
- Academic researcher studying MEV or liquidity patterns
- Security researcher monitoring specific contracts for anomalies
- Anyone at a hackathon who needs blockchain data fast

---

## How it works (the pipeline)

### Input — TOML config

Write a TOML config file describing what you want:

```toml
[node]
port = 8545

[[contracts]]
name = "Uniswap V3 ETH/USDC"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events = ["Swap", "Mint", "Burn"]
from = "17000000"
to = "17555000"
```

### Step 1 — Header Sync

Download block headers for the requested range from the Ethereum P2P network.
Headers are verified trustlessly via the beacon chain sync committee — a set of
512 validators that sign each block. No RPC provider needed.

Each header contains:
- `logsBloom` — a 2048-bit bloom filter fingerprinting every contract that
  emitted an event in this block
- `receiptsRoot` — a Merkle root that cryptographically commits to all receipts
  in the block
- `transactionsRoot` — same for transactions (used in wallet/EOA mode)

Headers for 555,000 blocks ≈ 280MB. Fast to download.

### Step 2 — Bloom Filter Scan

The `logsBloom` in each header lets us ask: "did contract 0x8ad... emit anything
in this block?" in microseconds, with no network call.

We scan all downloaded headers locally. Output: a list of block hashes where
our target contract *might* have emitted events. Pure CPU work.

Bloom filters have false positives, never false negatives — we might fetch some
blocks that turn out empty, but we never miss a block that matters.

### Step 3 — Receipt Fetching (contract mode)

For each block that passed the bloom scan, request full receipts from devp2p
peers — full nodes on the main Ethereum P2P network via the ETH wire protocol
(`GetReceipts` message).

This is the slow step. A block's receipts contain the result of every
transaction: gas used, success/fail, and every log (event) emitted.

For wallet/EOA mode: fetch block bodies instead (the transaction list), look for
transactions where `from` or `to` matches the target address.

### Step 4 — Merkle Verification

How do we know the peer didn't send us fake receipts?

We verify them against the `receiptsRoot` in the block header (which came from
the beacon chain — trustless).

Process:
1. Take all received receipts for a block
2. RLP-encode each one
3. Build a Merkle Patricia Trie from them
4. Compute the trie root hash
5. Assert: computed root == header.receiptsRoot

If they match — the receipts are real, guaranteed by math. If they don't —
discard, try a different peer.

This is the core of why scopenode doesn't require trusting anyone.

### Step 5 — Event Extraction

Receipts verified. Now scan all logs inside them. Keep only:
- Logs from the target contract address
- With the requested event topic hash (keccak256 of the event signature)
- Discard everything else

### Step 6 — ABI Decoding + Storage

Raw log data is ABI-encoded binary. Decode it using the ABI fetched from
Sourcify into named, typed fields. Store in SQLite with full context
(block number, tx hash, decoded fields).

### Step 7 — JSON-RPC Server

Serve a standard Ethereum JSON-RPC interface on localhost:8545.

In-scope calls (contracts we've indexed): answered instantly from SQLite.
Out-of-scope calls: could be proxied to a public endpoint in future.

Any Ethereum library works without code changes:
```js
const logs = await client.getLogs({ address: "0x8ad...", event: swapAbi })
// → answered from local SQLite, instant, free
```

### Step 8 — Live Sync

After historical sync completes, continue watching for new blocks. For each new
block: bloom scan → fetch receipts → verify → extract → store. Runs in the
background forever.

---

## Architecture diagram

```
TOML config
         |
         v
   [Sourcify]  <-- fetch ABI once (event signatures, param types)
         |
         v
+--------+--------+
|   Header Sync   |  <-- Beacon sync committee (P2P, trustless)
|   (all blocks   |
|    in range)    |
+--------+--------+
         |
         v
+--------+--------+
|  Bloom Scanner  |  <-- local CPU, milliseconds
|  (find matches) |
+--------+--------+
         |
         v
+--------+--------+
| Receipt Fetcher |  <-- devp2p (ETH wire protocol)
|  (matching      |
|   blocks only)  |
+--------+--------+
         |
         v
+--------+--------+
| Merkle Verifier |  <-- receiptsRoot proof (trust nobody)
|                 |
+--------+--------+
         |
         v
+--------+--------+
|  Log Extractor  |  <-- filter + ABI decode
|  + SQLite store |
+--------+--------+
         |
         v
+--------+--------+
| JSON-RPC Server |  <-- localhost:8545
|                 |
+--------+--------+
         |
         v
   your app (viem / ethers.js / web3.py / alloy / anything)
```

---

## Developer Experience

The pipeline being correct is table stakes. The DX is what makes people
actually use this instead of just signing up for Alchemy.

### First-run experience

Install is a single command:
```bash
cargo install scopenode
```

Write a `config.toml` describing the contracts and events you want, then run:
```bash
scopenode sync config.toml
```

---

### CLI commands

```bash
scopenode sync config.toml             # start sync (resumes if interrupted)
scopenode sync config.toml --dry-run   # bloom scan only, no receipt fetch
scopenode status                       # what's indexed, sync progress, peer count
scopenode query [options]              # query local data from terminal
scopenode export [options]             # export data to csv/json
scopenode validate config.toml        # check config before committing to a sync
scopenode abi 0x8ad599c3...            # show available events for a contract
scopenode retry                        # retry blocks that failed receipt fetch
scopenode doctor                       # diagnose connectivity, peer health, db integrity
```

---

### Dry run — know what you're getting into before you commit

```bash
scopenode sync config.toml --dry-run
```

Runs bloom scan across all headers in the range (fast, local, CPU only).
No receipts fetched. Output:

```
Dry run complete for Uniswap V3 ETH/USDC (0x8ad599c3...)
  Block range:     17000000 → 17555000 (555,000 blocks)
  Bloom matches:   48,231 blocks (8.7% hit rate)
  False positives: ~15% estimated (bloom filter)
  Net fetches:     ~41,000 receipt fetches needed
  Estimated time:  2.1 – 3.4 hours (devp2p, 4 peers)
  Estimated events: unknown until fetched

Start sync? [Y/n]
```

This is the single most important DX feature. A user starting a 3-hour sync
without knowing it's 3 hours is a user who closes the terminal and never comes back.

---

### Progress visibility during sync

Not a spinner. Real numbers.

```
scopenode — Uniswap V3 ETH/USDC (0x8ad599c3...)

Stage 1/3  Header sync      ████████████████████  555,000 / 555,000   done
Stage 2/3  Bloom scan       ████████████████████  555,000 / 555,000   done (48,231 hits)
Stage 3/3  Receipt fetch    ████████░░░░░░░░░░░░   21,847 / 41,000    53%

Peers: 5 active  |  Speed: 14.2 receipts/sec  |  ETA: 1h 23m
Events found so far: 12,439 Swap  |  844 Mint  |  601 Burn
Failed blocks (pending retry): 3
```

Updates every second. On `--quiet`, only final summary. On `--verbose`, per-block logs.

---

### Resumable sync — Ctrl+C safe

Sync progress is stored in SQLite. If interrupted:

```bash
^C
Interrupted. Progress saved. Run `scopenode sync config.toml` to resume.

scopenode sync config.toml
Resuming from block 17321847 (58% complete)...
```

Never restarts from zero. Bloom scan results are cached. Only un-fetched
receipt blocks are retried.

---

### `scopenode query` — use data without writing code

```bash
# print to terminal as table
scopenode query --contract 0x8ad... --event Swap --limit 20

# filter by decoded field
scopenode query --contract 0x8ad... --event Swap --where "amount0 > 1000000"

# output formats
scopenode query --contract 0x8ad... --event Swap --output json
scopenode query --contract 0x8ad... --event Swap --output csv > swaps.csv
```

Terminal table output:
```
block     tx_hash    sender      amount0      amount1      sqrtPrice...
────────  ─────────  ──────────  ───────────  ───────────  ────────────
17000021  0xabc...   0x123...    -1000000000  500000       1234567...
17000034  0xdef...   0x456...    2000000000   -998000      1234621...
...
Showing 20 of 12,439 results
```

CSV and JSON output makes this immediately useful for data science / analytics
workflows (pandas, dbt, etc.) without any code.

---

### HTTP REST API + SSE stream (alongside JSON-RPC)

JSON-RPC at `:8545` for Ethereum library compatibility (viem, ethers, web3.py).

REST API at `:8546` for everything else:

```
GET  /events?contract=0x8ad...&event=Swap&fromBlock=17000000&limit=100
GET  /events?contract=0x8ad...&event=Swap&where=amount0>0&orderBy=block_number
GET  /status                   → sync state, peer count, indexed contracts
GET  /contracts                → list of all indexed contracts
GET  /abi/0x8ad...             → ABI for a contract
```

Server-Sent Events for live streaming — no polling:

```
GET  /stream/events?contract=0x8ad...&event=Swap
```

```js
const source = new EventSource('http://localhost:8546/stream/events?contract=0x8ad...')
source.onmessage = (e) => console.log(JSON.parse(e.data))
// fires in real-time as new events arrive
```

This makes scopenode usable from any language (Python, Go, shell scripts) without
an Ethereum library.

---

### `scopenode status` — know what's indexed

```
scopenode status

Contracts indexed:
  Uniswap V3 ETH/USDC (0x8ad599c3...)
    Events: Swap (12,439)  Mint (844)  Burn (601)
    Range:  block 17000000 → 17555000 (fully synced)
    Live:   watching (last block: 19001234, 3s ago)
    DB:     142 MB

  Aave V3 (0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2)
    Events: Supply (3,211)  Withdraw (1,844)
    Range:  block 16291127 → live
    Status: syncing... (78% — ETA 42min)

Node:  localhost:8545 (JSON-RPC)  |  localhost:8546 (REST)
Peers: 6 devp2p peers
DB:    /Users/you/.scopenode/data.db (286 MB)
```

---

### `scopenode validate` — catch config errors before a long sync

```bash
scopenode validate config.toml

✓ Contract address format valid: 0x8ad599c3...
✓ Proxy detected → using implementation ABI (0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640)
✓ ABI fetched from Sourcify (4 events: Swap, Mint, Burn, Flash)
✓ Requested events found in ABI: Swap ✓  Mint ✓  Burn ✓
✓ Block range valid: 17000000 → 17555000
✗ from_block 17000000 is before Uniswap V3 deploy (block 12369621) — range is valid but will return no events before 12369621
! fallback_rpc not set — devp2p peer failures will leave blocks as pending_retry

Config looks good. Run `scopenode sync config.toml` to start.
```

---

### `scopenode doctor` — diagnose issues

```bash
scopenode doctor

devp2p connectivity
  Peers discovered:   12
  Peers responsive:   9 / 12
  Receipt fetch test: ✓ (block 17000001, 340ms)

Beacon chain
  Sync committee:     current period, 432/512 validators signing
  Latest header:      block 19001300

Database
  Path:               /Users/you/.scopenode/data.db
  Size:               286 MB
  Integrity:          ✓
  Pending retry:      3 blocks

Network
  devp2p bootstrap:   ✓ connected
  Sourcify API:       ✓ reachable
  Fallback RPC:       not configured

All systems operational.
```

---

### Good error messages — no cryptic hex dumps

Bad:
```
Error: 0x00000000000000000000000000000000 != 0xabcdef1234...
thread 'main' panicked at src/verify.rs:142
```

Good:
```
Verification failed for block 17000423.
  Expected receiptsRoot: 0xabcdef...
  Got from peer 192.168.1.4: 0x000000...
  → Peer sent invalid data. Trying 2 more peers.

[peer 192.168.1.7] Verification succeeded. Continuing.
```

Every error tells the user: what happened, why, and what scopenode is doing about it.
If user action is required, say exactly what command to run.

---

## vs alternatives

| | scopenode | The Graph | Infura/Alchemy | Full node |
|---|---|---|---|---|
| Setup | One command | Subgraph in AssemblyScript, deploy | Sign up, get API key | Weeks to sync |
| Trust | Cryptographically verified | Trust indexers | Trust the provider | Trustless |
| Cost | Free | Pay GRT tokens | Rate limits / paid tiers | Hardware cost |
| Data | Exactly what you specify | Full subgraph | Everything | Everything |
| Latency | Local (instant) | External API | External API | Local |
| Rate limits | None | Yes | Yes | None |

---

## What we build in (rough order)

Phase 1 — MVP (RPC-backed, correct pipeline):
1. Block header parsing — using `alloy` types and RLP support.
2. Bloom filter scan — using `alloy`'s bloom filter from parsed headers.
3. Parallel receipt fetching — `buffer_unordered(32)` + batch consecutive blocks.
4. Merkle verification — verify `receiptsRoot` proofs using `alloy`'s trie utilities.
5. ABI decoding — decode event logs using `alloy-sol-types` / `alloy-dyn-abi`.
6. SQLite storage layer — store verified events with `reorged`, `source`, `block_hash` columns.
7. JSON-RPC server — serve data at `localhost:8545`.
8. Progress TUI — real numbers, ETA, pipeline stages.
9. Resumable sync — store cursor in SQLite, resume on restart.
10. `--dry-run` — bloom scan only, report estimate before committing to full sync.
11. `scopenode status` — show what's indexed, sync state, DB size.
12. `scopenode query` — query local data from terminal.

Phase 2 — Trustless (no API keys required):
13. Beacon light client (live sync only) — Helios, multiple consensus endpoints, required agreement before accepting headers.
14. devp2p networking — `reth-network` + `reth-eth-wire` for `GetReceipts` from mainnet peers.
15. ERA1 archive support — local flat-file fallback for historical blocks devp2p peers can't serve.
16. Fallback RPC — optional last resort, still Merkle-verified.
17. Proxy contract detector — EIP-1967 storage slot check, ABI redirect.
18. `scopenode validate` — catch config errors before a long sync starts.
19. `scopenode abi` — show available events for a contract.

Phase 3a — Reliability (production-grade core):
20. Live sync — watch new blocks in real-time after historical sync completes.
21. Reorg handler — detect chain splits, soft-invalidate affected events.
22. `scopenode doctor` — diagnose peers, beacon health, DB integrity.
23. `scopenode retry` — re-fetch all pending_retry blocks.
24. Full progress TUI refinement — peer count, events found, speed.

Phase 3b — Developer surface area (additive features):
25. REST API at `:8546` — `GET /events`, `GET /status`, `GET /stream/events` (SSE).
26. `eth_subscribe` WebSocket — live event subscriptions for Viem/ethers.js.
27. `scopenode export` — CSV/JSON output.

---

## Why Rust

- Dominant language in the Ethereum client ecosystem (reth, lighthouse, alloy
  are all Rust)
- Performance matters for bloom scanning millions of headers
- Memory safety matters when parsing untrusted binary data from P2P peers
- Best ecosystem for Ethereum tooling — `alloy`, `helios`, `lighthouse`, `trin`
  are all Rust and actively maintained

---

## Libraries we use

The goal is to ship a working, correct pipeline fast — not to reinvent the wheel.
We use reliable, battle-tested libraries for all lower-level primitives.

| Component | Library | Notes |
|---|---|---|
| Primitive types (B256, Address, U256) | `alloy-primitives` | Standard across the Rust Ethereum ecosystem |
| RLP encoding/decoding | `alloy-rlp` | Used by reth, well-tested |
| Block header / receipt types | `alloy` | Full Ethereum type system |
| ABI decoding | `alloy-dyn-abi` / `alloy-sol-types` | Decode event logs into named fields |
| Bloom filter | `alloy` (via `Header::logs_bloom`) | Already in the header type |
| Merkle Patricia Trie | `alloy-trie` | Verify `receiptsRoot` proofs |
| Beacon light client | `helios` | Trustless header sync via sync committee (live sync only) |
| devp2p networking | `reth-network` + `reth-eth-wire` + `reth-discv4` | devp2p peer management + ETH wire protocol + peer discovery |
| Keccak256 / BLS | `sha3`, `lighthouse` BLS crates | Never roll your own crypto |
| SQLite | `sqlx` with `sqlite` feature | Async, compile-time checked queries |
| JSON-RPC server | `jsonrpsee` | Standard in the Rust Ethereum ecosystem |
| Async runtime | `tokio` | Standard |
| CLI | `clap` | Argument parsing |
| TUI progress | `indicatif` | Progress bars and spinners |

---

## Reorg handling

During live sync, every new block: assert `block.parent_hash == our_stored_tip`.

If they don't match, a reorg has occurred:
1. Walk back via `parent_hash` links until we find the common ancestor between
   our chain and the new chain
2. Mark all events from the forked blocks as `reorged = true` in SQLite
   (soft delete — never hard delete, useful for debugging)
3. Re-sync forward from the common ancestor on the new chain

Post-Merge safety: anything older than 2 epochs (~12.8 minutes, 64 blocks) is
finalized and cryptographically cannot be reorged. We only need to handle reorgs
within the last 64 blocks during live sync.

SQLite schema implication: every event row gets a `reorged BOOLEAN DEFAULT 0`
column and a `block_hash TEXT` column so we can invalidate by block.

---

## Proxy contract handling

Many contracts are proxies (EIP-1967, EIP-1822, OpenZeppelin TransparentProxy).
The proxy address is what emits events, but the ABI lives at the implementation.

Detection:
- Read EIP-1967 storage slot `0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc`
  from the proxy address at any recent block
- If non-zero, that 32-byte value (last 20 bytes) is the implementation address
- Fetch the implementation's ABI from Sourcify instead

Config override (for non-standard proxies or contracts not on Sourcify):
```toml
[[contracts]]
address = "0x..."
abi_override = "./abis/MyContract.json"   # local file, bypasses Sourcify
```

Warn the user in the CLI when a proxy is detected and which implementation ABI
is being used.

---

## Data source fallback chain

devp2p peer availability is uneven for older historical data — non-archive
nodes prune old receipts. We don't paper over this — we handle it with a
layered fallback chain. Every source feeds into the same Merkle verification
step.

Strategy (tried in order per block):
1. devp2p peers (mainnet full nodes, ETH wire protocol) — retry up to 3 different peers
2. ERA1 archives — if `era1_dir` is configured and the block falls within a
   locally available archive file
3. `fallback_rpc` — if configured, used as last resort
4. If all fail: log a warning, record the block as `pending_retry`

### ERA1 archives

ERA1 files are flat-file archives of historical Ethereum data published by the
Ethereum Foundation. Each file covers ~8192 blocks and contains headers, block
bodies, and receipts. The files are checksummed and available via HTTP mirrors,
BitTorrent, and IPFS.

ERA1 solves the biggest gap in devp2p coverage: old historical data. Non-archive
devp2p peers prune old receipts, but an ERA1 file downloaded once covers that
range forever. Fully offline after download, no peers needed.

Verification is identical: extract receipts from the ERA1 file, rebuild the
Merkle Patricia Trie, check root against the header's `receiptsRoot`. Same
math, different transport.

```toml
[node]
port = 8545
era1_dir = "~/.scopenode/era1"  # optional, local ERA1 archive directory
fallback_rpc = "https://eth-mainnet.g.alchemy.com/v2/YOUR_KEY"  # optional, last resort
```

Every receipt is tagged with its source: `source = "devp2p"`, `source = "era1"`,
or `source = "rpc"`. The `scopenode status` output shows the breakdown:

```
Sources: 38,412 blocks from devp2p (94%)  |  2,100 from ERA1 (5%)  |  419 from RPC (1%)
```

The verification step (Merkle check) runs regardless of source — we verify
everything. The goal: never silently miss events. Either verify + store, or
loudly fail.

---

## Goal

Build a working, useful scoped Ethereum node as fast as possible using reliable
libraries — while deeply understanding every aspect of what we're building.

We don't reinvent the wheel, but we never use something as a black box either.
For every library we use, we understand what it's doing under the hood: why RLP
encodes the way it does, how bloom filters work mathematically, what the beacon
sync committee is actually signing, how Merkle Patricia Tries prove data integrity.

Ship the pipeline. Learn by doing. Use great tools. Understand everything.
