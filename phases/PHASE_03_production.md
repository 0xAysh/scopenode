# Phase 3 — Production-Ready

## Goal

Complete scopenode into a production-grade tool. Add live sync with reorg
handling, full developer-experience CLI commands, REST API with SSE streaming,
webhooks, and optional Claude API integration. After this phase, it's something
you'd actually use and recommend to others.

---

## What we build

1. **Live sync** — watch new blocks after historical sync completes
2. **Reorg handling** — detect chain splits, soft-delete affected events
3. **Full CLI** — `init`, `doctor`, `retry`, `export` commands
4. **Full progress TUI** — real numbers, ETA, peer count, events found
5. **REST API at `:8546`** — `GET /events`, `GET /status`, SSE stream
6. **Webhooks** — POST new events to external URLs during live sync
7. **Claude API** — optional `-m "natural language"` config generation

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

### Server-Sent Events (SSE)

SSE is a W3C standard for server → client streaming over plain HTTP. Unlike
WebSockets, it's one-directional and automatically reconnects.

Wire format:
```
data: {"event":"Swap","block":19000042,...}\n\n
```

Browser API:
```js
const source = new EventSource('http://localhost:8546/stream/events?contract=0x8ad...')
source.onmessage = (e) => console.log(JSON.parse(e.data))
```

We use a `tokio::sync::broadcast` channel internally. Live syncer publishes
events. Each SSE connection subscribes and streams to the client.

### Webhooks — reliability model

A webhook is a fire-and-forget HTTP POST. Key design decisions:
- Max 3 retries with exponential backoff (1s, 2s, 4s)
- Run in a separate `tokio::spawn` — never block live sync
- On failure: log warning, don't crash, don't re-queue
- Include `X-Scopenode-Event` header for routing on the receiver side

### Claude API integration

The `-m "natural language"` flag calls Claude API to:
1. Parse intent (contract address, event names, block range)
2. Look up ABI from Etherscan if address is provided
3. Generate a `config.toml` and print it
4. Ask user to confirm before starting sync

