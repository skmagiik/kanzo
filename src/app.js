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
const BLOCKED_COLOR = "#e03131";
const CUSTOM_COLOR = "#ae3ec9";
const KIND_LABELS = { human: "Human", orchestrator: "Orchestrator", agent: "Task agent" };
// The assignee that queues a task for AI agents; an orchestrator hands it to a task agent.
const AI_AGENTS = "AI Agents";

const state = {
  project: "all",
  board: null,
  changes: null,
  task: null,
  dragging: null,
  dropped: false,
  layout: "",
  allDone: false,
  menu: null,
  net: null,
  guests: null,
  selected: new Set(),
  sweeping: false,
  // The board's build: when it restarts into a rebuilt Kanzo, the page reloads with it.
  build: null,
};

async function api(method, path, body) {
  const image = body instanceof Blob;
  const res = await fetch("/api" + path, {
    method,
    headers: body ? { "Content-Type": image ? body.type : "application/json" } : {},
    body: image || !body ? body : JSON.stringify(body),
  });
  noticeVersion(res.headers.get("x-kanzo-version"));
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw Object.assign(new Error(data.error || res.statusText), { data });
  return data;
}

// An update restarts the board under the page, which then reloads to match it. Not while
// someone types, though: the next reply tries again.
let pageVersion = null;

function noticeVersion(version) {
  if (!version) return;
  if (pageVersion === null) pageVersion = version;
  if (version !== pageVersion && !document.activeElement?.matches("input, textarea, select")) location.reload();
}

function plural(n, word) {
  // In zh-CN the noun's translation carries its measure word: 个任务, 条评论, 台电脑.
  const w = DICT?.[word] ?? `${word}${n === 1 ? "" : "s"}`;
  return `${n} ${w}`;
}

// Storage can be off, as in private windows.
function localStorageGet(k) {
  try { return localStorage.getItem(k); } catch { return null; }
}

function localStorageSet(k, v) {
  try { localStorage.setItem(k, v); } catch {}
}

// The board's language: a saved choice wins, else the browser's. English is the
// source strings, so it runs with no dictionary at all.
const LANG = (() => {
  const saved = localStorageGet("kanzo.lang");
  if (saved === "zh-cn" || saved === "en") return saved;
  return (navigator.language || "").toLowerCase().startsWith("zh") ? "zh-cn" : "en";
})();

let DICT = null;

// Looks a string up in the dictionary; {0}, {1}… fill in the pieces that move
// around in another language. Without a dictionary every string is itself.
function tr(s, ...args) {
  const out = LANG === "en" || !DICT ? s : (DICT[s] ?? s);
  return out.replace(/\{(\d+)\}/g, (_, i) => args[i] ?? "");
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

// From Lucide, ISC licence.
const ICONS = {
  newProject: '<path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.4 2.6a2.1 2.1 0 0 1 3 3L12 15l-4 1 1-4Z"/>',
  template: '<rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2"/>',
  board: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M8 7v7M12 7v4M16 7v9"/>',
  columns: '<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M9 3v18M15 3v18"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>',
  close: '<path d="M18 6 6 18M6 6l12 12"/>',
  more: '<circle cx="5" cy="12" r="1"/><circle cx="12" cy="12" r="1"/><circle cx="19" cy="12" r="1"/>',
  plus: '<path d="M5 12h14M12 5v14"/>',
  list: '<path d="M4 6h16M4 12h16M4 18h16"/>',
  trash: '<path d="M3 6h18M8 6V4h8v2M19 6l-1 14H6L5 6M10 11v6M14 11v6"/>',
  check: '<path d="M20 6 9 17l-5-5"/>',
  moved: '<path d="M5 12h14M12 5l7 7-7 7"/>',
  copy: '<rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h10a2 2 0 0 1 2 2"/>',
  link: '<path d="M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7"/><path d="M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7"/>',
  human: '<circle cx="12" cy="8" r="4"/><path d="M5 21v-1a6 6 0 0 1 6-6h2a6 6 0 0 1 6 6v1"/>',
  url: '<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>',
  github: '<path d="M15 22v-4a4.8 4.8 0 0 0-1-3.5c3 0 6-2 6-5.5.1-1.3-.3-2.5-1-3.5.3-1.2.3-2.4 0-3.5 0 0-1 0-3 1.5a12 12 0 0 0-8 0C6 2 5 2 5 2c-.3 1.2-.3 2.4 0 3.5A5.4 5.4 0 0 0 4 9c0 3.5 3 5.5 6 5.5-.4.5-.7 1-.8 1.7-.2.6-.3 1.2-.2 1.8v4"/><path d="M9 18c-4.5 2-5-2-7-2"/>',
  sync: '<path d="M12 13v8"/><path d="M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 2.5 8.2"/><path d="m8 17 4-4 4 4"/>',
  collapse: '<path d="m13 17 5-5-5-5M6 17l5-5-5-5"/>',
  expand: '<path d="m11 17-5-5 5-5M18 17l-5-5 5-5"/>',
  rename: '<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
  update: '<circle cx="12" cy="12" r="9"/><path d="M12 7v9M8 12l4 4 4-4"/>',
  settings: '<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>',
  send: '<path d="M12 19V5M5 12l7-7 7 7"/>',
  info: '<circle cx="12" cy="12" r="9"/><path d="M12 16v-4M12 8h.01"/>',
  sidebar: '<rect x="3" y="4" width="18" height="16" rx="4"/><path d="M9 4v16"/>',
  waits: '<path d="M6 3h12M6 21h12M7 3v4l5 5 5-5V3M7 21v-4l5-5 5 5v4"/>',
  news: '<path d="M12 3l2.6 5.6 6.1.7-4.5 4.2 1.2 6-5.4-3-5.4 3 1.2-6-4.5-4.2 6.1-.7z"/>',
  comment: '<path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z"/>',
  share: '<circle cx="18" cy="5" r="3"/><circle cx="6" cy="12" r="3"/><circle cx="18" cy="19" r="3"/><path d="m8.6 13.5 6.8 4M15.4 6.5l-6.8 4"/>',
  bot: '<path d="M12 8V4H8"/><rect width="16" height="12" x="4" y="8" rx="2"/><path d="M2 14h2M20 14h2M15 13v2M9 13v2"/>',
  eye: '<path d="M2.06 12.35a1 1 0 0 1 0-.7 10.75 10.75 0 0 1 19.88 0 1 1 0 0 1 0 .7 10.75 10.75 0 0 1-19.88 0"/><circle cx="12" cy="12" r="3"/>',
  flag: '<path d="M4 15s1-1 4-1 5 2 8 2 4-1 4-1V3s-1 1-4 1-5-2-8-2-4 1-4 1z"/><path d="M4 22v-7"/>',
  star: '<path d="m12 2 3.09 6.26L22 9.27l-5 4.87 1.18 6.88L12 17.77l-6.18 3.25L7 14.14 2 9.27l6.91-1.01z"/>',
  bug: '<path d="m8 2 1.88 1.88M14.12 3.88 16 2M9 7.13v-1a3 3 0 1 1 6 0v1"/><path d="M12 20c-3.3 0-6-2.7-6-6v-3a4 4 0 0 1 4-4h4a4 4 0 0 1 4 4v3c0 3.3-2.7 6-6 6M12 20v-9M6.53 9C4.6 8.8 3 7.1 3 5M6 13H2M3 21c0-2.1 1.7-3.9 3.8-4M20.97 5c0 2.1-1.6 3.8-3.5 4M22 13h-4M17.2 17c2.1.1 3.8 1.9 3.8 4"/>',
  rocket: '<path d="M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z"/><path d="m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z"/><path d="M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5"/>',
  clock: '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>',
  shield: '<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/>',
  beaker: '<path d="M4.5 3h15M6 3v16a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V3M6 14h12"/>',
  chevron: '<path d="m6 9 6 6 6-6"/>',
};

function icon(name, color) {
  const span = el("span", { class: "icon", style: color ? `color:${color}` : null });
  span.innerHTML = `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[name]}</svg>`;
  return span;
}

document.querySelectorAll("[data-icon]").forEach((n) => n.replaceWith(icon(n.dataset.icon)));

let toastTimer;
// The clipboard API needs a secure page; a board opened from another device over plain
// HTTP falls back.
async function copyKey(text, message = tr("Copied {0}", text)) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const area = el("textarea", { style: "position:fixed;opacity:0" });
    area.value = text;
    document.body.append(area);
    area.select();
    document.execCommand("copy");
    area.remove();
  }
  toast(message);
}

function toast(message) {
  document.querySelector(".toast")?.remove();
  const t = el("div", { class: "toast" }, message);
  document.body.append(t);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.remove(), 3500);
}

// The in-page confirm(): resolves true when the primary button is chosen, false on Cancel, Escape or a backdrop click.
let confirmDone = null;
function askConfirm({ title, message = "", confirm = "OK", cancel = "Cancel", danger = false }) {
  if (confirmDone) finishConfirm(false);
  const back = document.activeElement;
  const ok = $("#confirm-ok");
  $("#confirm-title").textContent = title;
  $("#confirm-message").textContent = message;
  $("#confirm-message").hidden = !message;
  $("#confirm-cancel").textContent = cancel;
  ok.textContent = confirm;
  ok.classList.toggle("danger", danger);
  $("#confirm-dialog").hidden = false;
  ok.focus();
  return new Promise((resolve) => {
    confirmDone = (yes) => {
      $("#confirm-dialog").hidden = true;
      if (back && back.focus && document.contains(back)) back.focus();
      resolve(yes);
    };
  });
}

function finishConfirm(yes) {
  const done = confirmDone;
  confirmDone = null;
  if (done) done(yes);
}

$("#confirm-ok").addEventListener("click", () => finishConfirm(true));
$("#confirm-cancel").addEventListener("click", () => finishConfirm(false));
// Keys stay in the dialog, so board shortcuts don't fire under it; Tab keeps to its two buttons.
$("#confirm-dialog").addEventListener("keydown", (e) => {
  e.stopPropagation();
  if (e.key === "Enter") { e.preventDefault(); finishConfirm(document.activeElement !== $("#confirm-cancel")); }
  else if (e.key === "Tab") {
    e.preventDefault();
    (document.activeElement === $("#confirm-ok") ? $("#confirm-cancel") : $("#confirm-ok")).focus();
  }
});

// The names match PROJECT_COLORS in store.rs.
const PROJECT_COLORS = {
  gray: "#8e8e93", red: "#ef4444", orange: "#f97316", yellow: "#eab308", green: "#22c55e",
  teal: "#14b8a6", blue: "#3b82f6", purple: "#a855f7", pink: "#ec4899",
};

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

// Projects' own statuses, from the board.
function customColumns() {
  return (state.board && state.board.columns) || [];
}

function isCustom(status) {
  return customColumns().some((c) => c.name === status);
}

// The statuses a task of `slug` may take, in board order: the base ones and the project's own.
function projectStatuses(slug) {
  if (!state.board) return [];
  const mine = customColumns().filter((c) => c.project === slug).map((c) => c.name);
  const statuses = state.board.statuses.filter((s) => !isCustom(s) || mine.includes(s));
  // Viewing another project, its columns are not on the board; they still sit before done.
  const missing = mine.filter((s) => !statuses.includes(s));
  const done = statuses.indexOf("done");
  if (missing.length) statuses.splice(done < 0 ? statuses.length : done, 0, ...missing);
  return statuses;
}

// What a custom column is for, shown on its header.
function columnDescription(status) {
  const defs = customColumns().filter((c) => c.name === status && (state.project === "all" || c.project === state.project));
  if (state.project !== "all") return defs.map((c) => c.description).join("\n");
  return defs.map((c) => `${c.project}: ${c.description}`).join("\n");
}

// The icons a custom column may wear, in picker order, as the server allows them; person and chat reuse ICONS.
const COLUMN_ICON_NAMES = ["dot", "eye", "flag", "star", "check", "bug", "rocket", "clock", "person", "chat", "shield", "beaker"];
const COLUMN_ICONS = { person: "human", chat: "comment" };

// The icon a custom column shows: this project's, or in All projects the first that has one.
function columnIconName(status) {
  const defs = customColumns().filter((c) => c.name === status);
  const mine = defs.filter((c) => c.project === state.project);
  const c = (mine.length ? mine : defs).find((d) => d.icon);
  return c ? c.icon : null;
}

// A custom column's icon, small and in CUSTOM_COLOR; dot or none is the dotted ring.
function columnIconMarkup(name) {
  if (!name || name === "dot" || !COLUMN_ICON_NAMES.includes(name)) {
    return `<svg viewBox="0 0 14 14" aria-hidden="true"><circle cx="7" cy="7" r="6" fill="none" stroke="${CUSTOM_COLOR}" stroke-width="1.6" stroke-linecap="round" stroke-dasharray="0.01 3.14"/>` +
      `<circle cx="7" cy="7" r="2.4" fill="${CUSTOM_COLOR}"/></svg>`;
  }
  return `<svg viewBox="0 0 24 24" fill="none" stroke="${CUSTOM_COLOR}" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${ICONS[COLUMN_ICONS[name] || name]}</svg>`;
}

function columnIcon(name) {
  const span = el("span", { class: "status-icon" });
  span.innerHTML = columnIconMarkup(name);
  return span;
}

// A column icon picker: a button wearing the picked icon that opens a grid of them; its value, "" for default, is the name.
function iconPicker(button, value, onPick) {
  button.type = "button";
  button.classList.add("icon-pick");
  button.setAttribute("aria-haspopup", "true");
  button.setAttribute("aria-expanded", "false");
  setIconPick(button, value);
  button.addEventListener("click", () => openIconPick(button, onPick));
  return button;
}

function setIconPick(button, value) {
  button.value = value || "";
  button.replaceChildren(columnIcon(button.value), icon("chevron"));
}

const ICON_GRID_COLUMNS = 5;

function openIconPick(button, onPick) {
  if (!toggleMenu(button)) return;
  button.setAttribute("aria-expanded", "true");
  const cells = ["", ...COLUMN_ICON_NAMES].map((n) => {
    const label = n ? n[0].toUpperCase() + n.slice(1) : "Default";
    const current = n === button.value;
    return el("button", { type: "button", class: "icon-cell" + (current ? " current" : ""), role: "menuitemradio",
                          title: label, "aria-label": label, "aria-checked": String(current),
                          onclick: () => { closeMenu(); button.focus(); setIconPick(button, n); if (onPick) onPick(n); } },
      columnIcon(n));
  });
  const menu = el("div", { class: "menu icon-grid", role: "menu", "aria-label": button.getAttribute("aria-label") }, cells);
  menu.addEventListener("keydown", (e) => {
    if (e.key === "Escape") { e.stopPropagation(); closeMenu(); button.focus(); return; }
    const at = cells.indexOf(document.activeElement);
    const step = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -ICON_GRID_COLUMNS, ArrowDown: ICON_GRID_COLUMNS }[e.key];
    const to = e.key === "Home" ? 0 : e.key === "End" ? cells.length - 1 : step ? at + step : -1;
    if (to < 0 || to >= cells.length) return;
    e.preventDefault();
    cells[to].focus();
  });
  document.body.append(menu);
  const r = button.getBoundingClientRect();
  const below = r.bottom + 4 + menu.offsetHeight <= innerHeight;
  menu.style.top = `${below ? r.bottom + 4 : r.top - 4 - menu.offsetHeight}px`;
  menu.style.left = `${Math.max(8, Math.min(r.left, innerWidth - menu.offsetWidth - 8))}px`;
  (cells.find((c) => c.classList.contains("current")) || cells[0]).focus();
}

// Every <select> wears a button and a floating list in the parent picker's look. The select stays the source of
// truth: code keeps setting its value and options, and a pick sets its value and fires its change event.
const SELECT_VALUE = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value");
const DROPDOWN_HEIGHT = 232;
const dropdowns = new WeakMap();
let dropdownCount = 0;
let closeOpenDropdown = null;

// The control a select shows as: its button once enhanced.
function dropdownButton(select) {
  return dropdowns.get(select) || select;
}

