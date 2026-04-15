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
        .stdout(contains("echo hi"))
        .stdout(contains("takeover"));
}

#[test]
fn launch_with_shared_flag_reports_shared_mode() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    Command::cargo_bin("pa")
        .unwrap()
        .args(["launch", "claude", "--dry-run", "--shared"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("shared"))
        .stdout(contains("other clients stay"));
}

#[test]
fn claim_with_explicit_name_dry_runs_as_takeover() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    Command::cargo_bin("pa")
        .unwrap()
        .args(["claim", "tests", "--dry-run"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("would launch"))
        .stdout(contains("tests"))
        .stdout(contains("takeover"));
}

#[test]
fn claim_without_name_defaults_to_first_session() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    // demo workspace has two sessions: "claude" (declared first) and "tests".
    Command::cargo_bin("pa")
        .unwrap()
        .args(["claim", "--dry-run"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("claude"))
        .stdout(contains("takeover"));
}

#[test]
fn export_to_stdout_uses_workspace_default_format() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    Command::cargo_bin("pa")
        .unwrap()
        .args(["export"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("#!/usr/bin/env bash"))
        .stdout(contains("tmux new-session"))
        .stdout(contains("'claude'"))
        .stdout(contains("'tests'"));
}

#[test]
fn export_with_zellij_format_emits_kdl() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    Command::cargo_bin("pa")
        .unwrap()
        .args(["export", "--format", "zellij"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("layout {"))
        .stdout(contains(r#"tab name="claude""#))
        .stdout(contains(r#"tab name="tests""#));
}

#[test]
fn export_writes_to_output_path_when_dash_o_given() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);
    let out = tmp.child("starter.sh");

    Command::cargo_bin("pa")
        .unwrap()
        .args(["export", "--format", "tmux"])
        .arg("--workspace")
        .arg(&ws_path)
        .arg("-o")
        .arg(out.path())
        .assert()
        .success()
        .stdout(predicates::str::is_empty());

    assert!(out.path().is_file(), "output file should exist");
    let contents = std::fs::read_to_string(out.path()).unwrap();
    assert!(contents.contains("tmux new-session"));
}

#[test]
fn claim_errors_when_workspace_has_no_sessions() {
    let tmp = assert_fs::TempDir::new().unwrap();
    tmp.child("empty.portagenty.toml")
        .write_str(
            r#"
name = "Empty"
multiplexer = "tmux"
"#,
        )
        .unwrap();

    Command::cargo_bin("pa")
        .unwrap()
        .args(["claim", "--dry-run"])
        .arg("--workspace")
        .arg(tmp.child("empty.portagenty.toml").path())
        .assert()
        .failure()
        .stderr(contains("no sessions to claim"));
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
fn init_creates_starter_workspace_with_sane_defaults() {
    let tmp = assert_fs::TempDir::new().unwrap();

    Command::cargo_bin("pa")
        .unwrap()
        .args(["init", "my-space"])
        .current_dir(tmp.path())
        .assert()
        .success()
        .stdout(contains("created"))
        .stdout(contains("pa add"));

    let path = tmp.child("my-space.portagenty.toml");
    assert!(path.path().is_file(), "workspace file should exist");

    let contents = std::fs::read_to_string(path.path()).unwrap();
    assert!(contents.contains(r#"name = "my-space""#));
    assert!(contents.contains(r#"multiplexer = "tmux""#));
    assert!(contents.contains(r#"name = "shell""#));
    assert!(contents.contains(r#"command = "bash""#));
    assert!(contents.contains(r#"kind = "shell""#));
}

#[test]
fn init_with_zellij_mpx_flag_pins_zellij() {
    let tmp = assert_fs::TempDir::new().unwrap();

    Command::cargo_bin("pa")
        .unwrap()
        .args(["init", "zj-space", "--mpx", "zellij"])
        .current_dir(tmp.path())
        .assert()
        .success();

    let path = tmp.child("zj-space.portagenty.toml");
    let contents = std::fs::read_to_string(path.path()).unwrap();
    assert!(contents.contains(r#"multiplexer = "zellij""#));
}

#[test]
fn init_errors_when_file_already_exists_unless_force() {
    let tmp = assert_fs::TempDir::new().unwrap();
    tmp.child("dup.portagenty.toml").write_str("").unwrap();

    Command::cargo_bin("pa")
        .unwrap()
        .args(["init", "dup"])
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(contains("--force"));

    // With --force it overwrites.
    Command::cargo_bin("pa")
        .unwrap()
        .args(["init", "dup", "--force"])
        .current_dir(tmp.path())
        .assert()
        .success();
}

#[test]
fn init_defaults_name_to_current_directory() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let nested = tmp.child("my-project-name");
    nested.create_dir_all().unwrap();

    Command::cargo_bin("pa")
        .unwrap()
        .args(["init"])
        .current_dir(nested.path())
        .assert()
        .success();

    let path = nested.child("my-project-name.portagenty.toml");
    assert!(path.path().is_file());
    let contents = std::fs::read_to_string(path.path()).unwrap();
    assert!(contents.contains(r#"name = "my-project-name""#));
}

#[test]
fn add_appends_new_session_and_pa_list_sees_it() {
    let tmp = assert_fs::TempDir::new().unwrap();
    // Bootstrap with init.
    Command::cargo_bin("pa")
        .unwrap()
        .args(["init", "myws"])
        .current_dir(tmp.path())
        .assert()
        .success();

    let ws_path = tmp.child("myws.portagenty.toml");

    // Append a new session.
    Command::cargo_bin("pa")
        .unwrap()
        .args([
            "add",
            "claude",
            "-c",
            "claude --resume",
            "--kind",
            "claude-code",
        ])
        .arg("--workspace")
        .arg(ws_path.path())
        .assert()
        .success()
        .stdout(contains("added session"));

    // pa list sees both the original shell session and the new one.
    Command::cargo_bin("pa")
        .unwrap()
        .args(["list"])
        .arg("--workspace")
        .arg(ws_path.path())
        .assert()
        .success()
        .stdout(contains("shell"))
        .stdout(contains("claude"))
        .stdout(contains("claude --resume"));
}

#[test]
fn add_errors_on_duplicate_session_name() {
    let tmp = assert_fs::TempDir::new().unwrap();
    Command::cargo_bin("pa")
        .unwrap()
        .args(["init", "ws"])
        .current_dir(tmp.path())
        .assert()
        .success();
    let ws_path = tmp.child("ws.portagenty.toml");

    // First add succeeds.
    Command::cargo_bin("pa")
        .unwrap()
        .args(["add", "tests", "-c", "cargo test"])
        .arg("--workspace")
        .arg(ws_path.path())
        .assert()
        .success();

    // Second add with same name fails cleanly.
    Command::cargo_bin("pa")
        .unwrap()
        .args(["add", "tests", "-c", "cargo test"])
        .arg("--workspace")
        .arg(ws_path.path())
        .assert()
        .failure()
        .stderr(contains("already exists"));
}

#[test]
fn add_errors_when_no_workspace_found() {
    let tmp = assert_fs::TempDir::new().unwrap();
    Command::cargo_bin("pa")
        .unwrap()
        .args(["add", "claude", "-c", "claude"])
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(contains("pa init"));
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
