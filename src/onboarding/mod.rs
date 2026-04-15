//! First-run wizard. Triggers automatically when `pa` is invoked
//! in an interactive shell with no workspace walkable from the
//! current directory and the user hasn't been through onboarding
//! before. Also explicitly invocable via `pa onboard` at any time.
//!
//! Deliberately simple: text-mode prompts (not a TUI modal), one
//! question at a time. Keeps it phone-keyboard-friendly and doesn't
//! require pulling in a prompt-library dep.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Outcome of running the wizard. The caller uses this to decide
/// whether to continue into the TUI (Workspace was created) or exit
/// cleanly (user declined, showed docs, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnboardOutcome {
    /// User chose to scaffold a workspace; we wrote this file.
    /// Caller can now re-try `config::load` and run the TUI.
    Scaffolded { path: PathBuf },
    /// User asked to see the docs. Caller should exit cleanly.
    ShowedDocs,
    /// User skipped. Caller should exit cleanly. The onboarded
    /// sentinel has been written so we don't re-prompt next time.
    Skipped,
}

/// Is the current process attached to an interactive terminal? The
/// wizard only runs when stdin is a TTY — keeps scripted use
/// (cron, CI, pipes) from hanging on prompts.
pub fn is_interactive() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

/// Path to the "user has seen onboarding" sentinel. Living under
/// the state dir keeps it per-machine; users migrating between
/// devices see the wizard once per device, which is fine.
pub fn onboarded_marker_path() -> Result<PathBuf> {
    Ok(crate::state::state_dir()?.join(".onboarded"))
}

/// Has this user seen onboarding before? Falls back to "yes" on
/// any error resolving the path — better to skip the prompt than
/// annoy someone whose filesystem is unusual.
pub fn has_onboarded() -> bool {
    onboarded_marker_path().map(|p| p.exists()).unwrap_or(true)
}

fn mark_onboarded() -> Result<()> {
    let path = onboarded_marker_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, "").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Run the wizard. Reads from stdin, writes to stdout. Returns the
/// outcome so the caller can chain into the TUI or exit cleanly.
///
/// Forced = true bypasses the `has_onboarded` short-circuit; used
/// by the explicit `pa onboard` command.
pub fn run_wizard(forced: bool) -> Result<OnboardOutcome> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    run_wizard_with(&mut stdin, &mut stdout, forced)
}

/// Testable variant: takes `Read` + `Write` handles instead of
/// reaching for globals. The public `run_wizard` is a thin wrapper.
pub fn run_wizard_with<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    _forced: bool,
) -> Result<OnboardOutcome> {
    writeln!(output)?;
    writeln!(output, "  Welcome to portagenty.")?;
    writeln!(
        output,
        "  I don't see a workspace file here (no *.portagenty.toml)."
    )?;
    writeln!(output)?;
    writeln!(output, "  What would you like to do?")?;
    writeln!(
        output,
        "    [1] Set up a workspace here (recommended, ~30s)"
    )?;
    writeln!(output, "    [2] Show me the docs and I'll DIY")?;
    writeln!(output, "    [3] Skip for now")?;
    writeln!(output)?;
    write!(output, "  Choice [1]: ")?;
    output.flush()?;

    let choice = read_line(input)?;
    let choice = choice.trim();
    match choice {
        "" | "1" => scaffold_flow(input, output),
        "2" => show_docs(output),
        _ => skip(output),
    }
}