function dropdown(select, opts = {}) {
  if (select.hasAttribute("data-dropdown")) return dropdownButton(select);
  const id = `dropdown-${++dropdownCount}`;
  select.setAttribute("data-dropdown", id);
  const button = el("button", { type: "button", "aria-haspopup": "listbox", "aria-expanded": "false" });
  const iconOf = (value) => (opts.icon ? opts.icon(value) : null);
  let list = null, rows = [], values = [], cursor = -1, typed = "", typedAt = 0;

  const refresh = () => {
    const option = select.options[select.selectedIndex];
    button.className = `dropdown-button ${select.className}`.trim();
    button.disabled = select.disabled;
    button.hidden = select.hidden;
    for (const name of ["title", "aria-label"]) {
      const v = select.getAttribute(name);
      if (v == null) button.removeAttribute(name);
      else button.setAttribute(name, v);
    }
    const chevron = icon("chevron");
    chevron.classList.add("chevron");
    button.replaceChildren(...[option ? iconOf(option.value) : null,
      el("span", { class: "label" }, option ? option.textContent : ""), chevron].filter(Boolean));
    if (list) {
      if (button.disabled || button.hidden) close();
      else { draw(); place(); }
    }
  };

  // The keyboard cursor stays on the same value when the options are rebuilt under an open list.
  const draw = () => {
    const keep = cursor >= 0 ? values[cursor] : select.value;
    const options = Array.from(select.options);
    values = options.map((o) => o.value);
    rows = options.map((o, i) => {
      const current = i === select.selectedIndex;
      return el("button", { type: "button", tabindex: "-1", id: `${id}-${i}`, role: "option", disabled: o.disabled,
                            class: "dropdown-option" + (current ? " current" : ""), "aria-selected": String(current),
                            onmousemove: () => { if (i !== cursor) move(i, false); }, onclick: () => pick(i) },
        iconOf(o.value), el("span", { class: "title" }, o.textContent),
        current ? el("span", { class: "check" }, icon("check")) : null);
    });
    list.replaceChildren(...(rows.length ? rows : [el("div", { class: "note" }, "Nothing to pick.")]));
    cursor = -1;
    const at = values.indexOf(keep);
    move(at >= 0 && !options[at].disabled ? at : next(-1, 1), true);
  };

  // The next enabled option from `from` in direction `dir`, or `from` itself at either end.
  const next = (from, dir) => {
    const options = select.options;
    for (let i = from + dir; i >= 0 && i < options.length; i += dir) if (!options[i].disabled) return i;
    return from;
  };

  const move = (i, reveal) => {
    if (!rows[i]) return;
    if (rows[cursor]) rows[cursor].classList.remove("selected");
    cursor = i;
    rows[i].classList.add("selected");
    button.setAttribute("aria-activedescendant", rows[i].id);
    if (!reveal) return;
    const row = rows[i];
    if (row.offsetTop < list.scrollTop) list.scrollTop = row.offsetTop - 4;
    else if (row.offsetTop + row.offsetHeight > list.scrollTop + list.clientHeight) {
      list.scrollTop = row.offsetTop + row.offsetHeight - list.clientHeight + 4;
    }
  };

  // Under the button, or above it when there is more room there; it follows the button as the page scrolls.
  const place = () => {
    if (!button.isConnected || !button.getClientRects().length) { close(); return; }
    const r = button.getBoundingClientRect();
    list.style.minWidth = `${r.width}px`;
    list.style.maxHeight = "";
    const natural = Math.min(list.offsetHeight, DROPDOWN_HEIGHT);
    const below = innerHeight - r.bottom - 12, above = r.top - 12;
    const down = below >= natural || below >= above;
    list.style.maxHeight = `${Math.max(80, Math.min(DROPDOWN_HEIGHT, down ? below : above))}px`;
    list.style.top = `${down ? r.bottom + 4 : r.top - 4 - list.offsetHeight}px`;
    list.style.left = `${Math.max(8, Math.min(r.left, innerWidth - list.offsetWidth - 8))}px`;
  };

  const onScroll = (e) => { if (list && e.target !== list) place(); };
  const onResize = () => close();
  const onOutside = (e) => {
    if (!button.contains(e.target) && !(list && list.contains(e.target))) close();
  };

  const open = () => {
    if (list || button.disabled) return;
    if (closeOpenDropdown) closeOpenDropdown();
    list = el("div", { id: `${id}-list`, role: "listbox", "aria-label": select.getAttribute("aria-label") || select.title || null,
                       class: "dropdown-list floating" + (select.closest(".search-dialog") ? " over-dialog" : ""),
                       onmousedown: (e) => e.preventDefault() });
    list.style.textTransform = getComputedStyle(button).textTransform;
    document.body.append(list);
    cursor = -1;
    draw();
    place();
    if (!list) return;
    move(cursor, true);
    button.setAttribute("aria-expanded", "true");
    button.setAttribute("aria-controls", list.id);
    closeOpenDropdown = close;
    addEventListener("scroll", onScroll, true);
    addEventListener("resize", onResize);
    document.addEventListener("mousedown", onOutside, true);
  };

  function close(refocus) {
    if (!list) return;
    list.remove();
    list = null;
    rows = [];
    cursor = -1;
    button.setAttribute("aria-expanded", "false");
    button.removeAttribute("aria-activedescendant");
    button.removeAttribute("aria-controls");
    if (closeOpenDropdown === close) closeOpenDropdown = null;
    removeEventListener("scroll", onScroll, true);
    removeEventListener("resize", onResize);
    document.removeEventListener("mousedown", onOutside, true);
    if (refocus === true) button.focus();
  }

  const toggle = () => {
    button.focus();
    if (list) close();
    else open();
  };

  // Like the native list, a pick of the value it already has changes nothing.
  const pick = (i) => {
    const option = select.options[i];
    if (!option || option.disabled) return;
    close(true);
    if (i === select.selectedIndex) return;
    select.value = option.value;
    select.dispatchEvent(new Event("input", { bubbles: true }));
    select.dispatchEvent(new Event("change", { bubbles: true }));
  };

  // Letters typed in quick succession find the next option whose label starts with them.
  const typeAhead = (ch) => {
    const now = Date.now();
    typed = (now - typedAt < 700 ? typed : "") + ch.toLowerCase();
    typedAt = now;
    const n = select.options.length;
    const start = typed.length > 1 ? Math.max(cursor, 0) : cursor + 1;
    for (let k = 0; k < n; k++) {
      const i = (start + k) % n, o = select.options[i];
      if (!o.disabled && o.textContent.trim().toLowerCase().startsWith(typed)) { move(i, true); return; }
    }
  };

  button.addEventListener("keydown", (e) => {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    const letter = e.key.length === 1 && e.key !== " ";
    if (!list) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp" || e.key === "Enter" || letter) {
        e.preventDefault();
        open();
        if (letter) typeAhead(e.key);
      }
      return;
    }
    if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); close(true); }
    else if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); move(next(cursor, e.key === "ArrowDown" ? 1 : -1), true); }
    else if (e.key === "Home") { e.preventDefault(); move(next(-1, 1), true); }
    else if (e.key === "End") { e.preventDefault(); move(next(select.options.length, -1), true); }
    else if (e.key === "Enter") { e.preventDefault(); pick(cursor); }
    else if (letter) { e.preventDefault(); typeAhead(e.key); }
  });
  // Space clicks the button: it opens the list, or picks from it once open. A mouse click toggles it.
  button.addEventListener("click", (e) => {
    if (list && e.detail === 0) pick(cursor);
    else toggle();
  });
  button.addEventListener("blur", () => close());
  // A label around the select opens the list instead of reaching for the hidden select.
  const label = select.closest("label");
  if (label) label.addEventListener("click", (e) => { if (!button.contains(e.target)) { e.preventDefault(); toggle(); } });

  Object.defineProperty(select, "value", {
    configurable: true,
    get() { return SELECT_VALUE.get.call(this); },
    set(v) { SELECT_VALUE.set.call(this, v); refresh(); },
  });
  new MutationObserver(refresh).observe(select, {
    childList: true, subtree: true, characterData: true,
    attributes: true, attributeFilter: ["class", "disabled", "hidden", "title", "aria-label"],
  });
  if (select.form) select.form.addEventListener("reset", () => setTimeout(refresh));
  dropdowns.set(select, button);
  select.after(button);
  refresh();
  return button;
}

// Icons for the selects that wear them, by id.
const DROPDOWN_ICONS = {
  "p-status": (s) => (s ? statusIcon(s) : null),
  "p-comment-status": (s) => (s ? statusIcon(s) : null),
  "p-assignee": (name) => (name ? icon(isBot(name) ? "bot" : "human") : null),
};

function enhanceSelects(root) {
  const found = root.matches("select") ? [root] : root.querySelectorAll("select");
  for (const select of found) {
    if (!select.hasAttribute("data-dropdown")) dropdown(select, { icon: DROPDOWN_ICONS[select.id] });
  }
}

// Blocked and projects' own statuses sit off the todo-to-done progression, so the steps skip them.
function statusStep(status) {
  const statuses = state.board.statuses.filter((s) => s !== "blocked" && !isCustom(s));
  const i = statuses.indexOf(status);
  return i < 0 ? 0 : i - statuses.indexOf("todo");
}

function statusColor(status) {
  if (status === "blocked") return BLOCKED_COLOR;
  if (isCustom(status)) return CUSTOM_COLOR;
  const i = state.board ? statusStep(status) : 0;
  return STATUS_COLORS[Math.max(i, 0) % STATUS_COLORS.length];
}

function statusIcon(status) {
  const color = statusColor(status);
  const span = el("span", { class: "status-icon" });
  if (status === "blocked") {
    span.innerHTML = `<svg viewBox="0 0 14 14" aria-hidden="true"><circle cx="7" cy="7" r="6" fill="none" stroke="${color}" stroke-width="1.5"/>` +
      `<path d="m2.8 2.8 8.4 8.4" stroke="${color}" stroke-width="1.5" stroke-linecap="round"/></svg>`;
    return span;
  }
  if (isCustom(status)) {
    span.innerHTML = columnIconMarkup(columnIconName(status));
    return span;
  }
  const step = statusStep(status), steps = statusStep("done");
  const f = Math.max(step, 0) / steps;
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
  span.innerHTML = `<svg viewBox="0 0 14 14" aria-hidden="true"><circle cx="7" cy="7" r="6" fill="none" stroke="${color}" stroke-width="1.5"${dash}/>${inside}</svg>`;
  return span;
}

const key = (t) => `${t.project}#${t.id}`;

function parseTime(ts) {
  return new Date(ts.replace(" ", "T"));
}

function ago(ts) {
  const s = (Date.now() - parseTime(ts)) / 1000;
  if (s < 60) return tr("just now");
  if (s < 3600) return tr("{0}m ago", Math.floor(s / 60));
  if (s < 86400) return tr("{0}h ago", Math.floor(s / 3600));
  if (s < 86400 * 7) return tr("{0}d ago", Math.floor(s / 86400));
  return parseTime(ts).toLocaleDateString();
}

function timeEl(ts) {
  return el("time", { datetime: ts, title: parseTime(ts).toLocaleString() }, ago(ts));
}

function escapeHtml(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

function unescapeHtml(s) {
  return s.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&quot;/g, "\"").replace(/&amp;/g, "&");
}

function isProject(slug) {
  return !!state.board && state.board.projects.some((p) => p.slug === slug);
}

// The hash that opens task `id` with the board kept on the project in view.
function taskHref(id) {
  return `#${encodeURIComponent(state.project)}/${id}`;
}

// A link to a task, or to one of its comments. It keeps the board on the project in view, and
// following it adds a history entry (see the click handler), so Back returns to the previous task.
function taskRef(id, comment, text) {
  const href = `${taskHref(id)}${comment ? `/c${comment}` : ""}`;
  return `<a class="ref" href="${href}" data-task="${id}"${comment ? ` data-comment="${comment}"` : ""}>${text}</a>`;
}

// Code spans, links and bare URLs are swapped for placeholders as soon as they are
// rendered, so later rules never see, or rewrite, the HTML they produced. The input
// is already HTML-escaped, so it holds no tags or raw quotes of its own.
// `project` is the slug a bare #12 refers to: the task being shown.
function inline(s, project) {
  const saved = [];
  const keep = (html) => `\u0000${saved.push(html) - 1}\u0000`;
  const link = (url, text) => keep(`<a href="${url}" target="_blank" rel="noopener">${text}</a>`);
  s = s
    .replace(/\u0000/g, "")
    .replace(/`([^`]+)`/g, (_, code) => keep(`<code>${code}</code>`))
    // Only the board's own images load: its Content Security Policy blocks others, which stay links.
    .replace(/!\[([^\]]*)\]\((\/images\/[0-9a-f]{16}\.(?:png|jpg|gif|webp))\)/g,
      (_, alt, src) => keep(`<a href="${src}" target="_blank" rel="noopener"><img src="${src}" alt="${alt || "Image"}"></a>`))
    .replace(/!\[([^\]]*)\]\((https?:\/\/[^\s)\u0000]+)\)/g, (_, alt, url) => link(url, alt || url))
    .replace(/\[([^\]]+)\]\(((?:https?:\/\/|mailto:|file:\/\/\/)[^\s)\u0000]+)\)/g, (_, text, url) => link(url, emphasis(text)))
    .replace(/(^|[\s(])(https?:\/\/[^\s)\u0000]+)/g, (_, before, url) => before + link(url, url))
    .replace(/\[\[([a-z0-9][a-z0-9-]{0,31})\]\]/g, (all, slug) =>
      isProject(slug) ? keep(`<a class="ref" href="#${slug}" data-project="${slug}">${slug}</a>`) : all)
    .replace(/(^|[^\w#&;/-])([a-z0-9][a-z0-9-]{0,31})?#(\d+)(?:\/c(\d+))?(?![\w/])/g, (all, before, slug, id, comment) => {
      const target = slug || project;
      if (!target || !isProject(target)) return all;
      return before + keep(taskRef(id, comment, all.slice(before.length)));
    });
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

// The cells of a table row, with its outer pipes dropped. An escaped \| stays in its cell.
function tableCells(line) {
  const row = line.trim().replace(/\\\|/g, "\u0001").replace(/^\|/, "").replace(/\|$/, "");
  return row.split("|").map((c) => c.trim().replace(/\u0001/g, "|"));
}

const TABLE_DELIMITER = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;

// Syntax highlighting for fenced code: one small tokenizer per language, no library.
// Each part is a non-capturing pattern, tried in order at each position: comments,
// strings, the language's extras, numbers, then words, which keywords colour.
function codeLanguage({ kw = "", comment = [], string = [], extra = [] }) {
  const parts = [
    ...comment.map((r) => [r, "c"]),
    ...string.map((r) => [r, "s"]),
    ...extra,
    [String.raw`\b(?:0[xX][\da-fA-F_]+|0[bB][01_]+|\d[\d_]*(?:\.\d+)?(?:[eE][+-]?\d+)?)\w*`, "n"],
    [String.raw`[A-Za-z_$][\w$]*`, "w"],
  ];
  return {
    kw: new Set(kw.split(/\s+/).filter(Boolean)),
    re: new RegExp(parts.map(([r]) => `(${r})`).join("|"), "gm"),
    cls: parts.map(([, c]) => c),
  };
}

const C_COMMENTS = [String.raw`\/\/.*`, String.raw`\/\*[\s\S]*?\*\/`];
const C_STRINGS = [String.raw`"(?:\\.|[^"\\\n])*"`, String.raw`'(?:\\.|[^'\\\n])*'`];
const C_WORDS = "auto break case char const continue default do double else enum extern float for goto if inline int long " +
  "register restrict return short signed sizeof static struct switch typedef union unsigned void volatile while bool true false NULL";
const JS_WORDS = "break case catch class const continue debugger default delete do else export extends false finally for " +
  "function if import in instanceof let new null return super switch this throw true try typeof undefined var void while " +
  "with yield async await of static get set from as";
const C_LIKE = { comment: C_COMMENTS, string: C_STRINGS, extra: [[String.raw`^[ \t]*#[ \t]*\w+`, "p"]] };
const CODE_LANGUAGES = {};
for (const [names, def] of [
  [["c", "h"], codeLanguage({ ...C_LIKE, kw: C_WORDS })],
  [["cpp", "c++", "cc", "cxx", "hpp"], codeLanguage({ ...C_LIKE, kw: C_WORDS + " class namespace template typename public " +
    "private protected virtual override final new delete this operator using try catch throw nullptr constexpr noexcept " +
    "friend mutable explicit static_cast dynamic_cast reinterpret_cast const_cast decltype concept requires co_await " +
    "co_return co_yield std" })],
  [["rust", "rs"], codeLanguage({
    comment: C_COMMENTS,
    string: [String.raw`b?r#*"[\s\S]*?"#*`, String.raw`b?"(?:\\.|[^"\\])*"`, String.raw`b?'(?:\\.|[^'\\])'`],
    extra: [[String.raw`'[A-Za-z_]\w*`, "v"], [String.raw`#!?\[[^\]\n]*\]`, "p"], [String.raw`\b[a-z_]\w*!`, "p"]],
    kw: "as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move " +
      "mut pub ref return self Self static struct super trait true type unsafe use where while Option Some None Result Ok " +
      "Err Box Vec String u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64 bool char str",
  })],
  [["python", "py"], codeLanguage({
    comment: [String.raw`#.*`],
    string: [String.raw`[rRbBfFuU]{0,2}(?:"""[\s\S]*?"""|'''[\s\S]*?''')`,
      String.raw`[rRbBfFuU]{0,2}(?:"(?:\\.|[^"\\\n])*"|'(?:\\.|[^'\\\n])*')`],
    extra: [[String.raw`^[ \t]*@[\w.]+`, "p"]],
    kw: "False None True and as assert async await break class continue def del elif else except finally for from global " +
      "if import in is lambda nonlocal not or pass raise return try while with yield self",
  })],
  [["asm", "assembly", "s", "nasm", "x86", "arm"], codeLanguage({
    comment: [String.raw`;.*`, String.raw`\/\/.*`, String.raw`\/\*[\s\S]*?\*\/`, String.raw`^[ \t]*#.*`],
    string: C_STRINGS,
    extra: [
      [String.raw`^[ \t]*[\w.$]+:`, "p"],
      [String.raw`\.[A-Za-z_]\w*`, "p"],
      [String.raw`%?\b(?:[re]?[abcd]x|[abcd][lh]|[re]?(?:si|di|sp|bp|ip)|r(?:[89]|1[0-5])[dwb]?|[xyz]mm\d{1,2}|[xw](?:[12]?\d|3[01])|lr|pc|fp)\b`, "v"],
      [String.raw`^[ \t]*[A-Za-z][\w.]*\b(?!:)`, "k"],
    ],
  })],
  [["bash", "sh", "shell", "zsh", "console"], codeLanguage({
    comment: [String.raw`(?:^|[ \t])#.*`],
    string: [String.raw`"(?:\\.|[^"\\])*"`, String.raw`'[^']*'`],
    extra: [[String.raw`\$(?:\{[^}\n]*\}|\w+|[@#?$!*-])`, "v"]],
    kw: "if then else elif fi for while until do done case esac in function return local export readonly declare set " +
      "unset shift exit source alias echo cd test true false sudo",
  })],
  [["javascript", "js", "jsx", "mjs", "json"], codeLanguage({ comment: C_COMMENTS, string: [...C_STRINGS, "`(?:\\\\.|[^`\\\\])*`"], kw: JS_WORDS })],
  [["typescript", "ts", "tsx"], codeLanguage({ comment: C_COMMENTS, string: [...C_STRINGS, "`(?:\\\\.|[^`\\\\])*`"],
    kw: JS_WORDS + " interface type enum implements private public protected readonly declare namespace abstract number " +
      "string boolean any unknown never" })],
]) for (const name of names) CODE_LANGUAGES[name] = def;

// `code` is raw text; the result is escaped HTML.
function highlightCode(code, name) {
  const def = CODE_LANGUAGES[(name || "").toLowerCase()];
  if (!def) return escapeHtml(code);
  let out = "", last = 0, m;
  def.re.lastIndex = 0;
  while ((m = def.re.exec(code))) {
    if (!m[0]) { def.re.lastIndex++; continue; }
    let cls = def.cls[m.findIndex((g, i) => i > 0 && g !== undefined) - 1];
    if (cls === "w") cls = def.kw.has(m[0]) ? "k" : "";
    out += escapeHtml(code.slice(last, m.index));
    out += cls ? `<span class="tok-${cls}">${escapeHtml(m[0])}</span>` : escapeHtml(m[0]);
    last = m.index + m[0].length;
  }
  return out + escapeHtml(code.slice(last));
}

