//! ratatui app: workspace tree + session list, vim-flavored keybindings,
//! responsive layout for Termux/small-screen use. See `DESIGN.md` §10.

pub mod app;
pub mod confirm;
pub mod edit;
pub mod find;
pub mod footer;
pub mod help;
pub mod picker;
#[cfg(target_os = "linux")]
pub mod resources;
pub mod view;

pub use app::{
    Action, App, AppOutcome, AppRunResult, LaunchKind, RowSelectionIdentity, SupervisionIntent,
};
pub use view::{build_rows, SessionRow, SessionState};

use anyhow::{Context, Result};

use crate::config::{load, LoadOptions};
use crate::mux::{ClientCompletion, TmuxAdapter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitialScreen {
    Picker,
    Workspace,
}

fn initial_screen(explicit_path: bool) -> InitialScreen {
    if explicit_path {
        InitialScreen::Workspace
    } else {
        InitialScreen::Picker
    }
}

/// Entry point for the interactive TUI. Bare `pa` opens the global workspace
/// picker — the home screen — while `pa PATH` jumps directly into the workspace
/// resolved from that explicit path.
///
/// `explicit_path` accepts either a `*.portagenty.toml` file or a directory
/// (walks up from the directory if it's a dir).
pub fn run(explicit_path: Option<&std::path::Path>) -> Result<()> {
    // See DESIGN.md §12 for the full entry-point contract. Key
    // invariant: the workspace picker is the *home screen*. Esc from
    // the session TUI always returns here, regardless of whether the
    // user entered via walk-up, wizard-scaffold, or the picker
    // itself. There is exactly one back-stack; pa is never ambiguous
    // about "what does Esc do."

    // Resolve the explicit path (if any) into LoadOptions once up
    // front — used for both the onboarding guard below and the main
    // first-iteration load. A path pointing at a directory triggers
    // walk-up-from-there; a file is used directly. A missing or
    // broken path errors cleanly instead of silently falling back
    // to walk-up from $PWD.
    let explicit_opts = match explicit_path {
        Some(p) if p.is_file() => Some(LoadOptions {
            workspace_path: Some(p.to_path_buf()),
            ..Default::default()
        }),
        Some(p) if p.is_dir() => Some(LoadOptions {
            cwd: Some(p.to_path_buf()),
            ..Default::default()
        }),
        Some(p) => {
            return Err(anyhow::anyhow!(
                "path {} doesn't exist (or isn't a file / directory)",
                p.display()
            ));
        }
        None => None,
    };
    let load_opts = || explicit_opts.clone().unwrap_or_default();

    // First-run wizard short-circuits before the TUI loop, since
    // showing the picker with zero workspaces on a brand-new machine
    // would just bounce straight to onboarding anyway.
    if load(&load_opts()).is_err()
        && crate::onboarding::is_interactive()
        && !crate::onboarding::has_onboarded()
    {
        use crate::onboarding::OnboardOutcome;
        match crate::onboarding::run_wizard(false)? {
            OnboardOutcome::ShowedDocs | OnboardOutcome::Skipped => return Ok(()),
            OnboardOutcome::Scaffolded { .. } => {
                // Fall through to the TUI loop with the new workspace
                // pre-selected. Walk-up will pick it up now.
            }
        }
    }

    // Non-interactive (piped, CI, cron) with no walkable workspace
    // and no onboarding: nothing useful to show, exit cleanly with
    // the original error.
    if !crate::onboarding::is_interactive() && load(&load_opts()).is_err() {
        return Err(anyhow::anyhow!(
            "no *.portagenty.toml found walking up from {}",
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".into())
        ));
    }

    // Main loop: picker ↔ session TUI, sharing one ratatui session. Bare `pa`
    // starts at the picker; only an explicit `pa PATH` enters a workspace
    // directly. After Esc, every path returns to the picker.
    let mut terminal = ratatui::init();
    let mut first_iteration = true;
    let mut resume_workspace: Option<(crate::domain::Workspace, RowSelectionIdentity, String)> =
        None;
    loop {
        let resumed = resume_workspace.take();
        let ws = if let Some((workspace, _, _)) = &resumed {
            Some(workspace.clone())
        } else if first_iteration {
            first_iteration = false;
            let loaded = load(&load_opts()).ok();
            // Auto-re-register the workspace reachable from the launch path even
            // when bare `pa` opens the picker. This preserves moved-workspace
            // reconciliation without making the walk-up workspace the home
            // screen.
            if let Some(ref w) = loaded {
                if let Some(ref path) = w.file_path {
                    let _ = crate::config::register_global_workspace(path);
                    let _ = crate::config::reconcile_previous_paths_on_reregister(path);
                }
            }
            match initial_screen(explicit_opts.is_some()) {
                InitialScreen::Picker => None,
                InitialScreen::Workspace => loaded,
            }
        } else {
            None
        };

        let ws = match ws {
            Some(w) => w,
            None => match show_picker(&mut terminal) {
                Ok(PickResult::Workspace(w)) => w,
                Ok(PickResult::OpenShellAt(dir)) => {
                    ratatui::restore();
                    return spawn_shell_at(&dir);
                }
                Ok(PickResult::Quit) => {
                    ratatui::restore();
                    return Ok(());
                }
                Err(e) => {
                    ratatui::restore();
                    return Err(e);
                }
            },
        };

        let (resume_selection, resume_notice) = match resumed {
            Some((_, selection, notice)) => (Some(selection), Some(notice)),
            None => (None, None),
        };
        match run_session_tui(&mut terminal, ws, resume_selection.as_ref(), resume_notice) {
            Ok(SessionRunOutcome::Back) => continue,
            Ok(SessionRunOutcome::Quit) => {
                ratatui::restore();
                return Ok(());
            }
            Ok(SessionRunOutcome::SetupFailed {
                workspace,
                selection,
                error,
            }) => {
                eprintln!();
                eprintln!("  pa: session setup failed before entering the multiplexer client.");
                eprintln!("  {error:#}");
                eprintln!("  Returning to the same workspace and session.");
                eprintln!();
                let notice = format!("setup failed: {error:#}");
                terminal = ratatui::init();
                resume_workspace = Some((workspace, selection, notice));
                continue;
            }
            Ok(SessionRunOutcome::ClientReturned {
                completion,
                workspace_name,
                session_name,
            }) => {
                ratatui::restore();
                return finalize_launch(completion, &workspace_name, &session_name);
            }
            Ok(SessionRunOutcome::OpenShell(dir)) => {
                ratatui::restore();
                return spawn_shell_at(&dir);
            }
            Err(e) => {
                ratatui::restore();
                return Err(e);
            }
        }
    }
}

/// Spawn the user's login shell at `dir` and wait for it to exit.
/// No mpx, no session — just `cd <dir> && exec $SHELL`. Used by the
/// `o` key in the session TUI and tree mode for "Open in Terminal"
/// behavior. Returns the shell's exit status as a Result; we
/// propagate failure up but exit 0 on a clean shell exit.
fn spawn_shell_at(dir: &std::path::Path) -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    eprintln!();
    eprintln!("  pa → shell at {}", dir.display());
    eprintln!("        exit the shell to return to your original terminal.");
    eprintln!();
    let status = std::process::Command::new(&shell)
        .current_dir(dir)
        .status()
        .with_context(|| format!("spawning shell {shell:?} at {}", dir.display()))?;
    if !status.success() {
        // Don't treat a non-zero shell exit as a pa error — the user
        // might have intentionally exited with a status code.
        tracing::debug!(
            target = "portagenty::tui",
            status = ?status,
            "shell exited non-zero"
        );
    }
    Ok(())
}

