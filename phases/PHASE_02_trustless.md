# Phase 2 — Trustless (Beacon Light Client + Portal Network)

## Goal

Replace the Phase 1 RPC dependencies with trustless P2P sources. After this
phase, scopenode runs without any API key or centralized provider. Headers come
from the beacon chain (verified by BLS signatures of 512 validators). Receipts
come from the Portal Network (verified against the Merkle root in the header).

This phase also adds proxy contract detection and the `scopenode validate` /
`scopenode abi` commands.

---

## What changes from Phase 1

| Component | Phase 1 | Phase 2 |
|---|---|---|
| Header source | `alloy` provider (public RPC) | `helios` beacon light client |
| Receipt source | `alloy` provider (public RPC) | Portal Network (`trin` crates) |
| Fallback | None | `fallback_rpc` in config (still Merkle-verified) |
| ABI source | Etherscan direct | Etherscan + proxy detection |

Everything else — bloom scan, Merkle verification, ABI decoding, SQLite
storage, JSON-RPC server — is unchanged.

---

## Concepts to understand deeply

### The beacon chain and why it enables trustless light clients

Before the Merge (Sep 2022): Ethereum used proof-of-work. Light clients had to
verify PoW hashes — doable but slow and trust-limited.

After the Merge: Ethereum uses proof-of-stake via the beacon chain. Every ~12
seconds, a validator proposes a block and a sync committee of 512 randomly
selected validators signs it with BLS signatures.

The key insight: to verify a block header, you only need:
1. The aggregated BLS public key of the current sync committee (64 bytes)
2. Their 96-byte aggregated BLS signature over the block header
3. A Merkle proof that this sync committee is the legitimate one

This is the Ethereum light client protocol (EIP-3526 / Altair). 512 validators
must collude to deceive you — essentially the security of the entire network.

### BLS signatures and aggregation

BLS (Boneh-Lynn-Shacham) signatures have a unique property: multiple signatures
can be **aggregated** into a single 96-byte signature. Verifying one aggregated
signature over N messages is nearly as fast as verifying a single signature.

This is why sync committees work: 512 individual BLS signatures → 1 aggregated
signature → fast verification by a light client.

We use `helios` which uses the `blst` crate (the reference BLS implementation
used by all major Ethereum clients). Never implement BLS yourself.

### Helios — the Ethereum light client

`helios` (by a16z) implements the Altair light client protocol in Rust. It:
1. **Bootstraps** from a trusted checkpoint (a recent finalized block hash,
   published widely and verifiable from multiple sources)
2. **Syncs** sync committee updates forward (each update is ~1KB, covering ~27 hours)
3. **Verifies** every new block header using the current sync committee BLS signature
4. **Exposes** an alloy-compatible provider interface

Checkpoint sync: instead of syncing every block since genesis, we start from a
recent trusted checkpoint. The sync period is ~27 hours (256 epochs × 32 slots).
From the checkpoint, we need only ~1KB per period to stay current.

Free public checkpoint providers:
- `https://sync-mainnet.beaconcha.in`
- `https://mainnet.checkpoint.sigp.io`

```toml
# config.toml additions
[node]
# Public beacon chain API for light client sync
consensus_rpc = "https://www.lightclientdata.org"
```

### Portal Network — decentralized historical data

The Portal Network is a DHT (Distributed Hash Table) built on top of `discv5`
(the same peer discovery protocol used by Ethereum nodes). Its purpose: serve
historical Ethereum data (headers, bodies, receipts) without each node needing
the full history.

Three sub-protocols, we care about **History Network**:
- Keys: content keys derived from block hashes
- Values: RLP-encoded headers, block bodies, or receipts
- Distribution: each node stores a slice of history (radius-based)

Content key format for block receipts: `[0x02] || block_hash` (33 bytes)

The Portal Network is built by the Ethereum Foundation Portal team and
implemented in Rust by `trin`.

### discv5 — how peers are found

`discv5` is a Kademlia-based peer discovery protocol. It:
1. Bootstraps from well-known bootnode ENRs (Ethereum Node Records — signed
   identity documents containing IP, port, protocol info)
2. Builds a routing table of peers organized by XOR distance
3. Finds content by querying peers closest to the content ID (`sha256(content_key)`)

We use the `discv5` crate (the same one used by Lighthouse, Lodestar, Trin).

### Fallback strategy

Portal Network peer availability is uneven, especially for old data. Our
strategy:
1. Try Portal Network (up to 3 different peers)
2. If all fail: mark `pending_retry`
3. If `fallback_rpc` is configured: use it as last resort

Critically: receipts from the fallback RPC still go through Merkle verification.
We never trust any source — we verify all receipts against `receipts_root`.

### Proxy contracts (EIP-1967)

Many major contracts are proxies:
- USDC: proxy → implementation
- Aave V3: proxy → implementation
- Compound: proxy → implementation

The proxy emits events but the ABI lives at the implementation address.

EIP-1967 storage slot (standard proxy slot):
```
keccak256("eip1967.proxy.implementation") - 1
= 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc
```

