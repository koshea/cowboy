//! Make the web-UI bundle directory exist (and, on request, build it) so
//! `rust-embed` (in `cmd::web`) can embed it.
//!
//! Three cases, in priority order:
//!
//!  1. **A bundle is already present** (CI runs `trunk build` before building
//!     the binary; a dev may have built it by hand) — embed it as-is, *unless it
//!     is older than the UI's inputs*. A stale bundle is rebuilt when `trunk` is
//!     on hand, and warned about when it is not: embedding one silently ships a
//!     UI that disagrees with the worker about the wire protocol, which once
//!     meant an `ask` the browser never showed and could not dismiss.
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
    println!(
        "cargo:rerun-if-changed={}",
        web_ui.join("index.html").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        web_ui.join("Cargo.toml").display()
    );
    println!("cargo:rerun-if-env-changed=COWBOY_WEB_UI");
    let inputs = bundle_inputs(&web_ui);
    for input in &inputs[4..] {
        println!("cargo:rerun-if-changed={}", input.display());
    }

    // (1) An existing, current bundle wins — don't rebuild over CI's work.
    if bundle_present(&dist) {
        if bundle_is_stale(&dist, &inputs) {
            refresh_stale_bundle(&web_ui);
        }
        return;
    }

    // (2) Opt-in: build the bundle, failing loudly if we can't.
    if std::env::var_os("COWBOY_WEB_UI").is_some_and(|v| v == "1") {
        if let Err(e) = build_bundle(&web_ui) {
            panic!(
                "COWBOY_WEB_UI=1 was set but the web UI bundle could not be built: {e}\n\n\
                 Install a PREBUILT `trunk` (building it from source pulls in libdeflate-sys, \
                 which fails on bleeding-edge compilers like GCC 16), then re-run the install:\n  \
                 • Arch:    sudo pacman -S trunk\n  \
                 • macOS:   brew install trunk\n  \
                 • any OS:  cargo binstall trunk   (cargo install cargo-binstall first)\n  \
                 • or grab a binary from https://github.com/trunk-rs/trunk/releases\n\n\
                 Also ensure the wasm target: rustup target add wasm32-unknown-unknown\n\
                 With `trunk` on PATH, the build reuses it and skips the source install. To verify \
                 manually: (cd {} && trunk build --release)\n",
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

/// What the bundle is built from: the UI crate, and the wire types it shares
/// with the worker. The first four are also watched above; the rest are the
/// extra `rerun-if-changed` paths.
fn bundle_inputs(web_ui: &Path) -> Vec<PathBuf> {
    vec![
        web_ui.join("src"),
        web_ui.join("index.html"),
        web_ui.join("Cargo.toml"),
        web_ui.join("Trunk.toml"),
        web_ui.join("viewport.js"),
        web_ui.join("../cowboy-proto/src"),
        web_ui.join("../cowboy-proto/Cargo.toml"),
    ]
}

/// The bundle predates a change to one of its inputs. By mtime: trunk rewrites
/// `dist/index.html` on every build, and a fresh checkout has no `dist/` at all.
fn bundle_is_stale(dist: &Path, inputs: &[PathBuf]) -> bool {
    let Ok(built) = std::fs::metadata(dist.join("index.html")).and_then(|m| m.modified()) else {
        return false;
    };
    inputs
        .iter()
        .filter_map(|p| newest_mtime(p))
        .any(|t| t > built)
}

/// Newest modification time at or under `path` (`None` if it doesn't exist).
fn newest_mtime(path: &Path) -> Option<std::time::SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    let own = meta.modified().ok();
    if !meta.is_dir() {
        return own;
    }
    std::fs::read_dir(path)
        .ok()?
        .flatten()
        .filter_map(|e| newest_mtime(&e.path()))
        .chain(own)
        .max()
}

/// Rebuild a stale bundle if `trunk` is already installed (never install it
/// behind the developer's back here), else say loudly that it is stale.
fn refresh_stale_bundle(web_ui: &Path) {
    if find_trunk().is_none() {
        println!(
            "cargo:warning=the web UI bundle in {} is older than its sources and `trunk` isn't \
             installed, so a STALE UI is being embedded. Rebuild it: \
             (cd {} && trunk build --release)",
            web_ui.join("dist").display(),
            web_ui.display()
        );
        return;
    }
    println!("cargo:warning=the web UI bundle is older than its sources; rebuilding it");
    if let Err(e) = build_bundle(web_ui) {
        panic!(
            "the web UI bundle is stale and rebuilding it failed: {e}\n\
             Fix the web UI build — (cd {} && trunk build --release) — or delete its dist/ \
             to build without the UI.",
            web_ui.display()
        );
    }
}

/// Ensure the toolchain, then `trunk build --release` into `dist/`.
fn build_bundle(web_ui: &Path) -> Result<(), String> {
    ensure_wasm_target();
    let trunk = ensure_trunk()?;

    println!("cargo:warning=building the web UI bundle (trunk build --release)…");
    let mut cmd = Command::new(&trunk);
    cmd.args(["build", "--release"]).current_dir(web_ui);
    // Cargo hands a build script the *host* build's settings — its encoded
    // rustflags (e.g. `-fuse-ld=mold`, which the wasm linker rejects), target
    // dir, profile. trunk's own cargo run must resolve its config afresh.
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        let host_build = (key.starts_with("CARGO_") && key != "CARGO_HOME")
            || matches!(
                &*key,
                "RUSTFLAGS"
                    | "RUSTC"
                    | "RUSTC_WRAPPER"
                    | "RUSTC_WORKSPACE_WRAPPER"
                    | "RUSTDOC"
                    | "TARGET"
                    | "HOST"
                    | "PROFILE"
                    | "OPT_LEVEL"
                    | "DEBUG"
                    | "OUT_DIR"
                    | "NUM_JOBS"
            );
        if host_build {
            cmd.env_remove(&*key);
        }
    }
    let status = cmd
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

/// Locate `trunk`, installing it if absent. Returns the path to invoke.
///
/// Prefer a **prebuilt** binary: `cargo install trunk` builds it from source,
/// which drags in the C crate `libdeflate-sys` and fails on bleeding-edge
/// toolchains (e.g. GCC 16 rejects the `evex512` target attributes it emits).
/// So if `cargo-binstall` is available we use it (downloads a release binary, no
/// C compile); only if that's missing do we fall back to the source build. A
/// distro package (`pacman -S trunk`, etc.) found on PATH already short-circuits
/// in `find_trunk` and is preferred over both.
fn ensure_trunk() -> Result<PathBuf, String> {
    if let Some(path) = find_trunk() {
        return Ok(path);
    }

    let cargo = std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from);
    let has_binstall = Command::new(&cargo)
        .args(["binstall", "-V"])
        .status()
        .is_ok_and(|s| s.success());

    let install = if has_binstall {
        println!(
            "cargo:warning=trunk not found; installing a prebuilt binary (cargo binstall trunk)…"
        );
        Command::new(&cargo)
            .args(["binstall", "--no-confirm", "trunk"])
            .status()
            .map_err(|e| format!("failed to run `cargo binstall trunk`: {e}"))?
    } else {
        // No prebuilt path available — build from source and hope the toolchain
        // is compatible. If this is the step that failed, the caller's error
        // message points at the prebuilt alternatives.
        println!("cargo:warning=trunk not found and cargo-binstall unavailable; building it from source (cargo install trunk)…");
        Command::new(&cargo)
            .args(["install", "trunk", "--locked"])
            .status()
            .map_err(|e| format!("failed to run `cargo install trunk`: {e}"))?
    };
    if !install.success() {
        let how = if has_binstall {
            "cargo binstall trunk"
        } else {
            "cargo install trunk"
        };
        return Err(format!("`{how}` exited with {install}"));
    }

    find_trunk().ok_or_else(|| {
        "installed trunk but couldn't locate the binary afterwards (is ~/.cargo/bin on PATH?)"
            .to_string()
    })
}

/// `trunk` on PATH, falling back to `$CARGO_HOME/bin` (or `~/.cargo/bin`), where
/// a just-installed binary lands even if PATH hasn't picked it up yet.
fn find_trunk() -> Option<PathBuf> {
    if Command::new("trunk")
        .arg("--version")
        .status()
        .is_ok_and(|s| s.success())
    {
        return Some(PathBuf::from("trunk"));
    }
    let cargo_bin = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cargo")))?
        .join("bin")
        .join("trunk");
    cargo_bin.is_file().then_some(cargo_bin)
}