/// What the picker returned: a workspace to enter, a shell-out
/// request, or a quit. `None` (quit) is returned as `Quit` so
/// the call site can distinguish it from OpenShell cleanly.
enum PickResult {
    Workspace(crate::domain::Workspace),
    OpenShellAt(std::path::PathBuf),
    Quit,
}

/// Run the workspace picker and return the user's choice.
/// `Err` only for unexpected IO errors.
fn show_picker(terminal: &mut ratatui::DefaultTerminal) -> Result<PickResult> {
    let registered = crate::config::list_registered_workspaces().unwrap_or_default();
    let archived_set = crate::config::archived_workspaces().unwrap_or_default();
    // Partition into the default (active) list and the archived list.
    // Membership tests canonicalize so a path spelled differently at
    // registration still matches the archived set.
    let (mut archived, mut active): (Vec<_>, Vec<_>) = registered.into_iter().partition(|p| {
        let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
        archived_set.contains(&canon)
    });
    // Recency sort each bucket: workspaces with a recorded launch
    // come first, most-recent at the top; never-launched fall to the
    // bottom alphabetically. The "live sessions" sentinel is added by
    // the picker itself (active view only) and always trails.
    let recency_sort = |list: &mut Vec<std::path::PathBuf>| {
        list.sort_by(|a, b| {
            let ra = crate::state::last_launch_for_workspace(a);
            let rb = crate::state::last_launch_for_workspace(b);
            match (ra, rb) {
                (Some(x), Some(y)) => y.cmp(&x), // more recent first
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.file_name().cmp(&b.file_name()),
            }
        });
    };
    recency_sort(&mut active);
    recency_sort(&mut archived);
    match picker::run(terminal, &active, &archived)? {
        picker::PickerOutcome::Quit => Ok(PickResult::Quit),
        picker::PickerOutcome::LiveBrowse => {
            Ok(PickResult::Workspace(synthetic_browse_workspace()?))
        }
        picker::PickerOutcome::Workspace(path) => {
            let opts = LoadOptions {
                workspace_path: Some(path),
                ..Default::default()
            };
            Ok(PickResult::Workspace(load(&opts)?))
        }
        picker::PickerOutcome::OpenShellAt(dir) => Ok(PickResult::OpenShellAt(dir)),
    }
}

