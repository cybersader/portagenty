//! CLI smoke tests. Each chunk adds tests here.

use assert_cmd::Command;
use assert_fs::prelude::*;
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
fn launch_errors_when_no_workspace_found() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let empty = tmp.child("empty");
    empty.create_dir_all().unwrap();

    Command::cargo_bin("pa")
        .unwrap()
        .args(["launch", "claude"])
        .current_dir(empty.path())
        .assert()
        .failure()
        .stderr(contains("no *.portagenty.toml"));
}

fn write_demo_workspace(tmp: &assert_fs::TempDir) -> std::path::PathBuf {
    tmp.child("demo.portagenty.toml")
        .write_str(
            r#"
name = "Demo"
multiplexer = "tmux"

[[session]]
name = "claude"
cwd = "."
command = "echo hi"

[[session]]
name = "tests"
cwd = "."
command = "cargo nextest run"
"#,
        )
        .unwrap();
    tmp.child("demo.portagenty.toml").path().to_path_buf()
}

#[test]
fn launch_dry_run_prints_what_would_happen() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    Command::cargo_bin("pa")
        .unwrap()
        .args(["launch", "claude", "--dry-run"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("would launch"))
        .stdout(contains("claude"))
        .stdout(contains("echo hi"));
}

#[test]
fn launch_errors_on_unknown_session_name() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    Command::cargo_bin("pa")
        .unwrap()
        .args(["launch", "nonexistent", "--dry-run"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .failure()
        .stderr(contains("no session named"))
        .stderr(contains("claude"))
        .stderr(contains("tests"));
}

#[test]
fn list_prints_workspace_summary() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    Command::cargo_bin("pa")
        .unwrap()
        .args(["list"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("workspace: Demo"))
        .stdout(contains("claude"))
        .stdout(contains("tests"))
        .stdout(contains("cargo nextest run"));
}

#[test]
fn list_walks_up_when_no_workspace_flag() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let _ = write_demo_workspace(&tmp);
    let deep = tmp.child("a/b/c");
    deep.create_dir_all().unwrap();

    Command::cargo_bin("pa")
        .unwrap()
        .arg("list")
        .current_dir(deep.path())
        .assert()
        .success()
        .stdout(contains("workspace: Demo"));
}
