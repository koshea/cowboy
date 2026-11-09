//! CLI tests for `cowboy crew` with several crews, the home config dir isolated
//! via `XDG_CONFIG_HOME` so they never touch the real `~/.config/cowboy`.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::prelude::*;

fn cowboy(home: &assert_fs::TempDir, proj: &assert_fs::TempDir) -> Command {
    let mut c = Command::cargo_bin("cowboy").unwrap();
    c.current_dir(proj.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env("COWBOY_ASSUME_YES", "1");
    c
}

const CREWS: &str = "version: 1\n# which crew new sessions get\nactive: oss\ndelegation:\n  \
    enabled: true\ncrews:\n  # cheap open models\n  oss:\n    crew:\n      \
    general: bifrost/glm # the workhorse\n  premium:\n    foreman: openai/gpt\n    crew:\n      \
    general: openai/gpt\n";

fn seed(home: &assert_fs::TempDir) {
    home.child("cowboy/providers.yaml")
        .write_str(
            "version: 1\nproviders:\n  bifrost:\n    base_url: http://b/v1\n  openai:\n    \
             base_url: http://o/v1\n    api_key: k\n",
        )
        .unwrap();
    home.child("cowboy/models.yaml")
        .write_str(
            "version: 1\ndefault: bifrost/glm\nmodels:\n  bifrost:\n    glm:\n      model: g\n  \
             openai:\n    gpt:\n      model: o\n",
        )
        .unwrap();
    home.child("cowboy/crew.yaml").write_str(CREWS).unwrap();
}

fn file(home: &assert_fs::TempDir) -> String {
    std::fs::read_to_string(home.path().join("cowboy/crew.yaml")).unwrap()
}

#[test]
fn list_shows_every_crew_and_the_active_ones_grid() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    cowboy(&home, &proj)
        .args(["crew", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("* oss"))
        .stdout(predicate::str::contains("premium  foreman: openai/gpt"))
        .stdout(predicate::str::contains("bifrost/glm"));
    cowboy(&home, &proj)
        .args(["crew", "list", "--crew", "premium"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "crew premium — foreman: openai/gpt",
        ));
    cowboy(&home, &proj)
        .args(["crew", "validate"])
        .assert()
        .success();
}

#[test]
fn use_and_solo_switch_without_losing_comments() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    cowboy(&home, &proj)
        .args(["crew", "use", "premium"])
        .assert()
        .success();
    let t = file(&home);
    assert!(t.contains("active: premium"), "{t}");
    assert!(
        t.contains("# the workhorse") && t.contains("# cheap open models"),
        "{t}"
    );
    cowboy(&home, &proj)
        .args(["crew", "solo"])
        .assert()
        .success();
    let t = file(&home);
    assert!(
        t.contains("enabled: false") && t.contains("# the workhorse"),
        "{t}"
    );
    cowboy(&home, &proj)
        .args(["crew", "use", "nope"])
        .assert()
        .failure();
}

#[test]
fn new_copy_rename_and_remove() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    cowboy(&home, &proj)
        .args(["crew", "copy", "oss", "oss2"])
        .assert()
        .success();
    assert_eq!(file(&home).matches("# the workhorse").count(), 2);
    cowboy(&home, &proj)
        .args(["crew", "copy", "oss", "oss2"])
        .assert()
        .failure();
    cowboy(&home, &proj)
        .args(["crew", "rename", "oss", "local"])
        .assert()
        .success();
    assert!(
        file(&home).contains("active: local"),
        "the active pointer follows"
    );
    // The active crew cannot be removed; another can.
    cowboy(&home, &proj)
        .args(["crew", "remove", "local"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("active crew"));
    cowboy(&home, &proj)
        .args(["crew", "remove", "oss2"])
        .assert()
        .success();
    // A crew from the models' price tiers.
    cowboy(&home, &proj)
        .args(["crew", "new", "tiers"])
        .assert()
        .success();
    let f = cowboy_core::crew::parse_file(&file(&home)).unwrap();
    assert_eq!(
        f.crews.keys().collect::<Vec<_>>(),
        ["local", "premium", "tiers"]
    );
    assert!(f.crews["tiers"].crew.contains_key("general"));
}

#[test]
fn validate_checks_crews_that_are_not_active() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    home.child("cowboy/crew.yaml")
        .write_str(&CREWS.replace("foreman: openai/gpt", "foreman: ghost/x"))
        .unwrap();
    cowboy(&home, &proj)
        .args(["crew", "validate"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("crew `premium`"))
        .stderr(predicate::str::contains("ghost"));
}

#[test]
fn a_single_roster_points_at_the_migration() {
    let (home, proj) = (
        assert_fs::TempDir::new().unwrap(),
        assert_fs::TempDir::new().unwrap(),
    );
    seed(&home);
    home.child("cowboy/crew.yaml")
        .write_str("version: 1\ncrew:\n  general: bifrost/glm\n")
        .unwrap();
    cowboy(&home, &proj)
        .args(["crew", "list"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cowboy models migrate"));
    cowboy(&home, &proj)
        .args(["models", "migrate", "--yes"])
        .assert()
        .success();
    cowboy(&home, &proj)
        .args(["crew", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("* default"));
}