/// Outcome of a single session-TUI run, as seen by the outer driver.
enum SessionRunOutcome {
    /// Esc — caller should return to the picker (or quit if no picker).
    Back,
    /// q / Ctrl+C — caller should exit cleanly.
    Quit,
    /// Launch setup failed before an attach client returned. Re-open the same
    /// workspace and logical row instead of dropping the user at the shell.
    SetupFailed {
        workspace: crate::domain::Workspace,
        selection: RowSelectionIdentity,
        error: anyhow::Error,
    },
    /// A multiplexer client actually ran and returned, normally or abnormally.
    ClientReturned {
        completion: ClientCompletion<()>,
        workspace_name: String,
        session_name: String,
    },
    /// User pressed `o` — spawn a plain shell at this directory and
    /// exit pa when the shell exits.
    OpenShell(std::path::PathBuf),
}

#[cfg(target_os = "linux")]
fn receipts_for_workspace(
    workspace_id: Option<&str>,
    receipts: Vec<crate::supervision::BindingReceipt>,
) -> Vec<crate::supervision::BindingReceipt> {
    receipts
        .into_iter()
        .filter(|receipt| workspace_id == Some(receipt.logical_id.workspace_id.as_str()))
        .collect()
}

#[cfg(target_os = "linux")]
fn pending_for_workspace(
    workspace_id: Option<&str>,
    pending_launches: Vec<crate::supervision::PendingLaunch>,
) -> Vec<crate::supervision::PendingLaunch> {
    pending_launches
        .into_iter()
        .filter(|pending| workspace_id == Some(pending.logical_id.workspace_id.as_str()))
        .collect()
}

fn declared_selection(
    workspace: &crate::domain::Workspace,
    session_name: &str,
) -> RowSelectionIdentity {
    let logical_id = workspace.id.as_deref().and_then(|workspace_id| {
        crate::supervision::LogicalSessionId::new(workspace_id, session_name).ok()
    });
    RowSelectionIdentity::Declared {
        logical_id,
        session_name: session_name.to_string(),
    }
}

fn classify_launch_result(
    result: Result<ClientCompletion<()>>,
    workspace: crate::domain::Workspace,
    selection: RowSelectionIdentity,
    workspace_name: String,
    session_name: String,
) -> SessionRunOutcome {
    match result {
        Ok(completion) => SessionRunOutcome::ClientReturned {
            completion,
            workspace_name,
            session_name,
        },
        Err(error) => SessionRunOutcome::SetupFailed {
            workspace,
            selection,
            error,
        },
    }
}

