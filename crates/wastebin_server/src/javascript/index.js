"use strict";

function $(id) {
  return document.getElementById(id);
}

const root = $("main-container");
const editorWrap = $("editor-wrap");
const codePane = $("code-pane");
const textarea = $("text");
const codeHighlight = $("code-highlight");
const lineNumbers = $("line-numbers");
const docEditor = $("doc-editor");
const mdPreview = $("md-preview");
const overlay = $("drop-overlay");
const syncStatus = $("sync-status");
const titleInput = $("title");
const langSelect = $("langs");
const langFilter = $("filter");
const padList = $("pad-list");
const btnPreview = $("btn-preview");
const btnDownload = $("btn-download");
const btnRaw = $("btn-raw");
const btnDelete = $("btn-delete");
const padActions = $("pad-actions");

// Localized strings, passed in as `data-i18n-*` attributes.
function t(key) {
  return root.dataset["i18n" + key] || "";
}

const clientId = Math.random().toString(36).slice(2) + Date.now().toString(36);
const SAVE_DELAY_MS = 250;
const PAD_LIST_DELAY_MS = 5000;

const state = {
  padId: null,
  // "code" edits plain text in the textarea, "doc" edits HTML in the document editor.
  mode: "code",
  previewing: false,
  // Selected language: "auto", "rich" (document) or a file extension.
  lang: "auto",
  // Extension the server detected for "auto".
  detected: "txt",
  // Content and extension the server last acknowledged for this pad.
  lastSaved: null,
  lastSavedExt: null,
  // Server revision `lastSaved` belongs to; writes based on an older one are refused.
  revision: null,
  titleDirty: false,
  expiryDirty: false,
  // Seconds until the current pad expires, `null` if it never does.
  expiresIn: null,
  saving: false,
  pending: false,
  saveTimer: null,
  // Bumped whenever another pad is shown, so that late responses for the old one are dropped.
  generation: 0,
  // Highlighted HTML of each line the server highlighted last, keyed by the line's text.
  rows: new Map(),
};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function escapeHtml(text) {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

let toastTimer = null;
function showToast(text) {
  const toast = $("toast");
  if (!toast) return;
  toast.textContent = text;
  toast.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.classList.add("hidden"), 1500);
}

function setStatus(text) {
  syncStatus.textContent = text;
}

function padUrl(id) {
  return "/?pad=" + encodeURIComponent(id);
}

function absoluteUrl(url) {
  try {
    return new URL(url, window.location.origin).href;
  } catch (_) {
    return url;
  }
}

function writeClipboardText(text) {
  if (navigator.clipboard && window.isSecureContext) {
    return navigator.clipboard.writeText(text);
  }
  // `navigator.clipboard` only exists in secure contexts, i.e. not on plain-HTTP LAN hosts.
  const helper = document.createElement("textarea");
  helper.value = text;
  helper.setAttribute("readonly", "");
  helper.className = "clipboard-helper";
  document.body.appendChild(helper);
  helper.select();
  const ok = document.execCommand("copy");
  helper.remove();
  return ok ? Promise.resolve() : Promise.reject(new Error("copy failed"));
}

// ---------------------------------------------------------------------------
// Sanitizer for document HTML: pasted, loaded from the server or received live.
// ---------------------------------------------------------------------------

const ALLOWED_TAGS = new Set([
  "H1", "H2", "H3", "H4", "H5", "H6", "P", "DIV", "BR", "HR",
  "UL", "OL", "LI", "TABLE", "THEAD", "TBODY", "TFOOT", "TR", "TH", "TD",
  "STRONG", "EM", "U", "S", "DEL", "SUB", "SUP", "CODE", "PRE", "BLOCKQUOTE",
  "A", "IMG", "FIGURE", "FIGCAPTION",
]);
const RENAMED_TAGS = { B: "strong", I: "em", STRIKE: "s" };
const DROPPED_TAGS = new Set([
  "SCRIPT", "STYLE", "IFRAME", "OBJECT", "EMBED", "META", "LINK", "HEAD", "TITLE",
  "TEMPLATE", "SVG", "MATH", "FORM", "INPUT", "BUTTON", "SELECT", "TEXTAREA", "NOSCRIPT",
]);

