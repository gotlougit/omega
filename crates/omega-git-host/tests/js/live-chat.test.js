// Node tests for the LiveChat state machine in assets/live.js using a tiny
// DOM/EventSource emulation (no browser required).
// Run with:
//   nix shell nixpkgs#nodejs_24 -c node crates/omega-git-host/tests/js/live-chat.test.js
'use strict';

const assert = require('assert');
let passed = 0;
function show(v) {
  try {
    return JSON.stringify(v);
  } catch (e) {
    return String(v);
  }
}
function eq(actual, expected, label) {
  try {
    assert.strictEqual(actual, expected);
    passed++;
  } catch (e) {
    console.error(`✗ ${label}\n  expected: ${show(expected)}\n  actual:   ${show(actual)}`);
    process.exitCode = 1;
  }
}
function ok(cond, label) {
  eq(!!cond, true, label);
}

// ---------------------------------------------------------------------------
// Minimal fake DOM
// ---------------------------------------------------------------------------
function FakeEl(tag) {
  this.tag = tag;
  this.children = [];
  this.parent = null;
  this.attrs = {};
  this.className = '';
  this.style = {};
  this._text = '';
  this.open = false;
  this.innerHTML = '';
}
Object.defineProperty(FakeEl.prototype, 'textContent', {
  // Browser semantics: on a text node it's the node value; on an element it
  // aggregates the descendants' text (and setting it replaces the children
  // with a single text node).
  get() {
    if (this.tag === '#text') return this._text;
    if (this._text !== '' || this.children.length === 0) return this._text;
    return this.children.map((c) => c.textContent).join('');
  },
  set(v) {
    this._text = String(v);
    if (this.tag !== '#text') this.children = [];
  },
});
FakeEl.prototype.appendChild = function (c) {
  if (c.parent) c.parent.removeChild(c);
  c.parent = this;
  this.children.push(c);
  return c;
};
FakeEl.prototype.removeChild = function (c) {
  const i = this.children.indexOf(c);
  if (i >= 0) this.children.splice(i, 1);
  c.parent = null;
};
FakeEl.prototype.remove = function () {
  if (this.parent) this.parent.removeChild(this);
};
FakeEl.prototype.setAttribute = function (k, v) { this.attrs[k] = v; };
FakeEl.prototype.getAttribute = function (k) { return this.attrs[k]; };
function hasClass(el, cls) {
  return el.className.split(/\s+/).includes(cls);
}
function matches(el, sel) {
  if (sel.startsWith('.')) return hasClass(el, sel.slice(1));
  if (sel.includes('.')) {
    const [tag, cls] = sel.split('.');
    return el.tag === tag && hasClass(el, cls);
  }
  return el.tag === sel;
}
function collect(root, sel, out) {
  for (const c of root.children) {
    if (matches(c, sel)) out.push(c);
    collect(c, sel, out);
  }
  return out;
}
FakeEl.prototype.querySelector = function (sel) {
  return collect(this, sel, [])[0] || null;
};
FakeEl.prototype.querySelectorAll = function (sel) {
  return collect(this, sel, []);
};

const document = {
  createElement(tag) { return new FakeEl(tag); },
  createTextNode(text) {
    const e = new FakeEl('#text');
    e.textContent = text;
    return e;
  },
  getElementById(id) { return document._byId[id] || null; },
  _byId: {},
};
global.document = document;

class FakeEventSource {
  constructor(url) {
    this.url = url;
    this.onmessage = null;
    this.onerror = null;
    this.onopen = null;
    FakeEventSource.last = this;
  }
  emit(data) {
    if (this.onmessage) this.onmessage({ data });
  }
  close() {}
}
global.EventSource = FakeEventSource;

// Load the asset now that `document`/`EventSource` exist and grab the
// controller class to drive directly (the browser bootstrap runs once at
// load, but node caches modules, so tests construct LiveChat themselves).
const { LiveChat } = require('../../assets/live.js');

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function setup(staticMessages) {
  const container = new FakeEl('div');
  const status = new FakeEl('p');
  container.setAttribute('data-stream-url', '/sessions/x/stream');
  for (let i = 0; i < staticMessages; i++) {
    const m = new FakeEl('div');
    m.className = 'session-message assistant';
    container.appendChild(m);
  }
  const chat = new LiveChat(container, status);
  chat.connect();
  const source = FakeEventSource.last;
  ok(source instanceof FakeEventSource, 'EventSource connected');
  const all = () => container.querySelectorAll('.session-message');
  return {
    container,
    status,
    source,
    chat,
    // Only the messages the *live* client appended (past the static ones).
    live: () => all().slice(staticMessages),
  };
}

function bodyTextOf(msgEl) {
  return msgEl.querySelector('.message-body');
}

