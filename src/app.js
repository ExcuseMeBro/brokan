// The Brokan board. Plain JavaScript, no build step; served at /app.js.
"use strict";

// The board runs on browsers back to Safari 13.1, Chrome 80 and Firefox 78. These
// two are newer than that, and are all it needs on top.
if (!Element.prototype.replaceChildren) {
  Element.prototype.replaceChildren = function (...nodes) {
    this.textContent = "";
    this.append(...nodes);
  };
}
if (!HTMLFormElement.prototype.requestSubmit) {
  HTMLFormElement.prototype.requestSubmit = function () {
    if (this.reportValidity()) this.dispatchEvent(new Event("submit", { cancelable: true }));
  };
}

const $ = (sel, root = document) => root.querySelector(sel);
const STATUS_COLORS = ["#8a8a83", "#3b82f6", "#e8a317", "#2f9e44", "#ae3ec9", "#e8590c"];

const state = {
  project: "all",        // slug or "all"
  dashboard: false,      // #~dashboard is reserved (not a valid project slug)
  dashboardData: null,
  board: null,           // last /api/board reply
  changes: null,         // change counter seen last
  task: null,            // open task id
  dragging: null,        // id of the card being dragged
  dropped: false,        // the current drag ended in a drop
  layout: "",            // key of the rendered column layout
  view: ["board", "table", "calendar", "gantt"].includes(localStorageGet("brokan.view"))
    ? localStorageGet("brokan.view") : "board",
  timelineMonth: new Date(new Date().getFullYear(), new Date().getMonth(), 1),
  tableSort: { key: "status", direction: "asc" },
  allDone: false,        // show every card in the done column
  menu: null,            // slug of the project whose "⋯" menu is open, or "settings"
};

// ---------- helpers ----------

async function api(method, path, body) {
  const res = await fetch("/api" + path, {
    method,
    headers: body ? { "Content-Type": "application/json" } : {},
    body: body ? JSON.stringify(body) : undefined,
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || res.statusText);
  return data;
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null || v === false) continue;
    if (k.startsWith("on")) node.addEventListener(k.slice(2), v);
    else if (k === "class") node.className = v;
    else node.setAttribute(k, v === true ? "" : v);
  }
  node.append(...children.flat().filter((c) => c != null && c !== false));
  return node;
}

// Line icons (from Lucide, ISC licence), drawn in the text colour.
const ICONS = {
  newProject: '<path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.4 2.6a2.1 2.1 0 0 1 3 3L12 15l-4 1 1-4Z"/>',
  copy: '<rect x="8" y="8" width="13" height="13" rx="2"/><path d="M16 8V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h3"/>',
  board: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M8 7v7M12 7v4M16 7v9"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  code: '<path d="m8 17-5-5 5-5m8 0 5 5-5 5"/>',
  terminal: '<rect x="2" y="3" width="20" height="18" rx="2"/><path d="m7 9 3 3-3 3m6 0h4"/>',
  globe: '<circle cx="12" cy="12" r="10"/><path d="M2 12h20M12 2a15 15 0 0 1 0 20M12 2a15 15 0 0 0 0 20"/>',
  database: '<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5M3 12a9 3 0 0 0 18 0"/>',
  server: '<rect x="2" y="3" width="20" height="8" rx="2"/><rect x="2" y="13" width="20" height="8" rx="2"/><path d="M6 7h.01M6 17h.01"/>',
  rocket: '<path d="M4.5 16.5c-1.5 1.2-2 5-2 5s3.8-.5 5-2M12 15l-3 3-3-3 3-3M9 12c.3-4.5 4-9 12-10-1 8-5.5 11.7-10 12"/><circle cx="16" cy="8" r="1"/>',
  book: '<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2Z"/>',
  briefcase: '<rect x="2" y="7" width="20" height="14" rx="2"/><path d="M7 7V5a2 2 0 0 1 2-2h6a2 2 0 0 1 2 2v2M2 12a20 20 0 0 0 20 0"/>',
  bug: '<path d="M8 2l1.9 1.9M16 2l-1.9 1.9M3 13h3m12 0h3M4 19l3-3m10 0 3 3M12 6a6 6 0 0 0-6 6v4a6 6 0 0 0 12 0v-4a6 6 0 0 0-6-6Z"/><path d="M6 13h12"/>',
  palette: '<path d="M12 2a10 10 0 1 0 0 20h1.2a2 2 0 0 0 1.5-3.3 2 2 0 0 1 1.5-3.3h1.3A4.5 4.5 0 0 0 22 11 9 9 0 0 0 12 2Z"/><path d="M7 11h.01M10 6h.01M16 7h.01M18 12h.01"/>',
  zap: '<path d="M13 2 3 14h8l-1 8 11-12h-8l1-8Z"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
  close: '<path d="M18 6 6 18M6 6l12 12"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  plus: '<path d="M5 12h14M12 5v14"/>',
  trash: '<path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6"/>',
  archive: '<rect x="3" y="4" width="18" height="4" rx="1"/><path d="M5 8v12h14V8M10 12h4"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  human: '<circle cx="12" cy="8" r="4"/><path d="M5 21v-1a6 6 0 0 1 6-6h2a6 6 0 0 1 6 6v1"/>',
  sync: '<path d="M12 13v8"/><path d="M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 2.5 8.2"/><path d="m8 17 4-4 4 4"/>',
  collapse: '<path d="m13 17 5-5-5-5M6 17l5-5-5-5"/>',
  expand: '<path d="m11 17-5-5 5-5M18 17l-5-5 5-5"/>',
  rename: '<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
  update: '<circle cx="12" cy="12" r="9"/><path d="M12 7v9M8 12l4 4 4-4"/>',
  chart: '<path d="M3 3v18h18M7 16v-5M12 16V7M17 16v-9"/>',
  settings: '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>',
  send: '<path d="M12 19V5M5 12l7-7 7 7"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 16v-4M12 8h.01"/>',
};