function isSafeHref(href) {
  return /^(?:https?:|mailto:|\/(?!\/)|#)/i.test(href.trim());
}

function isAssetUrl(url) {
  return /^\/raw\/[A-Za-z0-9_-]+(?:\.[A-Za-z0-9]+)?$/.test(url);
}

function sanitizeNode(node, out) {
  for (const child of Array.from(node.childNodes)) {
    if (child.nodeType === Node.TEXT_NODE) {
      out.appendChild(document.createTextNode(child.nodeValue));
      continue;
    }
    if (child.nodeType !== Node.ELEMENT_NODE) continue;

    const tag = child.tagName.toUpperCase();
    if (DROPPED_TAGS.has(tag) || child.classList.contains("editor-only")) continue;

    const name = RENAMED_TAGS[tag] || (ALLOWED_TAGS.has(tag) ? tag.toLowerCase() : null);
    if (!name) {
      // Unknown wrappers such as <span> or <font>: keep their content only.
      sanitizeNode(child, out);
      continue;
    }

    const el = document.createElement(name);

    if (name === "a") {
      const href = child.getAttribute("href") || "";
      if (isSafeHref(href)) el.setAttribute("href", href);
    } else if (name === "img") {
      const src = child.getAttribute("src") || "";
      if (!isAssetUrl(src) && !/^data:image\//i.test(src)) continue;
      el.setAttribute("src", src);
      el.setAttribute("alt", child.getAttribute("alt") || "");
    } else if (name === "figure") {
      if (child.classList.contains("inline-asset")) {
        el.className = "inline-asset";
        const kind = child.getAttribute("data-kind");
        if (kind === "image" || kind === "pdf") el.setAttribute("data-kind", kind);
        const src = child.getAttribute("data-src") || "";
        if (isAssetUrl(src)) el.setAttribute("data-src", src);
      }
    } else if (name === "td" || name === "th") {
      for (const attr of ["colspan", "rowspan"]) {
        const value = child.getAttribute(attr);
        if (value && /^\d{1,3}$/.test(value)) el.setAttribute(attr, value);
      }
    }

    sanitizeNode(child, el);
    out.appendChild(el);
  }
  return out;
}

function sanitizeHtml(html) {
  const doc = new DOMParser().parseFromString(html, "text/html");
  return sanitizeNode(doc.body, document.createElement("div"));
}

// ---------------------------------------------------------------------------
// Code editor: textarea over a highlighted copy, with a soft-wrap aware gutter.
// ---------------------------------------------------------------------------

const mirror = document.createElement("div");
mirror.id = "line-mirror";
mirror.setAttribute("aria-hidden", "true");
document.body.appendChild(mirror);

// Properties that influence where the text wraps, copied from the textarea.
const MIRROR_STYLE = [
  "font",
  "fontFeatureSettings",
  "fontKerning",
  "letterSpacing",
  "wordBreak",
  "wordSpacing",
  "tabSize",
  "whiteSpace",
  "overflowWrap",
];

const advanceContext = document.createElement("canvas").getContext("2d");

// Upper bound of the width a single code unit adds to a line.
function unitAdvance(font) {
  advanceContext.font = font;
  // Wide characters such as CJK, emoji and tab stops advance by at most twice
  // the step of the font, which covers them with a single bound per code unit.
  return 2 * advanceContext.measureText("M").width;
}

// Number of visual rows each of `lines` occupies when wrapped as in the textarea.
function measureRows(lines) {
  const style = getComputedStyle(textarea);
  const width = Math.max(0,
    textarea.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight));

  const rows = new Array(lines.length).fill(1);

  // A line that cannot be wider than the text area cannot wrap either. Only
  // lay out the remaining ones, so that typing in a long text stays cheap.
  const advance = unitAdvance(style.font);
  const candidateLines = [];
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].length * advance > width) {
      candidateLines.push(i);
    }
  }
  if (candidateLines.length === 0) {
    return rows;
  }

  for (const property of MIRROR_STYLE) {
    mirror.style[property] = style[property];
  }
  mirror.style.width = width + "px";

  // The lines are laid out together, so a single layout pass suffices.
  const spans = [];
  const fragment = document.createDocumentFragment();
  candidateLines.forEach(function(index, i) {
    if (i > 0) fragment.appendChild(document.createTextNode("\n"));
    const span = document.createElement("span");
    span.textContent = lines[index];
    fragment.appendChild(span);
    spans.push(span);
  });
  mirror.replaceChildren(fragment);

  spans.forEach(function(span, i) {
    // A space hanging at the end of a wrapped row forms a box of its own, so
    // count the distinct line boxes rather than the client rects.
    const boxes = new Set();
    for (const rect of span.getClientRects()) {
      boxes.add(Math.round(rect.top));
    }
    rows[candidateLines[i]] = Math.max(1, boxes.size);
  });

  return rows;
}

function updateLineNumbers() {
  if (state.mode !== "code" || state.previewing) return;

  const rows = measureRows(textarea.value.split("\n"));
  let html = "";
  for (let i = 0; i < rows.length; i++) {
    html += "<div>" + (i + 1) + "</div>";
  }
  lineNumbers.innerHTML = html;

  // Set through the CSSOM: the page's CSP (`style-src 'self' data:`) rejects
  // `style` attributes coming from markup, which would drop `--rows` again.
  const cells = lineNumbers.children;
  for (let i = 0; i < rows.length; i++) {
    if (rows[i] > 1) cells[i].style.setProperty("--rows", rows[i]);
  }
}

// Remember the server's highlighting of `content`, line by line.
function setHighlightRows(content, highlightedHtml) {
  state.rows = new Map();
  if (!highlightedHtml) return;

  const doc = new DOMParser().parseFromString(highlightedHtml, "text/html");
  const rows = doc.querySelectorAll('.src-code [id^="LC"]');
  const lines = content.split("\n");
  rows.forEach((row, i) => {
    if (i < lines.length) state.rows.set(lines[i], row.innerHTML);
  });
}

// Lines the server highlighted keep their colors while typing; edited lines show
// plain until the next save returns.
function renderHighlight() {
  const lines = textarea.value.split("\n");
  let html = lines.map((line) => state.rows.get(line) ?? escapeHtml(line)).join("\n");
  // A trailing newline only takes up a row in the textarea.
  if (textarea.value.endsWith("\n")) html += " ";
  codeHighlight.innerHTML = html;
}

function renderCode() {
  renderHighlight();
  updateLineNumbers();
  updateStats();
}

// ---------------------------------------------------------------------------
// Document editor
// ---------------------------------------------------------------------------

const BLOCK_TAGS = new Set([
  "DIV", "P", "H1", "H2", "H3", "H4", "H5", "H6", "LI", "TR", "PRE", "BLOCKQUOTE",
  "FIGURE", "TABLE", "UL", "OL", "HR",
]);

// Text of a document, one line per block, without inline asset captions.
function docText(node) {
  let out = "";
  const walk = (current) => {
    for (const child of current.childNodes) {
      if (child.nodeType === Node.TEXT_NODE) {
        out += child.nodeValue;
        continue;
      }
      if (child.nodeType !== Node.ELEMENT_NODE) continue;
      if (child.tagName === "FIGURE" || child.classList.contains("editor-only")) continue;
      if (child.tagName === "BR") {
        out += "\n";
        continue;
      }
      const block = BLOCK_TAGS.has(child.tagName);
      if (block && out && !out.endsWith("\n")) out += "\n";
      walk(child);
      if (block && out && !out.endsWith("\n")) out += "\n";
    }
  };
  walk(node);
  return out.replace(/\n$/, "");
}

function textToDocHtml(text) {
  if (!text) return "";
  return text
    .split("\n")
    .map((line) => "<div>" + (line ? escapeHtml(line) : "<br>") + "</div>")
    .join("");
}

function docHasContent(node) {
  return docText(node).trim() !== "" || node.querySelector("figure, img, table, hr") !== null;
}

// Editor-only controls for inline assets: the PDF frame and the remove button.
function hydrateAssets(node) {
  for (const figure of node.querySelectorAll("figure.inline-asset")) {
    figure.contentEditable = "false";
    const caption = figure.querySelector("figcaption");

    const src = figure.getAttribute("data-src");
    if (figure.getAttribute("data-kind") === "pdf" && src && !figure.querySelector("iframe")) {
      const frame = document.createElement("iframe");
      frame.className = "editor-only";
      frame.src = src;
      frame.title = caption ? caption.textContent : src;
      figure.insertBefore(frame, caption);
    }

    if (caption && !caption.querySelector(".asset-remove")) {
      const wrap = document.createElement("span");
      wrap.className = "editor-only";
      wrap.append(" · ");
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "asset-remove";
      remove.textContent = t("AssetRemove");
      wrap.appendChild(remove);
      caption.appendChild(wrap);
    }
  }
}

function setDocHtml(html) {
  const clean = sanitizeHtml(html);
  hydrateAssets(clean);
  docEditor.replaceChildren(...clean.childNodes);
}

