import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { test } from 'node:test';
import { DEFAULT_THEME, THEMES, applyTheme, themeOf } from '../src/themes.ts';

const source = new URL('../src/', import.meta.url);
const css = (await readFile(new URL('style.css', source), 'utf8')).replace(/\/\*[^]*?\*\//g, '');

// Every rule of the stylesheet: its selectors and its declarations. The
// token blocks hold no nested rules, so one level is enough.
const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map(([, selector, body]) => ({
  selectors: selector.split(',').map((one) => one.trim()),
  declarations: [...body.matchAll(/([\w-]+)\s*:\s*([^;]+)/g)].map(([, name, value]) => [
    name,
    value.trim()
  ])
}));

const tokenRule = (rule) => rule.selectors.some((selector) => selector.startsWith(':root'));
const tokensOf = (rule) =>
  Object.fromEntries(rule.declarations.filter(([name]) => name.startsWith('--')));

// The tokens one theme block sets, and the tokens every theme shares: the
// plain `:root` blocks, which include the default theme's block.
const themeBlock = (theme) => {
  const blocks = rules.filter((rule) => rule.selectors.includes(`:root[data-theme='${theme}']`));
  assert.equal(blocks.length, 1, `one block for the ${theme} theme`);
  return tokensOf(blocks[0]);
};
const shared = Object.assign(
  {},
  ...rules.filter((rule) => rule.selectors.includes(':root')).map(tokensOf)
);
const resolved = (theme) => ({ ...shared, ...themeBlock(theme) });

function luminance(hex) {
  const match = /^#([0-9a-f]{6})$/i.exec(hex);
  assert.ok(match, `${hex} is a six-digit colour`);
  const [r, g, b] = [0, 2, 4].map((at) => {
    const channel = parseInt(match[1].slice(at, at + 2), 16) / 255;
    return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a, b) {
  const [light, dark] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (light + 0.05) / (dark + 0.05);
}

const SURFACES = ['rail', 'bg', 'panel', 'card', 'hover', 'sel'];
const TEXT = ['ink', 'ink2', 'ink3', 'accent', 'accent-hover', 'danger-text', 'warn-ink', 'enc-fg'];

// [foreground, background, minimum]: WCAG 2.2 AA is 4.5:1 for text and 3:1
// for focus indicators, control borders and icons (1.4.3, 1.4.11).
const PAIRS = [
  ...TEXT.flatMap((text) => SURFACES.map((surface) => [text, surface, 4.5])),
  ['accent-ink', 'accent', 4.5],
  ['accent-ink', 'accent-hover', 4.5],
  ['ink', 'accent-soft', 4.5],
  ['accent', 'accent-soft', 4.5],
  ['warn-ink', 'warn-bg', 4.5],
  ['ink', 'line', 4.5],
  ['match-ink', 'match', 4.5],
  ...[0, 1, 2, 3, 4, 5].map((n) => ['on-solid', `avatar-${n}`, 4.5]),
  ...SURFACES.map((surface) => ['focus', surface, 3]),
  ['focus', 'accent-soft', 3],
  ...['rail', 'bg', 'panel', 'card', 'hover'].map((surface) => ['line-strong', surface, 3]),
  ...SURFACES.flatMap((surface) => [
    ['danger', surface, 3],
    ['secure', surface, 3]
  ]),
  ['on-solid', 'danger', 3],
  ['on-solid', 'secure', 3],
  ['ink3', 'line', 3]
];

test('GIVEN the themes offered WHEN listed THEN there are exactly three, each with its own colour block', () => {
  assert.deepEqual(
    THEMES.map((theme) => theme.value),
    ['orange', 'red', 'blue']
  );
  assert.equal(new Set(THEMES.map((theme) => theme.label)).size, 3);
  assert.equal(DEFAULT_THEME, 'orange');
  // The default block also styles a root with no theme or an unknown one.
  assert.ok(
    rules.some(
      (rule) => rule.selectors.includes(':root') && rule.selectors.includes(":root[data-theme='orange']")
    )
  );
});

test('GIVEN each theme WHEN its tokens are read THEN it sets every token the default theme sets, and no other', () => {
  const names = Object.keys(themeBlock(DEFAULT_THEME)).sort();
  assert.ok(names.length >= 20);
  for (const { value } of THEMES) {
    assert.deepEqual(Object.keys(themeBlock(value)).sort(), names, value);
  }
});

for (const { value: theme } of THEMES) {
  test(`GIVEN the ${theme} theme WHEN its colour pairs are measured THEN text and focus indicators meet WCAG AA`, () => {
    const tokens = resolved(theme);
    const failures = PAIRS.flatMap(([fg, bg, minimum]) => {
      assert.ok(tokens[`--${fg}`] && tokens[`--${bg}`], `${theme} sets --${fg} and --${bg}`);
      const ratio = contrast(tokens[`--${fg}`], tokens[`--${bg}`]);
      return ratio >= minimum ? [] : [`--${fg} on --${bg}: ${ratio.toFixed(2)} < ${minimum}`];
    });
    assert.deepEqual(failures, []);
  });

  test(`GIVEN the ${theme} theme WHEN its surfaces are compared THEN each layer is lighter than the one below`, () => {
    const tokens = resolved(theme);
    const levels = SURFACES.map((surface) => luminance(tokens[`--${surface}`]));
    for (let i = 1; i < levels.length; i += 1) {
      assert.ok(levels[i] > levels[i - 1], `--${SURFACES[i]} is lighter than --${SURFACES[i - 1]}`);
    }
    // Nearly black: the red and blue backgrounds are darker than the default's.
    if (theme !== DEFAULT_THEME) {
      assert.ok(luminance(tokens['--bg']) < luminance(resolved(DEFAULT_THEME)['--bg']));
    }
  });
}

// A colour written as a hex value, a colour function or a common name.
const COLOUR = /#[0-9a-f]{3,8}\b|\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color)\(|(?<![\w-])(?:white|black|red|green|blue|orange|gr[ae]y|yellow|purple|pink|silver|navy|maroon)(?![\w-])/i;

test('GIVEN the stylesheet WHEN read outside the token blocks THEN every colour is a token', () => {
  const stray = rules
    .filter((rule) => !tokenRule(rule))
    .flatMap((rule) =>
      rule.declarations
        .filter(([, value]) => COLOUR.test(value))
        .map(([name, value]) => `${rule.selectors.join(', ')} { ${name}: ${value} }`)
    );
  assert.deepEqual(stray, []);
});

test('GIVEN the components WHEN read THEN none writes a colour of its own', async () => {
  const files = (await readdir(source, { recursive: true })).filter((name) => /\.(svelte|svg)$/.test(name));
  assert.ok(files.length > 10);
  for (const name of files) {
    const text = await readFile(new URL(name, source), 'utf8');
    assert.doesNotMatch(text, /#[0-9a-f]{6}\b|#[0-9a-f]{3}\b(?=["';\s])|\b(?:rgba?|hsla?)\(/i, name);
    assert.doesNotMatch(text, /<style[\s>]|\b(?:fill|stroke|color)=["'](?!none|currentColor)/i, name);
  }
});

test('GIVEN a stored theme WHEN it names a known theme THEN it is kept', () => {
  for (const { value } of THEMES) assert.equal(themeOf(value), value);
});

test('GIVEN a stored theme WHEN it is unknown or not a theme name THEN the default theme is used', () => {
  for (const value of ['green', 'Red', 'BLUE', ' orange', '', 'light', null, undefined, 0, 1, true, {}, ['red']]) {
    assert.equal(themeOf(value), DEFAULT_THEME, JSON.stringify(value) ?? 'undefined');
  }
});

test('GIVEN the root element WHEN a theme is applied THEN it carries that theme, or the default for an unknown one', () => {
  const root = { dataset: {} };
  assert.equal(applyTheme(root, 'blue'), 'blue');
  assert.equal(root.dataset.theme, 'blue');
  assert.equal(applyTheme(root, 'red'), 'red');
  assert.equal(root.dataset.theme, 'red');
  assert.equal(applyTheme(root, 'sepia'), DEFAULT_THEME);
  assert.equal(root.dataset.theme, DEFAULT_THEME);
  assert.equal(applyTheme(root, undefined), DEFAULT_THEME);
  assert.equal(root.dataset.theme, DEFAULT_THEME);
});
