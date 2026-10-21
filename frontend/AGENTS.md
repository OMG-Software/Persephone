# Frontend guidance

This area inherits `../AGENTS.md`.

- Build a locally bundled Svelte UI. Keep `vite.config.js` output in `../.local/build/frontend`, relative asset URLs, no inline assets, and no production source maps unless a reviewed requirement changes them.
- Put all Tauri API access in `src/bridge/index.ts` or another module under `src/bridge/`. Components use the typed bridge. Do not call `invoke`, Tauri globals, or host APIs directly from components.
- Import public DTOs from `../crates/contracts/generated/contracts.ts`. Change Rust definitions and run the exporter when the DTO contract changes. TypeScript types do not validate runtime input or authorize commands.
- Render names and message bodies as escaped text. Preserve bidirectional isolation in `src/Message.svelte`, `src/RoomList.svelte`, and `src/Timeline.svelte`. Do not use `{@html}`, HTML insertion APIs, automatic previews, or arbitrary remote images.
- Keep timeline state bounded. Preserve event-ID deduplication, visible gaps, subscription cleanup, serialized subscribe/unsubscribe operations, and rejection of stale results when the selected room changes.
- Subscribe before the initial fetch. Treat change notifications as invalidations, then fetch bounded pages through the bridge. Do not put message content in notification queues.
- Keep loading and error states usable. Use semantic controls, keyboard access, and accessible labels. Report native accessibility checks separately from Node rendering tests.
- Run `python tools/dev.py frontend` from the root. It runs `svelte-check`, Vite build, and Node tests. The build must precede tests because `tests/build.test.mjs` inspects its output.
- Run `python tools/dev.py contracts` for public DTO changes. Check `src-tauri` permissions and core protocol changes when adding a bridge operation.
- Keep exact dependencies and `package-lock.json` aligned. Use `python tools/dev.py bootstrap` for locked installation; do not use commands that download an unpinned compiler.
