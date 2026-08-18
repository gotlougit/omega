// omega-git-host — live chat client.
//
// Turns the daemon's SSE event stream into the *same* per-message layout the
// read-only session page renders server-side: one `.session-message` block
// per turn with the assistant's body, a collapsed `details.thinking` trace,
// and one collapsed `details.tool` per tool call (input + result). Nothing
// here is a separate "live pane" code path — it is the client-side twin of
// the `session.html` loop, sharing the same data shape (the `DisplayMessage`
// JSON the server also feeds the template via `messages_json`) and the same
// CSS classes.
//
// Security: mirroring `transcript::markdown_to_html`, the markdown renderer
// is escape-first. Raw text is HTML-escaped before it can reach the page and
// raw HTML in the stream is shown as literal text; link/image destinations
// are scheme-checked (only http/https/mailto survive). The only DOM writes
// are `textContent`, `createTextNode`, or `innerHTML` fed by `mdToHtml`.
//
// The pure helpers below are CommonJS-exported so `node` can run them
// against the test file at `tests/js/live-markdown.test.js`; in the browser
// this file loads as a plain classic script and `module` is undefined.

'use strict';

// ---------------------------------------------------------------------------
// Pure helpers (node-testable)
// ---------------------------------------------------------------------------

/** HTML-escape text the same way the server's `html_escape` does. */
function esc(s) {
  return String(s)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#x27;');
}

/**
 * Drop dangerous URL schemes (javascript:, data:, file:, ...); keep http(s),
 * mailto, and scheme-less (relative) URLs — matches `sanitize_url` in
 * `transcript.rs`.
 */
function sanitizeUrl(url) {
  const t = String(url).trim();
  const m = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(t);
  if (m) {
    const scheme = m[1].toLowerCase();
    if (scheme !== 'http' && scheme !== 'https' && scheme !== 'mailto') {
      return '';
    }
  }
  return t;
}

/**
 * Minimal, escape-first CommonMark renderer (the safe subset the server
 * renders: headings, paragraphs, fenced code, lists, blockquotes, hr, and
 * inline code/bold/italic/strikethrough/links/images). Raw HTML is never
 * passed through — it stays literal escaped text, exactly like the server.
 * Tables and footnotes render as plain text.
 */
