# scopenode Context

This file defines the canonical vocabulary for the current architecture.

## Product boundary

**scopenode** — A local, bounded Ethereum event indexer backed by execution
history archive files. It is not a full node, live peer, state database, or
complete JSON-RPC implementation.

**Archive source** — A local directory containing supported `.era1` and `.ere`
execution-history files. The source layer owns discovery, filename metadata,
checksum status, block-range inspection, file selection, and block-fact streams.
Some code retains the historical `Era1*` name while supporting both formats.

**Contract scope** — One configured contract address, selected event names,
inclusive block range, and ABI resolution settings.

**Union range** — The minimum `from_block` through maximum `to_block` across all
contract scopes. Archive files are selected once against this range, then each
block is evaluated against every applicable scope.

## Sync domain

**Block fact** — The archive-decoded inputs needed by the pipeline for one block:
block number, header, receipts, and transaction hashes.

**Block fact stream** — The Archive source's traversal product: every selected
Block fact as one ordered stream plus a progress total. File selection, file
lifecycle, and format dispatch stay behind this interface; open and read
failures surface as stream items carrying the file path. The pipeline consumes
the stream and never coordinates archive files.

**ABI resolution** — Resolution of event definitions through the contract ABI
cache, a local override, or the remote Sourcify adapter. `impl_address` changes
the remote lookup address for proxies without changing the log address.

**Prepared scope** — A validated contract scope paired with a compiled
`EventDecoder`, ready for repeated block evaluation.

**Event decoder** — Compiled event definitions used to compute topic signatures,
match logs by address/topic, and decode indexed and non-indexed fields.

**Decoded event** — A matched Ethereum log enriched with decoded JSON and
storage-ready block, transaction, log, source, and timestamp identity.

**Receipt verification** — Reconstruction of the Ethereum receipt trie and
comparison with the block header's `receipts_root`.

**Pipeline sink** — The combined event and coverage output interface. SQLite and
the in-memory test sink are current adapters.

**Block outcome** — The per-block evaluation result. Preserves outside-range,
no-Bloom-match, valid emptiness, decoded events, verification failure, and
decode failure as distinct facts until Coverage eligibility is decided. An
empty block is valid emptiness, not a failure.

**Coverage** — A contract, inclusive block range, and source recorded only after
the scope completes without a read, verification, decode, or storage failure.
An incomplete block (verification or decode failure) makes every affected
contract scope ineligible for Coverage for that run; a successful rerun
recovers. Lossy decoded events are still stored with raw topics and data.

**Sync report** — The per-run completion summary: total stored events, covered
contract scopes, and incomplete scopes with their failure reason. A Coverage
write failure is reported as incomplete, never as unqualified success.

## Query domain

**Event query** — A transport-independent request containing optional contract,
event name, topic0, block bounds, limit, and offset.

**Event query outcome** — One of `Results`, `Empty`, `NotIndexed`,
`MissingCoverage`, or `Capped`. These meanings are explicit domain states, not
inferred from HTTP codes or row counts.

**Filter plan** — Normalization of JSON-RPC or REST inputs into an `EventQuery`
or an explicit unsupported/missing-address result.

**Query front door** — The shared RPC/REST path that executes a filter plan and
maps storage outcomes without duplicating policy in each transport.

**Projection** — Conversion from stored event rows into JSON-RPC logs or REST
event responses.

**Decode quality** — `Valid`, `Lossy`, or `Invalid` quality attached to parsing
and projection fallbacks so malformed stored data is not silently treated as
fully valid.

