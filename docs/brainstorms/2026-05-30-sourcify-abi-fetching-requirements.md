# Sourcify ABI Fetching Requirements

Created: 2026-05-30
Status: completed

Current implementation note: ABI resolution now lives in
`crates/scopenode-core/src/abi_resolution.rs`, with the Sourcify HTTP adapter in
`crates/scopenode/src/sourcify.rs`. The cache → local override → remote order,
proxy implementation hint, and wildcard event behavior are implemented. This
document preserves the requirements that led to that design.

## Problem frame

scopenode currently requires every contract to have an `abi_override` pointing to a local ABI JSON
file. For popular, Sourcify-verified contracts (Uniswap, AAVE, Compound, etc.) this means the user
must manually find, download, and wire up an ABI file before they can start indexing. That friction
is unnecessary given that Sourcify is a publicly available ABI registry.

scopenode's trust model is about **chain data**: receipts and events are verified via Merkle proofs
from local ERA1 files, avoiding reliance on external nodes. ABIs are source-code metadata, not chain
state, so fetching them from Sourcify does not undermine that model.

## Decisions made

| Decision | Resolution |
|---|---|
| HTTP client location | Binary crate (`scopenode`). `reqwest` added there. Core stays network-free. |
| Injection into core | New `AbiFetcher` trait in `scopenode-core`. `AbiCache` gains `Option<Box<dyn AbiFetcher>>`. Binary implements `SourcifyClient`. |
| Cache key for proxy contracts | Always proxy address (`contract.address`). `impl_address` is a fetch-time hint only — not persisted to DB, no schema change. |
| Config validation | Remove the hard `abi_override` required check from `validate()`. Failure deferred to runtime. |
| Sourcify endpoint | `https://repo.sourcify.dev/contracts/full_match/1/{address}/metadata.json`. ABI at `output.abi`. Fall back to `partial_match` on 404. |
| `events` wildcard | `events = ["*"]` indexes all events. `"*"` must be the sole element — mixing with names is a config error. |
| HTTP timeout | 10 seconds. |
| Fetch order | SQLite cache → local file (if path valid) → Sourcify → error. |
| Fallback from bad file to Sourcify | Emit a warning ("abi_override file missing/invalid, falling back to Sourcify for 0x…"). Cache Sourcify result in SQLite. Do not write back to the user's file path. |
| `events = ["*"]` + empty ABI | Emit a warning and proceed with zero events for that contract. |

## In scope

- Make `abi_override` optional in `ContractConfig`.
- Add optional `impl_address: Option<Address>` to `ContractConfig`. When set, Sourcify is queried
  for `impl_address` instead of the proxy address.
- New `AbiFetcher` trait in `scopenode-core`:
  ```rust
  #[async_trait]
  pub trait AbiFetcher: Send + Sync {
      async fn fetch(&self, address: Address) -> Result<String, AbiError>;
  }
  ```
  Returns the raw ABI JSON array string (same format as `abi_override` file contents).
- `AbiCache` gains `fetcher: Option<Box<dyn AbiFetcher>>`. Fetch order in `get_or_fetch`:
  1. SQLite cache hit → return.
  2. `abi_override` path set AND file is valid → load, cache in SQLite, return.
  3. Otherwise (no file, or file missing/invalid) → warn, call `fetcher.fetch(resolved_address)`
     where `resolved_address = impl_address.unwrap_or(contract.address)`.
  4. Sourcify result → cache in SQLite, return.
  5. All sources failed → return error.
- `SourcifyClient` in the binary crate:
  - Uses `reqwest` with a 10-second timeout.
  - GET `https://repo.sourcify.dev/contracts/full_match/1/{address}/metadata.json`.
  - If 404, retry with `partial_match`.
  - Extracts `response["output"]["abi"]` and serializes it back to a JSON string.
- `events = ["*"]` support:
  - `ContractConfig` validation: if `events` contains `"*"` and has more than one element →
    config error.
  - When `["*"]`, `get_or_fetch` returns all event entries from the ABI without name filtering.
  - If the resulting event list is empty, emit a warning and continue with zero events.
- Remove `ConfigError::AbiOverrideRequired` and the corresponding `validate()` check and test.
- Update `config.example.toml` with examples showing omitted `abi_override` and `impl_address`.

## Out of scope

- Support for chains other than Ethereum mainnet (chain ID 1).
- Automatic proxy detection (auto-resolving `impl_address` from on-chain storage).
- ABI cache invalidation or forced re-fetch flags.
- Writing Sourcify results back to the user's `abi_override` file path.
- Fetching non-event ABI entries (functions, constructors).
- `impl_address` stored in the `contracts` DB table (the column was dropped in migration 005 and
  stays dropped — `impl_address` is resolved at fetch time and is not persisted).

## Config examples

```toml
# Verified contract — no abi_override needed
[[contracts]]
name = "Uniswap V3 ETH/USDC Pool"
address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events = ["Swap"]
from_block = 16000000
to_block = 16100000

# Proxy contract — logic ABI at impl_address
[[contracts]]
name = "AAVE V3 Pool"
address = "0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2"
impl_address = "0x5faab9e1adbddad0a08734be8a52185fd6558e14"
events = ["Supply", "Borrow"]
from_block = 17000000
to_block = 17100000

# All events from a verified contract
[[contracts]]
address = "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
events = ["*"]
from_block = 16000000
to_block = 16100000

# Custom / unverified contract — abi_override still required
[[contracts]]
name = "Internal contract"
address = "0xAbc..."
events = ["MyEvent"]
from_block = 18000000
to_block = 18010000
abi_override = "./abis/MyContract.json"
```

## Success criteria

- `cargo test --workspace` passes.
- `cargo clippy --workspace --all-targets -- -D warnings` passes.
- A contract config without `abi_override` successfully indexes events for a Sourcify-verified
  contract on mainnet.
- A proxy config with `impl_address` indexes events whose signatures exist in the implementation
  ABI.
- A contract with `events = ["*"]` indexes all events emitted by that contract address.
- `events = ["Transfer", "*"]` produces a config error at startup.
- A missing/invalid `abi_override` file falls back to Sourcify with a visible warning.
- Second `scopenode sync` for the same address skips the Sourcify network call (SQLite cache hit).
- Sourcify 404 for a contract with no `abi_override` produces an actionable error ("add
  abi_override to provide the ABI manually").