// `project` is the slug that bare #12 references point into.
function markdown(src, project) {
  const lines = escapeHtml(src.replace(/\r\n?/g, "\n")).split("\n");
  const out = [];
  let para = [];
  let list = null;
  const md = (s) => inline(s, project);
  const flushPara = () => { if (para.length) out.push(`<p>${md(para.join("<br>"))}</p>`); para = []; };
  const flushList = () => {
    if (!list) return;
    out.push(`<${list.type}>${list.items.join("")}</${list.type}>`);
    list = null;
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    let m;
    if ((m = line.match(/^\s*```\s*([\w+#.-]*)/))) {
      flushPara(); flushList();
      const code = [];
      while (++i < lines.length && !/^\s*```/.test(lines[i])) code.push(lines[i]);
      const name = m[1].toLowerCase();
      const cls = name ? ` class="language-${escapeHtml(name)}"` : "";
      out.push(`<pre><code${cls}>${highlightCode(unescapeHtml(code.join("\n")), name)}</code></pre>`);
    } else if ((m = line.match(/^(#{1,6})\s+(.*)$/))) {
      flushPara(); flushList();
      const n = Math.min(m[1].length, 4);
      out.push(`<h${n}>${md(m[2])}</h${n}>`);
    } else if (line.includes("|") && TABLE_DELIMITER.test(lines[i + 1] || "")
               && tableCells(line).length === tableCells(lines[i + 1]).length) {
      // A table may follow a line of text directly, as on GitHub. Its body runs to the
      // first line with no pipe; short rows get empty cells and long ones are cut.
      flushPara(); flushList();
      const align = tableCells(lines[++i]).map((d) =>
        d.startsWith(":") ? (d.endsWith(":") ? "center" : "left") : d.endsWith(":") ? "right" : "");
      const row = (tag, cells) => "<tr>" + align.map((a, j) =>
        `<${tag}${a ? ` style="text-align:${a}"` : ""}>${md(cells[j] || "")}</${tag}>`).join("") + "</tr>";
      const body = [];
      while (i + 1 < lines.length && lines[i + 1].includes("|")) body.push(row("td", tableCells(lines[++i])));
      out.push(`<div class="table"><table><thead>${row("th", tableCells(line))}</thead>`
        + (body.length ? `<tbody>${body.join("")}</tbody>` : "") + "</table></div>");
    } else if ((m = line.match(/^\s*(?:[-*+]|(\d+)[.)])\s+(.*)$/))) {
      flushPara();
      const type = m[1] ? "ol" : "ul";
      if (list && list.type !== type) flushList();
      list = list || { type, items: [] };
      const check = m[2].match(/^\[( |x|X)\]\s+(.*)$/);
      list.items.push(check
        ? `<li class="check"><input type="checkbox" disabled ${check[1] === " " ? "" : "checked"}> ${md(check[2])}</li>`
        : `<li>${md(m[2])}</li>`);
    } else if ((m = line.match(/^&gt;\s?(.*)$/))) {
      flushPara(); flushList();
      const quote = [m[1]];
      while (i + 1 < lines.length && (m = lines[i + 1].match(/^&gt;\s?(.*)$/))) { quote.push(m[1]); i++; }
      out.push(`<blockquote>${md(quote.join("<br>"))}</blockquote>`);
    } else if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      flushPara(); flushList();
      out.push("<hr>");
    } else if (line.trim() === "") {
      flushPara(); flushList();
    } else if (list && /^\s{2,}\S/.test(line)) {
      list.items[list.items.length - 1] = list.items[list.items.length - 1].replace(/<\/li>$/, ` ${md(line.trim())}</li>`);
    } else {
      flushList();
      para.push(line);
    }
  }
  flushPara(); flushList();
  return out.join("");
}

// The in-place editor for descriptions and comments renders Markdown where it is typed, as Obsidian's
// Live Preview does. Its text is always exactly the Markdown: rendering only wraps that text in
// styled spans, and markers (`**`, `#`, `](url)` and so on) get class "mk", which CSS hides on lines
// away from the caret. So a caret position is just an offset into the text.
const mdMarker = (s) => s ? `<span class="mk">${escapeHtml(s)}</span>` : "";

// One line's inline Markdown as HTML, with every character of `raw` kept, in order. The rules, and
// their order, are inline()'s. Board images found are pushed onto `images`.
function editorInline(raw, project, images) {
  if (raw.includes("\u0000")) return escapeHtml(raw);
  const saved = [];
  const keep = (html) => `\u0000${saved.push(html) - 1}\u0000`;
  // The input is escaped already.
  const mk = (s) => s ? keep(`<span class="mk">${s}</span>`) : "";
  const wrap = (open, s, close) => mk(open) + keep(s) + mk(close);
  const emph = (s) => s
    .replace(/\*\*(.+?)\*\*/g, (_, t) => wrap("**", `<strong>${emph(t)}</strong>`, "**"))
    .replace(/(^|[^\w*])\*(?!\s)(.+?)\*(?!\w)/g, (_, before, t) => before + wrap("*", `<em>${emph(t)}</em>`, "*"))
    .replace(/(^|[^\w])_(?!\s)(.+?)_(?!\w)/g, (_, before, t) => before + wrap("_", `<em>${emph(t)}</em>`, "_"))
    .replace(/~~(.+?)~~/g, (_, t) => wrap("~~", `<del>${emph(t)}</del>`, "~~"));
  const s = escapeHtml(raw)
    .replace(/`([^`]+)`/g, (_, code) => wrap("`", `<code>${code}</code>`, "`"))
    .replace(/!\[([^\]]*)\]\((\/images\/[0-9a-f]{16}\.(?:png|jpg|gif|webp))\)/g, (all, alt, src) => {
      images.push({ src, alt });
      return mk(all);
    })
    .replace(/!\[([^\]]*)\]\((https?:\/\/[^\s)\u0000]+)\)/g, (_, alt, url) =>
      alt ? wrap("![", `<span class="a">${alt}</span>`, `](${url})`) : wrap("![](", `<span class="a">${url}</span>`, ")"))
    .replace(/\[([^\]]+)\]\(((?:https?:\/\/|mailto:)[^\s)\u0000]+)\)/g, (_, text, url) =>
      wrap("[", `<span class="a">${emph(text)}</span>`, `](${url})`))
    .replace(/(^|[\s(])(https?:\/\/[^\s)\u0000]+)/g, (_, before, url) => before + keep(`<span class="a">${url}</span>`))
    .replace(/\[\[([a-z0-9][a-z0-9-]{0,31})\]\]/g, (all, slug) =>
      isProject(slug) ? wrap("[[", `<span class="ref">${slug}</span>`, "]]") : all)
    .replace(/(^|[^\w#&;/-])([a-z0-9][a-z0-9-]{0,31})?#(\d+)(?:\/c(\d+))?(?![\w/])/g, (all, before, slug) => {
      const target = slug || project;
      if (!target || !isProject(target)) return all;
      return before + keep(`<span class="ref">${all.slice(before.length)}</span>`);
    });
  const restore = (html) => html.replace(/\u0000(\d+)\u0000/g, (_, i) => restore(saved[i]));
  return restore(emph(s));
}

// Each line of `text` as { cls, html }: `cls` names the line's kind for CSS, and the text of `html`
// is the line, exactly. Fenced code is coloured line by line in its fence's language.
function editorLines(text, project) {
  let fence = null;
  return text.split("\n").map((raw) => {
    const images = [];
    const inl = (s) => editorInline(s, project, images);
    let m, cls = "", html;
    if ((m = raw.match(/^(\s*```\s*)([\w+#.-]*)(.*)$/))) {
      if (fence === null) {
        fence = m[2].toLowerCase();
        cls = "fence open";
        html = mdMarker(m[1]) + (m[2] ? `<span class="lang">${escapeHtml(m[2])}</span>` : "") + mdMarker(m[3]);
      } else {
        fence = null;
        cls = "fence close";
        html = mdMarker(raw);
      }
    } else if (fence !== null) {
      cls = "code";
      html = highlightCode(raw, fence);
    } else if ((m = raw.match(/^(#{1,6})(\s+)(.*)$/))) {
      cls = "h" + Math.min(m[1].length, 4);
      html = mdMarker(m[1] + m[2]) + inl(m[3]);
    } else if ((m = raw.match(/^(\s*)(?:([-*+])|(\d+[.)]))(\s+)(.*)$/))) {
      const check = m[5].match(/^(\[( |x|X)\]\s+)(.*)$/);
      const bullet = '<span class="bul" contenteditable="false"></span>';
      cls = (m[3] ? "ol" : "ul") + (check ? (check[2] === " " ? " task" : " task done") : "");
      const body = inl(check ? check[3] : m[5]);
      html = escapeHtml(m[1]) + (m[3]
        ? escapeHtml(m[3] + m[4]) + (check ? bullet + mdMarker(check[1]) : "")
        : bullet + mdMarker(m[2] + m[4] + (check ? check[1] : ""))) + body;
    } else if ((m = raw.match(/^(>\s?)(.*)$/))) {
      cls = "quote";
      html = mdMarker(m[1]) + inl(m[2]);
    } else if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(raw)) {
      cls = "hr";
      html = mdMarker(raw);
    } else {
      html = inl(raw);
    }
    for (const i of images) html += `<img src="${i.src}" alt="${i.alt || "Image"}" contenteditable="false">`;
    return { cls, html };
  });
}

// Turns `ed`, an empty contenteditable div, into the in-place editor. It keeps a textarea's `value`
// and setSelectionRange(), so the code around it treats it as one. Its children are one div per line
// (a <br> in an empty one); browsers' edits can leave other shapes, which read() copes with.
function mdEditor(ed) {
  let text = "", history = [{ text, sel: null }], at = 0, group = null, composing = false, quiet = false;
  const focused = () => document.activeElement === ed;
  const lineOf = (o) => text.slice(0, o).split("\n").length - 1;

  // The text the DOM holds now, and for each top-level node the offset where its line starts and
  // that line's first node. Divs are lines; text and inline nodes between them form lines that a <br> ends.
  function read() {
    const lines = [], info = new Map();
    let cur = null, first = null, start = 0;
    const close = () => { lines.push(cur || ""); start += (cur || "").length + 1; cur = null; };
    for (const n of ed.childNodes) {
      if (n.nodeName === "DIV" || n.nodeName === "P") {
        if (cur !== null) close();
        info.set(n, { start, first: n });
        cur = n.textContent;
        close();
        continue;
      }
      if (cur === null) { cur = ""; first = n; }
      info.set(n, { start, first });
      if (n.nodeName === "BR") close();
      else cur += n.textContent;
    }
    if (cur !== null) close();
    return { value: lines.join("\n"), info };
  }

  // A DOM point as an offset into the text read() returned.
  function offsetOf(r, node, off) {
    let top = node;
    if (node === ed) {
      if (off >= ed.childNodes.length) return r.value.length;
      top = ed.childNodes[off];
    }
    while (top.parentNode !== ed) top = top.parentNode;
    const line = r.info.get(top);
    const range = document.createRange();
    range.setStartBefore(line.first);
    range.setEnd(node, off);
    return Math.min(line.start + range.toString().length, r.value.length);
  }

  // The selection as { start, end } offsets (anchor and focus, so end may come first), or null.
  function getSel(r) {
    const sel = getSelection();
    if (!sel.rangeCount || !ed.contains(sel.anchorNode) || !ed.contains(sel.focusNode)) return null;
    return { start: offsetOf(r, sel.anchorNode, sel.anchorOffset), end: offsetOf(r, sel.focusNode, sel.focusOffset) };
  }

  // The DOM point at offset `o` of the rendered text.
  function pointAt(o) {
    const lines = text.split("\n");
    if (!ed.children.length) return [ed, 0];
    let i = 0;
    while (i < lines.length - 1 && o > lines[i].length) { o -= lines[i].length + 1; i++; }
    const line = ed.children[Math.min(i, ed.children.length - 1)];
    o = Math.max(0, Math.min(o, lines[i].length));
    const walker = document.createTreeWalker(line, NodeFilter.SHOW_TEXT);
    let last = null;
    for (let t; (t = walker.nextNode()); last = t) {
      if (o <= t.length) return [t, o];
      o -= t.length;
    }
    return last ? [last, last.length] : [line, 0];
  }

  function setSel(sel) {
    const a = pointAt(sel.start), f = pointAt(sel.end);
    getSelection().setBaseAndExtent(a[0], a[1], f[0], f[1]);
  }

  function reveal(sel) {
    const node = pointAt(sel.end)[0];
    const box = node.nodeType === 1 ? node : node.parentElement;
    if (box.scrollIntoViewIfNeeded) box.scrollIntoViewIfNeeded(false);
    else box.scrollIntoView({ block: "nearest" });
  }

  // Lines the caret is on, or a selection touches, show their markers; the rest show rendered.
  function mark(sel) {
    let from = -1, to = -1;
    if (sel && focused()) {
      from = lineOf(Math.min(sel.start, sel.end));
      to = lineOf(Math.max(sel.start, sel.end));
    }
    [...ed.children].forEach((line, i) => {
      const cls = "ln" + (line._cls || "") + (i < from || i > to ? " rendered" : "");
      if (line.className !== cls) line.className = cls;
    });
  }

  // Brings the DOM in line with `text`, keeping the lines that are already right, so spellcheck
  // and the browser's own state survive. With `sel`, the selection is put back where it was.
  function paint(sel) {
    const rows = text ? editorLines(text, panelTask && panelTask.project) : [];
    rows.forEach((row, i) => {
      const old = ed.childNodes[i];
      if (old && old.nodeName === "DIV" && old._html === row.html && old._dom === old.innerHTML) {
        old._cls = row.cls ? " " + row.cls : "";
        return;
      }
      const line = document.createElement("div");
      line.innerHTML = row.html || "<br>";
      line._html = row.html;
      line._dom = line.innerHTML;
      line._cls = row.cls ? " " + row.cls : "";
      if (old) ed.replaceChild(line, old);
      else ed.append(line);
    });
    while (ed.childNodes.length > rows.length) ed.lastChild.remove();
    if (sel && focused()) setSel(sel);
    mark(sel);
  }

  // Typing merges into one undo step until it pauses, changes kind or passes a space.
  function record(kind, sel) {
    const now = Date.now();
    if (text === history[at].text) { history[at].sel = sel; return; }
    const merge = group && kind !== "other" && now - group.time < 1500 &&
      (group.kind === kind || (group.kind === "type" && kind === "space"));
    if (merge) history[at] = { text, sel };
    else {
      history.splice(at + 1);
      history.push({ text, sel });
      if (history.length > 200) history.shift();
      at = history.length - 1;
    }
    group = { kind, time: now };
  }

  // Tells the page's own input listeners (the send button, for one) that the text changed.
  function changed() {
    quiet = true;
    ed.dispatchEvent(new Event("input", { bubbles: true }));
    quiet = false;
  }

  function sync(e) {
    const r = read();
    const sel = getSel(r);
    text = r.value;
    const type = (e && e.inputType) || "";
    const kind = type === "insertText" ? (/^\s+$/.test(e.data || "") ? "space" : "type")
      : type.startsWith("delete") && !type.includes("ByCut") && !type.includes("ByDrag") ? type : "other";
    record(kind, sel);
    paint(sel);
  }

  // Sets the text through the model, rather than the browser's editing, as one undo step.
  function put(value, sel, kind) {
    text = value;
    record(kind, sel);
    group = null;
    paint(sel);
    reveal(sel);
    changed();
  }

  // Puts `str` in place of the selection.
  function insert(str) {
    const r = read();
    const sel = getSel(r) || { start: r.value.length, end: r.value.length };
    const from = Math.min(sel.start, sel.end), to = Math.max(sel.start, sel.end);
    const caret = { start: from + str.length, end: from + str.length };
    put(r.value.slice(0, from) + str + r.value.slice(to), caret, str === "\n" ? "line" : "other");
  }

  function go(to) {
    if (to < 0 || to >= history.length) return;
    at = to;
    group = null;
    text = history[at].text;
    const sel = history[at].sel || { start: text.length, end: text.length };
    paint(sel);
    reveal(sel);
    changed();
  }

  ed.addEventListener("compositionstart", () => { composing = true; });
  ed.addEventListener("compositionend", () => { composing = false; sync(null); });
  ed.addEventListener("input", (e) => {
    if (quiet || composing || e.isComposing) return;
    sync(e);
  });
  ed.addEventListener("beforeinput", (e) => {
    if (e.isComposing) return;
    const type = e.inputType;
    if (type === "historyUndo") { e.preventDefault(); go(at - 1); }
    else if (type === "historyRedo") { e.preventDefault(); go(at + 1); }
    else if (type === "insertParagraph" || type === "insertLineBreak") { e.preventDefault(); insert("\n"); }
    else if (type.startsWith("format")) e.preventDefault();
  });
  // Ctrl/Cmd+Enter, Escape and Tab are left to the page's own listeners.
  ed.addEventListener("keydown", (e) => {
    // 229: a key an input method is handling, like the Enter that ends a composition in Safari.
    if (composing || e.isComposing || e.keyCode === 229) return;
    const mod = e.metaKey || e.ctrlKey, k = e.key.toLowerCase();
    if (e.key === "Enter" && !mod) { e.preventDefault(); insert("\n"); }
    else if (mod && !e.altKey && k === "z") { e.preventDefault(); go(at + (e.shiftKey ? 1 : -1)); }
    else if (e.ctrlKey && !e.altKey && !e.shiftKey && k === "y") { e.preventDefault(); go(at + 1); }
    // No bold, italic or underline markup: the text stays plain Markdown.
    else if (mod && !e.altKey && (k === "b" || k === "i" || k === "u")) e.preventDefault();
  });
  // Images are attachImages()'s, in the page's own paste listener; text goes in as plain text.
  ed.addEventListener("paste", (e) => {
    if (!e.clipboardData || imagesIn(e.clipboardData).length) return;
    e.preventDefault();
    insert(e.clipboardData.getData("text/plain").replace(/\r\n?/g, "\n"));
  });
  // Copies the Markdown itself: the browser's own copy would drop hidden markers and line breaks.
  for (const type of ["copy", "cut"]) {
    ed.addEventListener(type, (e) => {
      const sel = getSel(read());
      if (!sel || sel.start === sel.end || !e.clipboardData) return;
      e.preventDefault();
      e.clipboardData.setData("text/plain", text.slice(Math.min(sel.start, sel.end), Math.max(sel.start, sel.end)));
      if (type === "cut") insert("");
    });
  }
  // Dragged text is the Markdown too, and a drop goes in as plain text, moving it when dragged from here.
  let dragged = null;
  ed.addEventListener("dragstart", (e) => {
    const sel = getSel(read());
    dragged = sel && sel.start !== sel.end ? { from: Math.min(sel.start, sel.end), to: Math.max(sel.start, sel.end) } : null;
    if (dragged && e.dataTransfer) e.dataTransfer.setData("text/plain", text.slice(dragged.from, dragged.to));
  });
  ed.addEventListener("dragend", () => { dragged = null; });
  // Image drops are the drop zone's, around the editor.
  ed.addEventListener("drop", (e) => {
    const transfer = e.dataTransfer;
    if (!transfer || imagesIn(transfer).length) return;
    const move = dragged;
    dragged = null;
    const str = move ? text.slice(move.from, move.to) : transfer.getData("text/plain").replace(/\r\n?/g, "\n");
    if (!str) return;
    e.preventDefault();
    const point = pointFromXY(e.clientX, e.clientY);
    let value = text, o = point && ed.contains(point[0]) ? offsetOf(read(), point[0], point[1]) : text.length;
    if (move) {
      if (o >= move.from && o <= move.to) return;
      value = value.slice(0, move.from) + value.slice(move.to);
      if (o > move.to) o -= move.to - move.from;
    }
    ed.focus();
    put(value.slice(0, o) + str + value.slice(o), { start: o, end: o + str.length }, "other");
  });
  document.addEventListener("selectionchange", () => {
    if (composing || !focused()) return;
    const r = read();
    if (r.value !== text) return;
    const sel = getSel(r);
    if (!sel) return;
    // Undo puts the caret back where it was just before the edit; moving it starts a new step.
    const now = history[at].sel;
    if (!now || now.start !== sel.start || now.end !== sel.end) { history[at].sel = sel; group = null; }
    mark(sel);
  });
  ed.addEventListener("blur", () => mark(null));

  Object.defineProperty(ed, "value", {
    get: () => text,
    set: (v) => {
      const sel = focused() ? getSel(read()) : null;
      text = String(v == null ? "" : v).replace(/\r\n?/g, "\n");
      history = [{ text, sel: null }];
      at = 0;
      group = null;
      const clamp = (o) => Math.min(o, text.length);
      paint(sel && { start: clamp(sel.start), end: clamp(sel.end) });
    },
  });
  ed.setSelectionRange = (start, end = start) => {
    const sel = { start: Math.min(start, text.length), end: Math.min(end, text.length) };
    setSel(sel);
    mark(sel);
  };
  paint(null);
}

