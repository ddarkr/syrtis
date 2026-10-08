---
status: active
id: kb-decision-0003
kind: canonical
scope: repository
read_when: evaluating an upstream-derived fix, preserving Syrtis adaptations, or deciding whether to add a client
last_verified: 2026-10-08
sources: ["Cargo.toml", "crates/syrtis_core/Cargo.toml", "vendor/README.md", "docs/knowledge/vendor-tokscale.md", "docs/knowledge/plans/tokscale-alignment.md", "public issue #45"]
---

# ADR 0003: Selective upstream alignment

## Decision

Selectively adopt useful upstream commits or hunks into Syrtis-owned
`crates/syrtis_core`. The engine is ordinary source in this repository:
no separate engine commit, pin, review, publication, or source-equivalence
gate is required. Inspect the actual change rather than treating a commit
title as a complete description of its runtime effect.

## Selection matrix

| Classification | Action |
|---|---|
| Already in engine | Record exact evidence and do not reapply |
| Correctness take | Land the smallest complete change and a regression fixture in `crates/syrtis_core` |
| Shared adaptation | Preserve engine streaming, cache, mtime, pricing, and report semantics while porting parser logic |
| Defer | Keep the public rationale and wait for a product or architecture decision |
| Skip | Record why the code is outside the shared engine's surface |
| Superseded | Point to the newer source of truth and remove stale bookkeeping |

## Rationale

The engine carries streaming aggregation, durable usage, cache identity,
client filtering, pricing, and discovery that can be lost by whole-file
replacement even when the result compiles. Selective adoption preserves
those contracts. The inherited `UPSTREAM.md` is provenance; current app and
engine ownership remain together in Syrtis.

> **Schema rule：** If a selected change alters serialized parser output,
> dedup keys, or attribution, advance the shared engine's invalidation state
> and prove stale-cache rebuild. That state is two counters, not one:
> `CACHE_FORMAT_VERSION` for the serialized layout and cross-client types,
> `parser_version(<client>)` for one client's parse semantics. Prefer the
> per-client counter for a parser fix. tokscale upstream's counters are not
> the shared engine's counters.

## Public tracking

Issue [#45](https://github.com/Nanako0129/TokenBar/issues/45) records the rolling inventory and deferred capability decisions. It is a decision surface and completeness ledger, not a requirement to implement every upstream client. The current plan routes correctness first and keeps product-expansion items behind explicit scope decisions.
