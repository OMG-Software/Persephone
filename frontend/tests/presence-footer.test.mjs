import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let render;
let Sidebar;

before(async () => {
  // A cache of its own: test files run in parallel (see views.test.mjs).
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/presence-footer',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: Sidebar } = await vite.ssrLoadModule('/src/Sidebar.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

const shown = (html) => html.replace(/<!--.*?-->/g, '');
const footer = (props) => {
  const html = shown(
    render(Sidebar, {
      props: {
        rooms: { rooms: [], truncated: false },
        selected: null,
        userId: '@me:example.org',
        ownName: 'Me',
        shortcut: 'Ctrl K',
        onselect() {},
        onsearch() {},
        ...props
      }
    }).body
  );
  return html.slice(html.indexOf('class="account-footer"'));
};

test('GIVEN a signed-in account WHEN the footer renders THEN the presence menu is under the name and the picture has its dot', () => {
  const html = footer({ presence: { sharePresence: true, presence: 'unavailable' }, onpresence() {} });
  const name = html.indexOf('<bdi class="account-name">Me</bdi>');
  const menu = html.indexOf('id="presence-menu-button"');
  const id = html.indexOf('<bdi class="account-id"');
  assert.ok(name >= 0 && name < menu && menu < id, 'name, then presence, then the user ID');
  assert.match(html, /aria-label="Your presence: Away"/);
  assert.match(html, /<span class="presence-text">Away<\/span>/);
  assert.match(html, /<span class="presence-avatar"><span class="avatar small[^"]*" aria-hidden="true">/);
  assert.equal((html.match(/presence-mark unavailable/g) ?? []).length, 2, 'on the picture and in the button');
});

test('GIVEN sharing is off WHEN the footer renders THEN it says Not shared and the picture has no dot', () => {
  const html = footer({ presence: { sharePresence: false, presence: 'online' }, onpresence() {} });
  assert.match(html, /aria-label="Your presence: Not shared"/);
  assert.match(html, /class="presence-mark hidden"/);
  assert.equal((html.match(/presence-mark/g) ?? []).length, 1, 'only in the button');
});

test('GIVEN no save callback or no account WHEN the footer renders THEN no presence menu shows', () => {
  assert.doesNotMatch(footer({ presence: { sharePresence: true, presence: 'online' } }), /presence-menu/);
  assert.doesNotMatch(footer({ userId: null, presence: null, onpresence() {} }), /presence/);
});

test('GIVEN a failed save WHEN the footer renders THEN the reason shows under the presence menu as an alert', () => {
  const html = footer({
    presence: { sharePresence: true, presence: 'online' },
    onpresence() {},
    presenceFailure: 'The setting could not be saved to this account.'
  });
  const menu = html.indexOf('id="presence-menu-button"');
  const alert = html.indexOf('<span class="presence-failure" role="alert">The setting could not be saved to this account.</span>');
  assert.ok(menu >= 0 && alert > menu, 'after the menu');
  assert.doesNotMatch(
    footer({ presence: { sharePresence: true, presence: 'online' }, onpresence() {} }),
    /presence-failure/
  );
});