function serializeDoc() {
  if (!docHasContent(docEditor)) return "";
  return sanitizeHtml(docEditor.innerHTML).innerHTML;
}

function figureHtml(asset) {
  const isPdf = asset.is_pdf;
  const name = escapeHtml(asset.filename || "");
  const url = escapeHtml(asset.url);
  const dl = escapeHtml(asset.dl_url);
  const links = isPdf
    ? '<a href="' + url + '">' + escapeHtml(t("AssetOpen")) + "</a> · " +
      '<a href="' + dl + '">' + escapeHtml(t("AssetDownload")) + "</a>"
    : '<a href="' + dl + '">' + escapeHtml(t("AssetDownload")) + "</a>";
  const media = isPdf ? "" : '<img src="' + url + '" alt="' + name + '">';
  return '<figure class="inline-asset" data-kind="' + (isPdf ? "pdf" : "image") + '"' +
    (isPdf ? ' data-src="' + url + '"' : "") + ">" + media +
    "<figcaption>" + name + " · " + escapeHtml(asset.size_formatted || "") + " · " + links +
    "</figcaption></figure>";
}

// Character offset of the caret in the document's text, to restore it after a remote update.
function docCaretOffset() {
  const selection = window.getSelection();
  if (!selection.rangeCount || !docEditor.contains(selection.focusNode)) return null;
  const range = document.createRange();
  range.setStart(docEditor, 0);
  range.setEnd(selection.focusNode, selection.focusOffset);
  return range.toString().length;
}

function restoreDocCaret(offset) {
  if (offset === null) return;
  const walker = document.createTreeWalker(docEditor, NodeFilter.SHOW_TEXT);
  let remaining = offset;
  let node = walker.nextNode();
  while (node) {
    if (remaining <= node.nodeValue.length) {
      const selection = window.getSelection();
      selection.collapse(node, remaining);
      return;
    }
    remaining -= node.nodeValue.length;
    node = walker.nextNode();
  }
}

// Where the next inline asset goes: the caret, or the end of the document.
function captureInsertRange() {
  const selection = window.getSelection();
  if (selection.rangeCount && docEditor.contains(selection.getRangeAt(0).commonAncestorContainer)) {
    return selection.getRangeAt(0).cloneRange();
  }
  const range = document.createRange();
  range.selectNodeContents(docEditor);
  range.collapse(false);
  return range;
}

function insertNodesAt(range, html) {
  const holder = document.createElement("div");
  holder.innerHTML = html;
  hydrateAssets(holder);
  const nodes = Array.from(holder.childNodes);
  if (nodes.length === 0) return;

  // Inline assets are blocks: split them out of the current line.
  let container = range.startContainer;
  while (container && container !== docEditor && container.parentNode !== docEditor) {
    container = container.parentNode;
  }

  range.deleteContents();
  const fragment = document.createDocumentFragment();
  nodes.forEach((node) => fragment.appendChild(node));
  const inLine = container && container !== docEditor && container.tagName !== "FIGURE";
  if (inLine && container.nodeType === Node.ELEMENT_NODE && !docHasContent(container)) {
    // An empty line becomes the asset.
    container.replaceWith(fragment);
  } else if (inLine) {
    container.after(fragment);
  } else {
    range.insertNode(fragment);
  }

  // Leave an empty line after the asset to keep typing in.
  const last = nodes[nodes.length - 1];
  let next = last.nextSibling;
  if (!next || next.nodeName === "FIGURE") {
    next = document.createElement("div");
    next.appendChild(document.createElement("br"));
    last.after(next);
  }
  const selection = window.getSelection();
  selection.collapse(next, 0);
}

// ---------------------------------------------------------------------------
// Modes and views
// ---------------------------------------------------------------------------

function effectiveLang() {
  if (state.mode === "doc") return "rich";
  return state.lang === "auto" ? state.detected : state.lang;
}

function canPreview() {
  return state.mode === "code" && effectiveLang() === "md" && state.padId !== null;
}

function updateView() {
  const previewing = state.previewing && canPreview();
  codePane.classList.toggle("hidden", state.mode !== "code" || previewing);
  docEditor.classList.toggle("hidden", state.mode !== "doc");
  mdPreview.classList.toggle("hidden", !previewing);

  btnPreview.classList.toggle("hidden", !canPreview());
  btnPreview.setAttribute("aria-pressed", String(previewing));
  const title = previewing ? btnPreview.dataset.titleSource : btnPreview.dataset.titleRendered;
  btnPreview.title = title;
  btnPreview.setAttribute("aria-label", title);
  btnPreview.querySelector(".icon-rendered").classList.toggle("hidden", previewing);
  btnPreview.querySelector(".icon-source").classList.toggle("hidden", !previewing);
}

function setMode(mode) {
  state.mode = mode;
  if (mode !== "code") state.previewing = false;
  updateView();
  if (mode === "code") renderCode();
  updateStats();
}

function switchToDoc() {
  if (state.mode === "doc") return;
  setDocHtml(textToDocHtml(textarea.value));
  textarea.value = "";
  state.lang = "rich";
  selectLang("rich");
  setMode("doc");
}

function switchToCode() {
  if (state.mode === "code") return true;
  if (docEditor.querySelector("figure.inline-asset") && !window.confirm(t("ConfirmDropAssets"))) {
    return false;
  }
  textarea.value = docText(docEditor);
  docEditor.replaceChildren();
  state.rows = new Map();
  setMode("code");
  return true;
}

function selectLang(value) {
  if (!Array.from(langSelect.options).some((option) => option.value === value)) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = value;
    langSelect.options.add(option, 2);
  }
  langSelect.value = value;
}

