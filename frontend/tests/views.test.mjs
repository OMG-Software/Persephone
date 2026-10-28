import assert from 'node:assert/strict';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

let vite;
let RoomList;
let Timeline;
let SignIn;
let Security;
let Composer;
let Message;
let ServerPicker;
let Verification;
let JumpPalette;
let IdentityChanges;
let Invites;
let NewConversation;
let NewRoom;
let RoomSettings;
let LevelPicker;
let Preferences;
let AccountPage;
let Devices;
let render;

before(async () => {
  // Test files run in parallel: each server keeps its own dependency cache,
  // and none discovers dependencies mid-run, so no server finds another's
  // pre-bundle outdated (ERR_OUTDATED_OPTIMIZED_DEP).
  vite = await createServer({
    configFile: './vite.config.js',
    cacheDir: 'node_modules/.vite-tests/views',
    optimizeDeps: { noDiscovery: true },
    ssr: { optimizeDeps: { noDiscovery: true } },
    server: { middlewareMode: true, hmr: false, ws: false },
    appType: 'custom'
  });
  ({ default: RoomList } = await vite.ssrLoadModule('/src/RoomList.svelte'));
  ({ default: Timeline } = await vite.ssrLoadModule('/src/Timeline.svelte'));
  ({ default: SignIn } = await vite.ssrLoadModule('/src/SignIn.svelte'));
  ({ default: Security } = await vite.ssrLoadModule('/src/Security.svelte'));
  ({ default: Composer } = await vite.ssrLoadModule('/src/Composer.svelte'));
  ({ default: Message } = await vite.ssrLoadModule('/src/Message.svelte'));
  ({ default: ServerPicker } = await vite.ssrLoadModule('/src/ServerPicker.svelte'));
  ({ default: Verification } = await vite.ssrLoadModule('/src/Verification.svelte'));
  ({ default: JumpPalette } = await vite.ssrLoadModule('/src/JumpPalette.svelte'));
  ({ default: IdentityChanges } = await vite.ssrLoadModule('/src/IdentityChanges.svelte'));
  ({ default: Invites } = await vite.ssrLoadModule('/src/Invites.svelte'));
  ({ default: NewConversation } = await vite.ssrLoadModule('/src/NewConversation.svelte'));
  ({ default: NewRoom } = await vite.ssrLoadModule('/src/NewRoom.svelte'));
  ({ default: RoomSettings } = await vite.ssrLoadModule('/src/RoomSettings.svelte'));
  ({ default: LevelPicker } = await vite.ssrLoadModule('/src/LevelPicker.svelte'));
  ({ default: Preferences } = await vite.ssrLoadModule('/src/Preferences.svelte'));
  ({ default: AccountPage } = await vite.ssrLoadModule('/src/AccountPage.svelte'));
  ({ default: Devices } = await vite.ssrLoadModule('/src/Devices.svelte'));
  ({ render } = await vite.ssrLoadModule('svelte/server'));
});

after(async () => {
  await vite?.close();
});

const hostileName = '<img src=x onerror=alert(1)>';

test('room names render as isolated, escaped text with capped unread counts', () => {
  const { body: html } = render(RoomList, {
    props: {
      rooms: [
        {
          roomId: '!a:example.invalid',
          displayName: hostileName,
          unreadCount: 4294967295,
          encryption: 'encrypted'
        },
        { roomId: '!b:example.invalid', displayName: null, unreadCount: 0, encryption: 'not_encrypted' }
      ],
      truncated: true,
      selected: '!a:example.invalid',
      onselect: () => {}
    }
  });
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /&lt;img src=x onerror=alert\(1\)>/);
  assert.match(html, /<bdi class="room-name">&lt;img/);
  // A room without a name falls back to its ID.
  assert.match(html, /<bdi class="room-name">!b:example\.invalid<\/bdi>/);
  assert.match(html, />999\+</);
  assert.match(html, /aria-current="true"/);
  assert.equal((html.match(/aria-current/g) ?? []).length, 1);
  assert.match(html, /Showing the first 2 rooms/);
  assert.match(html, /<nav aria-label="Rooms">/);
});

test('timeline bodies keep their text, are direction-isolated and never parsed as HTML', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: hostileName,
      items: [
        { kind: 'message', event_id: '$1', sender: '@m:example.invalid', body: '<script>alert(2)</script>' },
        { kind: 'message', event_id: '$2', sender: hostileName, body: 'Bidi ‮gnp.exe' },
        { kind: 'gap', next_cursor: null }
      ],
      hasOlder: true,
      loading: false,
      error: null,
      onolder: () => {}
    }
  });
  assert.doesNotMatch(html, /<script/);
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /&lt;script>alert\(2\)&lt;\/script>/);
  assert.equal((html.match(/<p dir="auto">/g) ?? []).length, 2);
  assert.match(html, /‮gnp\.exe/, 'bidi text is kept, isolated by dir=auto');
  assert.match(html, /Some messages are not shown here/);
  assert.match(html, /Load older messages/);
  assert.match(html, /<h2 id="timeline-heading"><bdi>&lt;img/);
});

test('timeline controls reflect loading, end of history and errors', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      items: [],
      hasOlder: false,
      loading: false,
      error: 'The Persephone core is unavailable.',
      onolder: () => {}
    }
  });
  assert.match(html, /<button type="button" disabled[^>]*>Start of history/);
  assert.match(html, /role="alert"/);
});

test('notices render as fixed text with an isolated sender, never as content', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      items: [
        { kind: 'notice', event_id: '$1', sender: hostileName, notice: 'undecryptable' },
        { kind: 'notice', event_id: '$2', sender: '@b:example.invalid', notice: 'unsupported' },
        { kind: 'notice', event_id: '$3', sender: '@c:example.invalid', notice: 'withheld' },
        { kind: 'notice', event_id: '$4', sender: '@d:example.invalid', notice: 'untrusted_sender' }
      ],
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {}
    }
  });
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /<bdi>&lt;img/);
  assert.match(html, /Encrypted message\. This device has no key for it\./);
  assert.match(html, /A kind of message Persephone does not show yet\./);
  assert.match(html, /The sender did not share its key with this device\./);
  assert.match(html, /from a device its owner has not verified/);
});

