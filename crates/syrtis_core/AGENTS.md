# Syrtis-owned engine routing

Read [`docs/knowledge/README.md`](../../docs/knowledge/README.md), then
[`architecture.md`](../../docs/knowledge/architecture.md),
[`vendor-tokscale.md`](../../docs/knowledge/vendor-tokscale.md), and
[`verification.md`](../../docs/knowledge/verification.md) for engine changes.

This is ordinary Syrtis source, not a submodule or separately published
engine. Parser, scanner, durable usage, pricing, and aggregation changes may
land together with their app integration. Keep app FFI in
`crates/tb_core_ffi`; preserve the Rust -> C ABI -> Swift contract.

`LICENSE` and `UPSTREAM.md` retain attribution and inherited provenance.
They do not require a separate engine commit, reviewed pin, publication, or
source-equivalence gate for new Syrtis work.

Use the workspace toolchain and root `Cargo.lock`. Run the relevant locked
engine and consumer checks from the repository root. Report Windows checks
as unverified when a Windows runtime is unavailable; do not substitute a
macOS build for them. Commit, push, PR, and release need separate explicit
authorization.
