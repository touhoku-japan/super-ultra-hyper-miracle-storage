//! Embeds the Web console (`web/dist`, built with `npm run build` in `web/`) into the binary.
//! Without a build the console serves a placeholder page, so `cargo build` and the tests work
//! without Node.js.

use std::{
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, base, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            let rel = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path.canonicalize().expect("asset path")));
        }
    }
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let dist = manifest.join("../../web/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-env-changed=SUHMS_REQUIRE_WEB");

    let mut files = Vec::new();
    walk(&dist, &dist, &mut files);
    files.sort();
    if env::var_os("SUHMS_REQUIRE_WEB").is_some()
        && !files.iter().any(|(name, _)| name == "index.html")
    {
        panic!(
            "SUHMS_REQUIRE_WEB is set but web/dist/index.html is missing (run `npm run build` in web/)"
        );
    }

    let mut code = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    for (name, path) in &files {
        writeln!(
            code,
            "    ({name:?}, include_bytes!({:?})),",
            path.display().to_string()
        )
        .expect("write to string");
    }
    code.push_str("];\n");
    let out = PathBuf::from(env::var("OUT_DIR").expect("out dir")).join("web_assets.rs");
    fs::write(out, code).expect("write web_assets.rs");
}