function icon(name, color) {
  const span = el("span", { class: "icon", style: color ? `color:${color}` : null });
  span.innerHTML = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`;
  return span;
}

document.querySelectorAll("[data-icon]").forEach((n) => n.replaceWith(icon(n.dataset.icon)));

let toastTimer;
// Copies a task's key, like api#12, to paste to an agent. The clipboard API needs a
// secure page; a board opened from another device over plain HTTP falls back.
async function copyKey(text) {
  let copied = false;
  try {
    await navigator.clipboard.writeText(text);
    copied = true;
  } catch {
    const focused = document.activeElement;
    const area = el("textarea", { style: "position:fixed;opacity:0" });
    area.value = text;
    document.body.append(area);
    try { area.select(); copied = document.execCommand("copy"); }
    catch { /* Clipboard permission can be denied on both paths. */ }
    finally {
      area.remove();
      if (focused?.isConnected) focused.focus({ preventScroll: true });
    }
  }
  toast(copied ? `Copied ${text}` : `Could not copy ${text}`);
  return copied;
}

function toast(message) {
  document.querySelector(".toast")?.remove();
  const t = el("div", { class: "toast", role: "status" }, message);
  document.body.append(t);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.remove(), 3500);
}

// The colours a project can be given; the names match PROJECT_COLORS in store.rs.
const PROJECT_COLORS = {
  gray: "#8e8e93", red: "#ef4444", orange: "#f97316", yellow: "#eab308", green: "#22c55e",
  teal: "#14b8a6", blue: "#3b82f6", purple: "#a855f7", pink: "#ec4899",
};

// The colour picked for a project, or until then one chosen from its slug.
function projectColorName(slug) {
  const picked = state.board?.projects.find((p) => p.slug === slug)?.color;
  if (PROJECT_COLORS[picked]) return picked;
  const names = Object.keys(PROJECT_COLORS).filter((n) => n !== "gray");
  let h = 0;
  for (const c of slug) h = (h * 31 + c.charCodeAt(0)) % 997;
  return names[h % names.length];
}

function projectColor(slug) {
  return PROJECT_COLORS[projectColorName(slug)];
}

// A missing icon means a folder for boards created before icon selection existed.
const PROJECT_ICONS = ["folder", "code", "terminal", "globe", "database", "server",
  "rocket", "book", "briefcase", "bug", "palette", "zap"];
function projectIconName(slug) {
  const picked = state.board?.projects.find((p) => p.slug === slug)?.icon;
  return PROJECT_ICONS.includes(picked) ? picked : "folder";
}
function projectGlyph(slug) {
  return icon(projectIconName(slug), projectColor(slug));
}

// How many columns a status is past todo: -1 for backlog, and 0 for a status the board
// doesn't know.
function statusStep(status) {
  const { statuses } = state.board;
  const i = statuses.indexOf(status);
  return i < 0 ? 0 : i - statuses.indexOf("todo");
}

function statusColor(status) {
  const i = state.board ? statusStep(status) : 0;
  return STATUS_COLORS[Math.max(i, 0) % STATUS_COLORS.length];
}

// A column's status as a small progress circle, as in Linear: dashed for backlog, empty
// for todo, filling up across the board, a check for done. A shape, so it can't be taken
// for a project's colour dot.
function statusIcon(status) {
  const step = statusStep(status), steps = statusStep("done");
  const f = Math.max(step, 0) / steps;
  const color = statusColor(status);
  const dash = step < 0 ? ' stroke-dasharray="2.36 2.36"' : "";
  let inside = "";
  if (f >= 1) {
    inside = `<circle cx="7" cy="7" r="6" fill="${color}"/>` +
      '<path d="m4.4 7.2 1.8 1.8 3.4-3.6" fill="none" stroke="var(--bg)" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>';
  } else if (f > 0) {
    const r = 3.4, a = f * 2 * Math.PI;
    const x = (7 + r * Math.sin(a)).toFixed(2), y = (7 - r * Math.cos(a)).toFixed(2);
    inside = `<path d="M7 7L7 ${7 - r}A${r} ${r} 0 ${f > 0.5 ? 1 : 0} 1 ${x} ${y}Z" fill="${color}"/>`;
  }
  const span = el("span", { class: "status-icon" });
  span.innerHTML = `<svg viewBox="0 0 14 14" aria-hidden="true"><circle cx="7" cy="7" r="6" fill="none" stroke="${color}" stroke-width="1.5"${dash}/>${inside}</svg>`;
  return span;
}

const key = (t) => `${t.project}#${t.id}`;

function parseTime(ts) {
  return new Date(ts.replace(" ", "T"));
}

function ago(ts) {
  const s = (Date.now() - parseTime(ts)) / 1000;
  if (s < 60) return "just now";
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  if (s < 86400 * 7) return `${Math.floor(s / 86400)}d ago`;
  return parseTime(ts).toLocaleDateString();
}

function timeEl(ts) {
  return el("time", { datetime: ts, "data-relative": true, title: parseTime(ts).toLocaleString() }, ago(ts));
}

// The picker uses the user's local time, while the API stores a UTC instant.
function localDeadline(ts) {
  const d = new Date(ts);
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function deadlineOverdue(t) {
  return !!t.deadline_at && t.status !== "done" && new Date(t.deadline_at).getTime() < Date.now();
}

function deadlineBadge(t) {
  if (!t.deadline_at) return null;
  const date = new Date(t.deadline_at);
  const overdue = deadlineOverdue(t);
  const label = overdue ? "Overdue" : "Deadline";
  return el("span", { class: `deadline-badge${overdue ? " overdue" : ""}`, "data-done": t.status === "done" ? "true" : "false" },
    `${label} · `, el("time", { datetime: t.deadline_at, title: date.toLocaleString() },
      date.toLocaleString([], { dateStyle: "medium", timeStyle: "short" })));
}

// Compare local calendar dates (not 24-hour intervals): DST and UTC offsets must
// not turn a deadline tomorrow into "today" or "two days away".
function deadlineDaysLeft(iso, now = new Date()) {
  const due = new Date(iso);
  return Math.round((Date.UTC(due.getFullYear(), due.getMonth(), due.getDate()) -
    Date.UTC(now.getFullYear(), now.getMonth(), now.getDate())) / 86400000);
}

function deadlineCountdown(iso, now = new Date()) {
  const days = deadlineDaysLeft(iso, now);
  if (days < 0) return `${-days} day${days === -1 ? "" : "s"} overdue`;
  if (days === 0) return new Date(iso) < now ? "Overdue today" : "Today · 0 days left";
  return `${days} day${days === 1 ? "" : "s"} left`;
}

function duration(seconds) {
  const s = Math.max(0, Math.floor(seconds));
  if (s < 60) return `${s}s`;
  const h = Math.floor(s / 3600), m = Math.floor(s % 3600 / 60);
  return h ? `${h}h${m ? ` ${m}m` : ""}` : `${m}m`;
}

// ---------- Markdown (small, escapes everything first) ----------

function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

// Code spans, links and bare URLs are swapped for placeholders as soon as they are
// rendered, so later rules never see, or rewrite, the HTML they produced. The input
// is already HTML-escaped, so it holds no tags or raw quotes of its own.
function inline(s) {
  const saved = [];
  const keep = (html) => `\u0000${saved.push(html) - 1}\u0000`;
  const link = (url, text) => keep(`<a href="${url}" target="_blank" rel="noopener">${text}</a>`);
  s = s
    .replace(/\u0000/g, "")
    .replace(/`([^`]+)`/g, (_, code) => keep(`<code>${code}</code>`))
    .replace(/\[([^\]]+)\]\(((?:https?:\/\/|mailto:)[^\s)\u0000]+)\)/g, (_, text, url) => link(url, emphasis(text)))
    .replace(/(^|[\s(])(https?:\/\/[^\s)\u0000]+)/g, (_, before, url) => before + link(url, url));
  const restore = (html) => html.replace(/\u0000(\d+)\u0000/g, (_, i) => restore(saved[i]));
  return restore(emphasis(s));
}

function emphasis(s) {
  return s
    .replace(/\*\*(.+?)\*\*/g, "<strong>$1</strong>")
    .replace(/(^|[^\w*])\*(?!\s)(.+?)\*(?!\w)/g, "$1<em>$2</em>")
    .replace(/(^|[^\w])_(?!\s)(.+?)_(?!\w)/g, "$1<em>$2</em>")
    .replace(/~~(.+?)~~/g, "<del>$1</del>");
}

function markdown(src) {
  const lines = escapeHtml(src.replace(/\r\n?/g, "\n")).split("\n");
  const out = [];
  let para = [];
  let list = null; // { type: "ul" | "ol", items: [] }
  const flushPara = () => { if (para.length) out.push(`<p>${inline(para.join("<br>"))}</p>`); para = []; };
  const flushList = () => {
    if (!list) return;
    out.push(`<${list.type}>${list.items.join("")}</${list.type}>`);
    list = null;
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    let m;
    if ((m = line.match(/^\s*```/))) {
      flushPara(); flushList();
      const code = [];
      while (++i < lines.length && !/^\s*```/.test(lines[i])) code.push(lines[i]);
      out.push(`<pre><code>${code.join("\n")}</code></pre>`);
    } else if ((m = line.match(/^(#{1,6})\s+(.*)$/))) {
      flushPara(); flushList();
      const n = Math.min(m[1].length, 4);
      out.push(`<h${n}>${inline(m[2])}</h${n}>`);
    } else if ((m = line.match(/^\s*(?:[-*+]|(\d+)[.)])\s+(.*)$/))) {
      flushPara();
      const type = m[1] ? "ol" : "ul";
      if (list && list.type !== type) flushList();
      list = list || { type, items: [] };
      const check = m[2].match(/^\[( |x|X)\]\s+(.*)$/);
      list.items.push(check
        ? `<li class="check"><input type="checkbox" disabled ${check[1] === " " ? "" : "checked"}> ${inline(check[2])}</li>`
        : `<li>${inline(m[2])}</li>`);
    } else if ((m = line.match(/^&gt;\s?(.*)$/))) {
      flushPara(); flushList();
      const quote = [m[1]];
      while (i + 1 < lines.length && (m = lines[i + 1].match(/^&gt;\s?(.*)$/))) { quote.push(m[1]); i++; }
      out.push(`<blockquote>${inline(quote.join("<br>"))}</blockquote>`);
    } else if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      flushPara(); flushList();
      out.push("<hr>");
    } else if (line.trim() === "") {
      flushPara(); flushList();
    } else if (list && /^\s{2,}\S/.test(line)) {
      list.items[list.items.length - 1] = list.items[list.items.length - 1].replace(/<\/li>$/, ` ${inline(line.trim())}</li>`);
    } else {
      flushList();
      para.push(line);
    }
  }
  flushPara(); flushList();
  return out.join("");
}

// ---------- board ----------

// Loads can overlap (a poll, a drop, an edit); only the newest one may render,
// so a slow older reply never undoes what is on screen.
let boardRequest = 0;

async function loadBoard() {
  const request = ++boardRequest;
  try {
    // The server leaves out older done tasks unless asked: the board does not show
    // them, and the done column only grows. Search needs them all.
    const done = state.allDone || state.view !== "board" ? "&done=all" : "";
    const board = await api("GET", "/board?project=" + encodeURIComponent(state.project) + done);
    if (request !== boardRequest) return;
    state.board = board;
    state.changes = board.changes;
    $("#offline").hidden = true;
    render();
  } catch (e) {
    if (request !== boardRequest) return;
    if (state.project !== "all" && /no project/.test(e.message)) {
      setProject("all");
    } else {
      toast(e.message);
    }
  }
}

// The address is #project or #project/task-id, so a task can be linked to.
function updateHash() {
  const hash = state.dashboard ? "#~dashboard" : "#" + state.project + (state.task ? "/" + state.task : "");
  if (location.hash !== hash) history.replaceState(null, "", hash);
}

function readHash() {
  let hash = location.hash.slice(1);
  try { hash = decodeURIComponent(hash); } catch {}
  const [slug, task] = hash.split("/");
  return { slug: slug || null, task: Number(task) || null };
}

function setProject(slug) {
  state.dashboard = false;
  state.project = slug;
  try { localStorage.setItem("brokan.project", slug); } catch {}
  updateHash();
  state.layout = "";
  loadBoard();
}

function render() {
  renderNav();
  renderHeader();
  renderColumns();
  renderTable();
  renderTimeline();
  renderDashboard();
}

function setView(view) {
  if (!["board", "table", "calendar", "gantt"].includes(view) || view === state.view) return;
  state.view = view;
  try { localStorage.setItem("brokan.view", view); } catch {}
  // The board normally limits the done column; other views need every matching task.
  loadBoard();
}

$("#view-board").addEventListener("click", () => setView("board"));
$("#view-table").addEventListener("click", () => setView("table"));
$("#view-calendar").addEventListener("click", () => setView("calendar"));
$("#view-gantt").addEventListener("click", () => setView("gantt"));

function showDashboard() {
  if (state.task) closeTask();
  state.dashboard = true;
  updateHash();
  if (state.board) render();
  loadDashboard();
}

let dashboardRequest = 0;
async function loadDashboard() {
  if (!state.dashboard) return;
  const request = ++dashboardRequest;
  try {
    const data = await api("GET", "/dashboard");
    if (request !== dashboardRequest || !state.dashboard) return;
    state.dashboardData = data;
    renderDashboard();
  } catch (err) { if (state.dashboard) toast(err.message); }
}

function renderNav() {
  // Redrawing would take the field away from someone renaming a project.
  if (renaming && document.contains(renaming.input)) return;
  const { projects } = state.board;
  const active = projects.filter((p) => !p.archived);
  const archived = projects.filter((p) => p.archived);
  const open = active.reduce((n, p) => n + p.open, 0);
  const link = (slug, name, count, glyph) =>
    el("a", { href: "#" + slug, class: !state.dashboard && state.project === slug ? "active" : null,
              onclick: (e) => { e.preventDefault(); setProject(slug); } },
      glyph,
      el("span", { class: "name" }, name),
      el("span", { class: "count" }, String(count)));
  const item = (p) => renaming && renaming.slug === p.slug
    ? el("div", { class: "item renaming" }, projectGlyph(p.slug), renaming.input)
    : el("div", { class: "item" + (state.menu === p.slug ? " menu-open" : "") },
    link(p.slug, p.name, p.open, projectGlyph(p.slug)),
    el("button", { class: "more", title: "More", "aria-label": `More for ${p.name}`,
                   "aria-expanded": String(state.menu === p.slug),
                   "aria-controls": state.menu === p.slug ? "project-settings-menu" : null,
                   onclick: (e) => toggleProjectMenu(p, e.currentTarget) }, icon("more")));
  const dashboard = el("a", { href: "#~dashboard", class: state.dashboard ? "active" : null,
    onclick: (e) => { e.preventDefault(); showDashboard(); } },
    icon("chart"), el("span", { class: "name" }, "Dashboard"));
  const archivedOpen = localStorageGet("brokan.archivedOpen") !== "0";
  const archivedGroup = archived.length ? el("div", { class: "archive-group" },
    el("button", { type: "button", class: "archive-toggle", "aria-expanded": String(archivedOpen),
      "aria-controls": "archived-projects", onclick: () => {
        try { localStorage.setItem("brokan.archivedOpen", archivedOpen ? "0" : "1"); } catch {}
        renderNav();
        $("#nav .archive-toggle")?.focus();
      } }, icon("archive"), el("span", { class: "name" }, "Archived"),
    el("span", { class: "count" }, String(archived.length))),
    el("div", { id: "archived-projects", class: "archive-items", hidden: !archivedOpen }, ...archived.map(item))) : null;
  $("#nav").replaceChildren(
    dashboard,
    link("all", "All projects", open, icon("board")),
    ...(active.length ? [el("div", { class: "label" }, "Projects")] : []),
    ...active.map(item),
    ...(archivedGroup ? [archivedGroup] : []),
  );
}

// ---------- dashboard ----------

const svgNS = "http://www.w3.org/2000/svg";
function svgNode(tag, attrs = {}) {
  const node = document.createElementNS(svgNS, tag);
  for (const [name, value] of Object.entries(attrs)) node.setAttribute(name, String(value));
  return node;
}

// SVG paths, rather than a chart dependency: works with the existing CSP and
// remains readable as a labelled list when charts are not visible.
function ringChart(rows, donut, label, total) {
  const svg = svgNode("svg", { class: "chart-svg", viewBox: "0 0 160 160", role: "img", "aria-label": label });
  let start = -Math.PI / 2;
  for (const row of rows) {
    if (!row.value) continue;
    const angle = row.value / total * 2 * Math.PI;
    if (angle > 2 * Math.PI - 1e-8) {
      svg.append(svgNode("circle", { cx: 80, cy: 80, r: 68, fill: row.color }));
    } else {
      const end = start + angle;
      const pt = (a) => [80 + 68 * Math.cos(a), 80 + 68 * Math.sin(a)].map((v) => v.toFixed(3));
      const [x1, y1] = pt(start), [x2, y2] = pt(end);
      svg.append(svgNode("path", { d: `M80 80L${x1} ${y1}A68 68 0 ${angle > Math.PI ? 1 : 0} 1 ${x2} ${y2}Z`, fill: row.color }));
      start = end;
    }
  }
  if (donut) {
    svg.append(svgNode("circle", { cx: 80, cy: 80, r: 43, fill: "var(--col)" }));
    const text = svgNode("text", { x: 80, y: 86, "text-anchor": "middle", fill: "var(--text)",
      "font-size": 17, "font-weight": 600 });
    text.textContent = duration(total);
    svg.append(text);
  }
  return svg;
}

function renderDashboard() {
  if (!state.dashboard) return;
  const root = $("#dashboard");
  const data = state.dashboardData;
  if (!data || !state.board) { root.replaceChildren(el("p", {}, "Loading dashboard…")); return; }
  const stat = (label, value, note) => el("div", { class: "dashboard-stat" },
    el("span", { class: "label" }, label), el("strong", {}, value), el("small", {}, note));
  const chart = (eyebrow, title, contents, note, wide) => el("section", { class: "chart-card" + (wide ? " wide" : "") },
    el("span", { class: "eyebrow" }, eyebrow), el("h2", {}, title), contents,
    note ? el("p", { class: "chart-note" }, note) : null);
  const statusRows = state.board.statuses.map((status, i) => ({
    name: status[0].toUpperCase() + status.slice(1),
    value: data.statuses.find((item) => item.status === status)?.count || 0,
    color: STATUS_COLORS[i % STATUS_COLORS.length],
  })).filter((row) => row.value > 0);
  const projectRows = data.projects.filter((p) => p.seconds > 0).map((p) => ({
    name: p.name, value: p.seconds, color: projectColor(p.slug),
  }));
  const legend = (rows, format) => el("ul", { class: "chart-legend" }, ...rows.map((row) =>
    el("li", {}, el("span", { class: "swatch", style: `background:${row.color}` }),
      el("span", { class: "name" }, row.name), el("span", { class: "value" }, format(row.value)))));
  const circular = (rows, donut, value, format, empty) => value
    ? el("div", { class: "chart-content" },
      ringChart(rows, donut, rows.map((r) => `${r.name}: ${format(r.value)}`).join(", "), value), legend(rows, format))
    : el("p", { class: "chart-empty" }, empty);
  const days = data.days || [];
  const max = Math.max(1, ...days.map((d) => d.seconds));
  const bars = el("div", { class: "bar-chart", role: "img", "aria-label": "Time by day: " + days.map((d) =>
    `${d.day}: ${duration(d.seconds)}`).join(", ") }, ...days.map((d) =>
    el("div", { class: "bar-day" }, el("span", { class: "amount" }, d.seconds ? duration(d.seconds) : "0"),
      el("span", { class: "track" }, el("span", { class: "fill", style: `height:${d.seconds / max * 100}%` })),
      el("span", { class: "day", title: d.day }, d.day.slice(5)))));
  const open = data.tasks - (data.statuses.find((s) => s.status === "done")?.count || 0);
  const now = new Date();
  const deadlines = (data.deadlines || []).filter((d) => deadlineDaysLeft(d.deadline_at, now) <= 7);
  const deadlineList = deadlines.length
    ? el("ul", { class: "dashboard-deadlines" }, ...deadlines.map((d) => {
      const remaining = deadlineCountdown(d.deadline_at, now);
      const overdue = new Date(d.deadline_at) < now;
      return el("li", {}, el("a", { class: "dashboard-deadline", href: `#${d.project}/${d.id}`,
        onclick: (e) => { e.preventDefault(); setProject(d.project); openTask(d.id); } },
      el("span", { class: "deadline-info" },
        el("strong", {}, d.title), el("span", { class: "deadline-project" }, d.project_name)),
      el("span", { class: "deadline-date" },
        el("time", { datetime: d.deadline_at }, new Date(d.deadline_at).toLocaleString([], { dateStyle: "medium", timeStyle: "short" })),
        el("span", { class: "deadline-count" + (overdue ? " overdue" : "") }, remaining))));
    }))
    : el("p", { class: "chart-empty" }, "No deadlines due in the next 7 days.");
  root.replaceChildren(
    el("p", { class: "dashboard-intro" }, "A clear view of work and time across every project."),
    el("div", { class: "dashboard-stats" },
      stat("Tracked time", duration(data.total_seconds), data.running_seconds ? `${duration(data.running_seconds)} running now` : "All time"),
      stat("Tasks", String(data.tasks), `${open} open`),
      stat("Projects", String(data.projects.length), `${projectRows.length} with tracked time`)),
    chart("NEXT UP", "Deadlines · next 7 days", deadlineList,
      "Local calendar days. Shows up to 12 nearest upcoming and 5 recently overdue tasks; completed and archived work is excluded.", true),
    el("div", { class: "dashboard-charts" },
      chart("01 / ACTIVITY", "Time logged · last 7 days",
        days.some((d) => d.seconds > 0) ? bars : el("p", { class: "chart-empty" }, "No time logged yet. Start a task timer or log minutes."),
        "Entries are grouped by start date in UTC. Running time is included.", true),
      chart("02 / WORK", "Tasks by status",
        circular(statusRows, false, data.tasks, (v) => `${v} task${v === 1 ? "" : "s"}`, "No tasks yet. Create one to see its status."),
        "All tasks, including older completed work."),
      chart("03 / TIME", "Time by project",
        circular(projectRows, true, data.total_seconds, duration, "No tracked time yet. Start a task timer or log minutes."),
        "All-time logged and currently running time.")));
}