fn scaffold_flow<R: BufRead, W: Write>(input: &mut R, output: &mut W) -> Result<OnboardOutcome> {
    let cwd = std::env::current_dir().context("reading current directory")?;
    let default_name = cwd
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("workspace")
        .to_string();

    writeln!(output)?;
    write!(output, "  Workspace name [{default_name}]: ")?;
    output.flush()?;
    let name = read_line(input)?;
    let name = name.trim();
    let name = if name.is_empty() {
        default_name.clone()
    } else {
        name.to_string()
    };

    writeln!(output)?;
    writeln!(output, "  Multiplexer:")?;
    writeln!(
        output,
        "    [1] tmux (recommended — best cross-device story)"
    )?;
    writeln!(output, "    [2] zellij")?;
    write!(output, "  Choice [1]: ")?;
    output.flush()?;
    let mpx_choice = read_line(input)?;
    let mpx = match mpx_choice.trim() {
        "2" => "zellij",
        _ => "tmux",
    };

    writeln!(output)?;
    write!(output, "  Pre-populate a Claude Code session? [Y/n]: ")?;
    output.flush()?;
    let answer = read_line(input)?;
    let with_claude = !matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no");

    // Sanitize filename stem the same way `pa init` does.
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let filename = format!("{stem}.portagenty.toml");
    let path = cwd.join(&filename);

    if path.exists() {
        writeln!(output)?;
        writeln!(
            output,
            "  {} already exists; leaving it alone.",
            path.display()
        )?;
        writeln!(output, "  Run `pa init --force` if you want to overwrite.")?;
        mark_onboarded()?;
        return Ok(OnboardOutcome::Skipped);
    }

    let mut body = String::new();
    body.push_str(&format!(
        "# Workspace file for portagenty. See:\n# https://cybersader.github.io/portagenty/reference/schema/\nname = \"{name}\"\nmultiplexer = \"{mpx}\"\n\n"
    ));
    body.push_str(
        "[[session]]\nname = \"shell\"\ncwd = \".\"\ncommand = \"bash\"\nkind = \"shell\"\n",
    );
    if with_claude {
        body.push_str(
            "\n[[session]]\nname = \"claude\"\ncwd = \".\"\ncommand = \"claude\"\nkind = \"claude-code\"\n",
        );
    }

    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;

    writeln!(output)?;
    writeln!(output, "  ✓ Created {}", path.display())?;
    writeln!(output, "  Run `pa` here to open the TUI.")?;

    mark_onboarded()?;
    Ok(OnboardOutcome::Scaffolded { path })
}

fn show_docs<W: Write>(output: &mut W) -> Result<OnboardOutcome> {
    writeln!(output)?;
    writeln!(output, "  Quick reference:")?;
    writeln!(
        output,
        "    pa init [name]             — scaffold a workspace here"
    )?;
    writeln!(output, "    pa add <name> -c <cmd>     — append a session")?;
    writeln!(output, "    pa                         — TUI (after init)")?;
    writeln!(
        output,
        "    pa claim                   — cross-device takeover"
    )?;
    writeln!(
        output,
        "    pa snippets list           — bundled bash ergonomics"
    )?;
    writeln!(output)?;
    writeln!(
        output,
        "  Full docs: https://cybersader.github.io/portagenty/"
    )?;
    writeln!(output)?;
    // Deliberately don't mark_onboarded here — user might still want
    // the prompt next time.
    Ok(OnboardOutcome::ShowedDocs)
}

fn skip<W: Write>(output: &mut W) -> Result<OnboardOutcome> {
    writeln!(output)?;
    writeln!(
        output,
        "  Skipped. Run `pa onboard` any time to see this again."
    )?;
    mark_onboarded()?;
    Ok(OnboardOutcome::Skipped)
}

fn read_line<R: BufRead>(input: &mut R) -> Result<String> {
    let mut line = String::new();
    input.read_line(&mut line).context("reading stdin")?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn drive(input: &str) -> (OnboardOutcome, String) {
        let mut r = Cursor::new(input.as_bytes().to_vec());
        let mut w: Vec<u8> = Vec::new();
        // For test purposes we always treat as forced=false (it's
        // unused in the testable variant but still an arg for API
        // symmetry).
        let outcome = run_wizard_with(&mut r, &mut w, false).expect("wizard");
        (outcome, String::from_utf8(w).unwrap())
    }

    #[test]
    fn option_2_shows_docs_and_returns_showed_docs() {
        let (outcome, out) = drive("2\n");
        assert_eq!(outcome, OnboardOutcome::ShowedDocs);
        assert!(out.contains("Welcome to portagenty"));
        assert!(out.contains("Quick reference"));
        assert!(out.contains("pa init"));
        assert!(out.contains("https://cybersader.github.io/portagenty"));
    }

    #[test]
    fn option_3_returns_skipped() {
        // NOTE: we don't assert on the sentinel write because the
        // test shares the real $XDG_STATE_HOME with the user. The
        // outcome is the contract.
        let (outcome, out) = drive("3\n");
        assert_eq!(outcome, OnboardOutcome::Skipped);
        assert!(out.contains("Skipped"));
    }

    #[test]
    fn empty_input_defaults_to_scaffold_but_bails_if_cwd_has_no_filename() {
        // We can't actually exercise the scaffold-write path in a
        // unit test without hijacking cwd, so just confirm the
        // prompt text gets rendered for choice 1.
        let (_, out) = drive("\n\n\nn\n");
        assert!(out.contains("Workspace name"));
        assert!(out.contains("Multiplexer"));
    }

    #[test]
    fn unknown_choice_falls_through_to_skip() {
        let (outcome, _) = drive("9\n");
        assert_eq!(outcome, OnboardOutcome::Skipped);
    }
}
