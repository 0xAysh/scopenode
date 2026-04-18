# Webhooks — Design Spec

**Date:** 2026-04-17
**Phase:** 4 — Developer APIs
**Status:** Approved

---

## Goal

Push live events to external HTTP endpoints as they are indexed. Webhooks
let applications react to on-chain events without polling `GET /events`.
They fire only for live events (blocks arriving after historical sync
completes). Historical data is served by the query API.

---

## Architecture

```
broadcast::Sender<StoredEvent>   (live.rs — unchanged)
         │
         │  broadcast::Receiver
         ▼
  WebhookDispatcher              (new: crates/scopenode-core/src/webhook.rs)
         │
         │  tokio::spawn per delivery
         ▼
  deliver_with_retry(url, body, headers)
         │
         ├── attempt 1 → 2xx → done
         ├── attempt 1 → fail → sleep 1s → attempt 2
         ├── attempt 2 → fail → sleep 2s → attempt 3
         └── attempt 3 → fail → warn!() → drop
```

`WebhookDispatcher` is constructed in `commands/sync.rs` alongside the SSE
task and spawned as `tokio::spawn(dispatcher.run())`. No changes to
`LiveSyncer` or `Pipeline`.

**URL routing per event (in order):**
1. `contract.webhook_events.get(event_name)` → use if present
2. `contract.webhook` → fallback if set
3. Neither set → skip silently

---

## Config

Two new optional fields on `ContractConfig`. Both are optional and
combinable — users choose whichever granularity they need.

```toml
[[contracts]]
address        = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
events         = ["Swap", "Mint", "Burn"]

# Catches any event with no specific override
webhook        = "https://myapp.com/hooks/all"

# HMAC-SHA256 secret — shared across all webhook URLs on this contract
webhook_secret = "my-secret-key"

# Per-event overrides — Swap goes here instead of the contract-level URL
webhook_events = { Swap = "https://myapp.com/hooks/swaps" }
```

**Rust struct additions to `ContractConfig`:**

```rust
pub webhook: Option<Url>,
pub webhook_secret: Option<String>,
#[serde(default)]
pub webhook_events: HashMap<String, Url>,
```

**Validation** (added to `Config::validate()`): if `webhook_events` contains
an event name not in `events`, emit a `warn!()` — it is likely a typo. Not
a hard error since the extra entry is harmless.

---

## Payload Format

Every POST body is a JSON object:

```json
{
  "event":        "Swap",
  "contract":     "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8",
  "block_number": 19842301,
  "block_hash":   "0xabc...",
  "tx_hash":      "0xdef...",
  "tx_index":     12,
  "log_index":    3,
  "timestamp":    1713388800,
  "decoded":      { "amount0": "-1200000", "amount1": "3841000000" }
}
```

**Headers sent on every request:**

```
Content-Type:          application/json
X-Scopenode-Event:     Swap
X-Scopenode-Contract:  0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8
X-Scopenode-Signature: sha256=<hmac-hex>   ← only when webhook_secret is set
```

**Signature:** `HMAC-SHA256(key=webhook_secret, msg=raw_body_bytes)`, hex-encoded
with `sha256=` prefix. Matches GitHub/Stripe convention so receivers can reuse
existing verification middleware.

---

## Retry & Delivery

```rust
async fn deliver_with_retry(
    client: &reqwest::Client,
    url: &Url,
    body: Bytes,
    headers: HeaderMap,
) {
    let delays = [Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(4)];
    for (attempt, delay) in delays.iter().enumerate() {
        match client.post(url).headers(headers.clone()).body(body.clone()).send().await {
            Ok(r) if r.status().is_success() => return,
            Ok(r) => warn!(status = %r.status(), attempt, "Webhook non-2xx"),
            Err(e) => warn!(err = %e, attempt, "Webhook request failed"),
        }
        if attempt < 2 {
            tokio::time::sleep(*delay).await;
        }
    }
    warn!(url = %url, "Webhook giving up after 3 attempts");
}
```

**Constraints:**
- Each delivery runs in `tokio::spawn` — dispatcher loop never blocks on HTTP
- `reqwest::Client` shared across all deliveries (connection pool reuse)
- 10s timeout per attempt — hung servers cannot stall retries
- Non-2xx responses (including 4xx) count as failure and trigger retry
- No persistent queue — after 3 attempts the event is dropped with a `warn!`
- Live sync is never blocked or crashed by webhook failures

---

## Testing

All tests in `crates/scopenode-core/src/webhook.rs`. HTTP mocked with
`wiremock` — no real servers, fully deterministic.

### URL routing
- Contract-level webhook fires when no event-level override is set
- Event-level webhook overrides contract-level for matching event name
- Both levels set: event-level wins for matched event, contract-level fires for others
- Event with no webhook config → no POST sent

### Retry behaviour
- Success on first attempt → one POST, no delay
- Fail then succeed → two POSTs, `warn!` on first failure
- Three consecutive failures → three POSTs, "giving up" warn, no panic

### HMAC signing
- `X-Scopenode-Signature` header present when `webhook_secret` configured
- Header absent when no secret configured
- HMAC value matches `HMAC-SHA256(secret, body)` computed independently
- Different secrets produce different signatures

### Dispatcher lifecycle
- Broadcast event with matching contract → delivery spawned
- Broadcast event with no webhook config → no delivery spawned
- Broadcast channel closed → `run()` returns cleanly (no hang)

---

## New Dependencies

```toml
# Cargo.toml (workspace)
hmac = "0.12"
sha2 = "0.10"

# dev-dependencies (scopenode-core)
wiremock = "0.6"
```

`reqwest` is already in the workspace. No other new deps required.

---

## Files Changed

| File | Change |
|---|---|
| `crates/scopenode-core/src/webhook.rs` | New — `WebhookDispatcher`, `deliver_with_retry`, HMAC signing, tests |
| `crates/scopenode-core/src/lib.rs` | Export `webhook` module |
| `crates/scopenode-core/src/config.rs` | Add `webhook`, `webhook_secret`, `webhook_events` to `ContractConfig` |
| `crates/scopenode-core/Cargo.toml` | Add `hmac`, `sha2`; add `wiremock` to dev-deps |
| `crates/scopenode/src/commands/sync.rs` | Construct and spawn `WebhookDispatcher` |
| `config.example.toml` | Document new fields |

---

## Definition of Done

- [ ] `webhook = "..."` in config delivers a POST for every live event on that contract
- [ ] `webhook_events = { Swap = "..." }` overrides the contract-level URL for Swap only
- [ ] Non-overridden events fall through to `webhook` if set
- [ ] `X-Scopenode-Signature` header present and correct when `webhook_secret` set
- [ ] Non-2xx response → retry up to 3 times with 1s/2s/4s backoff
- [ ] Webhook failure never stalls or crashes live sync
- [ ] Typo in `webhook_events` event name → `warn!` at startup, not a hard error
- [ ] All unit tests pass (routing, retry, HMAC, dispatcher lifecycle)
- [ ] `cargo build` and `cargo clippy -- -D warnings` clean
