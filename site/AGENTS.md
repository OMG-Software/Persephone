# Public site guidance

This area inherits `../AGENTS.md`.

- This is a static GitHub Pages site. `.github/workflows/pages.yml` uploads this directory directly; there is no site build step.
- Keep HTML, CSS, and image assets local. Preserve the restrictive CSP, `no-referrer`, and absence of third-party requests, scripts, analytics, web fonts, and embeds.
- Preserve semantic structure, the skip link, keyboard access, readable contrast, and responsive layout. Use descriptive link text and suitable image alternatives.
- Check claims against accepted ADRs and current project evidence. Distinguish goals from shipped features and retain the visible feasibility/readiness limits.
- Check local asset paths, anchor targets, and changed external links. Preview affected viewport sizes when layout changes. Do not infer site validation from Rust or frontend tests; this directory is a separate artifact.
- Publishing requires a deployment action or a push covered by the workflow. A local edit does not establish that the public page changed.
