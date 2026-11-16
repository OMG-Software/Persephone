import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { test } from 'node:test';

test('production output is self-contained and loads only its own bundles', async () => {
  const dist = new URL('../../.local/build/frontend/', import.meta.url);
  const html = await readFile(new URL('index.html', dist), 'utf8');
  const assets = await readdir(new URL('assets/', dist));

  assert.match(html, /\.\/assets\/index-[\w-]+\.js/);
  assert.match(html, /\.\/assets\/index-[\w-]+\.css/);
  assert.doesNotMatch(html, /https?:\/\//i);
  // The brand mark is drawn inline in the theme's colours, so the bundle
  // carries no image and no inlined (data:) asset.
  assert.deepEqual(assets.filter((name) => !/^index-[\w-]+\.(?:js|css)$/.test(name)), []);
  for (const name of assets.filter((name) => name.endsWith('.css'))) {
    const css = await readFile(new URL(`assets/${name}`, dist), 'utf8');
    assert.doesNotMatch(css, /url\(|@import|https?:\/\//i, name);
  }
});