This requires `ANTHROPIC_API_KEY` env var. If not set, the flag is unavailable
but everything else works normally.

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
                        // Never exit — log and continue
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
                // Mark orphaned events as reorged
                let hashes: Vec<_> = orphaned.iter().map(|h| format!("{h:?}")).collect();
                let n = self.pipeline.db.events().mark_reorged(&hashes).await?;
                tracing::warn!(orphaned = orphaned.len(), reorged_events = n, "Reorg handled");

                // Re-process winning chain
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
                continue; // bloom says no
            }

            let fetch = self.pipeline.fetch_and_verify(header).await?;
            let decoded = self.pipeline.decode(&fetch, contract).await?;
            self.pipeline.db.events().insert_events(&decoded, fetch.source).await?;

            for event in &decoded {
                // Broadcast to SSE subscribers
                let _ = self.live_events_tx.send(LiveEvent::from(event));

                // Fire webhook if configured
                if let Some(ref url) = contract.webhook {
                    let sender = WebhookSender::new();
                    let ev = LiveEvent::from(event);
                    let u = url.clone();
                    tokio::spawn(async move { sender.send(u.as_str(), &ev).await });
                }
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
                // Reorg! Find common ancestor by walking parent_hash chain
                let orphaned: Vec<B256> = self.recent.iter()
                    .rev()
                    .take_while(|(_, h)| *h != new.parent_hash)
                    .map(|(_, h)| *h)
                    .collect();

                // Fetch the new chain from DB or beacon
                let new_chain = db.headers()
                    .get_chain_from(new.parent_hash, orphaned.len())
                    .await?;

                // Update buffer
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

### REST API

```rust
// crates/scopenode-rpc/src/rest.rs

use axum::{Router, routing::get, extract::{Query, Path, State}};
use tower_http::cors::CorsLayer;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/events",          get(get_events))
        .route("/status",          get(get_status))
        .route("/contracts",       get(get_contracts))
        .route("/abi/:address",    get(get_abi))
        .route("/stream/events",   get(stream_events))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn get_events(Query(p): Query<EventsParams>, State(s): State<AppState>)
    -> impl IntoResponse
{
    let rows = s.db.events().query_events(EventQuery {
        contract: p.contract,
        event_name: p.event,
        from_block: p.from_block,
        to_block: p.to_block,
        limit: p.limit.or(Some(100)),
        offset: p.offset,
    }).await;

    match rows {
        Ok(rows) => Json(json!({ "events": rows, "count": rows.len() })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn stream_events(
    Query(p): Query<EventsParams>,
    State(s): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = s.live_events_tx.subscribe();
    let contract_filter = p.contract;
    let event_filter = p.event;

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if contract_filter.as_deref().map(|c| ev.contract == c).unwrap_or(true)
                        && event_filter.as_deref().map(|e| ev.event == e).unwrap_or(true)
                    {
                        let data = serde_json::to_string(&ev).unwrap_or_default();
                        yield Ok(Event::default().data(data));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "SSE subscriber lagged");
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
```

### `scopenode init` — interactive wizard

```rust
// crates/scopenode/src/commands/init.rs

use dialoguer::{Input, Select, Confirm, MultiSelect};

pub async fn run() -> Result<()> {
    println!("Welcome to scopenode. Let's create a config.toml.\n");

    let address: String = Input::new()
        .with_prompt("Contract address (0x...)")
        .validate_with(|s: &String| {
            s.parse::<Address>().map(|_| ()).map_err(|_| "Invalid Ethereum address")
        })
        .interact_text()?;

    let addr: Address = address.parse().unwrap();

    // Check for proxy
    println!("Checking for proxy...");
    let abi_addr = resolve_abi_address(addr, &provider).await?;
    if abi_addr != addr {
        println!("  Proxy detected → using implementation ABI from {abi_addr}");
    }

    // Fetch events
    println!("Fetching ABI from Etherscan...");
    let events = etherscan.fetch_events(abi_addr).await?;
    let event_names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();

    let selected = MultiSelect::new()
        .with_prompt("Select events to sync (space to select, enter to confirm)")
        .items(&event_names)
        .interact()?;

    let chosen_events: Vec<String> = selected.iter()
        .map(|&i| event_names[i].to_string())
        .collect();

    // Block range
    let from_str: String = Input::new()
        .with_prompt("From block (number or 'deploy' for contract deploy block)")
        .default("latest - 10000".into())
        .interact_text()?;

    let from_block = parse_block_ref(&from_str, &provider).await?;

    let live = Confirm::new()
        .with_prompt("Continue syncing new blocks after historical sync? (live mode)")
        .default(true)
        .interact()?;

    // Write config
    let config_str = format!(r#"[node]
port = 8545

[[contracts]]
address = "{addr}"
events = [{}]
from_block = {from_block}
{to_block_line}
"#,
        chosen_events.iter().map(|e| format!(r#""{e}""#)).collect::<Vec<_>>().join(", "),
        to_block_line = if live { "# to_block not set = live sync".into() }
                        else { "to_block = # fill in".into() }
    );

    std::fs::write("config.toml", &config_str)?;
    println!("\nWrote config.toml");

    let start = Confirm::new()
        .with_prompt("Start syncing now?")
        .default(true)
        .interact()?;

    if start {
        crate::commands::sync::run("config.toml".into(), false, false).await?;
    }

    Ok(())
}
```

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

    println!("\nNetwork");
    println!("  Portal bootstrap:  {}", check_icon(test_portal_bootstrap().await.is_ok()));
    println!("  Etherscan API:     {}", check_icon(test_etherscan().await.is_ok()));

    Ok(())
}

fn check_icon(ok: bool) -> impl std::fmt::Display {
    if ok { style("✓ connected").green() } else { style("✗ failed").red() }
}
```

### `scopenode export` command

```rust
// crates/scopenode/src/commands/export.rs

pub async fn run(contract: Option<String>, event: Option<String>, format: String, output: Option<PathBuf>)
    -> Result<()>
{
    let db = Db::open_default().await?;
    let rows = db.events().query_events(EventQuery {
        contract, event_name: event, ..Default::default()
    }).await?;

    let writer: Box<dyn std::io::Write> = match output {
        Some(path) => Box::new(std::fs::File::create(path)?),
        None => Box::new(std::io::stdout()),
    };

    match format.as_str() {
        "json" => write_json(rows, writer)?,
        "csv" => write_csv(rows, writer)?,
        "parquet" => write_parquet(rows, writer)?,  // using `parquet` crate
        other => return Err(anyhow::anyhow!("Unknown format: {other}. Use json, csv, or parquet")),
    }

    Ok(())
}
```

### Claude API integration

```rust
// crates/scopenode-core/src/claude.rs

use reqwest::Client;

/// Parse a natural language description of what to sync.
/// Returns a Config struct ready to be serialized to TOML.
/// Requires ANTHROPIC_API_KEY env var.
pub async fn parse_intent(message: &str) -> Result<Config, ClaudeError> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .map_err(|_| ClaudeError::NoApiKey)?;

    let client = Client::new();

    let system_prompt = r#"
You are helping configure scopenode, an Ethereum event indexer.
Given a user's description, extract:
- contract_address (required)
- event_names (list of event names they want)
- from_block (starting block number or "latest - N")
- to_block (optional ending block number; omit for live sync)