// The DOM point, [node, offset], under a viewport position, or null.
function pointFromXY(x, y) {
  if (document.caretPositionFromPoint) {
    const p = document.caretPositionFromPoint(x, y);
    return p && p.offsetNode ? [p.offsetNode, p.offset] : null;
  }
  if (document.caretRangeFromPoint) {
    const r = document.caretRangeFromPoint(x, y);
    return r ? [r.startContainer, r.startOffset] : null;
  }
  return null;
}

// Where in the Markdown `src` a click on its rendered view lands: the clicked text is looked up in the
// source after as many earlier copies of it as the view shows before it. Null when it can't tell.
function sourceOffsetAt(view, x, y, src) {
  const point = pointFromXY(x, y);
  if (!point) return null;
  const [node, off] = point;
  if (node.nodeType !== 3 || !node.data || !view.contains(node)) return null;
  const range = document.createRange();
  range.setStart(view, 0);
  range.setEnd(node, 0);
  const before = range.toString(), word = node.data;
  let seen = 0;
  for (let i = before.indexOf(word); i >= 0; i = before.indexOf(word, i + word.length)) seen++;
  let found = -word.length;
  for (let k = 0; k <= seen; k++) {
    found = src.indexOf(word, found + word.length);
    if (found < 0) return null;
  }
  return found + off;
}

// Loads can overlap (a poll, a drop, an edit); only the newest one may render,
// so a slow older reply never undoes what is on screen.
let boardRequest = 0;

