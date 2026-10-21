import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { test } from 'node:test';

async function sourceFiles(directory) {
  const entries = await readdir(directory, { withFileTypes: true });
  const files = await Promise.all(
    entries.map((entry) => {
      const path = new URL(entry.name + (entry.isDirectory() ? '/' : ''), directory);
      return entry.isDirectory() ? sourceFiles(path) : [path];
    })
  );
  return files.flat().filter((path) => /\.(ts|js|svelte)$/.test(path.pathname));
}

test('only the bridge module talks to the Tauri host', async () => {
  const src = new URL('../src/', import.meta.url);
  const bridge = new URL('bridge/', src).href;
  const offenders = [];
  for (const file of await sourceFiles(src)) {
    const text = await readFile(file, 'utf8');
    const talksToHost =
      /@tauri-apps\/api/.test(text) || /__TAURI(_INTERNALS)?__/.test(text) || /\binvoke\s*\(/.test(text);
    if (talksToHost && !file.href.startsWith(bridge)) {
      offenders.push(file.pathname);
    }
  }
  assert.deepEqual(offenders, []);
});

test('no source uses raw HTML insertion', async () => {
  const src = new URL('../src/', import.meta.url);
  for (const file of await sourceFiles(src)) {
    const text = await readFile(file, 'utf8');
    assert.doesNotMatch(text, /\{@html|innerHTML|outerHTML|insertAdjacentHTML|document\.write/, file.pathname);
  }
});