test('GIVEN state changes with hostile names WHEN rendered THEN each line is fixed text with every name isolated', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      items: [
        {
          kind: 'state',
          event_id: '$1',
          sender: '@a:example.invalid',
          origin_server_ts: null,
          change: { type: 'member', target: hostileName, membership: 'kicked' }
        },
        {
          kind: 'state',
          event_id: '$2',
          sender: hostileName,
          origin_server_ts: null,
          change: { type: 'topic', topic: '<script>alert(1)</script>' }
        },
        {
          kind: 'state',
          event_id: '$3',
          sender: '@c:example.invalid',
          origin_server_ts: null,
          change: { type: 'encryption' }
        }
      ],
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {}
    }
  });
  assert.doesNotMatch(html, /<(?:img|script)\b/i);
  // Without Svelte's hydration markers, which sit between the parts.
  const shown = html.replace(/<!--.*?-->/g, '');
  assert.match(shown, /<bdi>@a:example\.invalid<\/bdi> removed <bdi>&lt;img/);
  assert.match(shown, /changed the topic to <bdi>&lt;script>/);
  assert.match(shown, /<bdi>@c:example\.invalid<\/bdi> turned on end-to-end encryption/);
});

test('GIVEN replies WHEN rendered THEN a loaded message is quoted as isolated text, and a missing one is named only', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      items: [
        { kind: 'message', event_id: '$1', sender: hostileName, body: 'asked <b>', origin_server_ts: null, reply_to: null },
        { kind: 'message', event_id: '$2', sender: '@b:example.invalid', body: 'answer', origin_server_ts: null, reply_to: '$1' },
        { kind: 'message', event_id: '$3', sender: '@b:example.invalid', body: 'late', origin_server_ts: null, reply_to: '$gone' }
      ],
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {},
      onreply: () => {}
    }
  });
  const shown = html.replace(/<!--.*?-->/g, '');
  assert.doesNotMatch(shown, /<img|<b>/);
  assert.match(shown, /In reply to <bdi>&lt;img src=x onerror=alert\(1\)><\/bdi>: <span dir="auto">asked &lt;b><\/span>/);
  assert.match(shown, /In reply to an earlier message/);
  assert.equal((shown.match(/>\s*Reply <span class="visually-hidden">/g) ?? []).length, 3);
});

test('GIVEN the first unread message WHEN rendered THEN one New messages line comes just before it', () => {
  const msg = (id) => ({ kind: 'message', event_id: id, sender: '@b:example.invalid', body: id, origin_server_ts: null, reply_to: null });
  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      items: [msg('$1'), msg('$2'), msg('$3')],
      unreadFrom: '$2',
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {}
    }
  });
  const shown = html.replace(/<!--.*?-->/g, '');
  assert.equal((shown.match(/New messages/g) ?? []).length, 1);
  assert.ok(shown.indexOf('New messages') > shown.indexOf('>$1<'));
  assert.ok(shown.indexOf('New messages') < shown.indexOf('>$2<'));
});

test('GIVEN reactions WHEN rendered THEN keys are isolated text with counts, and our own is pressed', () => {
  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      ownUserId: '@me:example.invalid',
      items: [
        { kind: 'message', event_id: '$1', sender: '@a:example.invalid', body: 'hi', origin_server_ts: null, reply_to: null },
        { kind: 'reaction', event_id: '$r1', sender: '@me:example.invalid', reacts_to: '$1', key: '<b>bold</b>', origin_server_ts: null },
        { kind: 'reaction', event_id: '$r2', sender: '@a:example.invalid', reacts_to: '$1', key: '👍', origin_server_ts: null }
      ],
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {},
      onreact: () => {},
      onunreact: () => {}
    }
  });
  const shown = html.replace(/<!--.*?-->/g, '');
  assert.doesNotMatch(shown, /<b>/);
  assert.match(shown, /aria-pressed="true"[^>]*><bdi>&lt;b>bold&lt;\/b><\/bdi> <span class="count">1<\/span>/);
  assert.match(shown, /aria-pressed="false"[^>]*><bdi>👍<\/bdi> <span class="count">1<\/span>/);
});

test('the sign-in form asks only for the homeserver; the password goes to a native window', () => {
  const signedOut = { state: 'signed_out', userId: null, error: null };
  const form = (mode = 'sign_in', username = '') => ({ homeserver: 'https://matrix.org', mode, username });
  const props = (account, extra = {}) => ({
    account,
    busy: false,
    form: form(),
    onsignin: () => {},
    onregister: () => {},
    ...extra
  });
  const { body: html } = render(SignIn, { props: props(signedOut) });
  assert.match(html, /Your server/);
  assert.match(html, /<bdi class="server-value">matrix\.org<\/bdi>/, 'matrix.org until changed');
  assert.match(html, />Continue</);
  assert.doesNotMatch(html, /type="password"/i, 'no password field in the page');
  // The sign-in form itself; the proxy has its own form under Connection.
  const signInForm = html.slice(html.indexOf('<form'), html.indexOf('</form>'));
  assert.doesNotMatch(signInForm, /<input/, 'the server is chosen in the picker; nothing else is asked');
  assert.match(html, /<details class="connection"><summary>Connection<\/summary>[\s\S]*id="proxy-address"/);
  assert.match(html, /This page never sees your password/);
  assert.match(html, /<button type="button" class="link"[^>]*>\s*Create an account\s*<\/button>/);

  const failed = render(SignIn, {
    props: props({ state: 'sign_in_failed', userId: null, error: 'invalid_credentials' })
  }).body;
  assert.match(failed, /role="alert"/);
  assert.match(failed, /did not accept that username and password/);

  const waiting = render(SignIn, { props: props({ state: 'signing_in', userId: null, error: null }) }).body;
  assert.match(waiting, /Waiting for the Windows sign-in window/);
  assert.match(waiting, /<button type="submit" class="primary wide" disabled/);
});