async function loadBoard() {
  const request = ++boardRequest;
  try {
    const done = state.allDone ? "&done=all" : "";
    const q = filterQuery();
    const board = await api("GET", "/board?project=" + encodeURIComponent(state.project) + done
                            + (q ? "&q=" + encodeURIComponent(q) : ""));
    if (request !== boardRequest) return;
    state.board = board;
    document.documentElement.dataset.hide = (board.hidden_properties || []).join(" ");
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

function updateHash() {
  const hash = "#" + state.project + (state.task ? "/" + state.task : "");
  if (location.hash !== hash) history.replaceState(null, "", hash);
}

function readHash() {
  let hash = location.hash.slice(1);
  try { hash = decodeURIComponent(hash); } catch {}
  const [slug, task, comment] = hash.split("/");
  return { slug: slug || null, task: Number(task) || null, comment: Number((comment || "").replace(/^c/, "")) || null };
}

// The comment a link pointed at, scrolled to once its task shows.
let focusComment = null;

function showFocusedComment() {
  const target = focusComment && document.getElementById(`c${focusComment}`);
  if (!target) return;
  focusComment = null;
  target.scrollIntoView({ block: "center" });
  target.classList.add("flash");
  setTimeout(() => target.classList.remove("flash"), 2000);
}

// Task and project links change the hash themselves, which adds a history entry; the
// hashchange handler then opens them. Only a link to where you already are needs help.
document.addEventListener("click", (e) => {
  const ref = e.target.closest("a.ref");
  if (!ref || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey) return;
  if (ref.getAttribute("href") !== location.hash) return;
  e.preventDefault();
  focusComment = Number(ref.dataset.comment) || null;
  showFocusedComment();
});

function setProject(slug) {
  state.project = slug;
  localStorageSet("kanzo.project", slug);
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
  // Redrawing would take the field away from someone renaming a project.
  if (renaming && document.contains(renaming.input)) return;
  // Nor while dragging a project: it would cancel the drag.
  if (movingProject) return;
  const { projects } = state.board;
  const open = projects.reduce((n, p) => n + p.open, 0);
  const link = (slug, name, count, glyph, template) =>
    el("a", { href: "#" + slug, title: template ? tr("{0} · template", name) : name, draggable: "false",
              class: state.project === slug ? "active" : null,
              onclick: (e) => { e.preventDefault(); setProject(slug); } },
      glyph,
      el("span", { class: "name" }, name),
      template ? el("span", { class: "template-mark", title: "Template" }, icon("template")) : null,
      el("span", { class: "count" }, String(count)));
  const item = (p) => renaming && renaming.slug === p.slug
    ? el("div", { class: "item renaming" }, icon("folder", projectColor(p.slug)), renaming.input)
    : el("div", { class: "item" + (state.menu === p.slug ? " menu-open" : "") + (p.template ? " template" : ""),
                  draggable: "true", "data-slug": p.slug },
    link(p.slug, p.name, p.open, icon("folder", projectColor(p.slug)), p.template),
    el("button", { class: "more", title: tr("More"), "aria-label": tr("More for {0}", p.name), "aria-haspopup": "menu",
                   onclick: (e) => toggleProjectMenu(p, e.currentTarget) }, icon("more")));
  $("#nav").replaceChildren(
    link("all", tr("All projects"), open, icon("board")),
    ...(projects.length ? [el("div", { class: "label" }, tr("Projects"))] : []),
    ...projects.map(item),
  );
}

// `state.menu` holds the slug, so the row keeps its highlight while polling redraws the sidebar.
function toggleProjectMenu(p, button) {
  if (!toggleMenu(p.slug)) return;
  button.closest(".item").classList.add("menu-open");
  const swatches = el("div", { class: "swatches", role: "group", "aria-label": tr("Color") });
  const drawSwatches = () => swatches.replaceChildren(...Object.entries(PROJECT_COLORS).map(([name, hex]) => {
    const current = projectColorName(p.slug) === name;
    return el("button", { class: "swatch", style: `background:${hex}`, title: tr(name), "aria-label": tr(name),
                          "aria-pressed": String(current), onclick: () => pickColor(p.slug, name).then(drawSwatches) },
      current ? icon("check") : null);
  }));
  drawSwatches();
  const repos = el("div", { class: "repos" });
  const drawRepos = (list, focus) => {
    const input = el("input", { class: "field", placeholder: tr("Link a repo: owner/repo"), "aria-label": tr("Link a GitHub repo"),
                                autocomplete: "off", spellcheck: "false" });
    input.addEventListener("keydown", async (e) => {
      if (e.key !== "Enter" || !input.value.trim()) return;
      const q = await changeRepo(p.slug, "POST", input.value);
      if (q) drawRepos(q.repos, true);
    });
    repos.replaceChildren(...list.map((r) => el("div", { class: "repo" }, el("span", { class: "name" }, r),
      el("button", { class: "unlink", title: tr("Unlink {0}", r), "aria-label": tr("Unlink {0}", r),
                     onclick: async () => { const q = await changeRepo(p.slug, "DELETE", r); if (q) drawRepos(q.repos); } },
        icon("close")))), input);
    if (focus) input.focus();
  };
  drawRepos(p.repos);
  const rename = el("button", { onclick: () => startRename(p) }, icon("rename"), tr("Rename"));
  const columns = el("button", { onclick: () => { closeMenu(); openColumns(p); } }, icon("columns"), tr("Columns"));
  const template = el("button", { onclick: () => { closeMenu(); setTemplate(p, !p.template); } },
    icon("template"), p.template ? tr("Stop using as template") : tr("Use as template"));
  const fromTemplate = p.template
    ? el("button", { onclick: () => { closeMenu(); showNewProject(p.slug); } }, icon("newProject"), tr("New project from this template"))
    : null;
  const remove = el("button", { class: "danger", onclick: () => { closeMenu(); deleteProject(p); } },
    icon("trash"), tr("Delete project"));
  const menu = el("div", { class: "menu", role: "menu", tabindex: "-1" },
    el("div", { class: "label" }, tr("Color")), swatches, el("div", { class: "label" }, tr("Repos")), repos,
    el("div", { class: "sep" }), rename, columns, template, fromTemplate, remove);
  document.body.append(menu);
  const r = button.getBoundingClientRect();
  const below = r.bottom + 4 + menu.offsetHeight <= innerHeight;
  menu.style.top = `${below ? r.bottom + 4 : r.top - 4 - menu.offsetHeight}px`;
  menu.style.left = `${Math.max(8, Math.min(r.left, innerWidth - menu.offsetWidth - 8))}px`;
  // Focus the menu itself, so Tab reaches its buttons without one looking selected.
  menu.focus();
}

async function setTemplate(p, template) {
  try {
    await api("PATCH", `/projects/${encodeURIComponent(p.slug)}`, { template });
    await loadBoard();
    toast(template ? `${p.slug} is now a template` : `${p.slug} is no longer a template`);
  } catch (err) { toast(err.message); }
}

let renaming = null;
let movingProject = null;
const projectLine = el("div", { class: "drop-line" });
const projectAfter = (y) => [...$("#nav").querySelectorAll("[data-slug]")]
  .find((i) => { const r = i.getBoundingClientRect(); return y < r.top + r.height / 2; }) || null;

$("#nav").addEventListener("dragstart", (e) => {
  // Or it's text dragged out of the field renaming a project.
  movingProject = e.target.dataset?.slug;
  if (!movingProject) return;
  e.dataTransfer.effectAllowed = "move";
  e.dataTransfer.setData("text/plain", movingProject);
});
$("#nav").addEventListener("dragover", (e) => {
  if (!movingProject) return;
  e.preventDefault();
  $("#nav").insertBefore(projectLine, projectAfter(e.clientY));
});
$("#nav").addEventListener("dragleave", (e) => { if (!$("#nav").contains(e.relatedTarget)) projectLine.remove(); });
$("#nav").addEventListener("drop", async (e) => {
  if (!movingProject) return;
  e.preventDefault();
  const before = projectAfter(e.clientY)?.dataset.slug || null;
  try { await api("POST", `/projects/${encodeURIComponent(movingProject)}/move`, { before }); } catch (err) { toast(err.message); }
  loadBoard();
});
$("#nav").addEventListener("dragend", () => {
  movingProject = null;
  projectLine.remove();
  renderNav();
});

function startRename(p) {
  closeMenu();
  const input = el("input", { class: "field", "aria-label": tr("Rename {0}", p.name), spellcheck: "false", autocomplete: "off" });
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

async function changeRepo(slug, method, repo) {
  try {
    const p = await api(method, `/projects/${encodeURIComponent(slug)}/repos`, { repo });
    loadBoard();
    return p;
  } catch (err) { toast(err.message); return null; }
}

async function pickColor(slug, color) {
  try {
    await api("PATCH", `/projects/${encodeURIComponent(slug)}`, { color });
    await loadBoard();
  } catch (err) { toast(err.message); }
}

function toggleMenu(name) {
  const wasOpen = state.menu === name;
  closeMenu();
  if (wasOpen) return false;
  state.menu = name;
  return true;
}

function closeMenu() {
  if (!state.menu) return;
  state.menu = null;
  document.querySelector(".menu")?.remove();
  document.querySelectorAll(".nav .menu-open").forEach((i) => i.classList.remove("menu-open"));
  $("#settings").classList.remove("open");
  $("#network-menu").hidden = true;
  $("#network").classList.remove("open");
  document.querySelectorAll(".icon-pick[aria-expanded=true]").forEach((b) => b.setAttribute("aria-expanded", "false"));
}

// A click on a menu's own button is left to toggle it.
document.addEventListener("mousedown", (e) => {
  if (!e.target.closest(".menu, .nav .more, #settings, #network, #network-menu, .icon-pick")) closeMenu();
});
document.addEventListener("mousedown", (e) => {
  if (state.task && e.button === 0 && !e.target.closest("#panel, .card, .menu, .search-dialog, .dropdown-list")) closeTask();
});
window.addEventListener("resize", closeMenu);
window.addEventListener("blur", closeMenu);
$("#nav").addEventListener("scroll", closeMenu);
$("#columns-list").addEventListener("scroll", closeMenu);

function currentProject() {
  return state.board.projects.find((p) => p.slug === state.project) || null;
}

function renderHeader() {
  const p = currentProject();
  const { projects, tasks } = state.board;
  const header = $("#header");
  $("#new-task").hidden = !projects.length;
  $("#human-only").hidden = !projects.length || isPropertyHidden("human");
  $("#filter-box").hidden = !projects.length;
  $("#view").hidden = !projects.length;
  $("#view").setAttribute("aria-pressed", String(isList()));
  $("#filters").replaceChildren(...(projects.length ? savedFilters() : []).map((f, i) =>
    el("span", { class: "chip" }, el("span", {}, queryNodes(f)),
      el("button", { type: "button", title: tr(`Remove "{0}"`, f), "aria-label": tr("Remove the filter {0}", f),
                     onclick: () => setFilters(savedFilters().filter((_, j) => j !== i)) }, icon("close")))));
  $("#human-only").setAttribute("aria-pressed", String(isHumanOnly()));
  if (!projects.length) { header.replaceChildren(); return; }
  const open = (n) => plural(n, "open task");
  if (!p) {
    const count = projects.reduce((n, p) => n + p.open, 0);
    header.replaceChildren(el("h1", {}, tr("All projects")),
      el("span", { class: "sub" }, tr("{0} in {1}", open(count), plural(projects.length, "project"))));
    return;
  }
  header.replaceChildren(
    el("h1", {}, p.name),
    el("span", { class: "sub" }, open(p.open)),
    ...(p.template ? [el("span", { class: "sub template-note", title: "New projects can start from this one" },
      icon("template"), "Template · agents don't take these tasks")] : []),
    ...p.repos.map((r) => el("a", { class: "repo-link", href: `https://github.com/${r}`, target: "_blank", rel: "noopener",
                                    title: tr("Open {0} on GitHub", r) }, icon("github"), r)),
  );
}

async function deleteProject(p) {
  const tasks = p.total ? tr(" and its {0}, with their comments", plural(p.total, "task")) : "";
  if (!await askConfirm({ title: tr("Delete project?"), message: tr("Delete project {0}{1}? This cannot be undone.", p.slug, tasks),
                          confirm: tr("Delete"), danger: true })) return;
  try {
    await api("DELETE", `/projects/${encodeURIComponent(p.slug)}`);
    if (panelTask?.project === p.slug) { fresh = null; closeTask(); }
    if (state.project === p.slug) setProject("all"); else loadBoard();
  } catch (err) { toast(err.message); }
}

// The project whose own columns the columns dialog manages.
let columnsProject = null;

// The add form's picker; POST sends its value.
iconPicker($("#column-icon"), "");

function resetColumnForm() {
  $("#column-form").reset();
  setIconPick($("#column-icon"), "");
}

function openColumns(p) {
  columnsProject = p.slug;
  $("#columns-title").textContent = `Columns of ${p.name}`;
  resetColumnForm();
  $("#columns-list").replaceChildren();
  $("#column-after").replaceChildren();
  $("#columns-dialog").hidden = false;
  $("#column-form [name=name]").focus();
  drawColumns();
}

function closeColumns() {
  columnsProject = null;
  resetColumnForm();
  $("#columns-dialog").hidden = true;
}

// The board on screen may show another project, so ask for this one's columns in order.
async function drawColumns() {
  const slug = columnsProject;
  if (!slug) return;
  let board;
  try {
    board = await api("GET", "/board?project=" + encodeURIComponent(slug));
  } catch (err) { toast(err.message); return; }
  if (slug !== columnsProject) return;
  const mine = (board.columns || []).filter((c) => c.project === slug);
  $("#columns-list").replaceChildren(...(mine.length ? mine.map((c) =>
    el("div", { class: "account" },
      columnIcon(c.icon),
      el("span", { class: "text" }, el("b", {}, c.name),
        el("span", {}, `${c.description} · after ${c.after}`)),
      columnIconPick(slug, c),
      el("button", { class: "ghost danger", type: "button", title: "Remove column", "aria-label": `Remove the ${c.name} column`,
                     onclick: () => removeColumn(slug, c) }, icon("trash"))))
    : [el("div", { class: "note" }, "No columns of its own yet")]));
  const after = $("#column-after");
  const picked = after.value;
  const options = board.statuses.filter((s) => s !== "done");
  after.replaceChildren(...options.map((s) => el("option", { value: s }, `After ${s}`)));
  after.value = options.includes(picked) ? picked : options.includes("review") ? "review" : options[options.length - 1] || "";
}

// An existing column's icon picker; the row's leading icon follows it live.
function columnIconPick(slug, c) {
  const button = el("button", { "aria-label": `Icon of the ${c.name} column`, title: "Icon" });
  return iconPicker(button, c.icon, async (name) => {
    const row = button.closest(".account");
    if (row) row.firstChild.replaceWith(columnIcon(name));
    try {
      await api("PATCH", `/projects/${encodeURIComponent(slug)}/statuses/${encodeURIComponent(c.name)}`, { icon: name });
      await loadBoard();
    } catch (err) { toast(err.message); }
    drawColumns();
  });
}

async function removeColumn(slug, c) {
  if (!await askConfirm({ title: "Remove column?", message: `Remove the ${c.name} column from ${slug}?`,
                          confirm: "Remove", danger: true })) return;
  try {
    await api("DELETE", `/projects/${encodeURIComponent(slug)}/statuses/${encodeURIComponent(c.name)}`);
    await loadBoard();
    drawColumns();
  } catch (err) { toast(err.message); }
}

$("#column-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const slug = columnsProject;
  if (!slug) return;
  const form = e.target;
  const name = $("[name=name]", form), description = $("[name=description]", form);
  try {
    await api("POST", `/projects/${encodeURIComponent(slug)}/statuses`,
              { name: name.value.trim(), description: description.value.trim(), after: $("#column-after").value || undefined,
                icon: $("#column-icon").value || undefined });
    name.value = "";
    description.value = "";
    setIconPick($("#column-icon"), "");
    await loadBoard();
    await drawColumns();
    name.focus();
  } catch (err) { toast(err.message); }
});
$("#columns-close").addEventListener("click", closeColumns);

function renderColumns() {
  const { statuses, projects, tasks } = state.board;
  const columns = $("#columns");
  const inList = isList() && projects.length > 0;
  columns.hidden = inList;
  $("#list").hidden = $("#list-tools").hidden = !inList;
  if (!projects.length) {
    state.layout = "";
    columns.style.display = "flex";
    columns.replaceChildren(el("div", { class: "empty" },
      el("h1", {}, tr("Welcome to Kanzo")),
      el("p", {}, tr("Create your first project in the sidebar. Link it to a GitHub repo and agents working in that repo will find it on their own.")),
      el("button", { class: "btn primary", onclick: showNewProject }, tr("New project"))));
    return;
  }
  columns.style.display = "";

  // Build the columns once per layout so a refresh never wipes a half-typed task. A new
  // column icon changes the layout too, so headers redraw with it.
  const icons = (state.board.columns || []).map((c) => c.icon || "").join(",");
  const layout = statuses.join(",") + "|" + state.project + "|" + icons;
  if (state.layout !== layout) {
    state.layout = layout;
    columns.replaceChildren(...statuses.map((status) => {
      const collapsible = status === "backlog" || status === "done";
      const col = el("section", { class: "column", "data-status": status },
        el("h2", {},
          statusIcon(status), tr(status),
          el("span", { class: "count" }),
          collapsible ? el("button", { class: "collapse", onclick: (e) => { e.stopPropagation(); setCollapsed(status, !isCollapsed(status)); } }) : null),
        el("div", { class: "cards" }),
      );
      wireDrop(col, col.querySelector(".cards"), status);
      if (collapsible) col.addEventListener("click", () => { if (isCollapsed(status)) setCollapsed(status, false); });
      return col;
    }));
    showCollapsed();
    $("#bulk-status").replaceChildren(el("option", { value: "", hidden: true }, "Move to…"), ...statuses.map((s) => el("option", { value: s }, s)));
  }

  // During a drag the DOM is being moved by hand; refresh once it ends.
  if (state.dragging || state.sweeping || state.settling) return;
  const all = state.project === "all";
  const progress = subtaskProgress(tasks);
  const humanOnly = isHumanOnly();
  const query = filterQuery();
  $("#filter").classList.toggle("on", query.length > 0);
  // Filtered, the server's count of older done tasks says nothing about human ones.
  const filtered = humanOnly || query.length > 0;
  const hiddenDone = state.allDone ? 0 : state.board.done_hidden || 0;
  const more = hiddenDone
    ? el("button", { class: "ghost more", onclick: () => { state.allDone = true; loadBoard(); } },
         filtered ? tr("Show older") : tr("Show {0} older", hiddenDone))
    : null;
  if (inList) {
    renderList(tasks.filter((t) => !humanOnly || t.human), all, more, progress);
  } else {
    for (const status of statuses) {
      const col = columns.querySelector(`[data-status="${CSS.escape(status)}"]`);
      const mine = tasks.filter((t) => t.status === status && (!humanOnly || t.human));
      const last = status === statuses[statuses.length - 1];
      const count = mine.length + (!last || filtered ? 0 : hiddenDone);
      col.querySelector("h2 .count").textContent = count ? String(count) : "";
      const description = columnDescription(status);
      if (description) col.querySelector("h2").title = description;
      else col.querySelector("h2").removeAttribute("title");
      col.querySelector(".cards").replaceChildren(...(last && more ? [more] : []),
        ...mine.map((t) => card(t, all, progress.get(key(t)))));
    }
  }
  highlight("filter", inList ? $("#list") : columns, wordsOf(query));
  for (const id of [...state.selected]) {
    if (!document.querySelector(`.card[data-id="${id}"]`)) state.selected.delete(id);
  }
  showSelection();
}

function isList() {
  return localStorageGet("kanzo.view") === "list";
}

// Only the view on screen is rendered: every refresh would otherwise build both.
$("#view").addEventListener("click", () => {
  localStorageSet("kanzo.view", isList() ? "" : "list");
  state.layout = "";
  render();
});
for (const [select, name] of [[$("#list-sort"), "kanzo.sort"], [$("#list-group"), "kanzo.group"]]) {
  select.value = localStorageGet(name) || "";
  select.addEventListener("change", () => { localStorageSet(name, select.value); renderColumns(); });
}

function renderList(tasks, all, more, progress) {
  const { statuses, projects } = state.board;
  const by = $("#list-sort").value, group = $("#list-group").value;
  const sorted = !by ? tasks : [...tasks].sort((a, b) =>
    by === "id" ? a.id - b.id : by === "title" ? a.title.localeCompare(b.title) : b[by].localeCompare(a[by]));
  const groups = new Map((group === "status" ? statuses : group === "project" ? projects.map((p) => p.slug) : []).map((g) => [g, []]));
  for (const t of sorted) {
    for (const g of group === "tags" ? (t.tags.length ? t.tags : [""]) : [group ? t[group] : ""]) {
      if (!groups.has(g)) groups.set(g, []);
      groups.get(g).push(t);
    }
  }
  const names = group === "tags" ? [...groups.keys()].sort((a, b) => !a - !b || a.localeCompare(b)) : [...groups.keys()];
  $("#list").replaceChildren(...names.flatMap((g) => !groups.get(g).length ? [] : [
    group ? el("h2", { style: group === "status" ? "text-transform:capitalize" : null }, g || "No tag", el("span", { class: "count" }, String(groups.get(g).length))) : null,
    ...groups.get(g).map((t) => card(t, all, progress.get(key(t)), true)),
  ]).filter(Boolean), ...(more ? [more] : []));
}

function showSelection() {
  $("#bulk").hidden = !state.selected.size;
  $("#bulk-count").textContent = `${state.selected.size} selected`;
}

function setSelected(id, on) {
  if (on) state.selected.add(id); else state.selected.delete(id);
  // Grouped by tag, a task can have several rows.
  document.querySelectorAll(`.card[data-id="${id}"]`).forEach((n) => n.classList.toggle("selected", on));
}

function clearSelection() {
  state.selected.forEach((id) => setSelected(id, false));
  showSelection();
}

// Closes the panel: one or the other, so the bar can't cover it.
function toggleSelected(id) {
  if (state.task) closeTask();
  setSelected(id, !state.selected.has(id));
  showSelection();
}

// Drag on empty board space to select, with ⌘ to add to what's selected. Cards are measured once, when
// the drag starts, so moving the mouse never forces a layout.
function sweep(down) {
  const marquee = el("div", { class: "marquee" });
  const kept = new Set(down.metaKey || down.ctrlKey ? state.selected : []);
  let cards, x, y, frame = 0;
  const paint = () => {
    frame = 0;
    const x1 = Math.min(down.clientX, x), x2 = Math.max(down.clientX, x), y1 = Math.min(down.clientY, y), y2 = Math.max(down.clientY, y);
    marquee.style.cssText = `left:${x1}px;top:${y1}px;width:${x2 - x1}px;height:${y2 - y1}px`;
    for (const { id, r, list } of cards) {
      // A card scrolled out of its column can't be under the box.
      const hit = r.right > x1 && r.left < x2 && r.bottom > Math.max(y1, list.top) && r.top < Math.min(y2, list.bottom);
      if ((hit || kept.has(id)) !== state.selected.has(id)) setSelected(id, !state.selected.has(id));
    }
    showSelection();
  };
  const move = (e) => {
    x = e.clientX; y = e.clientY;
    if (!cards) {
      if (Math.hypot(x - down.clientX, y - down.clientY) < 4) return;
      state.sweeping = true;
      document.body.append(marquee);
      cards = [...$("#columns").querySelectorAll(".column:not(.collapsed) .card")].map((n) =>
        ({ id: +n.dataset.id, r: n.getBoundingClientRect(), list: n.parentNode.getBoundingClientRect() }));
    }
    if (!frame) frame = requestAnimationFrame(paint);
  };
  const up = () => {
    cancelAnimationFrame(frame);
    document.removeEventListener("mousemove", move);
    document.removeEventListener("mouseup", up);
    marquee.remove();
    state.sweeping = false;
    if (cards) renderColumns();
    else if (!kept.size) clearSelection();
  };
  document.addEventListener("mousemove", move);
  document.addEventListener("mouseup", up);
}

$("#columns").addEventListener("mousedown", (e) => {
  const onScrollbar = e.offsetX > e.target.clientWidth || e.offsetY > e.target.clientHeight;
  if (e.button || onScrollbar || e.target.closest(".card, button, a, .collapsed")) return;
  e.preventDefault();
  sweep(e);
});

async function changeSelected(change) {
  const ids = state.board.tasks.filter((t) => state.selected.has(t.id)).map((t) => t.id);
  try { await api("POST", "/tasks/bulk", { ids, ...change }); } catch (err) { toast(err.message); }
  loadBoard();
}

$("#bulk").addEventListener("keydown", (e) => {
  const { name, value } = e.target;
  if (e.key === "Escape") { e.stopPropagation(); e.target.blur(); clearSelection(); }
  else if (e.key === "Enter" && name && value.trim()) { changeSelected({ [name]: value }); e.target.value = ""; }
});
$("#bulk-status").addEventListener("change", (e) => {
  changeSelected({ status: e.target.value });
  e.target.value = "";
});
$("#bulk-delete").addEventListener("click", async () => {
  if (await askConfirm({ title: tr("Delete tasks?"), confirm: tr("Delete"), danger: true,
                         message: `Delete ${plural(state.selected.size, "task")}, with comments and images? This cannot be undone.` })) changeSelected({ delete: true });
});
$("#bulk-clear").addEventListener("click", clearSelection);

function savedFilters() {
  try { return JSON.parse(localStorageGet("kanzo.filters")) || []; } catch { return []; }
}

function setFilters(filters) {
  localStorageSet("kanzo.filters", JSON.stringify(filters));
  loadBoard();
}

function filterQuery() {
  return [...savedFilters(), $("#filter").value].join(" ").trim();
}

const anyText = () => true;
const isNumber = (v) => /^[+-]?\d+$/.test(v);
const isDate = (v) => /^\d{4}-\d{2}-\d{2}$/.test(v);
const inRange = (side) => (v) => {
  const ends = v.split("..");
  return ends.length > 1 ? ends.length === 2 && ends.every(side) : side(v.replace(/^(>=|<=|>|<)/, ""));
};
const isField = (v) => ["description", "tags", "url", "comments", "images"].includes(v);
// What the server takes as each qualifier's value: anything else makes it a plain word.
const QUALIFIERS = new Map([
  ["tag", anyText], ["status", anyText], ["project", anyText], ["title", anyText], ["description", anyText],
  ["url", anyText], ["comment", anyText], ["id", inRange(isNumber)], ["comments", inRange(isNumber)],
  ["images", inRange(isNumber)], ["created", inRange(isDate)], ["updated", inRange(isDate)],
  ["is", (v) => v === "human" || v === "waiting"], ["has", isField], ["no", isField],
]);

// A part of a query, read as the server reads it. Null for a word.
function qualifierIn(part) {
  const match = !part.startsWith('"') && part.replace(/"/g, "").toLowerCase().match(/^(-?)([^:]+):(.*)$/);
  const valid = match && QUALIFIERS.get(match[2]);
  if (!valid) return null;
  const [, negated, name, value] = match;
  // With no value yet it's still being typed; with a wrong one, it's a word.
  return !value || valid(value) ? { negated: negated === "-", name, value } : null;
}

// The text to highlight on cards: words, phrases, and what tag:, title: and description: look for.
function wordsOf(query) {
  const words = [];
  for (const part of query.match(/(?:[^\s"]+|"[^"]*"?)+/g) || []) {
    const qualifier = qualifierIn(part);
    if (!qualifier) words.push(part.replace(/"/g, "").toLowerCase());
    else if (!qualifier.negated && ["tag", "title", "description"].includes(qualifier.name)) words.push(qualifier.value);
  }
  return words.filter(Boolean);
}

// A query as text, with its qualifiers marked. Keeps every character, for .query-mirror to line up.
function queryNodes(query) {
  return (query.match(/\s+|(?:[^\s"]+|"[^"]*"?)+/g) || []).flatMap((part) => {
    if (!qualifierIn(part)) return part;
    const key = part.indexOf(":") + 1;
    return [el("span", { class: "qualifier-key" }, part.slice(0, key)), part.slice(key)];
  });
}

// An input can't color part of its text, so a copy behind it shows it, and the input keeps the caret.
function paintQuery(input) {
  const mirror = input.previousElementSibling;
  mirror.firstElementChild.replaceChildren(...queryNodes(input.value));
  mirror.scrollLeft = input.scrollLeft;
}

for (const input of document.querySelectorAll(".query input")) {
  for (const type of ["input", "scroll", "keyup", "click"]) input.addEventListener(type, () => paintQuery(input));
}

function highlight(name, root, words) {
  if (!window.Highlight) return;
  const re = new RegExp(words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|"), "gi");
  const ranges = [];
  for (const node of words.length ? root.querySelectorAll(".title, .desc, .tag") : []) {
    const text = node.firstChild;
    for (const m of text?.nodeType === Node.TEXT_NODE ? text.data.matchAll(re) : []) {
      const range = new Range();
      range.setStart(text, m.index);
      range.setEnd(text, m.index + m[0].length);
      ranges.push(range);
    }
  }
  CSS.highlights.set(name, new Highlight(...ranges));
}

function summary(t, ...extra) {
  const chips = [...extra.filter(Boolean), ...(isPropertyHidden("tags") ? [] : t.tags.map((tag) => el("span", { class: "tag" }, tag)))];
  return [
    t.description ? el("span", { class: "desc" }, plainText(t.description)) : null,
    chips.length ? el("span", { class: "tags" }, chips) : null,
    t.assignee ? assigneeTag(t.assignee) : null,
  ];
}

// Only on cards: search rows are buttons, and a link can't sit inside one.
function urlChip(t) {
  if (!t.url || isPropertyHidden("url")) return null;
  return el("a", { class: "tag url", href: t.url, target: "_blank", rel: "noopener", title: t.url, draggable: "false",
                   onclick: (e) => e.stopPropagation() },
    icon("url"), t.url.split("/")[2].replace(/^www\./, ""));
}

// Only on cards, like the URL. Most tasks have no waiting_on at all, to keep the board's reply small.
function waitChip(t) {
  return t.waiting_on ? el("span", { class: "waits" }, icon("waits"), `Waits on ${t.waiting_on.map((id) => `#${id}`).join(", ")}`) : null;
}

// A board on an older version sends no hidden_properties.
function isPropertyHidden(property) {
  return (state.board?.hidden_properties || []).includes(property);
}

// Queued for AI agents, or held by an agent or orchestrator, shows a robot; people keep the person.
function isBot(name) {
  const account = ((state.board && state.board.accounts) || []).find((a) => a.name === name);
  return name === AI_AGENTS || Boolean(account && account.kind !== "human");
}

function assigneeTag(name) {
  const queued = name === AI_AGENTS;
  return el("span", { class: "assignee", title: queued ? tr("Queued for AI agents") : tr("Assigned to {0}", name) },
    icon(isBot(name) ? "bot" : "human"), name);
}

function isHumanOnly() {
  return localStorageGet("kanzo.humanOnly") === "1" && !isPropertyHidden("human");
}

function setHumanOnly(humanOnly) {
  localStorageSet("kanzo.humanOnly", humanOnly ? "1" : "0");
  render();
}

function isCollapsed(status) {
  return localStorageGet(`kanzo.${status}Collapsed`) !== "0";
}

function setCollapsed(status, collapsed) {
  localStorageSet(`kanzo.${status}Collapsed`, collapsed ? "1" : "0");
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
    button.title = collapsed ? tr("Show {0}", tr(status)) : tr("Collapse {0}", tr(status));
    button.setAttribute("aria-label", button.title);
  }
  columns.style.gridTemplateColumns = cols.map((c) => (c.classList.contains("collapsed") ? "48px" : "minmax(260px, 1fr)")).join(" ");
}

function plainText(md) {
  return md.split("\n")
    .map((l) => l.replace(/^\s*(#{1,6}|>|[-*+]|\d+[.)])\s+/, "").replace(/^\[[ xX]\]\s+/, ""))
    .filter((l) => l.trim() && !/^\s*```/.test(l))
    .join(" ")
    .replace(/\*\*|__|`/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

// Dragging a selected card takes the others along: they fly behind it, and spread out where it lands.
const MOTION = { duration: 180, easing: "ease-out" };
const MOST_FLYING = 30;
const STACK_TOP = 8;

function calm() {
  return matchMedia("(prefers-reduced-motion: reduce)").matches;
}

function inView(card) {
  const r = card.getBoundingClientRect(), list = card.parentNode.getBoundingClientRect();
  return r.width > 0 && r.bottom > list.top && r.top < list.bottom;
}

// The browser draws the dragged image, and only from what's on the page when the drag starts.
function stackImage(card, count) {
  const stack = el("div", { class: "stack", style: `width:${card.offsetWidth}px` },
    card.cloneNode(true), el("span", { class: "stack-count" }, String(count)));
  document.body.append(stack);
  requestAnimationFrame(() => stack.remove());
  return stack;
}

// A copy above the board, so that its column can't clip it.
function fly(card, from, to, fade) {
  const { width, height } = card.getBoundingClientRect();
  const ghost = card.cloneNode(true);
  ghost.classList.add("flying");
  ghost.style.cssText = `left:${from.left}px;top:${from.top}px;width:${width}px;height:${height}px`;
  document.body.append(ghost);
  const end = `translate(${to.left - from.left}px,${to.top - from.top}px)`;
  const flight = ghost.animate([{ transform: "none", opacity: 1 }, { transform: end, opacity: fade ? 0 : 1 }], MOTION);
  return new Promise((done) => { flight.onfinish = () => { ghost.remove(); done(); }; });
}

// Makes a change to the board, then glides the cards it moved from where they were.
function shift(change) {
  const tops = new Map(calm() ? [] : [...$("#columns").querySelectorAll(".card:not(.gathered)")].filter(inView)
    .map((c) => [c, c.getBoundingClientRect().top]));
  change();
  tops.forEach((top, c) => {
    const r = c.getBoundingClientRect();
    if (r.width && r.top !== top) c.animate([{ transform: `translateY(${top - r.top}px)` }, { transform: "none" }], MOTION);
  });
}

function gather(group, grabbed) {
  const to = grabbed.getBoundingClientRect();
  const others = group.filter((c) => c !== grabbed);
  if (!calm()) others.filter(inView).slice(0, MOST_FLYING).forEach((c) => fly(c, c.getBoundingClientRect(), to, true));
  shift(() => {
    grabbed.classList.add("dragging");
    others.forEach((c) => c.classList.add("dragging", "gathered"));
  });
}

// `place` puts the cards where they land. A refresh waits, as it would replace what's flying.
async function spread(group, grabbed, from, place) {
  state.settling = true;
  shift(() => {
    place();
    group.forEach((c) => c.classList.remove("dragging", "gathered"));
  });
  const others = calm() ? [] : group.filter((c) => c !== grabbed && inView(c)).slice(0, MOST_FLYING);
  const flying = calm() || !inView(grabbed) ? others : [...others, grabbed];
  await Promise.all(flying.map((c) => {
    const landed = fly(c, from, c.getBoundingClientRect(), false);
    c.classList.add("landing");
    return landed.then(() => c.classList.remove("landing"));
  }));
  state.settling = false;
}

// Each parent's subtasks on the board, by the parent's key, as { done, total }. Done tasks the board
// hides are missing, so it may undercount.
function subtaskProgress(tasks) {
  const progress = new Map();
  for (const t of tasks) {
    if (!t.parent) continue;
    const p = progress.get(t.parent) || { done: 0, total: 0 };
    p.total++;
    if (t.status === "done") p.done++;
    progress.set(t.parent, p);
  }
  return progress;
}

// In the list, a row: no drag to reorder, since a sorted list has no one position to drop into.
// `progress` is the card's subtasks, as subtaskProgress() counts them, if it has any.
function card(t, showProject, progress, row) {
  const node = el("div", { class: "card" + (row ? " row" : "") + (state.task === t.id ? " open" : "") + (t.waiting_on ? " waiting" : "") + (state.selected.has(t.id) ? " selected" : ""),
                           draggable: row ? null : "true", "data-id": t.id,
                           onclick: (e) => { if (e.metaKey || e.ctrlKey) toggleSelected(t.id); else openTask(t.id); } },
    row ? statusIcon(t.status) : null,
    el("div", { class: "head" },
      el("div", { class: "title" }, t.title),
      t.human ? el("span", { class: "human-mark", title: tr("Human task: agents skip it") }, icon("human")) : null,
      progress ? el("span", { class: "progress" + (progress.done === progress.total ? " complete" : ""),
                              title: tr("{0} of {1} done", progress.done, plural(progress.total, "subtask")) },
                    `☑ ${progress.done}/${progress.total}`) : null,
      el("span", { class: "num", title: tr("Copy {0}", key(t)), onclick: (e) => { e.stopPropagation(); copyKey(key(t)); } },
        showProject ? el("span", { class: "dot", style: `background:${projectColor(t.project)}` }) : null, `#${t.id}`)),
    t.parent ? el("div", { class: "parent-line", title: tr("Subtask of {0}", t.parent) }, `↳ ${t.parent}`) : null,
    ...summary(t, waitChip(t), urlChip(t)).slice(row ? 1 : 0));
  node.addEventListener("dragstart", (e) => {
    state.dragging = t.id;
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", String(t.id));
    const selected = state.selected.has(t.id) ? [...$("#columns").querySelectorAll(".card.selected")] : [];
    const group = state.group = selected.length > 1 ? selected : null;
    if (group) {
      const r = node.getBoundingClientRect();
      state.grip = { x: e.clientX - r.left, y: e.clientY - r.top };
      e.dataTransfer.setDragImage(stackImage(node, group.length), state.grip.x, state.grip.y + STACK_TOP);
    }
    requestAnimationFrame(() => { if (group) gather(group, node); else node.classList.add("dragging"); });
  });
  node.addEventListener("dragend", async () => {
    node.classList.remove("dragging");
    document.querySelectorAll(".drop-line").forEach((d) => d.remove());
    state.dragging = null;
    const { dropped, group } = state;
    state.dropped = false;
    state.group = null;
    // A drop reloads once the move is saved; a cancelled drag puts the card back.
    if (dropped) return;
    if (group) await spread(group, node, node.getBoundingClientRect(), () => {});
    loadBoard();
  });
  return node;
}

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
    const spot = line.parentNode === list ? line : null;
    state.dropped = true;
    if (fresh === id) fresh = null;
    const before = after ? Number(after.dataset.id) : null;
    const { group } = state;
    if (group) {
      const from = { left: e.clientX - state.grip.x, top: e.clientY - state.grip.y };
      const ids = group.map((c) => Number(c.dataset.id));
      const landed = spread(group, dragged, from, () => {
        group.forEach((c) => list.insertBefore(c, spot));
        line.remove();
      });
      try {
        await api("POST", "/tasks/move", { ids, status, before });
        if (ids.includes(state.task)) refreshTask();
      } catch (err) { toast(err.message); }
      await landed;
      loadBoard();
      return;
    }
    if (dragged) list.insertBefore(dragged, spot);
    line.remove();
    try {
      await api("POST", `/tasks/${id}/move`, { status, before });
      if (state.task === id) refreshTask();
    } catch (err) { toast(err.message); }
    loadBoard();
  });
}

const NEW_TITLE = "New task";
let fresh = null;

async function newTask() {
  const { projects } = state.board;
  if (!projects.length) { toast(tr("Create a project first")); return; }
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
  // The panel may list it as a subtask of the task now open.
  api("DELETE", `/tasks/${id}`).then(() => { loadBoard(); refreshTask(); }, () => {});
}

// A reload or a closed tab may never blur the field being typed in.
window.addEventListener("pagehide", () => {
  const active = document.activeElement;
  const fields = !panelTask ? null
    : active === $("#p-desc") && active.value.trim() !== panelTask.description ? { description: active.value }
    : active === $("#p-tags") && cleanTags(active.value).join() !== panelTask.tags.join() ? { tags: cleanTags(active.value).join() }
    : null;
  if (fields) {
    fresh = null;
    fetch(`/api/tasks/${panelTask.id}`, { method: "PATCH", keepalive: true,
                                          headers: { "Content-Type": "application/json" }, body: JSON.stringify(fields) });
  }
  if (fresh) fetch(`/api/tasks/${fresh}`, { method: "DELETE", keepalive: true });
});

// `template` presets "Start from"; the select hides when no project is a template.
function showNewProject(template) {
  const select = $("#new-project-template");
  const templates = state.board ? state.board.projects.filter((p) => p.template) : [];
  select.replaceChildren(el("option", { value: "" }, "Blank project"),
    ...templates.map((p) => el("option", { value: p.slug }, p.name)));
  select.value = typeof template === "string" && templates.some((p) => p.slug === template) ? template : "";
  $("#new-project-start").hidden = !templates.length;
  $("#project-dialog").hidden = false;
  $("#new-project [name=slug]").focus();
}

function hideNewProject() {
  $("#new-project").reset();
  $("#project-dialog").hidden = true;
}

$("#new-task").addEventListener("click", newTask);
$("#human-only").addEventListener("click", () => setHumanOnly(!isHumanOnly()));
let filterTimer;
$("#filter").addEventListener("input", () => { clearTimeout(filterTimer); filterTimer = setTimeout(loadBoard, 120); });
$("#filter").addEventListener("keydown", (e) => {
  const box = e.target, text = box.value.trim();
  if (e.key === "Escape") { e.stopPropagation(); box.value = ""; paintQuery(box); loadBoard(); box.blur(); }
  else if (e.key === "Enter" && text) { box.value = ""; paintQuery(box); setFilters([...savedFilters(), text]); }
  else if (e.key === "Backspace" && !box.value && savedFilters().length) setFilters(savedFilters().slice(0, -1));
});
$("#new-project-toggle").addEventListener("click", () => showNewProject());
$("#new-project-cancel").addEventListener("click", hideNewProject);
$("#new-project").addEventListener("submit", async (e) => {
  e.preventDefault();
  const f = new FormData(e.target);
  try {
    const p = await api("POST", "/projects", { slug: f.get("slug"), name: f.get("name"), repo: f.get("repo") || null,
                                               template: f.get("template") || null });
    hideNewProject();
    setProject(p.slug);
  } catch (err) { toast(err.message); }
});

const panel = $("#panel");
let panelTask = null;
let panelWaits = "";
// Unsent comments by task, so closing the panel doesn't lose one.
const drafts = new Map();

const taskLink = (t) => el("button", { type: "button", class: "tag", title: key(t), onclick: () => pick(t) },
  statusIcon(t.status), el("span", {}, t.title));

async function openTask(id) {
  clearSelection();
  leavePanelField();
  if (fresh !== id) dropFresh();
  state.task = id;
  document.querySelectorAll(".card.open").forEach((c) => c.classList.remove("open"));
  document.querySelector(`.card[data-id="${id}"]`)?.classList.add("open");
  $("#p-comment").value = drafts.get(id) || "";
  $("#p-comment-status").value = "";
  $("#p-comment-agents").checked = false;
  pending = [];
  renderPending();
  updateHash();
  await refreshTask();
  showFocusedComment();
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
    if (state.task === id && /no task/.test(e.message)) { toast(tr("That task was deleted")); closeTask(); }
    return;
  }
  // Another task may have taken the panel meanwhile.
  if (state.task !== id) return;
  // A board on an older version sends neither list.
  const { task, comments, images, waits_on = [], blocks = [], children = [] } = data;
  panelTask = task;
  panelWaits = waits_on.map(key).join(", ");
  $("#p-key").textContent = key(task);
  $("#p-key").title = tr("Copy {0}", key(task));
  // Never overwrite what is being typed or picked.
  if (!$("#p-project").contains(document.activeElement)) renderProjectPick(task);
  for (const [field, value] of [[$("#p-title"), task.title], [$("#p-desc"), task.description], [$("#p-tags"), task.tags.join(", ")], [$("#p-url"), task.url || ""], [$("#p-waits"), panelWaits]]) {
    if (document.activeElement !== field) { field.value = value; fit(field); }
  }
  showDesc();
  const status = $("#p-status");
  const statuses = state.board ? projectStatuses(task.project) : [task.status];
  status.replaceChildren(...statuses.map((s) => el("option", { value: s }, s)));
  if (!statuses.includes(task.status)) status.append(el("option", { value: task.status }, task.status));
  status.value = task.status;
  renderCommentStatus(task, statuses);
  renderAssigneePick(task);
  renderParent(task);
  renderSubtasks(children);
  $("#p-human").checked = task.human;
  $("#p-waits-links").replaceChildren(...waits_on.map(taskLink));
  $("#p-blocks").replaceChildren(...blocks.map(taskLink));
  $("#p-blocks-row").hidden = !blocks.length;
  $("#p-created").replaceChildren(timeEl(task.created_at));
  $("#p-updated").replaceChildren(timeEl(task.updated_at));
  const notes = comments.filter((c) => c.moved_to || c.assigned_to != null).length;
  $("#p-comment-count").textContent = comments.length > notes ? String(comments.length - notes) : "";
  $("#p-images").replaceChildren(...images.filter((i) => i.comment_id === null).map((i) =>
    thumb(i.file, async () => {
      try { await api("DELETE", `/images/${i.id}`); refreshTask(); } catch (err) { toast(err.message); }
    })));
  $("#p-comments").replaceChildren(...comments.map((c) => {
    if (c.moved_to) {
      return el("div", { class: "move-note" }, icon("moved"),
        el("span", {}, "moved from ", el("b", {}, c.moved_from), " to ", el("b", {}, c.moved_to),
          ` by ${c.author} · `, timeEl(c.created_at)));
    }
    if (c.assigned_to != null) return assignNote(c);
    const body = el("div", { class: "md" });
    body.innerHTML = markdown(c.body, task.project);
    const attached = images.filter((i) => i.comment_id === c.id);
    const link = `${key(task)}/c${c.id}`;
    return el("div", { class: "comment", id: `c${c.id}` },
      el("div", { class: "who" }, el("b", {}, c.author), " · ", timeEl(c.created_at),
        el("button", { class: "remove copy-link", title: tr("Copy a link to this comment ({0})", link), "aria-label": tr("Copy a link to this comment"),
                       onclick: () => copyKey(link) }, icon("link")),
        el("button", { class: "remove", title: tr("Delete comment"), "aria-label": tr("Delete comment"),
                       onclick: () => deleteComment(c) }, icon("trash"))),
      c.body ? body : null,
      attached.length ? el("div", { class: "thumbs" }, attached.map((i) => thumb(i.file))) : null);
  }));
}

// An assignment change, as a slim line like a move note; "" on either side means nobody.
function assignNote(c) {
  const from = c.assigned_from || "", to = c.assigned_to || "";
  const name = (n) => el("b", { class: "name" }, n);
  const what = !from ? ["assigned to ", name(to)]
    : !to ? ["unassigned from ", name(from)]
    : ["reassigned from ", name(from), " to ", name(to)];
  return el("div", { class: "move-note" }, icon(isBot(to || from) ? "bot" : "human"),
    el("span", {}, ...what, ` by ${c.author} · `, timeEl(c.created_at)));
}

async function deleteComment(c) {
  if (!await askConfirm({ title: tr("Delete comment?"), message: tr("Delete this comment? This cannot be undone."),
                          confirm: tr("Delete"), danger: true })) return;
  try {
    await api("DELETE", `/comments/${c.id}`);
    await refreshTask();
    loadBoard();
  } catch (err) { toast(err.message); }
}

function renderProjectPick(task) {
  const projects = state.board ? state.board.projects.map((p) => p.slug) : [];
  if (!projects.includes(task.project)) projects.push(task.project);
  const select = el("select", { title: tr("Project"), "aria-label": tr("Project") },
    ...projects.map((slug) => el("option", { value: slug }, slug)));
  select.value = task.project;
  select.addEventListener("change", () => {
    localStorageSet("kanzo.newTaskProject", select.value);
    patchTask({ project: select.value });
  });
  $("#p-project").replaceChildren(el("label", { class: "tag project-pick" },
    el("span", { class: "dot", style: `background:${projectColor(task.project)}` }), select));
}

function accountLabel(a) {
  return a.kind === "agent" ? a.name : `${a.name} (${KIND_LABELS[a.kind] || a.kind})`;
}

function renderAssigneePick(task) {
  const select = $("#p-assignee");
  const accounts = (state.board && state.board.accounts) || [];
  const humans = accounts.filter((a) => a.kind === "human");
  select.replaceChildren(el("option", { value: "" }, "Unassigned"),
    el("option", { value: AI_AGENTS }, AI_AGENTS),
    ...humans.map((a) => el("option", { value: a.name }, accountLabel(a))));
  // A task agent or orchestrator holding it shows only while it does.
  if (task.assignee && task.assignee !== AI_AGENTS && !humans.some((a) => a.name === task.assignee)) {
    const account = accounts.find((a) => a.name === task.assignee);
    const label = account ? `${account.name} (${KIND_LABELS[account.kind] || account.kind})` : task.assignee;
    select.append(el("option", { value: task.assignee }, label));
  }
  select.value = task.assignee || "";
}

// Tasks met off the board, by key: a parent elsewhere still shows its title and status.
const knownTasks = new Map();
const loadingTasks = new Set();

// A task's `parent` is its parent's key; ids are board-wide, so its number alone finds it.
const keyId = (k) => Number(String(k).split("#").pop()) || null;

function boardTask(k) {
  return (state.board && state.board.tasks.find((t) => key(t) === k)) || null;
}

function loadKnownTask(k) {
  if (loadingTasks.has(k)) return;
  loadingTasks.add(k);
  api("GET", `/tasks/${keyId(k)}`).then(({ task }) => {
    knownTasks.set(k, task);
    if (panelTask && panelTask.parent === k) renderParent(panelTask);
  }, () => {}).then(() => loadingTasks.delete(k));
}

// While the picker is open, refreshes leave the field alone.
let parentPicking = false;
let parentOptions = [], parentSelected = 0, parentRequest = 0, parentTimer;

function renderParent(task) {
  if (parentPicking) return;
  const field = $("#p-parent");
  if (!task.parent) {
    field.replaceChildren(el("button", { type: "button", class: "ghost parent-none", title: "Pick a parent task",
                                         onclick: openParentPick }, "None"));
    return;
  }
  const parent = boardTask(task.parent);
  if (!parent) loadKnownTask(task.parent);
  const shown = parent || knownTasks.get(task.parent);
  const id = keyId(task.parent);
  field.replaceChildren(el("span", { class: "parent-row" },
    el("a", { class: "parent-link", href: taskHref(id), "data-task": id, title: `Open ${task.parent}` },
      shown ? statusIcon(shown.status) : null, el("span", { class: "mono" }, task.parent),
      shown ? el("span", { class: "title" }, shown.title) : null),
    el("button", { type: "button", class: "ghost", title: "Change parent", "aria-label": "Change parent",
                   onclick: openParentPick }, icon("rename")),
    el("button", { type: "button", class: "ghost", title: "Remove parent", "aria-label": "Remove parent",
                   onclick: () => patchTask({ parent: "" }) }, icon("close"))));
}

function openParentPick() {
  if (!panelTask) return;
  parentPicking = true;
  const input = el("input", { class: "field", placeholder: "Find a task by key or title", autocomplete: "off",
                              spellcheck: "false", "aria-label": "Parent task" });
  input.addEventListener("input", () => findParents(input.value));
  input.addEventListener("keydown", parentKeydown);
  input.addEventListener("blur", closeParentPick);
  $("#p-parent").replaceChildren(el("div", { class: "parent-pick" }, input,
    el("div", { class: "dropdown-list", role: "listbox" })));
  input.focus();
  findParents("");
}

function closeParentPick() {
  if (!parentPicking) return;
  parentPicking = false;
  clearTimeout(parentTimer);
  parentRequest++;
  if (panelTask) renderParent(panelTask);
}

// The board's matches show at once; from two characters on, the server's search adds tasks beyond it.
function findParents(q) {
  const words = wordsOf(q);
  const project = panelTask.project;
  const local = (state.board ? state.board.tasks : [])
    .filter((t) => words.every((w) => `${key(t)} ${t.title}`.toLowerCase().includes(w)))
    .sort((a, b) => (b.project === project) - (a.project === project));
  const request = ++parentRequest;
  clearTimeout(parentTimer);
  const searching = q.trim().length >= 2;
  showParentOptions(local, q, searching);
  if (!searching) return;
  parentTimer = setTimeout(async () => {
    try {
      const found = await api("GET", "/search?q=" + encodeURIComponent(q.trim()));
      if (request !== parentRequest) return;
      const seen = new Set(local.map(key));
      showParentOptions(local.concat(found.filter((t) => !seen.has(key(t)))), q, false);
    } catch (err) { if (request === parentRequest) toast(err.message); }
  }, 120);
}

// Leaves out the task itself and its subtasks, at any depth, as far as their parents are known.
function parentCandidates(tasks) {
  const own = key(panelTask);
  const byKey = new Map();
  for (const t of (state.board ? state.board.tasks : []).concat(tasks, panelChildren)) byKey.set(key(t), t);
  return tasks.filter((t) => {
    for (let k = key(t), hops = 0; k && hops < 100; hops++) {
      if (k === own) return false;
      const up = byKey.get(k);
      k = up ? up.parent : null;
    }
    return true;
  });
}

function showParentOptions(tasks, q, searching) {
  const list = $("#p-parent .dropdown-list");
  if (!list || !panelTask) return;
  parentOptions = parentCandidates(tasks).slice(0, 30);
  parentSelected = 0;
  const empty = searching ? "Searching…" : q.trim() ? "No tasks match." : "No tasks to pick.";
  list.replaceChildren(...parentOptions.map((t, i) =>
    el("button", { type: "button", class: "dropdown-option" + (i === 0 ? " selected" : ""), role: "option",
                   onmousedown: (e) => e.preventDefault(), onmousemove: () => selectParent(i), onclick: () => pickParent(t) },
      statusIcon(t.status), el("span", { class: "mono" }, key(t)), el("span", { class: "title" }, t.title))),
    ...(parentOptions.length ? [] : [el("div", { class: "note" }, empty)]));
}

function selectParent(i) {
  const rows = $("#p-parent").querySelectorAll(".dropdown-option");
  if (!rows.length) return;
  parentSelected = (i + rows.length) % rows.length;
  rows.forEach((r, j) => r.classList.toggle("selected", j === parentSelected));
  rows[parentSelected].scrollIntoView({ block: "nearest" });
}

function pickParent(t) {
  knownTasks.set(key(t), t);
  closeParentPick();
  patchTask({ parent: key(t) });
}

function parentKeydown(e) {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); selectParent(parentSelected + (e.key === "ArrowDown" ? 1 : -1)); }
  else if (e.key === "Enter") { e.preventDefault(); if (parentOptions[parentSelected]) pickParent(parentOptions[parentSelected]); }
  else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); closeParentPick(); }
}

// The open task's subtasks, as the server last sent them.
let panelChildren = [];

function renderSubtasks(children) {
  panelChildren = children;
  const done = children.filter((c) => c.status === "done").length;
  $("#p-subtasks-head").hidden = !children.length;
  $("#p-subtasks-count").textContent = `${done}/${children.length} done`;
  $("#p-children").replaceChildren(...children.map((c) =>
    el("a", { class: "subtask" + (c.status === "done" ? " done" : ""), href: taskHref(c.id), "data-task": c.id, title: `Open ${key(c)}` },
      statusIcon(c.status), el("span", { class: "mono" }, key(c)), el("span", { class: "title" }, c.title),
      c.assignee ? assigneeTag(c.assignee) : null)));
}

const NEW_SUBTASK_TITLE = "New subtask";

// Like newTask(), but under the open task. It gets a history entry, so Back returns to the parent,
// and as with a new task, one left untitled is dropped on the way out.
async function addSubtask() {
  if (!panelTask) return;
  const parent = panelTask;
  // Giving it a subtask keeps a just-made parent.
  if (fresh === parent.id) fresh = null;
  try {
    const task = await api("POST", "/tasks", { project: parent.project, title: NEW_SUBTASK_TITLE, parent: key(parent) });
    history.pushState(null, "", "#" + state.project + "/" + task.id);
    await openTask(task.id);
    fresh = task.id;
    loadBoard();
    $("#p-title").focus({ preventScroll: true });
    $("#p-title").select();
  } catch (err) { toast(err.message); }
}

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

function cancelField(e, saved) {
  e.stopPropagation();
  if (fresh && fresh === state.task) { closeTask(); return; }
  e.target.value = saved;
  fit(e.target);
  e.target.blur();
}

// For writing worth keeping: Esc leaves the field, and leaving it saves. Closing a new task
// saves what was typed, and drops the task if nothing was.
function leaveField(e) {
  e.stopPropagation();
  if (fresh && fresh === state.task) closeTask();
  else e.target.blur();
}

// A page can't open a file: link, so the board does, on the computer it runs on.
panel.addEventListener("click", async (e) => {
  const link = e.target.closest('a[href^="file:///"]');
  if (!link) return;
  e.preventDefault();
  try {
    const path = decodeURIComponent(link.getAttribute("href").slice(7)).replace(/^\/([a-zA-Z]:)/, "$1");
    await api("POST", "/open", { path });
  } catch (err) { toast(err.message); }
});

// "" is the default width. A drag saves and refits only when it ends: browsers send one move a frame.
function setPanelWidth(width, save) {
  panel.style.setProperty("--panel-width", width);
  if (!save) return;
  localStorageSet("kanzo.panelWidth", width);
  [$("#p-title"), $("#p-desc"), $("#p-comment")].forEach(fit);
}

setPanelWidth(localStorageGet("kanzo.panelWidth") || "");
$("#p-wide").addEventListener("click", () => setPanelWidth(panel.style.getPropertyValue("--panel-width") === "100vw" ? "" : "100vw", true));
$("#p-edge").addEventListener("dblclick", () => setPanelWidth("", true));
$("#p-edge").addEventListener("pointerdown", (down) => {
  const edge = down.target;
  edge.setPointerCapture(down.pointerId);
  edge.onpointermove = (e) => setPanelWidth(`${Math.max(360, innerWidth - e.clientX)}px`);
  edge.onpointerup = () => {
    edge.onpointermove = edge.onpointerup = null;
    setPanelWidth(panel.style.getPropertyValue("--panel-width"), true);
  };
});

$("#p-close").addEventListener("click", closeTask);
$("#p-key").addEventListener("click", () => { if (panelTask) copyKey(key(panelTask)); });
// Before the fields' other listeners, so the editors handle Enter and text pastes first.
mdEditor($("#p-desc"));
mdEditor($("#p-comment"));
$("#p-title").addEventListener("input", (e) => fit(e.target));

$("#p-title").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); closeTask(); }
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
  else if (e.key === "Escape") leaveField(e);
});
$("#p-desc").addEventListener("blur", (e) => {
  if (panelTask && e.target.value.trim() !== panelTask.description) patchTask({ description: e.target.value });
  showDesc();
});

// Empty, the editor stays, so its placeholder has something to click.
function showDesc() {
  const desc = $("#p-desc"), view = $("#p-desc-view");
  const editing = document.activeElement === desc || !desc.value.trim();
  desc.hidden = !editing;
  view.hidden = editing;
  // Hidden, it measured 0, as it does when the task before this one had a description.
  if (editing) fit(desc);
  else view.innerHTML = markdown(desc.value, panelTask && panelTask.project);
}

// `at` is where the caret goes, an offset into the description; by default its end.
function editDesc(at) {
  const desc = $("#p-desc");
  $("#p-desc-view").hidden = true;
  desc.hidden = false;
  desc.focus();
  desc.setSelectionRange(typeof at === "number" ? at : desc.value.length);
}

// A click on a link opens it instead of editing; elsewhere it edits, with the caret where it was clicked.
$("#p-desc-view").addEventListener("mousedown", (e) => {
  if (e.target.closest("a")) { e.preventDefault(); return; }
  if (e.button !== 0) return;
  e.preventDefault();
  editDesc(sourceOffsetAt(e.currentTarget, e.clientX, e.clientY, $("#p-desc").value));
});
$("#p-desc-view").addEventListener("focus", () => editDesc());

$("#p-status").addEventListener("change", (e) => patchTask({ status: e.target.value }));
$("#p-human").addEventListener("change", (e) => patchTask({ human: e.target.checked }));
$("#p-assignee").addEventListener("change", (e) => patchTask({ assignee: e.target.value }));
$("#p-add-subtask").addEventListener("click", addSubtask);

let pending = [];

function imagesIn(transfer) {
  if (!transfer) return [];
  const fromItems = transfer.items ? [...transfer.items].filter((i) => i.kind === "file").map((i) => i.getAsFile()) : [];
  return (fromItems.length ? fromItems : [...(transfer.files || [])]).filter((f) => f && f.type.startsWith("image/"));
}

async function uploadImage(image) {
  return (await api("POST", "/images", image)).file;
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

// Images open in a dialog over the board, not in a new tab; the arrow keys step through
// the others in view. Ctrl/Cmd-click still opens the file itself.
let imageList = [], imageAt = 0;

function showImage(i) {
  imageAt = (i + imageList.length) % imageList.length;
  const src = imageList[imageAt];
  $("#image-view").src = src;
  $("#image-original").href = src;
  $("#image-count").textContent = imageList.length > 1 ? `${imageAt + 1} of ${imageList.length}` : "";
}

function openImage(link) {
  const scope = link.closest("#panel, .search-dialog") || document;
  imageList = [...new Set([...scope.querySelectorAll('a[href^="/images/"]')].map((a) => a.getAttribute("href")))];
  showImage(Math.max(imageList.indexOf(link.getAttribute("href")), 0));
  $("#image-dialog").hidden = false;
  $("#image-close").focus();
}

function closeImage() {
  $("#image-dialog").hidden = true;
  $("#image-view").removeAttribute("src");
}

document.addEventListener("click", (e) => {
  const link = e.target.closest('a[href^="/images/"]');
  if (!link || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || link.closest("#image-dialog")) return;
  e.preventDefault();
  openImage(link);
});
$("#image-close").addEventListener("click", closeImage);
$("#image-dialog").addEventListener("keydown", (e) => {
  if (e.key === "ArrowRight") showImage(imageAt + 1);
  else if (e.key === "ArrowLeft") showImage(imageAt - 1);
});

function thumb(file, onRemove) {
  return el("div", { class: "thumb" },
    el("a", { href: `/images/${file}`, target: "_blank", rel: "noopener" }, el("img", { src: `/images/${file}`, alt: tr("Attached image") })),
    onRemove ? el("button", { type: "button", class: "remove-image", title: tr("Remove image"), "aria-label": tr("Remove image"), onclick: onRemove }, icon("close")) : null);
}

function renderPending() {
  $("#p-pending").replaceChildren(...pending.map((file, i) => thumb(file, () => { pending.splice(i, 1); renderPending(); })));
  showCommentStatus();
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

const cleanTags = (text) => [...new Set(text.split(",").map((t) => t.trim().replace(/\s+/g, " ").toLowerCase()).filter(Boolean))];
$("#p-tags").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); e.target.blur(); }
  else if (e.key === "Escape") leaveField(e);
});
$("#p-tags").addEventListener("blur", (e) => {
  if (!panelTask) return;
  const tags = cleanTags(e.target.value);
  if (tags.join(",") !== panelTask.tags.join(",")) patchTask({ tags: tags.join(", ") });
  else e.target.value = panelTask.tags.join(", ");
});

$("#p-url").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); e.target.blur(); }
  else if (e.key === "Escape") cancelField(e, panelTask ? panelTask.url || "" : "");
});
$("#p-url").addEventListener("blur", (e) => {
  if (!panelTask) return;
  const url = e.target.value.trim();
  if (url !== (panelTask.url || "")) patchTask({ url });
  else e.target.value = url;
});

