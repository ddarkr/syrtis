# Shared-engine task routing

Read [`vendor/README.md`](README.md) before changing the
`vendor/tokscale-core` pin. Then read
[`docs/knowledge/vendor-tokscale.md`](../docs/knowledge/vendor-tokscale.md) for
the shared-engine boundary and
[`docs/knowledge/verification.md`](../docs/knowledge/verification.md) for
required consumer evidence. Engine implementation work follows the pinned
engine's [`AGENTS.md`](tokscale-core/AGENTS.md).

## Invariants

| Boundary | Rule |
|---|---|
| Pin | `vendor/tokscale-core` remains a Git submodule; `vendor/README.md` records its source and pin. Local edits and a checkout differing from the gitlink are allowed for local builds. |
| Engine ownership | Edit shared Rust directly in `vendor/tokscale-core` for local fixes and verify it with the consuming app; no separate engine commit, review, or upstream integration is required. |
| Consumer ownership | TokenBar continues to own `crates/tb_core_ffi`, `Sources/CTB/include/ctb.h`, Swift code, build wiring, and the root `Cargo.lock`. |
| Parity | Engine edits and pin updates must verify affected materialized and streaming behavior, cache/schema consequences, FFI mappings, and Rust-to-Swift app gates. |
| Ledger | The engine's `UPSTREAM.md` owns the exact upstream and local-patch ledger. `vendor/README.md` records only TokenBar's source and pin. |

Do not commit, push, merge, or publish without explicit user authorization.
Return the diff and verification evidence to the user.
