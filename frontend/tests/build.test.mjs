import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { test } from 'node:test';

test('production output is self-contained and carries the imported local asset', async () => {
  const dist = new URL('../../.local/build/frontend/', import.meta.url);
  const html = await readFile(new URL('index.html', dist), 'utf8');
  const assets = await readdir(new URL('assets/', dist));

  assert.match(html, /\.\/assets\/index-[\w-]+\.js/);
  assert.match(html, /\.\/assets\/index-[\w-]+\.css/);
  assert.ok(assets.some((name) => /^mark-[\w-]+\.svg$/.test(name)));
  assert.doesNotMatch(html, /https?:\/\//i);
});
