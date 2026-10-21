# Documentation guidance

This area inherits `../AGENTS.md`.

- Use `README.md` as the documentation index, `development.md` for current layout and commands, accepted ADRs for decisions, and `planning/feasibility.md` for acceptance gates.
- Check claims against current source and manifests. Some plans and evidence describe earlier implementation states. Preserve dated history, but identify stale guidance and conflicts when updating current status.
- Distinguish requirements, proposals, implementation, recorded results, and open gates. State OS, build type, versions, run date, command, and limitations for security claims.
- Keep relative links valid after moves. Update the documentation index when adding a document that readers need to find. Use repository paths for source references and verify line references before publication.
- Do not convert a historical run into a current passing claim. Keep failing and unrun checks visible. Reconcile status changes with evidence and the verification ledger.
- Keep tokens, credentials, real identifiers, chat content, and unredacted logs out of Markdown, JSON, archives, and screenshots.
- For documentation-only edits, inspect links, command flags, paths, JSON syntax where applicable, and diff whitespace. Run relevant code checks when documentation changes require implementation changes.
