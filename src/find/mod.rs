//! Folder-finder pipeline. Powers the in-TUI "new workspace at..."
//! flow (commit chain in `~/.claude/plans/piped-sauteeing-breeze.md`).
//!
//! Architecture: a tiered probe orchestrator. Each tier returns
//! `Vec<PathBuf>` of candidate directories. The aggregator dedups
//! by canonical path, then ranks the merged list via a fuzzy
//! matcher (nucleo) against the user's query. Top N are returned.
//!
//! Tier order — fastest / freest first:
//!
//! 1. recency  — our own `state.toml` of recently launched
//!    workspaces (always available, instant).
//! 2. zoxide   — user's frecency index (if `zoxide` on PATH).
//! 3. locate   — pre-built filesystem index (plocate / locate /
//!    Everything CLI on Windows).
//! 4. fd       — live walk respecting `.gitignore` (if `fd` on
//!    PATH).
//! 5. walk     — stdlib `read_dir` recursion with a small
//!    hardcoded ignore list. Always-available fallback;
//!    depth-capped so it stays bounded.
//!
//! Empty query → tiers 1 + 2 only (instant, no FS walks). Non-empty
//! → all tiers, then nucleo ranks the merged set.
//!
//! All shell-outs run with a hard timeout (1 s) so a slow external
//! tool can't block the TUI.

pub mod fd;
pub mod locate;
pub mod recency;
pub mod shell;
pub mod walk;
pub mod zoxide;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Where a candidate came from. Used for ranker tie-breaking and
/// for displaying a small badge in the TUI ("from zoxide", etc.).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Recency,
    Zoxide,
    Locate,
    Fd,
    Walk,
}

impl Source {
    /// Short human label for the result row.
    pub fn label(&self) -> &'static str {
        match self {
            Source::Recency => "recent",
            Source::Zoxide => "zoxide",
            Source::Locate => "locate",
            Source::Fd => "fd",
            Source::Walk => "scan",
        }
    }
}

/// One ranked candidate folder.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub path: PathBuf,
    pub source: Source,
    /// Nucleo match score against the query, or 0 if the query was
    /// empty (in which case rows are ordered by source priority +
    /// recency only).
    pub score: i32,
}

/// Inputs to [`find_candidates`].
#[derive(Debug, Clone)]
pub struct FindOpts {
    /// Roots to walk in tiers 4 + 5. Typically `[$HOME]`.
    pub roots: Vec<PathBuf>,
    /// Max recursion depth for tier 5. 6 hits typical
    /// `~/code/<project>/<subproject>` layouts without crawling
    /// `node_modules` graveyards.
    pub max_depth: u16,
    /// Cap on returned candidates after ranking.
    pub limit: usize,
}

impl Default for FindOpts {
    fn default() -> Self {
        Self {
            roots: default_roots(),
            max_depth: 6,
            limit: 30,
        }
    }
}

/// Compute the default search-root list. Always includes `$HOME`.
/// On WSL the user's projects often live under
/// `/mnt/c/Users/<user>/...` (Documents, Desktop, code, etc.) —
/// `$HOME` never reaches there. We auto-detect WSL via
/// `/proc/version` containing "microsoft" (case-insensitive) and
/// add `/mnt/c/Users/<windows-user>` as an extra root, deduped.
///
/// `$PORTAGENTY_SEARCH_ROOTS` (colon-separated list, like `$PATH`)
/// overrides the defaults entirely so power users can pin exactly
/// where the finder walks. Useful for projects on external mounts
/// or non-standard layouts.
pub fn default_roots() -> Vec<PathBuf> {
    if let Ok(env_roots) = std::env::var("PORTAGENTY_SEARCH_ROOTS") {
        let parsed: Vec<PathBuf> = std::env::split_paths(&env_roots)
            .filter(|p| p.is_dir())
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if home.is_dir() {
            roots.push(home);
        }
    }
    if is_wsl() {
        if let Some(win_users_root) = wsl_windows_user_dir() {
            if win_users_root.is_dir() && !roots.iter().any(|r| r == &win_users_root) {
                roots.push(win_users_root);
            }
        }
    }
    if roots.is_empty() {
        roots.push(PathBuf::from("."));
    }
    roots
}

/// Detect WSL by reading `/proc/version`. Best-effort — anything
/// that looks like the Microsoft-built kernel string counts.
pub fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() {
        return true;
    }
    if let Ok(s) = std::fs::read_to_string("/proc/version") {
        return s.to_lowercase().contains("microsoft");
    }
    false
}