// ---------- project menu ----------

// The "⋯" menu of a sidebar project. `state.menu` holds its slug, so the row keeps
// its highlight while the sidebar is redrawn by polling.
function toggleProjectMenu(p, button) {
  const wasOpen = state.menu === p.slug;
  closeMenu();
  if (wasOpen) return;
  state.menu = p.slug;
  button.setAttribute("aria-expanded", "true");
  button.setAttribute("aria-controls", "project-settings-menu");
  button.closest(".item").classList.add("menu-open");
  const swatches = el("div", { class: "swatches", role: "group", "aria-label": "Color" });
  const drawSwatches = () => swatches.replaceChildren(...Object.entries(PROJECT_COLORS).map(([name, hex]) => {
    const current = projectColorName(p.slug) === name;
    return el("button", { class: "swatch", style: `background:${hex}`, title: name, "aria-label": name,
                          "aria-pressed": String(current), onclick: async () => {
                            const focused = document.activeElement?.getAttribute("aria-label") === name;
                            await pickColor(p.slug, name);
                            drawSwatches(); drawIcons();
                            if (focused) [...swatches.querySelectorAll("button")].find((b) => b.getAttribute("aria-label") === name)?.focus();
                          } }, current ? icon("check") : null);
  }));
  drawSwatches();
  const icons = el("div", { class: "project-icons", role: "group", "aria-label": "Project icon" });
  const drawIcons = () => icons.replaceChildren(...PROJECT_ICONS.map((name) =>
    el("button", { type: "button", class: "project-icon" + (projectIconName(p.slug) === name ? " selected" : ""),
                   title: name, "aria-label": `${name} icon`, "aria-pressed": String(projectIconName(p.slug) === name),
                   onclick: async () => {
                     const focused = document.activeElement?.getAttribute("aria-label") === `${name} icon`;
                     if (await pickIcon(p.slug, name)) {
                       drawIcons();
                       if (focused) [...icons.querySelectorAll("button")].find((b) => b.getAttribute("aria-label") === `${name} icon`)?.focus();
                     }
                   } },
      icon(name, projectColor(p.slug)))));
  drawIcons();
  const rename = el("button", { onclick: () => startRename(p) }, icon("rename"), "Rename");
  const archive = el("button", { onclick: () => archiveProject(p, !p.archived) }, icon("archive"),
    p.archived ? "Restore project" : "Archive project");
  const remove = el("button", { class: "danger", onclick: () => { closeMenu(); deleteProject(p); } },
    icon("trash"), "Delete project");
  const menu = el("div", { id: "project-settings-menu", class: "menu project-menu", role: "group", "aria-label": `Project settings for ${p.name}`, tabindex: "-1" },
    el("div", { class: "label" }, "Color"), swatches,
    el("div", { class: "label" }, "Icon"), icons,
    el("div", { class: "sep" }), rename, archive, remove);
  document.body.append(menu);
  menu.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      closeMenu();
      const trigger = button.isConnected ? button : [...document.querySelectorAll("#nav .more")]
        .find((b) => b.getAttribute("aria-label") === `More for ${p.name}`);
      trigger?.focus();
    }
  });
  const r = button.getBoundingClientRect();
  const below = r.bottom + 4 + menu.offsetHeight <= innerHeight;
  menu.style.top = `${Math.max(8, below ? r.bottom + 4 : r.top - 4 - menu.offsetHeight)}px`;
  menu.style.left = `${Math.max(8, Math.min(r.left, innerWidth - menu.offsetWidth - 8))}px`;
  // Focus the menu itself, so Tab reaches its buttons without one looking selected.
  menu.focus();
}

// Renaming a project, as in ChatGPT: its sidebar row becomes a field. Enter or leaving it
// saves, Esc cancels. The slug, and so the task keys, stay.
let renaming = null; // { slug, input }

function startRename(p) {
  closeMenu();
  const input = el("input", { class: "field", "aria-label": `Rename ${p.name}`, spellcheck: "false", autocomplete: "off" });
  input.value = p.name;
  renaming = { slug: p.slug, input };
  let finished = false;
  const finish = async (save) => {
    if (finished) return;
    finished = true;
    const name = input.value.trim();
    renaming = null;
    if (save && name && name !== p.name) {
      try { await api("PATCH", `/projects/${encodeURIComponent(p.slug)}`, { name }); } catch (err) { toast(err.message); }
    }
    loadBoard();
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { e.preventDefault(); finish(true); }
    else if (e.key === "Escape") { e.stopPropagation(); finish(false); }
  });
  input.addEventListener("blur", () => finish(true));
  renderNav();
  input.focus();
  input.select();
}

async function pickColor(slug, color) {
  try {
    await api("PATCH", `/projects/${encodeURIComponent(slug)}`, { color });
    await loadBoard();
  } catch (err) { toast(err.message); }
}

async function archiveProject(project, archived) {
  closeMenu();
  try {
    await api("PATCH", `/projects/${encodeURIComponent(project.slug)}`, { archived });
    if (archived) {
      try { localStorage.setItem("brokan.archivedOpen", "1"); } catch {}
    }
    await loadBoard();
    toast(`${project.name} ${archived ? "archived" : "restored"}.`);
  } catch (err) { toast(err.message); }
}

async function pickIcon(slug, iconName) {
  try {
    await api("PATCH", `/projects/${encodeURIComponent(slug)}`, { icon: iconName });
    await loadBoard();
    return true;
  } catch (err) { toast(err.message); return false; }
}

function closeMenu() {
  if (!state.menu) return;
  state.menu = null;
  document.querySelector(".menu")?.remove();
  document.querySelectorAll(".nav .menu-open").forEach((i) => {
    i.classList.remove("menu-open");
    i.querySelector(".more")?.setAttribute("aria-expanded", "false");
    i.querySelector(".more")?.removeAttribute("aria-controls");
  });
  $("#settings").classList.remove("open");
}