function updateAutoLabel() {
  const auto = langSelect.options[0];
  const detected = state.mode === "code" && state.lang === "auto" && state.detected !== "txt";
  auto.textContent = detected ? t("Auto") + " · " + state.detected : t("Auto");
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

const stats = $("stats");
const MAX_BYTES = parseInt(stats.dataset.maxBytes, 10) || 1024 * 1024;
const UNIT_KB = stats.dataset.unitKb;
const UNIT_MB = stats.dataset.unitMb;
const LABEL_LIMIT = stats.dataset.labelLimit;

function formatSize(bytes) {
  if (bytes >= 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + " " + UNIT_MB;
  return (bytes / 1024).toFixed(0) + " " + UNIT_KB;
}

$("progress-limit").textContent = LABEL_LIMIT + " " + formatSize(MAX_BYTES);

function updateStats() {
  const text = state.mode === "doc" ? docText(docEditor) : textarea.value;
  const content = state.mode === "doc" ? serializeDoc() : text;
  const lines = Math.max(1, text.split("\n").length);
  let bytes;
  try { bytes = new Blob([content]).size; } catch (_) { bytes = content.length; }

  $("stat-lines").textContent = lines;
  $("stat-chars").textContent = text.length;
  $("stat-bytes").textContent = bytes.toLocaleString();

  const pct = Math.min(100, (bytes / MAX_BYTES) * 100);
  const fill = $("progress-fill");
  fill.style.width = pct + "%";
  fill.classList.toggle("warn", pct > 85);
  $("progress-kb").textContent = (bytes / 1024).toFixed(1) + " " + UNIT_KB;
}

// ---------------------------------------------------------------------------
// Pad metadata in the panel
// ---------------------------------------------------------------------------

const defaultExpiry = document.querySelector('#expiry-list input[name="expires"]:checked');

function selectedExpiry() {
  const checked = document.querySelector('#expiry-list input[name="expires"]:checked');
  return checked ? parseInt(checked.value, 10) : 0;
}

// Show the current pad's expiration as the shortest option that still covers it.
function syncExpiryRadios() {
  if (!state.padId) return;
  const radios = Array.from(document.querySelectorAll('#expiry-list input[name="expires"]'));
  const remaining = state.expiresIn;
  const match = remaining === null
    ? radios.find((radio) => radio.value === "0")
    : radios
      .filter((radio) => radio.value !== "0" && parseInt(radio.value, 10) >= remaining)
      .sort((a, b) => parseInt(a.value, 10) - parseInt(b.value, 10))[0];
  for (const radio of radios) radio.checked = radio === match;
}

function updatePadActions() {
  padActions.classList.toggle("hidden", !state.padId);
  btnDownload.href = state.padId ? "/dl/" + state.padId : "#";
  btnRaw.href = state.padId ? "/raw/" + state.padId : "#";
  for (const item of padList.querySelectorAll(".pad-item")) {
    item.classList.toggle("active", item.dataset.id === state.padId);
  }
}

function relativeTime(iso) {
  const then = Date.parse(iso || "");
  if (Number.isNaN(then)) return "";
  const minutes = Math.floor((Date.now() - then) / 60000);
  if (minutes < 1) return "<1m";
  if (minutes < 60) return minutes + "m";
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return hours + "h";
  const days = Math.floor(hours / 24);
  if (days < 30) return days + "d";
  return new Date(then).toLocaleDateString(document.documentElement.lang);
}

function renderPads(pads) {
  if (!Array.isArray(pads)) return;
  if (pads.length === 0) {
    const empty = document.createElement("div");
    empty.className = "pad-empty";
    empty.textContent = t("Empty");
    padList.replaceChildren(empty);
    return;
  }

  padList.replaceChildren(...pads.map((pad) => {
    const item = document.createElement("button");
    item.type = "button";
    item.className = "pad-item";
    item.dataset.id = pad.id;
    item.dataset.updated = pad.updated_at || "";
    const label = document.createElement("span");
    label.className = "pad-label";
    label.textContent = pad.title || pad.preview || t("Untitled");
    const time = document.createElement("span");
    time.className = "pad-time";
    time.textContent = relativeTime(pad.updated_at);
    item.append(label, time);
    item.title = label.textContent;
    return item;
  }));
  updatePadActions();
}

let padListTimer = null;
async function refreshPads() {
  clearTimeout(padListTimer);
  padListTimer = null;
  try {
    const resp = await fetch("/api/pads");
    if (resp.ok) renderPads(await resp.json());
  } catch (err) {
    console.error(err);
  }
}

// The list changes with every save somewhere; refresh it at a slow pace.
function refreshPadsSoon() {
  if (padListTimer === null) {
    padListTimer = setTimeout(refreshPads, PAD_LIST_DELAY_MS);
  }
}

setInterval(() => {
  for (const time of padList.querySelectorAll(".pad-item")) {
    time.querySelector(".pad-time").textContent = relativeTime(time.dataset.updated);
  }
}, 60000);

// ---------------------------------------------------------------------------
// Loading and saving
// ---------------------------------------------------------------------------

function currentContent() {
  return state.mode === "doc" ? serializeDoc() : textarea.value;
}

function currentExtension() {
  return state.mode === "doc" ? "rich" : state.lang;
}

function hasUnsavedChanges() {
  return state.saveTimer !== null || state.saving || state.pending ||
    currentContent() !== (state.lastSaved ?? "");
}

function scheduleSave(immediate) {
  clearTimeout(state.saveTimer);
  state.saveTimer = null;
  if (immediate) {
    saveNow();
    return;
  }
  state.saveTimer = setTimeout(saveNow, SAVE_DELAY_MS);
}

function onLocalEdit(immediate) {
  if (state.mode === "doc" && !docHasContent(docEditor) && docEditor.childNodes.length) {
    // Drop the stray <br> left behind, so that the placeholder shows again.
    docEditor.replaceChildren();
  }
  updateStats();
  scheduleSave(immediate);
}

async function saveNow() {
  clearTimeout(state.saveTimer);
  state.saveTimer = null;

  if (state.saving) {
    state.pending = true;
    return;
  }

  const content = currentContent();
  const extension = currentExtension();
  const isCreate = state.padId === null;

  if (isCreate && content.trim() === "") return;
  if (!isCreate && content === state.lastSaved && extension === state.lastSavedExt &&
      !state.titleDirty && !state.expiryDirty) {
    return;
  }

  const body = { content, extension, client_id: clientId };
  if (!isCreate) body.base_revision = state.revision;
  const sentTitle = isCreate ? titleInput.value.trim() !== "" : state.titleDirty;
  const sentExpiry = isCreate || state.expiryDirty;
  if (sentTitle) body.title = titleInput.value;
  if (sentExpiry) body.expires = selectedExpiry();
  state.titleDirty = false;
  state.expiryDirty = false;

  const generation = state.generation;
  state.saving = true;
  setStatus(t("Saving"));

  try {
    const resp = await fetch(isCreate ? "/api/pad" : "/api/pad/" + encodeURIComponent(state.padId), {
      method: isCreate ? "POST" : "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body),
    });

    if (generation !== state.generation) return;

    if (resp.status === 409) {
      // Someone else saved first: replay this edit on top of their version and save again.
      rebase(content, await resp.json());
      state.titleDirty = state.titleDirty || sentTitle;
      state.expiryDirty = state.expiryDirty || sentExpiry;
      state.pending = true;
      return;
    }
    if (resp.status === 404 && !isCreate) {
      // The pad was deleted or expired elsewhere: keep the text as a new pad.
      state.padId = null;
      state.lastSaved = null;
      state.titleDirty = titleInput.value.trim() !== "";
      state.pending = true;
      return;
    }
    if (!resp.ok) throw new Error("save failed: " + resp.status);

    const pad = await resp.json();
    if (generation !== state.generation) return;

    if (isCreate) {
      state.padId = pad.id;
      history.replaceState(null, "", padUrl(pad.id));
    }
    state.lastSaved = content;
    state.lastSavedExt = extension;
    state.revision = pad.revision;
    state.expiresIn = pad.expires_in ?? null;

    if (state.mode === "code") {
      state.detected = pad.extension;
      if (pad.content === textarea.value || !hasUnsavedChanges()) {
        setHighlightRows(pad.content, pad.highlighted_html);
        renderHighlight();
      }
      updateAutoLabel();
    }

    syncExpiryRadios();
    updatePadActions();
    updateView();
    setStatus(t("Saved"));

    if (isCreate || sentTitle) {
      refreshPads();
    } else {
      refreshPadsSoon();
    }
  } catch (err) {
    console.error(err);
    if (generation === state.generation) {
      state.titleDirty = state.titleDirty || sentTitle;
      state.expiryDirty = state.expiryDirty || (sentExpiry && !isCreate);
      setStatus(t("Offline"));
    }
  } finally {
    state.saving = false;
    if (state.pending) {
      state.pending = false;
      saveNow();
    }
  }
}

// Wait until everything typed so far reached the server.
async function flushSave() {
  for (let i = 0; i < 100 && (state.saveTimer !== null || state.saving || state.pending); i++) {
    if (state.saving) {
      await new Promise((resolve) => setTimeout(resolve, 50));
    } else {
      await saveNow();
    }
  }
}

function resetEditor() {
  clearTimeout(state.saveTimer);
  state.saveTimer = null;
  state.pending = false;
  state.generation++;
  state.padId = null;
  state.previewing = false;
  state.lang = "auto";
  state.detected = "txt";
  state.lastSaved = null;
  state.lastSavedExt = null;
  state.revision = null;
  state.titleDirty = false;
  state.expiryDirty = false;
  state.expiresIn = null;
  state.rows = new Map();

  textarea.value = "";
  codeHighlight.replaceChildren();
  docEditor.replaceChildren();
  mdPreview.replaceChildren();
  titleInput.value = "";
  if (defaultExpiry) defaultExpiry.checked = true;
  langFilter.value = "";
  filterLangs();
  selectLang("auto");
  updateAutoLabel();
  setMode("code");
  syncExpiryRadios();
  updatePadActions();
  setStatus("");
}

function newPad() {
  resetEditor();
  history.pushState(null, "", "/new");
  textarea.focus();
}

function isDocument(pad) {
  return pad.extension === "rich" ||
    (pad.extension === "html" && /<(?:figure|img|table|p)[\s>]/i.test(pad.content));
}

// Show `pad` in the editor. For live updates the caret and scroll position are kept.
function applyPad(pad, live) {
  const scrollTop = editorWrap.scrollTop;

  state.padId = pad.id;
  state.lastSaved = pad.content;
  state.revision = pad.revision;
  state.expiresIn = pad.expires_in ?? null;
  if (!live || document.activeElement !== titleInput) {
    titleInput.value = pad.title || "";
    state.titleDirty = false;
  }

  if (isDocument(pad)) {
    state.lang = "rich";
    state.lastSavedExt = "rich";
    selectLang("rich");
    const caret = live ? docCaretOffset() : null;
    if (state.mode !== "doc" || serializeDoc() !== sanitizeHtml(pad.content).innerHTML) {
      setDocHtml(pad.content);
      // The sanitized form is what this editor sends back.
      state.lastSaved = serializeDoc();
    }
    setMode("doc");
    restoreDocCaret(caret);
  } else {
    if (!live) {
      state.lang = pad.auto_detected ? "auto" : pad.extension;
    } else if (state.lang !== "auto") {
      state.lang = pad.extension;
    }
    state.lastSavedExt = state.lang;
    state.detected = pad.extension;
    selectLang(state.lang);

    const before = textarea.value;
    const focused = document.activeElement === textarea;
    const [start, end] = [textarea.selectionStart, textarea.selectionEnd];
    textarea.value = pad.content;
    setHighlightRows(pad.content, pad.highlighted_html);
    setMode("code");

    if (live && focused) {
      textarea.setSelectionRange(shiftOffset(before, pad.content, start),
        shiftOffset(before, pad.content, end));
    }
  }

  updateAutoLabel();
  syncExpiryRadios();
  updatePadActions();
  updateView();
  if (live) {
    editorWrap.scrollTop = scrollTop;
    if (state.previewing) renderPreview();
  }
}

// The single region in which `before` and `after` differ: [start, end in before, end in after).
function changedRegion(before, after) {
  let start = 0;
  const max = Math.min(before.length, after.length);
  while (start < max && before[start] === after[start]) start++;
  let suffix = 0;
  while (suffix < max - start &&
    before[before.length - 1 - suffix] === after[after.length - 1 - suffix]) {
    suffix++;
  }
  return { start, endBefore: before.length - suffix, endAfter: after.length - suffix };
}

// Map an offset in `before` to `after`, assuming a single edited region.
function shiftOffset(before, after, offset) {
  const region = changedRegion(before, after);
  if (offset <= region.start) return offset;
  if (offset >= region.endBefore) return offset + after.length - before.length;
  return region.endAfter;
}

// Three-way merge of two edits of `base` that touch separate regions, or `null`.
function merge(base, mine, theirs) {
  const m = changedRegion(base, mine);
  const t = changedRegion(base, theirs);
  const inserted = mine.slice(m.start, m.endAfter);
  if (m.endBefore <= t.start) {
    return theirs.slice(0, m.start) + inserted + theirs.slice(m.endBefore);
  }
  if (t.endBefore <= m.start) {
    const delta = theirs.length - base.length;
    return theirs.slice(0, m.start + delta) + inserted + theirs.slice(m.endBefore + delta);
  }
  return null;
}

// Replay the local edit `mine` (made on top of `state.lastSaved`) onto the newer `pad`. When
// both touched the same region the local version wins, as it is the one being typed.
function rebase(mine, pad) {
  const merged = merge(state.lastSaved ?? "", mine, pad.content) ?? mine;
  state.lastSaved = pad.content;
  state.revision = pad.revision;
  const current = currentContent();
  // Keep anything typed while the request was in flight.
  const result = current === mine ? merged : merge(mine, current, merged) ?? current;
  if (result === current) return;

  if (state.mode === "doc") {
    const caret = docCaretOffset();
    setDocHtml(result);
    restoreDocCaret(caret);
  } else {
    const [start, end] = [textarea.selectionStart, textarea.selectionEnd];
    textarea.value = result;
    textarea.setSelectionRange(shiftOffset(current, result, start), shiftOffset(current, result, end));
    renderCode();
  }
}

async function openPad(id, push) {
  await flushSave();
  resetEditor();
  const generation = state.generation;
  try {
    const resp = await fetch("/api/pad/" + encodeURIComponent(id));
    if (generation !== state.generation) return;
    if (!resp.ok) {
      history.replaceState(null, "", "/new");
      return;
    }
    const pad = await resp.json();
    if (generation !== state.generation) return;
    applyPad(pad, false);
    if (push) {
      history.pushState(null, "", padUrl(pad.id));
    } else {
      history.replaceState(null, "", padUrl(pad.id));
    }
  } catch (err) {
    console.error(err);
    setStatus(t("Offline"));
  }
}

async function openLatest() {
  await flushSave();
  resetEditor();
  const generation = state.generation;
  try {
    const resp = await fetch("/api/pad/latest");
    if (!resp.ok || generation !== state.generation) return;
    const pad = await resp.json();
    if (generation !== state.generation || !pad.id) return;
    applyPad(pad, false);
    history.replaceState(null, "", padUrl(pad.id));
  } catch (err) {
    console.error(err);
    setStatus(t("Offline"));
  }
}

function route() {
  const id = new URLSearchParams(window.location.search).get("pad");
  if (id) return openPad(id, false);
  if (window.location.pathname === "/new") {
    resetEditor();
    return Promise.resolve();
  }
  return openLatest();
}

async function renderPreview() {
  await flushSave();
  if (!state.padId) return;
  try {
    const resp = await fetch("/api/pad/" + encodeURIComponent(state.padId) + "/md");
    if (!resp.ok) throw new Error("render failed: " + resp.status);
    const rendered = await resp.json();
    // Rendered and sanitized by the server.
    mdPreview.innerHTML = rendered.html;
  } catch (err) {
    console.error(err);
    setStatus(t("Offline"));
  }
}

async function togglePreview() {
  if (state.previewing) {
    state.previewing = false;
    updateView();
    renderCode();
    textarea.focus();
    return;
  }
  await renderPreview();
  state.previewing = true;
  updateView();
}

// ---------------------------------------------------------------------------
// Live updates
// ---------------------------------------------------------------------------

function onPadEvent(event) {
  let msg;
  try {
    msg = JSON.parse(event.data);
  } catch (_) {
    return;
  }

  if (msg.type === "created" || msg.type === "deleted") {
    refreshPads();
  } else {
    refreshPadsSoon();
  }

  if (msg.client_id === clientId || !msg.id || msg.id !== state.padId) return;

  if (msg.type === "deleted") {
    // Keep the text: the next edit saves it as a new pad.
    state.padId = null;
    state.lastSaved = null;
    state.expiresIn = null;
    syncExpiryRadios();
    updatePadActions();
    updateView();
    setStatus(t("Deleted"));
    return;
  }

  // Never overwrite local edits that have not reached the server yet; the next save wins.
  if (hasUnsavedChanges()) return;

  applyPad(msg, true);
  setStatus(t("Live"));
}

function connectEvents() {
  if (!window.EventSource) return;
  const events = new EventSource("/api/pad/events");
  let disconnected = false;
  events.addEventListener("pad_update", onPadEvent);
  events.addEventListener("error", () => {
    disconnected = true;
  });
  events.addEventListener("open", async () => {
    if (!disconnected) return;
    disconnected = false;
    // Catch up with what happened while the connection was down.
    refreshPads();
    if (state.padId && !hasUnsavedChanges()) {
      const generation = state.generation;
      const resp = await fetch("/api/pad/" + encodeURIComponent(state.padId));
      if (resp.ok && generation === state.generation && !hasUnsavedChanges()) {
        applyPad(await resp.json(), true);
      }
    }
  });
}

// ---------------------------------------------------------------------------
// Files, clipboard and drag & drop
// ---------------------------------------------------------------------------

function isMediaFile(file) {
  return /^image\//.test(file.type) || file.type === "application/pdf" ||
    /\.(?:png|jpe?g|gif|webp|svg|avif|bmp|ico|tiff?|pdf)$/i.test(file.name || "");
}

function readAsDataUrl(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
}

function screenshotName(file) {
  const ext = (file.type.split("/")[1] || "png").replace("jpeg", "jpg");
  const d = new Date();
  const pad = (n) => String(n).padStart(2, "0");
  return "screenshot " + d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()) +
    " " + pad(d.getHours()) + "." + pad(d.getMinutes()) + "." + pad(d.getSeconds()) + "." + ext;
}

async function uploadDataUrl(dataUrl, filename) {
  const resp = await fetch("/api/asset", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      data_url: dataUrl,
      filename,
      // Assets live as long as the pad that shows them.
      expires: state.padId ? Math.ceil(state.expiresIn ?? 0) : selectedExpiry(),
    }),
  });
  if (!resp.ok) throw new Error("upload failed: " + resp.status);
  return resp.json();
}

