const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
const start = source.indexOf('function renderTime(');
const end = source.indexOf('\n$("#p-timer").addEventListener', start);
assert.ok(start >= 0 && end > start);

function screen(status) {
  const nodes = new Map();
  const select = (key) => {
    if (!nodes.has(key)) nodes.set(key, {
      hidden: false, disabled: false, textContent: '', attributes: {}, nodes: [],
      setAttribute(name, value) { this.attributes[name] = value; },
      replaceChildren(...items) { this.nodes = items; },
    });
    return nodes.get(key);
  };
  const ctx = {
    panelTask: { id: 1, status }, state: { task: 1 }, $: select,
    el: (tag, attrs, ...children) => ({ tag, attrs, children: children.filter((child) => child != null) }),
    duration: (seconds) => `${Math.floor(seconds)}s`, setInterval() {}, Date,
  };
  vm.createContext(ctx);
  vm.runInContext(source.slice(start, end) + '\nthis.render = renderTime; this.tick = updateRunningTime;', ctx);
  return { ctx, select };
}

function entry(automatic, ended_at = null) {
  return { id: automatic ? -1 : 2, task_id: 1, started_at: Math.floor(Date.now() / 1000) - 12,
    ended_at, seconds: ended_at == null ? 0 : 12, automatic };
}

const now = () => Math.floor(Date.now() / 1000);

test('doing displays live automatic time without offering a second timer or a duplicate manual log', () => {
  const { ctx, select } = screen('doing');
  const auto = entry(true);
  ctx.render({ total_seconds: 12, running_since: auto.started_at, running_until: now() + 300,
    as_of: auto.started_at + 12, entries: [auto] });
  assert.equal(select('#p-timer').disabled, true);
  assert.equal(select('#p-timer').textContent, 'Agent activity');
  assert.equal(select('#p-timer').attributes['aria-label'], 'Time follows agent activity while doing');
  assert.equal(select('#p-minutes').disabled, true);
  assert.equal(select('#p-time-form button').disabled, true);
  assert.match(select('#p-time-running').textContent, /^Agent activity · /);
  assert.equal(select('#p-time-list').nodes[0].children[0].children[0], 'Agent · Active now');
  assert.equal(select('#p-time-list').nodes[0].children.length, 2, 'automatic entry has no remove action');
});

test('done freezes auto entry; manual time remains independently editable outside doing', () => {
  const { ctx, select } = screen('done');
  const auto = entry(true, Math.floor(Date.now() / 1000));
  const manual = entry(false, auto.ended_at);
  ctx.render({ total_seconds: 24, running_since: null, running_until: null, as_of: auto.ended_at,
    entries: [auto, manual] });
  assert.equal(select('#p-timer').disabled, false);
  assert.equal(select('#p-timer').textContent, 'Start timer');
  assert.equal(select('#p-minutes').disabled, false);
  assert.equal(select('#p-time-list').nodes[0].children.length, 2);
  assert.equal(select('#p-time-list').nodes[1].children[2].tag, 'button');
  assert.equal(select('#p-time-list').nodes[0].children[0].children[0], 'Agent · 12s');
});

test('activity elapsed freezes at the idle gap instead of running up empty hours', () => {
  const { ctx, select } = screen('doing');
  const started = now() - 1000;
  const auto = { id: -1, task_id: 1, started_at: started, ended_at: null, seconds: 0, automatic: true };
  ctx.render({ total_seconds: 300, running_since: started, running_until: started + 300, as_of: now(), entries: [auto] });
  assert.match(select('#p-time-running').textContent, /· 300s elapsed$/);
  assert.equal(select('#p-time-total').textContent, '300s tracked');
});

test('a lapsed activity interval reads Idle and is not counted as running', () => {
  const { ctx, select } = screen('doing');
  const started = now() - 1000;
  const auto = { id: -1, task_id: 1, started_at: started, ended_at: null, seconds: 0, automatic: true };
  ctx.render({ total_seconds: 300, running_since: null, running_until: null, as_of: now(), entries: [auto] });
  assert.equal(select('#p-time-running').hidden, true);
  assert.equal(select('#p-time-list').nodes[0].children[0].children[0], 'Agent · Idle');
  assert.equal(select('#p-time-total').textContent, '300s tracked');
});