let waitResults = [], waitSelected = 0, waitRequest = 0, waitTimer;

function closeWaits() {
  clearTimeout(waitTimer);
  waitRequest++;
  waitResults = [];
  $("#p-waits-list").hidden = true;
  $("#p-waits").setAttribute("aria-expanded", "false");
}

// The search behind the filter, for what's typed after the last comma.
async function findWaits() {
  const parts = $("#p-waits").value.split(",").map((part) => part.trim());
  const words = parts[parts.length - 1];
  if (!words || !panelTask) { closeWaits(); return; }
  const { id, project } = panelTask;
  const request = ++waitRequest;
  try {
    const found = await api("GET", "/search?q=" + encodeURIComponent(`project:${project} -status:done ${words}`));
    if (request !== waitRequest) return;
    waitResults = found.filter((t) => t.id !== id && !parts.includes(key(t))).slice(0, 8);
    waitSelected = 0;
    const list = $("#p-waits-list");
    list.replaceChildren(...waitResults.map((t, i) => el("button", { type: "button", tabindex: "-1", role: "option", class: "result" + (i === 0 ? " selected" : ""),
                                                                     onmousemove: () => selectWait(i),
                                                                     // Before the field loses focus, which would save half a word.
                                                                     onmousedown: (e) => { e.preventDefault(); pickWait(t); } },
      el("span", { class: "mono" }, `#${t.id}`),
      el("span", { class: "title" }, t.title),
      el("span", { class: "meta" }, t.status))));
    if (!waitResults.length) list.append(el("span", { class: "note" }, "No tasks match."));
    list.hidden = false;
    $("#p-waits").setAttribute("aria-expanded", "true");
    highlight("search", list, wordsOf(words));
  } catch (err) { toast(err.message); }
}

