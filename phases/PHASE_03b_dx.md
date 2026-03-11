# Phase 3b — Developer Surface Area

## Goal

Add the developer-facing features that make scopenode convenient beyond the
core pipeline. REST API, SSE streaming, webhooks, interactive wizard, data
export, and optional Claude API integration. Each feature is independent —
they can be shipped in any order.

**Prerequisite:** Phase 3a (live sync and reorg handling must work first, since
SSE and webhooks depend on the live event broadcast channel).

---

## Staging environment

All features in this phase are testable against `~/.scopenode-staging` using
`--data-dir` / `SCOPENODE_DATA_DIR`. Before testing the init wizard or export
commands against staging, snapshot the DB:

```bash
scopenode snapshot --label before-3b-test
# ... test ...
scopenode restore --label before-3b-test  # if needed
```

The `scopenode init` wizard should default to `~/.scopenode-staging` when
`SCOPENODE_DATA_DIR` is set, making it safe to run interactively in dev.

---

## What we build

1. **REST API at `:8546`** — `GET /events`, `GET /status`, `GET /contracts`, SSE stream
2. **Webhooks** — POST new events to external URLs during live sync
3. **`scopenode init`** — interactive wizard, generates config.toml
4. **`scopenode export`** — CSV/JSON/Parquet output
5. **Claude API integration** — optional `-m "natural language"` config generation (last priority)

---

## Concepts to understand deeply

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

We use a `tokio::sync::broadcast` channel internally (set up in Phase 3a).
Live syncer publishes events. Each SSE connection subscribes and streams to
the client.

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
but everything else works normally. This is the lowest-priority item in 3b.

---

## Implementation

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

### Webhook sender

```rust
// crates/scopenode-core/src/webhook.rs

pub struct WebhookSender {
    client: reqwest::Client,
}

impl WebhookSender {
    pub async fn send(&self, url: &str, event: &LiveEvent) {
        let body = serde_json::to_string(event).unwrap_or_default();

        for attempt in 0..3u32 {
            match self.client
                .post(url)
                .header("Content-Type", "application/json")
                .header("X-Scopenode-Event", &event.event_name)
                .body(body.clone())
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => return,
                Ok(resp) => {
                    tracing::warn!(
                        url, attempt, status = %resp.status(),
                        "Webhook returned non-success"
                    );
                }
                Err(e) => {
                    tracing::warn!(url, attempt, err = %e, "Webhook failed");
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1 << attempt)).await;
        }
        tracing::error!(url, "Webhook failed after 3 attempts — dropping");
    }
}
```

Integration with live sync (in `LiveSyncer::process_block` from Phase 3a):
```rust
if let Some(ref url) = contract.webhook {
    let sender = WebhookSender::new();
    let ev = LiveEvent::from(event);
    let u = url.clone();
    tokio::spawn(async move { sender.send(u.as_str(), &ev).await });
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

    println!("Checking for proxy...");
    let abi_addr = resolve_abi_address(addr, &provider).await?;
    if abi_addr != addr {
        println!("  Proxy detected → using implementation ABI from {abi_addr}");
    }

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

    let from_str: String = Input::new()
        .with_prompt("From block (number or 'deploy' for contract deploy block)")
        .default("latest - 10000".into())
        .interact_text()?;

    let from_block = parse_block_ref(&from_str, &provider).await?;

    let live = Confirm::new()
        .with_prompt("Continue syncing new blocks after historical sync? (live mode)")
        .default(true)
        .interact()?;

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
        "parquet" => write_parquet(rows, writer)?,
        other => return Err(anyhow::anyhow!("Unknown format: {other}. Use json, csv, or parquet")),
    }

    Ok(())
}
```

### Claude API integration

```rust
// crates/scopenode-core/src/claude.rs

use reqwest::Client;

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
# Add to workspace (Phase 3b only)
axum = "0.7"
tower-http = { version = "0.5", features = ["cors"] }
async-stream = "0.3"
dialoguer = "0.11"       # interactive prompts (init wizard)
parquet = "51"           # parquet export
```

---

## Tests

```
Unit:
  - Webhook retries on HTTP 500 (mock server)
  - Webhook does not crash live sync on failure
  - Webhook exponential backoff: 1s, 2s, 4s
  - Export CSV: correct headers and values for known events
  - Export JSON: valid JSON array
  - Claude parse_intent: returns ClaudeError::NoApiKey when env var missing

Integration (--ignored):
  - SSE stream receives events as they're inserted via broadcast channel
  - GET /events returns same results as eth_getLogs for indexed range
  - GET /status returns valid JSON with contract list
  - scopenode init generates valid config.toml (mocked Etherscan response)
  - Export parquet file readable by DuckDB/pandas
```

---

## Definition of done

- [ ] `GET /events` returns correctly filtered JSON from SQLite
- [ ] `GET /stream/events` delivers events via SSE in real-time
- [ ] Webhook POSTs arrive within 1 second of a live event
- [ ] `scopenode init` walks through wizard and produces valid config.toml
- [ ] `scopenode export --format csv > events.csv` produces valid CSV
- [ ] `scopenode export --format parquet > events.parquet` produces valid Parquet
- [ ] `scopenode -m "..."` generates config from natural language (with API key)
- [ ] All previous phase tests still pass

---

## What you learn in this phase

**SSE:** Wire format, the `broadcast` channel fan-out pattern, what happens
when a subscriber is too slow (lagged), automatic reconnect behavior.

**Webhooks:** Fire-and-forget async with `tokio::spawn`, why webhook errors
must not propagate, exponential backoff, making external integrations reliable.

**CLI UX:** What makes a tool feel professional — confirmations before
destructive actions, clear error messages with suggested next steps, interactive
prompts with validation, multiple output formats.

**LLM integration:** How to write effective system prompts for structured output,
JSON schema validation, graceful degradation when API key is absent.
