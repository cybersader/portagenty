//! End-to-end tests for `config::load`. Uses `assert_fs` tempdirs for
//! filesystem state; no state leaks out to the dev machine.

use assert_fs::prelude::*;
use portagenty::config::{load, LoadOptions};
use portagenty::domain::Multiplexer;

#[test]
fn load_explicit_path_end_to_end() {
    let tmp = assert_fs::TempDir::new().unwrap();

    tmp.child("demo.portagenty.toml")
        .write_str(
            r#"
name = "Demo"
multiplexer = "tmux"
projects = ["./alpha"]

[[session]]
name = "claude"
cwd = "./alpha"
command = "claude"
"#,
        )
        .unwrap();

    let project_dir = tmp.child("alpha");
    project_dir.create_dir_all().unwrap();
    project_dir
        .child("portagenty.toml")
        .write_str(
            r#"
[[session]]
name = "tests"
cwd = "."
command = "cargo nextest run"
"#,
        )
        .unwrap();

    let opts = LoadOptions {
        workspace_path: Some(tmp.child("demo.portagenty.toml").path().to_path_buf()),
        global_config_override: Some(tmp.child("no-such-global.toml").path().to_path_buf()),
        ..Default::default()
    };

    let w = load(&opts).expect("load");
    assert_eq!(w.name, "Demo");
    assert_eq!(w.multiplexer, Multiplexer::Tmux);
    assert_eq!(w.projects.len(), 1);

    // Two sessions in total: "claude" from the workspace and "tests"
    // from the project's own portagenty.toml.
    assert_eq!(w.sessions.len(), 2);
    let names: Vec<&str> = w.sessions.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"claude"));
    assert!(names.contains(&"tests"));
}

#[test]
fn load_walks_up_from_cwd_to_find_workspace() {
    let tmp = assert_fs::TempDir::new().unwrap();
    tmp.child("up.portagenty.toml")
        .write_str(
            r#"
name = "walked-up"
multiplexer = "tmux"
"#,
        )
        .unwrap();

    let deep = tmp.child("a/b/c");
    deep.create_dir_all().unwrap();

    let opts = LoadOptions {
        cwd: Some(deep.path().to_path_buf()),
        global_config_override: Some(tmp.child("no-such-global.toml").path().to_path_buf()),
        ..Default::default()
    };

    let w = load(&opts).expect("load");
    assert_eq!(w.name, "walked-up");
    assert!(w.sessions.is_empty());
}

#[test]
fn load_errors_when_no_workspace_found() {
    let tmp = assert_fs::TempDir::new().unwrap();
    let empty = tmp.child("empty");
    empty.create_dir_all().unwrap();

    let opts = LoadOptions {
        cwd: Some(empty.path().to_path_buf()),
        global_config_override: Some(tmp.child("no-such-global.toml").path().to_path_buf()),
        ..Default::default()
    };

    let err = load(&opts).expect_err("expected no-workspace error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("no *.portagenty.toml found"),
        "unexpected error: {msg}"
    );
}

#[test]
fn load_applies_global_default_multiplexer_when_workspace_omits_it() {
    let tmp = assert_fs::TempDir::new().unwrap();

    tmp.child("demo.portagenty.toml")
        .write_str(
            r#"
name = "Demo"
"#,
        )
        .unwrap();

    tmp.child("global.toml")
        .write_str(r#"default-multiplexer = "zellij""#)
        .unwrap();

    let opts = LoadOptions {
        workspace_path: Some(tmp.child("demo.portagenty.toml").path().to_path_buf()),
        global_config_override: Some(tmp.child("global.toml").path().to_path_buf()),
        ..Default::default()
    };

    let w = load(&opts).expect("load");
    assert_eq!(w.multiplexer, Multiplexer::Zellij);
}

#[test]
fn load_workspace_override_beats_global_default_multiplexer() {
    let tmp = assert_fs::TempDir::new().unwrap();

    tmp.child("demo.portagenty.toml")
        .write_str(
            r#"
name = "Demo"
multiplexer = "wezterm"
"#,
        )
        .unwrap();

    tmp.child("global.toml")
        .write_str(r#"default-multiplexer = "zellij""#)
        .unwrap();

    let opts = LoadOptions {
        workspace_path: Some(tmp.child("demo.portagenty.toml").path().to_path_buf()),
        global_config_override: Some(tmp.child("global.toml").path().to_path_buf()),
        ..Default::default()
    };

    let w = load(&opts).expect("load");
    assert_eq!(w.multiplexer, Multiplexer::Wezterm);
}

#[test]
fn load_workspace_session_beats_per_project_on_name_collision() {
    let tmp = assert_fs::TempDir::new().unwrap();

    tmp.child("demo.portagenty.toml")
        .write_str(
            r#"
name = "Demo"
multiplexer = "tmux"
projects = ["./alpha"]

[[session]]
name = "dev"
cwd = "./alpha"
command = "ws-version"
"#,
        )
        .unwrap();

    let project_dir = tmp.child("alpha");
    project_dir.create_dir_all().unwrap();
    project_dir
        .child("portagenty.toml")
        .write_str(
            r#"
[[session]]
name = "dev"
cwd = "."
command = "project-version"
"#,
        )
        .unwrap();

    let opts = LoadOptions {
        workspace_path: Some(tmp.child("demo.portagenty.toml").path().to_path_buf()),
        global_config_override: Some(tmp.child("no-such-global.toml").path().to_path_buf()),
        ..Default::default()
    };

    let w = load(&opts).expect("load");
    assert_eq!(w.sessions.len(), 1);
    assert_eq!(w.sessions[0].command, "ws-version");
}
