//! Build script: compile the vendored sr.ht SCSS (assets/scss/main.scss)
//! into a single bundled stylesheet with `grass`, a pure-Rust Sass compiler.
//! The result lands in OUT_DIR and is embedded into the binary via
//! `include_str!` from src/style.rs.

use std::path::PathBuf;

fn main() {
    let src = PathBuf::from("assets/scss/main.scss");
    if !src.exists() {
        panic!("assets/scss/main.scss missing — vendored sr.ht stylesheet tree incomplete");
    }

    let options = grass::Options::default().load_paths(&[PathBuf::from("assets/scss")]);
    let css = grass::from_path(&src, &options)
        .expect("failed to compile vendored sr.ht SCSS (assets/scss/main.scss)");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("main.css");
    std::fs::write(&out, css).expect("failed to write compiled main.css");
    println!("cargo:rerun-if-changed=assets/scss/main.scss");
    println!("cargo:rerun-if-changed=assets/scss/core");
    println!("cargo:rerun-if-changed=assets/scss/git-sr-ht.scss");
}