test('GIVEN the create-account mode WHEN it shows THEN only a username is asked, and its problems and the server\'s answer are clear', () => {
  const signedOut = { state: 'signed_out', userId: null, error: null };
  const view = (username, account = signedOut) =>
    render(SignIn, {
      props: {
        account,
        busy: false,
        form: { homeserver: 'https://matrix.org', mode: 'register', username },
        onsignin: () => {},
        onregister: () => {}
      }
    }).body;
  const empty = view('');
  assert.match(empty, /<h2 id="sign-in-heading">Create an account<\/h2>/);
  assert.match(empty, /<input id="register-username" class="field mono" type="text"/);
  const registerForm = empty.slice(empty.indexOf('<form'), empty.indexOf('</form>'));
  assert.equal((registerForm.match(/<input/g) ?? []).length, 1, 'the username is the only field');
  assert.doesNotMatch(empty, /type="password"/i, 'the password goes to a native window');
  assert.match(empty, /<button type="submit" class="primary wide" disabled[^>]*>\s*Create account/);
  assert.match(empty, /This page never sees them/);

  const fine = view('  @Newbie ');
  assert.match(fine, /Your Matrix ID will start with @newbie\./);
  assert.doesNotMatch(fine, /<button type="submit" class="primary wide" disabled/);

  assert.match(view('newbie:matrix.org'), /without ":" and a server/);
  assert.match(view('new bie'), /Use only letters, digits/);
  assert.match(view('<b>x</b>'), /aria-invalid="true"/);

  for (const [error, text] of [
    ['username_taken', /That username is taken/],
    ['registration_disabled', /does not let anyone register/],
    ['token_rejected', /did not accept the registration token/],
    ['unsupported_registration', /Register in the homeserver's own web page/],
    ['weak_password', /too weak/],
    ['future_code', /That did not work\./]
  ]) {
    const html = view('newbie', { state: 'sign_in_failed', userId: null, error });
    assert.match(html, /role="alert"/, error);
    assert.match(html.replaceAll('&#39;', "'"), text, error);
  }
  assert.match(view('newbie', { state: 'signing_in', userId: null, error: null }), /Checking the name/);
});

test('GIVEN the server picker WHEN another server is chosen THEN it asks only for a name or address', () => {
  const picker = (current) =>
    render(ServerPicker, { props: { current, onchoose: () => {}, oncancel: () => {} } }).body;

  const matrixOrg = picker('https://matrix.org');
  assert.match(matrixOrg, /<dialog[^>]*aria-labelledby="picker-heading"/);
  assert.equal((matrixOrg.match(/type="radio"/g) ?? []).length, 2);
  assert.doesNotMatch(matrixOrg, /id="homeserver"/, 'no address field until another server is chosen');
  assert.match(matrixOrg, />Use matrix\.org</);

  const other = picker(hostileName);
  assert.match(other, /<label for="homeserver" class="visually-hidden">Homeserver address<\/label>/);
  assert.match(other, /<input id="homeserver"[^>]*type="text"[^>]*required/);
  assert.match(other, /value="&lt;img src=x onerror=alert\(1\)>"/, 'the typed address stays text');
  assert.doesNotMatch(other, /<img/);
  assert.doesNotMatch(other, /type="password"/i);
  // A name, not an address: it is looked up before it is used.
  assert.match(other, />Find server</);
  assert.match(picker('https://chat.example.org'), />Use this server</);
});

test('the encryption panel offers the step that fits and never asks for a secret itself', () => {
  const status = (identity, extra = {}) => ({
    identity,
    recovery: 'disabled',
    operation: 'idle',
    error: null,
    verification: null,
    canSetUpRecovery: false,
    ...extra
  });
  const panel = (security, busy = false) =>
    render(Security, {
      props: {
        security,
        busy,
        onsetup: () => {},
        onrecover: () => {},
        onsetuprecovery: () => {},
        onverify: () => {}
      }
    }).body;

  const none = panel(status('none'));
  assert.match(none, />Set up encryption</);
  assert.doesNotMatch(none, /Use recovery key/);
  const unverified = panel(status('unverified', { recovery: 'enabled' }));
  assert.match(unverified, />Use recovery key</);
  assert.match(unverified, />Verify with another device</);
  assert.match(unverified, /This device is not verified/);
  assert.doesNotMatch(unverified, /Set up encryption/);
  const verified = panel(status('verified', { recovery: 'enabled' }));
  assert.match(verified, /This device is verified\./);
  assert.match(verified, /Recovery is set up\./);
  // The only step left is replacing the recovery key, which the core shows.
  assert.deepEqual([...verified.matchAll(/<button[^>]*>([^<]+)</g)].map((m) => m[1]), ['Change recovery key…']);
  assert.match(verified, /does not keep your recovery key/);
  assert.match(verified, /The old key stops working\./);
  assert.doesNotMatch(panel(status('unverified', { recovery: 'enabled' })), /Change recovery key/);
  assert.doesNotMatch(panel(status('verified', { recovery: 'disabled' })), /Change recovery key/);
  // Offered only when this device holds the keys recovery needs.
  assert.doesNotMatch(panel(status('verified', { recovery: 'disabled' })), /Set up recovery/);
  const noRecovery = panel(status('verified', { recovery: 'disabled', canSetUpRecovery: true }));
  assert.match(noRecovery, />Set up recovery</);
  assert.match(noRecovery, /cannot be read on a new device/);
  for (const html of [none, unverified, verified, panel(status('unknown'))]) {
    assert.doesNotMatch(html, /<input/, 'no field for a password or recovery key');
  }

  const waiting = panel(status('none', { operation: 'waiting_for_password' }));
  assert.match(waiting, /Enter your password in the Windows window/);
  assert.match(waiting, /<button type="button" disabled/);
  const wrong = panel(status('unverified', { error: 'wrong_recovery_key' }));
  assert.match(wrong, /role="alert"/);
  assert.match(wrong, /That recovery key did not work\./);
  const unknownError = panel(status('none', { error: 'something_new' }));
  assert.match(unknownError, /That did not work\./);

  // Health cards: green only for what is in place, amber for what needs a step.
  const tones = (html) => [...html.matchAll(/class="health-value (\w+)">([^<]+)</g)].map((m) => m.slice(1).join(':'));
  assert.deepEqual(tones(verified), ['good:Ready', 'good:Verified', 'good:Set up']);
  assert.deepEqual(tones(unverified), ['good:Ready', 'attention:Not verified', 'good:Set up']);
  assert.deepEqual(tones(none), ['attention:Not set up', 'attention:Not verified', 'attention:Not set up']);
  assert.deepEqual(tones(panel(status('unknown', { recovery: 'unknown' }))), [
    'unknown:Checking…',
    'unknown:Checking…',
    'unknown:Checking…'
  ]);
});

test('GIVEN a contact verification WHEN each phase shows THEN the contact is isolated text and no device steps show', () => {
  const view = (phase, extra = {}) =>
    render(Verification, {
      props: {
        flow: { flowId: 'f1', phase, emoji: [], otherDevice: null, contact: hostileName, ...extra },
        busy: false,
        onaccept: () => {},
        onconfirm: () => {},
        oncancel: () => {},
        ondismiss: () => {}
      }
    }).body;
  const incoming = view('incoming');
  assert.match(incoming, /role="alertdialog"/);
  assert.match(incoming, /<bdi>&lt;img src=x onerror=alert\(1\)><\/bdi> wants to verify each other/);
  assert.doesNotMatch(incoming, /<img/);
  assert.doesNotMatch(incoming, /Your other device/);
  assert.doesNotMatch(incoming, /class="steps"/, 'the sign-in steps are for this device only');
  assert.match(incoming, />Accept</);
  assert.match(incoming, />Decline</);
  assert.match(view('waiting'), /Waiting for <bdi>&lt;img[^<]*<\/bdi> to accept the request\./);
  assert.match(view('confirmed'), /Waiting for <bdi>&lt;img[^<]*<\/bdi> to confirm\./);
  assert.match(view('done'), /<bdi>&lt;img[^<]*<\/bdi> is verified\./);
  assert.match(view('cancelled'), /Verification was cancelled\./);
  const emoji = Array.from({ length: 7 }, (_, i) => ({ symbol: '🐶', description: 'Dog' + i }));
  const compare = view('compare', { emoji });
  assert.match(compare, /not in this chat/);
  assert.match(compare, />They match</);
});

test('GIVEN a direct-message room WHEN the header shows THEN the contact and their trust are fixed, isolated text', () => {
  const header = (trust, canVerify = true, verifying = false) =>
    render(Timeline, {
      props: {
        title: 'Bob',
        encryption: 'encrypted',
        items: [],
        hasOlder: false,
        loading: false,
        error: null,
        onolder: () => {},
        contact: { roomId: '!dm:example.invalid', userId: hostileName, trust },
        canVerify,
        verifying,
        onverify: () => {}
      }
    }).body;
  const unverified = header('unverified');
  assert.match(unverified, /class="contact-id"><bdi>&lt;img[^<]*<\/bdi> · Not verified/);
  assert.doesNotMatch(unverified, /<img/);
  assert.match(unverified, /class="secondary header-action"[^>]*>\s*Verify/);
  assert.doesNotMatch(unverified, /verified-shield/);
  assert.doesNotMatch(header('unverified', false), /header-action/, 'an unverified device offers no verification');
  assert.match(header('unverified', true, true), /header-action" disabled/);
  const verified = header('verified');
  assert.match(verified, /verified-shield/);
  assert.match(verified, / · Verified/);
  assert.doesNotMatch(verified, /header-action/);
  assert.match(header('no_identity'), / · Has not set up encryption/);
  assert.doesNotMatch(header('no_identity'), /header-action/);
  assert.match(header('changed'), / · Identity changed/);
  assert.doesNotMatch(header('changed'), /header-action/, 'a changed identity is handled in the room');
  assert.match(header('verified_changed'), / · Identity changed since you verified them/);
  assert.doesNotMatch(header('verified_changed'), /verified-shield/);
  // A trust level the page does not know never reads as verified.
  assert.match(header('trusted_anyway'), / · Checking…/);
  assert.doesNotMatch(header('trusted_anyway'), /verified-shield/);
  // Other rooms keep the room icon and no contact line.
  const room = render(Timeline, {
    props: { title: 'R', items: [], hasOlder: false, loading: false, error: null, onolder: () => {} }
  }).body;
  assert.doesNotMatch(room, /contact-id/);
  assert.match(room, /class="icon hash"/);
});

test('every room is labelled with its encryption in the list and the header', () => {
  const list = render(RoomList, {
    props: {
      rooms: [
        { roomId: '!a:example.invalid', displayName: 'A', unreadCount: 0, encryption: 'encrypted' },
        { roomId: '!b:example.invalid', displayName: 'B', unreadCount: 0, encryption: 'not_encrypted' },
        { roomId: '!c:example.invalid', displayName: 'C', unreadCount: 0, encryption: 'unknown' }
      ],
      truncated: false,
      selected: null,
      onselect: () => {}
    }
  }).body;
  const labels = (html) => [...html.matchAll(/role="img" aria-label="([^"]+)"/g)].map((m) => m[1]);
  // The icon comes first in each row, before the name.
  assert.deepEqual(labels(list), ['Encrypted', 'Not encrypted', 'Encryption unknown']);
  assert.match(list, /aria-label="Not encrypted" title="Not encrypted">[\s\S]*?<bdi class="room-name">B</);

  const timeline = (encryption) =>
    render(Timeline, {
      props: { title: 'R', encryption, items: [], hasOlder: false, loading: false, error: null, onolder: () => {} }
    }).body;
  assert.deepEqual(labels(timeline('not_encrypted')), ['Not encrypted']);
  assert.deepEqual(labels(timeline('encrypted')), ['Encrypted']);
  assert.match(timeline('not_encrypted'), /class="enc-icon large not_encrypted"/);
  // Unknown never claims encryption.
  assert.deepEqual(labels(timeline('unknown')), ['Encryption unknown']);
});

test('GIVEN links in a message WHEN rendered THEN web links are buttons, never anchors, and other schemes stay text', () => {
  const body = 'go https://example.org/a?b=<i>c</i> or javascript:alert(1)';
  const withLinks = render(Message, {
    props: { name: '@a:s', body, onlink: () => {} }
  }).body.replace(/<!--.*?-->/g, '');
  assert.doesNotMatch(withLinks, /<a\b|href=|<i>/);
  assert.match(withLinks, /<button type="button" class="link body-link" title="Open https:\/\/example\.org\/a\?b=">https:\/\/example\.org\/a\?b=<\/button>/);
  assert.match(withLinks, /&lt;i>c&lt;\/i> or javascript:alert\(1\)/);
  const without = render(Message, { props: { name: '@a:s', body } }).body;
  assert.doesNotMatch(without, /<button[^>]*body-link/);
});

test('GIVEN others typing and reading WHEN shown THEN each name is isolated text, and many are counted', () => {
  const composer = (typing, typingCut = false) =>
    render(Composer, {
      props: {
        pending: [],
        blocked: null,
        encryption: 'encrypted',
        roomName: 'general',
        draftVersion: 0,
        onsend: async () => true,
        onretry: () => {},
        ondiscard: () => {},
        onallow: () => {},
        typing,
        typingCut
      }
    }).body.replace(/<!--.*?-->/g, '');
  assert.match(composer([hostileName]), /<bdi>&lt;img src=x onerror=alert\(1\)><\/bdi> is typing…/);
  assert.match(composer(['@a:s', '@b:s']), /<bdi>@a:s<\/bdi> and <bdi>@b:s<\/bdi> are typing…/);
  assert.match(composer(['@a:s', '@b:s', '@c:s']), /Several people are typing…/);
  assert.match(composer(['@a:s'], true), /Several people are typing…/);
  assert.doesNotMatch(composer([]), /typing…/);

  const { body: html } = render(Timeline, {
    props: {
      title: 'Room',
      items: [],
      readBy: [hostileName, '@b:s'],
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {}
    }
  });
  const shown = html.replace(/<!--.*?-->/g, '');
  assert.doesNotMatch(shown, /<img/);
  assert.match(shown, /Seen by <bdi>&lt;img src=x onerror=alert\(1\)><\/bdi>,\s*<bdi>@b:s<\/bdi>/);
});

test('GIVEN a room that is not encrypted WHEN the composer shows THEN it warns, and only then', () => {
  const composer = (encryption) =>
    render(Composer, {
      props: {
        pending: [],
        blocked: null,
        encryption,
        roomName: 'general',
        draftVersion: 0,
        onsend: async () => true,
        onretry: () => {},
        ondiscard: () => {},
        onallow: () => {}
      }
    }).body;

  const plain = composer('not_encrypted');
  assert.match(plain, /<strong>Not encrypted\.<\/strong> Messages in this room can be read by the server and anyone\s+who joins\./);
  assert.match(plain, /aria-expanded="false"[^>]*>Learn more</);
  assert.match(plain, /class="composer plain"/);
  assert.doesNotMatch(plain, /class="enc-note"/, 'no "Encrypted" note in a room that is not');

  const encrypted = composer('encrypted');
  assert.doesNotMatch(encrypted, /Not encrypted/);
  assert.match(encrypted, /class="enc-note"><svg[\s\S]*?<\/svg>(?:<!---->)?Encrypted</);

  const unknown = composer('unknown');
  assert.doesNotMatch(unknown, /Not encrypted|enc-note/, 'unknown makes neither claim');
});

test('messages being sent show their state as fixed text with safe actions only', () => {
  const message = (localId, state, failure = null) => ({
    localId,
    roomId: '!r:example.invalid',
    body: hostileName,
    state,
    eventId: state === 'sent' ? '$e' : null,
    failure
  });
  const composer = (pending, blocked = null, encryption = 'encrypted', roomName = 'R') =>
    render(Composer, {
      props: {
        pending,
        blocked,
        encryption,
        roomName,
        draftVersion: 0,
        onsend: async () => true,
        onretry: () => {},
        ondiscard: () => {},
        onallow: () => {}
      }
    }).body;

  const html = composer([
    message(1, 'queued'),
    message(2, 'sending'),
    message(3, 'unknown'),
    message(4, 'failed', 'device_not_verified'),
    message(5, 'failed', 'confirmation_required'),
    message(6, 'sent')
  ]);
  assert.doesNotMatch(html, /<img/, 'bodies are text');
  assert.match(html, /<p dir="auto">&lt;img/);
  assert.match(html, /Waiting to send…/);
  assert.match(html, /Sending…/);
  assert.match(html, /Delivery unknown: the connection failed\. Retrying cannot send it twice\./);
  assert.match(html, /Not sent: verify this device first\./);
  assert.match(html, /Not sent: this room is not encrypted\./);
  assert.match(html, /Sent\./);
  assert.equal((html.match(/>Retry</g) ?? []).length, 3, 'unknown and the two failures');
  assert.equal((html.match(/>Send unencrypted</g) ?? []).length, 1);
  assert.equal((html.match(/>Discard</g) ?? []).length, 4, 'not while sending or once sent');

  const blocked = composer([], 'Verify this device in Settings, Security & encryption, to send encrypted messages.');
  assert.match(blocked, /<textarea[^>]*disabled/);
  assert.match(blocked, /Verify this device in Settings, Security &amp; encryption/);
  assert.match(composer([], null, 'not_encrypted'), />Message \(not encrypted\)</);
  // The room name in the placeholder is escaped and direction-isolated.
  const named = composer([], null, 'encrypted', hostileName);
  assert.doesNotMatch(named, /<img/);
  assert.match(named, /placeholder="Message #⁨&lt;img src=x onerror=alert\(1\)>⁩"/);
  assert.match(named, /<button type="submit" class="send" aria-label="Send"/);
});

test('verification shows each phase as fixed text, the emoji as text and the device name isolated', () => {
  const flow = (phase, extra = {}) => ({ flowId: 'f1', phase, emoji: [], otherDevice: null, ...extra });
  const view = (phase, extra = {}) =>
    render(Verification, {
      props: {
        flow: flow(phase, extra),
        busy: false,
        onaccept: () => {},
        onconfirm: () => {},
        oncancel: () => {},
        ondismiss: () => {}
      }
    }).body;
  // The settings panel while a verification is in a phase.
  const settings = (phase) =>
    render(Security, {
      props: {
        security: {
          identity: 'unverified',
          recovery: 'enabled',
          operation: 'idle',
          error: null,
          verification: flow(phase),
          canSetUpRecovery: false
        },
        busy: false,
        onsetup: () => {},
        onrecover: () => {},
        onsetuprecovery: () => {},
        onverify: () => {}
      }
    }).body;

  const incoming = view('incoming', { otherDevice: hostileName });
  assert.match(incoming, /role="alertdialog"/);
  assert.match(incoming, /<bdi>&lt;img/);
  assert.doesNotMatch(incoming, /<img/);
  assert.match(incoming, />Accept</);
  assert.match(incoming, />Decline</);
  assert.doesNotMatch(settings('incoming'), /Verify with another device/, 'no second flow while one is open');

  const emoji = [
    ['🐶', 'Dog'], ['🐱', 'Cat'], ['🦁', 'Lion'], ['🐎', 'Horse'], ['🦄', 'Unicorn'], ['🐷', 'Pig'], ['🐘', 'Elephant']
  ].map(([symbol, description]) => ({ symbol, description }));
  const compare = view('compare', { emoji });
  assert.equal((compare.match(/<li>/g) ?? []).length, 7);
  assert.match(compare, /🐶<\/span> Dog/);
  assert.match(compare, />They match</);
  assert.match(compare, />They don't match</);

  assert.match(view('waiting'), /Waiting for your other device\. Accept the request there\./);
  assert.match(view('confirmed'), /Waiting for your other device to confirm\./);
  // The confirmation may not have been sent, so the wait can always be ended.
  assert.match(view('confirmed'), />Cancel</);
  assert.match(view('done'), /Verification finished\./);
  assert.match(view('done'), />Continue</);
  assert.match(view('done'), /<li class="step done">Verify<\/li>/);
  assert.match(view('waiting'), /<li class="step current" aria-current="step">Verify<\/li>/);
  const cancelled = view('cancelled');
  assert.match(cancelled, /Verification was cancelled\./);
  assert.match(cancelled, />Continue</);
  assert.match(settings('cancelled'), />Verify with another device</, 'a new attempt is offered');
  for (const html of [incoming, compare, cancelled]) {
    assert.doesNotMatch(html, /<input/);
  }
  // Each dialog is named by its question, so a screen reader reads it first.
  for (const html of [incoming, compare]) {
    assert.match(html, /aria-labelledby="verification-question"/);
    assert.match(html, /<p id="verification-question">/);
  }
  assert.match(view('waiting'), /<div class="verification">/, 'only the dialogs are named');
  // A comparison without its seven emoji offers no match.
  const short = view('compare', { emoji: emoji.slice(0, 6) });
  assert.doesNotMatch(short, /They match/);
  assert.doesNotMatch(short, /alertdialog/);
  assert.match(short, /Waiting for your other device\. Accept the request there\./);
  assert.match(short, />Cancel</);
});


test('GIVEN the jump palette WHEN it opens THEN it lists rooms as escaped, bounded options', () => {
  const room = (n, displayName = `room ${n}`, encryption = 'encrypted') => ({
    roomId: `!${n}:example.invalid`,
    displayName,
    unreadCount: 0,
    encryption
  });
  const rooms = [room(0, hostileName, 'not_encrypted'), room(1, null), ...Array.from({ length: 80 }, (_, i) => room(i + 2))];
  const html = render(JumpPalette, { props: { rooms, onselect: () => {}, onclose: () => {} } }).body;

  assert.match(html, /<dialog class="palette" aria-label="Jump to a room"/);
  assert.match(html, /role="combobox"[^>]*aria-controls="jump-results"/);
  assert.match(html, /aria-activedescendant="jump-option-0"/);
  assert.equal((html.match(/role="option"/g) ?? []).length, 50, 'at most 50 results');
  assert.equal((html.match(/aria-selected="true"/g) ?? []).length, 1);
  assert.doesNotMatch(html, /<img/);
  // Svelte's hydration markers may follow the text inside the <bdi>.
  assert.match(html, /<bdi class="room-name">&lt;img src=x onerror=alert\(1\)>(?:<!--[^>]*-->)*<\/bdi>/);
  assert.match(
    html,
    /<bdi class="room-name">!1:example\.invalid(?:<!--[^>]*-->)*<\/bdi>/,
    'a room without a name shows its ID'
  );
  assert.match(html, /role="img" aria-label="Not encrypted"/);
  assert.doesNotMatch(html, /<mark>/, 'nothing is highlighted before a query');
});

test('GIVEN a draft kept by the caller WHEN the composer mounts again THEN it shows the draft as text', () => {
  const html = render(Composer, {
    props: {
      pending: [],
      blocked: null,
      encryption: 'encrypted',
      roomName: 'R',
      draft: hostileName,
      draftVersion: 0,
      onsend: async () => true,
      onretry: () => {},
      ondiscard: () => {},
      onallow: () => {}
    }
  }).body;
  assert.match(html, /<textarea[^>]*>&lt;img src=x onerror=alert\(1\)><\/textarea>/);
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /<button type="submit" class="send" aria-label="Send" title="Send">/, 'a kept draft can be sent');
});

test('the verification screen heading can take focus when the screen opens', () => {
  const html = render(Verification, {
    props: {
      flow: { flowId: 'f1', phase: 'incoming', emoji: [], otherDevice: null },
      busy: false,
      onaccept: () => {},
      onconfirm: () => {},
      oncancel: () => {},
      ondismiss: () => {}
    }
  }).body;
  assert.match(html, /<h2 id="verify-heading" tabindex="-1">(?:<!--[^>]*-->)*Verify this device(?:<!--[^>]*-->)*<\/h2>/);
});

test('GIVEN timeline items with times WHEN rendered THEN times and day dividers show, and bad times do not', () => {
  const start = 1_767_258_000_000;
  const item = (n, origin_server_ts) => ({
    kind: 'message',
    event_id: `$${n}`,
    sender: '@a:example.invalid',
    body: `m${n}`,
    origin_server_ts
  });
  const html = render(Timeline, {
    props: {
      title: 'R',
      items: [
        item(1, start),
        item(2, start + 60_000),
        { kind: 'notice', event_id: '$n', sender: '@b:example.invalid', notice: 'withheld', origin_server_ts: start + 120_000 },
        // Hostile times: past JavaScript's dates, negative, fractional, text.
        item(3, Number.MAX_SAFE_INTEGER),
        item(4, -1),
        item(5, 1.5),
        item(6, '<b>noon</b>'),
        item(7, null)
      ],
      hasOlder: false,
      loading: false,
      error: null,
      onolder: () => {}
    }
  }).body;
  assert.equal((html.match(/<time datetime=/g) ?? []).length, 3, 'two messages and the notice');
  assert.match(html, /<time datetime="2026-01-01T09:00:00\.000Z"/);
  assert.match(html, /<time datetime="2026-01-01T09:02:00\.000Z"/);
  assert.equal((html.match(/class="day-divider"/g) ?? []).length, 1, 'one day');
  assert.doesNotMatch(html, /<b>noon/);
  assert.doesNotMatch(html, /Invalid Date|NaN/);
  // Every message still shows, with or without a time.
  assert.equal((html.match(/<p dir="auto">/g) ?? []).length, 7);
});

test('a pending message shows when it was queued on this device', () => {
  const html = render(Composer, {
    props: {
      pending: [
        { localId: 1, roomId: '!r:example.invalid', body: 'hi', state: 'queued', eventId: null, failure: null, queuedAt: 1_767_258_000_000 },
        { localId: 2, roomId: '!r:example.invalid', body: 'hi', state: 'queued', eventId: null, failure: null, queuedAt: -5 }
      ],
      blocked: null,
      encryption: 'encrypted',
      roomName: 'R',
      draftVersion: 0,
      onsend: async () => true,
      onretry: () => {},
      ondiscard: () => {},
      onallow: () => {}
    }
  }).body;
  assert.equal((html.match(/<time datetime="2026-01-01T09:00:00\.000Z" title="Queued on this device /g) ?? []).length, 1);
  assert.equal((html.match(/Waiting to send…/g) ?? []).length, 2);
});

test('GIVEN members whose identity changed WHEN the room shows THEN sending is paused and each can be accepted as isolated text', () => {
  const view = (props) =>
    render(IdentityChanges, {
      props: {
        busy: false,
        confirming: false,
        onaccept: () => {},
        onverify: () => {},
        ...props
      }
    }).body;
  const changes = (list, truncated = false) => ({ roomId: '!r:s', changes: list, truncated });
  const one = view({ changes: changes([{ userId: hostileName, wasVerified: false }]) });
  assert.match(one, /role="alert"/);
  assert.match(one, /Sending is paused in this room/);
  assert.match(one, /This person's encryption identity changed\./);
  assert.match(one, /<bdi>&lt;img src=x onerror=alert\(1\)><\/bdi>/);
  assert.doesNotMatch(one, /<img/);
  assert.match(one, /Accept new identity…/);
  assert.doesNotMatch(one, /Verify instead/, 'only the direct-message contact can be verified here');
  assert.doesNotMatch(one, /was verified/);
  assert.doesNotMatch(one, /<input/);

  const contact = view({
    changes: changes([{ userId: '@bob:s', wasVerified: false }]),
    contact: '@bob:s',
    canVerify: true
  });
  assert.match(contact, />\s*Verify instead\s*</);
  const unverifiedDevice = view({
    changes: changes([{ userId: '@bob:s', wasVerified: false }]),
    contact: '@bob:s',
    canVerify: false
  });
  assert.doesNotMatch(unverifiedDevice, /Verify instead/);
  // A verified contact's change blocks the verification request too.
  const wasVerified = view({
    changes: changes([{ userId: '@bob:s', wasVerified: true }]),
    contact: '@bob:s',
    canVerify: true
  });
  assert.match(wasVerified, /· was verified; accepting withdraws that/);
  assert.doesNotMatch(wasVerified, /Verify instead/);
  assert.match(wasVerified, /Accept new identity…/);

  const cut = view({ changes: changes([{ userId: '@a:s', wasVerified: false }], true) });
  assert.match(cut, /These people's encryption identities changed\./);
  assert.match(cut, /More people changed identity than are listed here/);

  const asking = view({ changes: changes([{ userId: '@a:s', wasVerified: false }]), busy: true, confirming: true });
  assert.match(asking, /Answer in the Windows window Persephone opened\./);
  const failed = view({
    changes: changes([{ userId: '@a:s', wasVerified: false }]),
    failure: 'That identity change is no longer listed.'
  });
  assert.match(failed, /<p role="alert" class="error">That identity change is no longer listed\.<\/p>/);
  assert.doesNotMatch(one, /class="error"/, 'no failure, no error line');
  assert.match(asking, /<button[^>]*disabled/);
});

test('GIVEN direct-message invitations WHEN listed THEN inviters are isolated, escaped text with Accept and Decline', () => {
  const invites = [
    {
      roomId: '!a:example.invalid',
      roomName: null,
      direct: true,
      inviter: '@mallory:example.invalid',
      inviterName: hostileName
    },
    { roomId: '!b:example.invalid', roomName: null, direct: true, inviter: '@bob:example.invalid', inviterName: null }
  ];
  const props = { invites, truncated: true, busy: null, onaccept: () => {}, ondecline: () => {} };
  const html = render(Invites, { props }).body;

  assert.doesNotMatch(html, /<img/);
  assert.match(html, /<bdi class="invite-name">&lt;img src=x onerror=alert\(1\)>(?:<!--[^>]*-->)*<\/bdi>/);
  assert.match(html, /<bdi class="invite-id">@mallory:example\.invalid(?:<!--[^>]*-->)*<\/bdi>/);
  assert.match(
    html,
    /<bdi class="invite-name">@bob:example\.invalid(?:<!--[^>]*-->)*<\/bdi>/,
    'without a name the inviter ID is the name'
  );
  assert.equal((html.match(/>Accept</g) ?? []).length, 2);
  assert.equal((html.match(/>Decline</g) ?? []).length, 2);
  assert.doesNotMatch(html, /disabled/, 'nothing is being answered');
  assert.match(html, /Showing the first 2 invitations\./);

  const busy = render(Invites, { props: { ...props, busy: '!a:example.invalid' } }).body;
  assert.equal((busy.match(/disabled/g) ?? []).length, 4, 'one answer at a time');
});

test('GIVEN room invitations over federation WHEN listed THEN the room name is isolated text and a missing inviter is "Someone"', () => {
  const invites = [
    {
      roomId: '!plans:remote.invalid',
      roomName: hostileName,
      direct: false,
      inviter: '@carol:remote.invalid',
      inviterName: null
    },
    { roomId: '!x:remote.invalid', roomName: null, direct: false, inviter: null, inviterName: null }
  ];
  const html = render(Invites, {
    props: { invites, truncated: false, busy: null, onaccept: () => {}, ondecline: () => {} }
  }).body;

  assert.doesNotMatch(html, /<img/);
  assert.match(html, /invited you to <bdi class="invite-room">&lt;img src=x onerror=alert\(1\)>(?:<!--[^>]*-->)*<\/bdi>/);
  assert.match(html, /<span class="invite-name">Someone<\/span>/);
  assert.match(html, /invited you to a room/);
  assert.doesNotMatch(html, /wants to talk with you/, 'neither is a direct invitation');
  assert.equal((html.match(/>Accept</g) ?? []).length, 2, 'each can be answered');
});

test('GIVEN the new-room dialog WHEN it opens THEN nothing can be created or added yet, and it says the room is encrypted', () => {
  const html = render(NewRoom, { props: { onopen: () => {}, onclose: () => {} } }).body;

  assert.match(html, /<dialog class="picker new-room" aria-labelledby="new-room-heading"/);
  assert.match(html, /<input id="new-room-name" class="field" type="text" maxlength="255"/);
  assert.match(html, /<input id="new-room-person" class="field" type="text"[^>]*maxlength="255"/);
  assert.match(html, /<button type="button" class="secondary" disabled[^>]*>Add<\/button>/, 'no Matrix ID yet');
  assert.match(html, /<button type="submit" class="primary" disabled/, 'no room name yet');
  assert.match(html, /The room is encrypted and invite-only\./);
  assert.doesNotMatch(html, /People to invite/);
});

test('GIVEN a room with an invite action WHEN the header shows THEN it offers Invite, and only then', () => {
  const header = (oninvite) =>
    render(Timeline, {
      props: { title: 'general', items: [], hasOlder: false, loading: false, error: null, onolder: () => {}, oninvite }
    }).body;
  assert.match(header(() => {}), /<button type="button" class="secondary header-action">Invite<\/button>/);
  assert.doesNotMatch(header(undefined), />Invite</);
});

test('GIVEN a room with a settings action WHEN the header shows THEN it offers a labelled settings button, and only then', () => {
  const header = (onsettings) =>
    render(Timeline, {
      props: { title: 'general', items: [], hasOlder: false, loading: false, error: null, onolder: () => {}, onsettings }
    }).body;
  assert.match(header(() => {}), /<button type="button" class="icon-button header-settings" aria-label="Room settings"/);
  assert.doesNotMatch(header(undefined), /Room settings/);
});

test('GIVEN the room settings WHEN they open THEN the room name is isolated text, the tabs show and nothing is changeable before they load', () => {
  const html = render(RoomSettings, {
    props: { roomId: '!r:example.invalid', roomName: hostileName, changes: 0, onleft: () => {}, onclose: () => {} }
  }).body;
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /<dialog class="room-settings" aria-labelledby="room-settings-heading"/);
  assert.match(html, /<bdi>&lt;img src=x onerror=alert\(1\)><\/bdi>/);
  for (const tab of ['General', 'People', 'Notifications', 'Security &amp; privacy', 'Advanced']) {
    assert.match(html, new RegExp(String.raw`class="room-row"[^>]*>\s*${tab}\s*</button>`), tab);
  }
  assert.match(html, /aria-current="page"[^>]*>\s*General/);
  assert.match(html, /aria-label="Close room settings"/);
  assert.match(html, /role="status"[^>]*>Loading the room's settings…/);
  assert.doesNotMatch(html, /Save name|Leave room|Turn on encryption/);
});

test('GIVEN a level WHEN the picker shows THEN it names the roles this account may give and the current level', () => {
  const picker = (level, own, disabled = false) =>
    render(LevelPicker, {
      props: { id: 'p', label: 'Role of <b>Bob</b>', level, own, disabled, onpick: () => {} }
    }).body;
  const moderator = picker(0, 50);
  assert.match(moderator, /aria-label="Role of &lt;b>Bob&lt;\/b>"/);
  assert.doesNotMatch(moderator, />Admin</, 'not above this account');
  assert.match(moderator, /<option value="50"[^>]*>Moderator<\/option>/);
  assert.match(moderator, /<option value="0"[^>]*selected[^>]*>Member<\/option>/);
  assert.match(moderator, /<option value="custom"[^>]*>Custom…<\/option>/);
  assert.match(picker(75, null), /<option value="75"[^>]*selected[^>]*>Level 75<\/option>/);
  assert.match(picker(0, 100, true), /<select id="p" class="field" aria-label="[^"]*" disabled="">/);
  assert.doesNotMatch(moderator, /disabled/);
});

test('GIVEN Enter to send on or off WHEN the composer shows THEN its key shortcut says which keys send', () => {
  const composer = (enterSends) =>
    render(Composer, {
      props: {
        pending: [],
        blocked: null,
        encryption: 'encrypted',
        roomName: 'general',
        draftVersion: 0,
        onsend: async () => true,
        onretry: () => {},
        ondiscard: () => {},
        onallow: () => {},
        enterSends
      }
    }).body;
  assert.match(composer(true), /<textarea[^>]*aria-keyshortcuts="Enter"/);
  assert.match(composer(false), /<textarea[^>]*aria-keyshortcuts="Control\+Enter Meta\+Enter"/);
  assert.match(composer(undefined), /aria-keyshortcuts="Enter"/, 'on by default');
});

test('GIVEN the new-conversation dialog WHEN it opens THEN directory search is off and nothing can start yet', () => {
  const html = render(NewConversation, { props: { onopen: () => {}, onclose: () => {} } }).body;

  assert.match(html, /<dialog class="picker new-conversation" aria-labelledby="new-conversation-heading"/);
  assert.match(html, /<input id="new-conversation-id" class="field" type="text"[^>]*maxlength="255"/);
  assert.match(html, /<input type="checkbox" role="switch"(?![^>]*checked)[^>]*>/, 'directory search starts off');
  assert.match(html, /Off: nothing is sent until you press Start\./);
  assert.doesNotMatch(html, /Directory matches/);
  assert.match(html, /<button type="submit" class="primary" disabled/, 'no Matrix ID yet');
});

test('GIVEN the preferences WHEN they show THEN the sizes, the level and desktop notifications are offered, with no message text', () => {
  const settings = {
    enterSends: true,
    chatTextPercent: 125,
    interfaceTextPercent: 100,
    desktopNotifications: false
  };
  const html = render(Preferences, {
    props: { settings, failure: null, mac: false, onchange: () => {} }
  }).body;
  assert.match(html, /<label for="chat-text-size">Message text</);
  assert.match(html, /<label for="interface-text-size">Interface</);
  assert.match(html, /<option value="200">200%/);
  assert.doesNotMatch(html, /<option value="175">175%<\/option>[^]*id="interface-text-size"[^]*<option value="175">/);
  for (const level of ['All messages', 'Mentions and keywords only', 'Nothing']) {
    assert.match(html, new RegExp(`<span>${level}</span>`));
  }
  assert.match(html, /never the message itself/);
  assert.match(html, /Show desktop notifications/);
  // The level is read from the homeserver; until then nothing can be chosen.
  assert.match(html, /<fieldset class="choices" disabled/);
});

test('GIVEN the account page WHEN it shows THEN passwords are never asked on the page and other sessions are signed out by default', () => {
  const html = render(AccountPage, { props: { native: 'Windows' } }).body;
  assert.match(html, />Change password…</);
  assert.match(html, /<input type="checkbox" checked/);
  assert.match(html, /Sign out my other sessions/);
  assert.match(html, /own Windows windows, never on this page/);
  assert.doesNotMatch(html, /type="password"/);
  assert.match(html, /does not share them with an\s+identity server/);
});

test('GIVEN new sessions WHEN the alert shows THEN names are isolated text and each session can be signed out', async () => {
  const { default: NewSessions } = await vite.ssrLoadModule('/src/NewSessions.svelte');
  const html = render(NewSessions, {
    props: {
      found: {
        sessions: [
          { deviceId: 'ABC', displayName: hostileName },
          { deviceId: 'DEF', displayName: null }
        ],
        truncated: true
      },
      onknown: () => {},
      onremove: () => {}
    }
  }).body.replace(/<!--.*?-->/g, '');
  assert.doesNotMatch(html, /<img/);
  assert.match(html, /role="alert"/);
  assert.match(html, /New sessions signed in/);
  assert.match(html, /<bdi>&lt;img src=x onerror=alert\(1\)><\/bdi>/);
  assert.equal((html.match(/>Sign it out</g) ?? []).length, 2);
  assert.match(html, />It was me</);
  assert.match(html, /More new sessions are in Settings/);
});

test('GIVEN sessions WHEN this device cannot vouch THEN no Verify shows, and WHEN it can THEN only other unverified sessions get one', async () => {
  // Without a host the list does not load in server rendering, so the
  // offer is checked through the component's own rules.
  const source = await import('node:fs/promises').then(({ readFile }) =>
    readFile(new URL('../src/Devices.svelte', import.meta.url), 'utf8')
  );
  // Other sessions get actions; Verify only when unverified and this device
  // can vouch, Sign out always.
  assert.match(source, /\{#if !device\.current\}[\s\S]*\{#if !device\.verified && canVerify\}/);
  assert.match(source, /onverify\(device\.deviceId\)/);
  assert.match(source, /onclick=\{\(\) => remove\(device\.deviceId\)\}/);
  const html = render(Devices, { props: { canVerify: true } }).body;
  assert.doesNotMatch(html, />Verify</, 'nothing listed yet');
});
