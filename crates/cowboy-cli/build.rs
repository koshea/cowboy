//! Ensure the web-UI bundle directory exists so `rust-embed` (in `cmd::web`) can
//! embed it even on a checkout that hasn't run `trunk build`. An empty dir embeds
//! to zero assets, and the server falls back to a placeholder page. CI / a local
//! `trunk build` populates it with the real bundle, which then gets embedded.
use std::path::Path;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let dist = Path::new(&manifest).join("../cowboy-web-ui/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(&dist);
    }
    // Rebuild when the bundle changes so an updated UI is re-embedded.
    println!("cargo:rerun-if-changed={}", dist.display());
}
