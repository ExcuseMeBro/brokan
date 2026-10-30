// Dependency-free calendar/Gantt local-date tests, including DST boundaries.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
function body(name, next) {
  const start = source.indexOf(`function ${name}(`);
  const end = source.indexOf(`\nfunction ${next}(`, start);
  assert.ok(start >= 0 && end > start, `${name} source is present`);
  const part = source.slice(start, end);
  return part.slice(0, part.lastIndexOf('\n}') + 2);
}
const { localDayNumber, monthSchedule } = vm.runInNewContext(
  `${body('localDayNumber', 'monthSchedule')}\n${body('monthSchedule', 'renderTimeline')}\n({ localDayNumber, monthSchedule })`);

function withZone(zone, fn) {
  const previous = process.env.TZ;
  process.env.TZ = zone;
  try { fn(); } finally { if (previous === undefined) delete process.env.TZ; else process.env.TZ = previous; }
}

test('calendar days use local dates through the spring DST transition', () => withZone('America/Los_Angeles', () => {
  assert.equal(localDayNumber('2026-03-08T07:59:00Z'), localDayNumber('2026-03-07T12:00:00Z'));
  assert.equal(localDayNumber('2026-03-08T08:00:00Z'), localDayNumber('2026-03-09T06:59:00Z'));
  assert.equal(localDayNumber('2026-03-09T07:00:00Z') - localDayNumber('2026-03-08T08:00:00Z'), 1);
  const tasks = [
    { id: 1, title: 'Span', start_at: '2026-03-07T18:00:00Z', deadline_at: '2026-03-09T06:59:00Z' },
    { id: 2, title: 'Deadline only', start_at: null, deadline_at: '2026-03-09T07:00:00Z' },
    { id: 3, title: 'Start only', start_at: '2026-03-08T08:00:00Z', deadline_at: null },
    { id: 4, title: 'Unscheduled', start_at: null, deadline_at: null },
    { id: 5, title: 'Outside', start_at: '2026-02-01T10:00:00Z', deadline_at: '2026-02-02T10:00:00Z' },
    { id: 6, title: 'Clipped', start_at: '2026-02-01T10:00:00Z', deadline_at: '2026-04-02T10:00:00Z' },
  ];
  const result = monthSchedule(tasks, new Date(2026, 2, 1));
  assert.equal(result.days, 31);
  assert.equal(result.events[6].map((e) => e.kind).join(','), 'Start');
  assert.equal(result.events[7].map((e) => e.kind).join(','), 'Due,Start');
  assert.equal(result.events[8].map((e) => e.kind).join(','), 'Due');
  assert.equal(result.rows.map((r) => r.task.id).includes(4), false);
  assert.equal(result.rows.map((r) => r.task.id).includes(5), false);
  const span = result.rows.find((r) => r.task.id === 1);
  assert.equal(`${span.start}-${span.end}-${span.milestone}`, '6-7-false');
  assert.equal(`${result.rows.find((r) => r.task.id === 6).start}-${result.rows.find((r) => r.task.id === 6).end}`, '0-30');
  assert.equal(result.rows.find((r) => r.task.id === 2).milestone, true);
}));

test('calendar month/combined start-and-due and timezone rollover', () => withZone('Asia/Tashkent', () => {
  const task = { id: 7, title: 'Same day', start_at: '2026-04-30T20:00:00Z', deadline_at: '2026-05-01T01:00:00Z' };
  const may = monthSchedule([task], new Date(2026, 4, 1));
  assert.equal(may.days, 31);
  assert.equal(may.events[0].length, 1);
  assert.equal(may.events[0][0].kind, 'Start · Due');
  assert.equal(`${may.rows[0].start}-${may.rows[0].end}`, '0-0');
  assert.equal(monthSchedule([task], new Date(2026, 3, 1)).events.flat().length, 0);
}));

test('calendar events and Gantt spans render task links, month navigation and empty state', () => {
  const task = { id: 1, project: 'api', title: 'Build', status: 'todo', human: false,
    start_at: '2026-03-03T10:00:00Z', deadline_at: '2026-03-05T10:00:00Z' };
  const root = { hidden: true, replaceChildren(...nodes) { this.nodes = nodes; },
    querySelector(selector) { return selector.startsWith('button[') ? { focus() {} } : null; } };
  const state = { view: 'calendar', dashboard: false, board: { tasks: [task] },
    timelineMonth: new Date(2026, 2, 1), task: null };
  const el = (tag, attrs = {}, ...children) => ({ tag, attrs, children: children.flat().filter((x) => x != null && x !== false) });
  const walk = (node, match) => Array.isArray(node) ? node.flatMap((child) => walk(child, match))
    : !node || typeof node !== 'object' ? []
      : [...(match(node) ? [node] : []), ...(node.children || []).flatMap((child) => walk(child, match))];
  const render = vm.runInNewContext(`(${body('renderTimeline', 'isHumanOnly')})`, {
    state, $: () => root, document: { activeElement: null }, el,
    monthSchedule, localDayNumber, isHumanOnly: () => false, projectColor: () => '#3b82f6',
    openTask: async () => {},
  });
  render();
  assert.equal(root.hidden, false);
  assert.equal(walk(root.nodes, (n) => n.attrs?.class === 'calendar-cell').length >= 31, true);
  assert.equal(walk(root.nodes, (n) => n.tag === 'a').length, 2); // start and deadline
  state.view = 'gantt';
  render();
  assert.equal(walk(root.nodes, (n) => n.tag === 'table').length, 1);
  assert.equal(walk(root.nodes, (n) => n.tag === 'table')[0].attrs.style, 'width:1398px');
  assert.equal(walk(root.nodes, (n) => n.tag === 'td' && n.attrs?.colspan === 3).length, 1);
  assert.equal(walk(root.nodes, (n) => n.tag === 'a')[0].attrs.href, '#api/1');
  const previous = walk(root.nodes, (n) => n.tag === 'button' && n.attrs?.['data-shift'] === '-1')[0];
  previous.attrs.onclick();
  assert.equal(state.timelineMonth.getMonth(), 1);
  assert.equal(walk(root.nodes, (n) => n.attrs?.class === 'timeline-empty').length, 1);
  state.dashboard = true;
  render();
  assert.equal(root.hidden, true);
});

test('calendar and Gantt controls/links are wired to the task views', () => {
  const html = fs.readFileSync(path.join(__dirname, '../src/index.html'), 'utf8');
  for (const id of ['view-calendar', 'view-gantt', 'timeline-view', 'p-start', 'p-start-clear']) {
    assert.match(html, new RegExp(`id="${id}"`));
  }
  assert.match(source, /renderTimeline\(\);/);
  assert.match(source, /state\.view !== "board" \? "&done=all"/);
  assert.match(source, /href: `#\$\{task\.project\}\/\$\{task\.id\}`/);
});