function selectWait(i) {
  const rows = $("#p-waits-list").querySelectorAll(".result");
  waitSelected = (i + rows.length) % rows.length;
  rows.forEach((r, j) => r.classList.toggle("selected", j === waitSelected));
  rows[waitSelected].scrollIntoView({ block: "nearest" });
}

function pickWait(t) {
  const input = $("#p-waits");
  const kept = input.value.split(",").slice(0, -1).map((part) => part.trim()).filter(Boolean);
  input.value = [...kept, key(t)].join(", ") + ", ";
  closeWaits();
}

$("#p-waits").addEventListener("input", () => { clearTimeout(waitTimer); waitTimer = setTimeout(findWaits, 120); });
$("#p-waits").addEventListener("keydown", (e) => {
  const listed = waitResults.length > 0;
  if ((e.key === "ArrowDown" || e.key === "ArrowUp") && listed) { e.preventDefault(); selectWait(waitSelected + (e.key === "ArrowDown" ? 1 : -1)); }
  else if (e.key === "Enter") { e.preventDefault(); if (listed) pickWait(waitResults[waitSelected]); else e.target.blur(); }
  else if (e.key === "Escape" && !$("#p-waits-list").hidden) { e.stopPropagation(); closeWaits(); }
  else if (e.key === "Escape") cancelField(e, panelWaits);
});
$("#p-waits").addEventListener("blur", (e) => {
  closeWaits();
  e.target.value = e.target.value.replace(/[,\s]+$/, "");
  if (panelTask && e.target.value !== panelWaits) patchTask({ waits_on: e.target.value });
});
// The status the task moves to once the comment is sent; "" keeps it. A pick survives refreshes while still valid.
function renderCommentStatus(task, statuses) {
  const pick = $("#p-comment-status");
  const agents = $("#p-comment-agents");
  const queued = task.assignee === AI_AGENTS;
  agents.disabled = queued;
  agents.parentElement.classList.toggle("off", queued);
  if (queued) agents.checked = true;
  else if (agents.dataset.queued) agents.checked = false;
  agents.dataset.queued = queued ? "1" : "";
  if (document.activeElement !== dropdownButton(pick)) {
    const picked = pick.value;
    const moves = statuses.filter((s) => s !== task.status);
    pick.replaceChildren(el("option", { value: "" }, "Change status"),
      ...moves.map((s) => el("option", { value: s }, s[0].toUpperCase() + s.slice(1))));
    pick.value = moves.includes(picked) ? picked : "";
  }
  showCommentStatus();
}

// Send posts the comment, if any, then makes the picked changes; with neither there is nothing to send.
function showCommentStatus() {
  const pick = $("#p-comment-status");
  const agents = $("#p-comment-agents");
  const assign = agents.checked && !agents.disabled;
  const said = !!$("#p-comment").value.trim() || pending.length > 0;
  pick.classList.toggle("set", !!pick.value);
  const parts = [];
  if (pick.value) parts.push(`move to ${pick.value}`);
  if (assign) parts.push(`assign to ${AI_AGENTS}`);
  const action = parts.join(" and ");
  const title = said ? (action ? `Send and ${action}` : "Send") : action && action[0].toUpperCase() + action.slice(1);
  const send = $("#p-send");
  send.disabled = !said && !action;
  send.title = send.disabled ? "Send (⌘↵)" : `${title} (⌘↵)`;
  send.setAttribute("aria-label", said || !action ? "Send comment" : title);
}
$("#p-comment-status").addEventListener("change", showCommentStatus);
$("#p-comment-agents").addEventListener("change", showCommentStatus);

$("#p-comment-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const body = $("#p-comment").value.trim();
  const id = panelTask && panelTask.id, status = $("#p-comment-status").value;
  const agents = $("#p-comment-agents"), assign = agents.checked && !agents.disabled;
  const said = !!body || pending.length > 0;
  if (!panelTask || (!said && !status && !assign)) return;
  try {
    if (said) {
      await api("POST", `/tasks/${id}/comments`, { body, images: pending });
      if (fresh === id) fresh = null;
      drafts.delete(id);
      $("#p-comment").value = "";
      pending = [];
      renderPending();
      fit($("#p-comment"));
    }
    $("#p-comment-status").value = "";
    if (!agents.disabled) agents.checked = false;
    showCommentStatus();
    const fields = {};
    if (status) fields.status = status;
    if (assign) fields.assignee = AI_AGENTS;
    // patchTask refreshes the panel and board itself.
    if (Object.keys(fields).length && panelTask && panelTask.id === id) {
      await patchTask(fields);
      // Finished with: the panel goes, so the board shows. Only once the move saved.
      if (status === "done" && state.task === id && panelTask && panelTask.status === "done") closeTask();
      return;
    }
    await refreshTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});
$("#p-comment").addEventListener("input", (e) => {
  if (e.target.value) drafts.set(state.task, e.target.value); else drafts.delete(state.task);
  showCommentStatus();
  fit(e.target);
});
$("#p-comment").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); $("#p-comment-form").requestSubmit(); }
});

$("#p-delete").addEventListener("click", async () => {
  if (!panelTask) return;
  const extra = panelTask.comments ? tr(" and its {0}", plural(panelTask.comments, "comment")) : "";
  if (!await askConfirm({ title: tr("Delete task?"), message: tr("Delete {0}{1}? This cannot be undone.", key(panelTask), extra),
                          confirm: tr("Delete"), danger: true })) return;
  try {
    if (fresh === panelTask.id) fresh = null;
    await api("DELETE", `/tasks/${panelTask.id}`);
    closeTask();
    loadBoard();
  } catch (err) { toast(err.message); }
});

let results = [], selected = 0, searchRequest = 0, searchTimer;

function openSearch() {
  closeMenu();
  $("#search-dialog").hidden = false;
  $("#search").value = "";
  paintQuery($("#search"));
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
      ...(results.length ? [el("div", { class: "label" }, q ? tr("Tasks") : tr("Recent"))] : []),
      ...results.map((t, i) => el("button", { type: "button", class: "result" + (i === 0 ? " selected" : ""),
                                             onmousemove: () => select(i), onclick: () => pick(t) },
        el("span", { class: "dot", style: `background:${projectColor(t.project)}` }),
        el("span", { class: "body" }, el("span", { class: "title" }, t.title), ...summary(t)),
        el("span", { class: "meta" },
          t.comments ? el("span", { class: "comments", title: plural(t.comments, "comment") },
                          icon("comment"), String(t.comments)) : null,
          el("span", { class: "mono" }, key(t)), t.status))),
      ...(results.length ? [] : [el("div", { class: "note" }, q ? tr("No tasks match.") : tr("No tasks yet."))]));
    highlight("search", $("#search-results"), wordsOf(q));
  } catch (err) { toast(err.message); }
}

function select(i) {
  const rows = $("#search-results").querySelectorAll(".result");
  if (!rows.length) return;
  selected = (i + rows.length) % rows.length;
  rows.forEach((r, j) => r.classList.toggle("selected", j === selected));
  rows[selected].scrollIntoView({ block: "nearest" });
}

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
});

document.addEventListener("keydown", (e) => {
  const typing = e.target.closest('input, textarea, select, .dropdown-button, [contenteditable="true"]');
  const plain = !typing && !e.metaKey && !e.ctrlKey;
  if (e.key === "Escape" && state.menu) { closeMenu(); return; }
  // Only in the installed app: in a browser tab, these keys belong to the browser.
  if (/^\d$/.test(e.key) && (e.metaKey || e.ctrlKey) && state.board && matchMedia("(display-mode: standalone)").matches) {
    const { projects } = state.board;
    const slug = e.key === "0" ? "all" : projects[e.key === "9" ? projects.length - 1 : e.key - 1]?.slug;
    e.preventDefault();
    if (slug) setProject(slug);
  }
  if (e.key === "Escape" && state.task && !typing) closeTask();
  if (e.key === "Escape" && !typing) clearSelection();
  if ((e.key === "/" && plain) || (e.key === "k" && (e.metaKey || e.ctrlKey))) {
    e.preventDefault();
    openSearch();
  }
  if (e.key === "n" && plain && !$("#new-task").hidden) { e.preventDefault(); newTask(); }
  if (e.key === "f" && plain && !$("#filter-box").hidden) { e.preventDefault(); $("#filter").focus(); }
  if (e.key === "h" && plain && !$("#human-only").hidden) { e.preventDefault(); setHumanOnly(!isHumanOnly()); }
});

window.addEventListener("hashchange", () => {
  const { slug, task, comment } = readHash();
  if ((slug || "all") !== state.project) setProject(slug || "all");
  focusComment = comment;
  if (task && task !== state.task) openTask(task);
  else if (task) showFocusedComment();
  else if (!task && state.task) closeTask();
});

