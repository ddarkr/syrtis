# syrtis-core

`syrtis-core` is the Syrtis-owned Rust core. It owns the session parsers,
source scanner, message cache, pricing, and aggregation used by the app.
App-specific FFI, C ABI, UI, and product wiring remain in `crates/tb_core_ffi`
and the Swift sources.

## Build and test

Rust 1.96.1 is pinned in the workspace-root [`rust-toolchain.toml`](../../rust-toolchain.toml).
Run the workspace gates from the repository root:

```bash
cargo fetch --locked
cargo build --release --locked --offline
cargo test --locked --offline
cargo clippy --workspace --locked --offline --all-targets
```

## Durable local usage store and visitors

The app-owned `usage/source-message-cache.sqlite3` stores normalized facts,
not transcript bodies. File observations, SQLite source reconciliation,
retained history across parsers, verified Codex resume membership, and cohort
selection share the backend. Successful reads remain available after original
files or databases are removed or truncated, and query-scope eviction does not
delete them. Restored stable identities reconcile rather than count twice.

First ingestion safely imports the captured legacy SQLite store, including its
committed WAL, or supported shards when no database exists. The legacy inputs
remain untouched; clearing them after migration does not clear durable usage.
The retired unowned monolith remains inert. Format-3/4/5 decoding is separate
from current warm-cache and resume eligibility, including for non-warm rows.
See the canonical [storage contract](../../docs/knowledge/architecture.md#compact-usage-store)
for root overrides, source isolation, and migration publication.

Claude observations reconcile before the query generation is published: an
explicit uncached-input/cache split supersedes a bare whole-prompt input value,
in either rewrite order, while comparable buckets retain their maxima.
Changed-source and warm queries read the same canonical generation. Producer
rows keep live authority; history-only rows stay retained across writer order.

`synchronize_local_usage(context, options, selection)` verifies complete source
reads and atomically publishes a `LocalUsageSnapshot`. Source identity includes
the effective resolved context and relevant siblings. A readable empty SQLite
WAL and an absent WAL both mean no uncheckpointed frames; unreadable sidecars
fail, nonempty WALs remain fingerprinted, and the primary database is checked
independently. Failed or partial ingestion does not replace the last complete
scope. Recovery preserves retained history rather than silently returning an
empty successful result.

Each source is verified at its own read boundary. Publication validates the
pinned source generations rather than requiring every source to stay unchanged
until all other sources finish. A later append, rewrite, or removal belongs to
the next synchronization; it does not invalidate facts already captured from a
complete read. Consumers must retain a pre-ingestion watch baseline so those
later changes remain dirty. This is not a simultaneous snapshot of all producers.

`visit_local_usage_snapshot(snapshot, query, filter, &mut sink)` reads that
generation without source discovery, parsing, or pricing requests.
`capture_local_usage_pricing(&context)` uses the existing local pricing policy;
`visit_priced_local_usage_snapshot` accepts that captured service for coherent
priced queries. The snapshot also binds the store's persistent instance epoch:
recreating a cache cannot make an old handle valid by reusing an integer
generation. Stale or evicted snapshots fail before invoking caller callbacks.
Prepared, file-backed query state releases publication/read transactions before
predicates or sinks run, so a visitor can synchronize another scope safely.
`generate_local_graph_from_snapshot`, `get_model_report_from_snapshot`,
`get_hourly_report_from_snapshot`, and `get_agents_report_from_snapshot` reuse
the compatibility folds without rediscovery or pricing HTTP. They require an
unpruned Report snapshot with the exact producing scan-client scope; querying
a different client subset requires its own scope rather than post-filtering a
mixed ownership decision.


Selection preserves source traversal, first-duplicate order, OpenCode aliases,
provider authority, and filter-dependent report fallback. Local-client selection
remains independent of query filters; independent Claude discovery, retained/live
precedence, and Hermes/GJC dedup-before-client-filter behavior remain intact.
Inclusive lower/exclusive upper timestamp gates apply after authority selection.
`visit_recent_local_messages(options, cutoff_ms, &mut sink)` is the same
local-client selection followed by an inclusive cutoff, without pricing HTTP.

Decoded producer batches flush at 1,024 rows or 8 MiB, whichever comes first.
One oversized record is an explicit exception; the 16 MiB SQLite page-cache
setting is a tuning target, not a process-memory ceiling. Selection, alias,
resume, session, and time-fold work spills to disk. Materialized compatibility
APIs still return vectors, and unrelated parsers may buffer one source file.
LocalClients publication persists immutable selected-row references, not another
message payload copy. Repeated range queries seek the timestamp index and
hydrate only selected range facts and requested aliases. Full-domain catalog
and undated queries still read their requested domain; initial selection and
filter-dependent Report preparation remain history-scaled I/O. No app memory
budget or measured CPU/I/O improvement is claimed.

Current verification belongs to Syrtis's [verification contract](../../docs/knowledge/verification.md);
historical fork counts are not evidence for later edits. Windows runtime and
full steady-state resource acceptance remain separate checks. The inherited
[local patch ledger](UPSTREAM.md#local-patches-diverged-from-upstream) records
provenance, not a separate engine publication prerequisite.

## Contributing

[`CONTRIBUTING.md`](../../CONTRIBUTING.md) owns contributor guidance.
[`architecture.md`](../../docs/knowledge/architecture.md) and
[`verification.md`](../../docs/knowledge/verification.md) own engine contracts
and checks. Engine and app changes share this repository; no separate PR or
engine publication is required.

## Upstream and license

The inherited implementation comes from
[junhoyeo/tokscale](https://github.com/junhoyeo/tokscale).
[`UPSTREAM.md`](UPSTREAM.md) preserves the extraction baseline, 111-row audit,
and historical patches as provenance, not a release or source-equivalence gate.
This project retains upstream MIT attribution; see [`LICENSE`](LICENSE).