Read this slot via `eth_getStorageAt(proxy_address, slot)`. If non-zero, the
last 20 bytes are the implementation address. Fetch the implementation's ABI.

---

## Implementation

### Helios integration

```rust
// crates/scopenode-core/src/headers.rs (updated)

use helios_client::{Client, ClientBuilder, networks::Network};
use alloy_primitives::B256;

pub struct BeaconHeaderSource {
    client: Client<helios_consensus::ethereum::EthereumConsensus>,
}

impl BeaconHeaderSource {
    pub async fn new(consensus_rpc: &str, execution_rpc: &str) -> Result<Self, HeaderError> {
        let mut client = ClientBuilder::new()
            .network(Network::Mainnet)
            .consensus_rpc(consensus_rpc)
            .execution_rpc(execution_rpc)
            .build()
            .map_err(|e| HeaderError::Build(e.to_string()))?;

        client.start().await.map_err(|e| HeaderError::Start(e.to_string()))?;
        tracing::info!("Beacon light client synced");

        Ok(Self { client })
    }

    pub async fn get_header(&self, block: u64) -> Result<Option<ScopeHeader>, HeaderError> {
        // helios exposes an alloy-compatible provider
        // Headers are verified against sync committee signatures
        let block = self.client
            .get_block_by_number(block.into(), false)
            .await
            .map_err(|e| HeaderError::Fetch(e.to_string()))?;

        Ok(block.map(|b| ScopeHeader::from(b.header)))
    }

    pub fn subscribe_new_heads(&self) -> broadcast::Receiver<ScopeHeader> {
        // Used in Phase 3 for live sync
        todo!("Phase 3")
    }
}
```

### Portal Network receipt fetching

```rust
// crates/scopenode-core/src/receipts.rs (updated)

use alloy_primitives::B256;

/// Content key for block receipts in Portal History Network
fn receipts_key(block_hash: &B256) -> Vec<u8> {
    let mut key = vec![0x02u8]; // receipts type prefix
    key.extend_from_slice(block_hash.as_slice());
    key
}

pub struct PortalReceiptSource {
    /// Portal Network JSON-RPC endpoint (trin running locally or library)
    client: reqwest::Client,
    endpoint: String,
    fallback: Option<Arc<dyn Provider>>,
}

impl PortalReceiptSource {
    pub async fn fetch(&self, block_num: u64, block_hash: B256)
        -> Result<Vec<Receipt>, ReceiptError>
    {
        for attempt in 0..3 {
            match self.try_portal(block_hash).await {
                Ok(receipts) => return Ok(receipts),
                Err(e) => {
                    tracing::warn!(
                        block = block_num,
                        attempt,
                        err = %e,
                        "Portal fetch failed"
                    );
                }
            }
        }

        // Try fallback RPC if configured
        if let Some(ref provider) = self.fallback {
            tracing::info!(block = block_num, "Trying fallback RPC");
            return provider.get_block_receipts(block_num.into())
                .await
                .map_err(|e| ReceiptError::Rpc(e.to_string()))?
                .ok_or(ReceiptError::NotFound(block_num));
        }

        Err(ReceiptError::AllFailed(block_num))
    }

    async fn try_portal(&self, block_hash: B256) -> Result<Vec<Receipt>, ReceiptError> {
        let key = format!("0x{}", hex::encode(receipts_key(&block_hash)));
        let resp: serde_json::Value = self.client
            .post(&self.endpoint)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "portal_historyGetContent",
                "params": [key],
                "id": 1
            }))
            .send().await.map_err(ReceiptError::Http)?
            .json().await.map_err(ReceiptError::Http)?;

        let content = resp["result"]["content"].as_str()
            .ok_or(ReceiptError::NotAvailable)?;

        let raw = hex::decode(content.trim_start_matches("0x"))
            .map_err(|_| ReceiptError::Decode)?;

        decode_receipts_rlp(&raw)
    }
}
```

### Proxy detection

```rust
// crates/scopenode-core/src/abi.rs (addition)

const EIP1967_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

pub async fn resolve_abi_address(
    address: Address,
    provider: &impl Provider,
) -> Result<Address, AbiError> {
    let slot: B256 = EIP1967_SLOT.parse().unwrap();
    let value = provider.get_storage_at(address, slot.into()).await
        .map_err(|e| AbiError::Rpc(e.to_string()))?;

    let bytes = value.to_be_bytes::<32>();
    let impl_addr = Address::from_slice(&bytes[12..]);

    if impl_addr == Address::ZERO {
        Ok(address) // Not a proxy
    } else {
        tracing::info!(
            proxy = %address,
            implementation = %impl_addr,
            "Proxy detected (EIP-1967) — using implementation ABI"
        );
        // Store impl address in contracts table
        Ok(impl_addr)
    }
}
```

### `scopenode validate` command