async function poll() {
  if (document.hidden) return schedulePoll();
  try {
    const { build, changes, guests = null } = await api("GET", "/changes");
    // Not while something is being typed: the next check reloads instead.
    if (state.build && build !== state.build && !document.activeElement.closest('input, textarea, [contenteditable="true"]')) {
      location.reload();
      return;
    }
    if (!state.build) state.build = build;
    $("#offline").hidden = true;
    if (guests !== state.guests) {
      state.guests = guests;
      renderNetwork();
      if (state.menu === "network") loadNetwork();
    }
    if (changes !== state.changes) {
      state.changes = changes;
      // Joining or leaving a board, even from the terminal, shows another board.
      loadNetwork();
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

let syncing = false;

async function syncNow(keep) {
  if (syncing) return;
  syncing = true;
  toast(tr("Syncing…"));
  let diverged = null;
  try {
    toast((await api("POST", "/sync", { keep })).message);
    loadBoard();
  } catch (err) {
    if (err.data?.diverged) diverged = err.data;
    else toast(err.message);
  }
  syncing = false;
  if (diverged) chooseSide(diverged);
}

async function chooseSide({ first }) {
  const repo = repoName(syncRemote);
  if (first) {
    if (await askConfirm({ title: tr("Use the board in {0}?", repo), confirm: tr("Use theirs"), cancel: tr("Keep mine…"),
                           message: tr("{0} already has a board. Use it on this machine?\n\nThis machine's current board is kept in kanzo.db.bak.", repo) })) syncNow("remote");
    else if (await askConfirm({ title: tr("Replace the board in {0}?", repo), confirm: tr("Replace with mine"), danger: true,
                                message: tr("Replace the board in {0} with this machine's board instead?", repo) })) syncNow("local");
    return;
  }
  if (await askConfirm({ title: tr("Boards can't be merged"), confirm: tr("Keep mine"), cancel: tr("Use theirs…"), danger: true,
                         message: tr("This machine and another one both changed the board since the last sync, and boards can't be merged.\n\nKeep THIS machine's board, and overwrite the other one?") })) syncNow("local");
  else if (await askConfirm({ title: tr("Use the other board?"), confirm: tr("Use theirs"),
                              message: tr("Take the other machine's board instead? This machine's board is kept in kanzo.db.bak.") })) syncNow("remote");
}

let syncRemote = null;

function repoName(url) {
  return (url || "").replace(/\.git$/, "").split(/[/:]/).filter(Boolean).slice(-2).join("/");
}

async function loadSyncRemote() {
  try { syncRemote = (await api("GET", "/sync")).remote; } catch {}
  return syncRemote;
}

async function sync() {
  closeMenu();
  if (await loadSyncRemote()) syncNow();
  else openSettings("sync");
}

$("#sync-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const save = $("#sync-save");
  save.disabled = true;
  save.textContent = tr("Checking…");
  try {
    syncRemote = (await api("PUT", "/sync", { url: $("#sync-url").value })).remote;
    closeSettings();
    syncNow();
  } catch (err) {
    $("#sync-error").textContent = err.message;
    $("#sync-error").hidden = false;
  }
  save.disabled = false;
  save.textContent = tr("Save and sync");
});

async function loadNetwork() {
  let net = null;
  try { net = await api("GET", "/network"); } catch {}
  setNet(net);
}

function setNet(net) {
  state.net = net;
  // A joined board's count comes from its host, with each check of /api/changes.
  if (net && net.state !== "joined") state.guests = net.state === "sharing" ? net.guests.length : null;
  renderNetwork();
}

function renderNetwork() {
  const net = state.net;
  const button = $("#network");
  // Opened from another computer's browser, the board can't tell which computer it is on.
  const away = !net && !["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  const hosted = net?.state === "joined" || away;
  const host = net?.host?.replace(/:4242$/, "") || location.hostname;
  const n = state.guests;
  const shared = hosted || n != null;
  const computers = n == null ? "" : tr(": {0} connected", plural(n, "computer"));
  button.replaceChildren(icon("share"), ...(shared ? [tr("Shared")] : []), ...(shared && n != null ? [el("span", { class: "count" }, String(n))] : []));
  button.classList.toggle("on", shared);
  button.title = hosted ? tr("Shared by {0}{1}", host, computers)
    : shared ? tr("Shared on your network{0}", computers) : tr("Share this board, or join another");
  if (state.menu === "network") renderNetworkMenu();
}

// While a change is on its way, the popover keeps showing it.
let networkBusy = false;

function renderNetworkMenu() {
  if (networkBusy) return;
  const net = state.net;
  const now = net?.state;
  const share = $("#network-share");
  share.checked = now === "sharing";
  share.disabled = !net || now === "joined";
  $("#network-share-row").classList.toggle("disabled", share.disabled);
  $("#network-share-note").textContent =
    !net ? tr("Only the computer with the board can change this")
    : now === "sharing" ? tr("Other computers join it at {0}", net.addresses.length > 1 ? tr("the address for their network")
      : net.addresses.length ? tr("this address") : tr("this computer's IP address"))
    : now === "joined" ? tr("Leave the shared board to share this one")
    : tr("Other computers on your network can use it");
  const addresses = $("#network-addresses");
  addresses.hidden = now !== "sharing" || !net.addresses.length;
  if (now === "sharing") {
    addresses.replaceChildren(...net.addresses.map((a) => el("button", { class: "copy", type: "button", title: tr("Copy the address") },
      el("span", { class: "mono" }, a), icon("copy"))));
  }
  const guests = $("#network-guests");
  guests.hidden = now !== "sharing";
  if (now === "sharing") {
    guests.replaceChildren(
      el("span", { class: "label" }, net.guests.length ? tr("Connected now") : tr("No other computer is connected yet")),
      ...net.guests.map((g) => el("div", { class: "guest" },
        el("span", { class: "dot" }), g.name || tr("A browser"), el("span", { class: "address" }, g.address))));
  }
  const joined = now === "joined";
  $("#network-join").hidden = !net;
  $("#network-join").classList.toggle("disabled", now === "sharing");
  $("#network-join-text").replaceChildren(joined ? tr("Connected to {0}", net.host) : tr("Join a shared board"),
    el("span", {}, joined ? tr("This computer uses that board")
      : now === "sharing" ? tr("Stop sharing to join another board")
      : tr("The address the sharing computer shows")));
  $("#network-leave").hidden = !joined;
  $("#network-join-fields").hidden = joined;
  $("#join-host").disabled = $("#join-submit").disabled = now === "sharing";
}

function toggleNetworkMenu() {
  if (!toggleMenu("network")) return;
  const button = $("#network");
  button.classList.add("open");
  $("#network-error").hidden = true;
  renderNetworkMenu();
  const menu = $("#network-menu");
  menu.hidden = false;
  const r = button.getBoundingClientRect();
  menu.style.top = `${r.bottom + 6}px`;
  menu.style.left = `${Math.max(8, Math.min(r.right - menu.offsetWidth, innerWidth - menu.offsetWidth - 8))}px`;
  loadNetwork();
}

// Joining or leaving shows another board, so the page reloads.
async function changeNetwork(change) {
  $("#network-error").hidden = true;
  networkBusy = true;
  let net = state.net;
  try {
    net = await api("POST", "/network", change);
    if (change.action === "join" || change.action === "leave") { location.reload(); return; }
  } catch (err) {
    $("#network-error").textContent = err.message;
    $("#network-error").hidden = false;
  }
  networkBusy = false;
  setNet(net);
}

$("#network").addEventListener("click", toggleNetworkMenu);
$("#network-share").addEventListener("change", (e) => changeNetwork({ action: e.target.checked ? "on" : "off" }));
$("#network-addresses").addEventListener("click", (e) => {
  const button = e.target.closest(".copy");
  if (!button) return;
  copyKey(button.textContent);
  button.querySelector(".icon").replaceWith(icon("check"));
  setTimeout(() => button.querySelector(".icon").replaceWith(icon("copy")), 1500);
});
$("#network-leave").addEventListener("click", () => changeNetwork({ action: "leave" }));
$("#network-join").addEventListener("submit", async (e) => {
  e.preventDefault();
  const submit = $("#join-submit");
  submit.disabled = true;
  submit.textContent = tr("Joining…");
  await changeNetwork({ action: "join", host: $("#join-host").value });
  submit.disabled = false;
  submit.textContent = tr("Join");
});

function toggleSettingsMenu() {
  if (!state.board || !toggleMenu("settings")) return;
  const button = $("#settings");
  button.classList.add("open");
  const menu = el("div", { class: "menu", role: "menu", tabindex: "-1" },
    el("button", { onclick: () => openSettings() }, icon("settings"), tr("Settings")),
    el("button", { onclick: () => { closeMenu(); checkForUpdates(); } }, icon("update"), tr("Check for updates"),
      el("span", { class: "value" }, `v${state.board.version}`)),
    el("button", { onclick: () => openSettings("about") }, icon("news"), tr("What's new?")),
    el("button", { onclick: sync }, icon("sync"), tr("Sync")));
  document.body.append(menu);
  const r = button.getBoundingClientRect();
  menu.style.width = `${r.width}px`;
  menu.style.left = `${r.left}px`;
  menu.style.top = `${r.top - 6 - menu.offsetHeight}px`;
  menu.focus();
}

$("#settings").addEventListener("click", toggleSettingsMenu);

function openSettings(section = "board") {
  closeMenu();
  $("#settings-backlog").checked = state.board?.statuses.includes("backlog") ?? false;
  document.querySelectorAll("[data-property]").forEach((box) => { box.checked = !isPropertyHidden(box.dataset.property); });
  $("#sync-url").value = syncRemote || "";
  $("#sync-create").hidden = !!syncRemote;
  $("#sync-error").hidden = true;
  $("#settings-version").textContent = state.board ? `Kanzo ${state.board.version}` : "";
  showSection(section);
  $("#settings-dialog").hidden = false;
  if (section === "sync") $("#sync-url").focus();
  else $("#settings-close").focus();
}

function showSection(section) {
  document.querySelectorAll("#settings-nav button").forEach((b) => b.classList.toggle("active", b.dataset.section === section));
  document.querySelectorAll(".settings-pane").forEach((p) => { p.hidden = p.dataset.section !== section; });
  if (section === "about") loadWhatsNew();
  if (section === "accounts") showAccounts();
}

function showAccounts() {
  const pick = $("#account-project");
  const picked = pick.value;
  const projects = state.board ? state.board.projects.map((p) => p.slug) : [];
  pick.replaceChildren(el("option", { value: "" }, "No project"),
    ...projects.map((slug) => el("option", { value: slug }, slug)));
  pick.value = projects.includes(picked) ? picked : "";
  loadAccounts();
}

async function loadAccounts() {
  let accounts;
  try {
    accounts = await api("GET", "/accounts");
  } catch (err) { toast(err.message); return; }
  $("#accounts-list").replaceChildren(...(accounts.length ? accounts.map((a) =>
    el("div", { class: "account" },
      el("span", { class: "text" }, el("b", {}, a.name),
        el("span", {}, [KIND_LABELS[a.kind] || a.kind, a.project].filter(Boolean).join(" · "))),
      el("button", { class: "ghost", type: "button", title: "Copy token", "aria-label": `Copy ${a.name}'s token`,
                     onclick: () => copyKey(a.token, "Token copied") }, icon("copy")),
      el("button", { class: "ghost danger", type: "button", title: "Remove account", "aria-label": `Remove ${a.name}`,
                     onclick: () => removeAccount(a) }, icon("trash"))))
    : [el("div", { class: "note" }, "No accounts yet")]));
}

async function removeAccount(a) {
  if (!await askConfirm({ title: "Remove account?", confirm: "Remove", danger: true,
                          message: `Remove ${a.name}? Its token stops working and its tasks become unassigned. This cannot be undone.` })) return;
  try {
    await api("DELETE", `/accounts/${encodeURIComponent(a.name)}`);
    loadAccounts();
    loadBoard();
  } catch (err) { toast(err.message); }
}

// Fetched when first shown, so the page doesn't carry the notes on every load.
async function loadWhatsNew() {
  const notes = $("#whats-new");
  if (notes.firstChild) return;
  try {
    const res = await fetch("/changelog.md");
    if (res.ok) notes.innerHTML = markdown(await res.text());
  } catch {}
}

function closeSettings() {
  $("#settings-dialog").hidden = true;
}

$("#settings-nav").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (b) showSection(b.dataset.section);
});
$("#settings-close").addEventListener("click", closeSettings);
for (const [dialog, close] of [[$("#project-dialog"), hideNewProject], [$("#search-dialog"), closeSearch],
                               [$("#settings-dialog"), closeSettings], [$("#columns-dialog"), closeColumns],
                               [$("#confirm-dialog"), () => finishConfirm(false)], [$("#image-dialog"), closeImage]]) {
  dialog.addEventListener("mousedown", (e) => { if (e.target === dialog) close(); });
  dialog.addEventListener("keydown", (e) => { if (e.key === "Escape") { e.stopPropagation(); close(); } });
}
async function saveSetting(box, fields) {
  try {
    await api("PATCH", "/settings", fields);
    loadBoard();
  } catch (err) { box.checked = !box.checked; toast(err.message); }
}
$("#settings-backlog").addEventListener("change", (e) => saveSetting(e.target, { backlog: e.target.checked }));
$('.settings-pane[data-section="board"]').append(...["Description", "Tags", "URL", "Human", "Waiting", "Created", "Updated", "Comments", "Images"].map((name) => {
  const property = name.toLowerCase();
  return el("label", { class: "setting" }, el("span", { class: "text" }, tr(name)),
    el("span", { class: "switch" },
      el("input", { type: "checkbox", "data-property": property, "aria-label": tr(name),
                    onchange: (e) => saveSetting(e.target, { property, visible: e.target.checked }) }),
      el("span")));
}));
{
  // The language choice, held in localStorage like the filters. Changing it
  // reloads, so every string and <html lang> come back in one piece.
  const pane = $('.settings-pane[data-section="board"]');
  const choose = el("select", { class: "field", style: "width:auto", "aria-label": tr("Language"),
                                onchange: (e) => { localStorageSet("kanzo.lang", e.target.value); location.reload(); } },
    el("option", { value: "en" }, "English"), el("option", { value: "zh-cn" }, "中文"));
  choose.value = LANG;
  pane.insertBefore(el("label", { class: "setting" },
    el("span", { class: "text" }, tr("Language"), el("span", {}, tr("The language of the board's interface"))), choose),
    pane.querySelector(".setting"));
}
$("#settings-update").addEventListener("click", checkForUpdates);
$("#account-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const name = $("#account-name");
  try {
    const a = await api("POST", "/accounts", { name: name.value.trim(), kind: $("#account-kind").value,
                                               project: $("#account-project").value || null });
    name.value = "";
    loadAccounts();
    loadBoard();
    copyKey(a.token, `Added ${a.name}. Token copied`);
  } catch (err) { toast(err.message); }
});

async function checkUpdate() {
  try {
    showUpdate(await api("GET", "/update"));
  } catch {}
}

function showUpdate({ current, available }) {
  const button = $("#update");
  button.hidden = !available;
  if (!available) return;
  $("#update-label").textContent = tr("Update to {0}", available);
  button.title = tr("You have {0}", current);
  button.onclick = async () => {
    if (await askConfirm({ title: tr("Update Kanzo?"), confirm: tr("Update"),
                           message: tr("Update Kanzo from {0} to {1}? The board restarts, which takes a few seconds.", current, available) })) installUpdate(available);
  };
}

async function checkForUpdates() {
  toast(tr("Checking for updates…"));
  try {
    const status = await api("GET", "/update?now=1");
    showUpdate(status);
    if (status.available) {
      if (await askConfirm({ title: tr("Update Kanzo?"), confirm: tr("Install"),
                             message: tr("Kanzo {0} is out (you have {1}). Download and install it now? The board restarts.", status.available, status.current) })) {
        installUpdate(status.available);
      }
    } else if (status.latest) {
      toast(tr("Kanzo is up to date ({0})", status.current));
    } else {
      toast(tr("Can't reach GitHub to check for updates"));
    }
  } catch (err) { toast(err.message); }
}

async function installUpdate(available) {
  const button = $("#update");
  button.hidden = false;
  button.disabled = true;
  $("#update-label").textContent = tr("Updating…");
  let res;
  try {
    res = await api("POST", "/update");
  } catch (err) {
    toast(err.message);
    button.disabled = false;
    $("#update-label").textContent = tr("Update to {0}", available);
    return;
  }
  if (!res.restarting) {
    $("#update-label").textContent = tr("Restart Kanzo to finish");
    toast(tr("Kanzo {0} is installed. Restart Kanzo to use it.", available));
    return;
  }
  // The old program answered, then gives way to the new one: wait for it, then reload.
  await new Promise((r) => setTimeout(r, 1500));
  for (let i = 0; i < 60; i++) {
    try { await api("GET", "/update"); break; } catch { await new Promise((r) => setTimeout(r, 1000)); }
  }
  location.reload();
}

function setRail(on) {
  document.documentElement.classList.toggle("rail", on);
  $("#rail").setAttribute("aria-expanded", String(!on));
  localStorageSet("kanzo.rail", on ? "1" : "");
}
setRail(localStorageGet("kanzo.rail") === "1");
document.documentElement.classList.add("ready");
$("#rail").addEventListener("click", () => setRail($("#rail").getAttribute("aria-expanded") === "true"));

// Chromium's browsers ask this only while the board isn't installed; the others never do.
let installPrompt;
window.addEventListener("beforeinstallprompt", (e) => {
  e.preventDefault();
  installPrompt = e;
  $("#install").hidden = false;
});
window.addEventListener("appinstalled", () => { $("#install").hidden = true; });
$("#install").addEventListener("click", () => installPrompt.prompt());

setInterval(() => {
  document.querySelectorAll("time[datetime]").forEach((t) => { t.textContent = ago(t.getAttribute("datetime")); });
}, 60000);

// The dictionary is served separately and only fetched when the board isn't
// English. Everything below waits for it, so the first paint is never half
// translated, and a missing dictionary just stays English.
function loadDictionary() {
  if (LANG === "en") return Promise.resolve();
  return new Promise((resolve) => {
    const script = el("script", { src: `/i18n/${LANG}.js` });
    script.addEventListener("load", () => { try { DICT = ZH_CN; } catch { DICT = null; } resolve(); });
    script.addEventListener("error", () => resolve());
    document.head.append(script);
  });
}

// Text, title, aria-label and placeholder from index.html: an exact dictionary
// match swaps them, anything else keeps the English original. Runs once, while
// the board's dynamic parts are still empty, so no task content is touched.
function walkStatic() {
  if (!DICT) return;
  document.documentElement.lang = LANG === "zh-cn" ? "zh-CN" : "en";
  for (const node of document.body.querySelectorAll("*")) {
    for (const attr of ["title", "aria-label", "placeholder"]) {
      const value = node.getAttribute(attr);
      if (value && DICT[value]) node.setAttribute(attr, DICT[value]);
    }
  }
  const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    const text = node.textContent, hit = DICT[text.trim()];
    if (hit) node.textContent = text.replace(text.trim(), hit);
  }
}

loadDictionary().then(() => {
  walkStatic();
  // Every select, now and as later rendering adds them, gets the board's own dropdown;
  // after walkStatic, so its buttons show the translated labels.
  enhanceSelects(document.body);
  new MutationObserver((records) => {
    for (const r of records) for (const node of r.addedNodes) if (node.nodeType === 1) enhanceSelects(node);
  }).observe(document.body, { childList: true, subtree: true });
  checkUpdate();
  setInterval(checkUpdate, 24 * 60 * 60 * 1000);
  loadSyncRemote();
  loadNetwork();
  schedulePoll();
  {
    const { slug, task, comment } = readHash();
    state.project = slug || localStorageGet("kanzo.project") || "all";
    focusComment = comment;
    loadBoard().then(() => { if (task) openTask(task); });
  }
});
