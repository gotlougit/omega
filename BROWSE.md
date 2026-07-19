# BROWSE.md — Web Research Skill

Use CDP (Chrome DevTools Protocol) + Python + Playwright to search the web
and extract information, then synthesize findings into a summary.

---

## 1. Check what's available

```bash
# Is there already a CDP endpoint running?
curl -s http://127.0.0.1:9222/json/version 2>/dev/null
# → returns {"Browser": "...", "webSocketDebuggerUrl": "ws://..."} if yes

# What pages are open?
curl -s http://127.0.0.1:9222/json 2>/dev/null | python3 -m json.tool

# Is Playwright available?
which playwright
playwright --version

```

## 2. Choose your approach

```python
import asyncio
from playwright.async_api import async_playwright

async def browse():
    async with async_playwright() as pw:
        # Connect to existing CDP endpoint
        browser = await pw.chromium.connect_over_cdp("http://127.0.0.1:9333")

        # Use existing context or create one
        ctx = browser.contexts[0] if browser.contexts else await browser.new_context()

        # Reuse the default page if available (some CDP browsers like Lightpanda
        # don't support new_page — reuse existing pages instead)
        pages = ctx.pages
        page = pages[0] if pages else await ctx.new_page()

        await page.goto("https://example.com")
        await page.wait_for_load_state("networkidle")
        text = await page.locator("body").inner_text()
        print(text)

        await browser.close()

asyncio.run(browse())
```

**Troubleshooting CDP connections:**

| Browser | connect_over_cdp | new_page | Notes |
|---|---|---|---|
| Chrome/Chromium | Not allowed | Not allowed | You are not allowed to use or launch Chrome/Chromium AT ALL |
| Lightpanda | ✅ Works | ❌ Not supported | Only one implicit page; use existing `ctx.pages[0]` |

If `new_page` fails with `TargetAlreadyLoaded` or similar, use the existing
page from `ctx.pages` instead:

```python
page = ctx.pages[0] if ctx.pages else await ctx.new_page()
```

### C. Lightpanda CDP Custom Domain (LP)

Lightpanda exposes a custom **LP domain** via CDP with agent-optimized methods not
available in standard Chrome DevTools Protocol. Use these via `page.evaluate` with
CDP sessions or direct WebSocket messages.

This allows you to avoid doing regex to parse the html or something like that,
and allows you to much more easily see and understand the page in a token-efficient
manner.

**Content extraction:**

| Method | Description | Key Params |
|---|---|---|
| `LP.getMarkdown` | Extract page content as markdown | `nodeId` (optional) |
| `LP.getSemanticTree` | Get semantic tree representation | `format` (`text`), `prune` (default: `true`), `interactiveOnly`, `backendNodeId`, `maxDepth` |
| `LP.getStructuredData` | Extract structured data (JSON-LD, OpenGraph, etc.) | — |

**Interactive elements:**

| Method | Description | Key Params |
|---|---|---|
| `LP.getInteractiveElements` | Find all interactive elements | `nodeId` (optional) |
| `LP.detectForms` | Detect and extract form information | — |
| `LP.getNodeDetails` | Get detailed info about a node | `backendNodeId` (required) |
| `LP.waitForSelector` | Wait for a CSS selector match | `selector` (required), `timeout` (default: 5000ms) |

**Actions:**

| Method | Description | Key Params |
|---|---|---|
| `LP.clickNode` | Click a node | `nodeId` or `backendNodeId` |
| `LP.fillNode` | Fill an input/select element | `nodeId` or `backendNodeId`, `text` |
| `LP.scrollNode` | Scroll page or element | `nodeId` or `backendNodeId` (optional), `x`, `y` |

**Example using CDP session with Playwright:**

```python
async def lp_browse():
    async with async_playwright() as pw:
        browser = await pw.chromium.connect_over_cdp("http://127.0.0.1:9333")
        ctx = browser.contexts[0] if browser.contexts else await browser.new_context()
        page = ctx.pages[0] if ctx.pages else await ctx.new_page()

        await page.goto("https://example.com")

        # Create CDP session for LP domain
        client = await ctx.new_cdp_session(page)

        # Get page as markdown
        resp = await client.send("LP.getMarkdown")
        markdown = resp.get("markdown", "")
        print(markdown[:2000])

        # Get semantic tree (text format, limited depth)
        resp = await client.send("LP.getSemanticTree", {"format": "text", "maxDepth": 5})
        print(resp.get("semanticTree", ""))

        # Wait for an element and click it
        resp = await client.send("LP.waitForSelector", {"selector": "#submit-btn", "timeout": 3000})
        await client.send("LP.clickNode", {"backendNodeId": resp["backendNodeId"]})

        await ctx.close()
        await browser.close()
```

**Important Notes for Lightpanda:**

- **Use DuckDuckGo for web searches.** Google blocks Lightpanda due to browser
  fingerprinting.
- **Lightpanda is under heavy development** — occasional issues are expected. It
  executes JavaScript fully, making it suitable for dynamic websites and SPAs.
- **CDP connection limits:** Only 1 CDP connection per process. Each connection
  supports 1 context and 1 page. For parallel browsing, start multiple processes
  on different ports — Lightpanda starts nearly instantly, so this is fast.
- **CDP state management:** The browser resets all state on CDP connection close.
  Keep the WebSocket connection open throughout a session. On each connection,
  always create a new context and page, and close both when done.


## 3. Running the research

Write a throwaway Python script (e.g. `_browse.py`) in the project root, run it,
read the output, refine queries, repeat.

```bash
python3 _browse.py 2>&1
```

**Pattern:**

```python
queries = [
    "your first query here",
    "narrower query here",
    "related concept query",
]

for q in queries:
    results = search(q)
    for title, url in results[:5]:
        print(f"  [{i}] {title}")
        if i == 0:  # open top result for detail
            text = fetch_text(url)
            print(text[:1500])  # condensed preview
```

## 4. Synthesizing findings

After collecting text, summarize what you learned in a structured format:

```markdown
## Research Summary

**Topic:** [what you searched for]

**Key findings:**
1. Finding one with source link
2. Finding two with source link
3. ...

**Implications for our task:**
- How this affects the code change needed
- What approach to take

**Concrete next steps:**
1. Step one
2. Step two
```

## 5. Tips from experience

- **DuckDuckGo HTML** (`html.duckduckgo.com/html/`) is the most reliable way to
  search without a browser — no JS, no cookies, just plain HTML. Use this as
  your default.
- **Rate limiting**: sleep 0.3–0.5s between requests to avoid being blocked.
- **User-Agent**: Always set a realistic User-Agent header. Some sites 403 on
  Python's default.
- **GitHub raw content**: Use `raw.githubusercontent.com` instead of the web UI
  to get plain text without HTML noise.
- **docs.rs**: Returns HTML that's relatively clean to strip, but the main
  content area is the easiest to extract.
- **Launching your own browser**: If no CDP endpoint is running, you can NOT launch
  one YOURSELF! Ask user to do it first and abort the tool call.
- **Timeout handling**: Always wrap page loads in try/except — networks fail,
  pages 404, and CDP browsers sometimes hang.
- **Text extraction**: The regex-based HTML→text stripping is fast and works
  well for research. For production use an HTML parser, but for throwaway
  scripts this is sufficient.
- **Iterate**: Run the script, read output, refine queries, repeat 2–3 times
  until you have enough context. Don't try to get everything in one pass.
