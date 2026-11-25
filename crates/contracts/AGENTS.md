# Frontend data contracts

This crate defines small Rust view types and exports their TypeScript representation. It must not expose SDK types, tokens, keys, store handles, or generic backend authority.

- Edit DTOs and generation logic in `src/lib.rs`. Preserve explicit Serde field names, enum tags, unknown-field rejection, and JavaScript-safe numeric ranges.
- Regenerate `generated/contracts.ts` with `cargo run --locked -p persephone-contracts --bin export-contracts -- --write`. Do not edit generated bindings directly.
- Update JSON fixtures in `fixtures/`, Rust tests, and `tests/type-contracts.ts` when the intended wire contract changes. The exporter writes TypeScript only; it does not update the JSON fixtures.
- Inspect `frontend/src/bridge/index.ts`, core view production, and Tauri consumers for compatibility. Static TypeScript types do not validate untrusted runtime data.
- Keep sample data synthetic, including hostile text that must remain inert when rendered.

- Call statistics (ADR-0066): `CallStats` and its parts hold integers with bounds (`CALL_STATS_*`). `CallStats::within_limits` is the check that the core, the UI host and the page's `parseCallStats` all repeat; change them together. The `CALL_STATS_*` bounds repeat `MAX_STATS_*` in `crates/call-protocol/src/stats.rs`, and `crates/core/src/calls/stats.rs` checks at compile time that they match. Each `Option` field is required-nullable (`Option::deserialize`): the key must be there, and only its value may be null.

## Checks

- `cargo test --locked -p persephone-contracts`
- `python tools/dev.py contracts` checks generated-file drift and TypeScript contracts with the installed frontend compiler.
- `python tools/dev.py frontend` checks frontend consumers. Install locked frontend dependencies with `python tools/dev.py bootstrap` if needed.