// A click elsewhere closes the menu; one on a "⋯" or the Settings button is left to toggle it.
document.addEventListener("mousedown", (e) => {
  if (!e.target.closest(".menu, .nav .more, #settings")) closeMenu();
});
// A click outside the task panel closes it. Cards are left to open their own task,
// and the project menu to do its job.
document.addEventListener("mousedown", (e) => {
  if (state.task && e.button === 0 && !e.target.closest("#panel, .card, .menu, .search-dialog")) closeTask();
});
window.addEventListener("resize", closeMenu);
window.addEventListener("blur", closeMenu);
$("#nav").addEventListener("scroll", closeMenu);

function currentProject() {
  return state.board.projects.find((p) => p.slug === state.project) || null;
}

function renderHeader() {
  if (state.dashboard) {
    $("#new-task").hidden = true;
    $("#human-only").hidden = true;
    $("#view-switch").hidden = true;
    $("#header").replaceChildren(el("h1", {}, "Dashboard"), el("span", { class: "sub" }, "Projects · tasks · time"));
    return;
  }
  const p = currentProject();
  const { projects, tasks } = state.board;
  const active = projects.filter((project) => !project.archived);
  const header = $("#header");
  $("#view-switch").hidden = false;
  $("#view-board").setAttribute("aria-pressed", String(state.view === "board"));
  $("#view-table").setAttribute("aria-pressed", String(state.view === "table"));
  $("#view-calendar").setAttribute("aria-pressed", String(state.view === "calendar"));
  $("#view-gantt").setAttribute("aria-pressed", String(state.view === "gantt"));
  $("#new-task").hidden = !active.length || Boolean(p?.archived);
  $("#human-only").hidden = !projects.length;
  $("#human-only").setAttribute("aria-pressed", String(isHumanOnly()));
  if (!projects.length) { header.replaceChildren(); return; }
  // Plain words for how much work is open in the selected project.
  const open = (n) => `${n} open task${n === 1 ? "" : "s"}`;
  if (!p) {
    const count = active.reduce((n, project) => n + project.open, 0);
    header.replaceChildren(el("h1", {}, "All projects"),
      el("span", { class: "sub" }, `${open(count)} in ${active.length} project${active.length === 1 ? "" : "s"}`));
    return;
  }
  header.replaceChildren(
    el("h1", { class: "project-heading" }, projectGlyph(p.slug), p.name),
    el("span", { class: "sub" }, p.archived ? `Archived · ${open(p.open)}` : open(p.open)),
  );
}

async function deleteProject(p) {
  const tasks = p.total ? ` and its ${p.total} task${p.total > 1 ? "s" : ""}, with their comments` : "";
  if (!confirm(`Delete project ${p.slug}${tasks}? This cannot be undone.`)) return;
  try {
    await api("DELETE", `/projects/${encodeURIComponent(p.slug)}`);
    if (panelTask?.project === p.slug) { fresh = null; closeTask(); }
    if (state.project === p.slug) setProject("all"); else loadBoard();
  } catch (err) { toast(err.message); }
}

function renderColumns() {
  const columns = $("#columns");
  $("#dashboard").hidden = !state.dashboard;
  if (state.dashboard || state.view !== "board") { columns.style.display = "none"; return; }
  const { statuses, projects, tasks } = state.board;
  if (!projects.length || (state.project === "all" && projects.every((p) => p.archived))) {
    state.layout = "";
    columns.style.display = "flex";
    columns.replaceChildren(el("div", { class: "empty" },
      el("h1", {}, projects.length ? "No active projects" : "Welcome to Brokan"),
      el("p", {}, projects.length
        ? "Your projects are in Archived. Restore one there, or create a new project."
        : "Create your first project in the sidebar. You can choose it by name when working with an agent."),
      el("button", { class: "btn primary", onclick: showNewProject }, "New project")));
    return;
  }
  columns.style.display = "";

  // Build the columns once per layout so a refresh never wipes a half-typed task.
  const layout = statuses.join(",") + "|" + state.project;
  if (state.layout !== layout) {
    state.layout = layout;
    columns.replaceChildren(...statuses.map((status) => {
      const collapsible = status === "backlog" || status === "done";
      const col = el("section", { class: "column", "data-status": status },
        el("h2", {},
          statusIcon(status), status,
          el("span", { class: "count" }),
          collapsible ? el("button", { class: "collapse", onclick: (e) => { e.stopPropagation(); setCollapsed(status, !isCollapsed(status)); } }) : null),
        el("div", { class: "cards" }),
      );
      wireDrop(col, col.querySelector(".cards"), status);
      if (collapsible) col.addEventListener("click", () => { if (isCollapsed(status)) setCollapsed(status, false); });
      return col;
    }));
    showCollapsed();
  }

  // During a drag the DOM is being moved by hand; refresh once it ends.
  if (state.dragging) return;
  const all = state.project === "all";
  const humanOnly = isHumanOnly();
  for (const status of statuses) {
    const col = columns.querySelector(`[data-status="${CSS.escape(status)}"]`);
    const mine = tasks.filter((t) => t.status === status && (!humanOnly || t.human));
    // Older done tasks the server left out (see loadBoard).
    const last = status === statuses[statuses.length - 1];
    const hidden = last && !state.allDone ? state.board.done_hidden || 0 : 0;
    // Filtered, the server's count of older done tasks says nothing about human ones.
    const count = mine.length + (humanOnly ? 0 : hidden);
    col.querySelector("h2 .count").textContent = count ? String(count) : "";
    const more = hidden
      ? el("button", { class: "ghost more", onclick: () => { state.allDone = true; loadBoard(); } },
           humanOnly ? "Show older" : `Show ${hidden} older`)
      : null;
    col.querySelector(".cards").replaceChildren(...[more].filter(Boolean), ...mine.map((t) => card(t, all)));
  }
}

// The table is a second view of the same board, never a second task order. A stable
// original-index tie break leaves manual kanban order unchanged by column sorting.
function sortTableTasks(tasks, sort, statuses, projects) {
  const priorities = { urgent: 0, high: 1, medium: 2, low: 3, none: 4 };
  const names = new Map(projects.map((p) => [p.slug, p.name]));
  const value = (t) => {
    switch (sort.key) {
      case "title": return t.title;
      case "project": return names.get(t.project) || t.project;
      case "status": { const i = statuses.indexOf(t.status); return i < 0 ? statuses.length : i; }
      case "priority": return priorities[t.priority] ?? priorities.none;
      case "deadline": return t.deadline_at || null;
      case "updated": return t.updated_at;
      default: return null;
    }
  };
  return tasks.map((task, index) => ({ task, index })).sort((a, b) => {
    const left = value(a.task), right = value(b.task);
    // Unscheduled work stays last in either deadline direction.
    if (left == null || right == null) return (left == null ? 1 : 0) - (right == null ? 1 : 0) || a.index - b.index;
    const compared = typeof left === "number" ? left - right : String(left).localeCompare(String(right), undefined,
      { sensitivity: "base", numeric: true });
    return compared * (sort.direction === "asc" ? 1 : -1) || a.index - b.index;
  }).map((row) => row.task);
}

function renderTable() {
  const root = $("#table-view");
  root.hidden = state.dashboard || state.view !== "table";
  if (root.hidden || !state.board) return;
  const { projects, tasks, statuses } = state.board;
  if (!projects.length || (state.project === "all" && projects.every((p) => p.archived))) {
    root.replaceChildren(el("div", { class: "empty" }, el("h2", {}, projects.length ? "No active projects" : "Welcome to Brokan"),
      el("p", {}, projects.length ? "Restore a project from Archived, or create a new one."
        : "Create your first project in the sidebar to add tasks."),
      el("button", { class: "btn primary", onclick: showNewProject }, "New project")));
    return;
  }
  const visible = tasks.filter((t) => !isHumanOnly() || t.human);
  const sorted = sortTableTasks(visible, state.tableSort, statuses, projects);
  if (!sorted.length) {
    root.replaceChildren(el("div", { class: "empty" }, el("h2", {}, isHumanOnly() ? "No human tasks" : "No tasks yet"),
      el("p", {}, isHumanOnly() ? "Turn off the Human filter to see all tasks." : "Use New task to add one here.")));
    return;
  }
  const focusedSort = document.activeElement?.closest("#table-view th button")?.dataset.sort;
  const focusedTask = document.activeElement?.closest("#table-view a[data-task-id]")?.dataset.taskId;
  const previousScroll = root.querySelector(".table-scroll");
  const scrollTop = previousScroll?.scrollTop || 0;
  const scrollLeft = previousScroll?.scrollLeft || 0;
  const columns = [["title", "Task"], ["project", "Project"], ["status", "Status"],
    ["priority", "Priority"], ["deadline", "Deadline"], ["updated", "Updated"]];
  const header = el("tr", {}, ...columns.map(([name, label]) => {
    const selected = state.tableSort.key === name;
    return el("th", { scope: "col", "aria-sort": selected ? (state.tableSort.direction === "asc" ? "ascending" : "descending") : "none" },
      el("button", { type: "button", "data-sort": name, "aria-label": `Sort by ${label.toLowerCase()}`,
        onclick: () => {
          state.tableSort = { key: name, direction: selected && state.tableSort.direction === "asc" ? "desc"
            : selected ? "asc" : name === "updated" ? "desc" : "asc" };
          renderTable();
          root.querySelector(`th button[data-sort="${name}"]`)?.focus();
        } }, label, selected ? el("span", { class: "sort-arrow", "aria-hidden": "true" },
          state.tableSort.direction === "asc" ? "↑" : "↓") : null));
  }));
  const projectNames = new Map(projects.map((p) => [p.slug, p.name]));
  const rows = sorted.map((t) => {
    const deadline = t.deadline_at && new Date(t.deadline_at);
    return el("tr", { class: state.task === t.id ? "selected" : null },
      el("th", { scope: "row" }, el("a", { href: `#${state.project}/${t.id}`, "data-task-id": t.id,
        onclick: (e) => { e.preventDefault(); openTask(t.id).then(() => {
          if (state.task === t.id) $("#p-title").focus({ preventScroll: true });
        }); } }, t.title),
      el("span", { class: "task-key mono" }, key(t), t.human ? " · Human" : "")),
      el("td", {}, projectGlyph(t.project), projectNames.get(t.project) || t.project),
      el("td", {}, statusIcon(t.status), t.status),
      el("td", {}, t.priority && t.priority !== "none" ? t.priority[0].toUpperCase() + t.priority.slice(1) : "—"),
      el("td", { class: deadlineOverdue(t) ? "overdue" : null }, deadline
        ? el("time", { datetime: t.deadline_at, title: deadline.toLocaleString() },
          `${deadlineOverdue(t) ? "Overdue · " : ""}${deadline.toLocaleString([], { dateStyle: "medium", timeStyle: "short" })}`) : "—"),
      el("td", {}, timeEl(t.updated_at)));
  });
  root.replaceChildren(el("div", { class: "table-scroll" }, el("table", { class: "task-table" },
    el("thead", {}, header), el("tbody", {}, ...rows))));
  const nextScroll = root.querySelector(".table-scroll");
  nextScroll.scrollTop = scrollTop;
  nextScroll.scrollLeft = scrollLeft;
  if (focusedSort) root.querySelector(`th button[data-sort="${focusedSort}"]`)?.focus({ preventScroll: true });
  if (focusedTask) root.querySelector(`a[data-task-id="${focusedTask}"]`)?.focus({ preventScroll: true });
}

