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
const padActions = $("pad-actions");
const btnPreview = $("btn-preview");
const btnDownload = $("btn-download");
const btnRaw = $("btn-raw");

// Localized strings, passed in as `data-i18n-*` attributes.
function t(key) {
  return root.dataset["i18n" + key] || "";
}

const SAVE_DELAY_MS = 250;
const HIGHLIGHT_DELAY_MS = 800;
const PAD_LIST_DELAY_MS = 5000;
const MAX_RETRY_DELAY_MS = 30000;

const state = {
  padId: null,
  // "code" edits plain text in the textarea, "doc" edits HTML in the document editor.
  mode: "code",
  previewing: false,
  // Selected language: "auto", "rich" (document) or a file extension.
  lang: "auto",
  // Extension the server detected for "auto".
  detected: "txt",
  // Content and extension the server last acknowledged, and its revision. Revisions of the
  // same pad compare in order.
  lastSaved: null,
  lastSavedExt: null,
  revision: null,
  titleDirty: false,
  expiryDirty: false,
  // Seconds until the current pad expires, `null` if it never does.
  expiresIn: null,
  saving: false,
  pending: false,
  saveTimer: null,
  retryDelay: 0,
  // Key that makes retries of a lost create return the same pad.
  createKey: null,
  // An update was announced while local edits were pending: check again once they are saved.
  remoteSkipped: false,
  // The open pad was deleted elsewhere; the editor is read-only until New.
  deleted: false,
  // Bumped whenever another pad is shown, so that late responses for the old one are dropped.
  generation: 0,
  // Highlighted HTML of each line the server highlighted last, keyed by the line's text.
  rows: new Map(),
};

// == Helpers

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
  toast.textContent = text;
  toast.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => toast.classList.add("hidden"), 1500);
}

function setStatus(text) {
  syncStatus.textContent = text;
}

function padUrl(id) {
  return "/pad/" + id;
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
  helper.className = "clipboard-helper";
  document.body.appendChild(helper);
  helper.select();
  const ok = document.execCommand("copy");
  helper.remove();
  return ok ? Promise.resolve() : Promise.reject(new Error("copy failed"));
}

async function fetchPad(id) {
  try {
    const resp = await fetch("/api/pad/" + encodeURIComponent(id));
    return resp.ok ? await resp.json() : null;
  } catch (err) {
    console.error(err);
    return null;
  }
}

// == Sanitizer for document HTML: pasted, loaded from the server or received live.

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

function isAssetUrl(url) {
  return /^\/raw\/[A-Za-z0-9_+-]+(?:\.[A-Za-z0-9]+)?$/.test(url);
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
      if (/^(?:https?:|mailto:|\/(?!\/)|#)/i.test(href.trim())) el.setAttribute("href", href);
    } else if (name === "img") {
      const src = child.getAttribute("src") || "";
      if (!isAssetUrl(src)) continue;
      el.setAttribute("src", src);
      el.setAttribute("alt", child.getAttribute("alt") || "");
    } else if (name === "figure" && child.classList.contains("inline-asset")) {
      el.className = "inline-asset";
      const kind = child.getAttribute("data-kind");
      if (kind === "image" || kind === "pdf") el.setAttribute("data-kind", kind);
      const src = child.getAttribute("data-src") || "";
      if (isAssetUrl(src)) el.setAttribute("data-src", src);
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

// == Code editor: textarea over a highlighted copy, with a soft-wrap aware gutter.

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
  const lines = content.split("\n");
  doc.querySelectorAll('.src-code [id^="LC"]').forEach((row, i) => {
    if (i < lines.length) state.rows.set(lines[i], row.innerHTML);
  });
}

// Lines the server highlighted keep their colors while typing; edited lines show
// plain until the editor is idle and their highlighting is fetched.
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

let highlightTimer = null;
function scheduleHighlight() {
  clearTimeout(highlightTimer);
  if (state.mode !== "code" || !state.padId) return;
  highlightTimer = setTimeout(async () => {
    if (hasUnsavedChanges()) return;
    const pad = await fetchPad(state.padId);
    if (pad && pad.content === textarea.value && state.mode === "code") {
      setHighlightRows(pad.content, pad.highlighted_html);
      renderHighlight();
    }
  }, HIGHLIGHT_DELAY_MS);
}

// == Document editor

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
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "asset-remove";
      remove.textContent = t("AssetRemove");
      wrap.append(" · ", remove);
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
  const name = escapeHtml(asset.filename);
  const url = escapeHtml(asset.url);
  const dl = '<a href="' + escapeHtml(asset.dl_url) + '">' + escapeHtml(t("AssetDownload")) + "</a>";
  const caption = "<figcaption>" + name + " · " + escapeHtml(asset.size_formatted) + " · ";
  if (asset.is_pdf) {
    return '<figure class="inline-asset" data-kind="pdf" data-src="' + url + '">' + caption +
      '<a href="' + url + '">' + escapeHtml(t("AssetOpen")) + "</a> · " + dl + "</figcaption></figure>";
  }
  return '<figure class="inline-asset" data-kind="image"><img src="' + url + '" alt="' + name +
    '">' + caption + dl + "</figcaption></figure>";
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
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (remaining <= node.nodeValue.length) {
      window.getSelection().collapse(node, remaining);
      return;
    }
    remaining -= node.nodeValue.length;
  }
}

