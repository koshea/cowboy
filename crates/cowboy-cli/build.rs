//! Make the web-UI bundle directory exist (and, on request, build it) so
//! `rust-embed` (in `cmd::web`) can embed it.
//!
//! Three cases, in priority order:
//!
//!  1. **A bundle is already present** (CI runs `trunk build` before building
//!     the binary; a dev may have built it by hand) — embed it as-is. Never
//!     rebuild over an existing bundle.
//!  2. **`COWBOY_WEB_UI=1`** — the opt-in install path
//!     (`COWBOY_WEB_UI=1 cargo install --git … cowboy-cli`). Ensure the wasm
//!     target and `trunk` are present (installing them if missing), run
//!     `trunk build --release`, and embed the result. A failure here is a hard
//!     error with instructions: the user explicitly asked for the UI, so a
//!     silent fallback to the placeholder would be confusing.
//!  3. **Otherwise** (plain `cargo build`/`test` during dev, and the default
//!     `cargo install` without the opt-in) — just ensure an empty `dist/`
//!     exists. It embeds to zero assets and the server serves a placeholder.
//!     This keeps the dev inner loop fast and never runs WASM tooling behind
//!     the developer's back.
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let web_ui = Path::new(&manifest).join("../cowboy-web-ui");
    let dist = web_ui.join("dist");

    // Re-embed when the bundle or the UI source changes; re-evaluate when the
    // opt-in flag is toggled.
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-changed={}", web_ui.join("src").display());
    println!("cargo:rerun-if-changed={}", web_ui.join("index.html").display());
    println!("cargo:rerun-if-changed={}", web_ui.join("Cargo.toml").display());
    println!("cargo:rerun-if-env-changed=COWBOY_WEB_UI");

    // (1) An existing bundle always wins — don't rebuild over CI's work.
    if bundle_present(&dist) {
        return;
    }

    // (2) Opt-in: build the bundle, failing loudly if we can't.
    if std::env::var_os("COWBOY_WEB_UI").is_some_and(|v| v == "1") {
        if let Err(e) = build_bundle(&web_ui) {
            panic!(
                "COWBOY_WEB_UI=1 was set but the web UI bundle could not be built: {e}\n\n\
                 Build it manually, then re-run the install:\n  \
                 rustup target add wasm32-unknown-unknown\n  \
                 cargo install trunk\n  \
                 (cd {} && trunk build --release)\n",
                web_ui.display()
            );
        }
        return;
    }

    // (3) Default: ensure an empty dist so rust-embed has a folder to read.
    if !dist.exists() {
        let _ = std::fs::create_dir_all(&dist);
    }
}

/// A bundle exists if `dist/index.html` is present (trunk always emits one).
fn bundle_present(dist: &Path) -> bool {
    dist.join("index.html").is_file()
}

/// Ensure the toolchain, then `trunk build --release` into `dist/`.
fn build_bundle(web_ui: &Path) -> Result<(), String> {
    ensure_wasm_target();
    let trunk = ensure_trunk()?;

    println!("cargo:warning=building the web UI bundle (trunk build --release)…");
    let status = Command::new(&trunk)
        .args(["build", "--release"])
        .current_dir(web_ui)
        .status()
        .map_err(|e| format!("failed to run trunk ({}): {e}", trunk.display()))?;
    if !status.success() {
        return Err(format!("`trunk build --release` exited with {status}"));
    }
    Ok(())
}

/// Best-effort `rustup target add wasm32-unknown-unknown`. Ignored if rustup
/// isn't the toolchain manager — trunk will surface a clear error if the target
/// is genuinely missing.
fn ensure_wasm_target() {
    let _ = Command::new("rustup")
        .args(["target", "add", "wasm32-unknown-unknown"])
        .status();
}

/// Locate `trunk`, installing it via `cargo install trunk` if absent. Returns
/// the path to invoke.
fn ensure_trunk() -> Result<PathBuf, String> {
    if let Some(path) = find_trunk() {
        return Ok(path);
    }

    // Not found — install it with the same cargo that's driving this build.
    let cargo = std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from);
    println!("cargo:warning=trunk not found; installing it (cargo install trunk)…");
    let status = Command::new(&cargo)
        .args(["install", "trunk", "--locked"])
        .status()
        .map_err(|e| format!("failed to run `cargo install trunk`: {e}"))?;
    if !status.success() {
        return Err(format!("`cargo install trunk` exited with {status}"));
    }

    find_trunk().ok_or_else(|| {
        "installed trunk but couldn't locate the binary afterwards (is ~/.cargo/bin on PATH?)"
            .to_string()
    })
}

/// `trunk` on PATH, falling back to `$CARGO_HOME/bin` (or `~/.cargo/bin`), where
/// a just-installed binary lands even if PATH hasn't picked it up yet.
fn find_trunk() -> Option<PathBuf> {
    if Command::new("trunk").arg("--version").status().is_ok_and(|s| s.success()) {
        return Some(PathBuf::from("trunk"));
    }
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")))?
        .join("bin")
        .join("trunk");
    cargo_bin.is_file().then_some(cargo_bin)
}