Respond with JSON matching this schema:
{
  "contract": "0x...",
  "events": ["EventName1", "EventName2"],
  "from_block": 12345678,
  "to_block": null
}
Only JSON. No prose.
"#;

    let resp: serde_json::Value = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&serde_json::json!({
            "model": "claude-opus-4-6",
            "max_tokens": 1024,
            "system": system_prompt,
            "messages": [{ "role": "user", "content": message }]
        }))
        .send().await?
        .json().await?;

    let content = resp["content"][0]["text"].as_str()
        .ok_or(ClaudeError::InvalidResponse)?;

    let parsed: serde_json::Value = serde_json::from_str(content)?;

    // Build and return Config
    Ok(Config {
        node: NodeConfig::default(),
        contracts: vec![ContractConfig {
            address: parsed["contract"].as_str().unwrap_or("").parse()?,
            events: parsed["events"].as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            from_block: parsed["from_block"].as_u64().unwrap_or(0),
            to_block: parsed["to_block"].as_u64(),
            ..Default::default()
        }],
    })
}
```

Used in `main.rs`:
```rust
// If -m flag provided
if let Some(ref message) = cli.message {
    let config = claude::parse_intent(message).await?;
    let toml = toml::to_string_pretty(&config)?;
    println!("Generated config:\n\n{toml}");
    let confirm = dialoguer::Confirm::new()
        .with_prompt("Start sync with this config?")
        .interact()?;
    if confirm {
        commands::sync::run_with_config(config, false, cli.quiet).await?;
    }
    return Ok(());
}
```

---

## `scopenode status` output

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

Node:  localhost:8545 (JSON-RPC)  |  localhost:8546 (REST)
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
Events found so far: 12,439 Swap  |  844 Mint  |  601 Burn
Failed blocks (pending retry): 3
```

---

## REST API reference

```
GET /events?contract=0x8ad...&event=Swap&fromBlock=17000000&limit=100
GET /events?contract=0x8ad...&event=Swap&offset=100&limit=100
GET /status
GET /contracts
GET /abi/0x8ad599c3...
GET /stream/events?contract=0x8ad...&event=Swap    ← SSE
```

---

## Dependency additions

```toml
# Add to workspace
axum = "0.7"
tower-http = { version = "0.5", features = ["cors"] }
async-stream = "0.3"
dialoguer = "0.11"       # interactive prompts (init wizard)
parquet = "51"           # parquet export
indicatif = "0.17"       # progress bars (already in Phase 1)
```

---

## Tests

```
Unit:
  - ReorgDetector detects single-block reorg correctly
  - ReorgDetector: no reorg on normal chain extension
  - LiveEvent serializes to valid JSON
  - Webhook retries on HTTP 500 (mock server)
  - Webhook does not crash live sync on failure

Integration (--ignored):
  - SSE stream receives events as they're inserted
  - GET /events returns same results as eth_getLogs
  - scopenode init generates valid config.toml
  - Reorg: insert events for block A, mark reorged, verify hidden from API
  - Export CSV has correct headers and values
```

---

## Definition of done

- [ ] Live sync runs indefinitely in background after historical sync
- [ ] Reorg detected and handled: orphaned events marked `reorged = 1`, hidden
  from all query interfaces
- [ ] `scopenode init` walks through wizard and produces valid config.toml
- [ ] `scopenode doctor` shows portal, beacon, DB, and network status
- [ ] `scopenode retry` re-fetches all `pending_retry` blocks
- [ ] `scopenode export --format csv > events.csv` produces valid CSV
- [ ] `GET /events` returns correctly filtered JSON
- [ ] `GET /stream/events` delivers events via SSE in real-time
- [ ] Webhook POSTs arrive within 1 second of a live event
- [ ] `scopenode -m "..."` generates config from natural language (with API key)
- [ ] All previous phase tests still pass

---

## What you learn in this phase

**Reorgs:** Why they happen, post-Merge finality guarantees, the parent_hash
chain, common ancestor algorithm, why soft-delete is the right pattern.

**SSE:** Wire format, the `broadcast` channel fan-out pattern, what happens
when a subscriber is too slow (lagged), automatic reconnect behavior.

**Webhooks:** Fire-and-forget async with `tokio::spawn`, why webhook errors
must not propagate, exponential backoff, making external integrations reliable.

**Async patterns:** `tokio::select!` for shutdown signals, `oneshot` channel
for one-time stop, structured concurrency in long-running background tasks.

**CLI UX:** What makes a tool feel professional — confirmations before
destructive actions, clear error messages with suggested next steps, the
`doctor` command as a debugging affordance, progress with real numbers.

**LLM integration:** How to write effective system prompts for structured output,
JSON schema validation, graceful degradation when API key is absent.