// Display calendar dates using the user's zone, but compare day ordinals instead of
// elapsed 24-hour intervals: a DST switch must not move a task into another day.
function localDayNumber(iso) {
  const date = new Date(iso);
  return Date.UTC(date.getFullYear(), date.getMonth(), date.getDate()) / 86400000;
}

function monthSchedule(tasks, month) {
  const first = localDayNumber(new Date(month.getFullYear(), month.getMonth(), 1));
  const last = localDayNumber(new Date(month.getFullYear(), month.getMonth() + 1, 1));
  const days = last - first;
  const events = Array.from({ length: days }, () => []);
  const rows = [];
  for (const task of tasks) {
    const start = task.start_at ? localDayNumber(task.start_at) : null;
    const due = task.deadline_at ? localDayNumber(task.deadline_at) : null;
    if (start != null && start >= first && start < last) events[start - first].push({ task, kind: start === due ? "Start · Due" : "Start" });
    if (due != null && due >= first && due < last && due !== start) events[due - first].push({ task, kind: "Due" });
    const from = start ?? due, to = due ?? start;
    if (from != null && to != null && from < last && to >= first) {
      rows.push({ task, start: Math.max(0, from - first), end: Math.min(days - 1, to - first),
        milestone: start == null || due == null });
    }
  }
  for (const day of events) day.sort((a, b) => (a.kind.startsWith("Due") ? 0 : 1) - (b.kind.startsWith("Due") ? 0 : 1)
    || a.task.title.localeCompare(b.task.title));
  rows.sort((a, b) => a.start - b.start || a.end - b.end || a.task.title.localeCompare(b.task.title));
  return { first, days, events, rows };
}

function renderTimeline() {
  const root = $("#timeline-view");
  root.hidden = state.dashboard || !["calendar", "gantt"].includes(state.view);
  if (root.hidden || !state.board) return;
  const month = state.timelineMonth;
  const year = month.getFullYear(), index = month.getMonth();
  const visible = state.board.tasks.filter((t) => !isHumanOnly() || t.human);
  const { days, events, rows } = monthSchedule(visible, month);
  const focused = document.activeElement?.closest("#timeline-view button[data-shift]")?.dataset.shift;
  const focusedTask = document.activeElement?.closest("#timeline-view a[data-task-id]")?.dataset.taskId;
  const previousScroll = root.querySelector(".timeline-scroll");
  const left = previousScroll?.scrollLeft || 0, top = previousScroll?.scrollTop || 0;
  const move = (n) => {
    state.timelineMonth = new Date(year, index + n, 1);
    renderTimeline();
    root.querySelector(`button[data-shift="${n}"]`)?.focus({ preventScroll: true });
  };
  const heading = el("div", { class: "timeline-heading" },
    el("div", {}, el("h2", {}, month.toLocaleDateString([], { month: "long", year: "numeric" })),
      el("p", {}, state.view === "gantt" ? "Planned start to deadline · local dates" : "Planned starts and deadlines · local dates")),
    el("div", { class: "timeline-controls", role: "group", "aria-label": "Navigate months" },
      el("button", { class: "btn", type: "button", "data-shift": "-1", "aria-label": "Previous month", onclick: () => move(-1) }, "‹"),
      el("button", { class: "btn", type: "button", "data-shift": "today", onclick: () => {
        const today = new Date(); state.timelineMonth = new Date(today.getFullYear(), today.getMonth(), 1);
        renderTimeline(); root.querySelector('button[data-shift="today"]')?.focus({ preventScroll: true });
      } }, "Today"),
      el("button", { class: "btn", type: "button", "data-shift": "1", "aria-label": "Next month", onclick: () => move(1) }, "›")));
  const open = (task, kind) => el("a", { href: `#${task.project}/${task.id}`, "data-task-id": task.id,
    onclick: (e) => { e.preventDefault(); openTask(task.id).then(() => {
      if (state.task === task.id) $("#p-title").focus({ preventScroll: true });
    }); }, "aria-label": `${kind}: ${task.title}, ${task.project}` }, task.title);
  let content;
  if (state.view === "calendar") {
    const offset = (new Date(year, index, 1).getDay() + 6) % 7;
    const cells = Array.from({ length: Math.ceil((offset + days) / 7) * 7 }, (_, i) => {
      const day = i - offset + 1;
      if (day < 1 || day > days) return el("div", { class: "calendar-cell outside", "aria-hidden": "true" });
      const date = new Date(year, index, day);
      const dateLabel = date.toLocaleDateString([], { weekday: "long", year: "numeric", month: "long", day: "numeric" });
      return el("section", { class: "calendar-cell" + (localDayNumber(date) === localDayNumber(new Date()) ? " today" : ""),
        "aria-label": dateLabel },
        el("h3", {}, el("span", {}, String(day))),
        el("ul", { class: "calendar-events" }, ...events[day - 1].map(({ task, kind }) =>
          el("li", { style: `--event-color:${projectColor(task.project)}` },
            el("span", { class: "event-kind" }, kind), open(task, `${kind} on ${dateLabel}`)))));
    });
    content = el("div", { class: "timeline-scroll" },
      events.every((day) => !day.length) ? el("p", { class: "timeline-empty" }, "No starts or deadlines this month. Add dates to a task to plan it here.") : null,
      el("div", { class: "calendar-grid", role: "group", "aria-label": "Calendar for " + month.toLocaleDateString([], { month: "long", year: "numeric" }) },
        ...["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"].map((d) => el("div", { class: "calendar-weekday" }, d)), ...cells));
  } else {
    const today = localDayNumber(new Date());
    const header = el("tr", {}, el("th", { scope: "col", class: "gantt-task" }, "Task"),
      ...Array.from({ length: days }, (_, n) => el("th", { scope: "col", class: today === localDayNumber(new Date(year, index, n + 1)) ? "today" : null,
        title: new Date(year, index, n + 1).toLocaleDateString([], { dateStyle: "full" }) }, String(n + 1))));
    const body = rows.map(({ task, start, end, milestone }) => el("tr", {},
      el("th", { scope: "row", class: "gantt-task" }, el("span", { class: "gantt-title" }, task.title),
        el("span", { class: "gantt-meta" }, `${task.project} · ${task.status}`)),
      ...(start ? [el("td", { colspan: start })] : []),
      el("td", { colspan: end - start + 1, class: "gantt-slot", style: `--event-color:${projectColor(task.project)}` },
        el("span", { class: milestone ? "gantt-marker" : "gantt-bar" },
          open(task, `${milestone ? "Milestone" : "Planned"} ${task.start_at ? new Date(task.start_at).toLocaleString() : ""}${task.deadline_at ? " to " + new Date(task.deadline_at).toLocaleString() : ""}`))),
      ...(end < days - 1 ? [el("td", { colspan: days - end - 1 })] : [])));
    content = rows.length ? el("div", { class: "timeline-scroll" },
      el("table", { class: "gantt-chart", style: `width:${220 + days * 38}px`, "aria-label": "Gantt schedule for " + month.toLocaleDateString([], { month: "long", year: "numeric" }) },
        el("colgroup", {}, el("col", { class: "gantt-label-col" }), ...Array.from({ length: days }, () => el("col", {}))),
        el("thead", {}, header), el("tbody", {}, ...body)))
      : el("p", { class: "timeline-empty" }, "No scheduled tasks in this month. Add a planned start or deadline to a task.");
  }
  root.replaceChildren(heading, content);
  const scroll = root.querySelector(".timeline-scroll");
  if (scroll) { scroll.scrollLeft = left; scroll.scrollTop = top; }
  if (focused) root.querySelector(`button[data-shift="${focused}"]`)?.focus({ preventScroll: true });
  if (focusedTask) root.querySelector(`a[data-task-id="${focusedTask}"]`)?.focus({ preventScroll: true });
}

// The board can show only human tasks; off at first, and each browser remembers the choice.
function isHumanOnly() {
  return localStorageGet("brokan.humanOnly") === "1";
}

function setHumanOnly(humanOnly) {
  try { localStorage.setItem("brokan.humanOnly", humanOnly ? "1" : "0"); } catch {}
  render();
}

// Backlog and done, which only grows, start collapsed to a thin strip, as in Trello or
// Linear, and each browser remembers the choice. Cards can still be dropped on a strip.
function isCollapsed(status) {
  return localStorageGet(`brokan.${status}Collapsed`) !== "0";
}

function setCollapsed(status, collapsed) {
  try { localStorage.setItem(`brokan.${status}Collapsed`, collapsed ? "1" : "0"); } catch {}
  showCollapsed();
}

function showCollapsed() {
  const columns = $("#columns");
  const cols = [...columns.querySelectorAll(".column")];
  if (!cols.length) return;
  for (const col of cols) {
    const button = col.querySelector(".collapse");
    if (!button) continue;
    const { status } = col.dataset;
    const collapsed = isCollapsed(status);
    col.classList.toggle("collapsed", collapsed);
    // The arrows point the way the column folds: to the right edge, or for backlog,
    // to the left one.
    const left = status === "backlog";
    button.replaceChildren(icon(collapsed !== left ? "expand" : "collapse"));
    button.title = collapsed ? `Show ${status}` : `Collapse ${status}`;
    button.setAttribute("aria-label", button.title);
  }
  columns.style.gridTemplateColumns = cols.map((c) => (c.classList.contains("collapsed") ? "48px" : "minmax(260px, 1fr)")).join(" ");
}

