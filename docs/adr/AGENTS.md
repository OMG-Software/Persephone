# Architecture decision guidance

This area inherits `../AGENTS.md`.

- Use `template.md` and the existing MADR 2.1.2 layout. Each record needs status, deciders, and date.
- Use the next unused four-digit number and a lowercase dashed filename. Do not put record numbers in headings.
- Add the record to `index.md` in the same change. Preserve existing numbers and superseded records.
- Do not mark a proposal accepted or invent an owner decision. Cite the actual decision and its supporting evidence.
- Explain the concrete problem, constraints, options, chosen outcome, and consequences. Distinguish an accepted design from implementation or security verification.
- When a decision changes, record the replacement and cross-link superseded records. Update affected current plans without rewriting historical evidence as if it used the new design.
