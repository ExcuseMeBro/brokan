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

let toastTimer;
function toast(message) {
  document.querySelector(".toast")?.remove();
  const t = el("div", { class: "toast" }, message);
  document.body.append(t);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.remove(), 3500);
}

function projectColor(slug) {
  let h = 0;
  for (const c of slug) h = (h * 31 + c.charCodeAt(0)) % 360;
  return `hsl(${h} 55% 52%)`;
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
    const board = await api("GET", "/board?project=" + encodeURIComponent(state.project));
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
  const link = (slug, name, count, color) =>
    el("a", { href: "#" + slug, class: state.project === slug ? "active" : null,
              onclick: (e) => { e.preventDefault(); setProject(slug); } },
      color ? el("span", { class: "dot", style: `background:${color}` }) : null,
      el("span", { class: "name" }, name),
      el("span", { class: "count" }, String(count)));
  $("#nav").replaceChildren(
    link("all", "All projects", open),
    projects.length ? el("div", { class: "label" }, "Projects") : null,
    ...projects.map((p) => link(p.slug, p.name, p.open, projectColor(p.slug))),
  );
}

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
      el("span", { class: "sub" }, `${tasks.length} tasks across ${projects.length} projects`));
    return;
  }
  header.replaceChildren(
    el("h1", {}, p.name),
    el("span", { class: "sub mono" }, p.slug),
    p.repo ? el("span", { class: "sub" }, el("a", { href: `https://github.com/${p.repo}`, target: "_blank", rel: "noopener" }, p.repo)) : null,
  );
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
          el("span", { class: "count" })),
        el("div", { class: "cards" }),
      );
      if (i === 0) col.append(addTaskForm());
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
    col.querySelector("h2 .count").textContent = mine.length ? String(mine.length) : "";
    col.querySelector(".cards").replaceChildren(...mine.map((t) => card(t, all)));
  }
  const select = columns.querySelector(".add select");
  if (select) fillProjectSelect(select);
}

// Every word of the search must appear in the task's id, title, description or project.
function matches(t) {
  const words = state.query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return true;
  const text = `${key(t)} ${t.title} ${t.description} ${t.project}`.toLowerCase();
  return words.every((w) => text.includes(w));
}

function card(t, showProject) {
  const node = el("div", { class: "card" + (state.task === t.id ? " open" : ""), draggable: "true", "data-id": t.id,
                           onclick: () => openTask(t.id) },
    el("div", { class: "title" }, t.title),
    el("div", { class: "meta" },
      el("span", { class: "mono" }, key(t)),
      showProject ? el("span", { class: "tag" }, el("span", { class: "dot", style: `background:${projectColor(t.project)}` }), t.project) : null,
      el("span", { class: "spacer" }),
      t.description ? el("span", { title: "Has a description" }, "≡") : null,
      t.comments ? el("span", { title: `${t.comments} comments` }, `💬 ${t.comments}`) : null,
    ));
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
    const before = after ? Number(after.dataset.id) : null;
    try {
      await api("POST", `/tasks/${id}/move`, { status, before });
      if (state.task === id) refreshTask();
    } catch (err) { toast(err.message); }
    loadBoard();
  });
}

function fillProjectSelect(select) {
  const { projects } = state.board;
  const keep = select.value || localStorageGet("kanzo.newTaskProject");
  select.replaceChildren(...projects.map((p) => el("option", { value: p.slug }, p.name)));
  if (projects.some((p) => p.slug === keep)) select.value = keep;
}

function localStorageGet(k) {
  try { return localStorage.getItem(k); } catch { return null; }
}

function addTaskForm() {
  const wrap = el("div", { class: "add" });
  const toggle = el("button", { class: "ghost", onclick: () => { toggle.hidden = true; form.hidden = false; title.focus(); } }, "+ Add task");
  const title = el("textarea", { class: "field", placeholder: "Task title", rows: "2" });
  const select = state.project === "all" ? el("select", { class: "field", title: "Project" }) : null;
  const close = () => { form.hidden = true; toggle.hidden = false; title.value = ""; };
  const form = el("form", { hidden: true },
    title,
    el("div", { class: "row" },
      select,
      el("button", { class: "btn primary", type: "submit" }, "Add"),
      el("button", { class: "btn", type: "button", onclick: close }, "Cancel")));
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const text = title.value.trim();
    if (!text) return;
    const project = select ? select.value : state.project;
    if (!project) { toast("Create a project first"); return; }
    try {
      if (select) try { localStorage.setItem("kanzo.newTaskProject", project); } catch {}
      await api("POST", "/tasks", { project, title: text });
      title.value = "";
      title.focus();
      loadBoard();
    } catch (err) { toast(err.message); }
  });
  title.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); form.requestSubmit(); }
    if (e.key === "Escape") { e.stopPropagation(); close(); }
  });
  wrap.append(toggle, form);
  wrap.open = () => toggle.click();
  return wrap;
}

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

const panel = $("#panel");
let panelTask = null;

async function openTask(id) {
  state.task = id;
  document.querySelectorAll(".card.open").forEach((c) => c.classList.remove("open"));
  document.querySelector(`.card[data-id="${id}"]`)?.classList.add("open");
  closeDescEditor();
  $("#p-comment").value = "";
  updateHash();
  await refreshTask();
  if (state.task !== id) return;
  panel.classList.add("shown");
  panel.setAttribute("aria-hidden", "false");
}

function closeTask() {
  state.task = null;
  panelTask = null;
  updateHash();
  panel.classList.remove("shown");
  panel.setAttribute("aria-hidden", "true");
  document.querySelectorAll(".card.open").forEach((c) => c.classList.remove("open"));
}