/// Runs the session-list TUI against an already-initialized terminal.
/// Does *not* restore the terminal — caller owns init/restore so the
/// picker can share one ratatui session with the session list.
fn run_session_tui(
    terminal: &mut ratatui::DefaultTerminal,
    workspace: crate::domain::Workspace,
    resume_selection: Option<&RowSelectionIdentity>,
    resume_notice: Option<String>,
) -> Result<SessionRunOutcome> {
    let mpx_kind = workspace.multiplexer;
    let mux: Box<dyn crate::mux::Multiplexer> = match workspace.multiplexer {
        crate::domain::Multiplexer::Tmux => Box::new(TmuxAdapter::new()),
        crate::domain::Multiplexer::Zellij => {
            if crate::mux::ZellijAdapter::is_inside_zellij() {
                let cur =
                    std::env::var("ZELLIJ_SESSION_NAME").unwrap_or_else(|_| "<unknown>".into());
                // Restore before bailing so the user sees the message
                // on their shell, not over the ratatui buffer.
                ratatui::restore();
                anyhow::bail!(
                    "refusing to open the TUI: you're already inside zellij session {cur:?}.\n\
                     zellij can't attach to another session from within a client.\n\
                     Detach first (Ctrl+O then d by default), then run `pa` again.\n\
                     The global picker will show your existing live sessions."
                );
            }
            Box::new(crate::mux::ZellijAdapter::new())
        }
        crate::domain::Multiplexer::Wezterm => {
            anyhow::bail!("the wezterm multiplexer adapter is not implemented yet (v1.x)")
        }
    };

    let live = mux.list_sessions().unwrap_or_default();
    let mut app = App::new(workspace, mux, live);
    #[cfg(target_os = "linux")]
    {
        let current_boot_id = crate::supervision::linux_systemd::read_current_boot_id().ok();
        app = app.with_current_boot_id(current_boot_id);
        match crate::supervision::ReceiptStore::standard().and_then(|store| store.load()) {
            Ok(file) => {
                let receipts = receipts_for_workspace(app.workspace_id(), file.bindings);
                let pending = pending_for_workspace(app.workspace_id(), file.pending_launches);
                app = app.with_supervision_evidence(receipts, pending);
            }
            Err(error) => app.fail_closed_supervision(format!(
                "ownership and pending-launch evidence unavailable; idle supervision is fail-closed: {error:#}"
            )),
        }
    }
    if let Some(selection) = resume_selection {
        app.restore_selection(selection);
    }
    if let Some(notice) = resume_notice {
        app.set_persistent_status(notice);
    }
    let AppRunResult {
        outcome,
        mux,
        workspace,
    } = app.run(terminal)?;
    let workspace_file = workspace.file_path.clone();
    let workspace_name = workspace.name.clone();
    let workspace_for_resume = workspace.clone();
    let workspace_for_supervision = workspace;

    let mode = crate::mux::AttachMode::Takeover;
    Ok(match outcome {
        AppOutcome::Back => SessionRunOutcome::Back,
        AppOutcome::Quit => SessionRunOutcome::Quit,
        AppOutcome::Launch(LaunchKind::Create { session, mpx_name }) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &session.name);
            }
            // Restore terminal so the mpx takes a clean tty and the
            // pre-launch banner prints to the user's real shell.
            ratatui::restore();
            print_launch_banner(mpx_kind, &session.name);
            let session_name = session.name.clone();
            classify_launch_result(
                mux.create_and_attach(&session, &mpx_name, mode)
                    .map(|completion| completion.map(|_| ())),
                workspace_for_resume.clone(),
                declared_selection(&workspace_for_resume, &session_name),
                workspace_name,
                session_name,
            )
        }
        AppOutcome::Launch(LaunchKind::CreateSupervised {
            session,
            limits,
            intent,
        }) => {
            ratatui::restore();
            print_launch_banner(mpx_kind, &session.name);
            let session_name = session.name.clone();
            let result = match intent {
                SupervisionIntent::RoutineEnter => {
                    match crate::cli::launch_supervised_routine_resolved(
                        session.clone(),
                        workspace_for_supervision,
                        mode,
                        limits.clone(),
                    ) {
                        #[cfg(target_os = "linux")]
                        Ok(crate::cli::RoutineSupervisedLaunch::ClientReturned(completion)) => {
                            Ok(completion)
                        }
                        Ok(crate::cli::RoutineSupervisedLaunch::FallbackSafe(reason)) => {
                            print_supervision_fallback_notice(
                                &workspace_name,
                                &session_name,
                                &limits,
                                &reason,
                            );
                            if let Some(path) = &workspace_file {
                                let _ = crate::state::record_launch(path, &session.name);
                            }
                            let mpx_name =
                                crate::mux::workspace_session_name(&workspace_name, &session.name);
                            mux.create_and_attach(&session, &mpx_name, mode)
                                .map(|completion| completion.map(|_| ()))
                                .map_err(|ordinary_error| {
                                    anyhow::anyhow!(
                                        "supervision preflight was unavailable ({reason:#}); ordinary fallback also failed: {ordinary_error:#}"
                                    )
                                })
                        }
                        Err(error) => Err(error),
                    }
                }
                SupervisionIntent::ExplicitCustom => crate::cli::launch_supervised_resolved(
                    session,
                    workspace_for_supervision,
                    false,
                    mode,
                    false,
                    limits,
                ),
                SupervisionIntent::StaleReplacement => Err(anyhow::anyhow!(
                    "stale replacement requires an exact receipt"
                )),
            };
            classify_launch_result(
                result,
                workspace_for_resume.clone(),
                declared_selection(&workspace_for_resume, &session_name),
                workspace_name,
                session_name,
            )
        }
        AppOutcome::Launch(LaunchKind::ReplaceStaleSupervised {
            session,
            receipt,
            limits,
            prior_boot_relaunch,
        }) => {
            ratatui::restore();
            if prior_boot_relaunch {
                eprintln!();
                eprintln!(
                    "  pa: revalidating the exact prior-boot stale binding and relaunching without signalling the old workload."
                );
            }
            print_launch_banner(mpx_kind, &session.name);
            let session_name = session.name.clone();
            classify_launch_result(
                crate::cli::replace_stale_supervised_resolved(
                    session,
                    workspace_for_supervision,
                    *receipt,
                    mode,
                    limits,
                ),
                workspace_for_resume.clone(),
                declared_selection(&workspace_for_resume, &session_name),
                workspace_name,
                session_name,
            )
        }
        AppOutcome::Launch(LaunchKind::Attach {
            mpx_name,
            display_name,
        }) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &display_name);
            }
            ratatui::restore();
            print_launch_banner(mpx_kind, &display_name);
            let selection = if workspace_for_resume
                .sessions
                .iter()
                .any(|session| session.name == display_name)
            {
                declared_selection(&workspace_for_resume, &display_name)
            } else {
                RowSelectionIdentity::Untracked {
                    mpx_name: mpx_name.clone(),
                }
            };
            classify_launch_result(
                mux.attach(&mpx_name, mode),
                workspace_for_resume.clone(),
                selection,
                workspace_name,
                display_name,
            )
        }
        AppOutcome::Launch(LaunchKind::AttachOwned {
            target,
            display_name,
        }) => {
            if let Some(path) = &workspace_file {
                let _ = crate::state::record_launch(path, &display_name);
            }
            ratatui::restore();
            print_launch_banner(mpx_kind, &display_name);
            classify_launch_result(
                crate::cli::attach_receipted_target(&target, mode),
                workspace_for_resume.clone(),
                declared_selection(&workspace_for_resume, &display_name),
                workspace_name,
                display_name,
            )
        }
        AppOutcome::OpenShellAt(dir) => SessionRunOutcome::OpenShell(dir),
    })
}

