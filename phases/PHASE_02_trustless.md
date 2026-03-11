# Phase 2 — Trustless (devp2p + Beacon Light Client)

## Goal

Replace the Phase 1 RPC dependencies with trustless P2P sources. After this
phase, scopenode runs without any API key or centralized provider:

- **Headers (historical):** fetched from devp2p peers — mainnet full nodes using
  the ETH wire protocol. Multiple peers queried; agreement required.
- **Headers (live sync):** Helios beacon light client — verified by BLS
  signatures of 512 validators. Multiple `consensus_rpc` endpoints configured;
  agreement required before accepting sync committee updates.
- **Receipts:** fetched from devp2p peers using `GetReceipts` ETH wire message.
  Fallback: ERA1 archives → `fallback_rpc` (last resort).
- **ABIs:** fetched from Sourcify — Ethereum Foundation's open ABI registry, no
  API key required.

Everything is Merkle-verified before storage. The source does not affect trust.

---

## Staging environment

The `--data-dir` flag and `SCOPENODE_DATA_DIR` env var are inherited from
Phase 1. Use them throughout this phase.

Add Phase 2 fields to `config.test.toml` for testing trustless sources:

```toml
# config.test.toml additions for Phase 2
[node]
port = 8545
data_dir = "~/.scopenode-staging"
# consensus_rpc — one or more beacon APIs for live sync (free public endpoints)
# Must all agree on sync committee updates before accepting them.
consensus_rpc = [
    "https://www.lightclientdata.org",
    "https://sync-mainnet.beaconcha.in",
]
# era1_dir = "~/.scopenode-staging/era1"   # optional, ERA1 archive directory
fallback_rpc = "https://eth.llamarpc.com"   # last-resort during testing
```

Snapshot the staging DB before switching header source:

```bash
cp ~/.scopenode-staging/scopenode.db ~/.scopenode-staging/pre-phase2.db.snap
```

---

## What changes from Phase 1

| Component | Phase 1 | Phase 2 |
|---|---|---|
| Header source (historical) | `alloy` provider (public RPC) | devp2p peers — 3+ peers must agree (`GetBlockHeaders`) |
| Header source (live) | `alloy` provider (public RPC) | Helios beacon light client — multiple `consensus_rpc` endpoints must agree |
| Receipt source | `alloy` provider (public RPC) | devp2p peers (`GetReceipts`) → ERA1 archives → `fallback_rpc` |
| ABI source | Etherscan | Sourcify (EF, no API key) → `abi_override` fallback |
| Proxy detection | None | `eth_getStorageAt` via `fallback_rpc` (setup-only, one-time) |

Everything else — bloom scan, Merkle verification, ABI decoding, SQLite
storage, JSON-RPC server — is unchanged.

---

## Concepts to understand deeply

### The Ethereum devp2p network

The Ethereum execution layer uses a P2P network called devp2p. Every full node
is on it. It has two layers:

1. **Discovery (discv4):** UDP-based Kademlia DHT. Nodes announce themselves
   via signed ENR records. Used to find peers.

2. **RLPx (TCP):** Encrypted, authenticated TCP connections between peers.
   After connecting, peers run sub-protocols. We use the ETH sub-protocol.

The ETH sub-protocol (ETH/68) includes:
- `GetBlockHeaders` / `BlockHeaders` — request headers by number or hash
- `GetBlockBodies` / `BlockBodies` — request transaction lists
- `GetReceipts` / `Receipts` — request block receipts

Full nodes are abundant (tens of thousands), always online, and serve this
data as part of normal chain sync. We connect as a peer and request exactly
what we need.

**Peer reciprocity:** Full nodes expect peers to also serve data. We announce
ourselves honestly but serve nothing (we have nothing to serve). In practice
peers tolerate this for short-lived data requests, but we should connect to
multiple peers and rotate to avoid being seen as a leech.

### RLPx — the transport

RLPx is an ECIES-encrypted, MAC-authenticated TCP protocol. The handshake:

