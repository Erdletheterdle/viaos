//! Embed the web console. It is built by `web/` (`bun run build`), or by Nix
//! with VIA_WEB_DIST pointing at the result. Without either the server still
//! builds, so `cargo test` needs no JavaScript, and serves a note instead.

use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=VIA_WEB_DIST");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist = std::env::var("VIA_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| manifest.join("../../web/dist"));
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    walk(&dist, &mut files);
    files.sort();
    let mut code = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let name = f
            .strip_prefix(&dist)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        code += &format!(
            "    ({name:?}, include_bytes!({:?})),\n",
            f.display().to_string()
        );
    }
    code += "];\n";
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("web_files.rs");
    std::fs::write(out, code).unwrap();
}
