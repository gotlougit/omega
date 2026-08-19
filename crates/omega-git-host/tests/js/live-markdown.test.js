// Node test harness for the pure helpers in assets/live.js (the client-side
// twin of the server-rendered session transcript). Run with:
//   nix shell nixpkgs#nodejs_24 -c node crates/omega-git-host/tests/js/live-markdown.test.js
//
// Mirrors the Rust unit tests in transcript.rs: the live markdown renderer
// must be escape-first and produce the same safe subset the server does.
'use strict';

const assert = require('assert');
const {
  esc,
  sanitizeUrl,
  mdToHtml,
  prettyJson,
  lastLine,
} = require('../../assets/live.js');

let passed = 0;
function eq(actual, expected, label) {
  try {
    assert.strictEqual(actual, expected);
    passed++;
  } catch (e) {
    console.error(`✗ ${label}\n  expected: ${JSON.stringify(expected)}\n  actual:   ${JSON.stringify(actual)}`);
    process.exitCode = 1;
  }
}
function ok(cond, label) {
  eq(cond, true, label);
}

// ---- esc ----
eq(esc('<script>x</script>'), '&lt;script&gt;x&lt;/script&gt;', 'esc escapes tags');
eq(esc('a&b"c\'d'), 'a&amp;b&quot;c&#x27;d', 'esc escapes entities/quotes');

// ---- sanitizeUrl ----
eq(sanitizeUrl('https://a.b'), 'https://a.b', 'https kept');
eq(sanitizeUrl('mailto:x@y.z'), 'mailto:x@y.z', 'mailto kept');
eq(sanitizeUrl('javascript:alert(1)'), '', 'javascript dropped');
eq(sanitizeUrl('data:text/html,x'), '', 'data dropped');
eq(sanitizeUrl('/relative/path'), '/relative/path', 'relative kept');

// ---- mdToHtml: safety ----
ok(!mdToHtml('**bold** <script>x</script>').includes('<script>'), 'raw html never passed through');
ok(mdToHtml('**bold** <script>x</script>').includes('&lt;script&gt;'), 'raw html shown as literal text');
ok(
  mdToHtml('[x](javascript:alert(1))').includes('href=""'),
  'javascript: link dropped'
);
ok(
  mdToHtml('[ok](https://a.b)').includes('href="https://a.b"'),
  'safe link kept'
);

// ---- mdToHtml: structure ----
eq(
  mdToHtml('**bold** and *italic*'),
  '<p><strong>bold</strong> and <em>italic</em></p>\n',
  'paragraph with inline formatting'
);
ok(mdToHtml('```rust\nfn f() {}\n```').includes('<pre><code class="language-rust">'), 'fenced code with lang');
ok(mdToHtml('```\nplain\n```').includes('<pre><code>plain</code></pre>'), 'fenced code without lang');
eq(
  mdToHtml('- a\n- b'),
  '<ul>\n<li>a</li>\n<li>b</li>\n</ul>\n',
  'unordered list'
);
ok(mdToHtml('1. x\n2. y').includes('<ol>'), 'ordered list');
ok(mdToHtml('> quote').includes('<blockquote>'), 'blockquote');
ok(mdToHtml('|a|b|\n|-|-|\n|1|2|').includes('<p>'), 'tables degrade to plain text (live subset)');
ok(mdToHtml('# Head').includes('<h1>Head</h1>'), 'heading');
ok(mdToHtml('---').includes('<hr>'), 'hr');
// inline code, strikethrough, image-as-link, soft break
ok(mdToHtml('`code`').includes('<code>code</code>'), 'inline code');
ok(mdToHtml('~~gone~~').includes('<del>gone</del>'), 'strikethrough');
ok(
  mdToHtml('![alt](https://a.b/c.png)').includes('<a href="https://a.b/c.png">alt</a>'),
  'image renders as sanitized link'
);
ok(mdToHtml('line1\nline2').includes('line1<br>\nline2'), 'soft break becomes <br>');
// code block content is escaped, raw HTML inside code stays literal
ok(mdToHtml('```html\n<b>x</b>\n```').includes('&lt;b&gt;x&lt;/b&gt;'), 'code content escaped');

// ---- prettyJson ----
eq(prettyJson({ command: 'ls' }), '{\n  "command": "ls"\n}', 'prettyJson formats');
eq(prettyJson('plain'), '"plain"', 'prettyJson string');

// ---- lastLine ----
eq(lastLine('building\ncompiling', 90), 'compiling', 'lastLine picks final non-empty line');
eq(lastLine('  x  ', 90), 'x', 'lastLine trims');
eq(lastLine('abc' + 'd'.repeat(90), 90).endsWith('…'), true, 'lastLine truncates');
eq(lastLine('\n\n', 90), '', 'lastLine empty');

console.log(`\n${process.exitCode ? 'FAILED' : 'passed'}: ${passed} checks`);