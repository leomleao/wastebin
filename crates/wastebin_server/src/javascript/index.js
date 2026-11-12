function $(id) {
  return document.getElementById(id);
}

const clientId = "c_" + Math.random().toString(36).slice(2) + Date.now().toString(36);

// DOM Elements
const visualEditor = $("visual-editor");
const codeEditorWrap = $("code-editor-wrap");
const codeHighlightLayer = $("code-highlight-layer");
const codeTextarea = $("code-textarea");
const lineNumbers = $("line-numbers");
const markdownPreview = $("markdown-preview");
const editorWrap = $("editor-wrap");
const dropOverlay = $("drop-overlay");

const btnHome = $("btn-home");
const btnNew = $("btn-new");
const recentSelect = $("recent-select");
const syncStatus = $("sync-status");
const syncText = $("sync-text");
const langPill = $("lang-pill");
const btnCopy = $("btn-copy");
const btnCopyMd = $("btn-copy-md");
const btnOpen = $("open");
const fileInput = $("file-input");

const btnSettings = $("btn-settings");
const settingsPopover = $("settings-popover");
const settingExpires = $("setting-expires");
const settingLang = $("setting-lang");
const btnDownload = $("btn-download");
const btnRaw = $("btn-raw");
const btnDelete = $("btn-delete");
const toastEl = $("toast");

// State
let currentPadId = null;
let currentLangSetting = "auto";
let detectedLang = "txt";
let currentExpiresSecs = loadSavedExpires();
let editorMode = "visual"; // "visual" | "code" | "preview"
let previousEditMode = "visual";
let saveTimer = null;
let isSaving = false;
let pendingSaveAfterCurrent = false;
let isApplyingRemote = false;
let lastSavedSignature = "";
let lastServerRenderedMd = "";
let editRevision = 0;

// Language display names
const LANG_NAMES = {
  auto: "Auto",
  rich: "Document",
  html_doc: "Document",
  js: "JavaScript",
  javascript: "JavaScript",
  ts: "TypeScript",
  typescript: "TypeScript",
  php: "PHP",
  abap: "ABAP",
  md: "Markdown",
  markdown: "Markdown",
  py: "Python",
  python: "Python",
  sql: "SQL",
  json: "JSON",
  html: "HTML",
  css: "CSS",
  sh: "Bash",
  bash: "Bash",
  rs: "Rust",
  rust: "Rust",
  yaml: "YAML",
  yml: "YAML",
  txt: "Plain Text",
};

const CODE_LANGS = new Set([
  "js", "javascript", "ts", "typescript", "php", "abap",
  "py", "python", "sql", "json", "html", "css", "sh", "bash",
  "rs", "rust", "yaml", "yml", "c", "cpp", "go", "java", "rb", "toml", "xml"
]);

function loadSavedExpires() {
  try {
    const raw = localStorage.getItem("stash_expires");
    if (raw !== null) {
      const parsed = parseInt(raw, 10);
      if (!Number.isNaN(parsed) && parsed >= 0) {
        return parsed;
      }
    }
  } catch (_) {}
  return 2592000; // Default: 30 days
}

function saveExpiresPreference(secs) {
  currentExpiresSecs = secs;
  try {
    localStorage.setItem("stash_expires", String(secs));
  } catch (_) {}
}

function initExpirationUI() {
  if (!settingExpires) return;
  const radios = settingExpires.querySelectorAll('input[name="stash_expires"]');
  let matched = false;
  radios.forEach((r) => {
    if (parseInt(r.value, 10) === currentExpiresSecs) {
      r.checked = true;
      matched = true;
    } else {
      r.checked = false;
    }
  });
  if (!matched) {
    radios.forEach((r) => {
      if (r.value === "2592000") r.checked = true;
    });
  }

  settingExpires.addEventListener("change", (e) => {
    if (e.target && e.target.name === "stash_expires") {
      const secs = parseInt(e.target.value, 10);
      if (!Number.isNaN(secs)) {
        saveExpiresPreference(secs);
        if (currentPadId || !isEditorEmpty()) {
          saveNow();
        }
      }
    }
  });
}

let toastTimeout = null;
function showToast(msg) {
  if (!toastEl) return;
  toastEl.textContent = msg;
  toastEl.classList.remove("hidden");
  clearTimeout(toastTimeout);
  toastTimeout = setTimeout(() => {
    toastEl.classList.add("hidden");
  }, 2200);
}

function setSyncStatus(state, label) {
  if (!syncStatus || !syncText) return;
  syncStatus.dataset.state = state;
  syncText.textContent = label;
}

