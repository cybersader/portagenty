//! Per-OS registration for the `pa://` URL protocol.
//!
//! Three layers:
//! 1. **Terminal detection** — find a sensible emulator on PATH and
//!    build the command template that hosts `pa open <url>` inside it.
//! 2. **Show** — print the OS-appropriate registration snippet so the
//!    user can read/apply it by hand. Always safe, never writes.
//! 3. **Install / uninstall** — actually write the file (`.desktop`)
//!    or registry entry. Linux + WSL + Windows natively supported;
//!    macOS falls back to `show` for now (the .app-bundle dance is
//!    beyond a one-file helper).

use anyhow::{anyhow, Context, Result};
use std::fmt;
use std::path::{Path, PathBuf};

/// A terminal emulator the registration can target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terminal {
    /// Short user-facing name ("Windows Terminal", "Alacritty", etc.).
    pub name: String,
    /// Absolute path or PATH-resolvable binary name.
    pub binary: PathBuf,
    /// Platform this entry belongs to — "windows", "linux", "macos".
    /// Filters detection results to the current OS.
    pub platform: &'static str,
    /// Argv template. `{cmd}` is substituted with the full command
    /// string to run inside the terminal (space-joined `argv[0] args`).
    ///
    /// Example for Windows Terminal: `["--", "{cmd}"]` so the final
    /// argv is `wt.exe -- pa open pa://open/...`. Values that need
    /// shell interpretation go through `sh -c` first.
    pub args_template: Vec<String>,
    /// If true, the template expects the command split into separate
    /// argv tokens (we'll append them to `args_template`). If false,
    /// the template will have `{cmd}` substituted with one joined
    /// string — use this for emulators that take `-e "cmd args"`.
    pub split_args: bool,
}

impl fmt::Display for Terminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.name, self.binary.display())
    }
}

/// Detect terminal emulators available on the current OS's PATH.
/// Returns the highest-priority match first. Empty if none found.
///
/// On WSL, prefers Windows terminals (wt.exe etc.) since clicks on
/// `pa://` links typically originate from a Windows browser — the
/// URL scheme lives in the Windows registry, not the Linux
/// desktop database.
pub fn detect_terminals() -> Vec<Terminal> {
    let mut out = Vec::new();
    let on_wsl = crate::find::is_wsl();
    for cand in candidates_for_env(on_wsl) {
        if let Some(bin) = which_with_extra_paths(&cand.probe_bin, on_wsl) {
            out.push(Terminal {
                name: cand.name.to_string(),
                binary: bin,
                platform: cand.platform,
                args_template: cand.args_template.iter().map(|s| s.to_string()).collect(),
                split_args: cand.split_args,
            });
        }
    }
    out
}

/// Like `candidates()` but expands on WSL to include the Windows set
/// first, so `wt.exe` is the default registration target when the
/// user is running pa from inside WSL.
fn candidates_for_env(on_wsl: bool) -> Vec<Candidate> {
    if on_wsl {
        // Offer Windows terminals first (the typical WSL pattern:
        // register the handler on Windows, have it invoke wsl pa),
        // then fall back to Linux terminals if the user prefers.
        let current = candidates();
        let windows = all_candidates()
            .into_iter()
            .filter(|c| c.platform == "windows");
        windows.chain(current.into_iter()).collect()
    } else {
        candidates()
    }
}

/// Are we the current OS that `show_snippet` and `install` use? On
/// WSL we override to "windows" so the handler lands in the Windows
/// registry. Native Linux stays "linux".
fn effective_os() -> &'static str {
    if crate::find::is_wsl() {
        "windows"
    } else {
        std::env::consts::OS
    }
}

/// Given a detected terminal and the URL action command the user
/// wants wrapped, build the final argv that the OS handler will
/// invoke when the user clicks a `pa://` link.
///
/// On WSL with a Windows-side terminal (e.g. wt.exe), the Linux pa
/// binary can't be invoked directly — we wrap it in `wsl.exe -e`
/// so the Windows terminal shells out to WSL first.
pub fn build_invocation(term: &Terminal, pa_binary: &Path, url_placeholder: &str) -> Vec<String> {
    let on_wsl = crate::find::is_wsl();
    // When the terminal is Windows-side (platform = windows) but we
    // ARE on WSL, pa is a Linux binary and needs wsl.exe in front.
    let wsl_wrap = on_wsl && term.platform == "windows";

    // Inner command as separate tokens — pa + "open" + url.
    let inner_tokens: Vec<String> = if wsl_wrap {
        vec![
            "wsl.exe".into(),
            "-e".into(),
            pa_binary.display().to_string(),
            "open".into(),
            url_placeholder.to_string(),
        ]
    } else {
        vec![
            pa_binary.display().to_string(),
            "open".into(),
            url_placeholder.to_string(),
        ]
    };
    // Joined form for templates that take a single-string `{cmd}`.
    let inner_joined = inner_tokens
        .iter()
        .map(|t| shell_quote(Path::new(t)))
        .collect::<Vec<_>>()
        .join(" ");

    let mut argv = vec![term.binary.display().to_string()];
    if term.split_args {
        for tpl in &term.args_template {
            if tpl == "{cmd}" {
                argv.extend(inner_tokens.iter().cloned());
            } else {
                argv.push(tpl.clone());
            }
        }
    } else {
        for tpl in &term.args_template {
            argv.push(tpl.replace("{cmd}", &inner_joined));
        }
    }
    argv
}

