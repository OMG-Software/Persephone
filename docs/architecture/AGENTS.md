# Architecture guidance

This area inherits `../AGENTS.md`.

- Follow accepted records in `../adr/index.md`. Keep `development-plan.md`, `security-design.md`, and `sdk-lifecycle.md` consistent with decisions and current component ownership.
- Describe trust boundaries, data authority, resource bounds, and failure behavior. Separate the target design from implemented controls and tested guarantees.
- For lifecycle changes, inventory SDK tasks and retained completion handles. Specify stop, drain, timeout, cancellation, stale-callback, and store-handoff behavior. Do not treat wrapper completion as SDK-wide completion.
- For IPC or process changes, show where validation and admission occur before body allocation, how peers and handles are owned, and how crashes affect protected state.
- State platform-specific enforcement and missing OS coverage. Link concrete evidence and remaining acceptance work instead of claiming that a framework or process split alone provides isolation.
