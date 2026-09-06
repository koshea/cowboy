//! CLI test for the security-config load path behind `cowboy sandbox plan`/`exec`.
//!
//! Regression guard for H5: `cmd::sandbox::load` merges the personal credential
//! overlay and MUST re-`validate()` afterward (as the session/worker paths do).
//! `validate()` is the only place that enforces `mount_targets_host_secret` on
//! credential-grant sources, so without the re-validate an overlay grant sourced at
//! `providers.yaml`/`.cowboy` config would be bound into the sandbox. The overlay is
//! host-owned, so this is a defense-in-depth / contract-consistency gap rather than
//! an agent-reachable escape — but the check must be present on every merge site.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy() -> Command {
    Command::cargo_bin("cowboy").unwrap()
}

#[test]
fn sandbox_plan_rejects_a_footgun_overlay_grant_exposing_provider_creds() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();

    // A valid, minimal project config — on its own it validates fine.
    proj.child(".cowboy/security.yaml")
        .write_str("version: 1\n")
        .unwrap();

    // A host-owned personal overlay with a foot-gun credential grant: its source is
    // the provider credential file. This is exactly what `validate()` must catch
    // once merged; if `load()` skips the re-validate, the plan path would accept it.
    home.child("cowboy/secrets/global.yaml")
        .write_str(
            "files:\n  \
             - source: ~/.config/cowboy/providers.yaml\n    \
             target: /tmp/keys\n    \
             read_only: true\n",
        )
        .unwrap();

    cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["sandbox", "plan"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("host-owned secrets"));
}

#[test]
fn sandbox_plan_accepts_an_ordinary_overlay_grant() {
    let home = assert_fs::TempDir::new().unwrap();
    let proj = assert_fs::TempDir::new().unwrap();

    // A project that mounts its workspace at the workdir, so plan building itself
    // has no complaint once the config validates.
    proj.child(".cowboy/security.yaml")
        .write_str(
            "version: 1\n\
             sandbox:\n  \
             workdir: /workspace\n  \
             mounts:\n    \
             - source: .\n      target: /workspace\n      mode: rw\n",
        )
        .unwrap();

    // An ordinary credential grant (a real tool config, not a host secret) merges
    // and validates cleanly.
    home.child("cowboy/secrets/global.yaml")
        .write_str(
            "files:\n  \
             - source: ~/.config/gh\n    \
             target: /tmp/.config/gh\n    \
             read_only: true\n",
        )
        .unwrap();

    // The command validates the merged config (the thing under test). It may still
    // fail later for environment reasons (no bwrap, etc.), so we only assert that it
    // does NOT fail with the host-secret validation error.
    let out = cowboy()
        .current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .args(["sandbox", "plan"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("host-owned secrets"),
        "an ordinary grant must not trip the host-secret guard: {stderr}"
    );
}