// A description as one line of plain text for a card: Markdown markers dropped, lines joined.
function plainText(md) {
  return md.split("\n")
    .map((l) => l.replace(/^\s*(#{1,6}|>|[-*+]|\d+[.)])\s+/, "").replace(/^\[[ xX]\]\s+/, ""))
    .filter((l) => l.trim() && !/^\s*```/.test(l))
    .join(" ")
    .replace(/\*\*|__|`/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

function card(t, showProject) {
  const node = el("div", { class: "card" + (state.task === t.id ? " open" : ""), draggable: "true", "data-id": t.id,
                           onclick: () => openTask(t.id) },
    el("div", { class: "head" },
      el("div", { class: "title" }, t.title),
      // On "All projects" a dot in the project's colour tells the projects apart.
      t.human ? el("span", { class: "human-mark", title: "Human task: agents skip it" }, icon("human")) : null,
      el("span", { class: "num", title: `Copy ${key(t)}`, onclick: (e) => { e.stopPropagation(); copyKey(key(t)); } },
        showProject ? el("span", { class: "dot", style: `background:${projectColor(t.project)}` }) : null, `#${t.id}`)),
    t.description ? el("div", { class: "desc" }, plainText(t.description)) : null,
    t.priority && t.priority !== "none"
      ? el("span", { class: `priority ${t.priority}`, title: `${t.priority} priority` },
          t.priority[0].toUpperCase() + t.priority.slice(1)) : null,
    deadlineBadge(t),
    t.tags.length ? el("div", { class: "tags" }, t.tags.map((tag) => el("span", { class: "tag" }, tag))) : null);
  node.addEventListener("dragstart", (e) => {
    state.dragging = t.id;
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", String(t.id));
    requestAnimationFrame(() => node.classList.add("dragging"));
  });
  node.addEventListener("dragend", () => {
    node.classList.remove("dragging");
    document.querySelectorAll(".drop-line").forEach((d) => d.remove());
    state.dragging = null;
    // A drop reloads once the move is saved; a cancelled drag puts the card back.
    if (!state.dropped) loadBoard();
    state.dropped = false;
  });
  return node;
}

// The whole column takes drops; cards land in its list.
function wireDrop(col, list, status) {
  const line = el("div", { class: "drop-line" });
  const cardAfter = (y) => {
    const cards = [...list.querySelectorAll(".card:not(.dragging)")];
    return cards.find((c) => { const r = c.getBoundingClientRect(); return y < r.top + r.height / 2; }) || null;
  };
  col.addEventListener("dragover", (e) => {
    if (!state.dragging) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    const after = cardAfter(e.clientY);
    if (after) list.insertBefore(line, after); else list.append(line);
  });
  col.addEventListener("dragleave", (e) => {
    if (!col.contains(e.relatedTarget)) line.remove();
  });
  col.addEventListener("drop", async (e) => {
    e.preventDefault();
    const id = state.dragging;
    if (!id) return;
    const after = cardAfter(e.clientY);
    const dragged = document.querySelector(`.card[data-id="${id}"]`);
    if (dragged) list.insertBefore(dragged, line.parentNode === list ? line : null);
    line.remove();
    state.dropped = true;
    if (fresh === id) fresh = null;
    const before = after ? Number(after.dataset.id) : null;
    try {
      await api("POST", `/tasks/${id}/move`, { status, before });
      if (state.task === id) refreshTask();
    } catch (err) { toast(err.message); }
    loadBoard();
  });
}

function localStorageGet(k) {
  try { return localStorage.getItem(k); } catch { return null; }
}

// ---------- new task ----------

// New task creates the task in todo at once and opens it like any other, with its title
// selected to type over; its Status field moves it elsewhere. Closed untouched, it is
// deleted again.
const NEW_TITLE = "New task";
let fresh = null; // id of the task just added, until something in it is saved

async function newTask() {
  const projects = state.board.projects.filter((p) => !p.archived);
  if (!projects.length) { toast("Create or restore a project first"); return; }
  const last = localStorageGet("brokan.newTaskProject");
  const project = state.project !== "all" && projects.some((p) => p.slug === state.project) ? state.project
    : projects.some((p) => p.slug === last) ? last : projects[0].slug;
  try {
    const task = await api("POST", "/tasks", { project, title: NEW_TITLE });
    await openTask(task.id);
    fresh = task.id;
    loadBoard();
    $("#p-title").focus({ preventScroll: true });
    $("#p-title").select();
  } catch (err) { toast(err.message); }
}

function dropFresh() {
  if (!fresh) return;
  const id = fresh;
  fresh = null;
  api("DELETE", `/tasks/${id}`).then(loadBoard, () => {});
}

// Leaving the page also counts as closing the task.
window.addEventListener("pagehide", () => {
  if (fresh) fetch(`/api/tasks/${fresh}`, { method: "DELETE", keepalive: true });
});

// ---------- new project ----------

function showNewProject() {
  $("#project-dialog").hidden = false;
  $("#new-project [name=slug]").focus();
}

function hideNewProject() {
  $("#new-project").reset();
  $("#project-dialog").hidden = true;
}

$("#new-task").addEventListener("click", newTask);
$("#human-only").addEventListener("click", () => setHumanOnly(!isHumanOnly()));
$("#new-project-toggle").addEventListener("click", showNewProject);
$("#new-project-cancel").addEventListener("click", hideNewProject);
$("#project-dialog").addEventListener("mousedown", (e) => { if (e.target.id === "project-dialog") hideNewProject(); });
$("#project-dialog").addEventListener("keydown", (e) => { if (e.key === "Escape") { e.stopPropagation(); hideNewProject(); } });
$("#new-project").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = new FormData(e.target);
  try {
    const p = await api("POST", "/projects", { slug: f.get("slug"), name: f.get("name") });
    hideNewProject();
    setProject(p.slug);
  } catch (err) { toast(err.message); }
});

// ---------- task panel ----------

// Every field is editable in place and saves when it loses focus.
const panel = $("#panel");
let panelTask = null;

async function openTask(id) {
  leavePanelField();
  if (fresh !== id) dropFresh();
  state.task = id;
  panelTask = null;
  timeReport = null;
  $("#p-timer").disabled = true;
  $("#p-time-list").replaceChildren();
  $("#p-time-total").textContent = "Loading time…";
  $("#p-time-running").hidden = true;
  document.querySelectorAll(".card.open").forEach((c) => c.classList.remove("open"));
  document.querySelector(`.card[data-id="${id}"]`)?.classList.add("open");
  $("#p-comment").value = "";
  pending = [];
  renderPending();
  fit($("#p-comment"));
  updateHash();
  await refreshTask();
  if (state.task !== id) return;
  panel.classList.add("shown");
  panel.inert = false;
  panel.setAttribute("aria-hidden", "false");
}

// Blurring a field saves it, so do that before the panel lets go of a task.
function leavePanelField() {
  if (panel.contains(document.activeElement)) document.activeElement.blur();
}

function closeTask() {
  leavePanelField();
  dropFresh();
  state.task = null;
  panelTask = null;
  timeReport = null;
  $("#p-timer").disabled = true;
  updateHash();
  panel.classList.remove("shown");
  panel.inert = true;
  panel.setAttribute("aria-hidden", "true");
  document.querySelectorAll(".card.open").forEach((c) => c.classList.remove("open"));
}

async function refreshTask() {
  const id = state.task;
  if (!id) return;
  let data;
  try {
    data = await api("GET", `/tasks/${id}`);
  } catch (e) {
    if (state.task === id && /no task/.test(e.message)) { toast("That task was deleted"); closeTask(); }
    return;
  }
  // Another task may have taken the panel meanwhile.
  if (state.task !== id) return;
  const { task, comments, images } = data;
  panelTask = task;
  refreshTime();
  $("#p-key-text").textContent = key(task);
  $("#p-key").title = `Copy task ID ${key(task)}`;
  $("#p-key").setAttribute("aria-label", `Copy task ID ${key(task)}`);
  // Never overwrite what is being typed or picked.
  if (!$("#p-project").contains(document.activeElement)) renderProjectPick(task);
  for (const [field, value] of [[$("#p-title"), task.title], [$("#p-desc"), task.description], [$("#p-tags"), task.tags.join(", ")]]) {
    if (document.activeElement !== field) { field.value = value; fit(field); }
  }
  showDesc();
  const status = $("#p-status");
  const statuses = state.board ? state.board.statuses : [task.status];
  status.replaceChildren(...statuses.map((s) => el("option", { value: s }, s)));
  if (!statuses.includes(task.status)) status.append(el("option", { value: task.status }, task.status));
  status.value = task.status;
  if (document.activeElement !== $("#p-priority")) $("#p-priority").value = task.priority || "none";
  if (document.activeElement !== $("#p-start")) $("#p-start").value = task.start_at ? localDeadline(task.start_at) : "";
  $("#p-start-clear").hidden = !task.start_at;
  if (document.activeElement !== $("#p-deadline")) $("#p-deadline").value = task.deadline_at ? localDeadline(task.deadline_at) : "";
  $("#p-deadline-clear").hidden = !task.deadline_at;
  updateDeadlineHint(task);
  $("#p-human").checked = task.human;
  $("#p-created").replaceChildren(timeEl(task.created_at));
  $("#p-updated").replaceChildren(timeEl(task.updated_at));
  $("#p-comment-count").textContent = comments.length ? String(comments.length) : "";
  $("#p-images").replaceChildren(...images.filter((i) => i.comment_id === null).map((i) =>
    thumb(i.file, async () => {
      try { await api("DELETE", `/images/${i.id}`); refreshTask(); } catch (err) { toast(err.message); }
    })));
  $("#p-comments").replaceChildren(...comments.map((c) => {
    const body = el("div", { class: "md" });
    body.innerHTML = markdown(c.body);
    const attached = images.filter((i) => i.comment_id === c.id);
    return el("div", { class: "comment" },
      el("div", { class: "who" }, el("b", {}, c.author), " · ", timeEl(c.created_at),
        el("button", { class: "remove", title: "Delete comment", "aria-label": "Delete comment",
                       onclick: () => deleteComment(c) }, icon("trash"))),
      c.body ? body : null,
      attached.length ? el("div", { class: "thumbs" }, attached.map((i) => thumb(i.file))) : null);
  }));
}

function updateDeadlineHint(task) {
  const overdue = deadlineOverdue(task);
  $("#p-deadline-hint").textContent = overdue ? "Overdue · local time" : "Local time";
  $("#p-deadline-hint").classList.toggle("overdue", overdue);
}

let timeReport = null;
async function refreshTime() {
  const id = state.task;
  if (!id) return;
  try {
    const report = await api("GET", `/tasks/${id}/time`);
    if (state.task === id) renderTime(report);
  } catch (err) { if (state.task === id) toast(err.message); }
}

function renderTime(report) {
  timeReport = report;
  $("#p-timer").disabled = false;
  $("#p-time-total").textContent = `${duration(report.total_seconds)} tracked`;
  $("#p-timer").textContent = report.running_since == null ? "Start timer" : "Stop timer";
  $("#p-timer").setAttribute("aria-label", report.running_since == null ? "Start task timer" : "Stop task timer");
  $("#p-time-running").hidden = report.running_since == null;
  if (report.running_since != null) updateRunningTime();
  $("#p-time-list").replaceChildren(...report.entries.map((entry) =>
    el("li", { class: "time-entry" },
      el("span", { class: "mono" }, entry.ended_at == null ? "Running" : duration(entry.seconds)),
      el("time", { datetime: new Date(entry.started_at * 1000).toISOString() },
        new Date(entry.started_at * 1000).toLocaleString()),
      entry.ended_at != null ? el("button", { type: "button", title: "Remove time entry",
        "aria-label": `Remove ${duration(entry.seconds)} time entry`, onclick: async () => {
          if (!confirm(`Remove this ${duration(entry.seconds)} time entry?`)) return;
          try {
            const result = await api("DELETE", `/tasks/${entry.task_id}/time/${entry.id}`);
            if (state.task === entry.task_id) renderTime(result);
            if (state.dashboard) loadDashboard();
          } catch (err) { toast(err.message); }
        } }, "Remove") : null)));
}

function updateRunningTime() {
  if (timeReport?.running_since == null || !state.task) return;
  const elapsed = Math.max(0, Math.floor(Date.now() / 1000) - timeReport.running_since);
  $("#p-time-running").textContent = `Timer running · ${duration(elapsed)} elapsed`;
  $("#p-time-total").textContent = `${duration(timeReport.total_seconds + Math.max(0, Math.floor(Date.now() / 1000) - timeReport.as_of))} tracked`;
}
setInterval(updateRunningTime, 1000);

$("#p-timer").addEventListener("click", async (e) => {
  if (!panelTask || e.currentTarget.disabled) return;
  const id = panelTask.id;
  const running = timeReport?.running_since != null;
  e.currentTarget.disabled = true;
  try {
    const result = await api(running ? "DELETE" : "POST", `/tasks/${id}/timer`);
    if (state.task === id) renderTime(result);
    if (state.dashboard) loadDashboard();
  } catch (err) { toast(err.message); }
  e.currentTarget.disabled = false;
});

$("#p-time-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  if (!panelTask) return;
  const id = panelTask.id;
  const button = e.currentTarget.querySelector("button");
  if (button.disabled) return;
  button.disabled = true;
  try {
    const report = await api("POST", `/tasks/${id}/time`, { minutes: Number($("#p-minutes").value) });
    if (state.task === id) { renderTime(report); $("#p-minutes").value = ""; }
    if (state.dashboard) loadDashboard();
  } catch (err) { toast(err.message); }
  button.disabled = false;
});

async function deleteComment(c) {
  if (!confirm("Delete this comment? This cannot be undone.")) return;
  try {
    await api("DELETE", `/comments/${c.id}`);
    await refreshTask();
    loadBoard();
  } catch (err) { toast(err.message); }
}

// The project tag doubles as a picker that moves the task to another project.
function renderProjectPick(task) {
  const projects = state.board ? state.board.projects.filter((p) => !p.archived).map((p) => p.slug) : [];
  if (!projects.includes(task.project)) projects.push(task.project);
  const select = el("select", { title: "Project", "aria-label": "Project" },
    ...projects.map((slug) => el("option", { value: slug }, slug)));
  select.value = task.project;
  select.addEventListener("change", () => {
    try { localStorage.setItem("brokan.newTaskProject", select.value); } catch {}
    patchTask({ project: select.value });
  });
  $("#p-project").replaceChildren(el("label", { class: "tag project-pick" },
    projectGlyph(task.project), select));
}

// Textareas grow with their text instead of scrolling.
function fit(t) {
  if (t.tagName !== "TEXTAREA") return;
  t.style.height = "auto";
  t.style.height = t.scrollHeight + "px";
}

async function patchTask(fields) {
  if (!panelTask) return;
  if (fresh === panelTask.id) fresh = null;
  try {
    await api("PATCH", `/tasks/${panelTask.id}`, fields);
    await refreshTask();
    loadBoard();
    return true;
  } catch (e) { toast(e.message); refreshTask(); return false; }
}

// Escape undoes the field's edit; on a task just added and not saved yet, it drops the task.
function cancelField(e, saved) {
  e.stopPropagation();
  if (fresh && fresh === state.task) { closeTask(); return; }
  e.target.value = saved;
  fit(e.target);
  e.target.blur();
}

$("#p-close").addEventListener("click", closeTask);
$("#p-key").addEventListener("click", () => { if (panelTask) copyKey(key(panelTask)); });
for (const field of [$("#p-title"), $("#p-desc")]) field.addEventListener("input", () => fit(field));

// ⌘↵ in the title or description saves and closes.
$("#p-title").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); closeTask(); }
  // The title is one line: Enter, like Tab, goes on to the description.
  else if ((e.key === "Enter" || e.key === "Tab") && !e.shiftKey) { e.preventDefault(); editDesc(); }
  else if (e.key === "Escape") cancelField(e, panelTask ? panelTask.title : "");
});
$("#p-title").addEventListener("blur", (e) => {
  if (!panelTask) return;
  const v = e.target.value.trim();
  if (v && v !== panelTask.title) patchTask({ title: v });
  else { e.target.value = panelTask.title; fit(e.target); }
});

