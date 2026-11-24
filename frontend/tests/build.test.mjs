import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { test } from 'node:test';

const dist = new URL('../../.local/build/frontend/', import.meta.url);

test('production output is self-contained and loads only its own bundles', async () => {
  const assets = await readdir(new URL('assets/', dist));
  // Two pages: the main window and the call window (ADR-0060).
  for (const [page, entry] of [
    ['index.html', 'index'],
    ['call.html', 'call']
  ]) {
    const html = await readFile(new URL(page, dist), 'utf8');
    assert.match(html, new RegExp(`\\./assets/${entry}-[\\w-]+\\.js`), page);
    assert.match(html, /\.\/assets\/[\w-]+\.css/, page);
    assert.doesNotMatch(html, /https?:\/\//i, page);
    for (const [, name] of html.matchAll(/(?:src|href)="\.\/assets\/([^"]+)"/g)) {
      assert.ok(assets.includes(name), `${page} loads ${name}`);
    }
  }
  // The brand mark is drawn inline in the theme's colours, so the bundle
  // carries no image and no inlined (data:) asset: scripts and styles only.
  assert.deepEqual(assets.filter((name) => !/^[\w]+-[\w-]+\.(?:js|css)$/.test(name)), []);
  for (const name of assets.filter((name) => name.endsWith('.css'))) {
    const css = await readFile(new URL(`assets/${name}`, dist), 'utf8');
    assert.doesNotMatch(css, /url\(|@import|https?:\/\//i, name);
  }
});
