// The Kanzo board. Plain JavaScript, no build step; served at /app.js.
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
  board: null,           // last /api/board reply
  changes: null,         // change counter seen last
  task: null,            // open task id
  dragging: null,        // id of the card being dragged
  dropped: false,        // the current drag ended in a drop
  layout: "",            // key of the rendered column layout
  query: "",             // search box text
  allDone: false,        // show every card in the done column
  menu: null,            // slug of the project whose "⋯" menu is open
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
  board: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M8 7v7M12 7v4M16 7v9"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
  close: '<path d="M18 6 6 18M6 6l12 12"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  plus: '<path d="M5 12h14M12 5v14"/>',
  trash: '<path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  send: '<path d="M12 19V5M5 12l7-7 7 7"/>',
};

function icon(name, color) {
  const span = el("span", { class: "icon", style: color ? `color:${color}` : null });
  span.innerHTML = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`;
  return span;
}

document.querySelectorAll("[data-icon]").forEach((n) => n.replaceWith(icon(n.dataset.icon)));

let toastTimer;
function toast(message) {
  document.querySelector(".toast")?.remove();
  const t = el("div", { class: "toast" }, message);
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

function statusColor(status) {
  const i = state.board ? state.board.statuses.indexOf(status) : 0;
  return STATUS_COLORS[Math.max(i, 0) % STATUS_COLORS.length];
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
  return el("time", { datetime: ts, title: parseTime(ts).toLocaleString() }, ago(ts));
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
    const done = fullDone() ? "&done=all" : "";
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

function fullDone() {
  return state.allDone || state.query.trim() !== "";
}

// The address is #project or #project/task-id, so a task can be linked to.
function updateHash() {
  const hash = "#" + state.project + (state.task ? "/" + state.task : "");
  if (location.hash !== hash) history.replaceState(null, "", hash);
}

function readHash() {
  let hash = location.hash.slice(1);
  try { hash = decodeURIComponent(hash); } catch {}
  const [slug, task] = hash.split("/");
  return { slug: slug || null, task: Number(task) || null };
}

function setProject(slug) {
  state.project = slug;
  try { localStorage.setItem("kanzo.project", slug); } catch {}
  updateHash();
  state.layout = "";
  loadBoard();
}

function render() {
  renderNav();
  renderHeader();
  renderColumns();
}

function renderNav() {
  const { projects } = state.board;
  const open = projects.reduce((n, p) => n + p.open, 0);
  const link = (slug, name, count, glyph) =>
    el("a", { href: "#" + slug, class: state.project === slug ? "active" : null,
              onclick: (e) => { e.preventDefault(); setProject(slug); } },
      glyph,
      el("span", { class: "name" }, name),
      el("span", { class: "count" }, String(count)));
  const item = (p) => el("div", { class: "item" + (state.menu === p.slug ? " menu-open" : "") },
    link(p.slug, p.name, p.open, icon("folder", projectColor(p.slug))),
    el("button", { class: "more", title: "More", "aria-label": `More for ${p.name}`, "aria-haspopup": "menu",
                   onclick: (e) => toggleProjectMenu(p, e.currentTarget) }, icon("more")));
  $("#nav").replaceChildren(
    link("all", "All projects", open, icon("board")),
    ...(projects.length ? [el("div", { class: "label" }, "Projects")] : []),
    ...projects.map(item),
  );
}

// ---------- project menu ----------

// The "⋯" menu of a sidebar project. `state.menu` holds its slug, so the row keeps
// its highlight while the sidebar is redrawn by polling.
function toggleProjectMenu(p, button) {
  const wasOpen = state.menu === p.slug;
  closeMenu();
  if (wasOpen) return;
  state.menu = p.slug;
  button.closest(".item").classList.add("menu-open");
  const swatches = el("div", { class: "swatches", role: "group", "aria-label": "Color" });
  const drawSwatches = () => swatches.replaceChildren(...Object.entries(PROJECT_COLORS).map(([name, hex]) => {
    const current = projectColorName(p.slug) === name;
    return el("button", { class: "swatch", style: `background:${hex}`, title: name, "aria-label": name,
                          "aria-pressed": String(current), onclick: () => pickColor(p.slug, name).then(drawSwatches) },
      current ? icon("check") : null);
  }));
  drawSwatches();
  const remove = el("button", { class: "danger", onclick: () => { closeMenu(); deleteProject(p); } },
    icon("trash"), "Delete project");
  const menu = el("div", { class: "menu", role: "menu", tabindex: "-1" },
    el("div", { class: "label" }, "Color"), swatches, el("div", { class: "sep" }), remove);
  document.body.append(menu);
  const r = button.getBoundingClientRect();
  const below = r.bottom + 4 + menu.offsetHeight <= innerHeight;
  menu.style.top = `${below ? r.bottom + 4 : r.top - 4 - menu.offsetHeight}px`;
  menu.style.left = `${Math.max(8, Math.min(r.left, innerWidth - menu.offsetWidth - 8))}px`;
  // Focus the menu itself, so Tab reaches its buttons without one looking selected.
  menu.focus();
}

async function pickColor(slug, color) {
  try {
    await api("PATCH", `/projects/${encodeURIComponent(slug)}`, { color });
    await loadBoard();
  } catch (err) { toast(err.message); }
}

function closeMenu() {
  if (!state.menu) return;
  state.menu = null;
  document.querySelector(".menu")?.remove();
  document.querySelectorAll(".nav .menu-open").forEach((i) => i.classList.remove("menu-open"));
}

// A click elsewhere closes the menu; one on a "⋯" button is left to toggle it.
document.addEventListener("mousedown", (e) => {
  if (!e.target.closest(".menu, .nav .more")) closeMenu();
});
// A click outside the task panel closes it. Cards are left to open their own task,
// and the project menu to do its job.
document.addEventListener("mousedown", (e) => {
  if (state.task && e.button === 0 && !e.target.closest("#panel, .card, .menu")) closeTask();
});
window.addEventListener("resize", closeMenu);
window.addEventListener("blur", closeMenu);
$("#nav").addEventListener("scroll", closeMenu);

function currentProject() {
  return state.board.projects.find((p) => p.slug === state.project) || null;
}

function renderHeader() {
  const p = currentProject();
  const { projects, tasks } = state.board;
  const header = $("#header");
  $("#search-box").hidden = !projects.length;
  if (!projects.length) { header.replaceChildren(); return; }
  if (!p) {
    header.replaceChildren(el("h1", {}, "All projects"),
      el("span", { class: "sub" }, `${projects.reduce((n, p) => n + p.total, 0)} tasks across ${projects.length} projects`));
    return;
  }
  header.replaceChildren(
    el("h1", {}, p.name),
    el("span", { class: "sub mono" }, p.slug),
    ...(p.repo ? [el("span", { class: "sub" }, el("a", { href: `https://github.com/${p.repo}`, target: "_blank", rel: "noopener" }, p.repo))] : []),
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
  const { statuses, projects, tasks } = state.board;
  const columns = $("#columns");
  if (!projects.length) {
    state.layout = "";
    columns.style.display = "flex";
    columns.replaceChildren(el("div", { class: "empty" },
      el("h1", {}, "Welcome to Kanzo"),
      el("p", {}, "Create your first project in the sidebar. Link it to a GitHub repo and agents working in that repo will find it on their own."),
      el("button", { class: "btn primary", onclick: showNewProject }, "New project")));
    return;
  }
  columns.style.display = "";

  // Build the columns once per layout so a refresh never wipes a half-typed task.
  const layout = statuses.join(",") + "|" + state.project;
  if (state.layout !== layout) {
    state.layout = layout;
    columns.replaceChildren(...statuses.map((status, i) => {
      const col = el("section", { class: "column", "data-status": status },
        el("h2", {}, el("span", { class: "dot", style: `background:${statusColor(status)}` }), status,
          el("span", { class: "count" }),
          // At the top, so a long column never hides it.
          i === 0 ? el("button", { class: "add", title: "Add task (n)", "aria-label": "Add task", onclick: newTask }, icon("plus")) : null),
        el("div", { class: "cards" }),
      );
      wireDrop(col, col.querySelector(".cards"), status);
      return col;
    }));
  }

  // During a drag the DOM is being moved by hand; refresh once it ends.
  if (state.dragging) return;
  const all = state.project === "all";
  const shown = tasks.filter(matches);
  $("#search-note").textContent = !state.query.trim() ? ""
    : shown.length ? `${shown.length} of ${tasks.length}` : "No matches";
  for (const status of statuses) {
    const col = columns.querySelector(`[data-status="${CSS.escape(status)}"]`);
    const mine = shown.filter((t) => t.status === status);
    // Older done tasks the server left out (see loadBoard).
    const last = status === statuses[statuses.length - 1];
    const hidden = last && !fullDone() ? state.board.done_hidden || 0 : 0;
    const count = mine.length + hidden;
    col.querySelector("h2 .count").textContent = count ? String(count) : "";
    const more = hidden
      ? el("button", { class: "ghost more", onclick: () => { state.allDone = true; loadBoard(); } }, `Show ${hidden} older`)
      : null;
    col.querySelector(".cards").replaceChildren(...[more].filter(Boolean), ...mine.map((t) => card(t, all)));
  }
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

// Every word of the search must appear in the task's id, title, description or project.
function matches(t) {
  const words = state.query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return true;
  const text = `${key(t)} ${t.title} ${t.description} ${t.project} ${t.tags.join(" ")}`.toLowerCase();
  return words.every((w) => text.includes(w));
}

function card(t, showProject) {
  const node = el("div", { class: "card" + (state.task === t.id ? " open" : ""), draggable: "true", "data-id": t.id,
                           onclick: () => openTask(t.id) },
    el("div", { class: "head" },
      el("div", { class: "title" }, t.title),
      // On "All projects" a dot in the project's colour tells the projects apart.
      el("span", { class: "num", title: key(t) },
        showProject ? el("span", { class: "dot", style: `background:${projectColor(t.project)}` }) : null, `#${t.id}`)),
    t.description ? el("div", { class: "desc" }, plainText(t.description)) : null,
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

// "+ Add task" creates the task at once and opens it like any other, with its
// title selected to type over. Closed untouched, it is deleted again.
const NEW_TITLE = "New task";
let fresh = null; // id of the task just added, until something in it is saved

async function newTask() {
  const { projects } = state.board;
  if (!projects.length) { toast("Create a project first"); return; }
  const last = localStorageGet("kanzo.newTaskProject");
  const project = state.project !== "all" ? state.project
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
  $("#new-project-toggle").hidden = true;
  $("#new-project").hidden = false;
  $("#new-project [name=slug]").focus();
}

function hideNewProject() {
  $("#new-project").reset();
  $("#new-project").hidden = true;
  $("#new-project-toggle").hidden = false;
}

$("#new-project-toggle").addEventListener("click", showNewProject);
$("#new-project-cancel").addEventListener("click", hideNewProject);
$("#new-project").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = new FormData(e.target);
  try {
    const p = await api("POST", "/projects", { slug: f.get("slug"), name: f.get("name"), repo: f.get("repo") || null });
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
  document.querySelectorAll(".card.open").forEach((c) => c.classList.remove("open"));
  document.querySelector(`.card[data-id="${id}"]`)?.classList.add("open");
  $("#p-comment").value = "";
  $("#p-send").disabled = true;
  fit($("#p-comment"));
  updateHash();
  await refreshTask();
  if (state.task !== id) return;
  panel.classList.add("shown");
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
  updateHash();
  panel.classList.remove("shown");
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
  const { task, comments } = data;
  panelTask = task;
  $("#p-key").textContent = key(task);
  // Never overwrite what is being typed or picked.
  if (!$("#p-project").contains(document.activeElement)) renderProjectPick(task);
  for (const [field, value] of [[$("#p-title"), task.title], [$("#p-desc"), task.description], [$("#p-tags"), task.tags.join(", ")]]) {
    if (document.activeElement !== field) { field.value = value; fit(field); }
  }
  const status = $("#p-status");
  const statuses = state.board ? state.board.statuses : [task.status];
  status.replaceChildren(...statuses.map((s) => el("option", { value: s }, s)));
  if (!statuses.includes(task.status)) status.append(el("option", { value: task.status }, task.status));
  status.value = task.status;
  $("#p-created").replaceChildren(timeEl(task.created_at));
  $("#p-updated").replaceChildren(timeEl(task.updated_at));
  $("#p-comment-count").textContent = comments.length ? String(comments.length) : "";
  $("#p-comments").replaceChildren(...comments.map((c) => {
    const body = el("div", { class: "md" });
    body.innerHTML = markdown(c.body);
    return el("div", { class: "comment" },
      el("div", { class: "who" }, el("b", {}, c.author), " · ", timeEl(c.created_at),
        el("button", { class: "remove", title: "Delete comment", "aria-label": "Delete comment",
                       onclick: () => deleteComment(c) }, icon("trash"))),
      body);
  }));
}

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
  const projects = state.board ? state.board.projects.map((p) => p.slug) : [];
  if (!projects.includes(task.project)) projects.push(task.project);
  const select = el("select", { title: "Project", "aria-label": "Project" },
    ...projects.map((slug) => el("option", { value: slug }, slug)));
  select.value = task.project;
  select.addEventListener("change", () => {
    try { localStorage.setItem("kanzo.newTaskProject", select.value); } catch {}
    patchTask({ project: select.value });
  });
  $("#p-project").replaceChildren(el("label", { class: "tag project-pick" },
    el("span", { class: "dot", style: `background:${projectColor(task.project)}` }), select));
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
  } catch (e) { toast(e.message); refreshTask(); }
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
for (const field of [$("#p-title"), $("#p-desc")]) field.addEventListener("input", () => fit(field));

// ⌘↵ in the title or description saves and closes.
$("#p-title").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); closeTask(); }
  // The title is one line: Enter, like Tab, goes on to the description.
  else if ((e.key === "Enter" || e.key === "Tab") && !e.shiftKey) { e.preventDefault(); $("#p-desc").focus(); }
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
});