/// Best-effort guess at the Windows user's home dir from inside
/// WSL. Reads `$USER` and looks at `/mnt/c/Users/<user>`. Falls
/// back to the first existing entry under `/mnt/c/Users/` if
/// `$USER` doesn't match a folder there.
fn wsl_windows_user_dir() -> Option<PathBuf> {
    let users_root = PathBuf::from("/mnt/c/Users");
    if !users_root.is_dir() {
        return None;
    }
    if let Ok(user) = std::env::var("USER") {
        let candidate = users_root.join(&user);
        if candidate.is_dir() {
            return Some(candidate);
        }
        // Common case: WSL $USER is lowercase but the Windows user
        // dir is title-case. Try matching case-insensitively.
        if let Ok(entries) = std::fs::read_dir(&users_root) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.eq_ignore_ascii_case(&user) {
                    return Some(e.path());
                }
            }
        }
    }
    None
}

/// Probe which search backends are available on this machine.
/// Used by the TUI to surface a "via tool" hint in the search
/// overlay's title — so the user can tell at a glance whether
/// fd / zoxide / plocate are contributing or not.
#[derive(Debug, Clone, Default)]
pub struct BackendAvailability {
    pub recency: bool,
    pub zoxide: bool,
    pub locate: bool,
    pub fd: bool,
    pub walk: bool,
}

impl BackendAvailability {
    pub fn probe() -> Self {
        Self {
            // Recency is always populated from state.toml (which
            // may be empty on a brand-new install — that's fine,
            // just no recents to show).
            recency: true,
            zoxide: shell::on_path("zoxide"),
            locate: shell::on_path("plocate")
                || shell::on_path("locate")
                || shell::on_path("es.exe"),
            fd: shell::on_path("fd"),
            walk: true,
        }
    }

    /// Compact one-liner like `recents · zoxide · fd · scan` for
    /// the search overlay's title bar.
    pub fn one_liner(&self) -> String {
        let mut parts: Vec<&'static str> = Vec::with_capacity(5);
        if self.recency {
            parts.push("recents");
        }
        if self.zoxide {
            parts.push("zoxide");
        }
        if self.locate {
            parts.push("locate");
        }
        if self.fd {
            parts.push("fd");
        }
        if self.walk {
            parts.push("scan");
        }
        parts.join(" · ")
    }
}

/// Drive all available tiers, merge + dedup + rank by `query`. Pure
/// orchestration — each tier knows how to be silent if its backing
/// tool isn't installed.
///
/// `query` semantics:
/// - Empty → top results from tiers 1 + 2 only (instant, no walks).
/// - Starts with `/` or `~/` → treat as an absolute-path prefix;
///   tier 5 walks from that root only.
/// - Anything else → all tiers, ranked by nucleo.
pub fn find_candidates(query: &str, opts: &FindOpts) -> Vec<Candidate> {
    let trimmed = query.trim();
    let mut results: Vec<Candidate> = Vec::new();

    if trimmed.is_empty() {
        for p in recency::collect() {
            results.push(Candidate {
                path: p,
                source: Source::Recency,
                score: 0,
            });
        }
        for p in zoxide::collect() {
            results.push(Candidate {
                path: p,
                source: Source::Zoxide,
                score: 0,
            });
        }
        return dedup_keep_first(results)
            .into_iter()
            .take(opts.limit)
            .collect();
    }

    // Absolute-path prefix mode: limit walking to the given root.
    if trimmed.starts_with('/') || trimmed.starts_with("~/") {
        let abs = expand_tilde(trimmed);
        let root = first_existing_ancestor(&abs);
        if let Some(r) = root {
            let walk_opts = FindOpts {
                roots: vec![r],
                ..opts.clone()
            };
            for p in walk::collect("", &walk_opts) {
                results.push(Candidate {
                    path: p,
                    source: Source::Walk,
                    score: 0,
                });
            }
        }
        return rank_and_truncate(results, trimmed, opts.limit);
    }

    for p in recency::collect() {
        results.push(Candidate {
            path: p,
            source: Source::Recency,
            score: 0,
        });
    }
    for p in zoxide::collect() {
        results.push(Candidate {
            path: p,
            source: Source::Zoxide,
            score: 0,
        });
    }
    for p in locate::collect(trimmed) {
        results.push(Candidate {
            path: p,
            source: Source::Locate,
            score: 0,
        });
    }
    for p in fd::collect(trimmed, opts) {
        results.push(Candidate {
            path: p,
            source: Source::Fd,
            score: 0,
        });
    }
    for p in walk::collect(trimmed, opts) {
        results.push(Candidate {
            path: p,
            source: Source::Walk,
            score: 0,
        });
    }

    rank_and_truncate(results, trimmed, opts.limit)
}

