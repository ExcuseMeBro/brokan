// MCP dashboard charts are rendered from bounded aggregate data; no chart library or browser needed.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
function body(name, next) {
  const start = source.indexOf(`function ${name}(`);
  const end = source.indexOf(`\nfunction ${next}(`, start);
  assert.ok(start >= 0 && end > start, `${name} source exists`);
  const part = source.slice(start, end);
  return part.slice(0, part.lastIndexOf('\n}') + 2);
}
const el = (tag, attrs = {}, ...children) => ({ tag, attrs,
  children: children.flat().filter((v) => v !== null && v !== undefined && v !== false) });
const walk = (node, match) => (!node || typeof node !== 'object') ? []
  : [...(match(node) ? [node] : []), ...(node.children || []).flatMap((child) => walk(child, match))];
const mcpLatency = vm.runInNewContext(`(${body('mcpLatency', 'mcpDayChart')})`);
const mcpDayChart = vm.runInNewContext(`(${body('mcpDayChart', 'renderDashboard')})`, { el, mcpLatency });

const days = Array.from({ length: 30 }, (_, n) => ({
  day: `2026-10-${String(n + 1).padStart(2, '0')}`,
  calls: n === 29 ? 2 : 0, fastest_ms: n === 29 ? 18 : null, slowest_ms: n === 29 ? 1350 : null,
}));

test('MCP latency formatting handles zero history and ms/seconds', () => {
  assert.equal(mcpLatency(null), '—');
  assert.equal(mcpLatency(18), '18 ms');
  assert.equal(mcpLatency(1350), '1.4 s');
});

test('30-day MCP charts include every UTC day with accessible values and scroll focus', () => {
  const counts = mcpDayChart(days, false);
  const latency = mcpDayChart(days, true);
  assert.equal(counts.attrs.role, 'region');
  assert.equal(counts.attrs.tabindex, '0');
  assert.equal(walk(counts, (n) => n.attrs?.role === 'img').length, 30);
  assert.equal(walk(latency, (n) => n.attrs?.role === 'img').length, 30);
  assert.equal(walk(counts, (n) => n.attrs?.class === 'mcp-bar calls').length, 30);
  assert.equal(walk(latency, (n) => n.attrs?.class === 'mcp-bar fast').length, 30);
  assert.equal(walk(latency, (n) => n.attrs?.class === 'mcp-bar slow').length, 30);
  assert.equal(walk(latency, (n) => n.attrs?.role === 'img').at(-1).attrs['aria-label'],
    '2026-10-30: fastest 18 ms, slowest 1.4 s');
  assert.equal(walk(latency, (n) => n.attrs?.role === 'img')[0].attrs['aria-label'], '2026-10-01: no calls');
  assert.equal(walk(counts, (n) => n.attrs?.role === 'img').at(-1).attrs['aria-label'], '2026-10-30: 2 calls');
});

test('dashboard uses donut, not solid pie, for task status and renders both MCP charts', () => {
  const calls = [];
  const root = { replaceChildren(...children) { this.children = children; } };
  const state = { dashboard: true, board: { statuses: ['todo', 'doing', 'review', 'done'] },
    dashboardData: { statuses: [{ status: 'todo', count: 2 }], projects: [], days: [], tasks: 2,
      total_seconds: 0, running_seconds: 0, deadlines: [],
      mcp: { days, total_calls: 2, average_per_hour: .003, fastest_ms: 18, slowest_ms: 1350 } } };
  const renderDashboard = vm.runInNewContext(`(${body('renderDashboard', 'toggleProjectMenu')})`, {
    state, $: () => root, el, duration: () => '0m', STATUS_COLORS: ['blue'],
    projectColor: () => 'blue', deadlineDaysLeft: () => 0, deadlineCountdown: () => '0 days',
    ringChart: (_rows, donut, _label, _total, centerText) => {
      calls.push({ donut, centerText }); return el('svg');
    }, mcpLatency, mcpDayChart,
  });
  renderDashboard();
  assert.deepEqual(calls, [{ donut: 'tasks', centerText: '2' }]);
  const charts = walk(root, (n) => n.attrs?.class === 'mcp-scroll');
  assert.equal(charts.length, 2);
  assert.equal(walk(root, (n) => n.tag === 'strong').some((n) => n.children.includes('<0.01')), true);
  assert.equal(walk(root, (n) => n.tag === 'h2').filter((n) => n.children.includes('Tool calls by UTC day')).length, 1);
});

test('single-slice donut still has a center hole and numeric label', () => {
  const svgNode = (tag, attrs = {}) => ({ tag, attrs, children: [], append(child) { this.children.push(child); } });
  const ringChart = vm.runInNewContext(`(${body('ringChart', 'mcpLatency')})`, { svgNode, svgNS: 'http://www.w3.org/2000/svg' });
  const svg = ringChart([{ value: 2, color: 'blue' }], 'tasks', 'todo: 2 tasks', 2, '2');
  assert.equal(svg.children[0].tag, 'circle');
  assert.equal(svg.children[1].attrs.fill, 'var(--col)');
  assert.equal(svg.children[2].textContent, '2');
});