async function insertAssets(files) {
  switchToDoc();
  docEditor.focus();
  let range = captureInsertRange();

  for (const file of files) {
    setStatus(t("Uploading"));
    try {
      const asset = await uploadDataUrl(await readAsDataUrl(file), file.name || screenshotName(file));
      if (!docEditor.contains(range.startContainer)) range = captureInsertRange();
      insertNodesAt(range, figureHtml(asset));
      range = captureInsertRange();
    } catch (err) {
      console.error(err);
      showToast(t("UploadFailed"));
    }
  }

  onLocalEdit(true);
}

// Pasted HTML can carry images as `data:` URLs: store them as assets first.
async function cleanPastedHtml(html) {
  const clean = sanitizeHtml(html);
  for (const img of clean.querySelectorAll('img[src^="data:"]')) {
    try {
      const asset = await uploadDataUrl(img.getAttribute("src"), img.getAttribute("alt") || "image");
      const holder = document.createElement("div");
      holder.innerHTML = figureHtml(asset);
      img.replaceWith(holder.firstChild);
    } catch (err) {
      console.error(err);
      img.remove();
    }
  }
  return clean.innerHTML;
}

async function loadTextFile(file) {
  const text = await file.text();
  if (textarea.value || docHasContent(docEditor)) {
    await flushSave();
    newPad();
  }
  switchToCode();
  textarea.value = text.replace(/\n$/, "");

  const dot = (file.name || "").lastIndexOf(".");
  const ext = dot > 0 ? file.name.slice(dot + 1).toLowerCase() : "";
  const known = Array.from(langSelect.options).some((option) => option.value === ext);
  state.lang = known && ext !== "rich" ? ext : "auto";
  selectLang(state.lang);
  langFilter.value = "";
  filterLangs();
  if (!titleInput.value) titleInput.value = file.name || "";

  renderCode();
  updateAutoLabel();
  onLocalEdit(true);
}