```rust
// crates/scopenode/src/commands/validate.rs

use console::style;

pub async fn run(config_path: PathBuf) -> Result<()> {
    let config = Config::from_file(&config_path)?;
    let mut all_ok = true;

    for contract in &config.contracts {
        println!("Checking {} ({})...\n", contract.name.as_deref().unwrap_or("contract"), contract.address);

        // Address format (already validated by Config::from_file)
        println!("  {} Contract address valid", style("✓").green());

        // Proxy detection
        match resolve_abi_address(contract.address, &provider).await {
            Ok(addr) if addr != contract.address => {
                println!("  {} Proxy detected → using implementation ABI ({})", style("✓").green(), addr);
            }
            Ok(_) => println!("  {} Not a proxy", style("✓").green()),
            Err(e) => {
                println!("  {} Proxy check failed: {}", style("!").yellow(), e);
            }
        }

        // ABI fetch
        match etherscan.fetch_events(contract.address).await {
            Ok(events) => {
                println!("  {} ABI fetched ({} events: {})",
                    style("✓").green(),
                    events.len(),
                    events.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", ")
                );
                // Check requested events exist in ABI
                for event_name in &contract.events {
                    if events.iter().any(|e| &e.name == event_name) {
                        println!("  {} Event '{}' found in ABI", style("✓").green(), event_name);
                    } else {
                        println!("  {} Event '{}' NOT found in ABI", style("✗").red(), event_name);
                        all_ok = false;
                    }
                }
            }
            Err(e) => {
                println!("  {} ABI fetch failed: {}", style("✗").red(), e);
                all_ok = false;
            }
        }

        // Block range
        println!("  {} Block range: {} → {}",
            style("✓").green(),
            contract.from_block,
            contract.to_block.map(|b| b.to_string()).unwrap_or_else(|| "live".into())
        );

        // Warn if no fallback_rpc
        if config.node.fallback_rpc.is_none() {
            println!("  {} fallback_rpc not set — Portal failures will leave blocks as pending_retry",
                style("!").yellow());
        }

        println!();
    }

    if all_ok {
        println!("Config looks good. Run `scopenode sync {}` to start.",
            config_path.display());
    }

    Ok(())
}
```

### `scopenode abi` command

```rust
// crates/scopenode/src/commands/abi.rs

pub async fn run(address: String) -> Result<()> {
    let addr: Address = address.parse()
        .map_err(|_| anyhow::anyhow!("Invalid address: {}", address))?;

    let api_key = std::env::var("ETHERSCAN_API_KEY").ok();
    let client = EtherscanClient::new(api_key);

    println!("Fetching ABI for {addr}...\n");
    let events = client.fetch_events(addr).await?;

    for e in &events {
        println!("  {} {}", e.name, e.signature());
        println!("    Topic0: 0x{}", hex::encode(e.topic0()));
        for input in &e.inputs {
            let indexed = if input.indexed { " [indexed]" } else { "" };
            println!("    - {} {}{}", input.r#type, input.name, indexed);
        }
        println!();
    }
    Ok(())
}
```

---

## Config additions

```toml
[node]
port = 8545
# Beacon chain consensus RPC for Helios (free public endpoints)
consensus_rpc = "https://www.lightclientdata.org"
# Optional: Portal Network trin endpoint (defaults to bundled trin)
# portal_rpc = "http://localhost:8547"
# Optional: fallback RPC when Portal fails (receipts still Merkle-verified)
# fallback_rpc = "https://eth.llamarpc.com"
```

---

## Tests

```
Unit:
  - receipts_key() produces correct 33-byte content key
  - resolve_abi_address() returns same address when not a proxy
  - Helios client initializes (mocked consensus RPC)

Integration (--ignored, require network):
  - Helios fetches and verifies block 17000000 header
    (receipts_root matches known value)
  - Portal Network returns receipts for a known block
  - Receipts from Portal pass Merkle verification
  - USDC proxy detection → finds implementation address
```

---

## Definition of done

- [ ] `scopenode sync config.toml` works without any RPC API key
- [ ] Headers verified by beacon sync committee (Helios)
- [ ] Receipts fetched from Portal Network when available
- [ ] Fallback RPC kicks in when Portal fails (with warning in logs)
- [ ] Merkle verification runs on ALL receipts regardless of source
- [ ] `scopenode validate config.toml` catches bad event names, shows proxy info
- [ ] `scopenode abi 0x...` shows events with correct topic0 hashes
- [ ] Proxy contracts (USDC, Aave) correctly resolve to implementation ABI

---

## What you learn in this phase

**Beacon chain:** Post-Merge consensus, validator slots and epochs, why sync
committees exist and how BLS aggregation makes them efficient, what a checkpoint
is and why checkpoint sync is safe.

**BLS signatures:** How Boneh-Lynn-Shacham works, why signature aggregation is
possible and why it's used in Ethereum, why you should never implement it yourself.

**Portal Network:** DHT architecture, discv5 peer discovery, Kademlia routing,
ENR records, content key format, why Portal can serve historical data without
storing everything.

**Proxy patterns:** Why EIP-1967 exists, what delegate calls are, how the storage
slot convention standardizes proxy detection, the distinction between proxy
address (where events come from) and implementation address (where ABI lives).

**Production reliability:** Why fallback-with-verification is the right pattern
(trust no source, verify everything), source tracking in SQLite, retry semantics.
