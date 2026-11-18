//! Isolated CLI tests for workspace-scoped permission review.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::str::contains;
use std::path::{Path, PathBuf};

fn pa_cmd(home: &Path) -> Command {
    let mut command = Command::cargo_bin("pa").unwrap();
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"));
    command
}

fn workspace_fixture(tmp: &assert_fs::TempDir, with_id: bool) -> PathBuf {
    tmp.child("project with ' quote").create_dir_all().unwrap();
    let id = if with_id {
        "id = \"123e4567-e89b-12d3-a456-426614174000\"\n"
    } else {
        ""
    };
    let content = format!(
        "# preserved workspace comment\nname = \"Demo workspace\"\n{id}multiplexer = \"tmux\"\nprojects = [\"project with ' quote\"]\n\n[[session]]\nname = \"shell\"\ncwd = \".\"\ncommand = \"printf untouched\"\n"
    );
    let path = tmp.child("demo.portagenty.toml");
    path.write_str(&content).unwrap();
    path.path().to_path_buf()
}

#[cfg(unix)]
fn fake_controller(tmp: &assert_fs::TempDir, exit_code: i32) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let bin = tmp.child("fake-bin");
    bin.create_dir_all().unwrap();
    let args_file = tmp.child("controller-args.txt").path().to_path_buf();
    let executable = bin.child("agent-permissions-controller");
    executable
        .write_str(&format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PA_ARGS_FILE\"\nexit {exit_code}\n"
        ))
        .unwrap();
    let mut permissions = std::fs::metadata(executable.path()).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(executable.path(), permissions).unwrap();
    (bin.path().to_path_buf(), args_file)
}

#[test]
fn roots_use_projects_and_merged_session_cwds_without_uuid_fallback() {
    for with_id in [false, true] {
        let tmp = assert_fs::TempDir::new().unwrap();
        let workspace = workspace_fixture(&tmp, with_id);
        let project = tmp
            .child("project with ' quote")
            .path()
            .canonicalize()
            .unwrap();
        let workspace_root = tmp.path().canonicalize().unwrap();

        pa_cmd(tmp.path())
            .args(["permissions", "roots", "--workspace"])
            .arg(&workspace)
            .assert()
            .success()
            .stdout(contains(project.display().to_string()))
            .stdout(contains(workspace_root.display().to_string()));
    }
}

#[test]
fn roots_are_canonical_ordered_and_exactly_deduplicated() {
    let tmp = assert_fs::TempDir::new().unwrap();
    tmp.child("project").create_dir_all().unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        tmp.child("project").path(),
        tmp.child("project-link").path(),
    )
    .unwrap();
    #[cfg(not(unix))]
    std::fs::create_dir(tmp.child("project-link").path()).unwrap();
    let workspace = tmp.child("demo.portagenty.toml");
    workspace
        .write_str(
            "name = \"Demo\"\nprojects = [\"project-link\", \"project\"]\n\n[[session]]\nname = \"same\"\ncwd = \"project\"\ncommand = \"true\"\n",
        )
        .unwrap();

    let output = pa_cmd(tmp.path())
        .args(["permissions", "roots", "--workspace"])
        .arg(workspace.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let canonical = tmp.child("project").path().canonicalize().unwrap();
    assert_eq!(text.matches(&canonical.display().to_string()).count(), 1);
}

#[test]
fn empty_or_invalid_workspace_roots_refuse_without_fallback() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let empty = tmp.child("empty.portagenty.toml");
    empty.write_str("name = \"Empty\"\n").unwrap();
    pa_cmd(tmp.path())
        .args(["permissions", "roots", "--workspace"])
        .arg(empty.path())
        .assert()
        .failure()
        .stderr(contains("declares no existing project paths"));

    let missing = tmp.child("missing.portagenty.toml");
    missing
        .write_str("name = \"Missing\"\nprojects = [\"gone\"]\n")
        .unwrap();
    pa_cmd(tmp.path())
        .args(["permissions", "roots", "--workspace"])
        .arg(missing.path())
        .assert()
        .failure()
        .stderr(contains("missing or cannot be resolved"));
}

