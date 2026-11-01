const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
const html = fs.readFileSync(path.join(__dirname, '../src/index.html'), 'utf8');
const start = source.indexOf('function card(');
const end = source.indexOf('\nfunction wireDrop(', start);
assert.ok(start >= 0 && end > start);
const el = (tag, attrs = {}, ...children) => ({
  tag, attrs, children: children.flat().filter((child) => child != null), addEventListener() {},
});
const cardBody = source.slice(start, end);
const card = vm.runInNewContext(`(${cardBody.slice(0, cardBody.lastIndexOf('\n}') + 2)})`, {
  state: { task: null }, el, icon: (name) => ({ name }),
  key: (task) => `brokan#${task.id}`, copyKey() {}, openTask() {},
  deadlineBadge: () => null, plainText: (value) => value,
});
const task = { id: 514, title: 'A task', description: '', priority: 'none', tags: [], project: 'brokan' };
function owner(node) { return node.children[0].children[1]; }

test('human cards show a labeled person; agent cards show a labeled agent', () => {
  const human = owner(card({ ...task, human: true }, false));
  assert.equal(human.attrs.class, 'owner-mark');
  assert.equal(human.attrs.role, 'img');
  assert.equal(human.attrs['aria-label'], 'Human task: agents skip it');
  assert.equal(human.children[0].name, 'human');
  const agent = owner(card({ ...task, human: false }, false));
  assert.equal(agent.attrs.class, 'owner-mark');
  assert.equal(agent.attrs.role, 'img');
  assert.equal(agent.attrs['aria-label'], 'Agent task');
  assert.equal(agent.children[0].name, 'agent');
  assert.match(source, /agent: '<rect x="4" y="7"/);
  assert.match(html, /\.card \.owner-mark \.icon svg \{ width: 15px; height: 15px; \}/);
});

test('changing the human flag updates the icon; missing flag is not mislabeled', () => {
  const before = card({ ...task, human: true }, false);
  const after = card({ ...task, human: false }, false);
  assert.equal(owner(before).children[0].name, 'human');
  assert.equal(owner(after).children[0].name, 'agent');
  assert.equal(owner(card(task, false)).attrs.class, 'num');
  assert.equal(after.children[0].children[2].attrs.class, 'num');
});
