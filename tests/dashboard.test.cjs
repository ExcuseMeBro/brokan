const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
const html = fs.readFileSync(path.join(__dirname, '../src/index.html'), 'utf8');
const code = source.slice(source.indexOf('function showDashboard()'), source.indexOf('// ---------- project menu ----------'));
const el = (tag, attrs = {}, ...children) => ({ tag, attrs,
  children: children.flat().filter((item) => item !== null && item !== false),
  append(...items) { this.children.push(...items); },
});
const walk = (node, predicate) => Array.isArray(node) ? node.flatMap((item) => walk(item, predicate))
  : node && typeof node === 'object'
    ? [...(predicate(node) ? [node] : []), ...(node.children || []).flatMap((item) => walk(item, predicate))] : [];
const git = { root: '/tmp/projects', warnings: [], repos: [{ name: 'site', stash_count: 1, warning: null, branches: [
  { name: 'feature', dirty: true, unpushed: true, unmerged: false, status_error: null },
  { name: 'dev', dirty: false, unpushed: false, unmerged: false, status_error: null },
] }] };
function setup(api = async () => git) {
  const root = { replaceChildren(...nodes) { this.nodes = nodes; } };
  const state = { dashboard: true, dashboardData: { statuses: [{ status: 'todo', count: 2 }], tasks: 2,
    projects: [{ name: 'Site', slug: 'site', seconds: 5 }], days: [{ day: '2026-10-30', seconds: 5 }],
    total_seconds: 5, running_seconds: 0, deadlines: [],
    mcp: { total_calls: 3, average_per_hour: 0.004, fastest_us: 500, average_us: 550000, slowest_us: 1100000,
      days: Array.from({ length: 30 }, (_, i) => ({ day: `2026-10-${String(i + 1).padStart(2, '0')}`,
        calls: i === 28 ? 1 : i === 29 ? 2 : 0,
        fastest_ms: i >= 28 ? 1 : null, slowest_ms: i === 29 ? 1100 : i === 28 ? 500 : null })) } },
    dashboardGitData: git, dashboardGitError: null, board: { statuses: ['todo', 'done'] } };
  const context = { state, el, document: { createElementNS: (_ns, tag) => ({ ...el(tag),
    setAttribute(name, value) { this.attrs[name] = value; } }) }, api, STATUS_COLORS: ['red', 'blue'],
    projectColor: () => 'green', duration: (n) => `${n}s`, deadlineDaysLeft: () => 1,
    deadlineCountdown: () => '', $: (selector) => { assert.equal(selector, '#dashboard'); return root; } };
  const methods = vm.runInNewContext(`${code}\n({ renderDashboard, dashboardGit, loadDashboardGit, mcpDuration })`, context);
  return { ...methods, state, root };
}

test('status is a donut; 30-day MCP counts and min/mean/max render without raw calls', () => {
  const ctx = setup();
  ctx.renderDashboard();
  const nodes = ctx.root.nodes;
  const status = walk(nodes, (x) => x.tag === 'section' && x.children.some((y) => y?.tag === 'h2' && y.children.includes('Tasks by status')))[0];
  const svg = walk(status, (x) => x.tag === 'svg')[0];
  assert.ok(walk(svg, (x) => x.tag === 'circle' && x.attrs.r === '43').length);
  assert.equal(walk(svg, (x) => x.tag === 'text')[0].textContent, '2');
  const charts = walk(nodes, (x) => x.attrs?.class === 'mcp-scroll');
  assert.equal(charts.length, 2);
  assert.equal(walk(charts[0], (x) => x.attrs?.class === 'mcp-day').length, 30);
  assert.equal(walk(charts[1], (x) => x.attrs?.class === 'mcp-day').length, 30);
  assert.match(charts[0].attrs['aria-label'], /last 30 UTC days/);
  assert.match(walk(charts[0], (x) => x.attrs?.role === 'img').at(-1).attrs['aria-label'], /2026-10-30: 2 calls/);
  const timing = walk(nodes, (x) => x.attrs?.class === 'mcp-latencies')[0];
  assert.ok(timing);
  assert.deepEqual(walk(timing, (x) => x.tag === 'dd').map((x) => x.children[0]), ['500 µs', '550.0 ms', '1.10 s']);
  assert.match(html, /dashboard-git-branches/);
});

test('dashboard Git summary includes flagged branches, stash count and a link to full overview', () => {
  const ctx = setup();
  const nodes = ctx.dashboardGit();
  assert.match(JSON.stringify(nodes), /feature/);
  assert.match(JSON.stringify(nodes), /uncommitted · unpushed/);
  assert.match(JSON.stringify(nodes), /1 stashed/);
  assert.equal(walk(nodes, (x) => x.attrs?.href === '#~git').length, 2);
  ctx.state.dashboardGitData = { root: null, repos: [], warnings: [] };
  assert.match(JSON.stringify(ctx.dashboardGit()), /No local Git folder configured/);
});

test('independent Git request discards stale results and surfaces errors', async () => {
  const pending = [];
  const ctx = setup(() => new Promise((resolve, reject) => pending.push({ resolve, reject })));
  const first = ctx.loadDashboardGit();
  const second = ctx.loadDashboardGit();
  pending[1].resolve(git);
  await second;
  pending[0].resolve({ root: null, warnings: [], repos: [] });
  await first;
  assert.equal(ctx.state.dashboardGitData, git);
  const third = ctx.loadDashboardGit();
  pending[2].reject(new Error('Offline'));
  await third;
  assert.match(JSON.stringify(ctx.dashboardGit()), /Git overview unavailable: Offline/);
});

test('no MCP samples do not invent historical calls or zero latency', () => {
  const ctx = setup();
  ctx.state.dashboardData.mcp = { days: [], total_calls: 0, fastest_us: null, average_us: null, slowest_us: null };
  ctx.renderDashboard();
  assert.match(JSON.stringify(ctx.root.nodes), /No MCP tool calls recorded yet/);
  assert.match(JSON.stringify(ctx.root.nodes), /Waiting for the first measured MCP tool call/);
  assert.equal(ctx.mcpDuration(null), '—');
});
