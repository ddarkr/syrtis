---
status: active
id: vendor-readme
kind: reference
scope: repository
read_when: advancing or auditing the vendor/tokscale-core consumer pin
last_verified: 2026-10-08
sources: [".gitmodules", "vendor/tokscale-core", "docs/knowledge/vendor-tokscale.md"]
---

# Shared Rust core pin

TokenBar consumes a pinned shared engine as a Git submodule. This branch uses
a reviewed local fork; the existing configured remote is unchanged.
Consumer integration rules are documented in
[`docs/knowledge/vendor-tokscale.md`](../docs/knowledge/vendor-tokscale.md).

| Field | Value |
|---|---|
| Path | `vendor/tokscale-core` |
| Repository | `https://github.com/Nanako0129/tokscale-core.git` |
| Reviewed pin | `890a306e908ab1e61bcd006285782768a8087593` |
| Upstream and local-patch ledger | Pinned [`UPSTREAM.md`](tokscale-core/UPSTREAM.md) |

## Ownership

The shared repository owns parsers, scanning, cache behavior, pricing, and
aggregation. TokenBar owns the gitlink, root `Cargo.lock`,
`crates/tb_core_ffi`, `Sources/CTB/include/ctb.h`, Swift code, and application
build wiring.

Local shared Rust edits in `vendor/tokscale-core` are allowed and must be
verified with the consuming app. No separate engine commit, review, or
upstream integration is required; commit or push only with explicit approval.

## Checkout

This local fork commit has not been published to the configured remote.
The current checkout has its objects; a fresh recursive clone cannot fetch
this pin until the fork is published and its remote is configured. No push,
PR, remote change, release, or Windows runtime verification is implied.

Clone recursively:

```bash
git clone --recurse-submodules https://github.com/Nanako0129/TokenBar.git
```

For an existing checkout:

```bash
git submodule update --init --recursive
```

Local builds may use modified submodule source or a checkout differing from
the gitlink. Record the tested source state; a gitlink alone does not capture
uncommitted edits or make unpublished objects available to fresh clones.