#[cfg(unix)]
#[test]
fn review_builds_fixed_argv_and_preserves_workspace_bytes() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let workspace = workspace_fixture(&tmp, true);
    let before = std::fs::read(&workspace).unwrap();
    let (path, args_file) = fake_controller(&tmp, 0);
    let store = tmp.child("store with ' quote.sqlite3");
    store
        .write_binary(b"not opened by fake controller")
        .unwrap();

    pa_cmd(tmp.path())
        .env("PATH", &path)
        .env("PA_ARGS_FILE", &args_file)
        .args(["permissions", "review", "--workspace"])
        .arg(&workspace)
        .arg("--list")
        .args(["--context", "ctx with ' quote", "--store"])
        .arg(store.path())
        .assert()
        .success()
        .stderr(contains("workspace roots:"));

    let project = tmp
        .child("project with ' quote")
        .path()
        .canonicalize()
        .unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let args = std::fs::read_to_string(args_file).unwrap();
    let expected = format!(
        "--terminal\n--workspace-root\n{}\n--workspace-root\n{}\n--list\n--context\nctx with ' quote\n--store\n{}\n",
        project.display(),
        root.display(),
        store.path().display()
    );
    assert_eq!(args, expected);
    assert_eq!(std::fs::read(&workspace).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn unknown_forwarding_flag_is_rejected_before_controller_spawn() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let workspace = workspace_fixture(&tmp, false);
    let (path, args_file) = fake_controller(&tmp, 0);

    pa_cmd(tmp.path())
        .env("PATH", &path)
        .env("PA_ARGS_FILE", &args_file)
        .args(["permissions", "review", "--workspace"])
        .arg(&workspace)
        .args(["--list", "--timeout-seconds", "1"])
        .assert()
        .code(2)
        .stderr(contains("unexpected argument '--timeout-seconds'"));
    assert!(!args_file.exists());
}

#[cfg(unix)]
#[test]
fn missing_controller_is_actionable_and_has_no_fallback() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let workspace = workspace_fixture(&tmp, false);
    let empty_path = tmp.child("empty-path");
    empty_path.create_dir_all().unwrap();

    pa_cmd(tmp.path())
        .env("PATH", empty_path.path())
        .args(["permissions", "review", "--workspace"])
        .arg(&workspace)
        .arg("--list")
        .assert()
        .failure()
        .stderr(contains("was not found on PATH"))
        .stderr(contains("no unfiltered fallback was attempted"));
}

#[cfg(unix)]
#[test]
fn old_controller_rejection_propagates_code_and_compatibility_hint() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let workspace = workspace_fixture(&tmp, false);
    let (path, args_file) = fake_controller(&tmp, 2);

    pa_cmd(tmp.path())
        .env("PATH", &path)
        .env("PA_ARGS_FILE", &args_file)
        .args(["permissions", "review", "--workspace"])
        .arg(&workspace)
        .arg("--list")
        .assert()
        .code(2)
        .stderr(contains("supports repeatable `--workspace-root`"))
        .stderr(contains("no unfiltered fallback was attempted"));
    assert!(args_file.exists());
}

#[cfg(unix)]
#[test]
fn child_failure_and_cancellation_codes_propagate() {
    for code in [7, 130] {
        let tmp = assert_fs::TempDir::new().unwrap();
        let workspace = workspace_fixture(&tmp, false);
        let (path, args_file) = fake_controller(&tmp, code);
        pa_cmd(tmp.path())
            .env("PATH", &path)
            .env("PA_ARGS_FILE", &args_file)
            .args(["permissions", "review", "--workspace"])
            .arg(&workspace)
            .arg("--list")
            .assert()
            .code(code);
        assert!(args_file.exists());
    }
}

#[cfg(unix)]
#[test]
fn interactive_review_refuses_non_tty_without_spawning_controller() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let workspace = workspace_fixture(&tmp, false);
    let (path, args_file) = fake_controller(&tmp, 0);

    pa_cmd(tmp.path())
        .env("PATH", &path)
        .env("PA_ARGS_FILE", &args_file)
        .args(["permissions", "review", "--workspace"])
        .arg(&workspace)
        .assert()
        .failure()
        .stderr(contains("requires a real TTY"));
    assert!(!args_file.exists());
}