function escapeHtml(str) {
  return String(str || "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function escapeAttr(str) {
  return escapeHtml(str).replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

// ============================================================================
// Line Numbers & Wrapping Mirror for Code Layer
// ============================================================================

const mirror = document.createElement("div");
mirror.id = "line-mirror";
mirror.setAttribute("aria-hidden", "true");
document.body.appendChild(mirror);

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

function unitAdvance(font) {
  advanceContext.font = font;
  return 2 * advanceContext.measureText("M").width;
}

function measureRows(lines) {
  if (!codeTextarea) return new Array(lines.length).fill(1);
  const style = getComputedStyle(codeTextarea);

  const scrollbar =
    codeTextarea.offsetWidth - codeTextarea.clientWidth -
    parseFloat(style.borderLeftWidth || 0) - parseFloat(style.borderRightWidth || 0);
  const width = Math.max(
    0,
    codeTextarea.getBoundingClientRect().width -
      parseFloat(style.borderLeftWidth || 0) -
      parseFloat(style.borderRightWidth || 0) -
      parseFloat(style.paddingLeft || 0) -
      parseFloat(style.paddingRight || 0) -
      scrollbar
  );

  const rows = new Array(lines.length).fill(1);
  if (width <= 0) return rows;

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

  const spans = [];
  const fragment = document.createDocumentFragment();
  candidateLines.forEach(function (index, i) {
    if (i > 0) fragment.appendChild(document.createTextNode("\n"));
    const span = document.createElement("span");
    span.textContent = lines[index];
    fragment.appendChild(span);
    spans.push(span);
  });
  mirror.replaceChildren(fragment);

  spans.forEach(function (span, i) {
    const boxes = new Set();
    for (const rect of span.getClientRects()) {
      boxes.add(Math.round(rect.top));
    }
    rows[candidateLines[i]] = Math.max(1, boxes.size);
  });

  return rows;
}

function updateLineNumbers() {
  if (!lineNumbers || !codeTextarea) return;
  const rows = measureRows(codeTextarea.value.split("\n"));
  let html = "";
  for (let i = 0; i < rows.length; i++) {
    html += "<div>" + (i + 1) + "</div>";
  }
  lineNumbers.innerHTML = html;

  const cells = lineNumbers.children;
  for (let i = 0; i < rows.length; i++) {
    if (rows[i] > 1 && cells[i]) {
      cells[i].style.setProperty("--rows", rows[i]);
    }
  }

  syncCodeScroll();
}

function syncCodeScroll() {
  if (!codeTextarea) return;
  if (lineNumbers && lineNumbers.scrollTop !== codeTextarea.scrollTop) {
    lineNumbers.scrollTop = codeTextarea.scrollTop;
  }
  if (codeHighlightLayer) {
    if (codeHighlightLayer.scrollTop !== codeTextarea.scrollTop) {
      codeHighlightLayer.scrollTop = codeTextarea.scrollTop;
    }
    if (codeHighlightLayer.scrollLeft !== codeTextarea.scrollLeft) {
      codeHighlightLayer.scrollLeft = codeTextarea.scrollLeft;
    }
  }
}

if (window.ResizeObserver && codeTextarea) {
  new ResizeObserver(() => {
    if (editorMode === "code") updateLineNumbers();
  }).observe(codeTextarea);
}
if (document.fonts && document.fonts.ready) {
  document.fonts.ready.then(() => {
    if (editorMode === "code") updateLineNumbers();
  });
}

// ============================================================================
// Smart Language Detection & Instant Client-Side Syntax Highlighter
// ============================================================================

function detectLanguageClient(text) {
  const trimmed = (text || "").trim();
  if (!trimmed) return "txt";

  // 1. JSON
  if (
    (trimmed.startsWith("{") && trimmed.endsWith("}")) ||
    (trimmed.startsWith("[") && trimmed.endsWith("]"))
  ) {
    if (trimmed.includes(":") || trimmed.length > 2) {
      try {
        JSON.parse(trimmed);
        return "json";
      } catch (_) {}
    }
  }

  // 2. PHP
  if (
    /<\?(?:php|=)/i.test(trimmed) ||
    (/(?:^\s*namespace\s+[\w\\]+;|^\s*use\s+[\w\\]+;|\$this->|\$[a-zA-Z_]\w*\s*=)/m.test(trimmed) &&
      /(?:function\s+\w+|public\s+function|private\s+function|protected\s+function|echo\s+|->|::class)/m.test(trimmed))
  ) {
    return "php";
  }

  // 3. ABAP
  if (
    /(?:^\s*REPORT\s+\w+|^\s*DATA:\s*|DATA\(\w+\)|FIELD-SYMBOLS:\s*<\w+>|LOOP\s+AT\s+\w+\s+(?:INTO|ASSIGNING)|ENDLOOP\.|SELECT\s+.*\s+FROM\s+\w+\s+INTO\s+TABLE\s+@?DATA|CALL\s+FUNCTION\s+'|^\s*FORM\s+\w+|^\s*ENDFORM\.|^\s*METHOD\s+\w+|^\s*ENDMETHOD\.|sy-subrc)/im.test(
      trimmed
    )
  ) {
    return "abap";
  }

  // 4. HTML
  if (
    /^<!DOCTYPE\s+html/i.test(trimmed) ||
    /^<html[\s>]/i.test(trimmed) ||
    (/<\/(?:div|section|article|main|header|footer|body|head|script|style|table|ul|ol)>/i.test(trimmed) &&
      /^<[a-zA-Z][\w-]*/m.test(trimmed))
  ) {
    return "html";
  }

  // 5. SQL
  if (
    /^\s*(?:SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|CREATE\s+(?:TABLE|VIEW|INDEX|DATABASE)|ALTER\s+TABLE|DROP\s+TABLE|WITH\s+\w+\s+AS)\b/im.test(
      trimmed
    ) &&
    /\b(?:FROM|WHERE|JOIN|LEFT\s+JOIN|INNER\s+JOIN|GROUP\s+BY|ORDER\s+BY|VALUES|SET|PRIMARY\s+KEY)\b/im.test(
      trimmed
    )
  ) {
    return "sql";
  }

  // 6. Rust
  if (
    /(?:^\s*(?:pub\s+)?fn\s+\w+|^\s*let\s+mut\s+\w+|^\s*impl(?:<[^>]+>)?\s+\w+|#\[derive\(|use\s+(?:std|crate|super)::|println!\(|Option<|Result<)/m.test(
      trimmed
    )
  ) {
    return "rs";
  }

  // 7. TypeScript
  if (
    /(?:^\s*(?:export\s+)?(?:interface|type|enum)\s+[A-Z]\w*|:\s*(?:string|number|boolean|any|unknown|void|never|Record<|Promise<|Array<)|\bas\s+const\b|import\s+type\s+)/m.test(
      trimmed
    )
  ) {
    return "ts";
  }

  // 8. JavaScript
  if (
    /(?:^\s*(?:const|let|var)\s+[\w{[].*=|^\s*(?:export\s+(?:default\s+)?)?(?:async\s+)?function\s*\w*\s*\(|=>\s*[{(]|console\.(?:log|error|warn)\(|document\.(?:getElementById|querySelector)|window\.addEventListener|require\(['"]|module\.exports\s*=)/m.test(
      trimmed
    )
  ) {
    return "js";
  }

  // 9. Python
  if (
    /(?:^\s*(?:async\s+)?def\s+\w+\s*\(.*\)\s*(?:->\s*[\w\[\], ]+)?\s*:|^\s*class\s+\w+(?:\([^)]*\))?\s*:|^\s*from\s+[\w.]+\s+import\s+|if\s+__name__\s*==\s*['"]__main__['"]\s*:|^\s*elif\s+.*:)/m.test(
      trimmed
    )
  ) {
    return "py";
  }

  // 10. Bash / Shell
  if (
    /^#!\s*\/(?:usr\/bin\/env\s+)?(?:ba|z)?sh/m.test(trimmed) ||
    /(?:^\s*set\s+-[euo]+|^\s*export\s+[A-Z0-9_]+=|\$\([a-zA-Z0-9_-]+|\b(?:fi|done|esac)\s*$)/m.test(
      trimmed
    )
  ) {
    return "sh";
  }

  // 11. CSS
  if (
    /(?:^:root\s*\{|@media\s+[^{]+\{|[.#]?[a-zA-Z][\w-]*\s*\{[^}]*\b(?:display|margin|padding|background|color|font-size|border|flex|grid)\s*:)/m.test(
      trimmed
    )
  ) {
    return "css";
  }

  // 12. Markdown
  if (
    /(?:^#{1,6}\s+\S|^```[\w-]*\s*$|^\s*[-*+]\s+\[[ xX]\]\s+|\[[^\]]+\]\(https?:\/\/[^)]+\)|^\s*>\s+\S)/m.test(
      trimmed
    )
  ) {
    return "md";
  }

  // 13. YAML
  const lines = trimmed.split("\n");
  if (
    lines.length >= 2 &&
    lines.filter((l) => /^[a-zA-Z0-9_-]+:\s*(?:\S.*)?$/.test(l.trim())).length >=
      Math.min(3, lines.length)
  ) {
    return "yaml";
  }

  return "txt";
}

function highlightCodeClient(code, lang) {
  if (!code) return "";
  const normalizedLang = (lang || "txt").toLowerCase();

  if (normalizedLang === "md" || normalizedLang === "markdown") {
    return code
      .split("\n")
      .map((line) => {
        if (/^#{1,6}\s/.test(line)) {
          return `<span class="markup heading tok-heading">${escapeHtml(line)}</span>`;
        }
        if (/^```/.test(line)) {
          return `<span class="comment tok-cmt">${escapeHtml(line)}</span>`;
        }
        if (/^\s*>\s/.test(line)) {
          return `<span class="comment tok-cmt">${escapeHtml(line)}</span>`;
        }
        return escapeHtml(line)
          .replace(/(`[^`]+`)/g, '<span class="string tok-str">$1</span>')
          .replace(/(\*\*[^*]+\*\*)/g, '<span class="keyword tok-kw">$1</span>');
      })
      .join("\n");
  }

  const tokenRegex =
    /(\/\/[^\n]*|#[^\n]*|--[^\n]*|\/\*[\s\S]*?\*\/|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`|\$[a-zA-Z_]\w*|\b(?:0x[0-9a-fA-F]+|\d+(?:\.\d+)?)\b|\b(?:function|const|let|var|return|if|else|elseif|elif|for|while|do|switch|case|break|continue|default|try|catch|finally|throw|new|class|extends|implements|interface|type|enum|export|import|from|as|async|await|yield|static|public|private|protected|readonly|abstract|namespace|use|echo|print|fn|pub|mut|impl|struct|trait|where|match|in|ref|move|Self|self|super|crate|mod|def|lambda|pass|raise|with|global|nonlocal|del|True|False|None|true|false|null|undefined|SELECT|FROM|WHERE|JOIN|LEFT|RIGHT|INNER|OUTER|ON|GROUP|BY|ORDER|HAVING|LIMIT|OFFSET|INSERT|INTO|VALUES|UPDATE|SET|DELETE|CREATE|TABLE|ALTER|DROP|AS|AND|OR|NOT|IN|IS|NULL|DISTINCT|UNION|ALL|REPORT|DATA|TYPES|FIELD-SYMBOLS|LOOP|AT|ENDLOOP|ASSIGNING|FORM|ENDFORM|METHOD|ENDMETHOD|CLASS|ENDCLASS|CALL|FUNCTION|WRITE|APPEND|READ|IF|ELSE|ELSEIF|ENDIF|CASE|WHEN|ENDCASE|PARAMETERS|TABLES|SELECT-OPTIONS)\b|\b[A-Z][A-Za-z0-9_]*\b|\b[a-zA-Z_]\w*(?=\s*\())/g;

  let result = "";
  let lastIndex = 0;
  let match;

  while ((match = tokenRegex.exec(code)) !== null) {
    if (match.index > lastIndex) {
      result += escapeHtml(code.slice(lastIndex, match.index));
    }
    const tok = match[0];
    let cls = "tok-kw keyword";
    if (
      tok.startsWith("//") ||
      tok.startsWith("#") ||
      tok.startsWith("--") ||
      tok.startsWith("/*")
    ) {
      cls = "comment line tok-cmt";
    } else if (
      tok.startsWith('"') ||
      tok.startsWith("'") ||
      tok.startsWith("`")
    ) {
      cls = "string quoted tok-str";
    } else if (tok.startsWith("$")) {
      cls = "variable other tok-var";
    } else if (/^(?:0x|\d)/.test(tok)) {
      cls = "constant numeric tok-num";
    } else if (
      code.slice(match.index + tok.length).match(/^\s*\(/) &&
      !/^(?:if|for|while|switch|catch|return|function|def|fn|match|LOOP|SELECT)$/i.test(tok)
    ) {
      cls = "entity name function tok-fn";
    } else if (/^[A-Z][a-z0-9_]/.test(tok)) {
      cls = "entity name type tok-type";
    }

    result += `<span class="${cls}">${escapeHtml(tok)}</span>`;
    lastIndex = tokenRegex.lastIndex;
  }

  if (lastIndex < code.length) {
    result += escapeHtml(code.slice(lastIndex));
  }

  if (code.endsWith("\n")) {
    result += "\n";
  }

  return result;
}

function updateCodeHighlightLayer(serverHighlightedHtml) {
  if (!codeHighlightLayer || !codeTextarea) return;
  const code = codeTextarea.value;

  if (serverHighlightedHtml && serverHighlightedHtml.includes('id="LC1"')) {
    try {
      const doc = new DOMParser().parseFromString(serverHighlightedHtml, "text/html");
      const rows = doc.querySelectorAll('[id^="LC"]');
      const codeLines = code.split("\n");
      if (rows.length === codeLines.length) {
        const joined = Array.from(rows)
          .map((r) => r.innerHTML)
          .join("\n");
        codeHighlightLayer.innerHTML = code.endsWith("\n") ? joined + "\n" : joined;
        syncCodeScroll();
        return;
      }
    } catch (_) {}
  }

  const effectiveLang =
    currentLangSetting !== "auto" && currentLangSetting !== "rich"
      ? currentLangSetting
      : detectedLang;
  codeHighlightLayer.innerHTML = highlightCodeClient(code, effectiveLang);
  syncCodeScroll();
}

// ============================================================================
// Mode Management: Visual Inline Editor vs Live Code Layer vs Markdown Preview
// ============================================================================

function hasRichHtmlElements(root) {
  if (!root) return false;
  return Boolean(
    root.querySelector(
      "figure, img, iframe, .inline-asset, table, h1, h2, h3, h4, h5, h6, ul, ol, blockquote, pre, hr"
    )
  );
}

function extractPlainTextFromVisual() {
  if (!visualEditor) return "";
  // Walk child nodes to preserve line breaks accurately
  const html = visualEditor.innerHTML || "";
  if (!html || html === "<br>" || html === "<p><br></p>") return "";
  return visualEditor.innerText.replace(/\r\n/g, "\n");
}

function setVisualPlainText(text) {
  if (!visualEditor) return;
  if (!text) {
    visualEditor.innerHTML = "";
    return;
  }
  const lines = text.split("\n");
  visualEditor.innerHTML = lines
    .map((line) => (line ? `<p>${escapeHtml(line)}</p>` : "<p><br></p>"))
    .join("");
}

function updateLangPillUI() {
  if (!langPill) return;
  if (editorMode === "preview") {
    langPill.textContent = "Preview · Click to Edit";
    langPill.classList.add("preview-active");
    return;
  }
  langPill.classList.remove("preview-active");

  if (editorMode === "visual" && hasRichHtmlElements(visualEditor)) {
    langPill.textContent =
      currentLangSetting === "auto" ? "Auto: Document" : "Document";
    return;
  }

  const activeLang =
    currentLangSetting !== "auto" ? currentLangSetting : detectedLang;
  const label = LANG_NAMES[activeLang] || activeLang.toUpperCase();

  if (currentLangSetting === "auto") {
    if (activeLang === "txt" && editorMode === "visual") {
      langPill.textContent = "Auto: Document";
    } else {
      langPill.textContent = "Auto: " + label;
    }
  } else {
    langPill.textContent = label;
  }
}

function switchToCodeMode(lang, preserveText) {
  const text =
    preserveText !== undefined
      ? preserveText
      : editorMode === "code"
      ? codeTextarea.value
      : extractPlainTextFromVisual();

  editorMode = "code";
  previousEditMode = "code";
  if (lang) detectedLang = lang;

  visualEditor.classList.add("hidden");
  markdownPreview.classList.add("hidden");
  codeEditorWrap.classList.remove("hidden");

  if (codeTextarea.value !== text) {
    codeTextarea.value = text;
  }
  updateLineNumbers();
  updateCodeHighlightLayer();
  updateLangPillUI();
}

function switchToVisualMode(wrapExistingCode) {
  if (editorMode === "code" && wrapExistingCode) {
    const existingCode = codeTextarea.value.trim();
    if (existingCode) {
      visualEditor.innerHTML = `<pre class="code-block"><code>${escapeHtml(existingCode)}</code></pre><p><br></p>`;
    } else {
      visualEditor.innerHTML = "";
    }
  } else if (editorMode === "code" && !wrapExistingCode) {
    setVisualPlainText(codeTextarea.value);
  }

  editorMode = "visual";
  previousEditMode = "visual";

  codeEditorWrap.classList.add("hidden");
  markdownPreview.classList.add("hidden");
  visualEditor.classList.remove("hidden");
  updateLangPillUI();
}

function togglePreviewMode() {
  if (editorMode === "preview") {
    markdownPreview.classList.add("hidden");
    if (previousEditMode === "code") {
      codeEditorWrap.classList.remove("hidden");
      editorMode = "code";
      updateLineNumbers();
    } else {
      visualEditor.classList.remove("hidden");
      editorMode = "visual";
    }
    updateLangPillUI();
    return;
  }

  previousEditMode = editorMode;
  editorMode = "preview";
  visualEditor.classList.add("hidden");
  codeEditorWrap.classList.add("hidden");
  markdownPreview.classList.remove("hidden");

  if (previousEditMode === "visual" && hasRichHtmlElements(visualEditor)) {
    const clone = visualEditor.cloneNode(true);
    clone.querySelectorAll(".inline-remove").forEach((b) => b.remove());
    markdownPreview.innerHTML = clone.innerHTML;
  } else {
    const rawText =
      previousEditMode === "code"
        ? codeTextarea.value
        : extractPlainTextFromVisual();
    markdownPreview.innerHTML =
      lastServerRenderedMd || renderMarkdownClient(rawText);
  }
  updateLangPillUI();
}

function renderMarkdownClient(md) {
  if (!md) return "<p><em>Empty pad</em></p>";
  let html = escapeHtml(md);

  // Fenced code blocks
  html = html.replace(/```([\w-]*)\n([\s\S]*?)```/g, (_, lang, code) => {
    return `<pre class="code-block"><code>${code}</code></pre>`;
  });

  // Images & links
  html = html.replace(
    /!\[([^\]]*)\]\(([^)]+)\)/g,
    '<img src="$2" alt="$1">'
  );
  html = html.replace(
    /\[([^\]]+)\]\(([^)]+)\)/g,
    '<a href="$2" target="_blank" rel="noopener">$1</a>'
  );

  // Headings, blockquotes, lists, paragraphs
  const blocks = html.split(/\n{2,}/);
  return blocks
    .map((block) => {
      if (block.startsWith("<pre ")) return block;
      if (/^#{1,6}\s/.test(block)) {
        const m = block.match(/^(#{1,6})\s+(.*)$/);
        if (m) {
          const level = m[1].length;
          return `<h${level}>${formatInlineMd(m[2])}</h${level}>`;
        }
      }
      if (/^>\s/.test(block)) {
        const inner = block.replace(/^>\s?/gm, "");
        return `<blockquote><p>${formatInlineMd(inner)}</p></blockquote>`;
      }
      if (/^(?:[-*+]\s+.+\n?)+$/.test(block)) {
        const items = block
          .split("\n")
          .filter(Boolean)
          .map((l) => `<li>${formatInlineMd(l.replace(/^[-*+]\s+/, ""))}</li>`)
          .join("");
        return `<ul>${items}</ul>`;
      }
      return `<p>${formatInlineMd(block).replace(/\n/g, "<br>")}</p>`;
    })
    .join("\n");
}

function formatInlineMd(text) {
  return text
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/\*([^*]+)\*/g, "<em>$1</em>")
    .replace(/`([^`]+)`/g, "<code>$1</code>");
}

// ============================================================================
// Serialization & Smart Mode Evaluation
// ============================================================================

function getSerializedPadContent() {
  if (editorMode === "code" || (editorMode === "preview" && previousEditMode === "code")) {
    return codeTextarea.value;
  }
  if (hasRichHtmlElements(visualEditor)) {
    return visualEditor.innerHTML.trim();
  }
  return extractPlainTextFromVisual();
}

function isEditorEmpty() {
  if (editorMode === "code") {
    return codeTextarea.value.length === 0;
  }
  if (hasRichHtmlElements(visualEditor)) {
    return false;
  }
  return extractPlainTextFromVisual().trim().length === 0;
}

function evaluateSmartModeOnInput() {
  if (editorMode === "visual") {
    if (hasRichHtmlElements(visualEditor)) {
      detectedLang = "rich";
      updateLangPillUI();
      return;
    }

    const plain = extractPlainTextFromVisual();
    if (currentLangSetting !== "auto" && currentLangSetting !== "rich") {
      if (CODE_LANGS.has(currentLangSetting) || currentLangSetting === "md") {
        switchToCodeMode(currentLangSetting, plain);
        codeTextarea.focus();
        codeTextarea.setSelectionRange(plain.length, plain.length);
        return;
      }
    }

    const guessed = detectLanguageClient(plain);
    detectedLang = guessed;
    if (currentLangSetting === "auto" && CODE_LANGS.has(guessed)) {
      switchToCodeMode(guessed, plain);
      codeTextarea.focus();
      codeTextarea.setSelectionRange(plain.length, plain.length);
      return;
    }
    updateLangPillUI();
  } else if (editorMode === "code") {
    const code = codeTextarea.value;
    if (currentLangSetting === "auto") {
      detectedLang = detectLanguageClient(code);
    } else {
      detectedLang = currentLangSetting;
    }
    updateLineNumbers();
    updateCodeHighlightLayer();
    updateLangPillUI();
  }
}

// ============================================================================
// HTML Sanitizer & Rich Paste / Inline Asset Upload
// ============================================================================

const ALLOWED_TAGS = new Set([
  "H1", "H2", "H3", "H4", "H5", "H6",
  "P", "DIV", "SPAN", "BR", "HR",
  "UL", "OL", "LI",
  "TABLE", "THEAD", "TBODY", "TFOOT", "TR", "TH", "TD",
  "STRONG", "B", "EM", "I", "U", "S", "DEL", "STRIKE", "SUB", "SUP",
  "CODE", "PRE", "BLOCKQUOTE",
  "A", "IMG", "FIGURE", "FIGCAPTION"
]);

const FORBIDDEN_TAGS = new Set([
  "SCRIPT", "STYLE", "IFRAME", "OBJECT", "EMBED", "META", "LINK", "HEAD", "TITLE", "XML", "SVG", "FORM"
]);

function sanitizePastedHtml(rawHtml) {
  const parser = new DOMParser();
  const doc = parser.parseFromString(rawHtml, "text/html");

  function cleanNode(node) {
    if (node.nodeType === Node.TEXT_NODE) {
      return document.createTextNode(node.textContent);
    }
    if (node.nodeType !== Node.ELEMENT_NODE) {
      return null;
    }

    const tag = node.tagName.toUpperCase();
    if (FORBIDDEN_TAGS.has(tag) || tag.includes(":")) {
      return null;
    }

    // Convert Word MsoListParagraph into <p> or <li>
    let targetTag = ALLOWED_TAGS.has(tag) ? tag.toLowerCase() : "span";
    if (tag === "B") targetTag = "strong";
    if (tag === "I") targetTag = "em";

    const el = document.createElement(targetTag);

    if (targetTag === "a") {
      const href = node.getAttribute("href") || "";
      if (href && !/^\s*javascript:/i.test(href)) {
        el.setAttribute("href", href);
        el.setAttribute("target", "_blank");
        el.setAttribute("rel", "noopener");
      }
    } else if (targetTag === "img") {
      const src = node.getAttribute("src") || "";
      if (src && (/^(?:https?:\/\/|\/|data:image\/)/i.test(src))) {
        el.setAttribute("src", src);
        el.setAttribute("alt", node.getAttribute("alt") || "Image");
      } else {
        return null;
      }
    } else if (targetTag === "td" || targetTag === "th") {
      const colspan = node.getAttribute("colspan");
      const rowspan = node.getAttribute("rowspan");
      if (colspan) el.setAttribute("colspan", colspan);
      if (rowspan) el.setAttribute("rowspan", rowspan);
    }

    for (const child of node.childNodes) {
      const cleanedChild = cleanNode(child);
      if (cleanedChild) {
        el.appendChild(cleanedChild);
      }
    }

    return el;
  }

  const container = document.createElement("div");
  for (const child of doc.body.childNodes) {
    const cleaned = cleanNode(child);
    if (cleaned) container.appendChild(cleaned);
  }
  return container;
}

function readFileAsDataUrl(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = (e) => resolve(e.target.result);
    reader.onerror = (err) => reject(err);
    reader.readAsDataURL(file);
  });
}

async function uploadAssetDataUrl(dataUrl, filename) {
  setSyncStatus("saving", "Uploading…");
  const resp = await fetch("/api/asset", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      data_url: dataUrl,
      filename: filename || "attachment",
      expires: currentExpiresSecs,
    }),
  });
  if (!resp.ok) {
    throw new Error("Asset upload failed: " + resp.status);
  }
  return await resp.json();
}

function insertHtmlAtVisualCursor(html) {
  if (editorMode !== "visual") {
    switchToVisualMode(true);
  }
  visualEditor.focus();

  const sel = window.getSelection();
  if (sel && sel.rangeCount > 0 && visualEditor.contains(sel.anchorNode)) {
    const range = sel.getRangeAt(0);
    range.deleteContents();
    const temp = document.createElement("div");
    temp.innerHTML = html;
    const frag = document.createDocumentFragment();
    let lastNode = null;
    while (temp.firstChild) {
      lastNode = frag.appendChild(temp.firstChild);
    }
    range.insertNode(frag);
    if (lastNode) {
      const newRange = document.createRange();
      newRange.setStartAfter(lastNode);
      newRange.collapse(true);
      sel.removeAllRanges();
      sel.addRange(newRange);
    }
  } else {
    visualEditor.insertAdjacentHTML("beforeend", html);
  }
}

function buildInlineImageMarkup(res) {
  const fn = escapeHtml(res.filename || "image.png");
  const sz = escapeHtml(res.size_formatted || "");
  return `<figure class="inline-asset inline-image" contenteditable="false"><img src="${escapeAttr(res.url)}" alt="${escapeAttr(res.filename || "image")}"><figcaption>${fn}${sz ? " (" + sz + ")" : ""} <a href="${escapeAttr(res.dl_url)}" download>Download</a> · <button type="button" class="inline-remove">Remove</button></figcaption></figure><p><br></p>`;
}

function buildInlinePdfMarkup(res) {
  const fn = escapeHtml(res.filename || "document.pdf");
  const sz = escapeHtml(res.size_formatted || "");
  return `<div class="inline-asset inline-pdf" contenteditable="false" data-url="${escapeAttr(res.url)}">
  <div class="inline-pdf-toolbar">
    <span>📄 ${fn}${sz ? " (" + sz + ")" : ""}</span>
    <div class="inline-pdf-actions">
      <a href="${escapeAttr(res.url)}" target="_blank" rel="noopener">Open in new tab</a>
      <a href="${escapeAttr(res.dl_url)}" download="${escapeAttr(res.filename || "document.pdf")}">Download PDF</a>
      <button type="button" class="inline-remove">Remove</button>
    </div>
  </div>
  <iframe class="inline-pdf-frame" src="${escapeAttr(res.url)}#toolbar=1" title="${escapeAttr(res.filename || "document.pdf")}"></iframe>
</div><p><br></p>`;
}

function generateScreenshotName(mime) {
  const d = new Date();
  const pad = (n) => String(n).padStart(2, "0");
  const ext = (mime && mime.split("/")[1]) || "png";
  return `Screenshot ${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} at ${pad(d.getHours())}.${pad(d.getMinutes())}.${pad(d.getSeconds())}.${ext}`;
}

function isImageFile(file) {
  if (file.type && file.type.startsWith("image/")) return true;
  return /\.(png|jpe?g|gif|webp|svg|avif|bmp|ico|tiff?)$/i.test(file.name || "");
}

function isPdfFile(file) {
  if (file.type === "application/pdf") return true;
  return /\.pdf$/i.test(file.name || "");
}

async function handleMediaFileUpload(file) {
  try {
    const isImg = isImageFile(file);
    const filename =
      file.name || (isImg ? generateScreenshotName(file.type) : "document.pdf");
    const dataUrl = await readFileAsDataUrl(file);
    const res = await uploadAssetDataUrl(dataUrl, filename);
    const markup =
      res.is_pdf || isPdfFile(file)
        ? buildInlinePdfMarkup(res)
        : buildInlineImageMarkup(res);
    insertHtmlAtVisualCursor(markup);
    updateLangPillUI();
    await saveNow();
  } catch (err) {
    console.error(err);
    setSyncStatus("offline", "Upload error");
    showToast("Failed to upload file");
  }
}

async function handleFilesList(fileList) {
  const files = Array.from(fileList || []);
  for (const file of files) {
    if (isImageFile(file) || isPdfFile(file)) {
      await handleMediaFileUpload(file);
    } else {
      try {
        const text = await file.text();
        const name = file.name || "";
        const dot = name.lastIndexOf(".");
        const ext = dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
        if (ext && CODE_LANGS.has(ext)) {
          switchToCodeMode(ext, text);
        } else {
          if (editorMode === "code") {
            codeTextarea.value = text;
          } else {
            setVisualPlainText(text);
          }
          evaluateSmartModeOnInput();
        }
        await saveNow();
      } catch (err) {
        console.error(err);
      }
    }
  }
}

// Remove button delegation for inline images & PDFs
if (visualEditor) {
  visualEditor.addEventListener("click", (e) => {
    const removeBtn = e.target.closest(".inline-remove");
    if (removeBtn) {
      e.preventDefault();
      const asset = removeBtn.closest(".inline-asset");
      if (asset) {
        asset.remove();
        updateLangPillUI();
        saveNow();
      }
    }
  });
}

// Clipboard Paste Handler (Screenshots, Rich .docx/Word/Docs HTML, Plain Text/Code)
window.addEventListener("paste", async (e) => {
  const active = document.activeElement;
  if (active && active.tagName === "INPUT") {
    return;
  }

  const cb = e.clipboardData;
  if (!cb) return;

  const items = Array.from(cb.items || []);
  const htmlData = cb.getData("text/html");
  const textData = cb.getData("text/plain");
  const imageItem = items.find((item) => item.type && item.type.startsWith("image/"));

  // 1. Direct Screenshot / Image File on Clipboard (no text/html or text/plain)
  if (imageItem && !htmlData && !textData) {
    const file = imageItem.getAsFile();
    if (file) {
      e.preventDefault();
      await handleMediaFileUpload(file);
      return;
    }
  }

  // 2. Rich Paste from .docx / Word / Google Docs / Pages (when text/html has semantic formatting or images)
  const hasRichStructure =
    htmlData &&
    /<(?:h[1-6]|table|tr|td|th|ul|ol|li|blockquote|img|figure|strong|b|em|i|u|s|pre|code)\b/i.test(
      htmlData
    );

  if (hasRichStructure && (editorMode === "visual" || /<(?:table|img|h[1-6]|ul|ol)\b/i.test(htmlData))) {
    e.preventDefault();
    const cleanedContainer = sanitizePastedHtml(htmlData);

    // Upload any embedded data:image/... assets automatically
    const dataImgs = Array.from(
      cleanedContainer.querySelectorAll('img[src^="data:image/"]')
    );
    for (const img of dataImgs) {
      try {
        const res = await uploadAssetDataUrl(
          img.getAttribute("src"),
          img.getAttribute("alt") || "pasted-image.png"
        );
        img.setAttribute("src", res.url);
      } catch (_) {}
    }

    // If HTML had no valid <img> left but clipboard also had an image file (e.g. Word image copy)
    if (cleanedContainer.querySelectorAll("img").length === 0 && imageItem && !textData.trim()) {
      const file = imageItem.getAsFile();
      if (file) {
        await handleMediaFileUpload(file);
        return;
      }
    }

    insertHtmlAtVisualCursor(cleanedContainer.innerHTML);
    updateLangPillUI();
    scheduleSave(true);
    return;
  }

  // 3. Plain Text / Code Paste inside Visual Editor
  if (editorMode === "visual" && textData) {
    e.preventDefault();
    if (!hasRichHtmlElements(visualEditor)) {
      const existingText = extractPlainTextFromVisual();
      const combined = existingText ? existingText + textData : textData;
      const guessed =
        currentLangSetting !== "auto" && currentLangSetting !== "rich"
          ? currentLangSetting
          : detectLanguageClient(combined);

      if (CODE_LANGS.has(guessed)) {
        switchToCodeMode(guessed, combined);
        codeTextarea.focus();
        scheduleSave();
        return;
      }
    }

    // Insert plain text preserving line breaks in Visual Editor
    const htmlLines = textData
      .replace(/\r\n/g, "\n")
      .split("\n")
      .map((line) => (line ? escapeHtml(line) : "<br>"))
      .join("<br>");
    insertHtmlAtVisualCursor(htmlLines);
    evaluateSmartModeOnInput();
    scheduleSave();
  }
});

// Drag & Drop Support
let dragCounter = 0;
if (editorWrap) {
  editorWrap.addEventListener("dragenter", (e) => {
    e.preventDefault();
    dragCounter++;
    if (dropOverlay) dropOverlay.classList.add("active");
  });

  editorWrap.addEventListener("dragleave", (e) => {
    e.preventDefault();
    dragCounter--;
    if (dragCounter <= 0) {
      dragCounter = 0;
      if (dropOverlay) dropOverlay.classList.remove("active");
    }
  });

  editorWrap.addEventListener("dragover", (e) => {
    e.preventDefault();
  });

  editorWrap.addEventListener("drop", async (e) => {
    e.preventDefault();
    dragCounter = 0;
    if (dropOverlay) dropOverlay.classList.remove("active");
    if (e.dataTransfer && e.dataTransfer.files && e.dataTransfer.files.length > 0) {
      await handleFilesList(e.dataTransfer.files);
    }
  });
}

if (btnOpen && fileInput) {
  btnOpen.addEventListener("click", () => {
    fileInput.value = "";
    fileInput.click();
  });
  fileInput.addEventListener("change", async (e) => {
    if (e.target.files && e.target.files.length > 0) {
      await handleFilesList(e.target.files);
    }
  });
}

// ============================================================================
// Copy to Clipboard (Word / Google Docs Rich HTML + Clean Markdown)
// ============================================================================

function toAbsoluteUrl(url) {
  if (!url) return "";
  try {
    return new URL(url, window.location.origin).href;
  } catch (_) {
    return url;
  }
}

async function copyRichOrCode() {
  try {
    if (editorMode === "code" || (editorMode === "preview" && previousEditMode === "code")) {
      await navigator.clipboard.writeText(codeTextarea.value);
      showToast("Copied code to clipboard");
      return;
    }

    const clone = visualEditor.cloneNode(true);
    clone.querySelectorAll(".inline-remove").forEach((b) => b.remove());
    clone.querySelectorAll("img[src]").forEach((img) => {
      img.setAttribute("src", toAbsoluteUrl(img.getAttribute("src")));
    });
    clone.querySelectorAll("a[href]").forEach((a) => {
      a.setAttribute("href", toAbsoluteUrl(a.getAttribute("href")));
    });
    clone.querySelectorAll(".inline-pdf").forEach((pdfCard) => {
      const url = toAbsoluteUrl(pdfCard.dataset.url || "");
      const p = document.createElement("p");
      const link = document.createElement("a");
      link.href = url;
      link.textContent = "📄 PDF Document (" + url + ")";
      p.appendChild(link);
      pdfCard.replaceWith(p);
    });

    const htmlContent = clone.innerHTML;
    const plainContent = extractPlainTextFromVisual();

    if (navigator.clipboard && window.ClipboardItem) {
      const item = new ClipboardItem({
        "text/html": new Blob([htmlContent], { type: "text/html" }),
        "text/plain": new Blob([plainContent], { type: "text/plain" }),
      });
      await navigator.clipboard.write([item]);
      showToast("Copied (Word / Docs ready)");
    } else {
      await navigator.clipboard.writeText(plainContent);
      showToast("Copied to clipboard");
    }
  } catch (err) {
    console.error(err);
    showToast("Failed to copy");
  }
}

function domNodeToMarkdown(node) {
  if (node.nodeType === Node.TEXT_NODE) {
    return node.textContent;
  }
  if (node.nodeType !== Node.ELEMENT_NODE) {
    return "";
  }

  const el = node;
  if (el.classList.contains("inline-remove")) return "";

  if (el.classList.contains("inline-pdf")) {
    const url = toAbsoluteUrl(el.dataset.url || "");
    const titleSpan = el.querySelector(".inline-pdf-toolbar span");
    const label = titleSpan ? titleSpan.textContent.trim() : "PDF";
    return `\n\n[${label}](${url})\n\n`;
  }

  if (el.classList.contains("inline-image")) {
    const img = el.querySelector("img");
    if (img) {
      const url = toAbsoluteUrl(img.getAttribute("src") || "");
      const alt = img.getAttribute("alt") || "image";
      return `\n\n![${alt}](${url})\n\n`;
    }
  }

  const tag = el.tagName.toUpperCase();
  const childrenMd = () =>
    Array.from(el.childNodes).map(domNodeToMarkdown).join("");

  switch (tag) {
    case "H1":
      return `\n\n# ${childrenMd().trim()}\n\n`;
    case "H2":
      return `\n\n## ${childrenMd().trim()}\n\n`;
    case "H3":
      return `\n\n### ${childrenMd().trim()}\n\n`;
    case "H4":
      return `\n\n#### ${childrenMd().trim()}\n\n`;
    case "H5":
      return `\n\n##### ${childrenMd().trim()}\n\n`;
    case "H6":
      return `\n\n###### ${childrenMd().trim()}\n\n`;
    case "P":
    case "DIV":
      return `\n\n${childrenMd().trim()}\n\n`;
    case "BR":
      return "\n";
    case "HR":
      return "\n\n---\n\n";
    case "STRONG":
    case "B": {
      const inner = childrenMd();
      return inner.trim() ? `**${inner.trim()}**` : "";
    }
    case "EM":
    case "I": {
      const inner = childrenMd();
      return inner.trim() ? `*${inner.trim()}*` : "";
    }
    case "S":
    case "DEL":
    case "STRIKE": {
      const inner = childrenMd();
      return inner.trim() ? `~~${inner.trim()}~~` : "";
    }
    case "CODE":
      if (el.parentElement && el.parentElement.tagName.toUpperCase() === "PRE") {
        return el.textContent;
      }
      return `\`${el.textContent}\``;
    case "PRE":
      return `\n\n\`\`\`\n${el.textContent.trim()}\n\`\`\`\n\n`;
    case "BLOCKQUOTE": {
      const lines = childrenMd()
        .trim()
        .split("\n")
        .map((l) => `> ${l}`)
        .join("\n");
      return `\n\n${lines}\n\n`;
    }
    case "UL": {
      const items = Array.from(el.children)
        .filter((c) => c.tagName.toUpperCase() === "LI")
        .map((li) => `- ${domNodeToMarkdown(li).trim()}`)
        .join("\n");
      return `\n\n${items}\n\n`;
    }
    case "OL": {
      const items = Array.from(el.children)
        .filter((c) => c.tagName.toUpperCase() === "LI")
        .map((li, i) => `${i + 1}. ${domNodeToMarkdown(li).trim()}`)
        .join("\n");
      return `\n\n${items}\n\n`;
    }
    case "LI":
      return childrenMd();
    case "A": {
      const href = toAbsoluteUrl(el.getAttribute("href") || "");
      return `[${childrenMd().trim() || href}](${href})`;
    }
    case "IMG": {
      const src = toAbsoluteUrl(el.getAttribute("src") || "");
      const alt = el.getAttribute("alt") || "image";
      return `![${alt}](${src})`;
    }
    case "TABLE": {
      const rows = Array.from(el.querySelectorAll("tr"));
      if (rows.length === 0) return "";
      const matrix = rows.map((tr) =>
        Array.from(tr.querySelectorAll("th, td")).map((cell) =>
          domNodeToMarkdown(cell).replace(/\n+/g, " ").trim()
        )
      );
      const colCount = Math.max(...matrix.map((r) => r.length), 1);
      const fmtRow = (r) => {
        const padded = Array.from({ length: colCount }, (_, i) => r[i] || "");
        return `| ${padded.join(" | ")} |`;
      };
      const header = fmtRow(matrix[0]);
      const sep = `| ${Array.from({ length: colCount }, () => "---").join(" | ")} |`;
      const body = matrix.slice(1).map(fmtRow).join("\n");
      return `\n\n${header}\n${sep}${body ? "\n" + body : ""}\n\n`;
    }
    default:
      return childrenMd();
  }
}

async function copyAsMarkdown() {
  try {
    let md = "";
    if (editorMode === "code" || (editorMode === "preview" && previousEditMode === "code")) {
      const code = codeTextarea.value;
      const lang =
        currentLangSetting !== "auto" ? currentLangSetting : detectedLang;
      if (lang === "md" || lang === "markdown" || lang === "txt") {
        md = code;
      } else {
        md = "```" + lang + "\n" + code + "\n```";
      }
    } else if (!hasRichHtmlElements(visualEditor)) {
      md = extractPlainTextFromVisual();
    } else {
      md = domNodeToMarkdown(visualEditor)
        .replace(/\n{3,}/g, "\n\n")
        .trim();
    }
    await navigator.clipboard.writeText(md);
    showToast("Copied as Markdown");
  } catch (err) {
    console.error(err);
    showToast("Failed to copy Markdown");
  }
}

if (btnCopy) btnCopy.addEventListener("click", copyRichOrCode);
if (btnCopyMd) btnCopyMd.addEventListener("click", copyAsMarkdown);
if (langPill) langPill.addEventListener("click", togglePreviewMode);

// ============================================================================
// Code Textarea Indentation & Input Listeners
// ============================================================================

const INDENT = "  ";

function insertIndent() {
  if (!document.execCommand("insertText", false, INDENT)) {
    codeTextarea.setRangeText(
      INDENT,
      codeTextarea.selectionStart,
      codeTextarea.selectionEnd,
      "end"
    );
  }
}

function removeIndent() {
  const lineStart =
    codeTextarea.value.lastIndexOf("\n", codeTextarea.selectionStart - 1) + 1;
  const indent = codeTextarea.value
    .slice(lineStart, lineStart + INDENT.length)
    .match(/^ +/);
  if (!indent) return;

  const width = indent[0].length;
  const start = codeTextarea.selectionStart;
  const end = codeTextarea.selectionEnd;

  codeTextarea.setSelectionRange(lineStart, lineStart + width);
  if (!document.execCommand("delete")) {
    codeTextarea.setRangeText("", lineStart, lineStart + width, "end");
  }
  codeTextarea.setSelectionRange(
    Math.max(lineStart, start - width),
    Math.max(lineStart, end - width)
  );
}

if (codeTextarea) {
  codeTextarea.addEventListener("scroll", syncCodeScroll);

  codeTextarea.addEventListener("input", () => {
    if (isApplyingRemote) return;
    editRevision++;
    lastServerRenderedMd = "";
    evaluateSmartModeOnInput();
    scheduleSave();
  });

  codeTextarea.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
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
      editRevision++;
      evaluateSmartModeOnInput();
      scheduleSave();
    }
  });
}

if (visualEditor) {
  visualEditor.addEventListener("input", () => {
    if (isApplyingRemote) return;
    editRevision++;
    lastServerRenderedMd = "";
    evaluateSmartModeOnInput();
    scheduleSave();
  });

  visualEditor.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "s") {
      e.preventDefault();
      saveNow();
    }
  });
}

// ============================================================================
// Real-Time Auto-Save, Recent Switcher & SSE Live Sync
// ============================================================================

function updatePadActionLinks() {
  if (btnDownload) {
    btnDownload.href = currentPadId ? "/dl/" + currentPadId : "#";
  }
  if (btnRaw) {
    btnRaw.href = currentPadId ? "/raw/" + currentPadId : "#";
  }
}

function updateRecentDropdown(recentList) {
  if (!recentSelect || !Array.isArray(recentList)) return;

  const opts = [];
  if (!currentPadId) {
    opts.push({ value: "", label: "✦ New Blank Pad" });
  }

  for (const item of recentList) {
    if (!item || !item.id) continue;
    const preview = (item.preview || "Untitled pad")
      .replace(/<[^>]+>/g, " ")
      .replace(/\s+/g, " ")
      .trim()
      .slice(0, 38);
    const extBadge = item.extension ? ` · .${item.extension}` : "";
    opts.push({
      value: item.id,
      label: `${preview || item.id}${extBadge} (${item.id})`,
    });
  }

  if (opts.length === 0) {
    opts.push({ value: "", label: "No active pads" });
  }

  recentSelect.innerHTML = opts
    .map(
      (o) =>
        `<option value="${escapeAttr(o.value)}"${
          o.value === (currentPadId || "") ? " selected" : ""
        }>${escapeHtml(o.label)}</option>`
    )
    .join("");
}

function scheduleSave(immediate) {
  clearTimeout(saveTimer);
  if (immediate) {
    saveNow();
    return;
  }
  setSyncStatus("saving", "Saving…");
  saveTimer = setTimeout(() => {
    saveNow();
  }, 200);
}

async function saveNow() {
  clearTimeout(saveTimer);
  if (isApplyingRemote) return;

  if (currentPadId === null && isEditorEmpty()) {
    setSyncStatus("ready", "Ready");
    return;
  }

  if (isSaving) {
    pendingSaveAfterCurrent = true;
    return;
  }

  const content = getSerializedPadContent();
  const extToSend =
    currentLangSetting !== "auto"
      ? currentLangSetting
      : hasRichHtmlElements(visualEditor)
      ? "html"
      : detectedLang;

  const signature = `${currentPadId || "new"}:${extToSend}:${currentExpiresSecs}:${content}`;
  if (currentPadId !== null && signature === lastSavedSignature) {
    setSyncStatus("ready", "Synced");
    return;
  }

  const revAtStart = editRevision;
  isSaving = true;
  setSyncStatus("saving", "Saving…");

  try {
    const isCreate = currentPadId === null;
    const url = isCreate ? "/api/pad" : `/api/pad/${encodeURIComponent(currentPadId)}`;
    const method = isCreate ? "POST" : "PUT";

    const resp = await fetch(url, {
      method,
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        content,
        extension: extToSend,
        expires: currentExpiresSecs,
        client_id: clientId,
      }),
    });

    if (!resp.ok) {
      throw new Error("Save failed: " + resp.status);
    }

    const res = await resp.json();
    if (res.id) {
      const wasNull = currentPadId === null;
      currentPadId = res.id;
      updatePadActionLinks();
      if (wasNull && window.location.pathname === "/new") {
        history.replaceState(null, "", "/");
      }
    }

    lastSavedSignature = `${currentPadId}:${extToSend}:${currentExpiresSecs}:${content}`;

    const returnedExt = (res.extension || res.detected_lang || "").toLowerCase();
    if (returnedExt && currentLangSetting === "auto" && !hasRichHtmlElements(visualEditor)) {
      detectedLang = returnedExt;
    }
    if (res.rendered_md) {
      lastServerRenderedMd = res.rendered_md;
    }
    if (res.highlighted_html && editorMode === "code" && editRevision === revAtStart) {
      updateCodeHighlightLayer(res.highlighted_html);
    }
    if (res.recent) {
      updateRecentDropdown(res.recent);
    }

    updateLangPillUI();
    setSyncStatus("ready", "Synced");
  } catch (err) {
    console.error(err);
    setSyncStatus("offline", "Offline");
  } finally {
    isSaving = false;
    if (pendingSaveAfterCurrent) {
      pendingSaveAfterCurrent = false;
      saveNow();
    }
  }
}

function applyPadDataToEditor(data, preserveScroll) {
  if (!data) return;
  isApplyingRemote = true;
  try {
    const prevVisualScroll = visualEditor ? visualEditor.scrollTop : 0;
    const prevCodeScroll = codeTextarea ? codeTextarea.scrollTop : 0;

    currentPadId = data.id || null;
    updatePadActionLinks();

    const content = data.content || "";
    lastServerRenderedMd = data.rendered_md || "";

    if (data.is_image && data.raw_url) {
      visualEditor.innerHTML = buildInlineImageMarkup({
        url: data.raw_url,
        dl_url: "/dl/" + data.id,
        filename: data.title || data.id,
        size_formatted: data.size_formatted || "",
      });
      switchToVisualMode(false);
    } else if (data.is_pdf && data.raw_url) {
      visualEditor.innerHTML = buildInlinePdfMarkup({
        url: data.raw_url,
        dl_url: "/dl/" + data.id,
        filename: data.title || data.id + ".pdf",
        size_formatted: data.size_formatted || "",
      });
      switchToVisualMode(false);
    } else if (
      /<(?:figure|div\s+class="inline-asset|table|img\s|h[1-6]\b|ul\b|ol\b|blockquote\b|p>)/i.test(
        content
      ) &&
      !/^<!DOCTYPE\s+html/i.test(content.trim()) &&
      !/^<html[\s>]/i.test(content.trim())
    ) {
      visualEditor.innerHTML = content;
      detectedLang = "rich";
      switchToVisualMode(false);
    } else {
      const srvLang = (data.extension || data.detected_lang || "").toLowerCase();
      const guessed =
        srvLang && srvLang !== "txt" && srvLang !== "auto" ? srvLang : detectLanguageClient(content);
      detectedLang = guessed;

      if (CODE_LANGS.has(guessed)) {
        switchToCodeMode(guessed, content);
        if (data.highlighted_html) {
          updateCodeHighlightLayer(data.highlighted_html);
        }
      } else {
        setVisualPlainText(content);
        switchToVisualMode(false);
      }
    }

    if (preserveScroll) {
      if (editorMode === "code" && codeTextarea) {
        codeTextarea.scrollTop = prevCodeScroll;
        syncCodeScroll();
      } else if (visualEditor) {
        visualEditor.scrollTop = prevVisualScroll;
      }
    }

    if (data.recent) {
      updateRecentDropdown(data.recent);
    }

    updateLangPillUI();
  } finally {
    isApplyingRemote = false;
  }
}

function resetToBlankPad(pushUrl) {
  clearTimeout(saveTimer);
  isApplyingRemote = true;
  try {
    currentPadId = null;
    currentLangSetting = "auto";
    detectedLang = "txt";
    lastSavedSignature = "";
    lastServerRenderedMd = "";
    if (settingLang) settingLang.value = "auto";

    if (visualEditor) visualEditor.innerHTML = "";
    if (codeTextarea) codeTextarea.value = "";
    if (codeHighlightLayer) codeHighlightLayer.innerHTML = "";
    if (markdownPreview) markdownPreview.innerHTML = "";

    updatePadActionLinks();
    switchToVisualMode(false);
    setSyncStatus("ready", "Ready");

    if (pushUrl) {
      history.pushState(null, "", pushUrl);
    }
    if (visualEditor) visualEditor.focus();
  } finally {
    isApplyingRemote = false;
  }
}

async function fetchAndLoadPad(id, pushStateUrl) {
  try {
    setSyncStatus("saving", "Loading…");
    const resp = await fetch(`/api/pad/${encodeURIComponent(id)}`);
    if (!resp.ok) {
      resetToBlankPad("/new");
      return;
    }
    const data = await resp.json();
    applyPadDataToEditor(data, false);
    if (pushStateUrl) {
      history.pushState(null, "", pushStateUrl);
    }
    setSyncStatus("ready", "Synced");
  } catch (err) {
    console.error(err);
    setSyncStatus("offline", "Offline");
  }
}

async function fetchLatestPad(onlyPopulateRecent) {
  try {
    const resp = await fetch("/api/pad/latest");
    if (!resp.ok) return;
    const data = await resp.json();
    if (data.recent) {
      updateRecentDropdown(data.recent);
    }
    if (!onlyPopulateRecent && data.id) {
      applyPadDataToEditor(data, false);
      setSyncStatus("ready", "Synced");
    } else if (!onlyPopulateRecent) {
      resetToBlankPad(null);
    }
  } catch (err) {
    console.error(err);
  }
}

// Header & Settings Controls
if (btnNew) {
  btnNew.addEventListener("click", () => {
    resetToBlankPad("/new");
  });
}

if (btnHome) {
  btnHome.addEventListener("click", async (e) => {
    e.preventDefault();
    history.pushState(null, "", "/");
    await fetchLatestPad(false);
  });
}

if (recentSelect) {
  recentSelect.addEventListener("change", async () => {
    const targetId = recentSelect.value;
    if (!targetId) {
      resetToBlankPad("/new");
      return;
    }
    await fetchAndLoadPad(targetId, "/?pad=" + encodeURIComponent(targetId));
  });
}

if (btnSettings && settingsPopover) {
  btnSettings.addEventListener("click", (e) => {
    e.stopPropagation();
    const isHidden = settingsPopover.classList.toggle("hidden");
    btnSettings.setAttribute("aria-expanded", String(!isHidden));
  });

  document.addEventListener("click", (e) => {
    if (
      !settingsPopover.classList.contains("hidden") &&
      !settingsPopover.contains(e.target) &&
      e.target !== btnSettings
    ) {
      settingsPopover.classList.add("hidden");
      btnSettings.setAttribute("aria-expanded", "false");
    }
  });

  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !settingsPopover.classList.contains("hidden")) {
      settingsPopover.classList.add("hidden");
      btnSettings.setAttribute("aria-expanded", "false");
    }
  });
}

if (settingLang) {
  settingLang.addEventListener("change", () => {
    currentLangSetting = settingLang.value;
    if (currentLangSetting === "rich") {
      switchToVisualMode(false);
    } else if (currentLangSetting !== "auto" && !hasRichHtmlElements(visualEditor)) {
      if (CODE_LANGS.has(currentLangSetting) || currentLangSetting === "md") {
        switchToCodeMode(currentLangSetting);
      } else {
        switchToVisualMode(false);
      }
    } else {
      evaluateSmartModeOnInput();
    }
    updateLangPillUI();
    if (currentPadId || !isEditorEmpty()) {
      saveNow();
    }
  });
}

if (btnDownload) {
  btnDownload.addEventListener("click", (e) => {
    if (!currentPadId) {
      e.preventDefault();
      showToast("Nothing to download yet");
    }
  });
}

if (btnRaw) {
  btnRaw.addEventListener("click", (e) => {
    if (!currentPadId) {
      e.preventDefault();
      showToast("Nothing to open yet");
    }
  });
}

if (btnDelete) {
  btnDelete.addEventListener("click", async () => {
    if (!currentPadId) {
      resetToBlankPad("/new");
      if (settingsPopover) settingsPopover.classList.add("hidden");
      return;
    }
    try {
      const idToDelete = currentPadId;
      const resp = await fetch(`/api/pad/${encodeURIComponent(idToDelete)}`, {
        method: "DELETE",
      });
      if (resp.ok) {
        showToast("Pad deleted");
        if (settingsPopover) settingsPopover.classList.add("hidden");
        currentPadId = null;
        history.pushState(null, "", "/");
        await fetchLatestPad(false);
      } else {
        showToast("Could not delete pad");
      }
    } catch (err) {
      console.error(err);
      showToast("Error deleting pad");
    }
  });
}

// ============================================================================
// Live SSE Listener (GET /api/pad/events)
// ============================================================================

function initLiveSSE() {
  if (!window.EventSource) return;
  try {
    const es = new EventSource("/api/pad/events");

    es.addEventListener("pad_update", (e) => {
      try {
        const msg = JSON.parse(e.data);
        if (!msg || msg.client_id === clientId) {
          return;
        }
        if (msg.recent) {
          updateRecentDropdown(msg.recent);
        }
        if (
          msg.type === "update" &&
          (msg.id === currentPadId ||
            (currentPadId === null && window.location.pathname === "/"))
        ) {
          applyPadDataToEditor(msg, true);
          setSyncStatus("live", "Live synced");
        } else if ((msg.type === "delete" || msg.type === "deleted") && msg.id === currentPadId) {
          resetToBlankPad("/new");
          showToast("Current pad was deleted remotely");
        }
      } catch (err) {
        console.error(err);
      }
    });

    es.onerror = () => {
      // EventSource automatically reconnects
    };
  } catch (err) {
    console.error(err);
  }
}

// ============================================================================
// Initial Load & Popstate
// ============================================================================

async function routeCurrentLocation() {
  const pathname = window.location.pathname;
  const padParam = new URLSearchParams(window.location.search).get("pad");
  if (padParam) {
    await fetchAndLoadPad(padParam, null);
  } else if (pathname === "/new") {
    resetToBlankPad(null);
    await fetchLatestPad(true);
  } else if (pathname === "/") {
    await fetchLatestPad(false);
  } else {
    const rawSeg = pathname.slice(1);
    const id = rawSeg.split(".")[0];
    if (id) {
      await fetchAndLoadPad(id, null);
    } else {
      await fetchLatestPad(false);
    }
  }
}

async function initWorkspace() {
  initExpirationUI();
  updatePadActionLinks();
  updateLangPillUI();
  initLiveSSE();
  await routeCurrentLocation();
}

window.addEventListener("popstate", () => {
  routeCurrentLocation();
});

initWorkspace();