function mdToHtml(src) {
  const lines = String(src).replace(/\r\n?/g, '\n').split('\n');
  const blocks = [];
  let i = 0;
  const L = lines.length;

  const takeParagraph = () => {
    const buf = [];
    while (i < L && lines[i].trim() !== '') {
      const l = lines[i];
      if (
        /^```/.test(l) ||
        /^(#{1,6})\s+/.test(l) ||
        /^>\s?/.test(l) ||
        /^([-*+])\s+/.test(l) ||
        /^(\d+)[.)]\s+/.test(l) ||
        /^(-{3,}|\*{3,}|_{3,})\s*$/.test(l)
      ) {
        break;
      }
      buf.push(l);
      i++;
    }
    blocks.push({ type: 'p', text: inline(buf.join('\n')) });
  };

  while (i < L) {
    const line = lines[i];

    if (line.trim() === '') {
      i++;
      continue;
    }

    // fenced code block
    if (/^```/.test(line)) {
      const lang = line.replace(/^```/, '').trim().replace(/[^A-Za-z0-9\-_+]/g, '');
      const buf = [];
      i++;
      while (i < L && !/^```/.test(lines[i])) {
        buf.push(lines[i]);
        i++;
      }
      i++; // skip closing fence (or EOF)
      blocks.push({ type: 'code', lang, text: buf.join('\n') });
      continue;
    }

    // heading
    const h = /^(#{1,6})\s+(.*)$/.exec(line);
    if (h) {
      blocks.push({ type: 'h' + h[1].length, text: inline(h[2]) });
      i++;
      continue;
    }

    // horizontal rule
    if (/^(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) {
      blocks.push({ type: 'hr' });
      i++;
      continue;
    }

    // blockquote (one <blockquote> per contiguous run)
    if (/^>\s?/.test(line)) {
      const buf = [];
      while (i < L && /^>\s?/.test(lines[i])) {
        buf.push(lines[i].replace(/^>\s?/, ''));
        i++;
      }
      blocks.push({ type: 'quote', text: inline(buf.join('\n')) });
      continue;
    }

    // lists (single level; indented lines continue the current item)
    const ordered = /^(\d+)[.)]\s+(.*)$/.test(line);
    const unordered = /^([-*+])\s+(.*)$/.test(line);
    if (ordered || unordered) {
      const itemOf = (l) => {
        if (ordered) {
          const m = /^(\d+)[.)]\s+(.*)$/.exec(l);
          return m ? m[2] : null;
        }
        const m = /^([-*+])\s+(.*)$/.exec(l);
        return m ? m[2] : null;
      };
      const items = [];
      let current = [];
      while (i < L) {
        const l = lines[i];
        if (l.trim() === '') break;
        const content = itemOf(l);
        if (content !== null && !/^\s/.test(l)) {
          if (current.length) items.push(current.join(' '));
          current = [content];
        } else if (/^[ \t]/.test(l) && current.length) {
          current.push(l.trim());
        } else {
          break;
        }
        i++;
      }
      if (current.length) items.push(current.join(' '));
      blocks.push({
        type: ordered ? 'ol' : 'ul',
        items: items.map((x) => inline(x)),
      });
      continue;
    }

    // default: paragraph
    takeParagraph();
  }

  // prettier-ignore
  let html = '';
  for (const b of blocks) {
    if (b.type === 'hr') { html += '<hr>\n'; continue; }
    if (b.type === 'p') { html += '<p>' + b.text + '</p>\n'; continue; }
    if (/^h[1-6]$/.test(b.type)) {
      html += '<h' + b.type.slice(1) + '>' + b.text + '</h' + b.type.slice(1) + '>\n';
      continue;
    }
    if (b.type === 'quote') { html += '<blockquote>\n' + b.text + '\n</blockquote>\n'; continue; }
    if (b.type === 'code') {
      html += b.lang
        ? '<pre><code class="language-' + b.lang + '">' + esc(b.text) + '</code></pre>\n'
        : '<pre><code>' + esc(b.text) + '</code></pre>\n';
      continue;
    }
    if (b.type === 'ul' || b.type === 'ol') {
      const tag = b.type === 'ul' ? 'ul' : 'ol';
      html += '<' + tag + '>\n' + b.items.map((x) => '<li>' + x + '</li>').join('\n') + '\n</' + tag + '>\n';
      continue;
    }
  }
  return html;
}

/**
 * Inline formatting over already-collected text. Escapes as it goes; links
 * and images get URL-sanitized destinations; soft breaks become `<br>`.
 * `raw` is the (possibly multi-line) block text.
 */
function inline(raw) {
  let out = '';
  let pos = 0;
  const re =
    /(`+)([^`]*?)\1|!\[([^\]]*)\]\(([^)]*)\)|\[([^\]]+)\]\(([^)]*)\)|\*\*([^*]+)\*\*|__([^_]+)__|\*([^*]+)\*|_([^_]+)_|~~([^~]+)~~/g;
  let m;
  while ((m = re.exec(raw)) !== null) {
    out += esc(raw.slice(pos, m.index)).replace(/\n/g, '<br>\n');
    if (m[1] !== undefined) {
      // code span
      out += '<code>' + esc(m[2]) + '</code>';
    } else if (m[3] !== undefined) {
      // image → a link to the (sanitized) URL, alt text as label
      const url = sanitizeUrl(m[4]);
      out += '<a href="' + esc(url) + '">' + esc(m[3] || '[image]') + '</a>';
    } else if (m[5] !== undefined) {
      // link
      const url = sanitizeUrl(m[6]);
      out += '<a href="' + esc(url) + '">' + inline(m[5]).replace(/\n/g, ' ') + '</a>';
    } else if (m[7] !== undefined || m[8] !== undefined) {
      out += '<strong>' + inline(m[7] !== undefined ? m[7] : m[8]) + '</strong>';
    } else if (m[9] !== undefined || m[10] !== undefined) {
      out += '<em>' + inline(m[9] !== undefined ? m[9] : m[10]) + '</em>';
    } else if (m[11] !== undefined) {
      out += '<del>' + inline(m[11]) + '</del>';
    }
    pos = m.index + m[0].length;
  }
  out += esc(raw.slice(pos)).replace(/\n/g, '<br>\n');
  return out;
}

/** Pretty-print a tool input payload (mirrors `transcript::pretty_json`). */
function prettyJson(v) {
  try {
    return JSON.stringify(v, null, 2);
  } catch (e) {
    return String(v);
  }
}

/**
 * Last non-empty line of a tool's raw output, truncated to `max` chars —
 * the web twin of the TUI's `tool_progress_line`.
 */
function lastLine(output, max) {
  const list = String(output).split('\n').filter((l) => l.trim() !== '');
  const last = list[list.length - 1];
  if (!last) return '';
  let s = last.trim();
  if (s.length > max) s = s.slice(0, max) + '…';
  return s;
}

