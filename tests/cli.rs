//! CLI smoke tests. Each chunk adds tests here.

use assert_cmd::Command;
use assert_fs::prelude::*;
use predicates::str::contains;

/// Shared tempdir used as the test process's XDG_CONFIG_HOME. Pointing
/// every spawned `pa` at an isolated config dir keeps tests from
/// polluting the real user's `~/.config/portagenty/config.toml` when
/// they exercise code paths that register workspaces globally
/// (`pa init`, `pa onboard`, etc.).
fn test_xdg_config_home() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static DIR: OnceLock<assert_fs::TempDir> = OnceLock::new();
    DIR.get_or_init(|| assert_fs::TempDir::new().unwrap())
        .path()
}

/// Wrapper around `Command::cargo_bin("pa")` that pins an isolated
/// XDG_CONFIG_HOME so writes to the global config stay inside the
/// test sandbox.
fn pa_cmd() -> Command {
    let mut c = Command::cargo_bin("pa").unwrap();
    c.env("XDG_CONFIG_HOME", test_xdg_config_home());
    c
}

#[test]
fn version_flag_prints_version() {
    pa_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(contains("pa "));
}

#[test]
fn help_flag_mentions_workspaces() {
    pa_cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("workspaces"));
}

#[test]
fn bare_pa_with_nonexistent_path_errors_cleanly() {
    // `pa /nope/path/that/does/not/exist` should fail with a clear
    // message instead of silently falling back to walk-up.
    pa_cmd()
        .args(["/nope-xyz-does-not-exist-hopefully"])
        .assert()
        .failure()
        .stderr(contains("doesn't exist"));
}

#[test]
fn launch_errors_when_no_workspace_found() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let empty = tmp.child("empty");
    empty.create_dir_all().unwrap();

    pa_cmd()
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

    pa_cmd()
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

    pa_cmd()
        .args(["launch", "claude", "--dry-run", "--shared"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("shared"))
        .stdout(contains("other clients stay"));
}

#[test]
fn launch_fresh_flag_surfaces_in_dry_run() {
    // --fresh should appear in the dry-run output so the user can
    // verify they typed the right flag, and should mention the mpx
    // session name that would be killed.
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["launch", "claude", "--dry-run", "--fresh"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("fresh"))
        .stdout(contains("kill any existing"));
}

#[test]
fn claim_accepts_fresh_flag() {
    // Parser-level smoke test — claim should accept --fresh just
    // like launch does (they share the same codepath).
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["claim", "claude", "--dry-run", "--fresh"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("fresh"));
}

#[test]
fn claim_with_explicit_name_dry_runs_as_takeover() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
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
    pa_cmd()
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

    pa_cmd()
        .args(["export"])
        .arg("--workspace")
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("#!/usr/bin/env bash"))
        .stdout(contains("tmux new-session"))
        .stdout(contains("'Demo-claude'"))
        .stdout(contains("'Demo-tests'"));
}

#[test]
fn export_with_zellij_format_emits_kdl() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
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

    pa_cmd()
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

    pa_cmd()
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

    pa_cmd()
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

    pa_cmd()
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

    pa_cmd()
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

    pa_cmd()
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

    pa_cmd()
        .args(["init", "dup"])
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(contains("--force"));

    // With --force it overwrites.
    pa_cmd()
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

    pa_cmd()
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
    pa_cmd()
        .args(["init", "myws"])
        .current_dir(tmp.path())
        .assert()
        .success();

    let ws_path = tmp.child("myws.portagenty.toml");

    // Append a new session.
    pa_cmd()
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
    pa_cmd()
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
    pa_cmd()
        .args(["init", "ws"])
        .current_dir(tmp.path())
        .assert()
        .success();
    let ws_path = tmp.child("ws.portagenty.toml");

    // First add succeeds.
    pa_cmd()
        .args(["add", "tests", "-c", "cargo test"])
        .arg("--workspace")
        .arg(ws_path.path())
        .assert()
        .success();

    // Second add with same name fails cleanly.
    pa_cmd()
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
    pa_cmd()
        .args(["add", "claude", "-c", "claude"])
        .current_dir(tmp.path())
        .assert()
        .failure()
        .stderr(contains("pa init"));
}