fn supervision_fallback_notice(
    workspace_name: &str,
    session_name: &str,
    limits: &crate::supervision::ResourceLimits,
    reason: &anyhow::Error,
) -> String {
    let memory = limits
        .memory_high_bytes
        .map(|bytes| format!("{:.0} GiB", bytes as f64 / 1024_f64.powi(3)))
        .unwrap_or_else(|| "unset".into());
    let memory_max = limits
        .memory_max_bytes
        .map(|bytes| format!("{:.0} GiB", bytes as f64 / 1024_f64.powi(3)))
        .unwrap_or_else(|| "unset".into());
    let swap_max = limits
        .memory_swap_max_bytes
        .map(|bytes| format!("{:.0} MiB", bytes as f64 / 1024_f64.powi(2)))
        .unwrap_or_else(|| "unset".into());
    let cpu = limits
        .cpu_quota_percent
        .map(|value| format!("{value}%"))
        .unwrap_or_else(|| "unset".into());
    let tasks = limits
        .tasks_max
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unset".into());
    format!(
        "  pa: RESOURCE SUPERVISION UNAVAILABLE\n      launching {:?} ordinarily without cgroup resource limits\n      omitted limits: MemoryHigh {memory} · MemoryMax {memory_max} · SwapMax {swap_max} · CPU {cpu} · TasksMax {tasks}\n      reason: {reason:#}",
        format!("{workspace_name} / {session_name}")
    )
}