/// Pretty-print a registration snippet for the current OS. Safe and
/// read-only — lets the user copy-paste to apply. On WSL, emits
/// Windows-side (.reg) snippets since that's where the OS scheme
/// handler lives for WSL users.
pub fn show_snippet(term: &Terminal, pa_binary: &Path) -> Result<String> {
    match effective_os() {
        "linux" => Ok(build_desktop_entry(term, pa_binary)),
        "windows" => Ok(build_windows_reg(term, pa_binary)),
        "macos" => Ok(build_macos_guidance(term, pa_binary)),
        other => Err(anyhow!("unsupported OS: {other}")),
    }
}

/// Actually install the registration. Returns the path/key that was
/// written so the caller can display it.
pub fn install(term: &Terminal, pa_binary: &Path) -> Result<String> {
    match effective_os() {
        "linux" => install_linux(term, pa_binary),
        "windows" => install_windows(term, pa_binary),
        "macos" => Err(anyhow!(
            "macOS install isn't automated yet — run `pa protocol show` and apply the guidance manually"
        )),
        other => Err(anyhow!("unsupported OS: {other}")),
    }
}

/// Remove a previously-installed registration.
pub fn uninstall() -> Result<String> {
    match effective_os() {
        "linux" => uninstall_linux(),
        "windows" => uninstall_windows(),
        "macos" => Err(anyhow!(
            "macOS uninstall isn't automated yet — remove the handler manually"
        )),
        other => Err(anyhow!("unsupported OS: {other}")),
    }
}

// ─── Linux ───────────────────────────────────────────────────────────────

fn linux_desktop_path() -> Result<PathBuf> {
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".local/share"))
        })
        .ok_or_else(|| anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(data_home.join("applications/portagenty.desktop"))
}

fn build_desktop_entry(term: &Terminal, pa_binary: &Path) -> String {
    let argv = build_invocation(term, pa_binary, "%u");
    // .desktop files want a shell-safe Exec line. Quote argv tokens
    // that contain whitespace or special chars.
    let exec_line = argv
        .iter()
        .map(|s| shell_quote(Path::new(s)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=portagenty\n\
         Comment=Open pa:// workspace URLs\n\
         Exec={exec_line}\n\
         NoDisplay=true\n\
         MimeType=x-scheme-handler/pa;\n\
         Terminal=false\n"
    )
}

fn install_linux(term: &Terminal, pa_binary: &Path) -> Result<String> {
    let path = linux_desktop_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, build_desktop_entry(term, pa_binary))
        .with_context(|| format!("writing {}", path.display()))?;
    // Best-effort: update the mimeapps scheme handler list so the
    // desktop environment picks up the new .desktop immediately.
    // Failures here aren't fatal — the .desktop file itself is
    // sufficient; `update-desktop-database` etc. are just caches.
    let _ = std::process::Command::new("xdg-mime")
        .args([
            "default",
            "portagenty.desktop",
            "x-scheme-handler/pa",
        ])
        .status();
    Ok(path.display().to_string())
}

fn uninstall_linux() -> Result<String> {
    let path = linux_desktop_path()?;
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing {}", path.display()))?;
        Ok(path.display().to_string())
    } else {
        Ok(format!("(nothing to remove at {})", path.display()))
    }
}

// ─── Windows ─────────────────────────────────────────────────────────────