async function refreshTask() {
  if (!state.task) return;
  let data;
  try {
    data = await api("GET", `/tasks/${state.task}`);
  } catch (e) {
    if (/no task/.test(e.message)) { toast("That task was deleted"); closeTask(); }
    return;
  }
  const { task, comments } = data;
  panelTask = task;
  $("#p-key").textContent = key(task);
  $("#p-project").replaceChildren(el("span", { class: "tag" },
    el("span", { class: "dot", style: `background:${projectColor(task.project)}` }), task.project));
  const title = $("#p-title");
  if (document.activeElement !== title) { title.value = task.title; fitTitle(); }
  const status = $("#p-status");
  const statuses = state.board ? state.board.statuses : [task.status];
  status.replaceChildren(...statuses.map((s) => el("option", { value: s }, s)));
  if (!statuses.includes(task.status)) status.append(el("option", { value: task.status }, task.status));
  status.value = task.status;
  $("#p-created").replaceChildren(timeEl(task.created_at));
  $("#p-updated").replaceChildren(timeEl(task.updated_at));
  if ($("#p-desc-editor").hidden) renderDesc(task.description);
  $("#p-comment-count").textContent = comments.length ? String(comments.length) : "";
  $("#p-comments").replaceChildren(...comments.map((c) => {
    const body = el("div", { class: "md" });
    body.innerHTML = markdown(c.body);
    return el("div", { class: "comment" }, el("div", { class: "who" }, el("b", {}, c.author), " · ", timeEl(c.created_at)), body);
  }));
}

function renderDesc(text) {
  const view = $("#p-desc-view");
  if (text) view.innerHTML = markdown(text);
  else view.replaceChildren(el("span", { class: "placeholder" }, "No description. Click to add one."));
}

function fitTitle() {
  const t = $("#p-title");
  t.style.height = "auto";
  t.style.height = t.scrollHeight + "px";
}

async function patchTask(fields) {
  if (!panelTask) return;
  try {
    await api("PATCH", `/tasks/${panelTask.id}`, fields);
    await refreshTask();
    loadBoard();
  } catch (e) { toast(e.message); refreshTask(); }
}

$("#p-close").addEventListener("click", closeTask);
$("#p-title").addEventListener("input", fitTitle);
$("#p-title").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); e.target.blur(); }
  if (e.key === "Escape") { e.stopPropagation(); e.target.value = panelTask.title; e.target.blur(); }
});
$("#p-title").addEventListener("blur", (e) => {
  const v = e.target.value.trim();
  if (panelTask && v && v !== panelTask.title) patchTask({ title: v });
  else if (panelTask) { e.target.value = panelTask.title; fitTitle(); }
});
$("#p-status").addEventListener("change", (e) => patchTask({ status: e.target.value }));

function openDescEditor() {
  $("#p-desc").value = panelTask ? panelTask.description : "";
  $("#p-desc-view").hidden = true;
  $("#p-desc-edit").hidden = true;
  $("#p-desc-editor").hidden = false;
  $("#p-desc").focus();
}

function closeDescEditor() {
  $("#p-desc-editor").hidden = true;
  $("#p-desc-view").hidden = false;
  $("#p-desc-edit").hidden = false;
  if (panelTask) renderDesc(panelTask.description);
}

$("#p-desc-edit").addEventListener("click", openDescEditor);
$("#p-desc-view").addEventListener("click", (e) => { if (!e.target.closest("a")) openDescEditor(); });
$("#p-desc-cancel").addEventListener("click", closeDescEditor);
$("#p-desc-save").addEventListener("click", async () => {
  const description = $("#p-desc").value;
  closeDescEditor();
  await patchTask({ description });
});
$("#p-desc").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); $("#p-desc-save").click(); }
  if (e.key === "Escape") { e.stopPropagation(); closeDescEditor(); }
});

$("#p-comment-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const body = $("#p-comment").value.trim();
  if (!body || !panelTask) return;
  try {
    await api("POST", `/tasks/${panelTask.id}/comments`, { body });
    $("#p-comment").value = "";
    await refreshTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});
$("#p-comment").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); $("#p-comment-form").requestSubmit(); }
});

$("#p-delete").addEventListener("click", async () => {
  if (!panelTask) return;
  const extra = panelTask.comments ? ` and its ${panelTask.comments} comment${panelTask.comments > 1 ? "s" : ""}` : "";
  if (!confirm(`Delete ${key(panelTask)}${extra}? This cannot be undone.`)) return;
  try {
    await api("DELETE", `/tasks/${panelTask.id}`);
    closeTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});

// ---------- search ----------

$("#search").addEventListener("input", (e) => {
  state.query = e.target.value;
  if (state.board) renderColumns();
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
  if (e.key === "Escape" && state.task && !typing) closeTask();
  if (e.key === "/" && !typing && !e.metaKey && !e.ctrlKey) {
    e.preventDefault();
    $("#search").focus();
  }
  if (e.key === "n" && !typing && !e.metaKey && !e.ctrlKey) {
    const add = $("#columns .add");
    if (add) { e.preventDefault(); add.open(); }
  }
});

window.addEventListener("hashchange", () => {
  const { slug, task } = readHash();
  if ((slug || "all") !== state.project) setProject(slug || "all");
  if (task && task !== state.task) openTask(task);
  else if (!task && state.task) closeTask();
});

// Agents change the board through `kanzo mcp`; a cheap counter tells us when.
setInterval(async () => {
  if (document.hidden) return;
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
}, 2000);

// Relative times ("5m ago") drift; refresh them now and then.
setInterval(() => {
  document.querySelectorAll("time[datetime]").forEach((t) => { t.textContent = ago(t.getAttribute("datetime")); });
}, 60000);

{
  const { slug, task } = readHash();
  state.project = slug || localStorageGet("kanzo.project") || "all";
  loadBoard().then(() => { if (task) openTask(task); });
}