fn print_supervision_fallback_notice(
    workspace_name: &str,
    session_name: &str,
    limits: &crate::supervision::ResourceLimits,
    reason: &anyhow::Error,
) {
    eprintln!();
    eprintln!(
        "{}",
        supervision_fallback_notice(workspace_name, session_name, limits, reason)
    );
    eprintln!();
}

fn finalize_launch(
    completion: ClientCompletion<()>,
    workspace_name: &str,
    session_name: &str,
) -> Result<()> {
    crate::cli::finish_client_return(completion, workspace_name, session_name)
}

/// Build a synthetic empty workspace so the TUI can render
/// live-multiplexer sessions even when no `*.portagenty.toml` is
/// reachable from the current directory. Picks the machine-default
/// multiplexer if set; otherwise prefers zellij if installed, else
/// tmux. Returns an error if neither mpx is installed — at that
/// point there's literally nothing to show.
fn synthetic_browse_workspace() -> Result<crate::domain::Workspace> {
    use crate::domain::Multiplexer;
    let mpx = crate::config::current_default_multiplexer()
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            // No pinned default — probe PATH. Fall through to tmux
            // when neither is present; the mpx adapter will surface a
            // friendlier "not installed" error at list_sessions time.
            if bin_on_path("zellij") {
                Multiplexer::Zellij
            } else {
                Multiplexer::Tmux
            }
        });
    Ok(crate::domain::Workspace {
        name: "(no workspace — live sessions)".into(),
        id: None,
        file_path: None,
        multiplexer: mpx,
        projects: vec![],
        sessions: vec![],
        tags: vec![],
    })
}

fn bin_on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|d| d.join(bin).is_file())
}