fn build_windows_reg(term: &Terminal, pa_binary: &Path) -> String {
    let argv = build_invocation(term, pa_binary, "%1");
    // .reg escaping: quotes become \", backslashes become \\.
    let cmd_line = argv
        .iter()
        .map(|a| format!("\\\"{}\\\"", a.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "Windows Registry Editor Version 5.00\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\pa]\n\
         @=\"URL:portagenty\"\n\
         \"URL Protocol\"=\"\"\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\pa\\shell]\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\pa\\shell\\open]\n\
         \n\
         [HKEY_CURRENT_USER\\Software\\Classes\\pa\\shell\\open\\command]\n\
         @=\"{cmd_line}\"\n"
    )
}

fn install_windows(term: &Terminal, pa_binary: &Path) -> Result<String> {
    // Use reg.exe for the HKCU write — no admin needed. Works from
    // both native Windows and WSL (WSL can invoke reg.exe via cmd.exe
    // or directly if PATH includes /mnt/c/Windows/system32).
    let argv = build_invocation(term, pa_binary, "%1");
    let quoted = argv
        .iter()
        .map(|a| format!("\"{}\"", a.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ");

    let reg = find_reg_exe()
        .ok_or_else(|| anyhow!("reg.exe not found on PATH (are you on Windows/WSL?)"))?;

    // Root scheme key.
    run(&reg, &[
        "add", "HKCU\\Software\\Classes\\pa", "/ve",
        "/d", "URL:portagenty", "/f",
    ])?;
    run(&reg, &[
        "add", "HKCU\\Software\\Classes\\pa",
        "/v", "URL Protocol", "/d", "", "/f",
    ])?;
    // shell/open/command value.
    run(&reg, &[
        "add", "HKCU\\Software\\Classes\\pa\\shell\\open\\command", "/ve",
        "/d", &quoted, "/f",
    ])?;
    Ok("HKCU\\Software\\Classes\\pa".into())
}

fn uninstall_windows() -> Result<String> {
    let reg = find_reg_exe()
        .ok_or_else(|| anyhow!("reg.exe not found on PATH"))?;
    let status = std::process::Command::new(&reg)
        .args(["delete", "HKCU\\Software\\Classes\\pa", "/f"])
        .status()
        .with_context(|| format!("invoking {}", reg.display()))?;
    if !status.success() {
        // Missing key = "nothing to uninstall", not an error.
        return Ok("(nothing registered under HKCU\\Software\\Classes\\pa)".into());
    }
    Ok("HKCU\\Software\\Classes\\pa".into())
}

fn find_reg_exe() -> Option<PathBuf> {
    // reg.exe is under %windir%\System32 on Windows. On WSL it's
    // reachable via /mnt/c/Windows/System32/reg.exe when the Windows
    // PATH is inherited (it is, by default).
    if let Some(p) = which("reg.exe") {
        return Some(p);
    }
    if let Some(p) = which("reg") {
        return Some(p);
    }
    // Fallback guess — common WSL path.
    let wsl_path = PathBuf::from("/mnt/c/Windows/System32/reg.exe");
    if wsl_path.is_file() {
        return Some(wsl_path);
    }
    None
}

fn run(exe: &Path, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new(exe)
        .args(args)
        .output()
        .with_context(|| format!("invoking {} {:?}", exe.display(), args))?;
    if !out.status.success() {
        return Err(anyhow!(
            "{} {:?} failed: {}",
            exe.display(),
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

// ─── macOS (guidance only for now) ───────────────────────────────────────

fn build_macos_guidance(term: &Terminal, pa_binary: &Path) -> String {
    let argv = build_invocation(term, pa_binary, "%URL%");
    let exec_line = argv
        .iter()
        .map(|s| shell_quote(Path::new(s)))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "macOS URL scheme registration requires a .app bundle with a\n\
         CFBundleURLTypes entry. The one-file dance:\n\
         \n\
         1. Create ~/Applications/pa-protocol.app/Contents/ with an\n\
            Info.plist that registers 'pa' as a URL scheme.\n\
         2. The Info.plist's LSHandler invokes this command:\n\
         \n\
         {exec_line}\n\
         \n\
         (replace %URL% with the full URL at launch time).\n\
         \n\
         Automation of this is on the roadmap. For now, prefer\n\
         invoking 'pa open <url>' from shell aliases or Raycast\n\
         script commands.\n"
    )
}

// ─── Terminal candidate list ────────────────────────────────────────────

struct Candidate {
    name: &'static str,
    probe_bin: &'static str,
    platform: &'static str,
    args_template: &'static [&'static str],
    split_args: bool,
}

fn candidates() -> Vec<Candidate> {
    let current = std::env::consts::OS;
    all_candidates()
        .into_iter()
        .filter(|c| c.platform == current)
        .collect()
}

fn all_candidates() -> Vec<Candidate> {
    vec![
        // Windows — highest priority first.
        Candidate {
            name: "Windows Terminal",
            probe_bin: "wt.exe",
            platform: "windows",
            args_template: &["--", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "ConEmu",
            probe_bin: "ConEmu64.exe",
            platform: "windows",
            args_template: &["-run", "{cmd}"],
            split_args: false,
        },
        Candidate {
            name: "Alacritty (Windows)",
            probe_bin: "alacritty.exe",
            platform: "windows",
            args_template: &["-e", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "WezTerm (Windows)",
            probe_bin: "wezterm.exe",
            platform: "windows",
            args_template: &["start", "--", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "Command Prompt",
            probe_bin: "cmd.exe",
            platform: "windows",
            args_template: &["/c", "start", "", "{cmd}"],
            split_args: false,
        },
        // Linux — try user-configured $TERMINAL first via a special
        // probe, then common ones.
        Candidate {
            name: "GNOME Terminal",
            probe_bin: "gnome-terminal",
            platform: "linux",
            args_template: &["--", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "Konsole",
            probe_bin: "konsole",
            platform: "linux",
            args_template: &["-e", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "Alacritty",
            probe_bin: "alacritty",
            platform: "linux",
            args_template: &["-e", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "Kitty",
            probe_bin: "kitty",
            platform: "linux",
            args_template: &["{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "WezTerm",
            probe_bin: "wezterm",
            platform: "linux",
            args_template: &["start", "--", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "Foot",
            probe_bin: "foot",
            platform: "linux",
            args_template: &["{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "XFCE Terminal",
            probe_bin: "xfce4-terminal",
            platform: "linux",
            args_template: &["-e", "{cmd}"],
            split_args: false,
        },
        Candidate {
            name: "xterm",
            probe_bin: "xterm",
            platform: "linux",
            args_template: &["-e", "{cmd}"],
            split_args: true,
        },
        // macOS — priority order.
        Candidate {
            name: "iTerm2",
            probe_bin: "/Applications/iTerm.app/Contents/MacOS/iTerm2",
            platform: "macos",
            args_template: &[],
            split_args: false,
        },
        Candidate {
            name: "Terminal.app",
            probe_bin: "/System/Applications/Utilities/Terminal.app/Contents/MacOS/Terminal",
            platform: "macos",
            args_template: &[],
            split_args: false,
        },
        Candidate {
            name: "Alacritty (macOS)",
            probe_bin: "alacritty",
            platform: "macos",
            args_template: &["-e", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "WezTerm (macOS)",
            probe_bin: "wezterm",
            platform: "macos",
            args_template: &["start", "--", "{cmd}"],
            split_args: true,
        },
        Candidate {
            name: "Kitty (macOS)",
            probe_bin: "kitty",
            platform: "macos",
            args_template: &["{cmd}"],
            split_args: true,
        },
    ]
}

/// `which` augmented with WSL-specific fallbacks — on WSL the user's
/// PATH sometimes omits the Windows PATH (depending on config), so we
/// also probe `/mnt/c/Windows/System32` and the WindowsApps dir for
/// `wt.exe` and friends.
fn which_with_extra_paths(name: &str, on_wsl: bool) -> Option<PathBuf> {
    if let Some(p) = which(name) {
        return Some(p);
    }
    if !on_wsl || !name.ends_with(".exe") {
        return None;
    }
    // Common WSL-visible Windows binary locations.
    let extras = [
        "/mnt/c/Windows/System32",
        "/mnt/c/Windows",
        "/mnt/c/Program Files/WindowsApps",
    ];
    for d in &extras {
        let p = Path::new(d).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    // WindowsApps has versioned subdirs like
    // Microsoft.WindowsTerminal_*, walk one level.
    if let Ok(rd) = std::fs::read_dir("/mnt/c/Program Files/WindowsApps") {
        for entry in rd.flatten() {
            let p = entry.path().join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Minimal `which`. Checks every `PATH` entry for `name` (with the
/// platform's executable suffix added on Windows if the caller didn't
/// include one).
fn which(name: &str) -> Option<PathBuf> {
    // Absolute or relative path given → check directly.
    if name.contains('/') || name.contains('\\') {
        let p = Path::new(name);
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        return None;
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if std::env::consts::OS == "windows" && !name.ends_with(".exe") {
            let with_ext = dir.join(format!("{name}.exe"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
    }
    None
}

fn shell_quote(p: &Path) -> String {
    let s = p.display().to_string();
    if s.is_empty() {
        return "''".into();
    }
    // Quote when the token contains anything a shell would split on
    // or expand. Conservative — a few extra quotes never hurt.
    let needs_quotes = s
        .chars()
        .any(|c| c.is_whitespace() || "\"'`$\\&|;<>()[]{}*?".contains(c));
    if needs_quotes {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s
    }
}

/// Terminal matching by name (case-insensitive, substring match).
/// Used by `--terminal <name>` to let the user pick without caring
/// about exact capitalization.
pub fn match_by_name(candidates: &[Terminal], query: &str) -> Option<Terminal> {
    let q = query.to_lowercase();
    candidates
        .iter()
        .find(|t| t.name.to_lowercase() == q)
        .cloned()
        .or_else(|| {
            candidates
                .iter()
                .find(|t| t.name.to_lowercase().contains(&q))
                .cloned()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_by_name_exact_and_substring() {
        let terms = vec![
            Terminal {
                name: "Windows Terminal".into(),
                binary: "/wt.exe".into(),
                platform: "windows",
                args_template: vec![],
                split_args: true,
            },
            Terminal {
                name: "Alacritty".into(),
                binary: "/alacritty".into(),
                platform: "linux",
                args_template: vec![],
                split_args: true,
            },
        ];
        // Exact case-insensitive match.
        assert_eq!(
            match_by_name(&terms, "windows terminal").unwrap().name,
            "Windows Terminal"
        );
        // Substring.
        assert_eq!(match_by_name(&terms, "alac").unwrap().name, "Alacritty");
        // Miss.
        assert!(match_by_name(&terms, "iterm").is_none());
    }

    #[test]
    fn shell_quote_bare_token_returns_unchanged() {
        assert_eq!(shell_quote(Path::new("simple")), "simple");
        assert_eq!(shell_quote(Path::new("/usr/bin/pa")), "/usr/bin/pa");
    }

    #[test]
    fn shell_quote_with_spaces_single_quotes() {
        assert_eq!(shell_quote(Path::new("has space")), "'has space'");
    }

    #[test]
    fn shell_quote_with_embedded_single_quote() {
        // POSIX '-escape inside single-quoted string.
        assert_eq!(shell_quote(Path::new("a'b")), "'a'\\''b'");
    }

    #[test]
    fn build_invocation_split_args_produces_three_tokens_for_pa() {
        let term = Terminal {
            name: "test".into(),
            binary: "/bin/wt".into(),
            platform: "linux",
            args_template: vec!["--".into(), "{cmd}".into()],
            split_args: true,
        };
        let argv = build_invocation(&term, Path::new("/bin/pa"), "%u");
        // wt, --, pa, open, %u
        assert_eq!(argv, vec!["/bin/wt", "--", "/bin/pa", "open", "%u"]);
    }

    #[test]
    fn build_invocation_joined_template_substitutes_cmd() {
        let term = Terminal {
            name: "test".into(),
            binary: "/bin/cmd".into(),
            platform: "windows",
            args_template: vec!["/c".into(), "start".into(), "".into(), "{cmd}".into()],
            split_args: false,
        };
        let argv = build_invocation(&term, Path::new("/bin/pa"), "%1");
        assert_eq!(argv[0], "/bin/cmd");
        assert_eq!(argv[1], "/c");
        assert_eq!(argv[2], "start");
        assert_eq!(argv[3], ""); // empty title arg to `start`
        // {cmd} expanded to `pa open %1` (shell-quoted).
        assert!(argv[4].contains("pa"));
        assert!(argv[4].contains("open"));
        assert!(argv[4].contains("%1"));
    }

    #[test]
    fn build_desktop_entry_contains_mimetype_and_exec() {
        let term = Terminal {
            name: "xterm".into(),
            binary: "/usr/bin/xterm".into(),
            platform: "linux",
            args_template: vec!["-e".into(), "{cmd}".into()],
            split_args: true,
        };
        let out = build_desktop_entry(&term, Path::new("/home/u/.cargo/bin/pa"));
        assert!(out.contains("MimeType=x-scheme-handler/pa;"));
        assert!(out.contains("Exec="));
        assert!(out.contains("/usr/bin/xterm"));
        assert!(out.contains("pa"));
    }

    #[test]
    fn build_windows_reg_has_url_protocol_and_command() {
        let term = Terminal {
            name: "wt".into(),
            binary: "C:\\wt.exe".into(),
            platform: "windows",
            args_template: vec!["--".into(), "{cmd}".into()],
            split_args: true,
        };
        let out = build_windows_reg(&term, Path::new("C:\\pa.exe"));
        assert!(out.contains("\"URL Protocol\"=\"\""));
        // Registry section headers use single backslashes in the
        // .reg format. In a Rust string literal that's `\\`.
        assert!(
            out.contains("HKEY_CURRENT_USER\\Software\\Classes\\pa"),
            "missing scheme key:\n{out}"
        );
        assert!(
            out.contains("shell\\open\\command"),
            "missing command key:\n{out}"
        );
    }
}
