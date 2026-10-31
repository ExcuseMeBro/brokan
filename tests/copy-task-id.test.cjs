const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
const html = fs.readFileSync(path.join(__dirname, '../src/index.html'), 'utf8');
const start = source.indexOf('async function copyKey(');
const end = source.indexOf('\nfunction toast(', start);
assert.ok(start >= 0 && end > start);
const copySource = source.slice(start, end).trim();

function setup(clipboard, fallback) {
  const notices = [];
  let area;
  const focused = { isConnected: true, focus() { this.restored = true; } };
  const context = {
    navigator: { clipboard },
    el: () => (area = { value: '', select() { this.selected = true; }, remove() { this.removed = true; } }),
    document: { activeElement: focused, body: { append() {} }, execCommand: fallback },
    toast: (message) => notices.push(message),
  };
  const copyKey = vm.runInNewContext(`(${copySource})`, context);
  return { copyKey, notices, focused, get area() { return area; } };
}

test('secure clipboard copies the exact project-qualified task ID', async () => {
  const writes = [];
  const fixture = setup({ writeText: async (value) => writes.push(value) }, () => { throw Error('fallback must not run'); });
  assert.equal(await fixture.copyKey('api#12'), true);
  assert.equal(writes.join(','), 'api#12');
  assert.equal(fixture.notices.join(','), 'Copied api#12');
  assert.equal(fixture.area, undefined);
});

test('HTTP fallback copies the ID and cleans up its temporary textarea', async () => {
  const fixture = setup({ writeText: async () => { throw Error('insecure page'); } }, () => true);
  assert.equal(await fixture.copyKey('api#12'), true);
  assert.equal(fixture.area.value, 'api#12');
  assert.equal(fixture.area.selected, true);
  assert.equal(fixture.area.removed, true);
  assert.equal(fixture.focused.restored, true);
  assert.equal(fixture.notices.join(','), 'Copied api#12');
});

test('denied clipboard never claims a successful copy', async () => {
  const fixture = setup({ writeText: async () => { throw Error('denied'); } }, () => false);
  assert.equal(await fixture.copyKey('api#12'), false);
  assert.equal(fixture.area.removed, true);
  assert.equal(fixture.notices.join(','), 'Could not copy api#12');
  const throwing = setup({ writeText: async () => { throw Error('denied'); } }, () => { throw Error('denied'); });
  assert.equal(await throwing.copyKey('api#12'), false);
  assert.equal(throwing.area.removed, true);
});

test('task header offers a named, focusable copy button with feedback and keeps closed panel inert', () => {
  assert.match(html, /<div class="panel" id="panel" aria-hidden="true" inert>/);
  assert.match(html, /<button class="key mono" id="p-key" type="button" aria-label="Copy task ID"[^>]*><span id="p-key-text"><\/span><span class="icon" data-icon="copy"><\/span><\/button>/);
  assert.match(html, /#p-key:focus-visible\s*\{\s*outline:\s*2px solid var\(--link\)/);
  assert.match(source, /\$\("#p-key-text"\)\.textContent = key\(task\)/);
  assert.match(source, /\$\("#p-key"\)\.addEventListener\("click", \(\) => \{ if \(panelTask\) copyKey\(key\(panelTask\)\); \}\)/);
  assert.match(source, /class: "toast", role: "status"/);
  assert.match(source, /panel\.inert = false/);
  assert.match(source, /panel\.inert = true/);
});