/// Print a one-line hand-off banner just before we replace the TUI
/// with the multiplexer client. Tells the user which session they're
/// entering and the mpx-specific detach chord they'll need to get
/// back out. Keeping this info local to `pa` (no keybind rebinding,
/// no config mutation) means we don't couple to any specific zellij
/// or tmux version's defaults — users with custom configs just ignore
/// the hint and use their own chord.
fn print_launch_banner(mpx: crate::domain::Multiplexer, session: &str) {
    let mpx_name = match mpx {
        crate::domain::Multiplexer::Tmux => "tmux",
        crate::domain::Multiplexer::Zellij => "zellij",
        crate::domain::Multiplexer::Wezterm => "wezterm",
    };
    eprintln!();
    eprintln!("  pa → {mpx_name} session \"{session}\"");
    match mpx {
        crate::domain::Multiplexer::Tmux => {
            eprintln!("        detach: Ctrl+B then d   ·   re-attach: pa claim {session}");
        }
        crate::domain::Multiplexer::Zellij => {
            // Zellij's Ctrl+Q is *quit with confirmation overlay*, NOT
            // detach. Users keep hitting it and landing on the
            // "Do you really want to quit zellij?" modal. Spell out
            // the right chord (Ctrl+O then d) and explicitly warn
            // against Ctrl+Q so the muscle-memory mistake is avoided.
            eprintln!("        detach: Ctrl+O then d   ·   re-attach: pa claim {session}");
            eprintln!("        NOTE: Ctrl+Q opens zellij's quit-confirmation overlay,");
            eprintln!("              NOT a detach. Use Ctrl+O then d to leave without killing.");
        }
        crate::domain::Multiplexer::Wezterm => {
            eprintln!("        detach: see wezterm docs");
        }
    }
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_pa_starts_at_global_picker() {
        assert_eq!(initial_screen(false), InitialScreen::Picker);
    }

    #[test]
    fn explicit_path_starts_in_resolved_workspace() {
        assert_eq!(initial_screen(true), InitialScreen::Workspace);
    }

    #[cfg(target_os = "linux")]
    fn receipt(workspace_id: &str, session_name: &str) -> crate::supervision::BindingReceipt {
        crate::supervision::BindingReceipt {
            schema_version: crate::supervision::model::LEGACY_RECEIPT_SCHEMA_VERSION,
            logical_id: crate::supervision::LogicalSessionId::new(workspace_id, session_name)
                .unwrap(),
            backend: crate::supervision::BackendKind::SystemdUserService,
            unit_name: "portagenty-wtest.service".into(),
            invocation_id: "00112233445566778899aabbccddeeff".into(),
            control_group: "/user.slice/user-1000.slice/user@1000.service/app.slice/test.service"
                .into(),
            mux_target: crate::supervision::MuxTarget::TmuxPrivate {
                socket: std::path::PathBuf::from("/run/user/1000/portagenty/test/tmux.sock"),
                session: session_name.into(),
            },
            observed_at_unix_ms: 1,
            limits: crate::supervision::ResourceLimits::default(),
            session_kind: None,
            requested_slice: None,
            workload_anchor: None,
            launch_boot_id: None,
        }
    }

    #[test]
    fn loud_fallback_notice_names_identity_and_omitted_limits() {
        let notice = supervision_fallback_notice(
            "workspace",
            "shell",
            &crate::supervision::ResourceLimits::claude_defaults(),
            &anyhow::anyhow!("systemd unavailable"),
        );
        assert!(notice.contains("RESOURCE SUPERVISION UNAVAILABLE"));
        assert!(notice.contains("workspace / shell"));
        assert!(notice.contains("ordinarily without cgroup resource limits"));
        assert!(notice.contains("MemoryHigh 3 GiB"));
        assert!(notice.contains("MemoryMax 5 GiB"));
        assert!(notice.contains("SwapMax 512 MiB"));
        assert!(notice.contains("CPU 800%"));
        assert!(notice.contains("TasksMax 1200"));
        assert!(notice.contains("systemd unavailable"));
    }

    #[test]
    fn launch_setup_failure_keeps_workspace_and_selection_for_resume() {
        let workspace = crate::domain::Workspace {
            name: "workspace".into(),
            id: None,
            multiplexer: crate::domain::Multiplexer::Tmux,
            file_path: None,
            sessions: Vec::new(),
            projects: Vec::new(),
            tags: Vec::new(),
        };
        let selection = RowSelectionIdentity::Untracked {
            mpx_name: "workspace-shell".into(),
        };
        match classify_launch_result(
            Err(anyhow::anyhow!("client never spawned")),
            workspace,
            selection.clone(),
            "workspace".into(),
            "shell".into(),
        ) {
            SessionRunOutcome::SetupFailed {
                workspace,
                selection: actual,
                error,
            } => {
                assert_eq!(workspace.name, "workspace");
                assert_eq!(actual, selection);
                assert!(format!("{error:#}").contains("client never spawned"));
            }
            _ => panic!("expected resumable setup failure"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn session_tui_receipts_are_scoped_to_the_current_workspace_uuid() {
        let current = "550e8400-e29b-41d4-a716-446655440000";
        let other = "123e4567-e89b-12d3-a456-426614174000";
        let filtered = receipts_for_workspace(
            Some(current),
            vec![receipt(current, "kept"), receipt(other, "excluded")],
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].logical_id.session_name, "kept");
        assert!(receipts_for_workspace(None, vec![receipt(current, "excluded")]).is_empty());
    }
}
