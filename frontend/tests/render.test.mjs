import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let Message;
let render;

before(async () => {
  vite = await createServer({
    configFile: './vite.config.js',
    server: { middlewareMode: true, hmr: false },
    appType: 'custom'
  });
  ({ default: Message } = await vite.ssrLoadModule('/src/Message.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

test('hostile sender names and message bodies render as inert text', () => {
  const hostileName = '" onmouseover="alert(1) <img src=x onerror=alert(2)>';
  const hostileBody = '<script>alert(3)</script><svg onload=alert(4)></svg>';
  const { body: html } = render(Message, {
    props: { name: hostileName, body: hostileBody }
  });

  assert.match(html, /&lt;img/);
  assert.match(html, /&lt;script>/);
  assert.match(html, /&lt;svg/);
  assert.match(html, /aria-label="Message from &quot; onmouseover=&quot;alert\(1\) &lt;img/);
  assert.doesNotMatch(html, /<(?:img|script|svg)\b/i);
});

test('message view contains no raw HTML insertion or attribute spreading', async () => {
  const { readFile } = await import('node:fs/promises');
  const source = await readFile(new URL('../src/Message.svelte', import.meta.url), 'utf8');

  assert.doesNotMatch(source, /\{@html\b|\binnerHTML\b|\{@const\s+.*\bspread\b/i);
  assert.doesNotMatch(source, /\.\.\.\w+/);
});
