//! CLI smoke tests. Each chunk should add tests here.

use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn version_flag_prints_version() {
    Command::cargo_bin("pa")
        .unwrap()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("pa "));
}

#[test]
fn help_flag_mentions_workspaces() {
    Command::cargo_bin("pa")
        .unwrap()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("workspaces"));
}

#[test]
fn launch_subcommand_is_unimplemented_for_now() {
    // Until chunk E lands, `pa launch ...` should exit non-zero with a clear
    // "not implemented yet" message rather than silently doing nothing.
    Command::cargo_bin("pa")
        .unwrap()
        .args(["launch", "demo/claude"])
        .assert()
        .failure()
        .stderr(contains("not implemented yet"));
}