// ---------------------------------------------------------------------------
// Test 1: a full turn folds into ONE assistant message with thinking +
// tools, then collapses on done.
// ---------------------------------------------------------------------------
{
  const t = setup(2); // two server-rendered messages pre-exist
  t.source.emit(JSON.stringify({ type: 'cleared' })); // reconnect reset

  t.source.emit(JSON.stringify({ type: 'thinking', text: 'hmm, ' }));
  t.source.emit(JSON.stringify({ type: 'thinking', text: 'let me check' }));
  t.source.emit(JSON.stringify({ type: 'thinking_complete', text: 'hmm, let me check' }));
  t.source.emit(JSON.stringify({ type: 'text', text: 'let me ' }));
  t.source.emit(JSON.stringify({ type: 'text', text: 'look' }));
  t.source.emit(JSON.stringify({ type: 'text_complete', text: 'let me look' }));
  t.source.emit(
    JSON.stringify({ type: 'tool_start', id: 'c1', name: 'Bash', input: { command: 'ls' } })
  );
  t.source.emit(JSON.stringify({ type: 'tool_progress', id: 'c1', output: 'reading' }));
  t.source.emit(
    JSON.stringify({
      type: 'tool_end',
      id: 'c1',
      name: 'Bash',
      input: { command: 'ls' },
      result: 'file1\nfile2',
      is_error: false,
      preview: 'file1 file2',
    })
  );
  t.source.emit(JSON.stringify({ type: 'done' }));

  eq(t.live().length, 1, 'one live assistant message after the turn');
  const m = t.live()[0];
  ok(hasClass(m, 'assistant'), 'message is an assistant message');
  const th = m.querySelector('details.thinking');
  ok(th, 'thinking details present');
  ok(th.textContent.includes('hmm, let me check'), 'thinking trace content');
  ok(!th.open, 'thinking collapsed after done');
  const tools = m.querySelector('details.tools');
  ok(tools, 'tools wrapper present');
  const tool = m.querySelector('details.tool');
  ok(tool, 'tool details present');
  ok(!tool.open, 'tool collapsed after done');
  const preview = tool.querySelector('.tool-preview');
  ok(preview.textContent === 'file1 file2', 'tool preview matches server preview');
  const result = tool.querySelector('.tool-result');
  ok(result.textContent === 'file1\nfile2', 'tool result content');
  // body is markdown-rendered at completion
  const body = bodyTextOf(m);
  ok(body.innerHTML.includes('<p>let me look</p>'), 'body markdown rendered: ' + body.innerHTML);
  eq(t.status.textContent, '✓ done', 'status shows done');
}

// ---------------------------------------------------------------------------
// Test 2: `cleared` removes only live-appended messages.
// ---------------------------------------------------------------------------
{
  const t = setup(2);
  t.source.emit(JSON.stringify({ type: 'cleared' }));
  t.source.emit(JSON.stringify({ type: 'text', text: 'hello' }));
  t.source.emit(JSON.stringify({ type: 'done' }));
  eq(t.live().length, 1, '2 static + 1 live before reconnect');
  t.source.emit(JSON.stringify({ type: 'cleared' }));
  eq(t.live().length, 0, 'cleared drops live messages, keeps static');
  eq(t.status.textContent, '', 'status cleared on cleared');
}

// ---------------------------------------------------------------------------
// Test 3: resuming mid-turn — ToolEnd without ToolStart still renders, and a
// second turn after done starts a fresh message.
// ---------------------------------------------------------------------------
{
  const t = setup(0);
  t.source.emit(JSON.stringify({ type: 'cleared' }));
  t.source.emit(
    JSON.stringify({
      type: 'tool_end',
      id: 'late',
      name: 'Bash',
      input: { command: 'pwd' },
      result: '/tmp',
      is_error: false,
      preview: '/tmp',
    })
  );
  eq(t.live().length, 1, 'tool_end reconstructs a message');
  eq(t.live()[0].querySelector('details.tool') !== null, true, 'reconstructed tool call');
  t.source.emit(JSON.stringify({ type: 'done' }));

  // New turn: fresh assistant message.
  t.source.emit(JSON.stringify({ type: 'text', text: 'second reply' }));
  t.source.emit(JSON.stringify({ type: 'done' }));
  eq(t.live().length, 2, 'two distinct assistant turns');
  eq(bodyTextOf(t.live()[1]).innerHTML.includes('second reply'), true, 'second turn body');
}

// ---------------------------------------------------------------------------
// Test 4: an errored tool shows the error badge + preview.
// ---------------------------------------------------------------------------
{
  const t = setup(0);
  t.source.emit(JSON.stringify({ type: 'text', text: 'oops' }));
  t.source.emit(
    JSON.stringify({
      type: 'tool_start',
      id: 'bad',
      name: 'Bash',
      input: { command: 'false' },
    })
  );
  t.source.emit(
    JSON.stringify({
      type: 'tool_end',
      id: 'bad',
      name: 'Bash',
      input: { command: 'false' },
      result: '',
      is_error: true,
      preview: 'error',
    })
  );
  t.source.emit(JSON.stringify({ type: 'done' }));
  const tool = t.live()[0].querySelector('details.tool');
  ok(hasClass(tool, 'error'), 'tool marked error');
  const badge = tool.querySelector('.badge-danger');
  ok(badge && badge.textContent === 'error', 'error badge shown');
  ok(tool.querySelector('.tool-result').textContent === '(no output)', 'empty error result');
}

console.log(`\n${process.exitCode ? 'FAILED' : 'passed'}: ${passed} checks`);