async function handleFiles(fileList) {
  const files = Array.from(fileList || []);
  const media = files.filter(isMediaFile);
  if (media.length) await insertAssets(media);
  const text = files.find((file) => !isMediaFile(file));
  if (text) await loadTextFile(text);
}

function clipboardFiles(clipboard) {
  return Array.from(clipboard.files || []).filter(isMediaFile);
}

textarea.addEventListener("paste", async (e) => {
  const clipboard = e.clipboardData;
  if (!clipboard) return;

  const files = clipboardFiles(clipboard);
  if (files.length && !clipboard.getData("text/plain")) {
    e.preventDefault();
    await insertAssets(files);
    return;
  }

  // A document with images, tables or headings pasted into an empty pad starts a document.
  const html = clipboard.getData("text/html");
  if (html && state.lang === "auto" && textarea.value === "" &&
      /<(?:img|table|h[1-6])[\s>]/i.test(html)) {
    e.preventDefault();
    switchToDoc();
    docEditor.focus();
    const clean = await cleanPastedHtml(html);
    insertNodesAt(captureInsertRange(), clean);
    onLocalEdit(true);
  }
  // Anything else is pasted as plain text by the browser.
});

docEditor.addEventListener("paste", async (e) => {
  const clipboard = e.clipboardData;
  if (!clipboard) return;
  e.preventDefault();

  const files = clipboardFiles(clipboard);
  const html = clipboard.getData("text/html");
  const text = clipboard.getData("text/plain");

  if (files.length && !text) {
    await insertAssets(files);
  } else if (html) {
    const range = captureInsertRange();
    const clean = await cleanPastedHtml(html);
    if (/<figure/i.test(clean)) {
      insertNodesAt(range, clean);
    } else {
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      document.execCommand("insertHTML", false, clean);
    }
    onLocalEdit();
  } else if (text) {
    // Inserted at the caret, replacing the selection, as part of the undo history.
    document.execCommand("insertText", false, text.replace(/\r\n?/g, "\n"));
  }
});