// Inline assets are blocks: insert them after the line holding the caret (replacing it when
// empty), followed by an empty line to keep typing in.
function insertBlocks(html) {
  const holder = document.createElement("div");
  holder.innerHTML = html;
  hydrateAssets(holder);
  const nodes = Array.from(holder.childNodes);
  const selection = window.getSelection();

  let line = selection.rangeCount && docEditor.contains(selection.anchorNode) ? selection.anchorNode : null;
  while (line && line.parentNode !== docEditor) line = line.parentNode;
  if (line && line.nodeType === Node.ELEMENT_NODE && line.tagName !== "FIGURE" && !docHasContent(line)) {
    line.replaceWith(...nodes);
  } else if (line) {
    line.after(...nodes);
  } else {
    docEditor.append(...nodes);
  }

  const last = nodes[nodes.length - 1];
  let next = last.nextSibling;
  if (!next || next.nodeName === "FIGURE") {
    next = document.createElement("div");
    next.append(document.createElement("br"));
    last.after(next);
  }
  selection.collapse(next, 0);
}

// == Modes and views

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
  const detected = state.mode === "code" && state.lang === "auto" && state.detected !== "txt";
  const option = Array.from(langSelect.options).find((o) => o.index > 1 && o.value === state.detected);
  langSelect.options[0].textContent = detected
    ? t("Auto") + " · " + (option ? option.textContent : state.detected)
    : t("Auto");
}

// == Stats

const stats = $("stats");
const MAX_BYTES = parseInt(stats.dataset.maxBytes, 10) || 1024 * 1024;
const UNIT_KB = stats.dataset.unitKb;
const UNIT_MB = stats.dataset.unitMb;

function formatSize(bytes) {
  if (bytes >= 1024 * 1024) return (bytes / (1024 * 1024)).toFixed(1) + " " + UNIT_MB;
  return (bytes / 1024).toFixed(0) + " " + UNIT_KB;
}

$("progress-limit").textContent = stats.dataset.labelLimit + " " + formatSize(MAX_BYTES);

function updateStats() {
  const text = state.mode === "doc" ? docText(docEditor) : textarea.value;
  const bytes = new Blob([currentContent()]).size;

  $("stat-lines").textContent = Math.max(1, text.split("\n").length);
  $("stat-chars").textContent = text.length;
  $("stat-bytes").textContent = bytes.toLocaleString();

  const pct = Math.min(100, (bytes / MAX_BYTES) * 100);
  const fill = $("progress-fill");
  fill.style.width = pct + "%";
  fill.classList.toggle("warn", pct > 85);
  $("progress-kb").textContent = (bytes / 1024).toFixed(1) + " " + UNIT_KB;
}

// == Pad metadata in the panel

const expiryRadios = Array.from(document.querySelectorAll('#expiry-list input[name="expires"]'));
const defaultExpiry = expiryRadios.find((radio) => radio.checked);

function selectedExpiry() {
  const checked = expiryRadios.find((radio) => radio.checked);
  return checked ? parseInt(checked.value, 10) : 0;
}