#[test]
fn completions_bash_produces_bash_completion_script() {
    let out = pa_cmd()
        .args(["completions", "bash"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    // Bash completion scripts always define _pa (or whatever bin
    // name) and use `complete -F` at the bottom.
    assert!(s.contains("_pa"), "expected _pa function:\n{s}");
    assert!(
        s.contains("complete -F"),
        "expected `complete -F` registration:\n{s}"
    );
}

#[test]
fn completions_zsh_produces_zsh_completion_script() {
    let out = pa_cmd()
        .args(["completions", "zsh"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    // Zsh completion scripts start with `#compdef`.
    assert!(s.starts_with("#compdef"), "expected #compdef header:\n{s}");
    // Our subcommand names should appear somewhere.
    assert!(s.contains("launch"));
    assert!(s.contains("claim"));
    assert!(s.contains("init"));
}

#[test]
fn completions_fish_produces_fish_completion_script() {
    pa_cmd()
        .args(["completions", "fish"])
        .assert()
        .success()
        .stdout(contains("complete -c pa"));
}

#[test]
fn rm_removes_matching_session_and_preserves_rest() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws = tmp.child("ws.portagenty.toml");
    ws.write_str(
        r#"# comment at top
name = "Demo"
multiplexer = "tmux"

# comment about claude
[[session]]
name = "claude"
cwd = "."
command = "claude"

[[session]]
name = "tests"
cwd = "."
command = "cargo test"
"#,
    )
    .unwrap();

    pa_cmd()
        .args(["rm", "claude", "-w"])
        .arg(ws.path())
        .assert()
        .success()
        .stdout(contains("removed session"));

    let after = std::fs::read_to_string(ws.path()).unwrap();
    assert!(after.contains("# comment at top"), "top comment preserved");
    assert!(!after.contains(r#"name = "claude""#), "claude block gone");
    assert!(after.contains(r#"name = "tests""#), "tests block kept");
    assert!(after.contains("cargo test"), "tests command kept");
}

#[test]
fn rm_errors_on_unknown_session_name_and_lists_available() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["rm", "nonexistent", "-w"])
        .arg(&ws_path)
        .assert()
        .failure()
        .stderr(contains("no session named"))
        .stderr(contains("claude"))
        .stderr(contains("tests"));
}

#[test]
fn edit_changes_command_in_place() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["edit", "claude", "--command", "claude --resume", "-w"])
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("edited"));

    let after = std::fs::read_to_string(&ws_path).unwrap();
    assert!(after.contains(r#"command = "claude --resume""#));
    // Other session's command left alone.
    assert!(after.contains(r#"command = "cargo nextest run""#));
}

#[test]
fn edit_can_rename_a_session() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["edit", "claude", "--rename", "agent", "-w"])
        .arg(&ws_path)
        .assert()
        .success();

    let after = std::fs::read_to_string(&ws_path).unwrap();
    assert!(after.contains(r#"name = "agent""#));
    // Tests session untouched.
    assert!(after.contains(r#"name = "tests""#));

    // `pa list` sees the new name.
    pa_cmd()
        .args(["list", "-w"])
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("agent"));
}

#[test]
fn edit_rename_collision_errors() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    // demo workspace has "claude" and "tests" — renaming claude to
    // tests should fail.
    pa_cmd()
        .args(["edit", "claude", "--rename", "tests", "-w"])
        .arg(&ws_path)
        .assert()
        .failure()
        .stderr(contains("already named"));
}

#[test]
fn edit_errors_without_any_change_flag() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["edit", "claude", "-w"])
        .arg(&ws_path)
        .assert()
        .failure()
        .stderr(contains("needs at least one"));
}

#[test]
fn edit_errors_with_multiple_field_change_flags() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["edit", "claude", "--command", "a", "--cwd", "/tmp", "-w"])
        .arg(&ws_path)
        .assert()
        .failure()
        .stderr(contains("at most one of"));
}

#[test]
fn edit_sets_env_var_on_session() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["edit", "claude", "--env", "FOO=bar", "-w"])
        .arg(&ws_path)
        .assert()
        .success()
        .stdout(contains("edited session"));
    let raw = std::fs::read_to_string(&ws_path).unwrap();
    assert!(raw.contains("FOO"), "missing env key:\n{raw}");
    assert!(raw.contains("bar"), "missing env val:\n{raw}");
}

#[test]
fn edit_unsets_env_var_on_session() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);
    // First set it.
    pa_cmd()
        .args(["edit", "claude", "--env", "REMOVE_ME=keep", "-w"])
        .arg(&ws_path)
        .assert()
        .success();
    // Now remove it.
    pa_cmd()
        .args(["edit", "claude", "--unset-env", "REMOVE_ME", "-w"])
        .arg(&ws_path)
        .assert()
        .success();
    let raw = std::fs::read_to_string(&ws_path).unwrap();
    assert!(!raw.contains("REMOVE_ME"), "key not removed:\n{raw}");
}