docEditor.addEventListener("click", (e) => {
  const remove = e.target.closest(".asset-remove");
  if (!remove) return;
  e.preventDefault();
  const figure = remove.closest("figure");
  if (figure) figure.remove();
  onLocalEdit(true);
});

let dragCounter = 0;
const editorArea = editorWrap.parentElement;

function isFileDrag(e) {
  return e.dataTransfer && Array.from(e.dataTransfer.types || []).includes("Files");
}

editorArea.addEventListener("dragenter", (e) => {
  if (!isFileDrag(e)) return;
  e.preventDefault();
  dragCounter++;
  overlay.classList.add("active");
});

editorArea.addEventListener("dragleave", (e) => {
  if (!isFileDrag(e)) return;
  e.preventDefault();
  dragCounter = Math.max(0, dragCounter - 1);
  if (dragCounter === 0) overlay.classList.remove("active");
});

editorArea.addEventListener("dragover", (e) => {
  if (isFileDrag(e)) e.preventDefault();
});

editorArea.addEventListener("drop", (e) => {
  if (!isFileDrag(e)) return;
  e.preventDefault();
  dragCounter = 0;
  overlay.classList.remove("active");
  handleFiles(e.dataTransfer.files);
});

$("open").addEventListener("click", () => {
  const input = document.createElement("input");
  input.type = "file";
  input.multiple = true;
  input.addEventListener("change", () => handleFiles(input.files));
  input.click();
});

// ---------------------------------------------------------------------------
// Copying
// ---------------------------------------------------------------------------

function docForExport() {
  const clone = sanitizeHtml(docEditor.innerHTML);
  for (const img of clone.querySelectorAll("img[src]")) img.src = absoluteUrl(img.getAttribute("src"));
  for (const a of clone.querySelectorAll("a[href]")) a.href = absoluteUrl(a.getAttribute("href"));
  return clone;
}

async function copyContent() {
  try {
    if (state.mode === "code") {
      await writeClipboardText(textarea.value);
    } else if (navigator.clipboard && window.ClipboardItem && window.isSecureContext) {
      await navigator.clipboard.write([new ClipboardItem({
        "text/html": new Blob([docForExport().innerHTML], { type: "text/html" }),
        "text/plain": new Blob([docText(docEditor)], { type: "text/plain" }),
      })]);
    } else {
      await writeClipboardText(docText(docEditor));
    }
    showToast(t("Copied"));
  } catch (err) {
    console.error(err);
    showToast(t("CopyFailed"));
  }
}