// ---------------------------------------------------------------------------
// Live chat controller (browser only; safe to define even under node — the
// functions only touch `document` when actually called)
// ---------------------------------------------------------------------------

/** Create an element: `el('div', 'cls', 'text')`. */
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.appendChild(document.createTextNode(text));
  return e;
}

  /**
   * Renders the SSE stream into `container` (`.session-message` blocks) with
   * per-turn status in `statusEl`. The initial transcript is server-rendered
   * into `container` before this runs; only live-appended messages (beyond
   * `baseCount`) are managed here, so a reconnect never touches persisted
   * history.
   */
  function LiveChat(container, statusEl) {
    this.container = container;
    this.statusEl = statusEl;
    this.baseCount = container.querySelectorAll('.session-message').length;
    this.es = null;
    this.reconnectTimer = null;
    // The assistant message currently being streamed, or null.
    this.cur = null;
    // True from 'done' until the next chunk starts a fresh turn.
    this.turnEnded = true;
  }

  LiveChat.prototype.connect = function () {
    const url = this.container.getAttribute('data-stream-url');
    if (!url) return;
    const self = this;
    this.es = new EventSource(url);
    this.es.onmessage = function (ev) {
      self.handle(ev.data);
    };
    this.es.onerror = function () {
      self.statusEl.textContent = '⏻ reconnecting…';
      clearTimeout(self.reconnectTimer);
      self.reconnectTimer = setTimeout(function () {
        window.location.reload();
      }, 4000);
    };
    this.es.onopen = function () {
      clearTimeout(self.reconnectTimer);
      self.statusEl.textContent = '';
    };
  };

  LiveChat.prototype.handle = function (data) {
    let m;
    try {
      m = JSON.parse(data);
    } catch (e) {
      return;
    }
    switch (m.type) {
      case 'cleared':
        this.handleCleared();
        break;
      case 'text':
        this.handleText(m.text);
        break;
      case 'text_complete':
        this.handleTextComplete(m.text);
        break;
      case 'thinking':
        this.handleThinking(m.text);
        break;
      case 'thinking_complete':
        this.handleThinkingComplete(m.text);
        break;
      case 'tool_start':
        this.handleToolStart(m);
        break;
      case 'tool_progress':
        this.handleToolProgress(m);
        break;
      case 'tool_end':
        this.handleToolEnd(m);
        break;
      case 'status':
        this.statusEl.textContent = m.message || '';
        break;
      case 'error':
        this.statusEl.textContent = '⚠ ' + (m.message || '');
        break;
      case 'done':
        this.handleDone();
        break;
    }
  };

  /** Reconnect: drop live-appended messages, back to the persisted state. */
  LiveChat.prototype.handleCleared = function () {
    const nodes = this.container.querySelectorAll('.session-message');
    for (let i = nodes.length - 1; i >= this.baseCount; i--) {
      nodes[i].remove();
    }
    this.cur = null;
    this.turnEnded = true;
    this.statusEl.textContent = '';
  };

  /** Start (or resume) the assistant block for the current turn. */
  LiveChat.prototype.ensureTurn = function () {
    if (this.cur && !this.turnEnded) return this.cur;
    this.turnEnded = false;
    const m = {
      root: el('div', 'session-message assistant'),
      bodyEl: null,
      blocks: [], // finalized text blocks
      curBlock: '', // in-flight text block
      thinkingEl: null,
      thinkingPre: null,
      thinkingCount: null,
      thinkingBlocks: [],
      thinkingBuf: '',
      toolsEl: null,
      toolsSummaryCount: null,
      tools: [],
    };
    m.root.appendChild(el('div', 'message-role', 'assistant'));
    this.container.appendChild(m.root);
    this.cur = m;
    return m;
  };

  /** Keep sections in the canonical order: body, thinking, tools. */
  function layout(m) {
    const kids = [];
    if (m.bodyEl) kids.push(m.bodyEl);
    if (m.thinkingEl) kids.push(m.thinkingEl);
    if (m.toolsEl) kids.push(m.toolsEl);
    for (const k of kids) m.root.appendChild(k);
  }

  LiveChat.prototype.handleText = function (text) {
    if (!text) return;
    const m = this.ensureTurn();
    m.curBlock += text;
    if (!m.bodyEl) {
      m.bodyEl = el('div', 'message-body');
      m.bodyEl.style.whiteSpace = 'pre-wrap';
      layout(m);
    }
    m.bodyEl.appendChild(document.createTextNode(text));
  };

  LiveChat.prototype.handleTextComplete = function (text) {
    if (this.turnEnded) return; // late completion after 'done' — ignore
    const m = this.ensureTurn();
    // Prefer the authoritative full block; keep the accumulated deltas if the
    // completion is empty (provider quirk — the deltas already covered it).
    if (text && text.trim()) {
      m.blocks.push(text);
    } else if (m.curBlock.trim()) {
      m.blocks.push(m.curBlock);
    }
    m.curBlock = '';
  };

  LiveChat.prototype.handleThinking = function (text) {
    if (!text) return;
    const m = this.ensureTurn();
    m.thinkingBuf += text;
    this.syncThinking(m);
  };

  LiveChat.prototype.handleThinkingComplete = function (text) {
    if (this.turnEnded) return;
    const m = this.ensureTurn();
    if (text && text.trim()) m.thinkingBuf = text;
    if (m.thinkingBuf.trim()) m.thinkingBlocks.push(m.thinkingBuf);
    m.thinkingBuf = '';
    this.syncThinking(m);
  };

  LiveChat.prototype.syncThinking = function (m) {
    if (!m.thinkingEl) {
      m.thinkingEl = el('details', 'thinking');
      m.thinkingEl.open = true; // visible while the model is still reasoning
      const summary = el('summary');
      summary.appendChild(document.createTextNode('thinking '));
      m.thinkingCount = el('small', 'text-muted');
      summary.appendChild(m.thinkingCount);
      m.thinkingEl.appendChild(summary);
      m.thinkingPre = el('pre');
      m.thinkingEl.appendChild(m.thinkingPre);
      layout(m);
    }
    const parts = [];
    if (m.thinkingBlocks.length) parts.push(m.thinkingBlocks.join('\n\n'));
    if (m.thinkingBuf.trim()) parts.push(m.thinkingBuf);
    m.thinkingPre.textContent = parts.join('\n\n');
    const n = m.thinkingBlocks.length + (m.thinkingBuf.trim() ? 1 : 0);
    m.thinkingCount.textContent = n ? '(' + n + ')' : '';
  };

  LiveChat.prototype.handleToolStart = function (e) {
    const m = this.ensureTurn();
    if (m.tools.some((t) => t.id === e.id)) return;
    if (!m.toolsEl) {
      m.toolsEl = el('details', 'tools');
      const summary = el('summary');
      summary.appendChild(document.createTextNode('tools '));
      m.toolsSummaryCount = el('small', 'text-muted');
      summary.appendChild(m.toolsSummaryCount);
      m.toolsEl.appendChild(summary);
      layout(m);
    }
    const rec = {
      id: e.id,
      name: e.name,
      input: prettyJson(e.input),
      result: '',
      isError: false,
      preview: 'running…',
      running: true,
      details: null,
      progPre: null,
    };
    rec.details = this.buildTool(rec);
    m.tools.push(rec);
    m.toolsEl.appendChild(rec.details);
    this.syncTools(m);
  };

  /** DOM for a tool call: collapsed `<details.tool>` with input + pane. */
  LiveChat.prototype.buildTool = function (rec) {
    const d = el('details', 'tool');
    if (rec.running) d.open = true;
    const summary = el('summary');
    summary.appendChild(el('code', null, rec.name));
    if (rec.isError) summary.appendChild(el('span', 'badge badge-danger', 'error'));
    const preview = rec.preview || (rec.isError ? 'error' : '(no output)');
    summary.appendChild(el('span', 'text-muted tool-preview', preview));
    d.appendChild(summary);

    const pane = el('div', 'tool-pane');
    if (rec.input && rec.input.trim() && rec.input !== 'null') {
      pane.appendChild(el('div', 'tool-label', 'input'));
      const pre = el('pre', 'tool-input');
      pre.textContent = rec.input;
      pane.appendChild(pre);
    }
    if (rec.running) {
      pane.appendChild(el('div', 'tool-label', 'progress'));
      rec.progPre = el('pre', 'tool-progress');
      pane.appendChild(rec.progPre);
    } else {
      pane.appendChild(el('div', 'tool-label', rec.isError ? 'result (error)' : 'result'));
      const rp = el('pre', 'tool-result');
      rp.textContent = rec.result && rec.result.trim() ? rec.result : '(no output)';
      pane.appendChild(rp);
    }
    d.appendChild(pane);
    return d;
  };

  LiveChat.prototype.handleToolProgress = function (e) {
    const m = this.cur;
    if (!m) return;
    const rec = m.tools.find((t) => t.id === e.id);
    if (!rec || !rec.running) return;
    const line = lastLine(e.output, 90);
    if (!line) return;
    if (!rec.progPre) {
      const pane = rec.details.querySelector('.tool-pane');
      if (!pane) return;
      rec.progPre = el('pre', 'tool-progress');
      pane.appendChild(rec.progPre);
    }
    rec.progPre.textContent = line;
    rec.details.open = true;
  };

  LiveChat.prototype.handleToolEnd = function (e) {
    const m = this.ensureTurn();
    let rec = m.tools.find((t) => t.id === e.id);
    if (!rec) {
      // Joined mid-run and missed ToolStart — reconstruct from the end event
      // so the call still appears in the transcript.
      if (!m.toolsEl) {
        m.toolsEl = el('details', 'tools');
        const summary = el('summary');
        summary.appendChild(document.createTextNode('tools '));
        m.toolsSummaryCount = el('small', 'text-muted');
        summary.appendChild(m.toolsSummaryCount);
        m.toolsEl.appendChild(summary);
        layout(m);
      }
      rec = {
        id: e.id,
        name: e.name,
        input: prettyJson(e.input),
        result: '',
        isError: false,
        preview: '',
        running: false,
        details: null,
        progPre: null,
      };
      rec.details = this.buildTool(rec);
      m.tools.push(rec);
      m.toolsEl.appendChild(rec.details);
    }
    rec.result = e.result || '';
    rec.isError = !!e.is_error;
    rec.preview = e.preview || '';
    rec.running = false;
    // Same `details.tool error` marker the persisted transcript uses.
    rec.details.className = 'tool' + (rec.isError ? ' error' : '');

    // Rewrite the summary line exactly like the persisted transcript.
    const summary = rec.details.querySelector('summary');
    summary.textContent = '';
    summary.appendChild(el('code', null, rec.name));
    if (rec.isError) summary.appendChild(el('span', 'badge badge-danger', 'error'));
    summary.appendChild(
      el('span', 'text-muted tool-preview', rec.preview || (rec.isError ? 'error' : '(no output)'))
    );

    // Replace the live progress pane with the final input + result panes.
    const pane = rec.details.querySelector('.tool-pane');
    pane.textContent = '';
    if (rec.input && rec.input.trim() && rec.input !== 'null') {
      pane.appendChild(el('div', 'tool-label', 'input'));
      const pre = el('pre', 'tool-input');
      pre.textContent = rec.input;
      pane.appendChild(pre);
    }
    pane.appendChild(el('div', 'tool-label', rec.isError ? 'result (error)' : 'result'));
    const rp = el('pre', 'tool-result');
    rp.textContent = rec.result && rec.result.trim() ? rec.result : '(no output)';
    pane.appendChild(rp);

    rec.details.open = false;
    this.syncTools(m);
  };

  LiveChat.prototype.syncTools = function (m) {
    if (m.toolsSummaryCount) m.toolsSummaryCount.textContent = '(' + m.tools.length + ')';
  };

  /** Turn complete: render the body as markdown, fold the traces away. */
  LiveChat.prototype.handleDone = function () {
    const m = this.cur;
    if (m) {
      const full = m.blocks.concat(m.curBlock ? [m.curBlock] : []).join('\n\n');
      if (full.trim()) {
        if (!m.bodyEl) {
          m.bodyEl = el('div', 'message-body markdown');
          layout(m);
        }
        m.bodyEl.style.whiteSpace = '';
        m.bodyEl.className = 'message-body markdown';
        m.bodyEl.innerHTML = mdToHtml(full);
      }
      if (m.thinkingBuf.trim()) {
        m.thinkingBlocks.push(m.thinkingBuf);
        m.thinkingBuf = '';
        this.syncThinking(m);
      }
      // The turn is over — collapse the traces like the persisted view.
      if (m.thinkingEl) m.thinkingEl.open = false;
      if (m.toolsEl) m.toolsEl.open = false;
    }
    this.cur = null;
    this.turnEnded = true;
    this.statusEl.textContent = '✓ done';
  };

  // Browser bootstrap: wire the widget up when the live chat container is
// present. Pure helpers stay module-scope so node tests can import them.
if (typeof document !== 'undefined') {
  const container = document.getElementById('live-conversation');
  const statusEl = document.getElementById('live-status');
  if (container && statusEl) {
    new LiveChat(container, statusEl).connect();
  }
}

if (typeof module !== 'undefined' && module.exports) {
  module.exports = {
    esc,
    sanitizeUrl,
    mdToHtml,
    prettyJson,
    lastLine,
    LiveChat,
  };
}