// Show the current pad's expiration as the shortest option that still covers it.
function syncExpiryRadios() {
  if (!state.padId) return;
  const remaining = state.expiresIn;
  const match = remaining === null
    ? expiryRadios.find((radio) => radio.value === "0")
    : expiryRadios
      .filter((radio) => radio.value !== "0" && parseInt(radio.value, 10) >= remaining)
      .sort((a, b) => parseInt(a.value, 10) - parseInt(b.value, 10))[0];
  for (const radio of expiryRadios) radio.checked = radio === match;
}

function updatePadActions() {
  padActions.classList.toggle("hidden", !state.padId);
  btnDownload.href = state.padId ? "/dl/" + state.padId : "#";
  btnRaw.href = state.padId ? "/raw/" + state.padId : "#";
  for (const item of padList.querySelectorAll(".pad-item")) {
    item.classList.toggle("active", item.dataset.id === state.padId);
  }
}

// Label of the current pad in the pad list: its first line of text without Markdown markers,
// or the name of its first image or PDF.
function padLabel() {
  const text = state.mode === "doc" ? docText(docEditor) : textarea.value;
  const line = text.split("\n").find((l) => l.trim());
  if (line) {
    return line.trim().replace(/^(?:#{1,6}\s+|[-*+]\s+(?:\[[ xX]\]\s+)?|>\s*|\d+[.)]\s+)/, "");
  }
  const caption = state.mode === "doc" && docEditor.querySelector("figure figcaption");
  return caption ? caption.firstChild.textContent.split(" · ")[0] : "";
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

// == Saving

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
  if (state.deleted) return;
  clearTimeout(state.saveTimer);
  state.saveTimer = null;
  if (immediate) {
    saveNow();
    return;
  }
  state.saveTimer = setTimeout(saveNow, SAVE_DELAY_MS);
}

// Try a failed save again, waiting longer each time.
function scheduleRetry() {
  state.retryDelay = Math.min(MAX_RETRY_DELAY_MS, state.retryDelay * 2 || 1000);
  clearTimeout(state.saveTimer);
  state.saveTimer = setTimeout(saveNow, state.retryDelay);
}

function onLocalEdit(immediate) {
  if (state.mode === "doc" && !docHasContent(docEditor) && docEditor.childNodes.length) {
    // Drop the stray <br> left behind, so that the placeholder shows again.
    docEditor.replaceChildren();
  }
  updateStats();
  scheduleSave(immediate);
}

async function saveNow(force) {
  clearTimeout(state.saveTimer);
  state.saveTimer = null;
  if (state.deleted) return;

  if (state.saving) {
    state.pending = true;
    return;
  }

  const content = currentContent();
  const extension = currentExtension();
  const isCreate = state.padId === null;

  if (isCreate && content.trim() === "" && !force) return;
  if (!isCreate && content === state.lastSaved && extension === state.lastSavedExt &&
      !state.titleDirty && !state.expiryDirty) {
    return;
  }

  const body = { content, extension, preview: padLabel() };
  const sentTitle = state.titleDirty;
  const sentExpiry = isCreate || state.expiryDirty;
  if (sentTitle) body.title = titleInput.value;
  if (sentExpiry) body.expires = selectedExpiry();
  if (isCreate) {
    state.createKey = state.createKey || Math.random().toString(36).slice(2) + Date.now().toString(36);
    body.create_key = state.createKey;
  } else {
    body.base_revision = state.revision;
  }
  state.titleDirty = false;
  state.expiryDirty = false;
  const keepDirty = () => {
    state.titleDirty = state.titleDirty || sentTitle;
    state.expiryDirty = state.expiryDirty || (sentExpiry && !isCreate);
  };

  const generation = state.generation;
  let retry = false;
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
      // Someone else saved first.
      keepDirty();
      rebase(content, await resp.json());
      return;
    }
    if (resp.status === 404 && !isCreate) {
      markDeleted();
      return;
    }
    if (resp.status >= 400 && resp.status < 500) {
      // Retrying would not help: keep the edits and say why they are not saved.
      keepDirty();
      setStatus(resp.status === 413 ? t("TooLarge") : t("Error"));
      return;
    }
    if (!resp.ok) throw new Error("save failed: " + resp.status);

    const pad = await resp.json();
    if (generation !== state.generation) return;

    if (isCreate) {
      state.padId = pad.id;
      state.createKey = null;
      history.replaceState(null, "", padUrl(pad.id));
    }
    state.lastSaved = content;
    state.lastSavedExt = extension;
    state.revision = pad.revision;
    state.expiresIn = pad.expires_in ?? null;
    state.retryDelay = 0;

    if (state.mode === "code") {
      state.detected = pad.extension;
      updateAutoLabel();
      scheduleHighlight();
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
      keepDirty();
      setStatus(t("Offline"));
      retry = true;
    }
  } finally {
    state.saving = false;
    if (retry) {
      scheduleRetry();
    } else if (state.pending) {
      state.pending = false;
      saveNow();
    } else if (state.remoteSkipped) {
      state.remoteSkipped = false;
      syncRemote();
    }
  }
}

