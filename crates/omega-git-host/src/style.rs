//! Compiled stylesheet.
//!
//! `build.rs` compiles the vendored sr.ht SCSS (assets/scss/main.scss) with
//! `grass`; the result is embedded here as a single static string. One
//! stylesheet, no framework JS — the same model sourcehut uses.

/// The full compiled sr.ht-style stylesheet (Bootstrap 4.1.1 + core.sr.ht +
/// git.sr.ht customizations, including `prefers-color-scheme` dark mode).
pub static MAIN_CSS: &str = include_str!(concat!(env!("OUT_DIR"), "/main.css"));
