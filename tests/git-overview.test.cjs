const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const path = require('node:path');

const source = fs.readFileSync(path.join(__dirname, '../src/app.js'), 'utf8');
const html = fs.readFileSync(path.join(__dirname, '../src/index.html'), 'utf8');
const code = source.slice(source.indexOf('function gitFlags('), source.indexOf('// ---------- dashboard ----------'));
const el = (tag, attrs = {}, ...children) => ({ tag, attrs, children: children.flat().filter((x) => x !== null && x !== false) });
const walk = (node, test) => node && typeof node === 'object'
  ? [...(test(node) ? [node] : []), ...(node.children || []).flatMap((child) => walk(child, test))] : [];
function setup(data) {
  const root = { hidden: true, replaceChildren(...nodes) { this.nodes = nodes; } };
  const state = { gitOverview: true, gitData: data, gitError: null, gitRepo: 'all', gitFilter: 'all' };
  const { renderGitOverview, gitFlags } = vm.runInNewContext(`${code}\n({ renderGitOverview, gitFlags })`,
    { $, state, el, icon: (name) => el('svg', {}, name), loadGitOverview() {} });
  function $(selector) { assert.equal(selector, '#git-overview'); return root; }
  return { root, state, renderGitOverview, gitFlags };
}
const data = { root: '/tmp/projects', warnings: [], repos: [{ name: 'app', base: 'dev', stash_count: 1,
  worktrees: ['feature — /tmp/projects/app', 'detached HEAD — /tmp/projects/detached'], branches: [
  { name: 'feature', worktree: '/tmp/projects/app', dirty: true, unpushed: true, unmerged: true,
    upstream_unknown: false, status_error: null, upstream: 'origin/feature', commits: ['A commit'], todos: [{ file: 'src/a', line: 3, text: 'TODO fix' }] },
  { name: 'dev', worktree: '/tmp/projects/app', dirty: false, unpushed: false, unmerged: false,
    upstream_unknown: false, status_error: null, upstream: 'origin/dev', commits: [], todos: [] },
] }] };

test('menu and route are separate from board task views', () => {
  assert.match(source, /href: "#~git"/);
  assert.match(source, /slug === "~git"/);
  assert.match(html, /id="git-overview"[^>]+hidden/);
  assert.doesNotMatch(source.slice(source.indexOf('function renderGitOverview()'), source.indexOf('// ---------- dashboard ----------')),
    /git (?:push|merge|branch -[dD])/);
});

test('status filters are explicit; stash is repository-scoped, unknown is not clean', () => {
  const { gitFlags } = setup(data);
  assert.equal(Array.from(gitFlags(data.repos[0].branches[0], data.repos[0])).join(','),
    'Uncommitted,Stash in repo,Unpushed,Unmerged');
  const unknown = { ...data.repos[0].branches[1], worktree: null, upstream_unknown: true, status_error: 'status failed' };
  assert.ok(!gitFlags(unknown, { ...data.repos[0], stash_count: 0 }).includes('Clean'));
});

test('overview renders filterable, keyboard-expandable branches and a setup state', () => {
  const ctx = setup(data);
  ctx.renderGitOverview();
  assert.equal(ctx.root.hidden, false);
  assert.equal(ctx.root.nodes.flatMap((n) => walk(n, (x) => x.attrs?.class === 'git-branch')).length, 2);
  assert.equal(ctx.root.nodes.flatMap((n) => walk(n, (x) => x.attrs?.class === 'git-worktrees')).length, 1);
  const selects = ctx.root.nodes.flatMap((n) => walk(n, (x) => x.tag === 'select'));
  assert.equal(selects.length, 2);
  assert.equal(selects[0].attrs['aria-label'], 'Filter Git repositories');
  ctx.state.gitFilter = 'Unmerged';
  ctx.renderGitOverview();
  assert.equal(ctx.root.nodes.flatMap((n) => walk(n, (x) => x.attrs?.class === 'git-branch')).length, 1);
  ctx.state.gitData = { root: null, warnings: [], repos: [] };
  ctx.renderGitOverview();
  assert.ok(ctx.root.nodes.flatMap((n) => walk(n, (x) => x.tag === 'code')).length);
  ctx.state.gitError = 'No access';
  ctx.renderGitOverview();
  assert.equal(ctx.root.nodes[0].attrs.role, 'alert');
});