function toMarkdown(node) {
  if (node.nodeType === Node.TEXT_NODE) return node.nodeValue;
  if (node.nodeType !== Node.ELEMENT_NODE) return "";

  const children = () => Array.from(node.childNodes).map(toMarkdown).join("");
  const inline = (marker) => {
    const inner = children().trim();
    return inner ? marker + inner + marker : "";
  };
  const tag = node.tagName;

  if (tag === "FIGURE") {
    const img = node.querySelector("img");
    const link = node.querySelector("figcaption a");
    const caption = node.querySelector("figcaption");
    const name = caption ? caption.textContent.split(" · ")[0] : "";
    if (img) return "\n\n![" + (img.getAttribute("alt") || name) + "](" + img.getAttribute("src") + ")\n\n";
    if (link) return "\n\n[" + name + "](" + link.getAttribute("href") + ")\n\n";
    return "";
  }

  switch (tag) {
    case "H1": case "H2": case "H3": case "H4": case "H5": case "H6":
      return "\n\n" + "#".repeat(Number(tag[1])) + " " + children().trim() + "\n\n";
    case "P":
      return "\n\n" + children().trim() + "\n\n";
    case "DIV":
      return "\n" + children() + "\n";
    case "BR":
      return "\n";
    case "HR":
      return "\n\n---\n\n";
    case "STRONG":
      return inline("**");
    case "EM":
      return inline("*");
    case "S":
    case "DEL":
      return inline("~~");
    case "CODE":
      return node.parentElement && node.parentElement.tagName === "PRE"
        ? node.textContent
        : "`" + node.textContent + "`";
    case "PRE":
      return "\n\n```\n" + node.textContent.replace(/\n$/, "") + "\n```\n\n";
    case "BLOCKQUOTE":
      return "\n\n" + children().trim().split("\n").map((line) => "> " + line).join("\n") + "\n\n";
    case "UL":
    case "OL": {
      const items = Array.from(node.children)
        .filter((child) => child.tagName === "LI")
        .map((li, i) => (tag === "OL" ? i + 1 + ". " : "- ") + toMarkdown(li).trim());
      return "\n\n" + items.join("\n") + "\n\n";
    }
    case "A":
      return "[" + (children().trim() || node.getAttribute("href")) + "](" + node.getAttribute("href") + ")";
    case "IMG":
      return "![" + (node.getAttribute("alt") || "") + "](" + node.getAttribute("src") + ")";
    case "TABLE": {
      const rows = Array.from(node.querySelectorAll("tr")).map((tr) =>
        Array.from(tr.children).map((cell) => toMarkdown(cell).replace(/\s*\n\s*/g, " ").trim()));
      if (rows.length === 0) return "";
      const width = Math.max(...rows.map((row) => row.length), 1);
      const line = (row) => "| " + Array.from({ length: width }, (_, i) => row[i] || "").join(" | ") + " |";
      const separator = "| " + Array(width).fill("---").join(" | ") + " |";
      return "\n\n" + [line(rows[0]), separator, ...rows.slice(1).map(line)].join("\n") + "\n\n";
    }
    default:
      return children();
  }
}

async function copyMarkdown() {
  let markdown;
  if (state.mode === "doc") {
    markdown = toMarkdown(docForExport()).replace(/\n{3,}/g, "\n\n").trim();
  } else {
    const lang = effectiveLang();
    markdown = lang === "md" || lang === "txt"
      ? textarea.value
      : "```" + lang + "\n" + textarea.value.replace(/\n$/, "") + "\n```";
  }
  try {
    await writeClipboardText(markdown);
    showToast(t("CopiedMd"));
  } catch (err) {
    console.error(err);
    showToast(t("CopyFailed"));
  }
}

$("btn-copy").addEventListener("click", copyContent);
$("btn-copy-md").addEventListener("click", copyMarkdown);

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

const INDENT = "    ";

function insertIndent() {
  // Prefer `execCommand` so the edit stays in the native undo history.
  if (!document.execCommand("insertText", false, INDENT)) {
    textarea.setRangeText(INDENT, textarea.selectionStart, textarea.selectionEnd, "end");
  }
}

function removeIndent() {
  const lineStart = textarea.value.lastIndexOf("\n", textarea.selectionStart - 1) + 1;
  const indent = textarea.value.slice(lineStart, lineStart + INDENT.length).match(/^ +/);

  if (!indent) {
    return;
  }

  const width = indent[0].length;
  const start = textarea.selectionStart;
  const end = textarea.selectionEnd;

  textarea.setSelectionRange(lineStart, lineStart + width);

  if (!document.execCommand("delete")) {
    textarea.setRangeText("", lineStart, lineStart + width, "end");
  }

  textarea.setSelectionRange(Math.max(lineStart, start - width), Math.max(lineStart, end - width));
}

textarea.addEventListener("input", () => {
  renderCode();
  onLocalEdit();
});

textarea.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "s") {
    e.preventDefault();
    saveNow();
    return;
  }

  if (e.key === "Tab" && !e.ctrlKey && !e.metaKey && !e.altKey) {
    e.preventDefault();
    if (e.shiftKey) {
      removeIndent();
    } else {
      insertIndent();
    }
    renderCode();
    onLocalEdit();
  }
});

docEditor.addEventListener("input", () => onLocalEdit());

docEditor.addEventListener("keydown", (e) => {
  if ((e.ctrlKey || e.metaKey) && e.key === "s") {
    e.preventDefault();
    saveNow();
  }
});

if (window.ResizeObserver) {
  new ResizeObserver(updateLineNumbers).observe(textarea);
}
if (document.fonts && document.fonts.ready) {
  document.fonts.ready.then(updateLineNumbers);
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

titleInput.addEventListener("input", () => {
  if (state.padId) state.titleDirty = true;
  scheduleSave();
});

function filterLangs() {
  const term = langFilter.value.toLowerCase();
  for (const option of langSelect.options) {
    const name = option.text.toLowerCase();
    const ext = option.value.toLowerCase();
    option.hidden = !(name.includes(term) || ext.includes(term));
  }
}

langFilter.addEventListener("input", filterLangs);

langSelect.addEventListener("change", () => {
  const value = langSelect.value;
  if (value === "rich") {
    switchToDoc();
  } else if (state.mode === "doc" && !switchToCode()) {
    selectLang("rich");
    return;
  }
  state.lang = value;
  state.previewing = false;
  updateAutoLabel();
  updateView();
  if (state.mode === "code") renderCode();
  onLocalEdit(true);
});

for (const radio of document.querySelectorAll('#expiry-list input[name="expires"]')) {
  radio.addEventListener("change", () => {
    // Without a pad this only picks the expiration of the next one.
    if (!state.padId) return;
    state.expiryDirty = true;
    scheduleSave(true);
  });
}

padList.addEventListener("click", (e) => {
  const item = e.target.closest(".pad-item");
  if (item && item.dataset.id !== state.padId) openPad(item.dataset.id, true);
});

for (const link of [btnDownload, btnRaw]) {
  link.addEventListener("click", (e) => {
    if (!state.padId) e.preventDefault();
  });
}

btnDelete.addEventListener("click", async () => {
  if (!state.padId || !window.confirm(t("ConfirmDelete"))) return;
  const id = state.padId;
  try {
    const resp = await fetch("/api/pad/" + encodeURIComponent(id), { method: "DELETE" });
    if (!resp.ok && resp.status !== 404) throw new Error("delete failed: " + resp.status);
    if (state.padId === id) newPad();
    refreshPads();
  } catch (err) {
    console.error(err);
    setStatus(t("Offline"));
  }
});

$("btn-new").addEventListener("click", async () => {
  await flushSave();
  newPad();
});

$("btn-home").addEventListener("click", (e) => {
  e.preventDefault();
  history.pushState(null, "", "/");
  openLatest();
});

btnPreview.addEventListener("click", togglePreview);

window.addEventListener("popstate", route);

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

setMode("code");
connectEvents();
refreshPads();
route();
