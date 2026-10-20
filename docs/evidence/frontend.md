# P0 frontend feasibility probe

> Historical run record: commands and source paths below describe the pre-restructure checkout. See [Development](../development.md) for current paths and commands.

**Date:** 2026-10-19  
**Status:** Frontend choice remains provisional; this probe supports continuing with TypeScript, Svelte, and Vite into P1, subject to real-webview, CSP, accessibility, and performance checks.

## Scope and decision context

The architecture plan recommends a TypeScript/Svelte/Vite frontend that builds to bundled static assets. ADR-0000 accepts Tauri with a local web UI but explicitly leaves the frontend framework and exact dependencies open. ADR-0001 selects sync behavior and does not affect this probe. The plan requires ordinary escaped bindings and prohibits raw HTML, server-controlled attribute spreading, remote presentation assets, and a production Node service.

This probe is isolated under `probes/p0/frontend`. It exercises a Svelte message view with untrusted sender and body strings, produces a production static build, and audits the resolved npm graph. It uses no Matrix account or real message data.

## Candidate versions and licenses

Queried npm registry package metadata on 2026-10-19 with `npm view`. Stable latest versions and declared licenses were:

| Package | Selected exact version | npm latest observed | Declared license | Reason |
| --- | ---: | ---: | --- | --- |
| `svelte` | 5.57.2 | 5.57.2 | MIT | Latest stable observed; ordinary bindings escape text and attribute values. |
| `vite` | 8.3.4 | 8.3.4 | MIT | Latest stable observed; builds static assets and supports a relative `./` base. |
| `@sveltejs/vite-plugin-svelte` | 7.3.1 | 7.3.1 | MIT | Latest stable observed; peers accept Vite 8 and Svelte 5. |
| `typescript` | 6.0.3 | 7.0.2 | Apache-2.0 | Latest version accepted by `svelte-check`'s declared `^5 || ^6` TypeScript peer range. TypeScript 7 was not selected because that peer range does not include it. |
| `svelte-check` | 4.7.7 | 4.7.7 | MIT | Svelte diagnostics and TypeScript-aware checks. |

All direct versions are exact in `package.json`; `package-lock.json` records the full resolution. npm metadata also reports Vite and its Svelte plugin require Node `^20.19 || ^22.12 || >=24`; the tested Node 24.15.0 satisfies this. npm links: [Svelte](https://www.npmjs.com/package/svelte/v/5.57.2), [Vite](https://www.npmjs.com/package/vite/v/8.3.4), [Svelte Vite plugin](https://www.npmjs.com/package/@sveltejs/vite-plugin-svelte/v/7.3.1), [TypeScript](https://www.npmjs.com/package/typescript/v/6.0.3), [svelte-check](https://www.npmjs.com/package/svelte-check/v/4.7.7).

The actual locked direct versions were confirmed by `npm ls --depth=0`. `npm audit` covered 71 graph entries (20 production, 52 development, 27 optional; npm categories overlap) and reported zero advisories at check time. This is the advisory service's result for this lockfile and date, not a complete license audit or a review of webview/native platform components. The declared direct-package licenses are recorded above; the graph includes platform-specific optional native packages, so downstream license/SBOM work still needs to inspect every package and packaged artifact.

## Probe behavior

`src/Message.svelte` renders names and bodies only through ordinary Svelte text bindings. The name is also interpolated into one fixed `aria-label`; the probe does not spread server-controlled attributes or accept a server-provided element, URL, class, or event handler. The message text is not parsed as markup. A local SVG is imported through the module graph, and Vite emits it as a separate hashed file because the inline threshold is disabled. CSS uses system fonts; no remote asset or font is requested.

The Node built-in test runner SSR-renders the component through the Vite/Svelte compiler and asserts that hostile `<img>`, `<script>`, and `<svg>` strings remain escaped text, quote characters cannot break the fixed `aria-label`, and no corresponding raw HTML element is emitted. A source-policy assertion checks this component for Svelte raw HTML, `innerHTML`, and attribute spreading. A production-output assertion verifies relative JS/CSS references, the separately emitted SVG, and no remote URL in the generated entry HTML. This follows Svelte's documented behavior for escaped text expressions; Svelte explicitly reserves `{@html}` for HTML insertion and warns about XSS when its input is uncontrolled: [basic markup](https://svelte.dev/docs/svelte/basic-markup).

Vite's production build generated a 26.64 kB JavaScript bundle (10.82 kB gzip), 0.63 kB CSS (0.38 kB gzip), and 0.22 kB SVG (0.19 kB gzip). Its documentation describes `vite build` as producing a static-hosting bundle and supports a relative base path for generated asset URLs: [production build](https://vite.dev/guide/build), [static assets](https://vite.dev/guide/assets).

## Verification record

Environment: Windows, Node v24.15.0, npm 11.13.0. The lockfile was generated, then reinstalled with `npm ci` using a cache inside the probe directory. Final checks after that clean lockfile install:

| Command | Exit | Result |
| --- | ---: | --- |
| `npm ci --cache ./.npm-cache --no-audit --no-fund` | 0 | Reproducible locked install; 46 packages installed for this platform. |
| `npm run check` | 0 | `svelte-check` found 0 errors and 0 warnings. |
| `npm run build` | 0 | Vite 8.3.4 built the local static bundle and separate SVG asset. |
| `npm test` | 0 | 3 tests passed, 0 failed. |
| `npm audit --json` | 0 | 0 info/low/moderate/high/critical advisories in the reported graph. |
| `npm ls --depth=0` | 0 | All five direct packages resolved to the pinned versions listed above. |

An initial SSR-test attempt failed because the component and renderer loaded separate Svelte module instances. The test now loads both through the same Vite SSR module graph; the final clean-install run passed. The first assertion also expected `>` to be escaped, which Svelte does not require for text safety; it now checks the meaningful invariant that `<` is escaped and no hostile element is emitted.

## Limits and next gate

This is evidence for Svelte's server-rendered escaping and a successful static build, not a browser DOM or desktop webview security test. It does not establish Tauri IPC permissions, navigation denial, CSP compatibility, renderer behavior in WebView2/WebKit, keyboard or screen-reader usability, timeline scale/performance, or actual runtime network/file isolation. It also does not justify a claim that every Svelte feature or arbitrary attribute use is safe. P1 should retain fixed semantic elements and ordinary text bindings, then test the packaged webview with restrictive CSP, accessibility checks, hostile fixtures, and navigation/IPC denial before adopting the frontend decision.

**Assessment:** TypeScript + Svelte + Vite is feasible for a small local-static UI and passes this narrow escaped-text/build/dependency-advisory probe. Adoption remains provisional.  
**Confidence:** 87% for the narrow feasibility result; low for webview, CSP, and accessibility conclusions because those checks were not run.