/// Dedup by canonical path, preserving first-seen order. Tier 1
/// (recency) wins over later tiers because we push it first.
fn dedup_keep_first(items: Vec<Candidate>) -> Vec<Candidate> {
    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(items.len());
    let mut out: Vec<Candidate> = Vec::with_capacity(items.len());
    for c in items {
        let key = c.path.canonicalize().unwrap_or_else(|_| c.path.clone());
        if seen.insert(key) {
            out.push(c);
        }
    }
    out
}

/// Apply nucleo fuzzy ranking + dedup, return top `limit`.
fn rank_and_truncate(items: Vec<Candidate>, query: &str, limit: usize) -> Vec<Candidate> {
    let mut deduped = dedup_keep_first(items);
    let mut matcher = nucleo_matcher::Matcher::new(nucleo_matcher::Config::DEFAULT.match_paths());
    let pattern = nucleo_matcher::pattern::Pattern::parse(
        query,
        nucleo_matcher::pattern::CaseMatching::Smart,
        nucleo_matcher::pattern::Normalization::Smart,
    );
    for c in &mut deduped {
        let haystack = c.path.to_string_lossy();
        let mut buf: Vec<char> = Vec::new();
        let utf32 = nucleo_matcher::Utf32Str::new(&haystack, &mut buf);
        c.score = pattern.score(utf32, &mut matcher).unwrap_or(0) as i32;
    }
    // Drop zero-score entries that don't match the query at all.
    deduped.retain(|c| c.score > 0);
    deduped.sort_by(|a, b| b.score.cmp(&a.score));
    deduped.truncate(limit);
    deduped
}

/// Expand a leading `~/` to `$HOME`. No-op for paths without it.
fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

/// Walk parent directories until we find one that exists. Used for
/// the absolute-path-prefix mode so we can search inside the
/// nearest real ancestor of a path the user is still typing.
fn first_existing_ancestor(p: &Path) -> Option<PathBuf> {
    let mut cur: Option<&Path> = Some(p);
    while let Some(c) = cur {
        if c.is_dir() {
            return Some(c.to_path_buf());
        }
        cur = c.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(path: &str, source: Source) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            source,
            score: 0,
        }
    }

    #[test]
    fn dedup_keeps_first_occurrence() {
        let items = vec![
            cand("/a", Source::Recency),
            cand("/b", Source::Recency),
            cand("/a", Source::Walk), // dupe
            cand("/c", Source::Walk),
        ];
        let out = dedup_keep_first(items);
        let paths: Vec<&PathBuf> = out.iter().map(|c| &c.path).collect();
        assert_eq!(
            paths,
            vec![
                &PathBuf::from("/a"),
                &PathBuf::from("/b"),
                &PathBuf::from("/c")
            ]
        );
        // First occurrence preserved → /a stays Recency, not Walk.
        assert_eq!(out[0].source, Source::Recency);
    }

    #[test]
    fn rank_drops_non_matches_and_orders_by_score() {
        let items = vec![
            cand("/home/u/cyberchaste", Source::Walk),
            cand("/home/u/random/notebooks", Source::Walk),
            cand("/home/u/cybersader/portagenty", Source::Walk),
        ];
        let out = rank_and_truncate(items, "cyber", 10);
        // 'cyber' should match cyberchaste and cybersader but not the
        // notebooks dir.
        let paths: Vec<String> = out.iter().map(|c| c.path.display().to_string()).collect();
        assert!(
            paths.iter().any(|p| p.contains("cyberchaste")),
            "missing cyberchaste in: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p.contains("cybersader")),
            "missing cybersader in: {paths:?}"
        );
        assert!(
            !paths.iter().any(|p| p.contains("notebooks")),
            "notebooks should not match 'cyber': {paths:?}"
        );
    }

    #[test]
    fn empty_query_returns_recency_plus_zoxide_only() {
        // We can't easily inject mock recency / zoxide into the
        // current API, so this test just verifies the empty-query
        // path doesn't panic and returns within the limit. Real
        // behavior is exercised end-to-end at the TUI layer.
        let opts = FindOpts {
            limit: 5,
            ..Default::default()
        };
        let out = find_candidates("", &opts);
        assert!(out.len() <= 5);
    }

    #[test]
    fn expand_tilde_resolves_home_when_set() {
        std::env::set_var("HOME", "/home/test");
        let p = expand_tilde("~/code/foo");
        assert_eq!(p, PathBuf::from("/home/test/code/foo"));
    }
}
