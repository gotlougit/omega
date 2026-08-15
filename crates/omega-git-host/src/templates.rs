//! Server-rendered templates (minijinja).
//!
//! The markup ports sourcehut's shared layout/nav (core.sr.ht, BSD-3) and the
//! git forge pages (git.sr.ht, AGPL-3.0) — see assets/NOTICE.md. Templates are
//! embedded into the binary at compile time; no JS is required to render.

use anyhow::{Context, Result};
use chrono::DateTime;
use minijinja::{Environment, Value};
use serde::Serialize;

pub struct Templates {
    env: Environment<'static>,
}

impl Templates {
    pub fn new() -> Result<Self> {
        let mut env = Environment::new();
        env.add_filter("date", date_filter);
        env.add_filter("diff", diff_filter);
        env.add_template("layout.html", include_str!("../templates/layout.html"))
            .context("register layout.html")?;
        env.add_template("nav.html", include_str!("../templates/nav.html"))
            .context("register nav.html")?;
        env.add_template("index.html", include_str!("../templates/index.html"))
            .context("register index.html")?;
        env.add_template("repo.html", include_str!("../templates/repo.html"))
            .context("register repo.html")?;
        env.add_template("summary.html", include_str!("../templates/summary.html"))
            .context("register summary.html")?;
        env.add_template("log.html", include_str!("../templates/log.html"))
            .context("register log.html")?;
        env.add_template("tree.html", include_str!("../templates/tree.html"))
            .context("register tree.html")?;
        env.add_template("blob.html", include_str!("../templates/blob.html"))
            .context("register blob.html")?;
        env.add_template("commit.html", include_str!("../templates/commit.html"))
            .context("register commit.html")?;
        env.add_template("refs.html", include_str!("../templates/refs.html"))
            .context("register refs.html")?;
        env.add_template("clone.html", include_str!("../templates/clone.html"))
            .context("register clone.html")?;
        env.add_template("sessions.html", include_str!("../templates/sessions.html"))
            .context("register sessions.html")?;
        env.add_template("session.html", include_str!("../templates/session.html"))
            .context("register session.html")?;
        env.add_template("session-prompt.html", include_str!("../templates/session-prompt.html"))
            .context("register session-prompt.html")?;
        Ok(Self { env })
    }

    pub fn render(&self, name: &str, ctx: impl Serialize) -> Result<String> {
        let template = self
            .env
            .get_template(name)
            .with_context(|| format!("get template {name}"))?;
        template
            .render(ctx)
            .with_context(|| format!("render template {name}"))
    }
}

/// Format an RFC3339 timestamp (as serialized by chrono) as `YYYY-MM-DD HH:MM`
/// in UTC. minijinja passes values as `Value`; we accept strings or fall back
/// to the raw value.
fn date_filter(value: &Value) -> String {
    match value.as_str() {
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|_| s.to_string()),
        None => value.to_string(),
    }
}

/// Render raw `git show` diff output as sr.ht-style markup: every line is
/// HTML-escaped, `+` lines get `text-success`, `-` lines `text-danger`
/// (matching the `.diff` styles in the vendored git.sr.ht stylesheet).
/// Filter outputs are not auto-escaped by minijinja, so escaping happens here.
fn diff_filter(value: &Value) -> String {
    let raw = value.as_str().unwrap_or_default();
    let mut out = String::with_capacity(raw.len());
    for line in raw.split('\n') {
        let escaped = html_escape(line);
        if let Some(rest) = line.strip_prefix('+') {
            out.push_str(&format!("<span class=\"text-success\">+{}</span>\n", html_escape(rest)));
        } else if let Some(rest) = line.strip_prefix('-') {
            out.push_str(&format!("<span class=\"text-danger\">-{}</span>\n", html_escape(rest)));
        } else {
            out.push_str(&escaped);
            out.push('\n');
        }
    }
    out
}

pub(crate) fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}