$("#p-desc").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); closeTask(); }
  else if (e.key === "Escape") cancelField(e, panelTask ? panelTask.description : "");
});
$("#p-desc").addEventListener("blur", (e) => {
  if (panelTask && e.target.value.trim() !== panelTask.description) patchTask({ description: e.target.value });
  showDesc();
});

// The description shows rendered, like a comment, and turns into its textarea to edit.
// Empty, the textarea stays, so its placeholder has something to click.
function showDesc() {
  const desc = $("#p-desc"), view = $("#p-desc-view");
  const editing = document.activeElement === desc || !desc.value.trim();
  desc.hidden = !editing;
  view.hidden = editing;
  if (!editing) view.innerHTML = markdown(desc.value);
}

function editDesc() {
  const desc = $("#p-desc");
  $("#p-desc-view").hidden = true;
  desc.hidden = false;
  fit(desc);
  desc.focus();
}

// A click on a link opens it instead of editing.
$("#p-desc-view").addEventListener("mousedown", (e) => { if (e.target.closest("a")) e.preventDefault(); });
$("#p-desc-view").addEventListener("focus", editDesc);

$("#p-status").addEventListener("change", (e) => patchTask({ status: e.target.value }));
$("#p-priority").addEventListener("change", (e) => patchTask({ priority: e.target.value }));
function bindSchedulePicker(id, name) {
  const field = $(id);
  const original = () => panelTask?.[name] ? localDeadline(panelTask[name]) : "";
  field.addEventListener("change", async () => {
    if (!field.value) {
      // A half-edited picker is not a clear operation; use the explicit Clear button.
      field.value = original();
      return;
    }
    const date = new Date(field.value);
    if (!Number.isFinite(date.getTime()) || localDeadline(date.toISOString()) !== field.value.slice(0, 16)) {
      toast("Choose a valid local date and time");
      field.value = original();
      return;
    }
    const saved = await patchTask({ [name]: date.toISOString().replace(/\.\d{3}Z$/, "Z") });
    if (!saved) field.value = original();
  });
  $(id + "-clear").addEventListener("click", async () => {
    if (await patchTask({ [name]: null })) field.focus();
  });
}
bindSchedulePicker("#p-start", "start_at");
bindSchedulePicker("#p-deadline", "deadline_at");
$("#p-human").addEventListener("change", (e) => patchTask({ human: e.target.checked }));

// ---------- images ----------

// Pasting or dropping an image attaches it, as in ChatGPT: to the task from the title or
// the description, to the comment being written from the comment box. Text is left alone.
let pending = []; // images waiting in the comment box, by file name

function imagesIn(transfer) {
  if (!transfer) return [];
  const fromItems = transfer.items ? [...transfer.items].filter((i) => i.kind === "file").map((i) => i.getAsFile()) : [];
  return (fromItems.length ? fromItems : [...(transfer.files || [])]).filter((f) => f && f.type.startsWith("image/"));
}

async function uploadImage(image) {
  const res = await fetch("/api/images", { method: "POST", headers: { "Content-Type": image.type }, body: image });
  const reply = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(reply.error || res.statusText);
  return reply.file;
}

async function attachImages(images, toComment) {
  if (!panelTask) return;
  const task = panelTask.id;
  try {
    for (const image of images) {
      const file = await uploadImage(image);
      if (toComment) {
        pending.push(file);
        renderPending();
      } else {
        await api("POST", `/tasks/${task}/images`, { file });
        if (fresh === task) fresh = null;
      }
    }
    if (!toComment) refreshTask();
  } catch (err) { toast(err.message); }
}

// A thumbnail that opens the full image; with `onRemove`, an × to take it off.
function thumb(file, onRemove) {
  return el("div", { class: "thumb" },
    el("a", { href: `/images/${file}`, target: "_blank", rel: "noopener" }, el("img", { src: `/images/${file}`, alt: "Attached image" })),
    onRemove ? el("button", { type: "button", class: "remove-image", title: "Remove image", "aria-label": "Remove image", onclick: onRemove }, icon("close")) : null);
}

function renderPending() {
  $("#p-pending").replaceChildren(...pending.map((file, i) => thumb(file, () => { pending.splice(i, 1); renderPending(); })));
  $("#p-send").disabled = !$("#p-comment").value.trim() && !pending.length;
}

for (const field of [$("#p-title"), $("#p-desc"), $("#p-comment")]) {
  field.addEventListener("paste", (e) => {
    const images = imagesIn(e.clipboardData);
    if (!images.length) return;
    e.preventDefault();
    attachImages(images, field.id === "p-comment");
  });
}
for (const [zone, toComment] of [[$("#p-desc").closest(".section"), false], [$("#p-comment-form"), true]]) {
  zone.addEventListener("dragover", (e) => {
    if (!e.dataTransfer || ![...e.dataTransfer.types].includes("Files")) return;
    e.preventDefault();
    zone.classList.add("dropping");
  });
  zone.addEventListener("dragleave", (e) => { if (!zone.contains(e.relatedTarget)) zone.classList.remove("dropping"); });
  zone.addEventListener("drop", (e) => {
    zone.classList.remove("dropping");
    const images = imagesIn(e.dataTransfer);
    if (!images.length) return;
    e.preventDefault();
    attachImages(images, toComment);
  });
}

// Tags are typed comma-separated and tidied the way the server stores them.
const cleanTags = (text) => [...new Set(text.split(",").map((t) => t.trim().replace(/\s+/g, " ").toLowerCase()).filter(Boolean))];
$("#p-tags").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); e.target.blur(); }
  else if (e.key === "Escape") cancelField(e, panelTask ? panelTask.tags.join(", ") : "");
});
$("#p-tags").addEventListener("blur", (e) => {
  if (!panelTask) return;
  const tags = cleanTags(e.target.value);
  if (tags.join(",") !== panelTask.tags.join(",")) patchTask({ tags: tags.join(", ") });
  else e.target.value = panelTask.tags.join(", ");
});

$("#p-comment-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const body = $("#p-comment").value.trim();
  if ((!body && !pending.length) || !panelTask) return;
  try {
    await api("POST", `/tasks/${panelTask.id}/comments`, { body, images: pending });
    if (fresh === panelTask.id) fresh = null;
    $("#p-comment").value = "";
    pending = [];
    renderPending();
    fit($("#p-comment"));
    await refreshTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});
// The send button is live only once there is something to send.
$("#p-comment").addEventListener("input", (e) => { $("#p-send").disabled = !e.target.value.trim() && !pending.length; fit(e.target); });
$("#p-comment").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); $("#p-comment-form").requestSubmit(); }
});

$("#p-delete").addEventListener("click", async () => {
  if (!panelTask) return;
  const extra = panelTask.comments ? ` and its ${panelTask.comments} comment${panelTask.comments > 1 ? "s" : ""}` : "";
  if (!confirm(`Delete ${key(panelTask)}${extra}? This cannot be undone.`)) return;
  try {
    if (fresh === panelTask.id) fresh = null;
    await api("DELETE", `/tasks/${panelTask.id}`);
    closeTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});

// ---------- search ----------

// The search window, as in ChatGPT: tasks of every project, found by the server, open
// ones first. Arrows pick, Enter opens, Esc closes. Without words it shows the latest changes.
let results = [], selected = 0, searchRequest = 0, searchTimer;

function openSearch() {
  closeMenu();
  $("#search-dialog").hidden = false;
  $("#search").value = "";
  $("#search").focus();
  runSearch();
}

function closeSearch() {
  $("#search-dialog").hidden = true;
}

async function runSearch() {
  const q = $("#search").value.trim();
  const request = ++searchRequest;
  try {
    const found = await api("GET", "/search?q=" + encodeURIComponent(q));
    if (request !== searchRequest) return;
    results = found;
    selected = 0;
    $("#search-results").replaceChildren(
      ...(results.length ? [el("div", { class: "label" }, q ? "Tasks" : "Recent")] : []),
      ...results.map((t, i) => el("button", { type: "button", class: "result" + (i === 0 ? " selected" : ""),
                                             onmousemove: () => select(i), onclick: () => pick(t) },
        el("span", { class: "dot", style: `background:${projectColor(t.project)}` }),
        el("span", { class: "title" }, t.title),
        el("span", { class: "meta" }, el("span", { class: "mono" }, key(t)), t.status))),
      ...(results.length ? [] : [el("div", { class: "note" }, q ? "No tasks match." : "No tasks yet.")]));
  } catch (err) { toast(err.message); }
}

function select(i) {
  const rows = $("#search-results").querySelectorAll(".result");
  if (!rows.length) return;
  selected = (i + rows.length) % rows.length;
  rows.forEach((r, j) => r.classList.toggle("selected", j === selected));
  rows[selected].scrollIntoView({ block: "nearest" });
}

// Opens the task, in its own project unless the board shows all of them.
function pick(t) {
  closeSearch();
  if (state.project !== "all" && state.project !== t.project) setProject(t.project);
  openTask(t.id);
}

$("#search-open").addEventListener("click", openSearch);
$("#search").addEventListener("input", () => { clearTimeout(searchTimer); searchTimer = setTimeout(runSearch, 120); });
$("#search").addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); select(selected + (e.key === "ArrowDown" ? 1 : -1)); }
  else if (e.key === "Enter" && results[selected]) { e.preventDefault(); pick(results[selected]); }
  else if (e.key === "Escape") { e.stopPropagation(); closeSearch(); }
});
$("#search-dialog").addEventListener("mousedown", (e) => { if (e.target.id === "search-dialog") closeSearch(); });

