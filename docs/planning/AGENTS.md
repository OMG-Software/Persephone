# Planning guidance

This area inherits `../AGENTS.md`.

- Keep `project-plan.md` focused on approved scope, milestones, and open decisions. Keep `feasibility.md` focused on evidence and acceptance gates.
- Do not expand the first-release scope or close a gate without a supported requirement or result. Link accepted ADRs and relevant reports.
- Distinguish current implementation from planned work and historical verification. Resolve status differences with the documentation index and `../evidence/verification.json`.
- Each acceptance item needs a clear completion condition. Preserve failures, unrun scenarios, platform gaps, and production limitations even when narrower tests pass.
- Avoid copying test counts or version pins across documents without checking the source run and manifests. Prefer links when duplication adds no value.
