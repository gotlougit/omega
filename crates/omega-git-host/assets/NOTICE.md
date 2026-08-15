# Vendored assets — provenance & licensing

Everything under `assets/` is copied from upstream sourcehut repositories
(verified 2026-08-15) and keeps its own license. Nothing here is original
omega code.

| Path | Origin | License |
|---|---|---|
| `scss/core/*` | `https://git.sr.ht/~sircmpwn/core.sr.ht` (`scss/`) | BSD-3-Clause (see `licenses/LICENSE.core.sr.ht`) |
| `scss/core/bootstrap/*` | `https://github.com/twbs/bootstrap` at commit `779ad9f174ea5ab7e755f6df0ec9e5912d67dd16` (v4.1.1, the commit pinned by core.sr.ht's `scss/bootstrap` submodule) | MIT (see `licenses/LICENSE.bootstrap`) |
| `scss/git-sr-ht.scss` | `https://git.sr.ht/~sircmpwn/git.sr.ht` (`scss/main.scss`, minus its leading `@import "base"`) | AGPL-3.0 (see `licenses/LICENSE.git.sr.ht`) |
| `reference/*.html` | `core.sr.ht` (`srht/templates/layout.html`, `nav.html`) and `git.sr.ht` (`gitsrht/templates/*.html`) | BSD-3-Clause / AGPL-3.0 respectively |

Notes:

- `scss/main.scss` is omega-git-host's own entry point that combines the two
  upstream stylesheets; it is original omega code (AGPL-3.0, project license).
- The whole omega project is AGPL-3.0-or-later; the BSD-3 and MIT assets above
  are permissive and AGPL-compatible, the git.sr.ht-derived stylesheet is
  AGPL-3.0.
- `reference/` templates are kept for porting reference only — the served
  templates are original implementations that follow the same markup/class
  structure.
- No font assets are vendored: sr.ht's icons are inline SVG (`scss/core/icons.scss`),
  and `font-awesome.min.css` (which references webfonts) is intentionally not
  copied because nothing imports it.