// ---------- keyboard, polling, start ----------

document.addEventListener("keydown", (e) => {
  const typing = e.target.closest("input, textarea, select");
  if (e.key === "Escape" && state.menu) { closeMenu(); return; }
  if (e.key === "Escape" && state.task && !typing) closeTask();
  if ((e.key === "/" && !typing && !e.metaKey && !e.ctrlKey) || (e.key === "k" && (e.metaKey || e.ctrlKey))) {
    e.preventDefault();
    openSearch();
  }
  if (e.key === "n" && !typing && !e.metaKey && !e.ctrlKey) {
    if (!$("#new-task").hidden) { e.preventDefault(); newTask(); }
  }
  if (e.key === "h" && !typing && !e.metaKey && !e.ctrlKey) {
    if (!$("#human-only").hidden) { e.preventDefault(); setHumanOnly(!isHumanOnly()); }
  }
});

window.addEventListener("hashchange", () => {
  const { slug, task } = readHash();
  if (slug === "~dashboard") { if (!state.dashboard) showDashboard(); return; }
  if (state.dashboard || (slug || "all") !== state.project) setProject(slug || "all");
  if (task && task !== state.task) openTask(task);
  else if (!task && state.task) closeTask();
});

// Agents change the board through `brokan mcp`; a cheap counter tells us when.
async function poll() {
  if (document.hidden) return schedulePoll();
  try {
    const { changes } = await api("GET", "/changes");
    $("#offline").hidden = true;
    if (changes !== state.changes) {
      state.changes = changes;
      await loadBoard();
      if (state.dashboard) loadDashboard();
      if (state.task) refreshTask();
    }
  } catch {
    $("#offline").hidden = false;
  }
  schedulePoll();
}

// The next check starts only after this one finished, so a slow or busy server
// never piles up requests.
function schedulePoll() {
  setTimeout(poll, 2000);
}
schedulePoll();

// ---------- sync ----------

// Sync runs `brokan sync`: it commits the board and pulls or pushes it through the data
// folder's git remote. When both machines changed it, the board asks which to keep.
let syncing = false;

async function syncNow(keep) {
  if (syncing) return;
  syncing = true;
  toast("Syncing…");
  let diverged = null;
  try {
    const res = await fetch("/api/sync", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ keep }) });
    const reply = await res.json().catch(() => ({}));
    if (res.ok) { toast(reply.message); loadBoard(); }
    else if (reply.diverged) diverged = reply;
    else toast(reply.error || res.statusText);
  } catch (err) { toast(err.message); }
  syncing = false;
  if (diverged) chooseSide(diverged);
}

function chooseSide({ first }) {
  const repo = repoName(syncRemote);
  if (first) {
    // A repo another machine already syncs with: most likely, this machine is joining it.
    if (confirm(`${repo} already has a board. Use it on this machine?\n\nThis machine's current board is kept in brokan.db.bak.`)) syncNow("remote");
    else if (confirm(`Replace the board in ${repo} with this machine's board instead?`)) syncNow("local");
    return;
  }
  if (confirm("This machine and another one both changed the board since the last sync, and boards can't be merged.\n\nKeep THIS machine's board, and overwrite the other one?")) syncNow("local");
  else if (confirm("Take the other machine's board instead? This machine's board is kept in brokan.db.bak.")) syncNow("remote");
}

// The repo the board syncs with; null before it's set up.
let syncRemote = null;

// "you/brokan-data" from any of its URLs.
function repoName(url) {
  return (url || "").replace(/\.git$/, "").split(/[/:]/).filter(Boolean).slice(-2).join("/");
}

async function loadSyncRemote() {
  try { syncRemote = (await api("GET", "/sync")).remote; } catch {}
  return syncRemote;
}

// Sync asks for a repo, in Settings, the first time.
async function sync() {
  closeMenu();
  if (await loadSyncRemote()) syncNow();
  else openSettings("sync");
}

$("#sync-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const save = $("#sync-save");
  save.disabled = true;
  save.textContent = "Checking…";
  try {
    const res = await fetch("/api/sync", { method: "PUT", headers: { "Content-Type": "application/json" },
                                           body: JSON.stringify({ url: $("#sync-url").value }) });
    const reply = await res.json().catch(() => ({}));
    if (res.ok) {
      syncRemote = reply.remote;
      closeSettings();
      syncNow();
    } else {
      $("#sync-error").textContent = reply.error || res.statusText;
      $("#sync-error").hidden = false;
    }
  } catch (err) {
    $("#sync-error").textContent = err.message;
    $("#sync-error").hidden = false;
  }
  save.disabled = false;
  save.textContent = "Save and sync";
});

// ---------- menu and settings ----------

// The Settings button at the bottom of the sidebar opens a menu, as in Claude: Settings,
// Check for updates and Sync.
function toggleSettingsMenu() {
  const wasOpen = state.menu === "settings";
  closeMenu();
  if (wasOpen || !state.board) return;
  state.menu = "settings";
  const button = $("#settings");
  button.classList.add("open");
  const menu = el("div", { class: "menu", role: "menu", tabindex: "-1" },
    el("button", { onclick: () => openSettings() }, icon("settings"), "Settings"),
    el("button", { onclick: () => { closeMenu(); checkForUpdates(); } }, icon("update"), "Check for updates",
      el("span", { class: "value" }, `v${state.board.version}`)),
    el("button", { onclick: sync }, icon("sync"), "Sync"));
  document.body.append(menu);
  const r = button.getBoundingClientRect();
  menu.style.width = `${r.width}px`;
  menu.style.left = `${r.left}px`;
  menu.style.top = `${r.top - 6 - menu.offsetHeight}px`;
  menu.focus();
}

$("#settings").addEventListener("click", toggleSettingsMenu);

// The settings window, as in Claude's: categories on the left, their settings on the right.
function openSettings(section = "board") {
  closeMenu();
  $("#settings-backlog").checked = state.board?.statuses.includes("backlog") ?? false;
  $("#sync-url").value = syncRemote || "";
  $("#sync-create").hidden = !!syncRemote;
  $("#sync-error").hidden = true;
  $("#settings-version").textContent = state.board ? `Brokan ${state.board.version}` : "";
  showSection(section);
  $("#settings-dialog").hidden = false;
  if (section === "sync") $("#sync-url").focus();
  else $("#settings-close").focus();
}

function showSection(section) {
  document.querySelectorAll("#settings-nav button").forEach((b) => b.classList.toggle("active", b.dataset.section === section));
  document.querySelectorAll(".settings-pane").forEach((p) => { p.hidden = p.dataset.section !== section; });
}

function closeSettings() {
  $("#settings-dialog").hidden = true;
}

$("#settings-nav").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (b) showSection(b.dataset.section);
});
$("#settings-close").addEventListener("click", closeSettings);
$("#settings-dialog").addEventListener("mousedown", (e) => { if (e.target.id === "settings-dialog") closeSettings(); });
$("#settings-dialog").addEventListener("keydown", (e) => { if (e.key === "Escape") { e.stopPropagation(); closeSettings(); } });
$("#settings-backlog").addEventListener("change", async (e) => {
  const box = e.target;
  try {
    await api("PATCH", "/settings", { backlog: box.checked });
    loadBoard();
  } catch (err) { box.checked = !box.checked; toast(err.message); }
});
$("#settings-update").addEventListener("click", checkForUpdates);

// ---------- update ----------

// Brokan checks GitHub for a newer release once a day and offers it at the bottom of the
// sidebar; its menu asks right away. Installing replaces the running board and
// restarts it, and the page reloads once it's back.
async function checkUpdate() {
  try {
    showUpdate(await api("GET", "/update"));
  } catch {}
}

function showUpdate({ current, available }) {
  const button = $("#update");
  button.hidden = !available;
  if (!available) return;
  $("#update-label").textContent = `Update to ${available}`;
  button.title = `You have ${current}`;
  button.onclick = () => {
    if (confirm(`Update Brokan from ${current} to ${available}? The board restarts, which takes a few seconds.`)) installUpdate(available);
  };
}

async function checkForUpdates() {
  toast("Checking for updates…");
  try {
    const status = await api("GET", "/update?now=1");
    showUpdate(status);
    if (status.available) {
      if (confirm(`Brokan ${status.available} is out (you have ${status.current}). Download and install it now? The board restarts.`)) {
        installUpdate(status.available);
      }
    } else if (status.latest) {
      toast(`Brokan is up to date (${status.current})`);
    } else {
      toast("Can't reach GitHub to check for updates");
    }
  } catch (err) { toast(err.message); }
}

async function installUpdate(available) {
  const button = $("#update");
  button.hidden = false;
  button.disabled = true;
  $("#update-label").textContent = "Updating…";
  try {
    await api("POST", "/update");
  } catch (err) {
    toast(err.message);
    button.disabled = false;
    $("#update-label").textContent = `Update to ${available}`;
    return;
  }
  // The old program answered, then gives way to the new one: wait for it, then reload.
  await new Promise((r) => setTimeout(r, 1500));
  for (let i = 0; i < 60; i++) {
    try { await api("GET", "/update"); break; } catch { await new Promise((r) => setTimeout(r, 1000)); }
  }
  location.reload();
}

checkUpdate();
setInterval(checkUpdate, 24 * 60 * 60 * 1000);
loadSyncRemote();

// Running time changes without a database write; refresh the dashboard periodically.
setInterval(() => { if (state.dashboard && !document.hidden) loadDashboard(); }, 30000);

// Refresh relative timestamps and overdue state without replacing absolute deadline/time-entry dates.
setInterval(() => {
  document.querySelectorAll("time[data-relative]").forEach((t) => { t.textContent = ago(t.getAttribute("datetime")); });
  document.querySelectorAll(".deadline-badge").forEach((badge) => {
    const overdue = badge.dataset.done !== "true" && new Date(badge.querySelector("time").dateTime).getTime() < Date.now();
    badge.classList.toggle("overdue", overdue);
    badge.firstChild.textContent = overdue ? "Overdue · " : "Deadline · ";
  });
  if (state.task && panelTask?.id === state.task) updateDeadlineHint(panelTask);
}, 60000);

{
  const { slug, task } = readHash();
  state.dashboard = slug === "~dashboard";
  state.project = state.dashboard ? "all" : slug || localStorageGet("brokan.project") || "all";
  loadBoard().then(() => { if (task && !state.dashboard) openTask(task); });
  if (state.dashboard) loadDashboard();
}