// Save what is pending, once, and wait for it. Returns whether everything reached the server.
async function flushSave() {
  if (state.saveTimer !== null || state.pending || (!state.saving && hasUnsavedChanges())) {
    await saveNow();
  }
  while (state.saving || state.pending) {
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  return !hasUnsavedChanges();
}

// Whether the editor may show something else: never drop unsaved edits without asking.
async function canLeave() {
  return (await flushSave()) || window.confirm(t("ConfirmDiscard"));
}

// The pad must exist before images can be uploaded into it.
async function ensurePad() {
  await flushSave();
  if (!state.padId) await saveNow(true);
  return state.padId;
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
  const ours = changedRegion(base, mine);
  const other = changedRegion(base, theirs);
  const inserted = mine.slice(ours.start, ours.endAfter);
  if (ours.endBefore <= other.start) {
    return theirs.slice(0, ours.start) + inserted + theirs.slice(ours.endBefore);
  }
  if (other.endBefore <= ours.start) {
    const delta = theirs.length - base.length;
    return theirs.slice(0, ours.start + delta) + inserted + theirs.slice(ours.endBefore + delta);
  }
  return null;
}

// Replay the local edit `mine` (made on top of `state.lastSaved`) onto the newer `pad` and
// save again. When both changed the same text, nothing is overwritten: the editor shows the
// newer pad and says so.
function rebase(mine, pad) {
  const current = currentContent();
  const merged = merge(state.lastSaved ?? "", mine, pad.content);
  // Keep anything typed while the request was in flight.
  const result = merged === null || current === mine ? merged : merge(mine, current, merged);

  if (result === null) {
    applyPad(pad, true);
    setStatus(t("Conflict"));
    return;
  }

  state.lastSaved = pad.content;
  state.revision = pad.revision;
  if (result !== current) setContent(result);
  state.pending = true;
}

// The open pad was deleted or expired elsewhere: keep showing its text, read-only, until New.
function markDeleted() {
  clearTimeout(state.saveTimer);
  Object.assign(state, { saveTimer: null, pending: false, padId: null, revision: null, deleted: true });
  state.lastSaved = currentContent();
  setReadOnly(true);
  setStatus(t("Deleted"));
  updatePadActions();
  updateView();
}

function setReadOnly(readOnly) {
  textarea.readOnly = readOnly;
  titleInput.readOnly = readOnly;
  docEditor.contentEditable = String(!readOnly);
}

// Replace the text of the current editor, keeping the caret where it was relative to the text.
function setContent(text) {
  if (state.mode === "doc") {
    const caret = docCaretOffset();
    setDocHtml(text);
    restoreDocCaret(caret);
    updateStats();
    return;
  }
  const [before, start, end] = [textarea.value, textarea.selectionStart, textarea.selectionEnd];
  const focused = document.activeElement === textarea;
  textarea.value = text;
  if (focused) textarea.setSelectionRange(shiftOffset(before, text, start), shiftOffset(before, text, end));
  renderCode();
}

// == Loading

function resetEditor() {
  clearTimeout(state.saveTimer);
  state.saveTimer = null;
  state.pending = false;
  state.generation++;
  Object.assign(state, {
    padId: null,
    previewing: false,
    lang: "auto",
    detected: "txt",
    lastSaved: null,
    lastSavedExt: null,
    revision: null,
    titleDirty: false,
    expiryDirty: false,
    expiresIn: null,
    retryDelay: 0,
    createKey: null,
    remoteSkipped: false,
    deleted: false,
    rows: new Map(),
  });
  setReadOnly(false);

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
  updatePadActions();
  setStatus("");
}

function newPad() {
  resetEditor();
  history.pushState(null, "", "/new");
  textarea.focus();
}

// Show `pad` in the editor. For live updates the caret and scroll position are kept.
function applyPad(pad, live) {
  const scrollTop = editorWrap.scrollTop;
  const content = pad.content ?? "";

  state.padId = pad.id;
  state.lastSaved = content;
  state.revision = pad.revision;
  state.expiresIn = pad.expires_in ?? null;
  if (!live || document.activeElement !== titleInput) {
    titleInput.value = pad.title || "";
    state.titleDirty = false;
  }

  if (pad.extension === "rich") {
    state.lang = "rich";
  } else if (!live || state.lang !== "auto") {
    state.lang = !live && pad.auto_detected ? "auto" : pad.extension;
    state.detected = pad.extension;
  }
  state.lastSavedExt = state.lang;
  selectLang(state.lang);
  setHighlightRows(content, pad.highlighted_html);
  const mode = pad.extension === "rich" ? "doc" : "code";
  if (state.mode !== mode) {
    textarea.value = "";
    docEditor.replaceChildren();
  }
  setMode(mode);
  setContent(content);
  // The sanitized form is what this editor sends back.
  if (mode === "doc") state.lastSaved = serializeDoc();

  updateAutoLabel();
  syncExpiryRadios();
  updatePadActions();
  updateView();
  if (live) {
    editorWrap.scrollTop = scrollTop;
    if (state.previewing) renderPreview();
  }
}

// Open pad `id`, or the most recently changed one.
async function openPad(id, push) {
  if (!(await canLeave())) {
    if (state.padId) history.replaceState(null, "", padUrl(state.padId));
    return;
  }
  resetEditor();
  const generation = state.generation;
  const pad = id ? await fetchPad(id) : await fetch("/api/pad/latest").then((r) => r.json(), () => null);
  if (generation !== state.generation) return;
  if (!pad) {
    if (id) history.replaceState(null, "", "/new");
    return;
  }
  applyPad(pad, false);
  history[push ? "pushState" : "replaceState"](null, "", padUrl(pad.id));
}

function route() {
  // `/?pad=` links of earlier builds open, and are rewritten to, `/pad/{id}`.
  const path = window.location.pathname.match(/^\/pad\/(.+)$/);
  const id = path ? decodeURIComponent(path[1]) : new URLSearchParams(window.location.search).get("pad");
  if (id) return openPad(id, false);
  if (window.location.pathname === "/new") {
    resetEditor();
    return Promise.resolve();
  }
  return openPad(null, false);
}

// The Markdown preview is the article of the pad's rendered view, sanitized by the server.
async function renderPreview() {
  try {
    const resp = await fetch("/md/" + encodeURIComponent(state.padId));
    const doc = new DOMParser().parseFromString(await resp.text(), "text/html");
    mdPreview.replaceChildren(...(doc.querySelector("article.markdown-body")?.childNodes || []));
  } catch (err) {
    console.error(err);
    setStatus(t("Offline"));
  }
}

async function togglePreview() {
  state.previewing = !state.previewing;
  if (state.previewing) {
    await flushSave();
    await renderPreview();
  }
  updateView();
  if (!state.previewing) {
    renderCode();
    textarea.focus();
  }
}

// == Live updates: events only say which pad changed to which revision.

async function syncRemote() {
  if (!state.padId) return;
  if (hasUnsavedChanges()) {
    // The next save is refused and rebased; or, if it gets through first, check again after it.
    state.remoteSkipped = true;
    return;
  }
  const generation = state.generation;
  const pad = await fetchPad(state.padId);
  if (!pad || generation !== state.generation || hasUnsavedChanges() ||
      (state.revision && pad.revision <= state.revision)) {
    return;
  }
  applyPad(pad, true);
  setStatus(t("Live"));
}

function onPadEvent(event) {
  let msg;
  try {
    msg = JSON.parse(event.data);
  } catch (_) {
    return;
  }

  if (msg.type === "update") {
    refreshPadsSoon();
  } else {
    refreshPads();
  }

  if (!msg.id || msg.id !== state.padId) return;

  if (msg.type === "deleted") {
    markDeleted();
    return;
  }

  if (msg.revision && (!state.revision || msg.revision > state.revision)) syncRemote();
}

function connectEvents() {
  if (!window.EventSource) return;
  const events = new EventSource("/api/pad/events");
  let disconnected = false;
  events.addEventListener("pad_update", onPadEvent);
  events.addEventListener("error", () => {
    disconnected = true;
  });
  events.addEventListener("open", () => {
    if (!disconnected) return;
    disconnected = false;
    // Back online: catch up, and send what could not be saved meanwhile.
    refreshPads();
    if (hasUnsavedChanges()) {
      state.retryDelay = 0;
      saveNow();
    } else {
      syncRemote();
    }
  });
}

window.addEventListener("beforeunload", (e) => {
  if (hasUnsavedChanges()) e.preventDefault();
});

// == Files, clipboard and drag & drop

function isMediaFile(file) {
  return file.type.startsWith("image/") || file.type === "application/pdf";
}

// Upload an image or PDF as is, into the open pad, which deletes it along with itself.
async function upload(blob, filename) {
  const query = "?pad=" + encodeURIComponent(state.padId) + "&filename=" + encodeURIComponent(filename);
  const resp = await fetch("/api/asset" + query, {
    method: "POST",
    headers: { "Content-Type": blob.type },
    body: blob,
  });
  if (!resp.ok) throw new Error("upload failed: " + resp.status);
  return resp.json();
}

async function insertAssets(files) {
  if (state.deleted) return;
  switchToDoc();
  if (!(await ensurePad())) {
    showToast(t("UploadFailed"));
    return;
  }
  for (const file of files) {
    setStatus(t("Uploading"));
    try {
      insertBlocks(figureHtml(await upload(file, file.name || "image")));
    } catch (err) {
      console.error(err);
      showToast(t("UploadFailed"));
    }
  }
  onLocalEdit(true);
}

// A text file opens as a new pad, in the language its extension names.
async function loadTextFile(file) {
  const text = await file.text();
  if (!(await canLeave())) return;
  newPad();
  textarea.value = text.replace(/\n$/, "");
  const ext = (file.name.match(/\.([^.]+)$/) || [])[1] || "auto";
  state.lang = ext !== "rich" && Array.from(langSelect.options).some((o) => o.value === ext) ? ext : "auto";
  selectLang(state.lang);
  titleInput.value = file.name;
  state.titleDirty = true;
  renderCode();
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

// Images and PDFs pasted into the code editor start a document; text pastes natively.
textarea.addEventListener("paste", async (e) => {
  const files = e.clipboardData ? clipboardFiles(e.clipboardData) : [];
  if (files.length && !e.clipboardData.getData("text/plain")) {
    e.preventDefault();
    await insertAssets(files);
  }
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
    document.execCommand("insertHTML", false, sanitizeHtml(html).innerHTML);
  } else if (text) {
    // Inserted at the caret, replacing the selection, as part of the undo history.
    document.execCommand("insertText", false, text.replace(/\r\n?/g, "\n"));
  }
});

docEditor.addEventListener("click", (e) => {
  const remove = e.target.closest(".asset-remove");
  if (!remove) return;
  e.preventDefault();
  remove.closest("figure").remove();
  onLocalEdit(true);
});

const editorArea = editorWrap.parentElement;

function isFileDrag(e) {
  return Array.from(e.dataTransfer?.types || []).includes("Files");
}

editorArea.addEventListener("dragover", (e) => {
  if (!isFileDrag(e)) return;
  e.preventDefault();
  overlay.classList.add("active");
});

editorArea.addEventListener("dragleave", (e) => {
  if (!editorArea.contains(e.relatedTarget)) overlay.classList.remove("active");
});

editorArea.addEventListener("drop", (e) => {
  overlay.classList.remove("active");
  if (!isFileDrag(e)) return;
  e.preventDefault();
  handleFiles(e.dataTransfer.files);
});

$("open").addEventListener("click", () => {
  const input = document.createElement("input");
  input.type = "file";
  input.multiple = true;
  input.addEventListener("change", () => handleFiles(input.files));
  input.click();
});

// == Copying

async function copyText(text, toast) {
  try {
    await writeClipboardText(text);
    showToast(toast);
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

  switch (tag) {
    case "FIGURE": {
      const img = node.querySelector("img");
      const link = node.querySelector("figcaption a");
      const name = (node.querySelector("figcaption")?.textContent || "").split(" · ")[0];
      const url = absoluteUrl(img ? img.getAttribute("src") : link.getAttribute("href"));
      return "\n\n" + (img ? "!" : "") + "[" + name + "](" + url + ")\n\n";
    }
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
    case "CODE":
      return node.parentElement.tagName === "PRE" ? node.textContent : "`" + node.textContent + "`";
    case "PRE":
      return "\n\n```\n" + node.textContent.replace(/\n$/, "") + "\n```\n\n";
    case "UL":
    case "OL": {
      const items = Array.from(node.children)
        .filter((child) => child.tagName === "LI")
        .map((li, i) => (tag === "OL" ? i + 1 + ". " : "- ") + toMarkdown(li).trim());
      return "\n\n" + items.join("\n") + "\n\n";
    }
    case "A": {
      const href = absoluteUrl(node.getAttribute("href") || "");
      return "[" + (children().trim() || href) + "](" + href + ")";
    }
    default:
      return children();
  }
}

function markdown() {
  if (state.mode === "doc") return toMarkdown(docEditor).replace(/\n{3,}/g, "\n\n").trim();
  const lang = effectiveLang();
  if (lang === "md" || lang === "txt") return textarea.value;
  return "```" + lang + "\n" + textarea.value.replace(/\n$/, "") + "\n```";
}

$("btn-copy").addEventListener("click", () =>
  copyText(state.mode === "doc" ? docText(docEditor) : textarea.value, t("Copied")));
$("btn-copy-md").addEventListener("click", () => copyText(markdown(), t("CopiedMd")));

// == Editing

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

document.addEventListener("keydown", (e) => {
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

// == Panel

titleInput.addEventListener("input", () => {
  // Sent with the next save, including one that creates the pad later.
  state.titleDirty = true;
  scheduleSave();
});

function filterLangs() {
  const term = langFilter.value.toLowerCase();
  for (const option of langSelect.options) {
    option.hidden = !(option.text.toLowerCase().includes(term) || option.value.includes(term));
  }
}

langFilter.addEventListener("input", filterLangs);

langSelect.addEventListener("change", () => {
  const value = langSelect.value;
  if (state.deleted) {
    selectLang(state.lang);
    return;
  }
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

for (const radio of expiryRadios) {
  radio.addEventListener("change", () => {
    // Without a pad this only picks the expiration of the next one.
    if (!state.padId || state.deleted) return;
    state.expiryDirty = true;
    scheduleSave(true);
  });
}

padList.addEventListener("click", (e) => {
  const item = e.target.closest(".pad-item");
  if (item && item.dataset.id !== state.padId) openPad(item.dataset.id, true);
});

$("btn-delete").addEventListener("click", async () => {
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
  if (await canLeave()) newPad();
});

$("btn-home").addEventListener("click", (e) => {
  e.preventDefault();
  history.pushState(null, "", "/");
  openPad(null, false);
});

btnPreview.addEventListener("click", togglePreview);

// Language and expiration live in a popover behind the cog.
const btnSettings = $("btn-settings");
const settings = $("settings");

function showSettings(open) {
  settings.classList.toggle("hidden", !open);
  btnSettings.setAttribute("aria-expanded", String(open));
}

btnSettings.addEventListener("click", () => showSettings(settings.classList.contains("hidden")));
document.addEventListener("click", (e) => {
  if (!settings.contains(e.target) && !btnSettings.contains(e.target)) showSettings(false);
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape") showSettings(false);
});

// The sidebar collapses; the choice is remembered per browser.
const contentArea = $("content-area");
const btnSidebar = $("btn-sidebar");

function showSidebar(open) {
  contentArea.classList.toggle("sidebar-collapsed", !open);
  btnSidebar.setAttribute("aria-expanded", String(open));
  try {
    localStorage.setItem("wastebin-sidebar", open ? "open" : "closed");
  } catch (_) {
    // Storage may be unavailable; the sidebar then starts open.
  }
}

btnSidebar.addEventListener("click", () => showSidebar(contentArea.classList.contains("sidebar-collapsed")));
try {
  if (localStorage.getItem("wastebin-sidebar") === "closed") showSidebar(false);
} catch (_) {
  // See above.
}

window.addEventListener("popstate", route);

// == Start

setMode("code");
connectEvents();
refreshPads();
route();