#[test]
fn edit_combines_field_change_with_env_change() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args([
            "edit",
            "claude",
            "--command",
            "newcmd",
            "--env",
            "X=1",
            "-w",
        ])
        .arg(&ws_path)
        .assert()
        .success();
    let raw = std::fs::read_to_string(&ws_path).unwrap();
    assert!(raw.contains("newcmd"), "command not updated:\n{raw}");
    assert!(
        raw.contains('X') && raw.contains('1'),
        "env not added:\n{raw}"
    );
}

#[test]
fn edit_rejects_malformed_env_pair() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let ws_path = write_demo_workspace(&tmp);

    pa_cmd()
        .args(["edit", "claude", "--env", "missing-equals", "-w"])
        .arg(&ws_path)
        .assert()
        .failure()
        .stderr(contains("KEY=VAL"));
}

#[test]
fn snippets_list_shows_known_snippets() {
    pa_cmd()
        .args(["snippets", "list"])
        .assert()
        .success()
        .stdout(contains("pa-aliases"))
        .stdout(contains("termux-friendly"));
}

#[test]
fn snippets_show_prints_file_contents() {
    pa_cmd()
        .args(["snippets", "show", "pa-aliases"])
        .assert()
        .success()
        .stdout(contains("alias p='pa'"))
        .stdout(contains("alias pc='pa claim'"));
}

#[test]
fn snippets_show_errors_on_unknown_name() {
    pa_cmd()
        .args(["snippets", "show", "nope"])
        .assert()
        .failure()
        .stderr(contains("no snippet"));
}

#[test]
fn snippets_install_writes_to_target_file() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let rc = tmp.child(".bashrc");
    rc.write_str("# my shell config\nexport FOO=bar\n").unwrap();

    pa_cmd()
        .args(["snippets", "install", "pa-aliases"])
        .arg("--to")
        .arg(rc.path())
        .assert()
        .success()
        .stdout(contains("installed snippet"));

    let contents = std::fs::read_to_string(rc.path()).unwrap();
    assert!(
        contents.contains("export FOO=bar"),
        "user content preserved"
    );
    assert!(
        contents.contains("pa snippet: pa-aliases"),
        "marker present"
    );
    assert!(contents.contains("alias p='pa'"), "snippet body present");
}

#[test]
fn snippets_install_is_idempotent() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let rc = tmp.child(".bashrc");
    rc.write_str("").unwrap();

    for _ in 0..3 {
        pa_cmd()
            .args(["snippets", "install", "pa-aliases", "--to"])
            .arg(rc.path())
            .assert()
            .success();
    }

    let contents = std::fs::read_to_string(rc.path()).unwrap();
    // Exactly one pair of markers (2 matches: begin + end).
    let marker_count = contents.matches("pa snippet: pa-aliases").count();
    assert_eq!(
        marker_count, 2,
        "expected exactly one begin+end marker pair after 3 installs, got {marker_count}"
    );
}

#[test]
fn snippets_uninstall_removes_block_and_keeps_user_content() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let rc = tmp.child(".bashrc");
    rc.write_str("export FOO=bar\n").unwrap();

    pa_cmd()
        .args(["snippets", "install", "pa-aliases", "--to"])
        .arg(rc.path())
        .assert()
        .success();

    pa_cmd()
        .args(["snippets", "uninstall", "pa-aliases", "--from"])
        .arg(rc.path())
        .assert()
        .success()
        .stdout(contains("removed snippet"));

    let contents = std::fs::read_to_string(rc.path()).unwrap();
    assert!(contents.contains("export FOO=bar"), "user content survived");
    assert!(
        !contents.contains("pa snippet: pa-aliases"),
        "snippet block should be gone"
    );
}

#[test]
fn snippets_install_dry_run_leaves_file_untouched() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let rc = tmp.child(".bashrc");
    rc.write_str("original\n").unwrap();

    pa_cmd()
        .args(["snippets", "install", "pa-aliases", "--dry-run", "--to"])
        .arg(rc.path())
        .assert()
        .success()
        .stdout(contains("DRY RUN"))
        .stdout(contains("alias p='pa'"));

    let contents = std::fs::read_to_string(rc.path()).unwrap();
    assert_eq!(contents, "original\n", "file should not be modified");
}

#[test]
fn list_walks_up_when_no_workspace_flag() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let _ = write_demo_workspace(&tmp);
    let deep = tmp.child("a/b/c");
    deep.create_dir_all().unwrap();

    pa_cmd()
        .arg("list")
        .current_dir(deep.path())
        .assert()
        .success()
        .stdout(contains("workspace: Demo"));
}
