const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const root = path.join(__dirname, '..');
const page = fs.readFileSync(path.join(root, 'src/index.html'), 'utf8');
const readme = fs.readFileSync(path.join(root, 'README.md'), 'utf8');

test('the supplied logo is shared by the sidebar, favicon, About and README', () => {
  assert.match(page, /<link rel="icon" href="\/logo\.png">/);
  assert.match(page, /class="brand">\s*<img src="\/logo\.png" alt="">/);
  assert.match(page, /data-section="about">[\s\S]*?<img src="\/logo\.png" alt="bro logo"/);
  assert.match(readme, /<img src="src\/logo\.png" alt="brodev\.uz — bro logo"/);
  assert.ok(fs.statSync(path.join(root, 'src/logo.png')).size > 0);
});

test('About and README link to brodev.uz without changing the local board navigation', () => {
  assert.match(page, /class="about-brand">[\s\S]*?href="https:\/\/brodev\.uz" target="_blank" rel="noopener noreferrer"/);
  assert.match(page, /\.about-brand a:focus-visible \{ outline:/);
  assert.match(readme, /<a href="https:\/\/brodev\.uz">brodev\.uz<\/a>/);
  assert.doesNotMatch(page, /Kanzo/i);
});
