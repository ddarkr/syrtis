---
status: active
id: vendor-readme
kind: reference
scope: repository
read_when: tracing the former shared-engine dependency or current engine ownership
last_verified: 2026-10-08
sources: ["Cargo.toml", "crates/syrtis_core/Cargo.toml", "docs/knowledge/vendor-tokscale.md"]
---

# Former shared Rust core

Syrtis owns its Rust engine as ordinary source in
[`crates/syrtis_core`](../crates/syrtis_core). The package is `syrtis-core`;
parsing, source synchronization, persistent usage, pricing, and aggregation are
changed and verified with the app in this repository. Builds do not fetch or
publish a separate engine repository and do not need recursive submodule setup.

The imported implementation derives from
[tokscale](https://github.com/junhoyeo/tokscale) and the former
[tokscale-core fork](https://github.com/Nanako0129/tokscale-core). Its MIT
[license](../crates/syrtis_core/LICENSE), attribution, and inherited
[upstream ledger](../crates/syrtis_core/UPSTREAM.md) remain with the source.
The ledger is historical provenance, not an integration or release gate.

The last local dependency checkpoint was
`890a306e908ab1e61bcd006285782768a8087593`; it is not a current consumer pin.
The source cutover also preserves local implementation changes made after that
checkpoint. Current ownership and data-retention rules live in
[`docs/knowledge/vendor-tokscale.md`](../docs/knowledge/vendor-tokscale.md) and
[`architecture.md`](../docs/knowledge/architecture.md).

Windows owns its own engine and app integration. This cutover does not update
or verify the Windows runtime. Commit, push, integration, and release still
require explicit authorization.
