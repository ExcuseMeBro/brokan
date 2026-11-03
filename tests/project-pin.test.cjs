// Dependency-free checks of sidebar pin order, menu actions and persistence requests.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
const part = (start, end) => {
  const a = source.indexOf(start), b = source.indexOf(end, a);
  assert.ok(a >= 0 && b > a, `source for ${start}`);
  return source.slice(a, b);
};
const el = (tag, attrs = {}, ...children) => ({
  tag, attrs, children: children.flat().filter((x) => x != null && x !== false),
  style: {}, classList: { add() {}, remove() {} },
  setAttribute(key, value) { this.attrs[key] = value; },
  replaceChildren(...nodes) { this.children = nodes; },
  addEventListener() {}, focus() {}, closest() { return { classList: this.classList }; },
});
const walk = (node, predicate) => node && typeof node === 'object'
  ? [...(predicate(node) ? [node] : []), ...(node.children || []).flatMap((child) => walk(child, predicate))] : [];
const project = (slug, pinned = false, archived = false) => ({ slug, name: slug, pinned, archived, open: 1 });

test('pinned active projects come first without mutating the board; archived remain separate', () => {
  const projects = [project('a'), project('b', true), project('c'), project('d', true), project('old', true, true)];
  const state = { board: { projects }, dashboard: false, gitOverview: false, project: 'c', menu: null };
  const root = { replaceChildren(...nodes) { this.children = nodes; } };
  const renderNav = vm.runInNewContext(`(${part('function renderNav()', '// ---------- read-only local Git overview ----------')})`, {
    state, renaming: null, document: { contains: () => false },
    $: (selector) => { assert.equal(selector, '#nav'); return root; },
    el, icon: (name) => el('svg', {}, name), projectGlyph: (slug) => el('svg', {}, slug), localStorageGet: () => null,
  });
  renderNav();
  const items = root.children.filter((node) => node.attrs?.class === 'item');
  assert.deepEqual(items.map((item) => item.children[0].attrs.href), ['#b', '#d', '#a', '#c']);
  assert.deepEqual(projects.map((p) => p.slug), ['a', 'b', 'c', 'd', 'old']);
  assert.equal(items[3].children[0].attrs.class, 'active');
  assert.equal(walk(items[0], (n) => n.attrs?.['aria-label'] === 'Pinned project').length, 1);
  assert.equal(walk(items[2], (n) => n.attrs?.['aria-label'] === 'Pinned project').length, 0);
  const archived = root.children.find((node) => node.attrs?.class === 'archive-group');
  assert.equal(walk(archived, (n) => n.attrs?.href === '#old').length, 1);
  assert.equal(walk(archived, (n) => n.attrs?.['aria-label'] === 'Pinned project').length, 1);
});

test('project menu offers the matching pin or unpin action', () => {
  for (const pinned of [false, true]) {
    let menu, call;
    const p = project('alpha', pinned);
    const button = { ...el('button'), isConnected: true, getBoundingClientRect: () => ({ bottom: 10, top: 5, left: 5 }) };
    const toggle = vm.runInNewContext(`(${part('function toggleProjectMenu(', '// Renaming a project')})`, {
      state: { menu: null }, closeMenu() {}, el, icon: (name) => el('svg', {}, name),
      PROJECT_COLORS: {}, PROJECT_ICONS: [], projectColorName() {}, projectIconName() {}, projectColor() {},
      document: { body: { append(node) { menu = node; } } }, innerHeight: 900, innerWidth: 900,
      pinProject(...args) { call = args; },
    });
    toggle(p, button);
    const name = pinned ? 'Unpin project' : 'Pin project';
    const action = walk(menu, (n) => n.tag === 'button' && n.children.includes(name))[0];
    assert.ok(action);
    assert.equal(action.attrs.type, 'button');
    action.attrs.onclick();
    assert.equal(call[0], p);
    assert.equal(call[1], !pinned);
    assert.equal(call[2], button);
  }
});

test('pin action sends an encoded PATCH, reloads, restores focus and reports failure', async () => {
  for (const { fail = false, stale = false, moved = false } of [{}, { fail: true }, { stale: true }, { moved: true }]) {
    const calls = [], messages = [];
    const document = { body: {}, activeElement: null, querySelectorAll: () => [next] };
    const button = { isConnected: !stale, focus() { document.activeElement = button; } };
    const next = { dataset: { projectSlug: 'one/two' }, focus() { document.activeElement = next; } };
    const other = {};
    const pinProject = vm.runInNewContext(`(${part('async function pinProject(', 'async function archiveProject(')})`, {
      document, closeMenu() { calls.push('close'); },
      api: async (method, url, data) => { calls.push([method, url, data.pinned]); if (fail) throw Error('offline'); },
      loadBoard: async () => { calls.push('load'); document.activeElement = moved ? other : document.body; },
      toast: (message) => messages.push(message),
    });
    await pinProject(project('one/two'), true, button);
    assert.deepEqual(calls[1], ['PATCH', '/projects/one%2Ftwo', true]);
    assert.equal(calls.includes('load'), !fail);
    assert.equal(document.activeElement, fail ? button : moved ? other : next);
    assert.equal(messages[0], fail ? 'offline' : 'one/two pinned.');
  }
});
