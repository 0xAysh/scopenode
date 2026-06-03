# scopenode Context

## Domain Terms

**ERA1 source** — A local directory of `.era1` archive files used as the only historical chain data source. The ERA1 source owns file discovery, manifest facts, checksum status, range coverage, and decoded block fact streaming.

**Contract scope** — One configured contract address, event selection, ABI resolution settings, and inclusive block range to index.

**Block fact** — The decoded facts scopenode needs from one ERA1 block: block number, header, receipts, and transaction hashes.

**ABI resolution** — The process of finding event ABIs for a contract scope through the SQLite cache, local ABI file, or configured remote fetch adapter.

**Event decoder** — The module that compiles event ABI definitions, matches receipt logs by contract/topic, decodes indexed and non-indexed fields, and produces decoded events.

**Decoded event** — A matched Ethereum log enriched with decoded JSON fields and storage-ready block, transaction, and log identity.

**Coverage** — A recorded contract/block range that has been processed. Query callers use coverage to fail loudly when a request asks for data outside local indexed scope.

**Event query** — A domain-level request for indexed decoded events, including contract, event name, topic0, block range, limit, and offset.

**Query adapter** — A transport-facing module that turns event query outcomes into JSON-RPC or REST responses.

**Event sink** — A pipeline adapter that receives decoded events and coverage facts. SQLite and in-memory sinks are the current adapters.