1. Initiator sends auth message (ECIES encrypted with recipient's public key)
2. Recipient sends auth-ack (ECIES encrypted with initiator's public key)
3. Both derive session keys from the handshake material (ECDH)
4. From here, all messages are frame-encrypted (AES-256-CTR + MAC)

After RLPx, the ETH sub-protocol handshake (`Status` message) exchanges:
- protocol version, network ID, total difficulty, genesis hash, best block hash

This confirms we're on the same network (mainnet, chain ID 1).

### `GetReceipts` — how we fetch receipts

The ETH wire protocol message `GetReceipts` takes a list of block hashes and
returns the receipts for those blocks. This is exactly what we need.

Receipts from devp2p peers are **not trusted** — we verify them against the
`receipts_root` in the block header using the same Merkle Patricia Trie
verification as Phase 1. A lying peer produces a root mismatch and gets
skipped; we try another peer.

### Multi-peer header agreement (historical sync)

For historical blocks (finalized, older than 64 blocks): the canonical chain
is cryptographically fixed. Any honest full node will return the same header
for a given block number.

Our approach: query 3+ independent peers for the same header. If they all
return the same `hash`, `receipts_root`, `logs_bloom` — accept it. If any
disagree — drop the outlier and try a different peer.

This gives us trustless historical headers without Helios. No beacon API
needed for historical data.

### Helios — for live sync only

For live sync (watching new blocks as they arrive), we need to know the
current canonical head. For this we use Helios — the beacon light client.

Helios (by a16z) implements the Altair light client protocol:
1. Bootstraps from a recent finalized checkpoint block hash
2. Syncs sync committee updates forward (~1KB per 27-hour period)
3. Verifies each new block header via BLS signature of the sync committee
4. 512 validators must collude to deceive us

**Multiple consensus endpoints:** We configure 2+ public beacon APIs. For
each sync committee update, we fetch from all configured endpoints and require
them to return identical data before accepting. This eliminates single-point-
of-trust while still using free public infrastructure.

```toml
consensus_rpc = [
    "https://www.lightclientdata.org",
    "https://sync-mainnet.beaconcha.in",
]
```

If they disagree: log an error, pause live sync, alert the user.

The checkpoint trust assumption: we bootstrap from a recent finalized block
hash published by multiple independent sources. This is the standard Ethereum
light client protocol design (EIP-3526 / Altair). After the initial bootstrap,
all verification is cryptographic.

### BLS signatures and aggregation

BLS (Boneh-Lynn-Shacham) signatures have a unique property: multiple
signatures can be **aggregated** into a single 96-byte signature. Verifying
one aggregated signature over N messages is nearly as fast as verifying one.

This is why sync committees work: 512 individual BLS signatures → 1 aggregated
signature → fast verification. We use `helios` which uses the `blst` crate
(the reference BLS implementation). Never implement BLS yourself.

### Sourcify — decentralized ABI registry

Sourcify (sourcify.dev) is the Ethereum Foundation's open, decentralized
contract verification platform. Unlike Etherscan:

- **No API key required** — fully open
- **Run by EF** — not a commercial provider
- **Open source** — the server and data are both open
- **Covers most major contracts** — Uniswap, Aave, USDC, etc.

API:
```
GET https://sourcify.dev/server/files/any/1/{address}
```

Returns a JSON object with `files` array. The ABI is in the file named
`metadata.json` under `output.abi`, or in a standalone `ABI.json`.

If a contract is not on Sourcify: scopenode logs an error and requires the
user to set `abi_override` in their config file. We do not fall back to
Etherscan — Etherscan requires an API key and is a centralized provider.

### Proxy contracts (EIP-1967)

Many contracts are proxies. Detection requires reading a storage slot:
```
keccak256("eip1967.proxy.implementation") - 1
= 0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc
```

This is a one-time setup check — not a data integrity concern. We use the
`fallback_rpc` (if configured) for this single `eth_getStorageAt` call.

If `fallback_rpc` is not configured and proxy detection is needed: warn the
user and tell them to either set `fallback_rpc` (temporarily) or set
`abi_override` manually in their config.

### ERA1 archives

ERA1 files are flat-file archives of historical Ethereum data (headers,
bodies, receipts) published by the Ethereum Foundation. Each covers ~8192
blocks.

ERA1 fills the gap where devp2p peers may not serve receipts — non-archive
nodes prune receipts for old blocks. Download the relevant ERA1 files once
via `scopenode download-era1` (added in Phase 3a) and have them forever.

Verification is identical: extract receipts, rebuild Merkle Patricia Trie,
check root against header's `receiptsRoot`. Same math, different transport.

---

## Implementation

### Crates added in Phase 2

```toml
# Workspace additions for Phase 2
reth-network    = { version = "1", default-features = false }
reth-eth-wire   = { version = "1", default-features = false }
reth-discv4     = { version = "1", default-features = false }
helios-client   = { version = "=0.8", features = ["ethereum"] }
```

Note: `reth-*` crates are modular — we use only the networking layer. No
state execution, no storage, no EVM.

### `HeaderSource` trait

```rust
// crates/scopenode-core/src/headers.rs (updated)

use alloy_primitives::B256;
use async_trait::async_trait;

/// Trait for fetching verified block headers.
/// Phase 1: RpcHeaderSource (public RPC)
/// Phase 2 historical: DevP2PHeaderSource (multi-peer agreement)
/// Phase 2 live: BeaconHeaderSource (helios, multiple consensus endpoints)
#[async_trait]
pub trait HeaderSource: Send + Sync {
    async fn get_header(&self, block: u64) -> Result<Option<ScopeHeader>, HeaderError>;
    async fn get_latest_block_number(&self) -> Result<u64, HeaderError>;
}

/// Historical header source: queries multiple devp2p peers and requires agreement.
pub struct DevP2PHeaderSource {
    network: Arc<NetworkHandle>,
    min_agreement: usize,  // default: 3 peers must return same header
}

#[async_trait]
impl HeaderSource for DevP2PHeaderSource {
    async fn get_header(&self, block: u64) -> Result<Option<ScopeHeader>, HeaderError> {
        let responses = self.network
            .get_block_headers_from_peers(block, self.min_agreement + 1)
            .await?;

        // Require min_agreement peers to return the same header hash
        let agreed = find_majority(&responses, self.min_agreement)?;
        Ok(agreed.map(ScopeHeader::from))
    }

    async fn get_latest_block_number(&self) -> Result<u64, HeaderError> {
        // Ask peers for their best block number from their Status message
        self.network.best_block_number().await.map_err(HeaderError::Network)
    }
}

/// Live sync header source: Helios beacon light client with multiple consensus endpoints.
pub struct BeaconHeaderSource {
    client: helios_client::Client<helios_consensus::ethereum::EthereumConsensus>,
}

impl BeaconHeaderSource {
    pub async fn new(consensus_rpcs: &[String]) -> Result<Self, HeaderError> {
        // Helios verifies BLS signatures; we validate by checking multiple RPC endpoints
        // agree on sync committee updates before accepting them.
        let mut client = helios_client::ClientBuilder::new()
            .network(helios_client::networks::Network::Mainnet)
            .consensus_rpc(&consensus_rpcs[0])  // primary
            .build()
            .map_err(|e| HeaderError::Build(e.to_string()))?;

        client.start().await.map_err(|e| HeaderError::Start(e.to_string()))?;
        tracing::info!("Beacon light client synced");

        Ok(Self { client })
    }
}
```

### devp2p network setup

```rust
// crates/scopenode-core/src/network.rs

use reth_discv4::{Discv4, Discv4Config, DEFAULT_DISCOVERY_PORT};
use reth_network::{NetworkConfig, NetworkManager};
use reth_eth_wire::EthVersion;

pub struct ScopeNetwork {
    handle: NetworkHandle,
}

impl ScopeNetwork {
    pub async fn start() -> Result<Self, NetworkError> {
        let secret_key = secp256k1::SecretKey::new(&mut rand::thread_rng());

        let discv4_config = Discv4Config::builder()
            .add_boot_nodes(reth_discv4::bootnodes::mainnet_nodes())
            .build();

        let network_config = NetworkConfig::builder(secret_key)
            .discovery(discv4_config)
            .build();

        let manager = NetworkManager::new(network_config).await?;
        let handle = manager.handle().clone();

        tokio::spawn(manager);

        // Wait for initial peer discovery
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        tracing::info!(peers = handle.num_connected_peers(), "devp2p network started");

        Ok(Self { handle })
    }

    /// Request block receipts from devp2p peers.
    /// Tries up to `max_peers` peers; returns first valid (Merkle-verified) result.
    pub async fn get_receipts(
        &self,
        block_hash: B256,
        expected_root: B256,
        block_num: u64,
    ) -> Result<(Vec<Receipt>, String), NetworkError> {
        let peers = self.handle.get_peers(5);

        for peer_id in peers {
            match self.handle.request_receipts(peer_id, vec![block_hash]).await {
                Ok(receipts) => {
                    match verify_receipts(&receipts, expected_root, block_num) {
                        Ok(()) => return Ok((receipts, format!("devp2p:{peer_id}"))),
                        Err(e) => {
                            tracing::warn!(
                                peer = %peer_id, block = block_num, err = %e,
                                "Peer sent invalid receipts"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(peer = %peer_id, block = block_num, err = %e,
                        "Receipt request failed");
                }
            }
        }

        Err(NetworkError::AllPeersFailed(block_num))
    }
}
```

### Receipt fetching — layered fallback chain

```
devp2p peers (mainnet full nodes) → ERA1 archives (local files) → fallback RPC (last resort)
```

Every layer feeds into the same Merkle verification.

```rust
// crates/scopenode-core/src/receipts.rs (updated)

pub enum ReceiptSource {
    DevP2P,
    Era1,
    Rpc,
}

pub struct ReceiptFetcher {
    network: Arc<ScopeNetwork>,
    era1_dir: Option<PathBuf>,
    fallback: Option<Arc<dyn Provider>>,
}

impl ReceiptFetcher {
    pub async fn fetch(
        &self,
        block_num: u64,
        block_hash: B256,
        expected_root: B256,
    ) -> Result<(Vec<Receipt>, ReceiptSource), ReceiptError> {

        // Layer 1: devp2p peers
        match self.network.get_receipts(block_hash, expected_root, block_num).await {
            Ok((receipts, _peer)) => return Ok((receipts, ReceiptSource::DevP2P)),
            Err(e) => {
                tracing::warn!(block = block_num, err = %e, "devp2p receipt fetch failed");
            }
        }

        // Layer 2: ERA1 archive (local flat files, if configured)
        if let Some(ref era1_dir) = self.era1_dir {
            match self.try_era1(era1_dir, block_num, expected_root).await {
                Ok(receipts) => {
                    tracing::info!(block = block_num, "Loaded from ERA1 archive");
                    return Ok((receipts, ReceiptSource::Era1));
                }
                Err(e) => {
                    tracing::debug!(block = block_num, err = %e, "ERA1 not available");
                }
            }
        }

        // Layer 3: Fallback RPC (last resort, if configured)
        if let Some(ref provider) = self.fallback {
            tracing::warn!(block = block_num, "Using fallback RPC — not ideal");
            let receipts = provider.get_block_receipts(block_num.into())
                .await
                .map_err(|e| ReceiptError::Rpc(e.to_string()))?
                .ok_or(ReceiptError::NotFound(block_num))?;

            // Still Merkle-verify even from fallback RPC
            verify_receipts(&receipts, expected_root, block_num)
                .map_err(|e| ReceiptError::Verification(block_num, e.to_string()))?;

            return Ok((receipts, ReceiptSource::Rpc));
        }

        Err(ReceiptError::AllFailed(block_num))
    }

    async fn try_era1(
        &self,
        era1_dir: &Path,
        block_num: u64,
        expected_root: B256,
    ) -> Result<Vec<Receipt>, ReceiptError> {
        let epoch = block_num / 8192;
        let pattern = format!("mainnet-{epoch:05}-");

        let file = std::fs::read_dir(era1_dir)
            .map_err(|_| ReceiptError::Era1NotFound(block_num))?
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().starts_with(&pattern))
            .ok_or(ReceiptError::Era1NotFound(block_num))?;

        let data = std::fs::read(file.path())
            .map_err(|_| ReceiptError::Era1Read(block_num))?;

        let receipts = decode_era1_receipts(&data, block_num)?;

        // Still Merkle-verify
        verify_receipts(&receipts, expected_root, block_num)
            .map_err(|e| ReceiptError::Verification(block_num, e.to_string()))?;

        Ok(receipts)
    }
}
```

### ABI fetching — Sourcify

```rust
// crates/scopenode-core/src/abi.rs (updated)

const SOURCIFY_API: &str = "https://sourcify.dev/server/files/any/1";

pub struct SourcifyClient {
    http: reqwest::Client,
}

impl SourcifyClient {
    pub async fn fetch_events(&self, address: Address) -> Result<Vec<EventAbi>, AbiError> {
        let url = format!("{SOURCIFY_API}/{address:?}");
        let resp: serde_json::Value = self.http
            .get(&url)
            .send().await
            .map_err(|e| AbiError::Http(e.to_string()))?
            .json().await
            .map_err(|e| AbiError::Http(e.to_string()))?;

        // Sourcify returns { status, files: [{ name, path, content }] }
        // ABI is in metadata.json under output.abi
        let files = resp["files"].as_array().ok_or(AbiError::NotOnSourcify(address))?;

        let abi_json = files.iter()
            .find(|f| f["name"].as_str() == Some("metadata.json"))
            .and_then(|f| f["content"].as_str())
            .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
            .and_then(|m| m["output"]["abi"].as_array().cloned())
            .ok_or(AbiError::AbiNotFound(address))?;

        parse_events_from_abi(&abi_json)
    }
}

// If contract not on Sourcify, require abi_override in config.
// Error message tells user exactly what to do:
//
// "ABI not found on Sourcify for 0x8ad...
//  Add to your config:
//    [[contracts]]
//    address = \"0x8ad...\"
//    abi_override = \"./abis/contract.json\"
//  Download ABI from Etherscan or the project's GitHub."
```

### Proxy detection

```rust
// crates/scopenode-core/src/abi.rs (addition)

const EIP1967_SLOT: &str =
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";

/// Detect EIP-1967 proxy. Uses fallback_rpc for the eth_getStorageAt call.
/// This is a one-time setup check — not a data integrity concern.
pub async fn resolve_abi_address(
    address: Address,
    fallback_rpc: Option<&Arc<dyn Provider>>,
) -> Result<Address, AbiError> {
    let provider = match fallback_rpc {
        Some(p) => p,
        None => {
            tracing::debug!(addr = %address, "No fallback_rpc — skipping proxy check");
            return Ok(address);
        }
    };

    let slot: B256 = EIP1967_SLOT.parse().unwrap();
    let value = provider.get_storage_at(address, slot.into()).await
        .map_err(|e| AbiError::Rpc(e.to_string()))?;

    let bytes = value.to_be_bytes::<32>();
    let impl_addr = Address::from_slice(&bytes[12..]);

    if impl_addr == Address::ZERO {
        Ok(address)
    } else {
        tracing::info!(
            proxy = %address,
            implementation = %impl_addr,
            "Proxy detected (EIP-1967) — using implementation ABI"
        );
        Ok(impl_addr)
    }
}
```

### Config additions

```toml
[node]
port = 8545
# consensus_rpc — list of beacon APIs for live sync (requires agreement)
consensus_rpc = [
    "https://www.lightclientdata.org",
    "https://sync-mainnet.beaconcha.in",
]
# era1_dir — local ERA1 archive directory (run `scopenode download-era1` to populate)
# era1_dir = "~/.scopenode/era1"
# fallback_rpc — last resort for receipts AND proxy detection (still Merkle-verified)
# fallback_rpc = "https://eth.llamarpc.com"
```

### `scopenode validate` command

```rust
// crates/scopenode/src/commands/validate.rs

pub async fn run(config_path: PathBuf, fallback_rpc: Option<Arc<dyn Provider>>) -> Result<()> {
    let config = Config::from_file(&config_path)?;
    let sourcify = SourcifyClient::new();
    let mut all_ok = true;

    for contract in &config.contracts {
        println!("Checking {} ({})...\n",
            contract.name.as_deref().unwrap_or("contract"),
            contract.address);

        println!("  {} Contract address valid", style("✓").green());

        // Proxy detection (requires fallback_rpc)
        match resolve_abi_address(contract.address, fallback_rpc.as_ref()).await {
            Ok(addr) if addr != contract.address => {
                println!("  {} Proxy detected → using implementation ABI ({})",
                    style("✓").green(), addr);
            }
            Ok(_) => println!("  {} Not a proxy", style("✓").green()),
            Err(e) => {
                println!("  {} Proxy check skipped: {}", style("!").yellow(), e);
            }
        }

        // ABI fetch from Sourcify
        match sourcify.fetch_events(contract.address).await {
            Ok(events) => {
                println!("  {} ABI fetched from Sourcify ({} events: {})",
                    style("✓").green(),
                    events.len(),
                    events.iter().map(|e| e.name.as_str()).collect::<Vec<_>>().join(", "));
                for event_name in &contract.events {
                    if events.iter().any(|e| &e.name == event_name) {
                        println!("  {} Event '{}' found", style("✓").green(), event_name);
                    } else {
                        println!("  {} Event '{}' NOT found in ABI",
                            style("✗").red(), event_name);
                        all_ok = false;
                    }
                }
            }
            Err(AbiError::NotOnSourcify(_)) => {
                if contract.abi_override.is_some() {
                    println!("  {} Using abi_override (not on Sourcify)",
                        style("✓").green());
                } else {
                    println!("  {} Not on Sourcify — add abi_override to config",
                        style("✗").red());
                    all_ok = false;
                }
            }
            Err(e) => {
                println!("  {} ABI fetch failed: {}", style("✗").red(), e);
                all_ok = false;
            }
        }

        if config.node.fallback_rpc.is_none() {
            println!("  {} fallback_rpc not set — proxy detection skipped, old block receipt \
                fallback unavailable", style("!").yellow());
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

    let client = SourcifyClient::new();
    println!("Fetching ABI from Sourcify for {addr}...\n");

    let events = client.fetch_events(addr).await
        .map_err(|e| match e {
            AbiError::NotOnSourcify(_) => anyhow::anyhow!(
                "Contract not verified on Sourcify.\n\
                 Use --abi-file to provide a local ABI JSON file."
            ),
            e => anyhow::Error::from(e),
        })?;

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

## Tests

```
Unit:
  - SourcifyClient: parses ABI correctly from known metadata.json structure
  - SourcifyClient: returns NotOnSourcify error for unknown address (mocked 404)
  - DevP2PHeaderSource: returns error when fewer than min_agreement peers respond
  - DevP2PHeaderSource: rejects header when peers disagree
  - DevP2PHeaderSource: accepts header when 3+ peers agree
  - ReceiptFetcher: falls through devp2p → ERA1 → RPC correctly (mocked)
  - ReceiptFetcher: Merkle verification runs on ERA1 receipts
  - ReceiptFetcher: Merkle verification runs on fallback RPC receipts
  - resolve_abi_address: returns same address when not a proxy (mocked storage)
  - resolve_abi_address: returns impl address for EIP-1967 proxy
  - resolve_abi_address: returns original address when no fallback_rpc configured
  - ERA1 epoch calculation: block 0 → epoch 0, block 8191 → epoch 0, block 8192 → epoch 1

Integration (--ignored, require network):
  - devp2p connects to mainnet peers and discovers 5+ nodes
  - GetBlockHeaders returns correct header for block 17000000 (check known hash)
  - GetReceipts returns receipts that pass Merkle verification for a known block
  - Sourcify returns valid ABI for Uniswap V3 pool address
  - ERA1 file loads and decodes receipts for a known block
  - USDC proxy detection → finds implementation address (requires fallback_rpc)
```

---

## Definition of done

- [ ] `scopenode sync config.toml` works without any RPC API key
- [ ] Headers for historical sync: 3+ devp2p peers must agree on header hash
- [ ] Headers for live sync: Helios verified by beacon sync committee
- [ ] Multiple `consensus_rpc` endpoints configured; disagreement halts live sync
- [ ] `HeaderSource` trait abstracts header backend — swappable without pipeline changes
- [ ] Receipts fetched from devp2p peers using `GetReceipts` ETH wire message
- [ ] ERA1 archives used as fallback when devp2p fails and `era1_dir` configured
- [ ] Fallback RPC kicks in as last resort (with warning in logs)
- [ ] Every receipt tagged with source (`devp2p`, `era1`, `rpc`)
- [ ] Merkle verification runs on ALL receipts regardless of source
- [ ] ABI fetched from Sourcify — no Etherscan, no API key
- [ ] If not on Sourcify: clear error telling user to set `abi_override`
- [ ] Proxy detection uses `fallback_rpc` for `eth_getStorageAt` (setup-only)
- [ ] `scopenode validate config.toml` catches bad event names, shows proxy info
- [ ] `scopenode abi 0x...` fetches from Sourcify, shows events with topic0 hashes

---

## What you learn in this phase

**devp2p:** RLPx transport (ECIES, MAC, frame encryption), discv4 peer
discovery (Kademlia DHT, ENR records, signed identity), the ETH wire protocol
(`Status` handshake, `GetBlockHeaders`, `GetReceipts`), peer reciprocity and
why full nodes tolerate light consumers.

**Multi-peer agreement:** Why agreement across independent peers is a strong
trustless guarantee for finalized data, how to detect and handle disagreeing
peers, the importance of peer diversity.

**Beacon chain:** Post-Merge consensus, validator slots and epochs, sync
committees, BLS aggregation, checkpoint sync safety, why multiple consensus
endpoints strengthen the trust model.

**Sourcify vs Etherscan:** Why decentralized open registries matter, the
tradeoffs of Sourcify coverage vs convenience, why `abi_override` is the right
escape hatch.

**Proxy patterns:** EIP-1967 storage slot convention, delegate calls, why
proxy detection is a setup concern (not a data integrity concern), why using
fallback_rpc for one-time setup checks is acceptable.

**ERA1 archives:** Flat-file format for historical Ethereum data, epoch-based
file naming, why offline archives complement P2P networks for old data.