$("#p-status").addEventListener("change", (e) => patchTask({ status: e.target.value }));

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
  if (!body || !panelTask) return;
  try {
    await api("POST", `/tasks/${panelTask.id}/comments`, { body });
    if (fresh === panelTask.id) fresh = null;
    $("#p-comment").value = "";
    $("#p-send").disabled = true;
    fit($("#p-comment"));
    await refreshTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});
// The send button is live only once there is something to send.
$("#p-comment").addEventListener("input", (e) => { $("#p-send").disabled = !e.target.value.trim(); fit(e.target); });
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

$("#search").addEventListener("input", (e) => {
  const wasFull = fullDone();
  state.query = e.target.value;
  // Searching needs every done task; load them when a search starts.
  if (fullDone() !== wasFull) loadBoard();
  else if (state.board) renderColumns();
});
$("#search").addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    e.stopPropagation();
    e.target.value = "";
    e.target.dispatchEvent(new Event("input"));
    e.target.blur();
  }
});

// ---------- keyboard, polling, start ----------

document.addEventListener("keydown", (e) => {
  const typing = e.target.closest("input, textarea, select");
  if (e.key === "Escape" && state.menu) { closeMenu(); return; }
  if (e.key === "Escape" && state.task && !typing) closeTask();
  if (e.key === "/" && !typing && !e.metaKey && !e.ctrlKey) {
    e.preventDefault();
    $("#search").focus();
  }
  if (e.key === "n" && !typing && !e.metaKey && !e.ctrlKey) {
    if ($("#columns .add")) { e.preventDefault(); newTask(); }
  }
});

window.addEventListener("hashchange", () => {
  const { slug, task } = readHash();
  if ((slug || "all") !== state.project) setProject(slug || "all");
  if (task && task !== state.task) openTask(task);
  else if (!task && state.task) closeTask();
});

// Agents change the board through `kanzo mcp`; a cheap counter tells us when.
async function poll() {
  if (document.hidden) return schedulePoll();
  try {
    const { changes } = await api("GET", "/changes");
    $("#offline").hidden = true;
    if (changes !== state.changes) {
      state.changes = changes;
      await loadBoard();
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

// Relative times ("5m ago") drift; refresh them now and then.
setInterval(() => {
  document.querySelectorAll("time[datetime]").forEach((t) => { t.textContent = ago(t.getAttribute("datetime")); });
}, 60000);

{
  const { slug, task } = readHash();
  state.project = slug || localStorageGet("kanzo.project") || "all";
  loadBoard().then(() => { if (task) openTask(task); });
}
