// Dependency-free checks of the browser's table sort and render behavior.
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
const sortTableTasks = vm.runInNewContext(`(${body('sortTableTasks', 'renderTable')})`);
const statuses = ['backlog', 'todo', 'doing', 'done'];
const projects = [{ slug: 'z', name: 'Alpha' }, { slug: 'a', name: 'Zeta' }];
const tasks = [
  { id: 1, project: 'a', title: 'Beta', status: 'todo', priority: 'low', deadline_at: null, updated_at: '2026-01-01' },
  { id: 2, project: 'z', title: 'Alpha', status: 'todo', priority: 'urgent', deadline_at: '2026-05-01T08:00:00Z', updated_at: '2026-01-03' },
  { id: 3, project: 'a', title: 'gamma', status: 'done', priority: 'high', deadline_at: '2026-04-01T08:00:00Z', updated_at: '2026-01-02' },
  { id: 4, project: 'z', title: 'delta', status: 'todo', priority: 'urgent', deadline_at: null, updated_at: '2026-01-03' },
];
const ids = (rows) => Array.from(rows, (row) => row.id).join(',');
const sorted = (key, direction) => sortTableTasks(tasks, { key, direction }, statuses, projects);

test('table sorting preserves board order on ties and never mutates tasks', () => {
  assert.equal(ids(sorted('status', 'asc')), '1,2,4,3');
  assert.equal(ids(sorted('priority', 'asc')), '2,4,3,1');
  assert.equal(ids(sorted('updated', 'desc')), '2,4,3,1');
  assert.equal(ids(tasks), '1,2,3,4');
});

test('deadline keeps undated tasks last in both directions; text and projects sort naturally', () => {
  assert.equal(ids(sorted('deadline', 'asc')), '3,2,1,4');
  assert.equal(ids(sorted('deadline', 'desc')), '2,3,1,4');
  assert.equal(ids(sorted('title', 'asc')), '2,1,4,3');
  assert.equal(ids(sorted('project', 'asc')), '2,4,1,3');
});

test('table renders semantic sortable headers, all supplied rows and task links', () => {
  const root = { hidden: true, replaceChildren(node) { this.node = node; }, querySelector() { return { focus() {} }; } };
  const state = { dashboard: false, view: 'table', project: 'all', task: null,
    tableSort: { key: 'status', direction: 'asc' }, board: { statuses, projects, tasks } };
  const el = (tag, attrs = {}, ...children) => ({ tag, attrs, children: children.flat().filter((v) => v != null && v !== false) });
  const walk = (node, match) => (!node || typeof node !== 'object') ? []
    : [...(match(node) ? [node] : []), ...(node.children || []).flatMap((child) => walk(child, match))];
  const render = vm.runInNewContext(`(${body('renderTable', 'localDayNumber')})`, {
    state, $, el, sortTableTasks, isHumanOnly: () => false,
    document: { activeElement: null }, showNewProject() {}, openTask: async () => {},
    projectGlyph: () => '', statusIcon: () => '', deadlineOverdue: () => false,
    timeEl: (ts) => ts, key: (task) => `${task.project}#${task.id}`,
  });
  function $(selector) { assert.equal(selector, '#table-view'); return root; }
  render();
  assert.equal(root.hidden, false);
  assert.equal(walk(root.node, (n) => n.tag === 'table').length, 1);
  const headers = walk(root.node, (n) => n.tag === 'th' && n.attrs.scope === 'col');
  assert.equal(headers.length, 6);
  assert.equal(headers[2].attrs['aria-sort'], 'ascending');
  assert.equal(walk(root.node, (n) => n.tag === 'th' && n.attrs.scope === 'row').length, 4);
  const links = walk(root.node, (n) => n.tag === 'a');
  assert.equal(links.map((link) => link.attrs.href).join(','), '#all/1,#all/2,#all/4,#all/3');
  // Clicking a header changes only client-side order, not the board's task array.
  headers[3].children[0].attrs.onclick();
  assert.equal(state.tableSort.key, 'priority');
  assert.equal(walk(root.node, (n) => n.tag === 'a').map((link) => link.attrs.href).join(','), '#all/2,#all/4,#all/3,#all/1');
  assert.equal(ids(tasks), '1,2,3,4');
  state.board.tasks = [];
  render();
  assert.equal(walk(root.node, (n) => n.tag === 'table').length, 0);
});

test('table view shows filtered human tasks, archived projects and hides for dashboard', () => {
  const root = { hidden: true, replaceChildren(node) { this.node = node; },
    querySelector(selector) { return selector === '.table-scroll' ? { scrollTop: 0, scrollLeft: 0 } : null; } };
  const state = { dashboard: false, view: 'table', project: 'z', task: null,
    tableSort: { key: 'status', direction: 'asc' }, board: { statuses, projects: [{ ...projects[0], archived: true }],
      tasks: [{ ...tasks[1], human: true }, { ...tasks[3], human: false }] } };
  const el = (tag, attrs = {}, ...children) => ({ tag, attrs, children: children.flat().filter((v) => v != null && v !== false) });
  const render = vm.runInNewContext(`(${body('renderTable', 'localDayNumber')})`, {
    state, $: () => root, el, sortTableTasks, isHumanOnly: () => true,
    document: { activeElement: null }, showNewProject() {}, openTask: async () => {},
    projectGlyph: () => '', statusIcon: () => '', deadlineOverdue: () => false,
    timeEl: (ts) => ts, key: (task) => `${task.project}#${task.id}`,
  });
  render();
  assert.equal(root.hidden, false);
  assert.equal(root.node.children[0].children[1].children.length, 1); // one tbody row
  state.dashboard = true;
  render();
  assert.equal(root.hidden, true);
});
